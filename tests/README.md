# editor.js regression tests

`src/editor.js` is ~700 lines of canvas code (shapes, hit-testing, undo/redo,
non-destructive blur) with no app server and no build step. These tests drive
the real file in headless Chrome instead of reimplementing any of its logic.

## Run it

```
node tests/run.js
# or
npm test
```

Needs `/usr/local/bin/node` and Google Chrome at
`/Applications/Google Chrome.app/Contents/MacOS/Google Chrome` (override with
`RECAP_TEST_CHROME=/path/to/chrome`). No npm install, no dependencies.

Exit code is non-zero if anything fails, so it's CI-friendly as-is.

## How it works

1. `run.js` copies `src/editor.js`, `editor.css`, `styles.css` and
   `editor.html` into a fresh temp dir (a new one per run, so nothing here is
   stateful).
2. It injects `harness/stub.js` right before editor.js's own `<script>` tag,
   and `harness/driver.js` right after it, into that copy of `editor.html`.
   - `stub.js` defines `window.__TAURI__` (`core.invoke("load_image", …)`
     resolves to a data URL of a synthetic checkerboard image;
     `event.listen` is a no-op) and neutralizes
     `setPointerCapture`/`releasePointerCapture`, which throw on synthetic
     pointer ids that were never backed by a real OS pointer-down.
   - `driver.js` waits for the image to decode, forces `setZoom(1)` (so
     `scale() === 1` and image-pixel coordinates map straight onto the
     canvas's own client rect regardless of window size), then runs the test
     suite and writes `{results, summary}` as JSON into a
     `<script type="application/json" id="recap-test-results">` tag.
3. `run.js` launches
   `chrome --headless=new --dump-dom --virtual-time-budget=10000 …` on that
   HTML file with `?path=test.png` in the URL (so editor.js's own
   `load(initial)` bootstraps itself, same as a real launch), then regexes
   the results block out of the dumped DOM and reports it.

This is the same pattern documented in `CLAUDE.md` / used by `devctl`: because
`editor.js` is a plain classic script (no module wrapper, no IIFE), its
top-level `let` bindings (`shapes`, `selected`, `redoStack`, `stepNext`,
`dirty`, `tool`, `draft`, `start`, `dragFrom`, `movedAny`, `zoom`) and
function declarations (`commit`, `undo`, `redo`, `setTool`, `setZoom`,
`hitTest`, `render`, …) all live in the same script-scope as anything loaded
after it in the same document — so `driver.js` can call and inspect them
directly, no eval, no bundler, no instrumentation of editor.js itself.

## What's covered

- **Regression pin**: a bare click (pointerdown+pointerup, no pointermove)
  right after a completed drag must not create a shape — a real bug where the
  endpoint came from stale state instead of the click's own event.
- Drag → shape type/geometry, and sub-threshold drags (a few px, or a
  near-zero-length line) rejected as slips, for both box-like and
  segment-like shapes.
- Selection: topmost shape wins on overlap; empty space deselects; an arrow
  is hit-tested against its actual segment (a point inside its padded
  bounding box but off the shaft must *not* select it); Delete removes the
  selection and rolls the step-number counter back.
- Undo/redo: lossless round-trip (including no-ops past either end of the
  stack), the step counter moves with it, and a fresh commit after an undo
  forks history (drops the redo stack).
- Blur: redacting changes pixels; undo restores the source bitmap's exact
  pixels; and — the sharper check — blurring the same region with something
  else drawn underneath it produces *identical* output to blurring it alone,
  which only holds if blur always samples the pristine image and never the
  live canvas. (A same-box double-blur is idempotent even for a buggy
  canvas-sampling implementation, since block-resampling a region with
  itself at the same grid is a fixed point — this harness verified that by
  temporarily reintroducing that exact bug and confirming only this
  underneath-shape test catches it.)

## Adding a test

Add another `test("name", () => { … })` block in `harness/driver.js`. Use
`resetState()` to clear shapes/tool/history between cases, `commit({...})` to
seed shapes directly, `drag(x1,y1,x2,y2)` / `click(x,y)` / `keydown(key)` for
pointer/keyboard flows, and `assert`/`assertEqual` for checks. No test needs
to be async — dispatching a DOM event runs its listeners synchronously.
