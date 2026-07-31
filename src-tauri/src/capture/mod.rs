//! Cross-platform capture backends.
//!
//! `recorder.rs` stays OS-agnostic: it runs a state machine and shells out to
//! ffmpeg. Everything that differs per platform — which source grabs the
//! screen, which encoders exist, how audio devices are identified — lives
//! behind `CaptureBackend`.
//!
//! Both backends compile on every platform. They only build argument vectors
//! (no OS APIs), so the Windows arg builder stays unit-testable from a Mac.
//! Only `active()` is cfg-selected.

use crate::recorder::{RecordingConfig, Region};
use serde::Serialize;
use std::path::{Path, PathBuf};

// Whichever backend isn't active is still compiled — that's what keeps its
// arg building unit-testable from the other platform — so nothing constructs
// it and dead_code fires. Intentional, not rot.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub mod macos;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub mod windows;

/// A capturable screen, with its backend-native identifier. On Windows that's
/// a ddagrab `output_idx`; on macOS an AVFoundation device index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenDevice {
    pub id: u32,
    pub label: String,
}

/// An audio input. `id` is an opaque backend token the UI round-trips back to
/// us untouched (a DirectShow device name on Windows, an AVFoundation index on
/// macOS); `label` is what the user sees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    pub id: String,
    pub label: String,
}

/// What to capture: a backend-native screen id, optionally cropped to a region.
#[derive(Clone, Copy, Debug)]
pub struct CaptureTarget {
    pub screen_id: u32,
    pub region: Option<Region>,
}

pub trait CaptureBackend: Send + Sync {
    /// Short identifier for logs and the UI ("windows-ddagrab", "macos-avfoundation").
    fn name(&self) -> &'static str;

    /// Hardware encoders in auto-pick priority order. libx264 is appended by
    /// the caller as the universal software fallback, so it never appears here.
    fn hw_encoders(&self) -> &'static [&'static str];

    /// Filenames to look for when hunting a bundled ffmpeg next to the exe.
    fn ffmpeg_filenames(&self) -> &'static [&'static str];

    /// Shown when ffmpeg can't be found anywhere.
    fn ffmpeg_hint(&self) -> &'static str;

    /// Shown when ffmpeg started, didn't fail, and captured no frames anyway.
    /// On both platforms that means the OS refused to hand over the screen —
    /// silently, which is why we have to name the likely cause ourselves.
    fn stall_hint(&self) -> &'static str;

    /// Enumerate capturable screens. An empty vec means "this backend can't
    /// enumerate — fall back to the windowing system's monitor list".
    fn screens(&self, ff: &Path) -> Vec<ScreenDevice>;

    /// Enumerate audio capture inputs.
    fn audio_devices(&self, ff: &Path) -> Vec<AudioDevice>;

    /// Command that grabs one full-screen frame of `display_index` to `out`
    /// as PNG. `display_index` is a 0-based position in the UI's monitor list,
    /// *not* a `ScreenDevice::id` — the two differ on macOS, where the video
    /// device index also counts cameras.
    ///
    /// Returns a program plus args rather than an arg vector, because this
    /// isn't necessarily ffmpeg: macOS delegates to Apple's `screencapture`.
    /// Always captures the whole display — cropping to a region happens
    /// afterwards in `still`, so region geometry stays in the one coordinate
    /// space (monitor-relative physical pixels) that the overlay, the
    /// recorder, and this all agree on.
    fn still_command(&self, ff: &Path, display_index: usize, out: &Path)
        -> (PathBuf, Vec<String>);

    /// Build the full ffmpeg argument vector for one recording segment.
    /// `encoder` is already resolved — never "auto".
    fn segment_args(
        &self,
        cfg: &RecordingConfig,
        target: &CaptureTarget,
        encoder: &str,
        out: &Path,
    ) -> Vec<String>;
}

#[cfg(target_os = "windows")]
pub fn active() -> &'static dyn CaptureBackend {
    &windows::Windows
}

#[cfg(target_os = "macos")]
pub fn active() -> &'static dyn CaptureBackend {
    &macos::MacOs
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
compile_error!("Recap has capture backends for Windows and macOS only");

// ---------------------------------------------------------------------------
// Shared helpers

/// H.264 4:2:0 needs even dimensions; overlay coordinates arrive in physical
/// pixels and may be odd. Also clamps negative offsets to 0.
pub fn sanitize_region(x: i32, y: i32, w: u32, h: u32) -> (u32, u32, u32, u32) {
    let x = x.max(0) as u32;
    let y = y.max(0) as u32;
    let w = (w & !1).max(2);
    let h = (h & !1).max(2);
    (x, y, w, h)
}

/// Common prefix for every segment invocation.
///
/// `-progress pipe:1` makes ffmpeg emit newline-separated `key=value` blocks on
/// stdout twice a second. That's the recorder's proof of life: the frame count
/// is the only reliable way to tell "capturing fine" from "stalled forever".
/// It goes to stdout specifically to keep stderr as a clean error log.
pub(crate) fn base_args() -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-y".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_is_evened_and_clamped() {
        assert_eq!(sanitize_region(-3, 5, 801, 601), (0, 5, 800, 600));
        assert_eq!(sanitize_region(10, 10, 1, 1), (10, 10, 2, 2));
    }

    #[test]
    fn active_backend_never_lists_libx264_as_hardware() {
        assert!(!active().hw_encoders().contains(&"libx264"));
    }
}
