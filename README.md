# Recap

A minimal Snagit-style screen recorder for **macOS and Windows**. Tauri 2 shell (Rust) driving ffmpeg's platform-native capture — no capture engine of our own, just clean process management around ones that already work.

**Features:** full-screen or drag-to-select region capture, still screenshots, an annotation editor, OCR text grab, mic audio, pause/resume, 3-2-1 countdown, tray icon, global hotkeys (`Ctrl+Alt+R` record/stop, `Ctrl+Alt+P` pause, `Ctrl+Alt+S` still, `Ctrl+Alt+T` text grab), hardware encoding with automatic software fallback.

## How capture works per platform

Everything platform-specific sits behind the `CaptureBackend` trait in `src-tauri/src/capture/`. The recorder state machine, UI, and Tauri shell are OS-agnostic.

| | macOS | Windows |
|---|---|---|
| Video source | `avfoundation` screen device | `ddagrab` (GPU Desktop Duplication) |
| Still capture | Apple's `screencapture` | `ddagrab` single frame + `hwdownload` |
| OCR | Vision.framework via `objc2` | not implemented (returns an error) |
| Region capture | `crop` filter on a full-screen grab | native `offset_x/offset_y/video_size` |
| Hardware encoder | `h264_videotoolbox` | NVENC → AMF → QuickSync |
| Audio | `avfoundation` input, by device index | `dshow` input, by device name |
| Bundle | `.app` / `.dmg` | NSIS installer |

Both backends compile on both platforms — they only build argument vectors, no OS APIs — so the Windows arg builder stays unit-testable from a Mac. Only `capture::active()` is `cfg`-selected.

## Prerequisites

**Both platforms:** [Rust](https://rustup.rs) and `cargo install tauri-cli --version "^2" --locked`.

**macOS:** Xcode Command Line Tools (`xcode-select --install`). ffmpeg must be a build with `avfoundation` and `videotoolbox` — put a static binary at `vendor/ffmpeg` (git-ignored; found automatically by walking up from the exe), set `RECAP_FFMPEG` to its path, or `brew install ffmpeg`.

**Windows:** Visual Studio Build Tools with "Desktop development with C++". WebView2 is preinstalled on Win 10/11. ffmpeg needs `ddagrab` and `dshow` (standard gyan.dev full builds have both) — `winget install Gyan.FFmpeg`, drop `ffmpeg.exe` next to the Recap executable, or set `RECAP_FFMPEG`.

### macOS screen recording permission

macOS gates screen capture behind TCC. The **app binary** needs Screen Recording permission — grant it in System Settings → Privacy & Security → Screen Recording. During `cargo tauri dev` the permission attaches to the dev binary, which changes identity on rebuild, so macOS may re-prompt.

Note that a capture producing no output is *not* reliable evidence of a permission problem — see the frame-rate note below, which presents identically. To tell them apart, run `screencapture -x -D 1 /tmp/t.png`: if that yields a real image, capture is permitted and the fault is elsewhere.

### Never set `-framerate` on an AVFoundation screen input

`AVCaptureScreenInput` rejects it (`Configuration of video device failed, falling back to default`). ffmpeg then can't estimate the input rate, assumes an enormous one, and duplicates frames without bound — writing megabytes a second, ignoring `-t`, never terminating, and leaving a file with no duration. Measured here: 35 MB and still going at a 25 s kill, versus a clean 1.5 MB / 3.00 s file once the rate moved to the output as `-r`. There is a regression test pinning this.

## Run

```
cargo tauri dev
```

First build takes a few minutes (cold crate compile). Release bundle:

```
cargo tauri build
```

Run the logic tests (both backends' arg builders, region rounding, device parsing, concat escaping):

```
cd src-tauri && cargo test
```

## Layout

```
src/                     vanilla HTML/CSS/JS, no Node needed (withGlobalTauri)
  index.html/main.js     main control window
  overlay.html/.js       transparent per-monitor overlay for region drag-select
  editor.html/.js/.css   annotation editor (canvas)
src-tauri/src/
  lib.rs                 commands, tray, global hotkeys, overlay window, events
  recorder.rs            state machine: Idle → Countdown → Recording ⇄ Paused → Finalizing
  ffmpeg.rs              OS-agnostic: locate binary, probe encoders, concat
  still.rs               screenshots: full-display grab, then crop to region
  editor.rs              annotation editor plumbing: load/save/copy, window
  ocr.rs                 text grab: Vision on macOS, reading-order sort
  capture/
    mod.rs               CaptureBackend trait, shared helpers, backend selection
    macos.rs             avfoundation + VideoToolbox
    windows.rs           ddagrab + dshow + NVENC/AMF/QSV
```

- **Encoders** are probed at startup with a real 3-frame test encode (`-encoders` lies — it lists NVENC even without an NVIDIA GPU).
- **Pause** stops the current ffmpeg segment gracefully (writes `q` to stdin; kill only after a 4 s timeout, since a hard kill truncates the MP4). Resume starts `seg_001.mp4`, `seg_002.mp4`, … Stop losslessly concatenates segments with the concat demuxer.
- **Annotation editor** opens automatically after a still. Tools: arrow, box, ellipse, line, highlight, blur/redact, text, step numbers — keys `A B E L H X T S`. `Cmd/Ctrl+Z` undo, `+Shift` redo, `Cmd/Ctrl+S` save, `+Shift` save-as, `Cmd/Ctrl+C` copy. **Save overwrites the file it opened**; use Save as… to keep the untouched capture.
- Shapes are stored as objects and the canvas is redrawn from the pristine bitmap each change, so undo is free and blur is non-destructive — a redaction samples the source image, never the canvas, so blurs never compound.
- **Text grab** (`Ctrl+Alt+T`) captures the current target, reads it with Vision, copies the text to the clipboard and shows it in an editable sheet. The screenshot is scratch and gets deleted — a text grab shouldn't leave PNGs behind. Vision returns observations unordered with a bottom-left origin; boxes are flipped to top-left and lines sorted into reading order with a row-overlap tolerance so side-by-side columns don't interleave.
- Closing the window hides to the tray; recording keeps running. Quit from the tray.

## Known limitations (v0.1)

- **Windows side is still uncompiled.** The cross-platform refactor keeps its arg-building unit-tested, but no one has run it on real Windows hardware yet. Its still-capture path in particular has never executed.
- **Still capture hides the main window and waits 220 ms** before grabbing. That delay is a guess at compositor repaint time, not a measured value; if Recap shows up in its own screenshot, raise it.
- **OCR accuracy drops on small text.** Reading a whole 3440×1440 screen, Vision returned `Snacit` for *Snagit*, `clioboard` for *clipboard* and `10.65 GE` for *10.65 GB* — UI text at that scale is near its limit. Region grabs of larger text are markedly better. Upscaling the scratch PNG ~2× before recognition is the obvious next lever, but it hasn't been measured, so it isn't in yet.
- **OCR is macOS-only.** Windows returns an explicit error; `Windows.Media.Ocr` needs a language pack and a WinRT binding that aren't wired up.
- **The editor draws but cannot re-select.** Shapes commit on release and can only be removed by undo — there's no click-to-move, resize or restyle after the fact.
- **The editor has no zoom or fit control.** The canvas renders at natural size scaled to the window width, so a tall screenshot scrolls vertically.
- **The whole UI layer is verified only in a headless browser** against a stubbed IPC bridge. The editor's renderers, pointer handling and undo/redo are covered; the actual Tauri commands behind Save, Save as… and Copy have never run.
- **Transparency requires Tauri's `macos-private-api`** feature, so the app cannot ship on the Mac App Store. Direct notarized distribution is unaffected.
- **Monitor mapping**: on macOS, screens are enumerated from AVFoundation and matched to Tauri's monitor list by position in that list. Multi-display setups where the two orders disagree will target the wrong screen. macOS multi-display is untested — only one display was available.
- **No system audio** — mic only. Neither Desktop Duplication nor AVFoundation screen capture carries audio; app audio needs a loopback device (VB-Cable / BlackHole) or native code later.
- A/V sync on long mic recordings is untuned (no `aresample` correction yet).
- Settings aren't persisted between launches yet.

## Roadmap

Three pillars are done: recording, still capture + annotation editor, and OCR. **Scrolling capture** is the one left, plus: GIF export (`palettegen`/`paletteuse`) · webcam picture-in-picture · system audio · click highlighting · trim-before-save · settings persistence (`tauri-plugin-store`) · configurable hotkeys.
