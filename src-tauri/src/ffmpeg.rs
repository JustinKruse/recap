//! The OS-agnostic half of talking to ffmpeg: locating the binary, probing
//! what this machine can actually encode, and concatenating segments.
//!
//! Anything platform-specific — how the screen is grabbed, which encoders to
//! try, how audio devices are named — lives in `capture::CaptureBackend`.

use crate::capture;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Build a Command with the Windows no-console flag so ffmpeg never flashes a
/// terminal window. No-op elsewhere.
pub fn quiet_command(program: &Path) -> Command {
    let cmd = Command::new(program);
    #[cfg(windows)]
    let cmd = {
        use std::os::windows::process::CommandExt;
        let mut c = cmd;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        c
    };
    cmd
}

/// Find ffmpeg. Order: RECAP_FFMPEG, next to the exe, a bundled `ffmpeg/`
/// folder, a `vendor/` folder anywhere up the tree (how it's found during
/// `cargo tauri dev`, where the exe sits deep under target/), then PATH.
pub fn locate() -> Option<PathBuf> {
    let names = capture::active().ffmpeg_filenames();

    if let Ok(p) = std::env::var("RECAP_FFMPEG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in names {
                let candidates = [
                    dir.join(name),
                    dir.join("ffmpeg").join(name),
                    dir.join("ffmpeg").join("bin").join(name),
                ];
                for c in candidates {
                    if c.is_file() {
                        return Some(c);
                    }
                }
            }
            // Dev-tree walk: src-tauri/target/debug/recap -> <root>/vendor/ffmpeg
            let mut cur = Some(dir);
            while let Some(d) = cur {
                for name in names {
                    let c = d.join("vendor").join(name);
                    if c.is_file() {
                        return Some(c);
                    }
                }
                cur = d.parent();
            }
        }
    }

    for name in names {
        let ok = quiet_command(Path::new(name))
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Some(PathBuf::from(name));
        }
    }
    None
}

/// Runtime-probe which hardware encoders actually work here. `-encoders` lists
/// everything compiled in (including nvenc on boxes with no NVIDIA GPU), so we
/// do a real 3-frame test encode instead. libx264 is always appended as the
/// software fallback.
pub fn usable_encoders(ff: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for enc in capture::active().hw_encoders() {
        if probe_encoder(ff, enc) {
            out.push(enc.to_string());
        }
    }
    out.push("libx264".to_string());
    out
}

fn probe_encoder(ff: &Path, enc: &str) -> bool {
    quiet_command(ff)
        .args([
            "-hide_banner",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=128x128:r=30:d=0.2",
            "-frames:v",
            "3",
            "-c:v",
            enc,
            "-f",
            "null",
            "-",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// One `file '...'` line for the concat demuxer. Forward slashes keep the
/// demuxer happy on Windows; embedded single quotes get the '\'' dance.
pub fn concat_list_line(p: &Path) -> String {
    let s = p
        .display()
        .to_string()
        .replace('\\', "/")
        .replace('\'', "'\\''");
    format!("file '{s}'")
}

/// Stitch pause/resume segments together losslessly. Returns Ok(()) only if
/// ffmpeg exits 0 and the output file exists.
pub fn run_concat(ff: &Path, list: &Path, out: &Path) -> Result<(), String> {
    let status = quiet_command(ff)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
        ])
        .arg(list)
        .args(["-c", "copy"])
        .arg(out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("could not run ffmpeg concat: {e}"))?;
    if status.success() && out.is_file() {
        Ok(())
    } else {
        Err("segment concat failed".into())
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_line_uses_forward_slashes_and_escapes_quotes() {
        let line = concat_list_line(Path::new(r"C:\Users\j'k\seg_000.mp4"));
        assert_eq!(line, "file 'C:/Users/j'\\''k/seg_000.mp4'");
    }

    #[test]
    fn software_fallback_is_always_offered() {
        // Even with no ffmpeg present, libx264 must be in the list so the UI
        // always has a selectable encoder.
        let encoders = usable_encoders(Path::new("/nonexistent/ffmpeg"));
        assert_eq!(encoders, vec!["libx264".to_string()]);
    }
}
