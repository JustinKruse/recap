//! Everything that touches ffmpeg: locating the binary, probing what the
//! machine can actually encode, listing DirectShow audio devices, and
//! building argument vectors for capture segments and concat.
//!
//! Capture strategy (Windows):
//!   - Video comes from ffmpeg's `ddagrab` lavfi source (GPU Desktop
//!     Duplication). Region capture is native via offset_x/offset_y/video_size.
//!   - NVENC and AMF accept the D3D11 frames directly (zero copy).
//!   - QSV and libx264 get a `hwdownload,format=bgra` hop into system memory.
//!   - Mic audio is a second dshow input, mapped and encoded to AAC.

use crate::recorder::{Region, RecordingConfig};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Encoders we know how to drive, in auto-pick priority order.
pub const HW_ENCODERS: [&str; 3] = ["h264_nvenc", "h264_amf", "h264_qsv"];

/// Build a Command with the Windows no-console flag so ffmpeg never
/// flashes a terminal window.
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

/// Find ffmpeg. Order: RECAP_FFMPEG env var, next to the exe, an `ffmpeg/`
/// folder next to the exe (drop-in bundle), then PATH.
pub fn locate() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("RECAP_FFMPEG") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidates = [
                dir.join("ffmpeg.exe"),
                dir.join("ffmpeg").join("ffmpeg.exe"),
                dir.join("ffmpeg").join("bin").join("ffmpeg.exe"),
            ];
            for c in candidates {
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let ok = quiet_command(Path::new(name))
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Some(PathBuf::from(name))
    } else {
        None
    }
}

/// Runtime-probe which hardware encoders actually work on this machine
/// (`-encoders` lists everything compiled in, including nvenc on boxes with
/// no NVIDIA GPU, so we do a real 3-frame test encode instead).
/// libx264 is always appended as the software fallback.
pub fn usable_encoders(ff: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for enc in HW_ENCODERS {
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

/// List DirectShow audio capture devices by parsing
/// `ffmpeg -list_devices true -f dshow -i dummy` stderr.
/// Returns an empty list on non-Windows hosts.
pub fn list_audio_devices(ff: &Path) -> Vec<String> {
    let Ok(output) = quiet_command(ff)
        .args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"])
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stderr);
    parse_dshow_audio(&text)
}

fn parse_dshow_audio(stderr_text: &str) -> Vec<String> {
    let mut devices = Vec::new();
    for line in stderr_text.lines() {
        if !line.contains("(audio)") {
            continue;
        }
        if let Some(start) = line.find('"') {
            if let Some(len) = line[start + 1..].find('"') {
                let name = &line[start + 1..start + 1 + len];
                if !name.is_empty() {
                    devices.push(name.to_string());
                }
            }
        }
    }
    devices
}

/// Software-frame encoders need the D3D11 frames pulled down to system memory.
fn needs_hwdownload(encoder: &str) -> bool {
    matches!(encoder, "libx264" | "h264_qsv")
}

/// H.264 4:2:0 requires even dimensions; overlay coordinates arrive in
/// physical pixels and may be odd. Also clamps negative offsets to 0.
pub fn sanitize_region(x: i32, y: i32, w: u32, h: u32) -> (u32, u32, u32, u32) {
    let x = x.max(0) as u32;
    let y = y.max(0) as u32;
    let w = (w & !1).max(2);
    let h = (h & !1).max(2);
    (x, y, w, h)
}

/// Build the full ffmpeg argument vector for one recording segment.
/// `encoder` must already be resolved (never "auto").
pub fn segment_args(
    cfg: &RecordingConfig,
    region: Option<Region>,
    encoder: &str,
    out: &Path,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-y".into(),
    ];

    // ---- video input: ddagrab lavfi graph ----------------------------------
    let monitor = region.map(|r| r.monitor_index).unwrap_or(cfg.monitor_index);
    let mut graph = format!(
        "ddagrab=output_idx={}:framerate={}:draw_mouse={}",
        monitor,
        cfg.fps.clamp(1, 120),
        if cfg.capture_cursor { 1 } else { 0 }
    );
    if let Some(r) = region {
        graph.push_str(&format!(
            ":offset_x={}:offset_y={}:video_size={}x{}",
            r.x, r.y, r.width, r.height
        ));
    }
    if needs_hwdownload(encoder) {
        graph.push_str(",hwdownload,format=bgra");
    }
    a.extend(["-f".into(), "lavfi".into(), "-i".into(), graph]);

    // ---- optional mic input -------------------------------------------------
    let mic = cfg
        .mic_device
        .as_deref()
        .filter(|d| cfg.mic_enabled && !d.is_empty());
    if let Some(device) = mic {
        a.extend([
            "-f".into(),
            "dshow".into(),
            "-i".into(),
            format!("audio={device}"),
        ]);
    }

    // ---- stream mapping -----------------------------------------------------
    a.extend(["-map".into(), "0:v".into()]);
    if mic.is_some() {
        a.extend(["-map".into(), "1:a".into()]);
    }

    // ---- video encoder ------------------------------------------------------
    let enc_args: &[&str] = match encoder {
        "h264_nvenc" => &["-c:v", "h264_nvenc", "-preset", "p4", "-cq", "23"],
        "h264_amf" => &["-c:v", "h264_amf", "-quality", "balanced", "-b:v", "10M"],
        "h264_qsv" => &["-c:v", "h264_qsv", "-global_quality", "23", "-pix_fmt", "nv12"],
        _ => &[
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p",
        ],
    };
    a.extend(enc_args.iter().map(|s| s.to_string()));

    // ---- audio encoder ------------------------------------------------------
    if mic.is_some() {
        a.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "160k".into()]);
    }

    a.push(out.display().to_string());
    a
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
    use crate::recorder::{Region, RecordingConfig};

    fn cfg() -> RecordingConfig {
        RecordingConfig {
            mode: "fullscreen".into(),
            monitor_index: 0,
            fps: 30,
            encoder: "auto".into(),
            capture_cursor: true,
            mic_enabled: false,
            mic_device: None,
            output_dir: "C:/tmp".into(),
        }
    }

    #[test]
    fn region_is_evened_and_clamped() {
        assert_eq!(sanitize_region(-3, 5, 801, 601), (0, 5, 800, 600));
        assert_eq!(sanitize_region(10, 10, 1, 1), (10, 10, 2, 2));
    }

    #[test]
    fn fullscreen_nvenc_has_no_download_hop() {
        let args = segment_args(&cfg(), None, "h264_nvenc", Path::new("C:/tmp/seg.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("ddagrab=output_idx=0:framerate=30:draw_mouse=1"));
        assert!(!joined.contains("hwdownload"));
        assert!(joined.contains("-c:v h264_nvenc"));
        assert!(!joined.contains("-map 1:a"));
    }

    #[test]
    fn region_x264_downloads_and_sets_geometry() {
        let region = Region {
            monitor_index: 1,
            x: 100,
            y: 60,
            width: 800,
            height: 600,
        };
        let args = segment_args(&cfg(), Some(region), "libx264", Path::new("C:/tmp/seg.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("output_idx=1"));
        assert!(joined.contains("offset_x=100:offset_y=60:video_size=800x600"));
        assert!(joined.contains("hwdownload,format=bgra"));
        assert!(joined.contains("-pix_fmt yuv420p"));
    }

    #[test]
    fn mic_adds_second_input_and_aac() {
        let mut c = cfg();
        c.mic_enabled = true;
        c.mic_device = Some("Microphone (Yeti)".into());
        let args = segment_args(&c, None, "libx264", Path::new("C:/tmp/seg.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("audio=Microphone (Yeti)"));
        assert!(joined.contains("-map 1:a"));
        assert!(joined.contains("-c:a aac"));
    }

    #[test]
    fn concat_line_uses_forward_slashes_and_escapes_quotes() {
        let line = concat_list_line(Path::new(r"C:\Users\j'k\seg_000.mp4"));
        assert_eq!(line, "file 'C:/Users/j'\\''k/seg_000.mp4'");
    }

    #[test]
    fn dshow_parse_extracts_audio_names_only() {
        let sample = r#"
[dshow @ 0000018] "OBS Virtual Camera" (video)
[dshow @ 0000018]   Alternative name "@device_pnp_..."
[dshow @ 0000018] "Microphone (Realtek(R) Audio)" (audio)
[dshow @ 0000018] "Line In (Yeti Stereo Microphone)" (audio)
"#;
        let devices = parse_dshow_audio(sample);
        assert_eq!(
            devices,
            vec![
                "Microphone (Realtek(R) Audio)".to_string(),
                "Line In (Yeti Stereo Microphone)".to_string()
            ]
        );
    }
}
