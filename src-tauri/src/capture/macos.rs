//! macOS capture backend.
//!
//!   - Video: ffmpeg's `avfoundation` input device. Screens appear as devices
//!     named "Capture screen N", listed after any cameras.
//!   - Encoding: VideoToolbox (Apple's hardware encoder) with libx264 fallback.
//!   - Region capture: AVFoundation has no capture-offset option the way
//!     ddagrab does, so we grab the full screen and `crop` in the filter graph.
//!   - Audio: a second `avfoundation` input, addressed by device index.
//!
//! Note that AVFoundation reports Retina screens in physical pixels, which is
//! the same coordinate space the region overlay reports in — so crop
//! geometry needs no scale conversion.

use super::{base_args, AudioDevice, CaptureBackend, CaptureTarget, ScreenDevice};
use crate::ffmpeg::quiet_command;
use crate::recorder::RecordingConfig;
use std::path::Path;

pub struct MacOs;

/// Parse `ffmpeg -f avfoundation -list_devices true -i ""` stderr into its
/// video and audio sections. Lines look like:
///   `[AVFoundation indev @ 0x..] [0] Capture screen 0`
pub(crate) fn parse_avfoundation_devices(
    stderr_text: &str,
) -> (Vec<ScreenDevice>, Vec<AudioDevice>) {
    #[derive(PartialEq)]
    enum Section {
        None,
        Video,
        Audio,
    }
    let mut section = Section::None;
    let (mut screens, mut audio) = (Vec::new(), Vec::new());

    for line in stderr_text.lines() {
        if line.contains("AVFoundation video devices:") {
            section = Section::Video;
            continue;
        }
        if line.contains("AVFoundation audio devices:") {
            section = Section::Audio;
            continue;
        }
        // Take the LAST bracketed group as the index — the log prefix
        // `[AVFoundation indev @ 0x..]` also uses brackets.
        let Some(open) = line.rfind('[') else { continue };
        let Some(close_rel) = line[open..].find(']') else {
            continue;
        };
        let close = open + close_rel;
        let Ok(index) = line[open + 1..close].trim().parse::<u32>() else {
            continue;
        };
        let label = line[close + 1..].trim().to_string();
        if label.is_empty() {
            continue;
        }
        match section {
            // Only screens are capturable targets; cameras share the same
            // index space but aren't what Recap records.
            Section::Video if label.starts_with("Capture screen") => {
                screens.push(ScreenDevice { id: index, label })
            }
            Section::Audio => audio.push(AudioDevice {
                id: index.to_string(),
                label,
            }),
            _ => {}
        }
    }
    (screens, audio)
}

fn list_devices_stderr(ff: &Path) -> String {
    // This command always exits non-zero (it has no real input to open), so
    // we read stderr regardless of status.
    quiet_command(ff)
        .args([
            "-hide_banner",
            "-f",
            "avfoundation",
            "-list_devices",
            "true",
            "-i",
            "",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
        .unwrap_or_default()
}

impl CaptureBackend for MacOs {
    fn name(&self) -> &'static str {
        "macos-avfoundation"
    }

    fn hw_encoders(&self) -> &'static [&'static str] {
        &["h264_videotoolbox"]
    }

    fn ffmpeg_filenames(&self) -> &'static [&'static str] {
        &["ffmpeg"]
    }

    fn ffmpeg_hint(&self) -> &'static str {
        "ffmpeg not found. Put a static build at vendor/ffmpeg, set RECAP_FFMPEG to its path, or install it (brew install ffmpeg)."
    }

    fn stall_hint(&self) -> &'static str {
        "No frames were captured. macOS is almost certainly blocking screen recording: open System Settings → Privacy & Security → Screen Recording, enable Recap, then restart the app."
    }

    fn screens(&self, ff: &Path) -> Vec<ScreenDevice> {
        parse_avfoundation_devices(&list_devices_stderr(ff)).0
    }

    fn audio_devices(&self, ff: &Path) -> Vec<AudioDevice> {
        parse_avfoundation_devices(&list_devices_stderr(ff)).1
    }

    fn segment_args(
        &self,
        cfg: &RecordingConfig,
        target: &CaptureTarget,
        encoder: &str,
        out: &Path,
    ) -> Vec<String> {
        let mut a = base_args();

        // ---- video input: avfoundation screen device -----------------------
        // Input options must precede their -i.
        a.extend([
            "-f".into(),
            "avfoundation".into(),
            "-capture_cursor".into(),
            if cfg.capture_cursor { "1".into() } else { "0".into() },
            "-capture_mouse_clicks".into(),
            "0".into(),
            "-framerate".into(),
            cfg.fps.clamp(1, 120).to_string(),
            "-i".into(),
            format!("{}:none", target.screen_id),
        ]);

        // ---- optional mic input --------------------------------------------
        let mic = cfg
            .mic_device
            .as_deref()
            .filter(|d| cfg.mic_enabled && !d.is_empty());
        if let Some(device) = mic {
            a.extend([
                "-f".into(),
                "avfoundation".into(),
                "-i".into(),
                format!(":{device}"),
            ]);
        }

        // ---- stream mapping --------------------------------------------------
        a.extend(["-map".into(), "0:v".into()]);
        if mic.is_some() {
            a.extend(["-map".into(), "1:a".into()]);
        }

        // ---- region crop -----------------------------------------------------
        // No native capture offset on AVFoundation; crop the full-screen grab.
        if let Some(r) = target.region {
            a.extend([
                "-vf".into(),
                format!("crop={}:{}:{}:{}", r.width, r.height, r.x, r.y),
            ]);
        }

        // ---- video encoder ---------------------------------------------------
        let enc_args: &[&str] = match encoder {
            "h264_videotoolbox" => &[
                "-c:v",
                "h264_videotoolbox",
                "-realtime",
                "1",
                "-b:v",
                "12M",
                "-pix_fmt",
                "yuv420p",
            ],
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
            output_dir: "/tmp".into(),
        }
    }

    fn target(screen_id: u32, region: Option<Region>) -> CaptureTarget {
        CaptureTarget { screen_id, region }
    }

    /// Real output captured from ffmpeg 6.0 on this machine, plus a camera
    /// line to prove cameras are skipped but still consume an index.
    const SAMPLE: &str = r#"
[AVFoundation indev @ 0x140607b30] AVFoundation video devices:
[AVFoundation indev @ 0x140607b30] [0] FaceTime HD Camera
[AVFoundation indev @ 0x140607b30] [1] Capture screen 0
[AVFoundation indev @ 0x140607b30] [2] Capture screen 1
[AVFoundation indev @ 0x140607b30] AVFoundation audio devices:
[AVFoundation indev @ 0x140607b30] [0] Justin's AirPods Pro #3
[AVFoundation indev @ 0x140607b30] [1] MacBook Pro Microphone
"#;

    #[test]
    fn parses_screens_skipping_cameras_but_keeping_their_indices() {
        let (screens, _) = parse_avfoundation_devices(SAMPLE);
        assert_eq!(
            screens,
            vec![
                ScreenDevice { id: 1, label: "Capture screen 0".into() },
                ScreenDevice { id: 2, label: "Capture screen 1".into() },
            ]
        );
    }

    #[test]
    fn parses_audio_devices_with_index_ids() {
        let (_, audio) = parse_avfoundation_devices(SAMPLE);
        assert_eq!(audio.len(), 2);
        assert_eq!(audio[0].id, "0");
        assert_eq!(audio[0].label, "Justin's AirPods Pro #3");
        assert_eq!(audio[1].id, "1");
    }

    #[test]
    fn log_prefix_brackets_are_not_mistaken_for_indices() {
        let (screens, audio) = parse_avfoundation_devices(
            "[AVFoundation indev @ 0x7f] AVFoundation video devices:\n\
             [AVFoundation indev @ 0x7f] [0] Capture screen 0\n",
        );
        assert_eq!(screens.len(), 1);
        assert_eq!(screens[0].id, 0);
        assert!(audio.is_empty());
    }

    #[test]
    fn fullscreen_uses_videotoolbox_and_no_crop() {
        let args = MacOs.segment_args(
            &cfg(),
            &target(1, None),
            "h264_videotoolbox",
            Path::new("/tmp/seg.mp4"),
        );
        let joined = args.join(" ");
        assert!(joined.contains("-f avfoundation"));
        assert!(joined.contains("-i 1:none"));
        assert!(joined.contains("-capture_cursor 1"));
        assert!(joined.contains("-c:v h264_videotoolbox"));
        assert!(!joined.contains("crop="));
        assert!(!joined.contains("-map 1:a"));
    }

    #[test]
    fn region_adds_crop_filter_in_wh_xy_order() {
        let region = Region {
            monitor_index: 0,
            x: 100,
            y: 60,
            width: 800,
            height: 600,
        };
        let args = MacOs.segment_args(
            &cfg(),
            &target(1, Some(region)),
            "libx264",
            Path::new("/tmp/seg.mp4"),
        );
        let joined = args.join(" ");
        // ffmpeg's crop is w:h:x:y — not x:y:w:h.
        assert!(joined.contains("-vf crop=800:600:100:60"));
        assert!(joined.contains("-c:v libx264"));
    }

    #[test]
    fn mic_adds_second_avfoundation_input_by_index() {
        let mut c = cfg();
        c.mic_enabled = true;
        c.mic_device = Some("1".into());
        let args = MacOs.segment_args(&c, &target(1, None), "libx264", Path::new("/tmp/seg.mp4"));
        let joined = args.join(" ");
        assert!(joined.contains("-i :1"));
        assert!(joined.contains("-map 1:a"));
        assert!(joined.contains("-c:a aac"));
    }

    #[test]
    fn cursor_off_is_passed_through() {
        let mut c = cfg();
        c.capture_cursor = false;
        let args = MacOs.segment_args(&c, &target(0, None), "libx264", Path::new("/tmp/seg.mp4"));
        assert!(args.join(" ").contains("-capture_cursor 0"));
    }
}
