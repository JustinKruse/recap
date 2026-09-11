# Recap — notes for Claude Code

Tauri 2 (Rust) + vanilla JS webview, driving ffmpeg for capture. No Node build
step: `withGlobalTauri` is on and `src/` is served as-is.

## You can drive this app directly — do that instead of guessing

`cargo tauri dev` opens a debug-only JSON socket on `127.0.0.1:7333`.
`scripts/recapctl` is the client. **Use it.** Unit tests cover the arg builders
and pure logic; everything else — event wiring, capabilities, the canvas — is
only observable through a running app.

```
scripts/recapctl state                       # recorder status, config, region, screens
scripts/recapctl windows                     # open window labels
scripts/recapctl eval main 'document.title'  # JS in any window, value returned
scripts/recapctl eval editor 'shapes.length'
scripts/recapctl invoke still                # still | grab_text | record_start | record_stop | pause
scripts/recapctl shot editor /tmp/ed.png     # cropped to that window — then Read the PNG
scripts/recapctl shot screen /tmp/full.png
```

`shot` crops a real `screencapture` to the window's own reported bounds, so it
shows what is actually composited. Reading that PNG back is the only way to see
the UI; there is no other channel.

Two real bugs were found this way and would not have been caught otherwise:
the editor window was missing from `capabilities/default.json` (so all its
events were silently denied), and unsaved annotations were being destroyed on
re-capture.

### When the socket misbehaves

- **Right after a rebuild** the dev watcher restarts the app; the socket may
  briefly be served by the dying process. Wait for `devctl: listening` in the
  dev log before driving it.
- `devctl` disables App Nap, because an occluded window otherwise gets its
  webview throttled and `eval` replies arrive tens of seconds late.
- `eval` runs in the page's global scope, so top-level `let` bindings in
  `editor.js` (`shapes`, `selected`, `zoom`) and function declarations
  (`commit`, `setTool`, `render`) are all reachable.

## Headless CLI

The binary also runs without a GUI, which is the right tool when you just want
pixels or text rather than to test the app:

```
recap shot [--display N] [--region X,Y,W,H] out.png
recap ocr  [--display N] [--region X,Y,W,H] [--json] [image.png]
```

`recap ocr` with no image grabs the screen, reads it, prints the text and
deletes the scratch file.

## Testing

`cd src-tauri && cargo test` — both capture backends' arg builders compile and
are tested on every platform, so the Windows path stays covered from a Mac.

For editor changes, there is a headless harness pattern: copy `src/editor.*`
somewhere, inject a stub that defines `window.__TAURI__`, and drive it with
Chrome `--headless --screenshot` / `--dump-dom`. That is how selection,
hit-testing and undo/redo were verified without a GUI.

## Landmines

- **Never put `-framerate` before `-i` on an AVFoundation screen input.** ffmpeg
  then can't estimate the rate, duplicates frames without bound, ignores `-t`
  and never terminates. There's a regression test.
- A capture producing nothing is *not* proof of a permission problem. Check
  `screencapture -x -D 1 /tmp/t.png` first.
- `devctl` is `#[cfg(debug_assertions)]` and does arbitrary JS eval. It must
  never ship in a release build.
- **A fresh clone cannot build.** `/vendor/` is gitignored (a 45 MB static
  ffmpeg does not belong in git history) but it is a *build input* —
  `bundle.resources` copies it into `Recap.app/Contents/Resources`. Repopulate it
  per `docs/RELEASE.md` step 0 before expecting `cargo tauri build` to work; the
  bundle step fails loudly rather than shipping a broken app.
- Custom Tauri commands are **not** capability-gated, but `core:*` ones are. A
  new window that only calls custom commands will appear to work while all its
  events are silently dropped. Add every new window label to
  `capabilities/default.json`.

## Repo

`github.com/JustinKruse/recap` (public, `main`). CI runs from
`.github/workflows/ci.yml` — pushing workflow changes needs a token with the
`workflow` scope, not just `repo`.
