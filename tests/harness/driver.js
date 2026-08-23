// Injected into a copy of editor.html AFTER editor.js. Runs in the same
// script-scope as editor.js, so its top-level `let`/function declarations
// (shapes, selected, redoStack, stepNext, dirty, zoom, tool, draft, start,
// dragFrom, movedAny, commit, undo, redo, setTool, setZoom, hitTest, render,
// canvas, ctx, img) are all directly reachable here — same trick devctl's
// `eval` uses against a live app (see CLAUDE.md).
//
// Results are written into a <script type="application/json"> tag so
// run.js can pull them out of `--dump-dom` output without HTML-entity
// escaping to worry about.

(async function () {
  const results = [];

  function assert(cond, msg) {
    if (!cond) throw new Error(msg || "assertion failed");
  }
  function assertEqual(actual, expected, msg) {
    if (actual !== expected) {
      throw new Error(`${msg || "not equal"} (expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)})`);
    }
  }
  function buffersEqual(a, b) {
    if (a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
    return true;
  }

  function test(name, fn) {
    try {
      fn();
      results.push({ name, pass: true });
    } catch (e) {
      results.push({ name, pass: false, error: String((e && e.stack) || e) });
    }
  }

  function waitFor(pred, timeoutMs) {
    return new Promise((resolve, reject) => {
      const started = Date.now();
      (function poll() {
        if (pred()) return resolve();
        if (Date.now() - started > timeoutMs) return reject(new Error("timed out waiting for condition"));
        setTimeout(poll, 20);
      })();
    });
  }

  // ---- pointer helpers, coordinates in image pixels --------------------------
  // setZoom(1) forces scale() === 1 regardless of window size or fitScale(),
  // so image-pixel coordinates map 1:1 onto the canvas's own client rect.
  function firePointer(type, x, y, extra) {
    const r = canvas.getBoundingClientRect();
    const ev = new PointerEvent(
      type,
      Object.assign(
        {
          bubbles: true,
          cancelable: true,
          composed: true,
          clientX: r.left + x,
          clientY: r.top + y,
          button: 0,
          buttons: type === "pointerup" ? 0 : 1,
          pointerId: 1,
          pointerType: "mouse",
          isPrimary: true,
        },
        extra || {}
      )
    );
    canvas.dispatchEvent(ev);
  }
  function drag(x1, y1, x2, y2) {
    firePointer("pointerdown", x1, y1);
    firePointer("pointermove", x2, y2);
    firePointer("pointerup", x2, y2);
  }
  function click(x, y) {
    firePointer("pointerdown", x, y);
    firePointer("pointerup", x, y);
  }
  function keydown(key) {
    window.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
  }

  function resetState() {
    shapes = [];
    redoStack = [];
    selected = -1;
    stepNext = 1;
    dirty = false;
    draft = null;
    start = null;
    dragFrom = null;
    movedAny = false;
    setTool("select");
    render();
  }

  let setupFailed = false;
  try {
    await waitFor(() => typeof img !== "undefined" && img && canvas.width > 0, 5000);
    setZoom(1);
  } catch (e) {
    setupFailed = true;
    results.push({ name: "setup: fixture image loads", pass: false, error: String((e && e.stack) || e) });
  }

  if (!setupFailed) {
    // ---- 1. REGRESSION: bare click after a completed drag ----------------------
    // Pins a real bug: a click at (900,150) after a drag ending at (500,700)
    // used to conjure a 400x550 phantom box, because the endpoint came from
    // stale state rather than the click's own event. pointerup must always
    // read its endpoint from the current event.
    test("bare click after a completed drag does not create a phantom shape", () => {
      resetState();
      setTool("box");
      drag(100, 150, 500, 700); // real drag: commits one 400x550 box
      assertEqual(shapes.length, 1, "the drag itself should commit exactly one shape");
      const before = shapes.length;
      click(900, 150); // pointerdown + pointerup, NO pointermove in between
      assertEqual(shapes.length, before, "a bare click after a drag must not add a shape");
      // Belt and suspenders: the original box must still be exactly what the
      // real drag produced, not silently replaced by a shape anchored at the
      // click/drag boundary the way the original bug did.
      assertEqual(shapes[0].x, 100, "the real drag's box is untouched (x)");
      assertEqual(shapes[0].y, 150, "the real drag's box is untouched (y)");
    });

    // ---- 2. drag geometry + sub-threshold slip rejection ------------------------
    test("a drag commits a shape of the right type and geometry", () => {
      resetState();
      setTool("box");
      drag(200, 150, 600, 500);
      assertEqual(shapes.length, 1, "one shape committed");
      const s = shapes[0];
      assertEqual(s.type, "box", "shape type");
      assertEqual(s.x, 200, "x");
      assertEqual(s.y, 150, "y");
      assertEqual(s.w, 400, "w");
      assertEqual(s.h, 350, "h");
    });

    test("a sub-threshold drag is rejected as a slip, for boxes and for lines", () => {
      resetState();
      setTool("box");
      drag(10, 10, 12, 12); // 2x2 — below the 4px w/h floor
      assertEqual(shapes.length, 0, "a 2x2 box drag must not commit a shape");

      setTool("line");
      drag(700, 700, 703, 702); // length ~3.6 — below the 6px length floor
      assertEqual(shapes.length, 0, "a ~3.6px line drag must not commit a shape");

      drag(50, 600, 50, 650); // length 50 — comfortably over threshold
      assertEqual(shapes.length, 1, "a real drag past the threshold still commits");
      assertEqual(shapes[0].type, "line", "shape type");
    });

    // ---- 3. selection --------------------------------------------------------
    test("click selects the topmost shape under the cursor", () => {
      resetState();
      commit({ type: "box", x: 0, y: 0, w: 100, h: 100, color: "#ff4545", stroke: 4 });
      commit({ type: "box", x: 50, y: 50, w: 100, h: 100, color: "#4aa8ff", stroke: 4 });
      setTool("select");
      click(75, 75); // inside both overlapping boxes
      assertEqual(selected, 1, "the later (topmost, on-screen) shape wins the hit test");
    });

    test("clicking empty space deselects", () => {
      resetState();
      commit({ type: "box", x: 0, y: 0, w: 100, h: 100, color: "#ff4545", stroke: 4 });
      setTool("select");
      click(50, 50);
      assertEqual(selected, 0, "sanity: selected the box first");
      click(900, 700); // nothing there
      assertEqual(selected, -1, "clicking empty space deselects");
    });

    test("an arrow is selectable by its thin shaft, not just its bounding box", () => {
      resetState();
      commit({ type: "arrow", x1: 300, y1: 300, x2: 500, y2: 300, color: "#ff4545", stroke: 4 });
      setTool("select");
      // Inside the arrow's padded bounding box, but ~14px off the actual
      // segment — beyond the hit margin. A bbox-only hit test would wrongly
      // select here; segment-distance hit testing must not.
      click(290, 310);
      assertEqual(selected, -1, "a point inside the bbox but off the shaft must not select it");
      click(400, 300); // dead center of the shaft
      assertEqual(selected, 0, "a point on the shaft selects the arrow");
    });

    test("Delete removes the selected shape and rolls back the step counter", () => {
      resetState();
      setTool("step");
      click(150, 150); // commits step #1 on pointerdown; stepNext -> 2
      assertEqual(shapes.length, 1, "step shape committed");
      assertEqual(stepNext, 2, "step counter advanced");
      setTool("select");
      click(150, 150);
      assertEqual(selected, 0, "sanity: step shape is selected");
      keydown("Delete");
      assertEqual(shapes.length, 0, "Delete removes the shape");
      assertEqual(selected, -1, "selection clears after delete");
      assertEqual(stepNext, 1, "deleting a step shape rolls the counter back");
    });

    // ---- 4. undo/redo ----------------------------------------------------------
    test("undo/redo is lossless", () => {
      resetState();
      commit({ type: "box", x: 10, y: 10, w: 50, h: 40, color: "#ff4545", stroke: 4 });
      commit({ type: "ellipse", x: 200, y: 200, w: 80, h: 60, color: "#4aa8ff", stroke: 4 });
      const original = JSON.stringify(shapes);

      undo();
      assertEqual(shapes.length, 1, "first undo removes the most recent shape");
      undo();
      assertEqual(shapes.length, 0, "second undo empties the canvas");
      undo(); // one too many — must be a no-op, not throw or corrupt state
      assertEqual(shapes.length, 0, "undo past empty history is a no-op");

      redo();
      redo();
      assertEqual(JSON.stringify(shapes), original, "redoing twice restores the exact original shapes, unchanged");
      redo(); // one too many — also a no-op
      assertEqual(JSON.stringify(shapes), original, "redo past the end of the redo stack is a no-op");
    });

    test("undo/redo of a step shape rolls the counter both ways", () => {
      resetState();
      setTool("step");
      click(400, 400);
      assertEqual(stepNext, 2, "sanity: counter advanced");
      undo();
      assertEqual(stepNext, 1, "undo rolls the step counter back");
      redo();
      assertEqual(stepNext, 2, "redo advances it again");
    });

    test("a new commit after undo forks history — redo stack is dropped", () => {
      resetState();
      commit({ type: "box", x: 10, y: 10, w: 50, h: 40, color: "#ff4545", stroke: 4 });
      undo();
      assertEqual(redoStack.length, 1, "sanity: the undone shape is on the redo stack");
      commit({ type: "box", x: 300, y: 300, w: 20, h: 20, color: "#ff4545", stroke: 4 });
      assertEqual(redoStack.length, 0, "committing after an undo clears the redo stack");
    });

    // ---- 5. blur is non-destructive and non-compounding ------------------------
    test("blur samples the source bitmap: non-destructive and non-compounding", () => {
      resetState();
      const box = { x: 100, y: 100, w: 200, h: 150 };
      const sample = () => ctx.getImageData(box.x, box.y, box.w, box.h).data;

      const original = sample();
      commit({ type: "blur", x: box.x, y: box.y, w: box.w, h: box.h, color: "#000", stroke: 4 });
      const afterFirst = sample();
      assert(!buffersEqual(original, afterFirst), "blurring the region actually changes its pixels");

      // Blurring the same region again must sample the pristine bitmap again,
      // not the already-blurred canvas — so the result is bit-identical to
      // the first blur, never progressively blurrier.
      commit({ type: "blur", x: box.x, y: box.y, w: box.w, h: box.h, color: "#000", stroke: 4 });
      const afterSecond = sample();
      assert(buffersEqual(afterFirst, afterSecond), "a second blur over the same region reproduces identical pixels — blurs never compound");

      undo();
      undo();
      const restored = sample();
      assert(buffersEqual(original, restored), "undoing both blurs restores the exact original pixels");
    });

    test("blur ignores whatever is drawn underneath it — it never samples the live canvas", () => {
      // The identical-box double-blur above is idempotent even for a buggy
      // canvas-sampling implementation, because block-resampling a region
      // with itself at the same grid is a fixed point. This test forces a
      // real discriminator: paint something else under the blur first. A
      // correct implementation samples the pristine bitmap and is blind to
      // it; one that samples the canvas would bake the highlight color in.
      const box = { x: 100, y: 100, w: 200, h: 150 };
      const sample = () => ctx.getImageData(box.x, box.y, box.w, box.h).data;

      resetState();
      commit({ type: "blur", x: box.x, y: box.y, w: box.w, h: box.h, color: "#000", stroke: 4 });
      const control = sample();

      resetState();
      commit({ type: "highlight", x: box.x, y: box.y, w: box.w, h: box.h, color: "#ff0000", stroke: 4 });
      commit({ type: "blur", x: box.x, y: box.y, w: box.w, h: box.h, color: "#000", stroke: 4 });
      const withSomethingUnderneath = sample();

      assert(
        buffersEqual(control, withSomethingUnderneath),
        "blur output must be identical whether or not a shape sits underneath it"
      );
    });
  }

  const summary = {
    total: results.length,
    passed: results.filter((r) => r.pass).length,
    failed: results.filter((r) => !r.pass).length,
  };

  const out = document.createElement("script");
  out.type = "application/json";
  out.id = "recap-test-results";
  out.textContent = JSON.stringify({ results, summary });
  document.documentElement.appendChild(out);
  document.title = "RECAP_TESTS_DONE";
})();
