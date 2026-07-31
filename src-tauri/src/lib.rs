pub mod cli;
mod capture;
// Debug-only: exposes an eval-anything socket. Must never ship in release.
#[cfg(debug_assertions)]
mod devctl;
mod editor;
mod ffmpeg;
mod ocr;
mod recorder;
mod settings;
mod still;

use capture::AudioDevice;
use recorder::{RecorderHandle, RecordingConfig};
use serde::Serialize;
use std::path::PathBuf;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Listener, Manager, WebviewUrl, WebviewWindowBuilder,
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
    audio_devices: Vec<AudioDevice>,
    monitors: Vec<MonitorInfo>,
    default_output_dir: String,
    /// The effective config, after restoring last run's settings. The UI mirrors
    /// its controls onto this so a restored setting is visible, not just active.
    config: RecordingConfig,
    /// Which capture backend is active, e.g. "macos-avfoundation".
    backend: &'static str,
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
    let backend = capture::active();
    let ffmpeg_path = ffmpeg::locate();
    let (encoders, audio_devices, screens) = match &ffmpeg_path {
        Some(p) => (
            ffmpeg::usable_encoders(p),
            backend.audio_devices(p),
            backend.screens(p),
        ),
        None => (vec!["libx264".to_string()], Vec::new(), Vec::new()),
    };
    let monitors = monitor_list(&app);
    let default_dir = default_output_dir(&app);
    let config = {
        let mut r = state.0.lock().unwrap();
        r.ffmpeg_path = ffmpeg_path.clone();
        r.encoders = encoders.clone();
        r.screens = screens;
        // A restored folder that has since been deleted is worse than useless:
        // every capture would fail. Fall back rather than persist a dead path.
        if r.config.output_dir.is_empty() || !PathBuf::from(&r.config.output_dir).is_dir() {
            r.config.output_dir = default_dir.clone();
        }
        // The monitor list can shrink between runs (display unplugged).
        if r.config.monitor_index >= monitors.len().max(1) {
            r.config.monitor_index = 0;
        }
        r.config.clone()
    };
    Ok(InitInfo {
        ffmpeg_path: ffmpeg_path.map(|p| p.display().to_string()),
        encoders,
        audio_devices,
        monitors,
        default_output_dir: default_dir,
        config,
        backend: backend.name(),
    })
}

/// UI pushes its current settings on every change so hotkey/tray starts
/// always use fresh config.
#[tauri::command]
fn sync_config(state: tauri::State<RecorderHandle>, cfg: RecordingConfig) {
    state.0.lock().unwrap().apply_config(cfg);
    settings::save_debounced(&state);
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
    app.state::<RecorderHandle>().0.lock().unwrap().apply_config(cfg);
    recorder::start(&app)
}

/// Grab a still of the current target. Runs off the UI thread because
/// `screencapture`/ffmpeg take a moment and blocking here freezes the window.
#[tauri::command]
async fn capture_still(app: tauri::AppHandle, cfg: RecordingConfig) -> Result<String, String> {
    app.state::<RecorderHandle>().0.lock().unwrap().apply_config(cfg);
    // Give the compositor a beat to finish hiding our own window before we
    // photograph the screen it was just covering.
    std::thread::sleep(std::time::Duration::from_millis(220));
    capture_still_flow(&app)
}

/// Capture the current target and read the text out of it. The screenshot is a
/// means to an end here, so it goes to a scratch file and is deleted after —
/// the user asked for text, not another PNG in their folder.
#[tauri::command]
async fn grab_text(app: tauri::AppHandle, cfg: RecordingConfig) -> Result<ocr::OcrResult, String> {
    app.state::<RecorderHandle>().0.lock().unwrap().apply_config(cfg);
    read_screen_text(&app)
}

/// Shared by the button and the hotkey. The screenshot is a means to an end, so
/// it goes to a scratch file and is deleted after — the user asked for text,
/// not another PNG in their folder.
pub(crate) fn read_screen_text(app: &tauri::AppHandle) -> Result<ocr::OcrResult, String> {
    let shot = still::capture_temp(app)?;
    let result = ocr::recognize(&shot);
    let _ = std::fs::remove_file(&shot);
    let result = result?;

    // Text on the clipboard is the whole point of a text grab; putting it there
    // unasked saves the one step everyone would take next.
    if !result.text.is_empty() {
        use tauri_plugin_clipboard_manager::ClipboardExt;
        let _ = app.clipboard().write_text(result.text.clone());
    }
    Ok(result)
}

#[tauri::command]
fn toggle_pause(app: tauri::AppHandle) -> Result<(), String> {
    recorder::toggle_pause(&app)
}

#[tauri::command]
fn stop_recording(app: tauri::AppHandle) -> Result<(), String> {
    recorder::stop(&app)
}

/// Turn a finished recording into a GIF beside it. Width and fps are dropped
/// from the source deliberately: a full-resolution 30fps GIF of a 3440px screen
/// is tens of megabytes and useless for sharing, which is the only reason to
/// want a GIF at all.
#[tauri::command]
async fn export_gif(app: tauri::AppHandle, path: String, fps: u32, width: u32) -> Result<String, String> {
    let ff = {
        let state = app.state::<RecorderHandle>();
        let r = state.0.lock().unwrap();
        r.ffmpeg_path.clone()
    }
    .ok_or_else(|| capture::active().ffmpeg_hint().to_string())?;
    let src = PathBuf::from(&path);
    if !src.is_file() {
        return Err("that recording is no longer there".into());
    }
    let out = src.with_extension("gif");
    ffmpeg::to_gif(&ff, &src, &out, fps, width)?;
    Ok(out.display().to_string())
}

/// "Open folder" on the finished-recording toast: reveal the file in Explorer.
#[tauri::command]
fn reveal_path(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .reveal_item_in_dir(path)
        .map_err(|e| e.to_string())
}

/// Capture a still and open it for annotation. The single definition of what
/// "take a still" means, so the button, the hotkey and devctl stay in step.
pub(crate) fn capture_still_flow(app: &tauri::AppHandle) -> Result<String, String> {
    let path = still::capture(app)?.display().to_string();
    editor::open_editor(app.clone(), path.clone())?;
    Ok(path)
}

/// Ctrl+Alt+S. Runs off the hotkey thread — the capture blocks, and stalling
/// the shortcut handler would wedge every other hotkey with it. Uses whatever
/// config the UI last pushed, so it works with the window hidden.
fn hotkey_snap(app: &tauri::AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let main = app.get_webview_window("main");
        let was_visible = main
            .as_ref()
            .map(|w| w.is_visible().unwrap_or(false))
            .unwrap_or(false);
        if was_visible {
            if let Some(w) = &main {
                let _ = w.hide();
            }
            std::thread::sleep(std::time::Duration::from_millis(220));
        }
        let result = capture_still_flow(&app);
        if was_visible {
            if let Some(w) = &main {
                let _ = w.show();
            }
        }
        match result {
            Ok(path) => {
                let _ = app.emit("still-captured", serde_json::json!({ "path": path }));
            }
            Err(message) => {
                let _ = app.emit(
                    "recording-error",
                    serde_json::json!({ "message": message, "log": "" }),
                );
            }
        }
    });
}

/// Ctrl+Alt+T. Same off-thread reasoning as `hotkey_snap`.
fn hotkey_grab_text(app: &tauri::AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let main = app.get_webview_window("main");
        let was_visible = main
            .as_ref()
            .map(|w| w.is_visible().unwrap_or(false))
            .unwrap_or(false);
        if was_visible {
            if let Some(w) = &main {
                let _ = w.hide();
            }
            std::thread::sleep(std::time::Duration::from_millis(220));
        }
        let result = read_screen_text(&app);
        if was_visible {
            if let Some(w) = &main {
                let _ = w.show();
                let _ = w.set_focus();
            }
        }
        match result {
            Ok(r) => {
                let _ = app.emit("text-grabbed", &r);
            }
            Err(message) => {
                let _ = app.emit(
                    "recording-error",
                    serde_json::json!({ "message": message, "log": "" }),
                );
            }
        }
    });
}

// ---------------------------------------------------------------------------
// App

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(RecorderHandle::new())
        .invoke_handler(tauri::generate_handler![
            init_info,
            sync_config,
            pick_output_dir,
            open_region_overlay,
            capture_still,
            grab_text,
            editor::open_editor,
            editor::load_image,
            editor::save_image,
            editor::save_image_as,
            editor::copy_image,
            start_recording,
            toggle_pause,
            stop_recording,
            reveal_path,
            export_gif
        ])
        .setup(|app| {
            settings::init(app.handle());
            if let Some(cfg) = settings::load() {
                app.state::<RecorderHandle>().0.lock().unwrap().config = cfg;
            }

            #[cfg(debug_assertions)]
            devctl::start(app.handle());

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
            let sc_snap = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyS);
            let sc_text = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyT);
            let (h_record, h_pause, h_snap, h_text) = (
                sc_record.clone(),
                sc_pause.clone(),
                sc_snap.clone(),
                sc_text.clone(),
            );
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
                        } else if *shortcut == h_snap {
                            hotkey_snap(app);
                        } else if *shortcut == h_text {
                            hotkey_grab_text(app);
                        }
                    })
                    .build(),
            )?;
            app.global_shortcut().register(sc_record)?;
            app.global_shortcut().register(sc_pause)?;
            app.global_shortcut().register(sc_snap)?;
            app.global_shortcut().register(sc_text)?;

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
                        settings::save_now(&app.state::<RecorderHandle>());
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
