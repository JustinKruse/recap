mod ffmpeg;
mod recorder;

use recorder::{RecorderHandle, RecordingConfig};
use serde::Serialize;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Listener, Manager, WebviewUrl, WebviewWindowBuilder,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

// ---------------------------------------------------------------------------
// Info surfaced to the UI on load

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct MonitorInfo {
    index: usize,
    name: String,
    width: u32,
    height: u32,
    scale: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitInfo {
    ffmpeg_path: Option<String>,
    encoders: Vec<String>,
    audio_devices: Vec<String>,
    monitors: Vec<MonitorInfo>,
    default_output_dir: String,
}

fn monitor_list(app: &tauri::AppHandle) -> Vec<MonitorInfo> {
    let monitors = app.available_monitors().unwrap_or_default();
    monitors
        .iter()
        .enumerate()
        .map(|(index, m)| MonitorInfo {
            index,
            name: m
                .name()
                .cloned()
                .unwrap_or_else(|| format!("Display {}", index + 1)),
            width: m.size().width,
            height: m.size().height,
            scale: m.scale_factor(),
        })
        .collect()
}

fn default_output_dir(app: &tauri::AppHandle) -> String {
    let base = app
        .path()
        .video_dir()
        .or_else(|_| app.path().home_dir())
        .unwrap_or_else(|_| std::env::temp_dir());
    let dir = base.join("Recap");
    let _ = std::fs::create_dir_all(&dir);
    dir.display().to_string()
}

// ---------------------------------------------------------------------------
// Commands

#[tauri::command]
fn init_info(
    app: tauri::AppHandle,
    state: tauri::State<RecorderHandle>,
) -> Result<InitInfo, String> {
    let ffmpeg_path = ffmpeg::locate();
    let (encoders, audio_devices) = match &ffmpeg_path {
        Some(p) => (ffmpeg::usable_encoders(p), ffmpeg::list_audio_devices(p)),
        None => (vec!["libx264".to_string()], Vec::new()),
    };
    let monitors = monitor_list(&app);
    let default_dir = default_output_dir(&app);
    {
        let mut r = state.0.lock().unwrap();
        r.ffmpeg_path = ffmpeg_path.clone();
        r.encoders = encoders.clone();
        if r.config.output_dir.is_empty() {
            r.config.output_dir = default_dir.clone();
        }
    }
    Ok(InitInfo {
        ffmpeg_path: ffmpeg_path.map(|p| p.display().to_string()),
        encoders,
        audio_devices,
        monitors,
        default_output_dir: default_dir,
    })
}

/// UI pushes its current settings on every change so hotkey/tray starts
/// always use fresh config.
#[tauri::command]
fn sync_config(state: tauri::State<RecorderHandle>, cfg: RecordingConfig) {
    let mut r = state.0.lock().unwrap();
    // Preserve a selected region only while it still matches region mode.
    if cfg.mode != "region" {
        r.region = None;
    }
    r.config = cfg;
}

#[tauri::command]
async fn pick_output_dir(app: tauri::AppHandle) -> Option<String> {
    use tauri_plugin_dialog::DialogExt;
    app.dialog()
        .file()
        .blocking_pick_folder()
        .and_then(|f| f.into_path().ok())
        .map(|p| p.display().to_string())
}

/// Spawn the transparent full-monitor overlay used for region selection.
#[tauri::command]
fn open_region_overlay(
    app: tauri::AppHandle,
    state: tauri::State<RecorderHandle>,
    monitor_index: usize,
) -> Result<(), String> {
    recorder::close_overlay(&app);
    let monitors = app.available_monitors().map_err(|e| e.to_string())?;
    let monitor = monitors
        .get(monitor_index)
        .ok_or_else(|| "monitor not found".to_string())?;
    {
        let mut r = state.0.lock().unwrap();
        r.pending_overlay_monitor = monitor_index;
    }
    let window = WebviewWindowBuilder::new(&app, "overlay", WebviewUrl::App("overlay.html".into()))
        .title("Select region")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .visible(false)
        .build()
        .map_err(|e| e.to_string())?;
    // Cover the chosen monitor exactly, in physical pixels.
    let _ = window.set_position(tauri::PhysicalPosition::new(
        monitor.position().x,
        monitor.position().y,
    ));
    let _ = window.set_size(tauri::PhysicalSize::new(
        monitor.size().width,
        monitor.size().height,
    ));
    let _ = window.show();
    let _ = window.set_focus();
    Ok(())
}

#[tauri::command]
fn start_recording(app: tauri::AppHandle, cfg: RecordingConfig) -> Result<(), String> {
    {
        let state = app.state::<RecorderHandle>();
        let mut r = state.0.lock().unwrap();
        if cfg.mode != "region" {
            r.region = None;
        }
        r.config = cfg;
    }
    recorder::start(&app)
}

#[tauri::command]
fn toggle_pause(app: tauri::AppHandle) -> Result<(), String> {
    recorder::toggle_pause(&app)
}

#[tauri::command]
fn stop_recording(app: tauri::AppHandle) -> Result<(), String> {
    recorder::stop(&app)
}

/// "Open folder" on the finished-recording toast: reveal the file in Explorer.
#[tauri::command]
fn reveal_path(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// App

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(RecorderHandle::new())
        .invoke_handler(tauri::generate_handler![
            init_info,
            sync_config,
            pick_output_dir,
            open_region_overlay,
            start_recording,
            toggle_pause,
            stop_recording,
            reveal_path
        ])
        .setup(|app| {
            // ---- overlay -> rust events -----------------------------------
            let handle = app.handle().clone();
            app.listen_any("region-selected", move |event| {
                if let Ok(sel) = serde_json::from_str::<recorder::RegionSel>(event.payload()) {
                    recorder::set_region(&handle, sel);
                }
            });
            let handle = app.handle().clone();
            app.listen_any("region-cancelled", move |_| {
                recorder::close_overlay(&handle);
            });

            // ---- global hotkeys -------------------------------------------
            let sc_record = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyR);
            let sc_pause = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyP);
            let (h_record, h_pause) = (sc_record.clone(), sc_pause.clone());
            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_handler(move |app, shortcut, event| {
                        if event.state() != ShortcutState::Pressed {
                            return;
                        }
                        if *shortcut == h_record {
                            recorder::toggle_record(app);
                        } else if *shortcut == h_pause {
                            recorder::hotkey_pause(app);
                        }
                    })
                    .build(),
            )?;
            app.global_shortcut().register(sc_record)?;
            app.global_shortcut().register(sc_pause)?;

            // ---- tray ------------------------------------------------------
            let show = MenuItem::with_id(app, "show", "Show Recap", true, None::<&str>)?;
            let toggle =
                MenuItem::with_id(app, "toggle", "Start / stop recording", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &toggle, &quit])?;

            let mut tray = TrayIconBuilder::with_id("recap-tray")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("Recap — Ctrl+Alt+R to record")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "toggle" => recorder::toggle_record(app),
                    "quit" => {
                        recorder::shutdown(app);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            Ok(())
        })
        // Closing the main window hides to tray; recording keeps running.
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Recap");
}
