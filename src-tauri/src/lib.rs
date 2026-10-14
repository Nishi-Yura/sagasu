mod ai;
mod commands;
mod db;
mod dupes;
mod engine;
mod extract;
mod index;
mod normalize;
mod organize;
mod query;
mod settings;

use std::sync::Arc;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

pub fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        let _ = app.emit("sagasu://focus", ());
    }
}

fn toggle_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false) {
            let _ = w.hide();
        } else {
            show_main(app);
        }
    }
}

/// Replaces the global launcher shortcut.
pub fn set_hotkey(app: &AppHandle, hotkey: &str) -> anyhow::Result<()> {
    let gs = app.global_shortcut();
    gs.unregister_all()?;
    if !hotkey.trim().is_empty() {
        gs.register(hotkey.trim())?;
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let started_hidden = std::env::args().any(|a| a == "--hidden");
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        toggle_main(app);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec!["--hidden"])))
        .setup(move |app| {
            let data_dir = app.path().app_data_dir()?;
            let engine = engine::Engine::new(&data_dir)?;
            engine.start();
            let hotkey = engine.settings.read().hotkey.clone();
            app.manage(Arc::clone(&engine));

            let handle = app.handle().clone();
            if let Err(e) = set_hotkey(&handle, &hotkey) {
                log::warn!("hotkey {hotkey} unavailable: {e}");
                *engine.hotkey_error.lock() = Some(hotkey.clone());
            }

            let open = MenuItem::with_id(app, "open", "Sagasu を開く", true, None::<&str>)?;
            let rescan = MenuItem::with_id(app, "rescan", "インデックスを再作成", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "終了", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &rescan, &quit])?;
            let eng = Arc::clone(&engine);
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().ok_or("missing icon")?)
                .tooltip("Sagasu")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, ev| match ev.id().as_ref() {
                    "open" => show_main(app),
                    "rescan" => eng.rescan(),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            if !started_hidden {
                show_main(&handle);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps Sagasu in the tray so the hotkey still works.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::search,
            commands::search_content,
            commands::ai_search,
            commands::recent,
            commands::open_path,
            commands::reveal_path,
            commands::preview,
            commands::status,
            commands::rescan,
            commands::list_pins,
            commands::add_pin,
            commands::remove_pin,
            commands::organize_inspect,
            commands::organize_preset,
            commands::organize_ai,
            commands::organize_apply,
            commands::history,
            commands::undo,
            commands::find_duplicates,
            commands::trash_path,
            commands::get_settings,
            commands::save_settings,
            commands::set_api_key,
            commands::test_api_key,
            commands::hide_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Sagasu");
}
