// Region selection overlay.
// The window covers one monitor exactly, so window coordinates == monitor
// coordinates. ffmpeg's ddagrab wants PHYSICAL pixels, so everything emitted
// is scaled by devicePixelRatio and rounded to even numbers (H.264 4:2:0).

const { emit } = window.__TAURI__.event;

const canvas = document.getElementById("c");
const ctx = canvas.getContext("2d");

let dragging = false;
let sx = 0, sy = 0;   // drag start (CSS px)
let cx = 0, cy = 0;   // current cursor (CSS px)

function resize() {
  const dpr = window.devicePixelRatio || 1;
  canvas.width = Math.round(window.innerWidth * dpr);
  canvas.height = Math.round(window.innerHeight * dpr);
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  draw();
}
window.addEventListener("resize", resize);

function physicalRect() {
  const dpr = window.devicePixelRatio || 1;
  const left = Math.min(sx, cx);
  const top = Math.min(sy, cy);
  const w = Math.abs(cx - sx);
  const h = Math.abs(cy - sy);
  let px = Math.round(left * dpr);
  let py = Math.round(top * dpr);
  let pw = Math.round(w * dpr);
  let ph = Math.round(h * dpr);
  pw -= pw % 2;
  ph -= ph % 2;
  if (px < 0) px = 0;
  if (py < 0) py = 0;
  return { x: px, y: py, width: pw, height: ph };
}

function draw() {
  const W = window.innerWidth;
  const H = window.innerHeight;
  ctx.clearRect(0, 0, W, H);

  // dim everything
  ctx.fillStyle = "rgba(10, 11, 14, 0.42)";
  ctx.fillRect(0, 0, W, H);

  if (dragging) {
    const left = Math.min(sx, cx);
    const top = Math.min(sy, cy);
    const w = Math.abs(cx - sx);
    const h = Math.abs(cy - sy);

    // punch out the selection
    ctx.clearRect(left, top, w, h);

    // border
    ctx.strokeStyle = "#ff4545";
    ctx.lineWidth = 2;
    ctx.strokeRect(left + 1, top + 1, Math.max(w - 2, 0), Math.max(h - 2, 0));

    // size label (physical pixels — what actually gets recorded)
    const r = physicalRect();
    const label = `${r.width} × ${r.height}`;
    ctx.font = "12px ui-monospace, Consolas, monospace";
    const tw = ctx.measureText(label).width;
    const lx = Math.min(left + 6, W - tw - 14);
    const ly = top > 26 ? top - 10 : top + 18;
    ctx.fillStyle = "rgba(16, 18, 22, 0.92)";
    ctx.fillRect(lx - 5, ly - 12, tw + 10, 17);
    ctx.fillStyle = "#e8ebf0";
    ctx.fillText(label, lx, ly);
  } else {
    // crosshair guides
    ctx.strokeStyle = "rgba(255, 69, 69, 0.55)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(cx + 0.5, 0);
    ctx.lineTo(cx + 0.5, H);
    ctx.moveTo(0, cy + 0.5);
    ctx.lineTo(W, cy + 0.5);
    ctx.stroke();
  }
}

canvas.addEventListener("pointerdown", (e) => {
  if (e.button === 2) return; // right-click handled below
  dragging = true;
  sx = cx = e.clientX;
  sy = cy = e.clientY;
  canvas.setPointerCapture(e.pointerId);
  draw();
});

canvas.addEventListener("pointermove", (e) => {
  cx = e.clientX;
  cy = e.clientY;
  draw();
});

canvas.addEventListener("pointerup", (e) => {
  if (!dragging || e.button === 2) return;
  dragging = false;
  cx = e.clientX;
  cy = e.clientY;
  const r = physicalRect();
  if (r.width < 32 || r.height < 32) {
    // too small — treat as a stray click, keep selecting
    draw();
    return;
  }
  emit("region-selected", r);
});

window.addEventListener("keydown", (e) => {
  if (e.key === "Escape") emit("region-cancelled", {});
});

window.addEventListener("contextmenu", (e) => {
  e.preventDefault();
  emit("region-cancelled", {});
});

resize();
