//! Auto-organize rules: "files of these kinds that land in this folder go to that folder".
//! Rules run when the watcher sees a new file (after it stops changing) or on demand,
//! and every run is recorded in the organize history so it can be undone.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::db::MoveRecord;
use crate::normalize::normalize;
use crate::organize::{category, check_root, unique_target};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Folder that is watched (only files directly inside it).
    pub folder: String,
    /// Kinds as shown in the organize presets (画像, PDF, 文書 …). Empty = any kind.
    pub kinds: Vec<String>,
    /// Words that must appear in the file name (any one of them). Empty = any name.
    pub name_contains: Vec<String>,
    /// Destination folder (absolute path).
    pub dest: String,
}

/// Download in progress or editor scratch files: never move these.
fn is_temporary(name: &str) -> bool {
    let l = name.to_lowercase();
    l.starts_with("~$")
        || l.starts_with('.')
        || l == "desktop.ini"
        || [".crdownload", ".part", ".partial", ".download", ".tmp", ".opdownload"].iter().any(|e| l.ends_with(e))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\").trim_end_matches('\\').to_string();
    n(a) == n(b)
}

impl Rule {
    /// Checks the folders before a rule is saved or run.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("ルールの名前を入力してください");
        }
        let folder = Path::new(&self.folder);
        check_root(folder)?;
        let dest = Path::new(&self.dest);
        if !dest.is_absolute() {
            bail!("移動先のフォルダを選んでください");
        }
        if same_dir(folder, dest) {
            bail!("監視するフォルダと移動先が同じです");
        }
        // The destination may not exist yet, so check the nearest existing parent.
        let mut existing = dest.to_path_buf();
        while !existing.exists() {
            match existing.parent() {
                Some(p) => existing = p.to_path_buf(),
                None => bail!("移動先のドライブが見つかりません: {}", self.dest),
            }
        }
        if existing.parent().is_some() {
            check_root(&existing).or_else(|e| if existing == dest { Err(e) } else { Ok(()) })?;
        }
        let dl = self.dest.to_lowercase();
        // Test temp dirs live under AppData, so that one entry is relaxed in unit tests only.
        if ["\\windows", "\\program files", "\\programdata", "\\appdata"].iter().any(|b| dl.contains(b) && !(cfg!(test) && *b == "\\appdata")) {
            bail!("システムやアプリのフォルダには移動できません");
        }
        Ok(())
    }

    /// Whether `path` (a file directly inside the watched folder) should be moved by this rule.
    pub fn matches(&self, path: &Path) -> bool {
        if !self.enabled {
            return false;
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name().map(|n| n.to_string_lossy())) else {
            return false;
        };
        if !same_dir(parent, Path::new(&self.folder)) || is_temporary(&name) {
            return false;
        }
        // Skip folders, and OneDrive "online-only" files: moving them would start a download.
        match std::fs::metadata(path) {
            Ok(md) if md.is_file() && !crate::index::FileMeta::from_metadata(&md).cloud => {}
            _ => return false,
        }
        let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !self.kinds.is_empty() && !self.kinds.iter().any(|k| k == category(&ext)) {
            return false;
        }
        let words: Vec<String> = self.name_contains.iter().map(|w| normalize(w.trim())).filter(|w| !w.is_empty()).collect();
        words.is_empty() || {
            let n = normalize(&name);
            words.iter().any(|w| n.contains(w.as_str()))
        }
    }

    /// Files currently in the watched folder that this rule would move, with their destinations.
    pub fn preview(&self) -> Result<Vec<MoveRecord>> {
        self.validate()?;
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.folder)?.flatten().map(|e| e.path()).filter(|p| self.matches(p)).collect();
        files.sort();
        Ok(self.plan(&files))
    }

    fn plan(&self, files: &[PathBuf]) -> Vec<MoveRecord> {
        let dest = PathBuf::from(&self.dest);
        let mut taken = HashSet::new();
        files
            .iter()
            .filter_map(|f| {
                let name = PathBuf::from(f.file_name()?);
                let target = unique_target(&dest, &name, &taken);
                taken.insert(target.to_string_lossy().to_lowercase());
                Some(MoveRecord { from: f.to_string_lossy().into_owned(), to: dest.join(target).to_string_lossy().into_owned() })
            })
            .collect()
    }

    /// Moves `files` (already matched) and returns what was done.
    pub fn run(&self, files: &[PathBuf]) -> (Vec<MoveRecord>, Vec<String>) {
        if let Err(e) = self.validate() {
            return (vec![], vec![e.to_string()]);
        }
        let mut done = Vec::new();
        let mut errors = Vec::new();
        for m in self.plan(files) {
            match move_file(Path::new(&m.from), Path::new(&m.to)) {
                Ok(()) => done.push(m),
                Err(e) => errors.push(format!("{}: {e}", m.from)),
            }
        }
        (done, errors)
    }
}

/// Rename, falling back to copy + remove when the destination is on another drive.
pub fn move_file(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        bail!("移動先に同名のファイルがあります");
    }
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    if std::fs::metadata(to)?.len() != std::fs::metadata(from)?.len() {
        let _ = std::fs::remove_file(to);
        bail!("コピーを確認できませんでした");
    }
    std::fs::remove_file(from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Rule) {
        let d = tempfile::tempdir().unwrap();
        let dl = d.path().join("dl");
        std::fs::create_dir_all(&dl).unwrap();
        for n in ["請求書_9月.pdf", "photo.png", "memo.txt", "big.pdf.crdownload", "~$draft.docx"] {
            std::fs::write(dl.join(n), n).unwrap();
        }
        let rule = Rule {
            id: "r1".into(),
            name: "PDFは書類へ".into(),
            enabled: true,
            folder: dl.to_string_lossy().into_owned(),
            kinds: vec!["PDF".into()],
            name_contains: vec![],
            dest: d.path().join("書類").to_string_lossy().into_owned(),
        };
        (d, rule)
    }

    #[test]
    fn matches_kind_and_skips_temporary() {
        let (d, rule) = setup();
        let dl = d.path().join("dl");
        assert!(rule.matches(&dl.join("請求書_9月.pdf")));
        assert!(!rule.matches(&dl.join("photo.png")));
        assert!(!rule.matches(&dl.join("big.pdf.crdownload")));
        let mut r2 = rule.clone();
        r2.kinds.clear();
        r2.name_contains = vec!["せいきゅう".into(), "メモ".into(), "MEMO".into()];
        assert!(r2.matches(&dl.join("memo.txt")));
        assert!(!r2.matches(&dl.join("photo.png")));
        assert!(!r2.matches(&dl.join("~$draft.docx")));
    }

    #[test]
    fn run_and_undo() {
        let (d, rule) = setup();
        let plan = rule.preview().unwrap();
        assert_eq!(plan.len(), 1);
        // A file with the same name already in the destination is kept; the new one is renamed.
        std::fs::create_dir_all(&rule.dest).unwrap();
        std::fs::write(Path::new(&rule.dest).join("請求書_9月.pdf"), "old").unwrap();
        let files: Vec<PathBuf> = plan.iter().map(|m| PathBuf::from(&m.from)).collect();
        let (done, errs) = rule.run(&files);
        assert!(errs.is_empty(), "{errs:?}");
        assert!(done[0].to.ends_with("請求書_9月 (2).pdf"));
        assert_eq!(std::fs::read_to_string(Path::new(&rule.dest).join("請求書_9月.pdf")).unwrap(), "old");
        let (ok, _) = crate::organize::undo(&done);
        assert_eq!(ok, 1);
        assert!(d.path().join("dl").join("請求書_9月.pdf").exists());
    }

    #[test]
    fn validation() {
        let (_d, rule) = setup();
        assert!(rule.validate().is_ok());
        let mut bad = rule.clone();
        bad.dest = bad.folder.clone();
        assert!(bad.validate().is_err());
        let mut bad = rule.clone();
        bad.dest = "C:\\Windows\\x".into();
        assert!(bad.validate().is_err());
        let mut bad = rule;
        bad.name = " ".into();
        assert!(bad.validate().is_err());
    }
}
