//! Organize a folder: build a move plan (from a preset rule or from the AI),
//! validate it so nothing can be overwritten or leave the folder, apply it, and undo it.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{Datelike, Local, TimeZone};
use serde::{Deserialize, Serialize};

use crate::db::MoveRecord;

/// Files beyond this count are not offered to a single plan.
pub const MAX_FILES: usize = 3000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    /// Path relative to the root, with `\` separators.
    pub rel: String,
    pub name: String,
    pub ext: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedMove {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub root: String,
    pub instruction: String,
    pub summary: String,
    pub moves: Vec<PlannedMove>,
    /// Problems found while validating (moves that were dropped or adjusted).
    pub notes: Vec<String>,
    pub total_files: usize,
}

const BLOCKED: &[&str] = &["\\windows", "\\program files", "\\program files (x86)", "\\programdata", "\\appdata", "\\$recycle.bin", "\\system volume information"];

/// Refuses folders where moving files around could break Windows or apps.
pub fn check_root(root: &Path) -> Result<()> {
    if !root.is_dir() {
        bail!("フォルダが見つかりません: {}", root.display());
    }
    let l = root.to_string_lossy().to_lowercase().replace('/', "\\");
    let l = l.trim_end_matches('\\');
    if l.len() <= 2 || root.parent().is_none() {
        bail!("ドライブ直下は整理できません。フォルダを選んでください。");
    }
    // Test temp dirs live under AppData, so that one entry is relaxed in unit tests only.
    if BLOCKED.iter().any(|b| l.contains(b) && !(cfg!(test) && *b == "\\appdata")) {
        bail!("システムやアプリのフォルダは整理できません: {}", root.display());
    }
    if let Some(home) = dirs::home_dir()
        && home.to_string_lossy().to_lowercase().trim_end_matches('\\') == l
    {
        bail!("ユーザーフォルダ直下はまとめて整理できません。「ダウンロード」「デスクトップ」など中のフォルダを選んでください。");
    }
    Ok(())
}

/// Lists files under `root` (only the top level unless `recursive`).
pub fn list_files(root: &Path, recursive: bool) -> Result<Vec<FileInfo>> {
    check_root(root)?;
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for ent in std::fs::read_dir(&dir)?.flatten() {
            let Ok(md) = ent.metadata() else { continue };
            let ft = ent.file_type().ok();
            if md.is_dir() {
                if recursive && !ft.map(|t| t.is_symlink()).unwrap_or(false) {
                    stack.push(ent.path());
                }
                continue;
            }
            let meta = crate::index::FileMeta::from_metadata(&md);
            if meta.hidden {
                continue;
            }
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.eq_ignore_ascii_case("desktop.ini") || name.starts_with("~$") {
                continue;
            }
            let rel = ent.path().strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            out.push(FileInfo {
                ext: ext_lower(&name),
                rel,
                name,
                size: meta.size,
                mtime: meta.mtime,
            });
            if out.len() >= MAX_FILES {
                return Ok(out);
            }
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

fn ext_lower(name: &str) -> String {
    Path::new(name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn category(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "heic" | "tif" | "tiff" | "svg" | "ico" | "raw" | "cr2" | "nef" => "画像",
        "mp4" | "mov" | "avi" | "mkv" | "wmv" | "webm" | "m4v" | "flv" => "動画",
        "mp3" | "wav" | "m4a" | "flac" | "aac" | "ogg" | "wma" => "音楽",
        "pdf" => "PDF",
        "doc" | "docx" | "odt" | "rtf" | "txt" | "md" => "文書",
        "xls" | "xlsx" | "xlsm" | "csv" | "ods" => "表計算",
        "ppt" | "pptx" | "odp" | "key" => "プレゼン",
        "zip" | "7z" | "rar" | "lzh" | "tar" | "gz" | "bz2" | "xz" => "圧縮ファイル",
        "exe" | "msi" | "msix" | "appx" | "appinstaller" => "インストーラー",
        "html" | "htm" | "css" | "js" | "ts" | "py" | "rs" | "java" | "c" | "cpp" | "cs" | "go" | "json" | "xml" | "yml" | "yaml" | "sql" | "sh" | "ps1" | "bat" => "コード",
        "ttf" | "otf" | "woff" | "woff2" => "フォント",
        "iso" | "img" | "vhd" | "vhdx" => "ディスクイメージ",
        "lnk" | "url" => "ショートカット",
        _ => "その他",
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)]
pub enum Preset {
    ByType,
    ByYearMonth,
    ByYear,
    ByExtension,
}

impl Preset {
    pub fn label(self) -> &'static str {
        match self {
            Preset::ByType => "種類別に整理",
            Preset::ByYearMonth => "年/月別に整理",
            Preset::ByYear => "年別に整理",
            Preset::ByExtension => "拡張子別に整理",
        }
    }
}

fn ym(ts: i64) -> (i32, u32) {
    let d = Local.timestamp_opt(ts, 0).single().unwrap_or_else(Local::now);
    (d.year(), d.month())
}

pub fn preset_plan(root: &Path, preset: Preset, recursive: bool) -> Result<Plan> {
    let files = list_files(root, recursive)?;
    let moves = files
        .iter()
        .map(|f| {
            let folder = match preset {
                Preset::ByType => category(&f.ext).to_string(),
                Preset::ByYearMonth => {
                    let (y, m) = ym(f.mtime);
                    format!("{y}\\{m:02}")
                }
                Preset::ByYear => format!("{}", ym(f.mtime).0),
                Preset::ByExtension => {
                    if f.ext.is_empty() { "拡張子なし".to_string() } else { f.ext.to_uppercase() }
                }
            };
            PlannedMove { from: f.rel.clone(), to: format!("{folder}\\{}", f.name) }
        })
        .collect();
    let plan = Plan {
        root: root.to_string_lossy().into_owned(),
        instruction: preset.label().to_string(),
        summary: format!("{}（{}件のファイル）", preset.label(), files.len()),
        moves,
        notes: Vec::new(),
        total_files: files.len(),
    };
    Ok(validate(plan))
}

/// A relative path that stays inside the root: no drive, no `..`, no reserved names.
fn clean_rel(rel: &str) -> Option<PathBuf> {
    let rel = rel.trim().replace('/', "\\");
    let p = Path::new(&rel);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => {
                let s = s.to_string_lossy();
                let t = s.trim().trim_end_matches('.');
                if t.is_empty() || t.contains(['<', '>', ':', '"', '|', '?', '*']) || is_reserved(t) {
                    return None;
                }
                out.push(t);
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit())
}

pub(crate) fn unique_target(root: &Path, rel: &Path, taken: &HashSet<String>) -> PathBuf {
    let key = |p: &Path| p.to_string_lossy().to_lowercase();
    if !root.join(rel).exists() && !taken.contains(&key(rel)) {
        return rel.to_path_buf();
    }
    let stem = rel.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = rel.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let parent = rel.parent().map(Path::to_path_buf).unwrap_or_default();
    for n in 2.. {
        let cand = parent.join(format!("{stem} ({n}){ext}"));
        if !root.join(&cand).exists() && !taken.contains(&key(&cand)) {
            return cand;
        }
    }
    unreachable!()
}

/// Drops unsafe or no-op moves and renames targets that would collide.
pub fn validate(mut plan: Plan) -> Plan {
    let root = PathBuf::from(&plan.root);
    let mut taken: HashSet<String> = HashSet::new();
    let mut seen_from: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    let mut notes = std::mem::take(&mut plan.notes);
    for m in plan.moves.drain(..) {
        let (Some(from), Some(to)) = (clean_rel(&m.from), clean_rel(&m.to)) else {
            notes.push(format!("無効なパスのため除外: {} → {}", m.from, m.to));
            continue;
        };
        let fk = from.to_string_lossy().to_lowercase();
        if !seen_from.insert(fk.clone()) {
            continue;
        }
        if !root.join(&from).is_file() {
            notes.push(format!("ファイルが見つからないため除外: {}", m.from));
            continue;
        }
        if fk == to.to_string_lossy().to_lowercase() {
            continue;
        }
        let target = unique_target(&root, &to, &taken);
        if target != to {
            notes.push(format!("同名ファイルがあるため名前を変更: {} → {}", to.display(), target.display()));
        }
        taken.insert(target.to_string_lossy().to_lowercase());
        out.push(PlannedMove { from: from.to_string_lossy().into_owned(), to: target.to_string_lossy().into_owned() });
    }
    plan.moves = out;
    plan.notes = notes;
    plan
}

/// Applies a validated plan. Returns the moves actually made (absolute paths).
pub fn apply(plan: &Plan) -> Result<(Vec<MoveRecord>, Vec<String>)> {
    let root = PathBuf::from(&plan.root);
    check_root(&root)?;
    let plan = validate(plan.clone());
    let mut done = Vec::new();
    let mut errors = Vec::new();
    for m in &plan.moves {
        let from = root.join(&m.from);
        let to = root.join(&m.to);
        if to.exists() {
            errors.push(format!("移動先に同名ファイルがあるためスキップ: {}", m.to));
            continue;
        }
        if let Some(parent) = to.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            errors.push(format!("フォルダを作れません {}: {e}", parent.display()));
            continue;
        }
        match std::fs::rename(&from, &to) {
            Ok(()) => done.push(MoveRecord { from: from.to_string_lossy().into_owned(), to: to.to_string_lossy().into_owned() }),
            Err(e) => errors.push(format!("{}: {e}", m.from)),
        }
    }
    Ok((done, errors))
}

/// Moves files back. Folders created by the organize step are removed only when empty.
pub fn undo(moves: &[MoveRecord]) -> (usize, Vec<String>) {
    let mut ok = 0;
    let mut errors = Vec::new();
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    for m in moves.iter().rev() {
        let (from, to) = (Path::new(&m.from), Path::new(&m.to));
        if !to.exists() {
            errors.push(format!("見つからないため戻せません: {}", m.to));
            continue;
        }
        let mut dest = from.to_path_buf();
        if dest.exists() {
            let parent = from.parent().unwrap_or(Path::new(""));
            let name = from.file_name().map(PathBuf::from).unwrap_or_default();
            dest = parent.join(unique_target(parent, &name, &HashSet::new()));
        }
        if let Some(p) = dest.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        match std::fs::rename(to, &dest) {
            Ok(()) => {
                ok += 1;
                if let Some(p) = to.parent() {
                    created_dirs.push(p.to_path_buf());
                }
            }
            Err(e) => errors.push(format!("{}: {e}", m.to)),
        }
    }
    created_dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    created_dirs.dedup();
    for d in created_dirs {
        let mut cur = Some(d.as_path());
        // remove_dir only succeeds on empty folders, so this never deletes files.
        while let Some(p) = cur {
            if std::fs::remove_dir(p).is_err() {
                break;
            }
            cur = p.parent();
        }
    }
    (ok, errors)
}

/// System and app folders: selected files there are never moved.
fn is_protected(path: &Path) -> bool {
    let l = path.to_string_lossy().to_lowercase();
    ["\\windows\\", "\\program files", "\\programdata\\", "\\$recycle.bin"].iter().any(|b| l.contains(b))
        || (!cfg!(test) && l.contains("\\appdata\\"))
}

fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for e in std::fs::read_dir(from)?.flatten() {
            copy_recursive(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// Moves (`copy` = false) or copies files and folders into `dest_dir`. Existing names get " (2)" etc.,
/// so nothing is overwritten. Returns what was done (absolute paths) and per-item errors.
pub fn transfer(paths: &[String], dest_dir: &Path, copy: bool) -> Result<(Vec<MoveRecord>, Vec<String>)> {
    if !dest_dir.is_dir() {
        bail!("移動先のフォルダが見つかりません: {}", dest_dir.display());
    }
    if !copy && is_protected(&dest_dir.join("x")) {
        bail!("システムやアプリのフォルダには移動できません");
    }
    let mut done = Vec::new();
    let mut errors = Vec::new();
    let mut taken = HashSet::new();
    for p in paths {
        let from = PathBuf::from(p);
        let Some(name) = from.file_name().map(PathBuf::from) else { continue };
        if !from.exists() {
            errors.push(format!("見つかりません: {p}"));
            continue;
        }
        if from.parent().is_some_and(|parent| parent == dest_dir) {
            errors.push(format!("すでにそのフォルダにあります: {p}"));
            continue;
        }
        if dest_dir.starts_with(&from) {
            errors.push(format!("フォルダをその中へは移動できません: {p}"));
            continue;
        }
        if !copy && is_protected(&from) {
            errors.push(format!("システムやアプリのファイルは移動できません: {p}"));
            continue;
        }
        let rel = unique_target(dest_dir, &name, &taken);
        taken.insert(rel.to_string_lossy().to_lowercase());
        let target = dest_dir.join(rel);
        let r = if copy {
            copy_recursive(&from, &target).map_err(anyhow::Error::from)
        } else if from.is_dir() {
            std::fs::rename(&from, &target).map_err(|e| anyhow::anyhow!("フォルダは同じドライブの中でだけ移動できます（{e}）"))
        } else {
            crate::rules::move_file(&from, &target)
        };
        match r {
            Ok(()) => done.push(MoveRecord { from: p.clone(), to: target.to_string_lossy().into_owned() }),
            Err(e) => errors.push(format!("{p}: {e}")),
        }
    }
    Ok((done, errors))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().join("dl");
        std::fs::create_dir_all(&r).unwrap();
        for n in ["a.pdf", "b.PNG", "c.xlsx", "setup.exe", "noext"] {
            std::fs::write(r.join(n), n).unwrap();
        }
        std::fs::create_dir_all(r.join("PDF")).unwrap();
        std::fs::write(r.join("PDF").join("a.pdf"), "existing").unwrap();
        d
    }

    #[test]
    fn preset_apply_undo() {
        let d = setup();
        let root = d.path().join("dl");
        let plan = preset_plan(&root, Preset::ByType, false).unwrap();
        assert_eq!(plan.moves.len(), 5);
        let pdf = plan.moves.iter().find(|m| m.from == "a.pdf").unwrap();
        assert_eq!(pdf.to, "PDF\\a (2).pdf", "collision must be renamed, never overwritten");
        let (done, errs) = apply(&plan).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(done.len(), 5);
        assert!(root.join("画像").join("b.PNG").exists());
        assert_eq!(std::fs::read_to_string(root.join("PDF").join("a.pdf")).unwrap(), "existing");
        let (ok, errs) = undo(&done);
        assert_eq!(ok, 5, "{errs:?}");
        assert!(root.join("a.pdf").exists());
        assert!(!root.join("画像").exists(), "empty folder created by organize is cleaned up");
        assert!(root.join("PDF").join("a.pdf").exists(), "pre-existing file untouched");
    }

    #[test]
    fn rejects_escape_and_missing() {
        let d = setup();
        let root = d.path().join("dl");
        let plan = Plan {
            root: root.to_string_lossy().into_owned(),
            instruction: "x".into(),
            summary: String::new(),
            moves: vec![
                PlannedMove { from: "a.pdf".into(), to: "..\\escape.pdf".into() },
                PlannedMove { from: "c.xlsx".into(), to: "C:\\abs.xlsx".into() },
                PlannedMove { from: "zzz.txt".into(), to: "x\\zzz.txt".into() },
                PlannedMove { from: "noext".into(), to: "CON\\noext".into() },
                PlannedMove { from: "b.PNG".into(), to: "pics/b.PNG".into() },
            ],
            notes: vec![],
            total_files: 5,
        };
        let v = validate(plan);
        assert_eq!(v.moves.len(), 1);
        assert_eq!(v.moves[0].to, "pics\\b.PNG");
        assert_eq!(v.notes.len(), 4);
    }

    #[test]
    fn transfer_move_copy_and_guards() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("src");
        let dst = d.path().join("dst");
        std::fs::create_dir_all(src.join("folder")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("a.txt"), "new").unwrap();
        std::fs::write(src.join("folder").join("in.txt"), "x").unwrap();
        std::fs::write(dst.join("a.txt"), "old").unwrap();
        let s = |p: &Path| p.to_string_lossy().into_owned();

        // copy keeps the originals and never overwrites
        let (done, errs) = transfer(&[s(&src.join("a.txt")), s(&src.join("folder"))], &dst, true).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(done.len(), 2);
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(dst.join("a (2).txt")).unwrap(), "new");
        assert!(dst.join("folder").join("in.txt").exists() && src.join("a.txt").exists());

        // move a file and a folder (the folder name now collides → renamed), then undo
        let (done, errs) = transfer(&[s(&src.join("a.txt")), s(&src.join("folder"))], &dst, false).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert!(!src.join("a.txt").exists() && dst.join("a (3).txt").exists() && dst.join("folder (2)").exists());
        let (ok, _) = undo(&done);
        assert_eq!(ok, 2);
        assert!(src.join("a.txt").exists() && src.join("folder").join("in.txt").exists());

        // guards: same folder, folder into itself, missing destination
        let (_, errs) = transfer(&[s(&dst.join("a.txt"))], &dst, false).unwrap();
        assert_eq!(errs.len(), 1);
        let (_, errs) = transfer(&[s(&src)], &src.join("folder"), false).unwrap();
        assert_eq!(errs.len(), 1);
        assert!(transfer(&[s(&src.join("a.txt"))], &d.path().join("nope"), false).is_err());
    }

    #[test]
    fn blocked_roots() {
        assert!(check_root(Path::new("C:\\")).is_err());
        assert!(check_root(Path::new("C:\\Windows")).is_err());
        if let Some(h) = dirs::home_dir() {
            assert!(check_root(&h).is_err());
        }
    }
}
