//! Background work: initial scan, live updates from the file watcher, and full-text indexing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;

use crate::db::Db;
use crate::extract;
use crate::index::{Excludes, FileMeta, Index, ScanProgress, ext_of, scan, scan_subtree};
use crate::normalize::normalize;
use crate::settings::Settings;

/// Upper bound on characters kept per document in the full-text index.
const CONTENT_MAX_CHARS: usize = 300_000;

#[derive(Default)]
pub struct Status {
    pub scanning: AtomicBool,
    pub scan_progress: Arc<ScanProgress>,
    pub content_running: AtomicBool,
    pub content_total: AtomicU64,
    pub content_done: AtomicU64,
    pub last_scan: AtomicI64,
}

impl Default for ScanProgress {
    fn default() -> Self {
        Self { count: AtomicU64::new(0) }
    }
}

#[derive(Serialize)]
pub struct StatusView {
    pub indexed: usize,
    pub scanning: bool,
    pub scanned: u64,
    pub content_running: bool,
    pub content_total: u64,
    pub content_done: u64,
    pub content_indexed: i64,
    pub last_scan: i64,
    pub hotkey_error: Option<String>,
}

enum ContentMsg {
    FullPass,
    File(String),
    Remove(String),
}

pub struct Engine {
    pub db: Db,
    pub index: RwLock<Index>,
    pub settings: RwLock<Settings>,
    settings_path: PathBuf,
    pub status: Status,
    pub opens: RwLock<HashMap<String, u32>>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    content_tx: Mutex<Option<Sender<ContentMsg>>>,
    scan_lock: Mutex<()>,
    /// Set when the launcher shortcut could not be registered.
    pub hotkey_error: Mutex<Option<String>>,
}

impl Engine {
    pub fn new(data_dir: &Path) -> anyhow::Result<Arc<Self>> {
        let settings_path = data_dir.join("settings.json");
        let settings = Settings::load(&settings_path);
        let db = Db::open(&data_dir.join("sagasu.db"))?;
        let opens = db.open_counts().unwrap_or_default();
        let index = Index::from_rows(db.load_snapshot().unwrap_or_default().into_iter().map(|r| {
            (r.path, FileMeta { size: r.size, mtime: r.mtime, is_dir: r.is_dir, cloud: false, hidden: false })
        }));
        Ok(Arc::new(Self {
            db,
            index: RwLock::new(index),
            settings: RwLock::new(settings),
            settings_path,
            status: Status::default(),
            opens: RwLock::new(opens),
            watcher: Mutex::new(None),
            content_tx: Mutex::new(None),
            scan_lock: Mutex::new(()),
            hotkey_error: Mutex::new(None),
        }))
    }

    pub fn save_settings(&self, s: Settings) -> anyhow::Result<()> {
        s.save(&self.settings_path)?;
        *self.settings.write() = s;
        Ok(())
    }

    pub fn status(&self) -> StatusView {
        StatusView {
            indexed: self.index.read().len(),
            scanning: self.status.scanning.load(Ordering::Relaxed),
            scanned: self.status.scan_progress.count.load(Ordering::Relaxed),
            content_running: self.status.content_running.load(Ordering::Relaxed),
            content_total: self.status.content_total.load(Ordering::Relaxed),
            content_done: self.status.content_done.load(Ordering::Relaxed),
            content_indexed: self.db.content_count().unwrap_or(0),
            last_scan: self.status.last_scan.load(Ordering::Relaxed),
            hotkey_error: self.hotkey_error.lock().clone(),
        }
    }

    /// Starts the content worker and runs the first full scan in the background.
    pub fn start(self: &Arc<Self>) {
        let (tx, rx) = channel();
        *self.content_tx.lock() = Some(tx);
        let me = self.clone();
        std::thread::Builder::new().name("content".into()).spawn(move || me.content_loop(rx)).ok();
        self.rescan();
    }

    pub fn rescan(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::Builder::new().name("scan".into()).spawn(move || me.full_scan()).ok();
    }

    fn full_scan(self: &Arc<Self>) {
        let Some(_guard) = self.scan_lock.try_lock() else { return };
        self.status.scanning.store(true, Ordering::Relaxed);
        self.status.scan_progress.count.store(0, Ordering::Relaxed);
        let (roots, ex, hidden) = {
            let s = self.settings.read();
            (s.root_paths(), Excludes::new(&s.excludes), s.show_hidden)
        };
        let t = std::time::Instant::now();
        let rows = scan(&roots, &ex, hidden, &self.status.scan_progress);
        log::info!("scanned {} entries in {:?}", rows.len(), t.elapsed());
        let ix = Index::from_rows(rows.into_iter());
        let snapshot = ix.snapshot_rows();
        *self.index.write() = ix;
        self.status.scanning.store(false, Ordering::Relaxed);
        self.status.last_scan.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
        if let Err(e) = self.db.save_snapshot(snapshot.iter().map(|(p, s, m, d)| (p.as_str(), *s, *m, *d))) {
            log::warn!("snapshot save failed: {e}");
        }
        self.restart_watcher(&roots);
        self.send(ContentMsg::FullPass);
    }

    fn send(&self, m: ContentMsg) {
        if let Some(tx) = self.content_tx.lock().as_ref() {
            let _ = tx.send(m);
        }
    }

    // ---------------- watcher ----------------

    fn restart_watcher(self: &Arc<Self>, roots: &[PathBuf]) {
        let (tx, rx) = channel::<notify::Result<notify::Event>>();
        let mut w = match notify::recommended_watcher(tx) {
            Ok(w) => w,
            Err(e) => {
                log::warn!("watcher unavailable: {e}");
                return;
            }
        };
        for r in roots {
            if let Err(e) = w.watch(r, RecursiveMode::Recursive) {
                log::warn!("cannot watch {}: {e}", r.display());
            }
        }
        *self.watcher.lock() = Some(w);
        let me = self.clone();
        std::thread::Builder::new().name("watch".into()).spawn(move || me.watch_loop(rx)).ok();
    }

    fn watch_loop(self: Arc<Self>, rx: Receiver<notify::Result<notify::Event>>) {
        // Batch events for half a second so bursts (unzip, git checkout) are handled once.
        while let Ok(first) = rx.recv() {
            let mut paths: HashSet<PathBuf> = HashSet::new();
            let mut push = |ev: notify::Result<notify::Event>| {
                if let Ok(ev) = ev
                    && !matches!(ev.kind, notify::EventKind::Access(_))
                {
                    paths.extend(ev.paths);
                }
            };
            push(first);
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
                match rx.recv_timeout(left) {
                    Ok(ev) => push(ev),
                    Err(_) => break,
                }
            }
            self.apply_changes(paths);
        }
        // Sender dropped: a newer watcher replaced this one.
    }

    fn apply_changes(&self, paths: HashSet<PathBuf>) {
        let (ex, hidden) = {
            let s = self.settings.read();
            (Excludes::new(&s.excludes), s.show_hidden)
        };
        let mut content = Vec::new();
        for p in paths {
            let ps = p.to_string_lossy().into_owned();
            if ex.is_excluded(&ps) || (!hidden && in_dot_dir(&ps)) {
                continue;
            }
            match std::fs::symlink_metadata(&p) {
                Ok(md) => {
                    let meta = FileMeta::from_metadata(&md);
                    if meta.hidden && !hidden {
                        continue;
                    }
                    let is_new_dir = meta.is_dir && !self.index.read().contains(&ps);
                    self.index.write().upsert(&ps, &meta);
                    if is_new_dir {
                        // A folder moved or copied in: its children produce no events of their own.
                        let rows = scan_subtree(&p, &ex, hidden);
                        let mut ix = self.index.write();
                        for (cp, cm) in rows {
                            if !cm.is_dir {
                                content.push(ContentMsg::File(cp.clone()));
                            }
                            ix.upsert(&cp, &cm);
                        }
                    } else if !meta.is_dir {
                        content.push(ContentMsg::File(ps));
                    }
                }
                Err(_) => {
                    self.index.write().remove(&ps);
                    content.push(ContentMsg::Remove(ps));
                }
            }
        }
        for m in content {
            self.send(m);
        }
    }

    // ---------------- full-text indexing ----------------

    fn content_loop(self: Arc<Self>, rx: Receiver<ContentMsg>) {
        while let Ok(msg) = rx.recv() {
            let res = match msg {
                ContentMsg::FullPass => self.content_full_pass(&rx),
                ContentMsg::File(p) => self.content_one(&p),
                ContentMsg::Remove(p) => self.db.conn().and_then(|c| self.db.remove_content(&c, &p)),
            };
            if let Err(e) = res {
                log::warn!("content indexing: {e}");
            }
        }
    }

    fn content_limits(&self) -> (bool, u64) {
        let s = self.settings.read();
        (s.content_enabled, s.content_max_mb * 1024 * 1024)
    }

    fn content_one(&self, path: &str) -> anyhow::Result<()> {
        let (enabled, max) = self.content_limits();
        if !enabled {
            return Ok(());
        }
        let Some(hit) = self.index.read().hit_for_path(path) else { return Ok(()) };
        let ext = ext_of(&normalize(&hit.name)).to_string();
        if hit.is_dir || hit.cloud || hit.size > max || !extract::is_supported(&ext) {
            return Ok(());
        }
        let c = self.db.conn()?;
        let body = extract::extract_text(Path::new(path), CONTENT_MAX_CHARS).ok().flatten().unwrap_or_default();
        self.db.put_content(&c, path, hit.mtime, hit.size, &body)
    }

    fn content_full_pass(&self, rx: &Receiver<ContentMsg>) -> anyhow::Result<()> {
        let (enabled, max) = self.content_limits();
        if !enabled {
            return Ok(());
        }
        let stored = self.db.content_mtimes()?;
        let candidates = self.index.read().content_candidates(max, extract::is_supported);
        let live: HashSet<&str> = candidates.iter().map(|c| c.path.as_str()).collect();
        let todo: Vec<_> = candidates.iter().filter(|c| stored.get(&c.path) != Some(&c.mtime)).collect();
        let mut c = self.db.conn()?;
        {
            let tx = c.transaction()?;
            for p in stored.keys().filter(|p| !live.contains(p.as_str())) {
                self.db.remove_content(&tx, p)?;
            }
            tx.commit()?;
        }
        self.status.content_total.store(todo.len() as u64, Ordering::Relaxed);
        self.status.content_done.store(0, Ordering::Relaxed);
        self.status.content_running.store(true, Ordering::Relaxed);
        // Newest files first: they are the ones people look for.
        let mut todo = todo;
        todo.sort_by_key(|c| std::cmp::Reverse(c.mtime));
        for chunk in todo.chunks(20) {
            if !self.settings.read().content_enabled {
                break;
            }
            let bodies: Vec<_> = chunk
                .iter()
                .map(|cand| {
                    let body = extract::extract_text(Path::new(&cand.path), CONTENT_MAX_CHARS).ok().flatten().unwrap_or_default();
                    (cand, body)
                })
                .collect();
            let tx = c.transaction()?;
            for (cand, body) in &bodies {
                self.db.put_content(&tx, &cand.path, cand.mtime, cand.size, body)?;
            }
            tx.commit()?;
            self.status.content_done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            // Stay polite to the rest of the PC, and let a newer full pass take over.
            std::thread::sleep(Duration::from_millis(15));
            if let Ok(next) = rx.try_recv() {
                match next {
                    ContentMsg::FullPass => {
                        self.status.content_running.store(false, Ordering::Relaxed);
                        return self.content_full_pass(rx);
                    }
                    ContentMsg::File(p) => self.content_one(&p)?,
                    ContentMsg::Remove(p) => self.db.remove_content(&c, &p)?,
                }
            }
        }
        self.status.content_running.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub fn record_open(&self, path: &str) {
        let _ = self.db.record_open(path);
        *self.opens.write().entry(path.to_string()).or_insert(0) += 1;
    }
}

/// True when a parent folder of `path` is a dot-folder (skipped by the scan unless hidden files are shown).
fn in_dot_dir(path: &str) -> bool {
    let mut segs: Vec<&str> = path.split(['\\', '/']).collect();
    segs.pop();
    segs.iter().any(|s| s.starts_with('.') && *s != "." && *s != "..")
}

#[cfg(test)]
mod tests {
    #[test]
    fn dot_dirs() {
        assert!(super::in_dot_dir("C:\\Users\\a\\.git\\config"));
        assert!(!super::in_dot_dir("C:\\Users\\a\\.gitignore"));
        assert!(!super::in_dot_dir("C:\\Users\\a\\docs\\a.txt"));
    }
}
