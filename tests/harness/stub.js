// Injected into a copy of editor.html BEFORE editor.js. Gives editor.js the
// two things it needs from a real Tauri host: window.__TAURI__, and a canvas
// it can actually decode an image into.
(function () {
  // A fine checkerboard, not a flat fill: the blur test needs real local
  // detail in the redacted region to prove pixels actually changed.
  function fixtureDataUrl() {
    const c = document.createElement("canvas");
    c.width = 1000;
    c.height = 800;
    const cx = c.getContext("2d");
    const tile = 16;
    for (let y = 0; y < c.height; y += tile) {
      for (let x = 0; x < c.width; x += tile) {
        const on = (x / tile + y / tile) % 2 === 0;
        cx.fillStyle = on ? "#2a6f97" : "#e8b84b";
        cx.fillRect(x, y, tile, tile);
      }
    }
    return c.toDataURL("image/png");
  }

  window.__TAURI__ = {
    core: {
      invoke: (cmd) => {
        if (cmd === "load_image") return Promise.resolve(fixtureDataUrl());
        return Promise.resolve(null);
      },
    },
    event: {
      listen: () => Promise.resolve(() => {}),
    },
  };

  // Synthetic PointerEvents built with `new PointerEvent(...)` are not backed
  // by a real OS pointer-down, so Chrome's setPointerCapture throws
  // InvalidPointerId for them. editor.js calls it unconditionally on
  // pointerdown and isn't ours to change, so neutralize it here instead —
  // capture semantics don't matter when every synthetic event is dispatched
  // straight at the canvas anyway.
  HTMLElement.prototype.setPointerCapture = function () {};
  HTMLElement.prototype.releasePointerCapture = function () {};
})();
