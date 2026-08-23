//! macOS Screen Recording (TCC) permission — checked *before* capture is
//! attempted, not discovered from a failed capture after the fact.
//!
//! `CGPreflightScreenCaptureAccess` reports the current decision without ever
//! prompting, so it's safe to call on every launch and every window focus.
//! `CGRequestScreenCaptureAccess` is the one that can show the system dialog
//! — but only the *first* time a binary identity is asked; once the user has
//! denied it, macOS won't re-prompt and the only way forward is System
//! Settings, which is why the UI offers both a "grant" and a "open settings"
//! path rather than just the former.
//!
//! Neither function is `unsafe` in objc2-core-graphics — they're thin, safe
//! wrappers over a C ABI that takes no arguments and can't misuse memory.

#[cfg(target_os = "macos")]
pub fn screen_recording_granted() -> bool {
    objc2_core_graphics::CGPreflightScreenCaptureAccess()
}

// Nothing plays the role of TCC here; capture just works.
#[cfg(not(target_os = "macos"))]
pub fn screen_recording_granted() -> bool {
    true
}

#[cfg(target_os = "macos")]
fn request_access() -> bool {
    objc2_core_graphics::CGRequestScreenCaptureAccess()
}

#[cfg(not(target_os = "macos"))]
fn request_access() -> bool {
    true
}

/// Status only — never prompts. Safe to call from `init_info` and from a
/// window-focus recheck alike.
#[tauri::command]
pub fn permission_status() -> bool {
    screen_recording_granted()
}

/// Explicit user action only (a button click) — this is the one call in this
/// module that can put a system dialog on screen, so it must never run from a
/// background poll.
#[tauri::command]
pub fn request_screen_recording_access() -> bool {
    request_access()
}

/// Deep-link into System Settings at the Screen Recording pane. The user
/// action that grants access is easy to miss inside Privacy & Security, so
/// this skips straight to it.
#[tauri::command]
pub fn open_screen_recording_settings(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
            None::<&str>,
        )
        .map_err(|e| e.to_string())
}
