//! Windows capture backend.
//!
//!   - Video: ffmpeg's `ddagrab` lavfi source (GPU Desktop Duplication).
//!     Region capture is native via offset_x/offset_y/video_size.
//!   - NVENC and AMF accept the D3D11 frames directly (zero copy).
//!   - QSV and libx264 get a `hwdownload,format=bgra` hop into system memory.
//!   - Audio: a second `dshow` input encoded to AAC.
//!
//! Compiled on all platforms so its arg building stays testable; only ever
//! *selected* on Windows.

use super::{base_args, AudioDevice, CaptureBackend, CaptureTarget, ScreenDevice};
use crate::ffmpeg::quiet_command;
use crate::recorder::RecordingConfig;
use std::path::Path;

pub struct Windows;

/// Software-frame encoders need the D3D11 frames pulled down to system memory.
fn needs_hwdownload(encoder: &str) -> bool {
    matches!(encoder, "libx264" | "h264_qsv")
}

pub(crate) fn parse_dshow_audio(stderr_text: &str) -> Vec<AudioDevice> {
    let mut devices = Vec::new();
    for line in stderr_text.lines() {
        if !line.contains("(audio)") {
            continue;
        }
        if let Some(start) = line.find('"') {
            if let Some(len) = line[start + 1..].find('"') {
                let name = &line[start + 1..start + 1 + len];
                if !name.is_empty() {
                    // dshow addresses devices by name, so id == label.
                    devices.push(AudioDevice {
                        id: name.to_string(),
                        label: name.to_string(),
                    });
                }
            }
        }
    }
    devices
}

impl CaptureBackend for Windows {
    fn name(&self) -> &'static str {
        "windows-ddagrab"
    }

    fn hw_encoders(&self) -> &'static [&'static str] {
        &["h264_nvenc", "h264_amf", "h264_qsv"]
    }

    fn ffmpeg_filenames(&self) -> &'static [&'static str] {
        &["ffmpeg.exe"]
    }

    fn ffmpeg_hint(&self) -> &'static str {
        "ffmpeg not found. Install it (winget install Gyan.FFmpeg), drop ffmpeg.exe next to Recap, or set RECAP_FFMPEG."
    }

    fn stall_hint(&self) -> &'static str {
        "No frames were captured. Desktop Duplication couldn't read this display — it doesn't work over Remote Desktop, on a locked screen, or when the display is driven by a different GPU than the one ffmpeg picked."
    }

    /// ddagrab has no enumeration API of its own — the caller falls back to
    /// Tauri's monitor list, where `output_idx` is the monitor index.
    fn screens(&self, _ff: &Path) -> Vec<ScreenDevice> {
        Vec::new()
    }

    fn audio_devices(&self, ff: &Path) -> Vec<AudioDevice> {
        let Ok(output) = quiet_command(ff)
            .args([
                "-hide_banner",
                "-list_devices",
                "true",
                "-f",
                "dshow",
                "-i",
                "dummy",
            ])
            .output()
        else {
            return Vec::new();
        };
        parse_dshow_audio(&String::from_utf8_lossy(&output.stderr))
    }

    fn segment_args(
        &self,
        cfg: &RecordingConfig,
        target: &CaptureTarget,
        encoder: &str,
        out: &Path,
    ) -> Vec<String> {
        let mut a = base_args();

        // ---- video input: ddagrab lavfi graph ------------------------------
        let mut graph = format!(
            "ddagrab=output_idx={}:framerate={}:draw_mouse={}",
            target.screen_id,
            cfg.fps.clamp(1, 120),
            if cfg.capture_cursor { 1 } else { 0 }
        );
        if let Some(r) = target.region {
            graph.push_str(&format!(
                ":offset_x={}:offset_y={}:video_size={}x{}",
                r.x, r.y, r.width, r.height
            ));
        }
        if needs_hwdownload(encoder) {
            graph.push_str(",hwdownload,format=bgra");
        }
        a.extend(["-f".into(), "lavfi".into(), "-i".into(), graph]);

        // ---- optional mic input --------------------------------------------
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

        // ---- stream mapping --------------------------------------------------
        a.extend(["-map".into(), "0:v".into()]);
        if mic.is_some() {
            a.extend(["-map".into(), "1:a".into()]);
        }

        // ---- video encoder ---------------------------------------------------
        let enc_args: &[&str] = match encoder {
            "h264_nvenc" => &["-c:v", "h264_nvenc", "-preset", "p4", "-cq", "23"],
            "h264_amf" => &["-c:v", "h264_amf", "-quality", "balanced", "-b:v", "10M"],
            "h264_qsv" => &["-c:v", "h264_qsv", "-global_quality", "23", "-pix_fmt", "nv12"],
            _ => &[
                "-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p",
            ],
        };
        a.extend(enc_args.iter().map(|s| s.to_string()));

        // ---- audio encoder ---------------------------------------------------
        if mic.is_some() {
            a.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "160k".into()]);
        }

        a.push(out.display().to_string());
        a
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::Region;

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

    fn target(screen_id: u32, region: Option<Region>) -> CaptureTarget {
        CaptureTarget { screen_id, region }
    }

    #[test]
    fn fullscreen_nvenc_has_no_download_hop() {
        let args = Windows.segment_args(
            &cfg(),
            &target(0, None),
            "h264_nvenc",
            Path::new("C:/tmp/seg.mp4"),
        );
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
        let args = Windows.segment_args(
            &cfg(),
            &target(1, Some(region)),
            "libx264",
            Path::new("C:/tmp/seg.mp4"),
        );
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
        let args = Windows.segment_args(
            &c,
            &target(0, None),
            "libx264",
            Path::new("C:/tmp/seg.mp4"),
        );
        let joined = args.join(" ");
        assert!(joined.contains("audio=Microphone (Yeti)"));
        assert!(joined.contains("-map 1:a"));
        assert!(joined.contains("-c:a aac"));
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
        let ids: Vec<&str> = devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "Microphone (Realtek(R) Audio)",
                "Line In (Yeti Stereo Microphone)"
            ]
        );
    }
}
