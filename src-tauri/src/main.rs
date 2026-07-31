// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `recap shot` / `recap ocr` run headlessly and exit; anything else starts
    // the window. Checked before Tauri so no GUI is created for a CLI run.
    if recap_lib::cli::maybe_run() {
        return;
    }
    recap_lib::run()
}
