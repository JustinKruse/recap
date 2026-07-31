//! Annotation editor plumbing.
//!
//! The editor itself is a canvas in a webview; Rust only moves pixels in and
//! out. Images travel as data URLs rather than over the asset protocol so the
//! editor needs no filesystem scope — it only ever sees what we hand it.

use base64::{engine::general_purpose::STANDARD, Engine};
use std::path::{Path, PathBuf};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// Refuse to inline anything absurd. A 6K screenshot is ~15 MB of PNG; base64
/// inflates by a third, and the IPC bridge has to carry all of it as a string.
const MAX_INLINE_BYTES: u64 = 64 * 1024 * 1024;

fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        _ => "image/png",
    }
}

/// Read an image off disk as a data URL the webview can assign to `img.src`.
#[tauri::command]
pub fn load_image(path: String) -> Result<String, String> {
    let path = PathBuf::from(&path);
    let len = std::fs::metadata(&path)
        .map_err(|e| format!("cannot open image: {e}"))?
        .len();
    if len > MAX_INLINE_BYTES {
        return Err(format!(
            "image is too large to edit ({} MB)",
            len / 1024 / 1024
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read image: {e}"))?;
    Ok(format!(
        "data:{};base64,{}",
        mime_for(&path),
        STANDARD.encode(bytes)
    ))
}

/// Split a `data:<mime>;base64,<payload>` URL into its payload and decode it.
fn decode_data_url(data_url: &str) -> Result<Vec<u8>, String> {
    let payload = data_url
        .split_once(";base64,")
        .map(|(_, p)| p)
        .ok_or("not a base64 data URL")?;
    STANDARD
        .decode(payload)
        .map_err(|e| format!("corrupt image data: {e}"))
}

/// Write the flattened canvas back to disk.
#[tauri::command]
pub fn save_image(path: String, data_url: String) -> Result<String, String> {
    let bytes = decode_data_url(&data_url)?;
    let path = PathBuf::from(&path);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create folder: {e}"))?;
    }
    std::fs::write(&path, bytes).map_err(|e| format!("cannot save image: {e}"))?;
    Ok(path.display().to_string())
}

/// Save-as, with a native file dialog. `Ok(None)` means the user cancelled —
/// distinct from an error, so the UI can stay quiet.
#[tauri::command]
pub async fn save_image_as(
    app: tauri::AppHandle,
    suggested: String,
    data_url: String,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    let suggested = PathBuf::from(&suggested);
    let dir = suggested.parent().map(|p| p.to_path_buf());
    let name = suggested
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Recap.png".into());

    let mut dialog = app.dialog().file().set_file_name(&name).add_filter("PNG", &["png"]);
    if let Some(d) = dir {
        dialog = dialog.set_directory(d);
    }
    let Some(target) = dialog.blocking_save_file() else {
        return Ok(None);
    };
    let mut target = target
        .into_path()
        .map_err(|e| format!("bad destination: {e}"))?;
    // Only PNG is ever encoded here, so a name ending .jpg would be a lie on
    // disk that every downstream tool would then misread.
    if !target
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("png"))
    {
        target.set_extension("png");
    }

    let bytes = decode_data_url(&data_url)?;
    std::fs::write(&target, bytes).map_err(|e| format!("cannot save image: {e}"))?;
    Ok(Some(target.display().to_string()))
}

/// Put the flattened canvas on the system clipboard as an image.
#[tauri::command]
pub fn copy_image(app: tauri::AppHandle, data_url: String) -> Result<(), String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let bytes = decode_data_url(&data_url)?;
    let image = tauri::image::Image::from_bytes(&bytes)
        .map_err(|e| format!("could not decode image: {e}"))?;
    app.clipboard()
        .write_image(&image)
        .map_err(|e| format!("could not copy: {e}"))
}

/// Open (or re-target) the editor window on `path`.
///
/// Deliberately a single reusable window: capture → annotate → capture again is
/// the core loop, and spawning a window per shot buries the screen in editors.
#[tauri::command]
pub fn open_editor(app: tauri::AppHandle, path: String) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("editor") {
        // Already open — hand it the new image rather than stacking windows.
        use tauri::Emitter;
        let _ = w.emit("editor-load", serde_json::json!({ "path": path }));
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    let url = format!("editor.html?path={}", urlencode(&path));
    WebviewWindowBuilder::new(&app, "editor", WebviewUrl::App(url.into()))
        .title("Recap — Annotate")
        .inner_size(1180.0, 760.0)
        .min_inner_size(720.0, 480.0)
        .center()
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Percent-encode everything outside the unreserved set. Paths land in a query
/// string, and `#` in particular would otherwise truncate the path silently.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapes_characters_that_would_break_a_query_string() {
        assert_eq!(urlencode("/Users/j/My Shots/a#1.png"),
                   "%2FUsers%2Fj%2FMy%20Shots%2Fa%231.png");
        assert_eq!(urlencode("plain-name_1.png"), "plain-name_1.png");
    }

    #[test]
    fn data_url_round_trips_through_decode() {
        let url = format!("data:image/png;base64,{}", STANDARD.encode(b"hello"));
        assert_eq!(decode_data_url(&url).unwrap(), b"hello");
    }

    #[test]
    fn decode_rejects_a_url_without_a_base64_payload() {
        assert!(decode_data_url("data:image/png,notbase64").is_err());
        assert!(decode_data_url("/just/a/path.png").is_err());
    }

    #[test]
    fn mime_follows_the_extension_and_defaults_to_png() {
        assert_eq!(mime_for(Path::new("a.jpg")), "image/jpeg");
        assert_eq!(mime_for(Path::new("a.JPEG")), "image/jpeg");
        assert_eq!(mime_for(Path::new("a.png")), "image/png");
        assert_eq!(mime_for(Path::new("noext")), "image/png");
    }
}
