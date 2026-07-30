# Recap

A minimal Snagit-style screen recorder for Windows. Tauri 2 shell (Rust) driving ffmpeg's GPU-accelerated `ddagrab` capture — no capture engine of our own, just clean process management around one that already works.

**Features:** full-screen or drag-to-select region capture, mic audio, pause/resume, 3-2-1 countdown, tray icon, global hotkeys (`Ctrl+Alt+R` record/stop, `Ctrl+Alt+P` pause), hardware encoding (NVENC → AMF → QuickSync → x264 auto-fallback).

## Prerequisites (Windows 10/11)

1. **Rust** — install via [rustup.rs](https://rustup.rs) (MSVC toolchain).
2. **Visual Studio Build Tools** with the "Desktop development with C++" workload (rustup will prompt if missing). WebView2 is preinstalled on Win 10/11.
3. **Tauri CLI** — `cargo install tauri-cli --version "^2" --locked`
4. **ffmpeg** — either `winget install Gyan.FFmpeg` (puts it on PATH), or drop `ffmpeg.exe` next to the Recap executable, or set the `RECAP_FFMPEG` env var to its full path. Needs a build with `ddagrab` and `dshow` (the standard gyan.dev full builds have both).

## Run

```
cd recap
cargo tauri dev
```

First build takes a few minutes (cold crate compile). Release installer:

```
cargo tauri build     # NSIS installer in src-tauri/target/release/bundle/nsis/
```

Run the logic tests (arg builder, region rounding, concat escaping, dshow parsing):

```
cd src-tauri
cargo test
```

## How it works

```
src/                     vanilla HTML/CSS/JS, no Node needed (withGlobalTauri)
  index.html/main.js     main control window
  overlay.html/.js       transparent per-monitor overlay for region drag-select
src-tauri/src/
  lib.rs                 commands, tray, global hotkeys, overlay window, events
  recorder.rs            state machine: Idle → Countdown → Recording ⇄ Paused → Finalizing
  ffmpeg.rs              locate binary, runtime encoder probe, dshow device list,
                         ddagrab command builder, concat
```

- **Capture** is ffmpeg's `ddagrab` lavfi source (Desktop Duplication on the GPU). Region capture uses its native `offset_x/offset_y/video_size` — the overlay reports physical pixels (DPI-aware) rounded to even values for H.264.
- **Encoders** are probed at startup with a real 3-frame test encode (`-encoders` lies — it lists NVENC even without an NVIDIA GPU). NVENC/AMF encode the D3D11 frames directly; QSV/x264 get a `hwdownload,format=bgra` hop.
- **Pause** stops the current ffmpeg segment gracefully (writes `q` to stdin; kill only after a 4 s timeout, since a hard kill truncates the MP4). Resume starts `seg_001.mp4`, `seg_002.mp4`, … Stop losslessly concatenates segments with the concat demuxer.
- **Mic** is a second `dshow` input encoded to AAC 160k.
- Closing the window hides to the tray; recording keeps running. Quit from the tray.

## Known limitations (v0.1)

- **Not compiled in the environment this was written in** (no Windows/Tauri toolchain there). The Rust is written carefully against Tauri 2 stable APIs and the pure logic is unit-tested, but expect the possibility of a minor API nit on first `cargo tauri dev`.
- **Monitor mapping**: the display dropdown assumes Tauri's monitor enumeration order matches ddagrab's `output_idx`. True on typical single-GPU setups; multi-adapter systems may need the index swapped. Proper DXGI enumeration is on the roadmap.
- **No system audio** — mic only. Desktop Duplication carries no audio and ffmpeg's dshow has no WASAPI loopback; capturing app audio needs a loopback device (VB-Cable / "Stereo Mix") or native WASAPI code later.
- A/V sync on long mic recordings is untuned (no `aresample` correction yet).
- Settings aren't persisted between launches yet.

## Roadmap

GIF export (`palettegen`/`paletteuse` two-pass) · webcam picture-in-picture (second dshow video + overlay filter) · system audio via WASAPI loopback · click highlighting · trim-before-save · DXGI-accurate monitor mapping · settings persistence (`tauri-plugin-store`) · configurable hotkeys.
