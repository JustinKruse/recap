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

/// Map the directory holding our executable to the `.app`'s Resources
/// directory, if we are in fact running from inside a bundle.
///
/// The layout is fixed: `Recap.app/Contents/MacOS/Recap` next to
/// `Recap.app/Contents/Resources/ffmpeg` (see `bundle.resources` in
/// tauri.conf.json). `locate()` is called from the CLI path too and has no
/// `AppHandle`, so Tauri's own resource resolver isn't available — the exe path
/// is the only handle we have on the bundle.
///
/// Both path components are checked by name rather than just going up two
/// levels, so a plain `target/release/recap` doesn't get a phantom
/// `target/Resources` and skip the dev-tree walk below it.
#[cfg(target_os = "macos")]
fn bundle_resource_dir(exe_dir: &Path) -> Option<PathBuf> {
    let contents = exe_dir.parent()?;
    if exe_dir.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    Some(contents.join("Resources"))
}

/// Find ffmpeg. Order: RECAP_FFMPEG, next to the exe, a bundled `ffmpeg/`
/// folder, the macOS `.app` Resources directory, a `vendor/` folder anywhere up
/// the tree (how it's found during `cargo tauri dev`, where the exe sits deep
/// under target/), then PATH.
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
            // An installed .app has no source tree above it to walk, so this is
            // the only branch that finds ffmpeg on a real user's machine. It
            // must come before the walk: /Applications/Recap.app must never
            // prefer a stray /Applications/vendor/ffmpeg over its own copy.
            #[cfg(target_os = "macos")]
            if let Some(res) = bundle_resource_dir(dir) {
                for name in names {
                    let c = res.join(name);
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

/// Convert a finished recording to a GIF.
///
/// Two passes on purpose: `palettegen` builds an optimal 256-colour table for
/// this specific clip, then `paletteuse` maps to it. A single-pass GIF uses a
/// generic web palette and looks visibly worse on screen recordings, which are
/// mostly flat UI colour and gradients that band badly.
pub fn to_gif(ff: &Path, src: &Path, out: &Path, fps: u32, width: u32) -> Result<(), String> {
    let filters = format!(
        "fps={},scale={}:-1:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=3",
        fps.clamp(1, 50),
        width.max(16)
    );
    let output = quiet_command(ff)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            &src.display().to_string(),
            "-filter_complex",
            &filters,
            "-loop",
            "0",
            &out.display().to_string(),
        ])
        .output()
        .map_err(|e| format!("could not start ffmpeg: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "GIF export failed: {}",
        err.lines().rev().take(2).collect::<Vec<_>>().join(" ")
    ))
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

    /// The ship blocker this guards: vendor/ is git-ignored and nowhere near an
    /// installed .app, so if this mapping is wrong a real install has no
    /// capture engine at all and every capture fails with "ffmpeg not found".
    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_resources_are_found_from_the_executable() {
        assert_eq!(
            bundle_resource_dir(Path::new("/Applications/Recap.app/Contents/MacOS")),
            Some(PathBuf::from("/Applications/Recap.app/Contents/Resources"))
        );
    }

    /// A bare cargo binary must fall through to the dev-tree walk rather than
    /// claim a bundle two levels up, or `cargo tauri dev` stops finding
    /// vendor/ffmpeg.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_loose_binary_is_not_mistaken_for_a_bundle() {
        assert_eq!(
            bundle_resource_dir(Path::new("/r/src-tauri/target/release")),
            None
        );
        // Right leaf name, wrong grandparent — e.g. a directory literally named
        // MacOS. Both components have to match.
        assert_eq!(bundle_resource_dir(Path::new("/r/build/MacOS")), None);
        assert_eq!(bundle_resource_dir(Path::new("/")), None);
    }

    #[test]
    fn software_fallback_is_always_offered() {
        // Even with no ffmpeg present, libx264 must be in the list so the UI
        // always has a selectable encoder.
        let encoders = usable_encoders(Path::new("/nonexistent/ffmpeg"));
        assert_eq!(encoders, vec!["libx264".to_string()]);
    }
}
