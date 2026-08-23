//! Still capture — the first half of the "capture + annotate" pillar.
//!
//! Always grabs the whole display, then crops. That's deliberate: every
//! platform can capture a full display trivially, but region syntax differs
//! wildly (and macOS's `screencapture -R` works in *points* in global desktop
//! coordinates, while our overlay reports physical pixels relative to one
//! monitor). Cropping afterwards with ffmpeg keeps a single coordinate space
//! shared with the recorder, and the extra pass costs milliseconds on a PNG.

use crate::capture::{self, sanitize_region};
use crate::ffmpeg;
use crate::recorder::{RecorderHandle, Region};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// A still that produces fewer bytes than this never contains an image — it's
/// a truncated or empty file from a capture the OS refused.
const MIN_PLAUSIBLE_PNG: u64 = 512;

/// Same reasoning as the recorder's stall watchdog: a blocked screen capture
/// can hang rather than fail. `screencapture` is fast, so this is generous.
const STILL_TIMEOUT: Duration = Duration::from_secs(10);

/// Capture to the user's output folder, named by timestamp.
pub fn capture(app: &AppHandle) -> Result<PathBuf, String> {
    let output_dir = {
        let handle = app.state::<RecorderHandle>();
        let r = handle.lock();
        r.config.output_dir.clone()
    };
    if output_dir.is_empty() {
        return Err("Choose an output folder first.".into());
    }
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    capture_into(
        app,
        PathBuf::from(output_dir).join(format!("Recap_{stamp}.png")),
    )
}

/// Capture to a scratch file. Text grab wants pixels to read, not a saved
/// screenshot, so this deliberately keeps the user's output folder clean.
pub fn capture_temp(app: &AppHandle) -> Result<PathBuf, String> {
    let stamp = chrono::Local::now().format("%Y%m%d%H%M%S%3f").to_string();
    capture_into(
        app,
        std::env::temp_dir().join(format!("recap-ocr-{stamp}.png")),
    )
}

fn capture_into(app: &AppHandle, final_path: PathBuf) -> Result<PathBuf, String> {
    let handle = app.state::<RecorderHandle>();
    let (ff, region, display_index) = {
        let r = handle.lock();
        let region = if r.config.mode == "region" {
            r.region
        } else {
            None
        };
        // A region belongs to the display it was drawn on.
        let display_index = region
            .map(|x| x.monitor_index)
            .unwrap_or(r.config.monitor_index);
        (r.ffmpeg_path.clone(), region, display_index)
    };
    let dir = final_path
        .parent()
        .ok_or("output path has no folder")?
        .to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create output folder: {e}"))?;

    let backend = capture::active();

    // Cropping needs somewhere to put the uncropped grab first. Sits beside the
    // destination so the crop never has to cross a filesystem boundary.
    let raw_path = if region.is_some() {
        let stem = final_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "shot".into());
        dir.join(format!(".recap-raw-{stem}.png"))
    } else {
        final_path.clone()
    };

    // ffmpeg is only strictly required for the crop pass and on Windows, but
    // it's needed often enough that a missing binary is worth catching early.
    let ff_for_cmd = ff.clone().unwrap_or_default();
    let (program, args) = backend.still_command(&ff_for_cmd, display_index, true, &raw_path);
    if program.as_os_str().is_empty() {
        return Err(backend.ffmpeg_hint().to_string());
    }

    run(&program, &args)?;

    let bytes = std::fs::metadata(&raw_path).map(|m| m.len()).unwrap_or(0);
    if bytes < MIN_PLAUSIBLE_PNG {
        let _ = std::fs::remove_file(&raw_path);
        return Err(backend.stall_hint().to_string());
    }

    if let Some(r) = region {
        let ff = ff.ok_or_else(|| backend.ffmpeg_hint().to_string())?;
        let result = crop(&ff, &raw_path, r, &final_path);
        let _ = std::fs::remove_file(&raw_path);
        result?;
    }
    Ok(final_path)
}

/// Crop in monitor-relative physical pixels — the overlay's coordinate space.
fn crop(ff: &Path, src: &Path, r: Region, out: &Path) -> Result<(), String> {
    let (x, y, w, h) = sanitize_region(r.x as i32, r.y as i32, r.width, r.height);
    let args = vec![
        "-hide_banner".to_string(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        src.display().to_string(),
        "-vf".into(),
        // ffmpeg's crop is w:h:x:y — not x:y:w:h.
        format!("crop={w}:{h}:{x}:{y}"),
        out.display().to_string(),
    ];
    run(ff, &args)
}

/// Run a capture command to completion, failing rather than hanging forever.
pub(crate) fn run(program: &Path, args: &[String]) -> Result<(), String> {
    let mut child = ffmpeg::quiet_command(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "could not start {}: {e}",
                program.file_name().unwrap_or_default().to_string_lossy()
            )
        })?;

    let deadline = std::time::Instant::now() + STILL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut msg = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut msg);
                }
                let tail = msg.lines().rev().take(3).collect::<Vec<_>>().join(" ");
                return Err(if tail.is_empty() {
                    format!("capture failed ({status})")
                } else {
                    format!("capture failed: {tail}")
                });
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(capture::active().stall_hint().to_string());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("capture failed: {e}")),
        }
    }
}
