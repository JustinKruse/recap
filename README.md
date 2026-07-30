# Recap

A minimal Snagit-style screen recorder for **macOS and Windows**. Tauri 2 shell (Rust) driving ffmpeg's platform-native capture — no capture engine of our own, just clean process management around ones that already work.

**Features:** full-screen or drag-to-select region capture, mic audio, pause/resume, 3-2-1 countdown, tray icon, global hotkeys (`Ctrl/Cmd+Alt+R` record/stop, `Ctrl/Cmd+Alt+P` pause), hardware encoding with automatic software fallback.

## How capture works per platform

Everything platform-specific sits behind the `CaptureBackend` trait in `src-tauri/src/capture/`. The recorder state machine, UI, and Tauri shell are OS-agnostic.

| | macOS | Windows |
|---|---|---|
| Video source | `avfoundation` screen device | `ddagrab` (GPU Desktop Duplication) |
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

macOS gates screen capture behind TCC. The **app binary** needs Screen Recording permission — grant it in System Settings → Privacy & Security → Screen Recording. Without it, `avfoundation` **hangs silently** rather than erroring: no output file, no message. If a recording produces nothing and never finishes, check permission first.

During `cargo tauri dev` the permission attaches to the dev binary, and it changes identity on rebuild, so macOS may re-prompt.

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
src-tauri/src/
  lib.rs                 commands, tray, global hotkeys, overlay window, events
  recorder.rs            state machine: Idle → Countdown → Recording ⇄ Paused → Finalizing
  ffmpeg.rs              OS-agnostic: locate binary, probe encoders, concat
  capture/
    mod.rs               CaptureBackend trait, shared helpers, backend selection
    macos.rs             avfoundation + VideoToolbox
    windows.rs           ddagrab + dshow + NVENC/AMF/QSV
```

- **Encoders** are probed at startup with a real 3-frame test encode (`-encoders` lies — it lists NVENC even without an NVIDIA GPU).
- **Pause** stops the current ffmpeg segment gracefully (writes `q` to stdin; kill only after a 4 s timeout, since a hard kill truncates the MP4). Resume starts `seg_001.mp4`, `seg_002.mp4`, … Stop losslessly concatenates segments with the concat demuxer.
- Closing the window hides to the tray; recording keeps running. Quit from the tray.

## Known limitations (v0.1)

- **Windows side is still uncompiled.** The cross-platform refactor keeps its arg-building unit-tested, but no one has run it on real Windows hardware yet.
- **Transparency requires Tauri's `macos-private-api`** feature, so the app cannot ship on the Mac App Store. Direct notarized distribution is unaffected.
- **Monitor mapping**: on macOS, screens are enumerated from AVFoundation and matched to Tauri's monitor list by position in that list. Multi-display setups where the two orders disagree will target the wrong screen. macOS multi-display is untested — only one display was available.
- **No system audio** — mic only. Neither Desktop Duplication nor AVFoundation screen capture carries audio; app audio needs a loopback device (VB-Cable / BlackHole) or native code later.
- A/V sync on long mic recordings is untuned (no `aresample` correction yet).
- Settings aren't persisted between launches yet.

## Roadmap

The three remaining Snagit pillars — **still capture + annotation editor** (arrows, boxes, text, highlight, blur/redact, step numbers), **scrolling capture**, and **OCR / text grab** (Vision.framework on macOS) — plus: GIF export (`palettegen`/`paletteuse`) · webcam picture-in-picture · system audio · click highlighting · trim-before-save · settings persistence (`tauri-plugin-store`) · configurable hotkeys.
