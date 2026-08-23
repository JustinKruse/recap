//! Persisted settings.
//!
//! Plain JSON next to the app's other config. Deliberately not a plugin: the
//! whole payload is one small struct, and a bad or missing file must degrade to
//! defaults rather than stop the app from starting.

use crate::recorder::{lock_recovering, RecorderHandle, RecordingConfig};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

static PATH: OnceLock<PathBuf> = OnceLock::new();
/// Bumped on every change; the writer thread only writes if it's still the
/// newest. The UI pushes a config on every keystroke-ish event, and settings
/// aren't worth a disk write each time.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static PENDING: OnceLock<Mutex<Option<RecordingConfig>>> = OnceLock::new();

const DEBOUNCE: Duration = Duration::from_millis(600);

pub fn init(app: &tauri::AppHandle) {
    use tauri::Manager;
    let dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&dir);
    let _ = PATH.set(dir.join("settings.json"));
    let _ = PENDING.set(Mutex::new(None));
}

/// Settings from the last run, or `None` if there are none or they're unreadable.
pub fn load() -> Option<RecordingConfig> {
    let raw = std::fs::read_to_string(PATH.get()?).ok()?;
    // A malformed file is not worth surfacing — fall back to defaults.
    serde_json::from_str(&raw).ok()
}

pub fn save_debounced(state: &RecorderHandle) {
    let Some(pending) = PENDING.get() else { return };
    {
        let cfg = state.lock().config.clone();
        *lock_recovering(pending) = Some(cfg);
    }
    let mine = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(DEBOUNCE);
        if GENERATION.load(Ordering::Relaxed) != mine {
            return; // superseded by a newer change
        }
        let Some(cfg) = PENDING.get().and_then(|p| lock_recovering(p).clone()) else {
            return;
        };
        write(&cfg);
    });
}

/// Write immediately, for shutdown where there's no time to debounce.
pub fn save_now(state: &RecorderHandle) {
    let cfg = state.lock().config.clone();
    write(&cfg);
}

fn write(cfg: &RecordingConfig) {
    let Some(path) = PATH.get() else { return };
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(path, json);
    }
}
