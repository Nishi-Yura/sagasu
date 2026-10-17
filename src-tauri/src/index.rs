//! In-memory file-name index. Searching ~1M entries takes tens of milliseconds
//! because every name is pre-normalized and the scan runs in parallel.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use rayon::prelude::*;
use serde::Serialize;

use crate::normalize::{normalize, subsequence_score};
use crate::query::Filter;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x40000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x400000;

#[derive(Debug, Clone)]
pub struct FileMeta {
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
    /// Cloud-only (e.g. OneDrive Files On-Demand): reading it would trigger a download.
    pub cloud: bool,
    pub hidden: bool,
}

impl FileMeta {
    pub fn from_metadata(m: &std::fs::Metadata) -> Self {
        let mtime = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let attrs = file_attributes(m);
        Self {
            size: if m.is_dir() { 0 } else { m.len() },
            mtime,
            is_dir: m.is_dir(),
            cloud: attrs & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0,
            hidden: attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0,
        }
    }
}

#[cfg(windows)]
fn file_attributes(m: &std::fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    m.file_attributes()
}

#[cfg(not(windows))]
fn file_attributes(_m: &std::fs::Metadata) -> u32 {
    0
}

struct Dir {
    raw: Box<str>,
    norm: Box<str>,
}

struct Entry {
    dir: u32,
    name: Box<str>,
    name_norm: Box<str>,
    size: u64,
    mtime: i64,
    is_dir: bool,
    cloud: bool,
    alive: bool,
    /// Hiragana reading of a name with kanji ("" = the IME could not read it, None = not looked up yet).
    reading: Option<Box<str>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub path: String,
    pub name: String,
    pub dir: String,
    pub size: u64,
    pub mtime: i64,
    pub is_dir: bool,
    pub cloud: bool,
    pub score: i32,
}

/// Candidate for full-text extraction.
pub struct ContentCandidate {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Default)]
pub struct Index {
    dirs: Vec<Dir>,
    dir_ids: HashMap<Box<str>, u32>,
    entries: Vec<Entry>,
    by_path: HashMap<u64, u32>,
    dead: usize,
}

fn key(path: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.to_lowercase().hash(&mut h);
    h.finish()
}

fn split(path: &str) -> (&str, &str) {
    match path.rfind(['\\', '/']) {
        Some(i) if i + 1 < path.len() => (&path[..i], &path[i + 1..]),
        _ => ("", path),
    }
}

pub fn ext_of(name_norm: &str) -> &str {
    match name_norm.rfind('.') {
        Some(i) if i > 0 => &name_norm[i + 1..],
        _ => "",
    }
}

impl Index {
    pub fn len(&self) -> usize {
        self.entries.len() - self.dead
    }

    fn dir_id(&mut self, raw: &str) -> u32 {
        if let Some(&id) = self.dir_ids.get(raw) {
            return id;
        }
        let id = self.dirs.len() as u32;
        self.dirs.push(Dir { raw: raw.into(), norm: normalize(raw).into() });
        self.dir_ids.insert(raw.into(), id);
        id
    }

    pub fn full_path(&self, i: usize) -> String {
        let e = &self.entries[i];
        let d = &self.dirs[e.dir as usize].raw;
        if d.is_empty() { e.name.to_string() } else { format!("{d}\\{}", e.name) }
    }

    fn find(&self, path: &str) -> Option<usize> {
        let i = *self.by_path.get(&key(path))? as usize;
        (self.entries[i].alive && self.full_path(i).eq_ignore_ascii_case(path)).then_some(i)
    }

    pub fn upsert(&mut self, path: &str, m: &FileMeta) {
        if let Some(i) = self.find(path) {
            let e = &mut self.entries[i];
            e.size = m.size;
            e.mtime = m.mtime;
            e.is_dir = m.is_dir;
            e.cloud = m.cloud;
            return;
        }
        let (dir, name) = split(path);
        let dir = self.dir_id(dir);
        let idx = self.entries.len() as u32;
        self.entries.push(Entry {
            dir,
            name: name.into(),
            name_norm: normalize(name).into(),
            size: m.size,
            mtime: m.mtime,
            is_dir: m.is_dir,
            cloud: m.cloud,
            alive: true,
            reading: None,
        });
        self.by_path.insert(key(path), idx);
    }

    /// Removes `path` and, if it was a folder, everything below it.
    pub fn remove(&mut self, path: &str) {
        if let Some(i) = self.find(path) {
            self.entries[i].alive = false;
            self.by_path.remove(&key(path));
            self.dead += 1;
        }
        let prefix = format!("{}\\", path.trim_end_matches(['\\', '/'])).to_lowercase();
        let dead_dirs: std::collections::HashSet<u32> = self
            .dirs
            .iter()
            .enumerate()
            .filter(|(_, d)| {
                let l = d.raw.to_lowercase();
                l.starts_with(&prefix) || format!("{l}\\") == prefix
            })
            .map(|(i, _)| i as u32)
            .collect();
        if dead_dirs.is_empty() {
            return;
        }
        for i in 0..self.entries.len() {
            if self.entries[i].alive && dead_dirs.contains(&self.entries[i].dir) {
                let p = self.full_path(i);
                self.by_path.remove(&key(&p));
                self.entries[i].alive = false;
                self.dead += 1;
            }
        }
    }

    pub fn contains(&self, path: &str) -> bool {
        self.find(path).is_some()
    }

    pub fn from_rows(rows: impl Iterator<Item = (String, FileMeta)>) -> Self {
        let mut ix = Index::default();
        for (p, m) in rows {
            ix.upsert(&p, &m);
        }
        ix
    }

    fn rows(&self) -> impl Iterator<Item = (String, &Entry)> + '_ {
        self.entries.iter().enumerate().filter(|(_, e)| e.alive).map(|(i, e)| (self.full_path(i), e))
    }

    /// Rows for the startup snapshot, produced one at a time (no full copy of every path).
    pub fn snapshot_iter(&self) -> impl Iterator<Item = (String, u64, i64, bool)> + '_ {
        self.rows().map(|(p, e)| (p, e.size, e.mtime, e.is_dir))
    }

    pub fn content_candidates(&self, max_size: u64, supported: impl Fn(&str) -> bool) -> Vec<ContentCandidate> {
        self.rows()
            .filter(|(_, e)| !e.is_dir && !e.cloud && e.size > 0 && e.size <= max_size && supported(ext_of(&e.name_norm)))
            .map(|(p, e)| ContentCandidate { path: p, size: e.size, mtime: e.mtime })
            .collect()
    }

    /// Local (non-cloud) files of at least `min_size`, optionally only below `under`.
    pub fn files_at_least(&self, min_size: u64, under: Option<&str>) -> Vec<Hit> {
        let prefix = under.map(|u| format!("{}\\", u.trim_end_matches(['\\', '/']).to_lowercase()));
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.alive && !e.is_dir && !e.cloud && e.size >= min_size.max(1))
            .map(|(i, _)| self.hit(i, 0))
            .filter(|h| prefix.as_ref().is_none_or(|p| h.path.to_lowercase().starts_with(p)))
            .collect()
    }

    /// Distinct file-name stems containing kanji whose reading has not been looked up yet.
    pub fn names_needing_reading(&self) -> Vec<String> {
        let mut set = std::collections::HashSet::new();
        for e in self.entries.iter().filter(|e| e.alive && e.reading.is_none()) {
            let stem = stem_of(&e.name);
            if crate::reading::has_kanji(stem) {
                set.insert(stem.to_string());
            }
        }
        set.into_iter().collect()
    }

    /// Stores readings (keyed by the original name stem); "" marks names the IME could not read.
    pub fn set_readings(&mut self, map: &HashMap<String, String>) {
        for e in self.entries.iter_mut().filter(|e| e.alive && e.reading.is_none()) {
            if let Some(r) = map.get(stem_of(&e.name)) {
                e.reading = Some(normalize(r).into());
            }
        }
    }

    /// The `limit` largest files (optionally only below `under`), biggest first.
    pub fn largest(&self, limit: usize, under: Option<&str>) -> Vec<Hit> {
        let prefix = under.map(|u| format!("{}\\", u.trim_end_matches(['\\', '/']).to_lowercase()));
        let mut v: Vec<(usize, u64)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.alive && !e.is_dir)
            .map(|(i, e)| (i, e.size))
            .collect();
        v.par_sort_unstable_by_key(|x| std::cmp::Reverse(x.1));
        v.into_iter()
            .map(|(i, _)| self.hit(i, 0))
            .filter(|h| prefix.as_ref().is_none_or(|p| h.path.to_lowercase().starts_with(p)))
            .take(limit)
            .collect()
    }

    pub fn hit(&self, i: usize, score: i32) -> Hit {
        let e = &self.entries[i];
        Hit {
            path: self.full_path(i),
            name: e.name.to_string(),
            dir: self.dirs[e.dir as usize].raw.to_string(),
            size: e.size,
            mtime: e.mtime,
            is_dir: e.is_dir,
            cloud: e.cloud,
            score,
        }
    }

    pub fn hit_for_path(&self, path: &str) -> Option<Hit> {
        self.find(path).map(|i| self.hit(i, 0))
    }

    /// Returns (top hits, total number of matches).
    pub fn search(&self, f: &Filter, limit: usize, opens: &HashMap<String, u32>, now: i64) -> (Vec<Hit>, usize) {
        let query_joined: String = f.terms.iter().filter_map(|t| t.first()).cloned().collect::<Vec<_>>().join(" ");
        let mut matches: Vec<(u32, i32, i64)> = self
            .entries
            .par_iter()
            .enumerate()
            .filter_map(|(i, e)| {
                if !e.alive {
                    return None;
                }
                let dir = &self.dirs[e.dir as usize];
                if !f.path_contains.is_empty() || !f.exts.is_empty() || f.after.is_some() || f.before.is_some()
                    || f.min_size.is_some() || f.max_size.is_some() || f.kind != crate::query::Kind::Any
                {
                    let pn = format!("{}\\{}", dir.norm, e.name_norm);
                    if !f.accepts_meta(&pn, ext_of(&e.name_norm), e.size, e.mtime, e.is_dir) {
                        return None;
                    }
                }
                let mut score = 0i32;
                for alts in &f.terms {
                    score += alts
                        .iter()
                        .filter_map(|t| term_score(&e.name_norm, &dir.norm, t).or_else(|| reading_score(e.reading.as_deref(), t)))
                        .max()?;
                }
                if f.has_terms() {
                    if *e.name_norm == *query_joined || stem(&e.name_norm) == query_joined {
                        score += 200;
                    }
                    score += 20 - (e.name_norm.chars().count() as i32 / 4).min(20);
                }
                let age = now - e.mtime;
                if age < 7 * 86400 {
                    score += 15;
                } else if age < 30 * 86400 {
                    score += 8;
                }
                Some((i as u32, score, e.mtime))
            })
            .collect();
        let total = matches.len();
        if !opens.is_empty() {
            for m in matches.iter_mut() {
                if let Some(c) = opens.get(&self.full_path(m.0 as usize)) {
                    m.1 += (*c as i32 * 8).min(60);
                }
            }
        }
        matches.par_sort_unstable_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
        let hits = matches.iter().take(limit).map(|&(i, s, _)| self.hit(i as usize, s)).collect();
        (hits, total)
    }

    pub fn recent(&self, limit: usize) -> Vec<Hit> {
        let mut v: Vec<(usize, i64)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.alive && !e.is_dir)
            .map(|(i, e)| (i, e.mtime))
            .collect();
        v.par_sort_unstable_by(|a, b| b.1.cmp(&a.1));
        v.into_iter().take(limit).map(|(i, _)| self.hit(i, 0)).collect()
    }
}

fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

fn is_boundary(c: char) -> bool {
    matches!(c, ' ' | '_' | '-' | '.' | '(' | ')' | '[' | ']' | '【' | '】' | '「' | '」' | '・' | '　')
}

/// Kana typed for a kanji name: 「けいり」 finds 「経理」 through the name's reading.
fn reading_score(reading: Option<&str>, t: &str) -> Option<i32> {
    let r = reading.filter(|r| !r.is_empty())?;
    let kana = t.chars().count() >= 2 && t.chars().all(|c| ('\u{3041}'..='\u{309f}').contains(&c) || c == 'ー');
    if !kana {
        return None;
    }
    let pos = r.find(t)?;
    Some(if pos == 0 { 70 } else { 45 })
}

fn term_score(name: &str, dir: &str, t: &str) -> Option<i32> {
    if let Some(pos) = name.find(t) {
        if pos == 0 {
            return Some(100);
        }
        let prev = name[..pos].chars().next_back().unwrap_or(' ');
        return Some(if is_boundary(prev) { 75 } else { 55 });
    }
    // A single character would match nearly every folder path, so it only counts in names.
    if t.chars().nth(1).is_some() && dir.contains(t) {
        return Some(25);
    }
    if t.chars().count() >= 3 {
        return subsequence_score(name, t).filter(|s| *s >= 40).map(|s| (s / 5) as i32);
    }
    None
}

// ---------------- scanning ----------------

pub struct Excludes {
    names: Vec<String>,
    prefixes: Vec<String>,
}

impl Excludes {
    pub fn new(list: &[String]) -> Self {
        let mut names = Vec::new();
        let mut prefixes = Vec::new();
        for e in list {
            let l = e.trim().to_lowercase().replace('/', "\\");
            if l.is_empty() {
                continue;
            }
            if l.contains('\\') || l.contains(':') {
                prefixes.push(l.trim_end_matches('\\').to_string());
            } else {
                names.push(l);
            }
        }
        Self { names, prefixes }
    }

    pub fn is_excluded(&self, path: &str) -> bool {
        let l = path.to_lowercase();
        if self.prefixes.iter().any(|p| l == *p || l.starts_with(&format!("{p}\\")) || (!p.contains(':') && l.contains(&format!("\\{p}\\")) || l.ends_with(&format!("\\{p}")))) {
            return true;
        }
        l.split('\\').any(|seg| self.names.iter().any(|n| n == seg))
    }
}

pub struct ScanProgress {
    pub count: AtomicU64,
}

/// Walks `roots` in parallel and returns every file and folder found.
pub fn scan(roots: &[PathBuf], ex: &Excludes, show_hidden: bool, progress: &ScanProgress) -> Vec<(String, FileMeta)> {
    let out = Mutex::new(Vec::with_capacity(200_000));
    rayon::scope(|s| {
        for r in roots {
            let r = r.clone();
            let out = &out;
            s.spawn(move |s| {
                if let Ok(m) = std::fs::metadata(&r) {
                    out.lock().push((r.to_string_lossy().trim_end_matches('\\').to_string(), FileMeta::from_metadata(&m)));
                }
                walk_dir(s, r, ex, show_hidden, out, progress);
            });
        }
    });
    let mut v = out.into_inner();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v.dedup_by(|a, b| a.0.eq_ignore_ascii_case(&b.0));
    v
}

fn walk_dir<'s>(
    s: &rayon::Scope<'s>,
    dir: PathBuf,
    ex: &'s Excludes,
    show_hidden: bool,
    out: &'s Mutex<Vec<(String, FileMeta)>>,
    progress: &'s ScanProgress,
) {
    let Ok(rd) = std::fs::read_dir(&dir) else { return };
    let mut local = Vec::new();
    for ent in rd.flatten() {
        let path = ent.path();
        let ps = path.to_string_lossy().into_owned();
        if ex.is_excluded(&ps) {
            continue;
        }
        let Ok(md) = ent.metadata() else { continue };
        let meta = FileMeta::from_metadata(&md);
        // Dot-folders (.git, .cache, .vscode…) are tool data, hidden by convention.
        let dot_dir = meta.is_dir && ent.file_name().to_string_lossy().starts_with('.');
        if (meta.hidden || dot_dir) && !show_hidden {
            continue;
        }
        let is_link = ent.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        if meta.is_dir && !is_link {
            let p = path.clone();
            s.spawn(move |s| walk_dir(s, p, ex, show_hidden, out, progress));
        }
        local.push((ps, meta));
    }
    progress.count.fetch_add(local.len() as u64, Ordering::Relaxed);
    out.lock().extend(local);
}

/// Scans a single folder subtree (used when the watcher sees a new folder appear).
pub fn scan_subtree(root: &Path, ex: &Excludes, show_hidden: bool) -> Vec<(String, FileMeta)> {
    let p = ScanProgress { count: AtomicU64::new(0) };
    scan(&[root.to_path_buf()], ex, show_hidden, &p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse;

    fn meta(is_dir: bool) -> FileMeta {
        FileMeta { size: 10, mtime: 1_000, is_dir, cloud: false, hidden: false }
    }

    fn sample() -> Index {
        Index::from_rows(
            [
                ("C:\\Users\\a\\経理\\見積書_A社.xlsx", false),
                ("C:\\Users\\a\\経理", true),
                ("C:\\Users\\a\\Documents\\ミツモリ メモ.txt", false),
                ("C:\\Users\\a\\Documents\\report_2026.pdf", false),
                ("C:\\Users\\a\\Documents\\old\\report_2025.pdf", false),
            ]
            .into_iter()
            .map(|(p, d)| (p.to_string(), meta(d))),
        )
    }

    #[test]
    fn name_search() {
        let ix = sample();
        let (h, n) = ix.search(&parse("見積"), 10, &HashMap::new(), 0);
        assert_eq!(n, 1);
        assert_eq!(h[0].name, "見積書_A社.xlsx");
        // folder name + file word
        let (h, _) = ix.search(&parse("経理 a社"), 10, &HashMap::new(), 0);
        assert_eq!(h[0].name, "見積書_A社.xlsx");
        // katakana query finds hiragana-normalized names and vice versa
        let (h, _) = ix.search(&parse("みつもり"), 10, &HashMap::new(), 0);
        assert_eq!(h.len(), 1);
        let (h, _) = ix.search(&parse("mitsumori"), 10, &HashMap::new(), 0);
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn filters_and_fuzzy() {
        let ix = sample();
        let (h, n) = ix.search(&parse("report ext:pdf in:old"), 10, &HashMap::new(), 0);
        assert_eq!(n, 1);
        assert_eq!(h[0].name, "report_2025.pdf");
        let (h, _) = ix.search(&parse("rprt26"), 10, &HashMap::new(), 0);
        assert_eq!(h[0].name, "report_2026.pdf");
        let (_, n) = ix.search(&parse("type:folder"), 10, &HashMap::new(), 0);
        assert_eq!(n, 1);
    }

    #[test]
    fn single_char_matches_names_only() {
        let ix = sample();
        // "o" appears in the folder "Documents" but only report_*.pdf / memo have it in the name.
        let (h, _) = ix.search(&parse("o"), 10, &HashMap::new(), 0);
        assert!(h.iter().all(|x| x.name.to_lowercase().contains('o')), "{h:?}");
    }

    #[test]
    fn reading_search() {
        let mut ix = sample();
        let names = ix.names_needing_reading();
        assert!(names.contains(&"経理".to_string()) && names.contains(&"見積書_A社".to_string()));
        let mut m = HashMap::new();
        m.insert("経理".to_string(), "ケイリ".to_string());
        m.insert("見積書_A社".to_string(), "みつもりしょ_aしゃ".to_string());
        ix.set_readings(&m);
        let (h, _) = ix.search(&parse("けいり"), 10, &HashMap::new(), 0);
        assert_eq!(h[0].name, "経理");
        let (h, _) = ix.search(&parse("mitsumori"), 10, &HashMap::new(), 0);
        assert!(h.iter().any(|x| x.name == "見積書_A社.xlsx"), "romaji -> kana -> reading: {h:?}");
        assert!(ix.names_needing_reading().is_empty());
    }

    #[test]
    fn upsert_remove() {
        let mut ix = sample();
        ix.remove("C:\\Users\\a\\経理");
        assert_eq!(ix.len(), 3);
        assert!(!ix.contains("C:\\Users\\a\\経理\\見積書_A社.xlsx"));
        ix.upsert("C:\\Users\\a\\new.txt", &meta(false));
        assert!(ix.contains("c:\\users\\a\\NEW.txt"));
    }

    #[test]
    fn excludes() {
        let ex = Excludes::new(&["node_modules".into(), "C:\\Windows".into(), "AppData".into()]);
        assert!(ex.is_excluded("C:\\Users\\a\\proj\\node_modules"));
        assert!(ex.is_excluded("C:\\Windows\\System32"));
        assert!(ex.is_excluded("C:\\Users\\a\\AppData\\Local"));
        assert!(!ex.is_excluded("C:\\Users\\a\\Documents"));
    }

    #[test]
    fn real_scan() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sub/node_modules")).unwrap();
        std::fs::write(d.path().join("sub/a.txt"), "x").unwrap();
        std::fs::write(d.path().join("sub/node_modules/b.txt"), "x").unwrap();
        let ex = Excludes::new(&["node_modules".into()]);
        let rows = scan_subtree(d.path(), &ex, false);
        let names: Vec<_> = rows.iter().map(|(p, _)| p.rsplit('\\').next().unwrap().to_string()).collect();
        assert!(names.contains(&"a.txt".to_string()));
        assert!(!names.contains(&"b.txt".to_string()));
    }
}
