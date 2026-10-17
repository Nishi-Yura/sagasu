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
use crate::rules::Rule;
use crate::settings::Settings;
use crate::{ocr, reading};

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
    pub reading_total: AtomicU64,
    pub reading_done: AtomicU64,
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
    pub reading_total: u64,
    pub reading_done: u64,
    /// Latest auto-organize result to show as a notification: (sequence, message, is_error).
    pub rule_notice: (u64, String, bool),
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
    /// New files in rule folders, waiting until they stop changing: path ↁE(size, last change).
    rule_pending: Mutex<HashMap<PathBuf, (u64, std::time::Instant)>>,
    reading_tx: Mutex<Option<Sender<()>>>,
    rule_notice: Mutex<(u64, String, bool)>,
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
            rule_pending: Mutex::new(HashMap::new()),
            reading_tx: Mutex::new(None),
            rule_notice: Mutex::new((0, String::new(), false)),
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
            reading_total: self.status.reading_total.load(Ordering::Relaxed),
            reading_done: self.status.reading_done.load(Ordering::Relaxed),
            rule_notice: self.rule_notice.lock().clone(),
        }
    }

    /// Starts the content worker and runs the first full scan in the background.
    pub fn start(self: &Arc<Self>) {
        let (tx, rx) = channel();
        *self.content_tx.lock() = Some(tx);
        let me = self.clone();
        std::thread::Builder::new().name("content".into()).spawn(move || me.content_loop(rx)).ok();
        let (rtx, rrx) = channel();
        *self.reading_tx.lock() = Some(rtx);
        let me = self.clone();
        std::thread::Builder::new().name("reading".into()).spawn(move || me.reading_loop(rrx)).ok();
        let me = self.clone();
        std::thread::Builder::new().name("rules".into()).spawn(move || me.rules_loop()).ok();
        self.rescan();
    }

    /// Re-applies the watched folders after the rules changed.
    pub fn refresh_watch(self: &Arc<Self>) {
        let roots = self.settings.read().root_paths();
        self.restart_watcher(&roots);
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
        *self.index.write() = ix;
        self.status.scanning.store(false, Ordering::Relaxed);
        self.status.last_scan.store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
        // Readers keep searching while the snapshot is written; watcher updates wait for it.
        if let Err(e) = self.db.save_snapshot(self.index.read().snapshot_iter()) {
            log::warn!("snapshot save failed: {e}");
        }
        self.restart_watcher(&roots);
        self.send(ContentMsg::FullPass);
        self.wake_reading();
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
        // Rule folders outside the search locations still need to be watched (top level only).
        let rule_dirs: Vec<PathBuf> = self.settings.read().rules.iter().filter(|r| r.enabled).map(|r| PathBuf::from(&r.folder)).collect();
        for d in rule_dirs.iter().filter(|d| !under_any(d, roots)) {
            if let Err(e) = w.watch(d, RecursiveMode::NonRecursive) {
                log::warn!("cannot watch rule folder {}: {e}", d.display());
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
        let (ex, hidden, roots, rule_dirs) = {
            let s = self.settings.read();
            let rule_dirs: Vec<PathBuf> = s.rules.iter().filter(|r| r.enabled).map(|r| PathBuf::from(&r.folder)).collect();
            (Excludes::new(&s.excludes), s.show_hidden, s.root_paths(), rule_dirs)
        };
        let mut content = Vec::new();
        let mut new_names = false;
        for p in paths {
            // Files landing in a rule folder are queued; they are moved once they stop changing.
            if p.parent().is_some_and(|parent| rule_dirs.iter().any(|d| same_path(d, parent)))
                && let Ok(md) = std::fs::metadata(&p)
                && md.is_file()
            {
                self.rule_pending.lock().insert(p.clone(), (md.len(), std::time::Instant::now()));
            }
            let ps = p.to_string_lossy().into_owned();
            if !under_any(&p, &roots) || ex.is_excluded(&ps) || (!hidden && in_dot_dir(&ps)) {
                continue;
            }
            match std::fs::symlink_metadata(&p) {
                Ok(md) => {
                    let meta = FileMeta::from_metadata(&md);
                    if meta.hidden && !hidden {
                        continue;
                    }
                    let is_new_dir = meta.is_dir && !self.index.read().contains(&ps);
                    new_names |= reading::has_kanji(&ps);
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
        if new_names {
            self.wake_reading();
        }
    }

    // ---------------- kanji readings ----------------

    fn wake_reading(&self) {
        if let Some(tx) = self.reading_tx.lock().as_ref() {
            let _ = tx.send(());
        }
    }

    /// Looks up readings for names with kanji: first in the database cache, then with the Windows IME.
    fn reading_loop(self: Arc<Self>, rx: Receiver<()>) {
        let mut ime: Option<Option<reading::Readings>> = None;
        while rx.recv().is_ok() {
            while rx.try_recv().is_ok() {}
            let names = self.index.read().names_needing_reading();
            if names.is_empty() {
                continue;
            }
            let cache = self.db.readings().unwrap_or_default();
            let (known, missing): (Vec<String>, Vec<String>) = names.into_iter().partition(|n| cache.contains_key(n));
            let found: HashMap<String, String> = known.into_iter().filter_map(|n| cache.get(&n).map(|r| (n, r.clone()))).collect();
            self.index.write().set_readings(&found);
            if missing.is_empty() {
                continue;
            }
            let Some(ime) = ime.get_or_insert_with(reading::Readings::new) else {
                log::info!("Japanese IME not available; reading search disabled");
                continue;
            };
            self.status.reading_total.store(missing.len() as u64, Ordering::Relaxed);
            self.status.reading_done.store(0, Ordering::Relaxed);
            let t = std::time::Instant::now();
            for chunk in missing.chunks(200) {
                let rows: Vec<(String, String)> =
                    chunk.iter().cloned().zip(ime.readings(chunk)).map(|(n, r)| (n, r.unwrap_or_default())).collect();
                let _ = self.db.put_readings(&rows);
                self.index.write().set_readings(&rows.into_iter().collect());
                self.status.reading_done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            self.status.reading_total.store(0, Ordering::Relaxed);
            log::info!("read {} kanji names in {:?}", missing.len(), t.elapsed());
        }
    }

    // ---------------- auto-organize rules ----------------

    fn rules_loop(self: Arc<Self>) {
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let ready: Vec<PathBuf> = {
                let mut pending = self.rule_pending.lock();
                let mut ready = Vec::new();
                pending.retain(|p, (size, since)| {
                    let Ok(md) = std::fs::metadata(p) else { return false };
                    if md.len() != *size {
                        *size = md.len();
                        *since = std::time::Instant::now();
                        return true;
                    }
                    // Unchanged for 5 seconds: the download or copy has finished.
                    if since.elapsed() >= Duration::from_secs(5) {
                        ready.push(p.clone());
                        return false;
                    }
                    true
                });
                ready
            };
            if ready.is_empty() {
                continue;
            }
            let rules: Vec<Rule> = self.settings.read().rules.iter().filter(|r| r.enabled).cloned().collect();
            let mut taken: HashSet<PathBuf> = HashSet::new();
            for rule in rules {
                let files: Vec<PathBuf> = ready.iter().filter(|p| !taken.contains(*p) && rule.matches(p)).cloned().collect();
                if files.is_empty() {
                    continue;
                }
                taken.extend(files.iter().cloned());
                let (done, errors) = rule.run(&files);
                for e in &errors {
                    log::warn!("rule {}: {e}", rule.name);
                }
                if !done.is_empty() {
                    log::info!("rule {} moved {} file(s)", rule.name, done.len());
                    let _ = self.db.add_history(&rule.folder, &format!("自動整理ルール「{}」", rule.name), &done);
                }
                let msg = match (done.len(), errors.first()) {
                    (n, None) => format!("自動整理「{}」: {n}件を移動しました（履歴から元に戻せます）", rule.name),
                    (0, Some(e)) => format!("自動整理「{}」: 移動できませんでした — {e}", rule.name),
                    (n, Some(e)) => format!("自動整理「{}」: {n}件を移動、{}件は失敗 — {e}", rule.name, errors.len()),
                };
                let mut notice = self.rule_notice.lock();
                *notice = (notice.0 + 1, msg, !errors.is_empty());
            }
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

    /// (enabled, max file size, include source code / logs, read images with OCR)
    fn content_limits(&self) -> (bool, u64, bool, bool) {
        let s = self.settings.read();
        (s.content_enabled, s.content_max_mb * 1024 * 1024, s.content_code, s.content_ocr && ocr_ready())
    }

    fn content_one(&self, path: &str) -> anyhow::Result<()> {
        let (enabled, max, code, use_ocr) = self.content_limits();
        if !enabled {
            return Ok(());
        }
        let Some(hit) = self.index.read().hit_for_path(path) else { return Ok(()) };
        let ext = ext_of(&normalize(&hit.name)).to_string();
        if hit.is_dir || hit.cloud || hit.size > max || !(extract::should_index(&ext, code) || (use_ocr && ocr::is_image(&ext))) {
            return Ok(());
        }
        if !code && ocr::is_image(&ext) && in_dev_project(Path::new(path), &mut HashMap::new()) {
            return Ok(());
        }
        let c = self.db.conn()?;
        let body = read_text(Path::new(path));
        self.db.put_content(&c, path, hit.mtime, hit.size, &body)
    }

    fn content_full_pass(&self, rx: &Receiver<ContentMsg>) -> anyhow::Result<()> {
        let (enabled, max, code, use_ocr) = self.content_limits();
        if !enabled {
            return Ok(());
        }
        let stored = self.db.content_mtimes()?;
        let mut candidates = self.index.read().content_candidates(max, |e| extract::should_index(e, code) || (use_ocr && ocr::is_image(e)));
        if !code {
            let mut cache = HashMap::new();
            candidates.retain(|c| !ocr::is_image(&ext_lower(&c.path)) || !in_dev_project(Path::new(&c.path), &mut cache));
        }
        let live: HashSet<&str> = candidates.iter().map(|c| c.path.as_str()).collect();
        let todo: Vec<_> = candidates.iter().filter(|c| stored.get(&c.path) != Some(&c.mtime)).collect();
        let mut c = self.db.conn()?;
        {
            let tx = c.transaction()?;
            let mut removed = 0usize;
            for p in stored.keys().filter(|p| !live.contains(p.as_str())) {
                self.db.remove_content(&tx, p)?;
                removed += 1;
            }
            tx.commit()?;
            if removed > 0 {
                log::info!("removed {removed} documents from the full-text index");
                let _ = self.db.shrink();
            }
        }
        let started = std::time::Instant::now();
        let total = todo.len();
        self.status.content_total.store(total as u64, Ordering::Relaxed);
        self.status.content_done.store(0, Ordering::Relaxed);
        self.status.content_running.store(true, Ordering::Relaxed);
        // Documents first (quick to read), images after (OCR is slower); newest first within each,
        // because recent files are the ones people look for.
        let mut todo: Vec<(String, i64, u64)> = todo.into_iter().map(|c| (c.path.clone(), c.mtime, c.size)).collect();
        todo.sort_by_key(|(p, m, _)| (ocr::is_image(&ext_lower(p)), std::cmp::Reverse(*m)));
        let todo = Arc::new(todo);

        // Reader threads extract text in parallel; this thread writes to SQLite in batches.
        let cancel = Arc::new(AtomicBool::new(false));
        let (out_tx, out_rx) = std::sync::mpsc::sync_channel::<(usize, String, Duration)>(128);
        {
            let todo = todo.clone();
            let cancel = cancel.clone();
            content_pool().spawn(move || {
                use rayon::prelude::*;
                (0..todo.len()).into_par_iter().with_max_len(4).for_each_with(out_tx, |tx, i| {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let t = std::time::Instant::now();
                    let body = read_text(Path::new(&todo[i].0));
                    let _ = tx.send((i, body, t.elapsed()));
                });
            });
        }
        let mut batch: Vec<(usize, String, Duration)> = Vec::with_capacity(128);
        let mut by_ext: HashMap<String, (u32, Duration)> = HashMap::new();
        let mut restart = false;
        loop {
            let finished = match out_rx.recv_timeout(Duration::from_millis(400)) {
                Ok(item) => {
                    batch.push(item);
                    while batch.len() < 128 {
                        match out_rx.try_recv() {
                            Ok(item) => batch.push(item),
                            Err(_) => break,
                        }
                    }
                    false
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
            };
            if !batch.is_empty() {
                let tx = c.transaction()?;
                for (i, body, took) in &batch {
                    let (path, mtime, size) = &todo[*i];
                    let e = by_ext.entry(ext_lower(path)).or_default();
                    e.0 += 1;
                    e.1 += *took;
                    self.db.put_content(&tx, path, *mtime, *size, body)?;
                }
                tx.commit()?;
                self.status.content_done.fetch_add(batch.len() as u64, Ordering::Relaxed);
                batch.clear();
            }
            if finished {
                break;
            }
            if !self.settings.read().content_enabled {
                cancel.store(true, Ordering::Relaxed);
            }
            // Live updates from the watcher are handled between batches; a newer full pass takes over.
            while let Ok(next) = rx.try_recv() {
                match next {
                    ContentMsg::FullPass => {
                        cancel.store(true, Ordering::Relaxed);
                        restart = true;
                    }
                    ContentMsg::File(p) => self.content_one(&p)?,
                    ContentMsg::Remove(p) => self.db.remove_content(&c, &p)?,
                }
            }
        }
        if restart {
            self.status.content_running.store(false, Ordering::Relaxed);
            return self.content_full_pass(rx);
        }
        self.status.content_running.store(false, Ordering::Relaxed);
        if total > 0 {
            log::info!("content pass: {total} files in {:?}", started.elapsed());
            let mut slow: Vec<_> = by_ext.into_iter().collect();
            slow.sort_by_key(|(_, (_, d))| std::cmp::Reverse(*d));
            let top: Vec<String> = slow.iter().take(6).map(|(e, (n, d))| format!("{e}: {n} files {:.1}s", d.as_secs_f64())).collect();
            log::info!("content time by type: {}", top.join(", "));
        }
        Ok(())
    }

    pub fn record_open(&self, path: &str) {
        let _ = self.db.record_open(path);
        *self.opens.write().entry(path.to_string()).or_insert(0) += 1;
    }
}

/// Files whose presence marks a folder as a software project.
const PROJECT_MARKERS: &[&str] = &[
    ".git", "package.json", "Cargo.toml", "ProjectSettings", "pom.xml", "build.gradle", "go.mod", "pyproject.toml",
    "composer.json", "Gemfile", "CMakeLists.txt", "pubspec.yaml",
];

/// Whether `path` lies inside a software project (some parent folder holds a project marker).
/// Images there are textures, icons and UI assets rather than screenshots or scans, so they are
/// not read with OCR unless the user opts into indexing source code. `cache` remembers folders.
fn in_dev_project(path: &Path, cache: &mut HashMap<PathBuf, bool>) -> bool {
    let mut chain = Vec::new();
    let mut result = false;
    let mut cur = path.parent();
    while let Some(dir) = cur {
        if let Some(&known) = cache.get(dir) {
            result = known;
            break;
        }
        chain.push(dir.to_path_buf());
        if PROJECT_MARKERS.iter().any(|m| dir.join(m).exists()) {
            result = true;
            break;
        }
        cur = dir.parent();
    }
    // Every folder walked lies between the file and the answer, so they share it.
    for dir in chain {
        cache.insert(dir, result);
    }
    result
}

fn ext_lower(path: &str) -> String {
    Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Threads for reading file contents: all cores but one, below normal priority so the PC stays responsive.
fn content_pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).saturating_sub(1).max(2);
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("content-{i}"))
            .start_handler(|_| lower_thread_priority())
            .build()
            .expect("content thread pool")
    })
}

#[cfg(windows)]
fn lower_thread_priority() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
    // SAFETY: changes only the priority of the calling thread.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

#[cfg(not(windows))]
fn lower_thread_priority() {}

/// Text of a document, or of an image through OCR. Unreadable files index as empty text.
fn read_text(path: &Path) -> String {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let r = if ocr::is_image(&ext) {
        let _slot = OcrSlot::acquire();
        ocr::ocr_image(path, CONTENT_MAX_CHARS)
    } else {
        extract::extract_text(path, CONTENT_MAX_CHARS)
    };
    r.ok().flatten().unwrap_or_default()
}

/// At most this many images are decoded and recognized at once: a large photo needs tens of MB
/// while it is processed, so unbounded parallel OCR could take a gigabyte.
const OCR_SLOTS: usize = 4;

struct OcrSlot;

static OCR_IN_USE: (parking_lot::Mutex<usize>, parking_lot::Condvar) = (parking_lot::Mutex::new(0), parking_lot::Condvar::new());

impl OcrSlot {
    fn acquire() -> Self {
        let mut n = OCR_IN_USE.0.lock();
        while *n >= OCR_SLOTS {
            OCR_IN_USE.1.wait(&mut n);
        }
        *n += 1;
        OcrSlot
    }
}

impl Drop for OcrSlot {
    fn drop(&mut self) {
        *OCR_IN_USE.0.lock() -= 1;
        OCR_IN_USE.1.notify_one();
    }
}

/// Windows OCR availability, checked once.
fn ocr_ready() -> bool {
    static READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *READY.get_or_init(ocr::available)
}

fn same_path(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| p.to_string_lossy().to_lowercase().replace('/', "\\").trim_end_matches('\\').to_string();
    n(a) == n(b)
}

fn under_any(p: &Path, roots: &[PathBuf]) -> bool {
    let l = p.to_string_lossy().to_lowercase();
    roots.iter().any(|r| {
        let r = r.to_string_lossy().to_lowercase();
        let r = r.trim_end_matches('\\');
        l == r || l.starts_with(&format!("{r}\\"))
    })
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
    fn dev_project_detection() {
        let d = tempfile::tempdir().unwrap();
        let proj = d.path().join("myapp");
        std::fs::create_dir_all(proj.join("assets/icons")).unwrap();
        std::fs::write(proj.join("package.json"), "{}").unwrap();
        std::fs::create_dir_all(d.path().join("Screenshots")).unwrap();
        let mut cache = std::collections::HashMap::new();
        assert!(super::in_dev_project(&proj.join("assets/icons/logo.png"), &mut cache));
        assert!(super::in_dev_project(&proj.join("assets/bg.png"), &mut cache), "cached parent");
        assert!(!super::in_dev_project(&d.path().join("Screenshots/shot.png"), &mut cache));
    }

    #[test]
    fn dot_dirs() {
        assert!(super::in_dot_dir("C:\\Users\\a\\.git\\config"));
        assert!(!super::in_dot_dir("C:\\Users\\a\\.gitignore"));
        assert!(!super::in_dot_dir("C:\\Users\\a\\docs\\a.txt"));
    }
}
