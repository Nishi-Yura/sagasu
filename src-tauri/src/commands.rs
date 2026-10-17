//! Tauri commands called from the UI. Errors are returned as Japanese messages.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::db::{HistoryItem, Pin};
use crate::engine::{Engine, StatusView};
use crate::index::Hit;
use crate::normalize::normalize;
use crate::organize::{self, Plan, Preset};
use crate::query::{self, Filter};
use crate::settings::{self, Settings};

type Res<T> = Result<T, String>;
type Eng<'a> = State<'a, Arc<Engine>>;

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Res<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(err)?.map_err(err)
}

#[derive(Serialize)]
pub struct PinHit {
    pub alias: String,
    pub path: String,
    pub exists: bool,
}

#[derive(Serialize)]
pub struct SearchResult {
    pub pins: Vec<PinHit>,
    pub items: Vec<Hit>,
    pub total: usize,
    pub took_ms: u64,
    pub has_terms: bool,
}

#[derive(Serialize)]
pub struct ContentResult {
    pub path: String,
    pub snippet: String,
    pub hit: Option<Hit>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[tauri::command]
pub async fn search(engine: Eng<'_>, query: String, limit: Option<usize>, sort: Option<crate::index::SortBy>) -> Res<SearchResult> {
    let t = std::time::Instant::now();
    let f = query::parse(&query);
    let q = normalize(query.trim());
    let pins = if q.is_empty() {
        vec![]
    } else {
        engine
            .db
            .pins()
            .map_err(err)?
            .into_iter()
            .filter(|p| {
                let a = normalize(&p.alias);
                a.starts_with(&q) || (q.chars().count() >= 2 && a.contains(&q))
            })
            .map(|p| PinHit { exists: Path::new(&p.path).exists(), alias: p.alias, path: p.path })
            .collect()
    };
    if f.is_empty() {
        return Ok(SearchResult { pins, items: vec![], total: 0, took_ms: 0, has_terms: false });
    }
    let opens = engine.opens.read().clone();
    let (items, total) = engine.index.read().search_sorted(&f, limit.unwrap_or(150), &opens, now(), sort.unwrap_or_default());
    Ok(SearchResult { pins, items, total, took_ms: t.elapsed().as_millis() as u64, has_terms: f.has_terms() })
}

fn content_results(engine: &Engine, terms: &[String], f: &Filter, limit: usize) -> anyhow::Result<Vec<ContentResult>> {
    let hits = engine.db.search_content(terms, limit * 3)?;
    let ix = engine.index.read();
    let mut out = Vec::new();
    for h in hits {
        let hit = ix.hit_for_path(&h.path);
        if let Some(x) = &hit {
            let pn = normalize(&x.path);
            if !f.accepts_meta(&pn, crate::index::ext_of(&normalize(&x.name)), x.size, x.mtime, x.is_dir) {
                continue;
            }
        } else {
            continue;
        }
        out.push(ContentResult { path: h.path, snippet: h.snippet, hit });
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

#[tauri::command]
pub async fn search_content(engine: Eng<'_>, query: String, limit: Option<usize>) -> Res<Vec<ContentResult>> {
    let f = query::parse(&query);
    if f.raw_terms.is_empty() {
        return Ok(vec![]);
    }
    let engine = engine.inner().clone();
    blocking(move || content_results(&engine, &f.raw_terms, &f, limit.unwrap_or(40))).await
}

#[tauri::command]
pub fn recent(engine: Eng<'_>, limit: Option<usize>) -> Vec<Hit> {
    engine.index.read().recent(limit.unwrap_or(50))
}

#[tauri::command]
pub fn open_path(app: AppHandle, engine: Eng<'_>, path: String) -> Res<()> {
    use tauri_plugin_opener::OpenerExt;
    if !Path::new(&path).exists() {
        return Err(format!("見つかりません（移動または削除された可能性があります）: {path}"));
    }
    app.opener().open_path(&path, None::<&str>).map_err(err)?;
    engine.record_open(&path);
    Ok(())
}

#[tauri::command]
pub fn reveal_path(engine: Eng<'_>, path: String) -> Res<()> {
    let p = PathBuf::from(&path);
    if p.exists() {
        std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn().map_err(err)?;
    } else if let Some(parent) = p.parent().filter(|d| d.exists()) {
        std::process::Command::new("explorer.exe").arg(parent).spawn().map_err(err)?;
    } else {
        return Err(format!("見つかりません: {path}"));
    }
    engine.record_open(&path);
    Ok(())
}

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[derive(Serialize)]
pub struct Preview {
    pub hit: Option<Hit>,
    pub text: Option<String>,
    pub children: Vec<String>,
}

/// Up to `len` chars of `body`, starting a little before the first query word so the match is visible.
fn excerpt(body: &str, words: &[String], len: usize) -> String {
    let lower = body.to_lowercase();
    let first = words
        .iter()
        .filter(|w| !w.is_empty())
        .filter_map(|w| lower.find(&w.to_lowercase()))
        .min()
        .filter(|&p| lower.is_char_boundary(p))
        .map(|p| lower[..p].chars().count())
        .unwrap_or(0);
    let start = first.saturating_sub(300);
    let mut s: String = body.chars().skip(start).take(len).collect();
    if start > 0 {
        s.insert(0, '…');
    }
    s
}

#[tauri::command]
pub async fn preview(engine: Eng<'_>, path: String, words: Option<Vec<String>>) -> Res<Preview> {
    let engine = engine.inner().clone();
    let words = words.unwrap_or_default();
    blocking(move || {
        let hit = engine.index.read().hit_for_path(&path);
        let p = Path::new(&path);
        let mut children = Vec::new();
        let mut text = None;
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                children = rd.flatten().take(40).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            }
        } else if !hit.as_ref().is_some_and(|h| h.cloud) {
            let body = engine.db.content_body(&path).ok().flatten().or_else(|| {
                p.metadata()
                    .is_ok_and(|m| m.len() < 30 * 1024 * 1024)
                    .then(|| crate::extract::extract_text(p, 200_000).ok().flatten())
                    .flatten()
            });
            text = body.map(|b| excerpt(&b, &words, 4000));
        }
        Ok(Preview { hit, text, children })
    })
    .await
}

#[tauri::command]
pub fn status(engine: Eng<'_>) -> StatusView {
    engine.status()
}

#[tauri::command]
pub fn rescan(engine: Eng<'_>) {
    engine.inner().rescan();
}

// ---------------- pins ----------------

#[tauri::command]
pub fn list_pins(engine: Eng<'_>) -> Res<Vec<Pin>> {
    engine.db.pins().map_err(err)
}

#[tauri::command]
pub fn add_pin(engine: Eng<'_>, alias: String, path: String) -> Res<()> {
    let alias = alias.trim();
    if alias.is_empty() {
        return Err("あだ名を入力してください".into());
    }
    if !Path::new(&path).exists() {
        return Err(format!("見つかりません: {path}"));
    }
    engine.db.add_pin(alias, &path).map_err(err)
}

#[tauri::command]
pub fn remove_pin(engine: Eng<'_>, alias: String) -> Res<()> {
    engine.db.remove_pin(&alias).map_err(err)
}

// ---------------- organize ----------------

#[derive(Serialize)]
pub struct FolderSummary {
    pub files: usize,
    pub by_category: Vec<(String, usize)>,
    pub capped: bool,
}

#[tauri::command]
pub async fn organize_inspect(root: String, recursive: bool) -> Res<FolderSummary> {
    blocking(move || {
        let files = organize::list_files(Path::new(&root), recursive)?;
        let mut m: HashMap<&str, usize> = HashMap::new();
        for f in &files {
            *m.entry(organize::category(&f.ext)).or_default() += 1;
        }
        let mut by_category: Vec<(String, usize)> = m.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        by_category.sort_by_key(|c| std::cmp::Reverse(c.1));
        Ok(FolderSummary { capped: files.len() >= organize::MAX_FILES, files: files.len(), by_category })
    })
    .await
}

#[tauri::command]
pub async fn organize_preset(root: String, preset: Preset, recursive: bool) -> Res<Plan> {
    blocking(move || organize::preset_plan(Path::new(&root), preset, recursive)).await
}

#[derive(Serialize)]
pub struct ApplyResult {
    pub history_id: Option<i64>,
    pub moved: usize,
    pub errors: Vec<String>,
}

#[tauri::command]
pub async fn organize_apply(engine: Eng<'_>, plan: Plan) -> Res<ApplyResult> {
    let engine = engine.inner().clone();
    blocking(move || {
        let (done, errors) = organize::apply(&plan)?;
        let history_id = if done.is_empty() { None } else { Some(engine.db.add_history(&plan.root, &plan.instruction, &done)?) };
        Ok(ApplyResult { history_id, moved: done.len(), errors })
    })
    .await
}

#[tauri::command]
pub fn history(engine: Eng<'_>) -> Res<Vec<HistoryItem>> {
    engine.db.history(100).map_err(err)
}

#[derive(Serialize)]
pub struct UndoResult {
    pub restored: usize,
    pub errors: Vec<String>,
}

#[tauri::command]
pub async fn undo(engine: Eng<'_>, id: i64) -> Res<UndoResult> {
    let engine = engine.inner().clone();
    blocking(move || {
        let item = engine.db.history_item(id)?.ok_or_else(|| anyhow::anyhow!("履歴が見つかりません"))?;
        if item.undone {
            anyhow::bail!("この整理はすでに元に戻しています");
        }
        let (restored, errors) = organize::undo(&item.moves);
        engine.db.mark_undone(id)?;
        Ok(UndoResult { restored, errors })
    })
    .await
}

// ---------------- duplicates ----------------

#[tauri::command]
pub async fn find_duplicates(engine: Eng<'_>, root: Option<String>, min_mb: Option<f64>) -> Res<Vec<crate::dupes::DupGroup>> {
    // Before the first scan finishes we cannot tell cloud-only files apart, and reading one would download it.
    if engine.status.last_scan.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return Err("ファイルの一覧を作成中です。終わってからもう一度お試しください。".into());
    }
    let engine = engine.inner().clone();
    let min = (min_mb.unwrap_or(1.0).max(0.0) * 1024.0 * 1024.0) as u64;
    let root = root.filter(|r| !r.trim().is_empty());
    blocking(move || {
        let files = engine.index.read().files_at_least(min, root.as_deref());
        Ok(crate::dupes::find(files))
    })
    .await
}

/// Moves one file to the Recycle Bin (it can be restored from there).
#[tauri::command]
pub fn trash_path(engine: Eng<'_>, path: String) -> Res<()> {
    let p = Path::new(&path);
    if !p.is_file() {
        return Err(format!("見つかりません: {path}"));
    }
    trash::delete(p).map_err(|e| format!("ごみ箱に移動できませんでした: {e}"))?;
    engine.index.write().remove(&path);
    Ok(())
}

#[tauri::command]
pub fn large_files(engine: Eng<'_>, root: Option<String>, limit: Option<usize>) -> Vec<Hit> {
    let root = root.filter(|r| !r.trim().is_empty());
    engine.index.read().largest(limit.unwrap_or(100).min(500), root.as_deref())
}

// ---------------- auto-organize rules ----------------

/// Files the rule would move right now (nothing is moved).
#[tauri::command]
pub async fn rule_preview(rule: crate::rules::Rule) -> Res<Vec<crate::db::MoveRecord>> {
    blocking(move || rule.preview()).await
}

/// Runs the rule once on the files already in its folder and records it in the history.
#[tauri::command]
pub async fn rule_run(engine: Eng<'_>, rule: crate::rules::Rule) -> Res<ApplyResult> {
    let engine = engine.inner().clone();
    blocking(move || {
        let mut r = rule.clone();
        r.enabled = true;
        let files: Vec<PathBuf> = r.preview()?.iter().map(|m| PathBuf::from(&m.from)).collect();
        let (done, errors) = r.run(&files);
        let history_id = if done.is_empty() { None } else { Some(engine.db.add_history(&r.folder, &format!("自動整理ルール「{}」", r.name), &done)?) };
        Ok(ApplyResult { history_id, moved: done.len(), errors })
    })
    .await
}

// ---------------- bulk move / copy ----------------

/// Moves (`copy` = false) or copies the selected files and folders into `dest`.
/// Moves are recorded in the history so they can be undone.
#[tauri::command]
pub async fn transfer(engine: Eng<'_>, paths: Vec<String>, dest: String, copy: bool) -> Res<ApplyResult> {
    let engine = engine.inner().clone();
    blocking(move || {
        let (done, errors) = organize::transfer(&paths, Path::new(&dest), copy)?;
        let history_id = if !copy && !done.is_empty() {
            Some(engine.db.add_history(&dest, &format!("{}件をまとめて移動", done.len()), &done)?)
        } else {
            None
        };
        Ok(ApplyResult { history_id, moved: done.len(), errors })
    })
    .await
}

// ---------------- drag out ----------------

/// Starts a native drag of the given files, so they can be dropped on Explorer, mail or chat apps.
#[tauri::command]
pub fn start_drag(app: AppHandle, paths: Vec<String>) -> Res<()> {
    let files: Vec<PathBuf> = paths.iter().map(PathBuf::from).filter(|p| p.exists()).collect();
    if files.is_empty() {
        return Err("ファイルが見つかりません".into());
    }
    let window = app.get_webview_window("main").ok_or("window not found")?;
    let w = window.clone();
    let n = files.len();
    // DoDragDrop must run on the UI thread.
    app.run_on_main_thread(move || {
        let icon = drag::Image::Raw(include_bytes!("../icons/32x32.png").to_vec());
        let done = move |r: drag::DragResult, _| log::info!("drag of {n} file(s): {r:?}");
        if let Err(e) = drag::start_drag(&w, drag::DragItem::Files(files), icon, done, drag::Options::default()) {
            log::warn!("drag failed: {e}");
        }
    })
    .map_err(err)
}

// ---------------- backup ----------------

#[derive(Serialize, serde::Deserialize)]
pub struct Backup {
    pub app: String,
    pub version: String,
    pub exported: i64,
    pub settings: Settings,
    pub pins: Vec<Pin>,
}

/// Writes settings, rules and pins to a JSON file the user chose.
#[tauri::command]
pub fn export_backup(app: AppHandle, engine: Eng<'_>, path: String) -> Res<()> {
    let b = Backup {
        app: "Sagasu".into(),
        version: app.package_info().version.to_string(),
        exported: now(),
        settings: engine.settings.read().clone(),
        pins: engine.db.pins().map_err(err)?,
    };
    std::fs::write(&path, serde_json::to_string_pretty(&b).map_err(err)?).map_err(|e| format!("書き出せませんでした: {e}"))
}

/// Reads a backup: settings and rules are replaced, pins are added (same name = overwritten).
#[tauri::command]
pub fn import_backup(app: AppHandle, engine: Eng<'_>, path: String) -> Res<usize> {
    let text = std::fs::read_to_string(&path).map_err(|e| format!("読み込めませんでした: {e}"))?;
    let b: Backup = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|_| "Sagasu のバックアップファイルではありません".to_string())?;
    if b.app != "Sagasu" {
        return Err("Sagasu のバックアップファイルではありません".into());
    }
    let mut settings = b.settings;
    // Folders that do not exist on this PC are dropped instead of failing the whole import.
    settings.roots.retain(|r| Path::new(r).exists());
    if settings.roots.is_empty() {
        settings.roots = engine.settings.read().roots.clone();
    }
    settings.rules.retain(|r| Path::new(&r.folder).exists());
    settings.onboarded = true;
    save_settings(app, engine.clone(), settings)?;
    for p in &b.pins {
        engine.db.add_pin(&p.alias, &p.path).map_err(err)?;
    }
    Ok(b.pins.len())
}

// ---------------- updates ----------------

#[derive(Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub current: String,
    pub notes: Option<String>,
}

fn update_err(e: impl std::fmt::Display) -> String {
    log::warn!("update: {e}");
    format!("更新を確認できませんでした（インターネット接続を確認してください）: {e}")
}

fn updater(app: &AppHandle) -> Res<tauri_plugin_updater::Updater> {
    use tauri_plugin_updater::UpdaterExt;
    let mut b = app.updater_builder();
    // Debug builds can point at a local test server: SAGASU_UPDATE_URL=http://127.0.0.1:8765/latest.json
    if cfg!(debug_assertions)
        && let Ok(url) = std::env::var("SAGASU_UPDATE_URL")
    {
        b = b.endpoints(vec![url.parse().map_err(update_err)?]).map_err(update_err)?;
    }
    b.build().map_err(update_err)
}

/// Asks GitHub Releases for a newer version. `None` means up to date.
#[tauri::command]
pub async fn check_update(app: AppHandle) -> Res<Option<UpdateInfo>> {
    let update = updater(&app)?.check().await.map_err(update_err)?;
    Ok(update.map(|u| UpdateInfo { version: u.version.clone(), current: u.current_version.clone(), notes: u.body.clone() }))
}

/// Downloads the signed installer, runs it, and restarts Sagasu.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Res<()> {
    let Some(update) = updater(&app)?.check().await.map_err(update_err)? else {
        return Err("すでに最新版です".into());
    };
    log::info!("installing update {} → {}", update.current_version, update.version);
    update.download_and_install(|_, _| {}, || {}).await.map_err(|e| {
        log::error!("update install failed: {e}");
        format!("更新をインストールできませんでした: {e}")
    })?;
    app.restart();
}

#[tauri::command]
pub fn open_log_folder(app: AppHandle) -> Res<()> {
    let dir = app.path().app_log_dir().map_err(err)?;
    std::fs::create_dir_all(&dir).map_err(err)?;
    std::process::Command::new("explorer.exe").arg(&dir).spawn().map_err(err)?;
    Ok(())
}

// ---------------- settings ----------------

#[derive(Serialize)]
pub struct SettingsView {
    pub settings: Settings,
    pub default_excludes: Vec<String>,
    pub version: String,
    pub data_version: i64,
}

#[tauri::command]
pub fn get_settings(app: AppHandle, engine: Eng<'_>) -> SettingsView {
    SettingsView {
        settings: engine.settings.read().clone(),
        default_excludes: settings::default_excludes(),
        version: app.package_info().version.to_string(),
        data_version: engine.db.schema_version().unwrap_or(0),
    }
}

#[tauri::command]
pub fn save_settings(app: AppHandle, engine: Eng<'_>, settings: Settings) -> Res<()> {
    let old = engine.settings.read().clone();
    if settings.hotkey != old.hotkey {
        if let Err(e) = crate::set_hotkey(&app, &settings.hotkey) {
            // Put the old shortcut back so the launcher keeps working.
            let _ = crate::set_hotkey(&app, &old.hotkey);
            return Err(format!("ショートカット「{}」を登録できません（他のアプリが使用中かもしれません）: {e}", settings.hotkey));
        }
        *engine.hotkey_error.lock() = None;
    }
    if settings.autostart != old.autostart {
        use tauri_plugin_autostart::ManagerExt;
        let al = app.autolaunch();
        let r = if settings.autostart { al.enable() } else { al.disable() };
        r.map_err(|e| format!("自動起動の設定に失敗しました: {e}"))?;
    }
    for r in settings.rules.iter().filter(|r| r.enabled) {
        r.validate().map_err(|e| format!("ルール「{}」: {e}", r.name))?;
    }
    let rules_changed = settings.rules != old.rules;
    let rescan = settings.roots != old.roots || settings.excludes != old.excludes || settings.show_hidden != old.show_hidden;
    let content_on = settings.content_enabled
        && (!old.content_enabled
            || settings.content_max_mb != old.content_max_mb
            || settings.content_code != old.content_code
            || settings.content_ocr != old.content_ocr);
    engine.save_settings(settings).map_err(err)?;
    if rescan || content_on {
        engine.inner().rescan();
    } else if rules_changed {
        engine.inner().refresh_watch();
    }
    Ok(())
}

#[tauri::command]
pub fn hide_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}
