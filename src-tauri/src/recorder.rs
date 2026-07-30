//! Recording state machine.
//!
//!   Idle -> Countdown -> Recording <-> Paused -> Finalizing -> Idle
//!
//! Pause is implemented as "stop this ffmpeg, start a fresh segment on
//! resume", then all segments are losslessly concatenated on stop. ffmpeg is
//! always stopped by writing `q` to its stdin (a hard kill truncates the MP4
//! moov atom); kill is only the 4-second timeout fallback.

use crate::capture::{self, CaptureTarget, ScreenDevice};
use crate::ffmpeg;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

// ---------------------------------------------------------------------------
// Types

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct RecordingConfig {
    pub mode: String, // "fullscreen" | "region"
    pub monitor_index: usize,
    pub fps: u32,
    pub encoder: String, // "auto" | "h264_nvenc" | "h264_amf" | "h264_qsv" | "libx264"
    pub capture_cursor: bool,
    pub mic_enabled: bool,
    pub mic_device: Option<String>,
    pub output_dir: String,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            mode: "fullscreen".into(),
            monitor_index: 0,
            fps: 30,
            encoder: "auto".into(),
            capture_cursor: true,
            mic_enabled: false,
            mic_device: None,
            output_dir: String::new(),
        }
    }
}

/// Payload the region overlay emits (physical pixels, monitor-relative).
#[derive(Clone, Copy, Deserialize, Debug)]
pub struct RegionSel {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub monitor_index: usize,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Idle,
    Countdown,
    Recording,
    Paused,
    Finalizing,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Countdown => "countdown",
            Status::Recording => "recording",
            Status::Paused => "paused",
            Status::Finalizing => "finalizing",
        }
    }
}

pub struct Recorder {
    pub status: Status,
    pub config: RecordingConfig,
    pub region: Option<Region>,
    /// Which monitor the currently-open overlay covers.
    pub pending_overlay_monitor: usize,
    pub ffmpeg_path: Option<PathBuf>,
    /// Encoders that passed the runtime probe at startup.
    pub encoders: Vec<String>,
    /// Screens as the capture backend enumerates them. Index is the UI's
    /// monitor index; `.id` is the backend-native identifier. Empty when the
    /// backend can't enumerate (Windows), where the two coincide.
    pub screens: Vec<ScreenDevice>,
    child: Option<Child>,
    segments: Vec<PathBuf>,
    session_dir: Option<PathBuf>,
    /// Bumped on every start/cancel so stale countdown threads can bail.
    session_id: u64,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

pub struct RecorderHandle(pub Arc<Mutex<Recorder>>);

impl RecorderHandle {
    pub fn new() -> Self {
        RecorderHandle(Arc::new(Mutex::new(Recorder {
            status: Status::Idle,
            config: RecordingConfig::default(),
            region: None,
            pending_overlay_monitor: 0,
            ffmpeg_path: None,
            encoders: vec!["libx264".into()],
            screens: Vec::new(),
            child: None,
            segments: Vec::new(),
            session_dir: None,
            session_id: 0,
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
        })))
    }
}

// ---------------------------------------------------------------------------
// Event helpers

fn emit_status(app: &AppHandle, status: Status) {
    let _ = app.emit("status", serde_json::json!({ "status": status.as_str() }));
}

fn stderr_tail_string(handle: &RecorderHandle) -> String {
    let tail = {
        let r = handle.0.lock().unwrap();
        r.stderr_tail.clone()
    };
    let t = tail.lock().unwrap();
    t.iter()
        .rev()
        .take(15)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

fn fail(app: &AppHandle, message: String) {
    let handle = app.state::<RecorderHandle>();
    let (child, session_dir) = {
        let mut r = handle.0.lock().unwrap();
        r.status = Status::Idle;
        r.session_id += 1;
        r.segments.clear();
        (r.child.take(), r.session_dir.take())
    };
    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
    if let Some(d) = session_dir {
        let _ = std::fs::remove_dir_all(d);
    }
    let log = stderr_tail_string(&handle);
    let _ = app.emit(
        "recording-error",
        serde_json::json!({ "message": message, "log": log }),
    );
    emit_status(app, Status::Idle);
}

// ---------------------------------------------------------------------------
// Region

pub fn set_region(app: &AppHandle, sel: RegionSel) {
    let handle = app.state::<RecorderHandle>();
    let (monitor_index, width, height) = {
        let mut r = handle.0.lock().unwrap();
        let (x, y, w, h) = capture::sanitize_region(sel.x, sel.y, sel.width, sel.height);
        let monitor_index = r.pending_overlay_monitor;
        r.region = Some(Region {
            monitor_index,
            x,
            y,
            width: w,
            height: h,
        });
        r.config.mode = "region".into();
        (monitor_index, w, h)
    };
    close_overlay(app);
    let _ = app.emit(
        "region-set",
        serde_json::json!({
            "width": width,
            "height": height,
            "monitorIndex": monitor_index
        }),
    );
}

pub fn close_overlay(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.close();
    }
}

// ---------------------------------------------------------------------------
// Start / segments

pub fn start(app: &AppHandle) -> Result<(), String> {
    let handle = app.state::<RecorderHandle>();
    let session_id = {
        let mut r = handle.0.lock().unwrap();
        if r.status != Status::Idle {
            return Err("Already recording.".into());
        }
        if r.ffmpeg_path.is_none() {
            return Err(capture::active().ffmpeg_hint().to_string());
        }
        if r.config.mode == "region" && r.region.is_none() {
            return Err("Select a region first.".into());
        }
        if r.config.output_dir.is_empty() {
            return Err("Choose an output folder first.".into());
        }
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let session_dir = PathBuf::from(&r.config.output_dir).join(format!(".recap-tmp-{stamp}"));
        std::fs::create_dir_all(&session_dir)
            .map_err(|e| format!("cannot create temp folder: {e}"))?;
        r.session_dir = Some(session_dir);
        r.segments.clear();
        {
            let mut tail = r.stderr_tail.lock().unwrap();
            tail.clear();
        }
        r.session_id += 1;
        r.status = Status::Countdown;
        r.session_id
    };
    emit_status(app, Status::Countdown);

    let app = app.clone();
    std::thread::spawn(move || {
        for n in (1..=3).rev() {
            let _ = app.emit("countdown", n);
            std::thread::sleep(Duration::from_secs(1));
            let handle = app.state::<RecorderHandle>();
            let alive = {
                let r = handle.0.lock().unwrap();
                r.session_id == session_id && r.status == Status::Countdown
            };
            if !alive {
                return; // cancelled during countdown
            }
        }
        let _ = app.emit("countdown", 0);
        match spawn_segment(&app, session_id) {
            Ok(()) => {
                {
                    let handle = app.state::<RecorderHandle>();
                    let mut r = handle.0.lock().unwrap();
                    r.status = Status::Recording;
                }
                emit_status(&app, Status::Recording);
                let _ = app.emit("recording-started", serde_json::json!({}));
            }
            Err(e) => fail(&app, e),
        }
    });
    Ok(())
}

fn spawn_segment(app: &AppHandle, session_id: u64) -> Result<(), String> {
    let handle = app.state::<RecorderHandle>();
    let mut r = handle.0.lock().unwrap();
    if r.session_id != session_id {
        return Err("session was cancelled".into());
    }
    let ff = r.ffmpeg_path.clone().ok_or("ffmpeg not found")?;
    let session_dir = r.session_dir.clone().ok_or("no active session")?;
    let seg_path = session_dir.join(format!("seg_{:03}.mp4", r.segments.len()));

    let encoder = resolve_encoder(&r.config.encoder, &r.encoders);
    let region = if r.config.mode == "region" { r.region } else { None };
    // Region mode records whichever monitor the overlay was drawn on.
    let monitor_index = region.map(|x| x.monitor_index).unwrap_or(r.config.monitor_index);
    let target = CaptureTarget {
        screen_id: resolve_screen_id(&r.screens, monitor_index),
        region,
    };
    let args = capture::active().segment_args(&r.config, &target, &encoder, &seg_path);

    let mut cmd = ffmpeg::quiet_command(&ff);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start ffmpeg: {e}"))?;

    if let Some(stderr) = child.stderr.take() {
        let tail = r.stderr_tail.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut t = tail.lock().unwrap();
                if t.len() >= 80 {
                    t.pop_front();
                }
                t.push_back(line);
            }
        });
    }

    r.child = Some(child);
    r.segments.push(seg_path);
    Ok(())
}

fn resolve_encoder(requested: &str, available: &[String]) -> String {
    if requested != "auto" {
        return requested.to_string();
    }
    for enc in capture::active().hw_encoders() {
        if available.iter().any(|a| a == enc) {
            return enc.to_string();
        }
    }
    "libx264".to_string()
}

/// Map a UI monitor index to the backend's native screen id. Backends that
/// can't enumerate (Windows/ddagrab) return an empty list, where the monitor
/// index *is* the id.
fn resolve_screen_id(screens: &[ScreenDevice], monitor_index: usize) -> u32 {
    screens
        .get(monitor_index)
        .map(|s| s.id)
        .unwrap_or(monitor_index as u32)
}

/// Ask ffmpeg to finish cleanly (writes `q`), fall back to kill after ~4s.
fn graceful_stop(child: &mut Child) {
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
        // dropping stdin closes the pipe (EOF) as a second stop signal
    }
    for _ in 0..40 {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Pause / resume

pub fn toggle_pause(app: &AppHandle) -> Result<(), String> {
    let handle = app.state::<RecorderHandle>();
    enum Action {
        Pause(Child),
        Resume(u64),
    }
    let action = {
        let mut r = handle.0.lock().unwrap();
        match r.status {
            Status::Recording => {
                let child = r.child.take().ok_or("recorder has no process")?;
                r.status = Status::Paused;
                Action::Pause(child)
            }
            Status::Paused => Action::Resume(r.session_id),
            _ => return Err("Not recording.".into()),
        }
    };
    match action {
        Action::Pause(mut child) => {
            graceful_stop(&mut child); // done outside the lock: takes up to 4s
            emit_status(app, Status::Paused);
            let _ = app.emit("recording-paused", serde_json::json!({}));
            Ok(())
        }
        Action::Resume(session_id) => {
            spawn_segment(app, session_id)?;
            {
                let mut r = handle.0.lock().unwrap();
                r.status = Status::Recording;
            }
            emit_status(app, Status::Recording);
            let _ = app.emit("recording-resumed", serde_json::json!({}));
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Stop / finalize

pub fn stop(app: &AppHandle) -> Result<(), String> {
    let handle = app.state::<RecorderHandle>();
    enum Action {
        CancelCountdown(Option<PathBuf>),
        Finalize(Option<Child>),
    }
    let action = {
        let mut r = handle.0.lock().unwrap();
        match r.status {
            Status::Countdown => {
                r.session_id += 1; // invalidates the countdown thread
                r.status = Status::Idle;
                Action::CancelCountdown(r.session_dir.take())
            }
            Status::Recording => {
                r.status = Status::Finalizing;
                Action::Finalize(r.child.take())
            }
            Status::Paused => {
                r.status = Status::Finalizing;
                Action::Finalize(None)
            }
            _ => return Err("Not recording.".into()),
        }
    };
    match action {
        Action::CancelCountdown(dir) => {
            if let Some(d) = dir {
                let _ = std::fs::remove_dir_all(d);
            }
            let _ = app.emit("recording-cancelled", serde_json::json!({}));
            emit_status(app, Status::Idle);
            Ok(())
        }
        Action::Finalize(child) => {
            emit_status(app, Status::Finalizing);
            let app = app.clone();
            std::thread::spawn(move || {
                if let Some(mut c) = child {
                    graceful_stop(&mut c);
                }
                finalize(&app);
            });
            Ok(())
        }
    }
}

fn finalize(app: &AppHandle) {
    let handle = app.state::<RecorderHandle>();
    let (ff, segments, session_dir, output_dir) = {
        let r = handle.0.lock().unwrap();
        let good: Vec<PathBuf> = r
            .segments
            .iter()
            .filter(|p| p.metadata().map(|m| m.len() > 1024).unwrap_or(false))
            .cloned()
            .collect();
        (
            r.ffmpeg_path.clone(),
            good,
            r.session_dir.clone(),
            r.config.output_dir.clone(),
        )
    };

    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let final_path = PathBuf::from(&output_dir).join(format!("Recap_{stamp}.mp4"));

    let result: Result<PathBuf, String> = (|| {
        if segments.is_empty() {
            return Err("No video was captured — ffmpeg produced nothing.".into());
        }
        if segments.len() == 1 {
            std::fs::rename(&segments[0], &final_path)
                .map_err(|e| format!("could not move recording: {e}"))?;
        } else {
            let ff = ff.ok_or("ffmpeg not found")?;
            let dir = session_dir.clone().ok_or("no session folder")?;
            let list = dir.join("list.txt");
            let body: String = segments
                .iter()
                .map(|p| ffmpeg::concat_list_line(p) + "\n")
                .collect();
            std::fs::write(&list, body).map_err(|e| format!("could not write list: {e}"))?;
            ffmpeg::run_concat(&ff, &list, &final_path)?;
        }
        Ok(final_path.clone())
    })();

    if let Some(d) = session_dir {
        let _ = std::fs::remove_dir_all(d);
    }
    {
        let mut r = handle.0.lock().unwrap();
        r.status = Status::Idle;
        r.child = None;
        r.segments.clear();
        r.session_dir = None;
    }

    match result {
        Ok(path) => {
            let _ = app.emit(
                "recording-stopped",
                serde_json::json!({ "path": path.display().to_string() }),
            );
        }
        Err(message) => {
            let log = stderr_tail_string(&handle);
            let _ = app.emit(
                "recording-error",
                serde_json::json!({ "message": message, "log": log }),
            );
        }
    }
    emit_status(app, Status::Idle);
}

// ---------------------------------------------------------------------------
// Hotkeys / tray / shutdown entry points

/// Ctrl+Alt+R and the tray "Start / Stop" item: start when idle, stop otherwise.
pub fn toggle_record(app: &AppHandle) {
    let status = {
        let handle = app.state::<RecorderHandle>();
        let r = handle.0.lock().unwrap();
        r.status
    };
    let result = match status {
        Status::Idle => start(app),
        _ => stop(app),
    };
    if let Err(e) = result {
        let _ = app.emit(
            "recording-error",
            serde_json::json!({ "message": e, "log": "" }),
        );
    }
}

/// Ctrl+Alt+P: only meaningful mid-recording.
pub fn hotkey_pause(app: &AppHandle) {
    let status = {
        let handle = app.state::<RecorderHandle>();
        let r = handle.0.lock().unwrap();
        r.status
    };
    if matches!(status, Status::Recording | Status::Paused) {
        let _ = toggle_pause(app);
    }
}

/// Best-effort cleanup on quit so we never leave an orphaned ffmpeg running.
pub fn shutdown(app: &AppHandle) {
    let handle = app.state::<RecorderHandle>();
    let child = {
        let mut r = handle.0.lock().unwrap();
        r.status = Status::Idle;
        r.session_id += 1;
        r.child.take()
    };
    if let Some(mut c) = child {
        graceful_stop(&mut c);
    }
}
