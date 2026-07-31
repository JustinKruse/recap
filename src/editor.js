// Recap — annotation editor.
//
// Shapes are kept as objects in image-pixel coordinates and the whole canvas is
// redrawn from the pristine image on every change. That's what makes undo free
// and blur non-destructive: nothing is ever painted into the source bitmap, so
// a redaction can be taken back even after other marks land on top of it.

const tauri = window.__TAURI__;
if (!tauri) {
  document.body.innerHTML =
    '<p style="padding:20px;font-family:system-ui">Open this through the Tauri app.</p>';
  throw new Error("Tauri API not available");
}
const invoke = tauri.core.invoke;
const { listen } = tauri.event;

const $ = (id) => document.getElementById(id);
const canvas = $("canvas");
const ctx = canvas.getContext("2d");
const wrap = $("wrap");
const textInput = $("text-input");

const COLORS = ["#ff4545", "#ffb03a", "#ffe14d", "#43d17c", "#4aa8ff", "#b06cff", "#ffffff", "#101216"];

// ---- state -----------------------------------------------------------------

let img = null;          // pristine source bitmap — never drawn into
let imagePath = "";
let shapes = [];         // committed annotations, in draw order
let redoStack = [];
let draft = null;        // shape under the cursor mid-drag
let tool = "arrow";
let color = COLORS[0];
let stroke = 4;
let stepNext = 1;
let dirty = false;

// ---- toast -----------------------------------------------------------------

const els = { toast: $("toast"), toastMsg: $("toast-msg"), toastAction: $("toast-action") };
let toastTimer = null;
function toast(message, kind = "ok", action = null) {
  els.toast.hidden = false;
  els.toast.className = `toast is-${kind}`;
  els.toastMsg.textContent = message;
  if (action) {
    els.toastAction.hidden = false;
    els.toastAction.textContent = action.label;
    els.toastAction.onclick = action.onClick;
  } else {
    els.toastAction.hidden = true;
    els.toastAction.onclick = null;
  }
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (els.toast.hidden = true), kind === "error" ? 9000 : 4500);
}

// ---- geometry ---------------------------------------------------------------

/// Displayed pixels per image pixel. The canvas is at natural size and CSS
/// scales it down to fit, so every pointer coordinate needs dividing by this.
function scale() {
  return canvas.clientWidth / canvas.width || 1;
}

function toImage(e) {
  const r = canvas.getBoundingClientRect();
  const s = scale();
  return { x: (e.clientX - r.left) / s, y: (e.clientY - r.top) / s };
}

/// Drags run in any direction; shapes are stored with a positive extent.
function normalize(a, b) {
  return {
    x: Math.min(a.x, b.x),
    y: Math.min(a.y, b.y),
    w: Math.abs(b.x - a.x),
    h: Math.abs(b.y - a.y),
  };
}

// ---- drawing ----------------------------------------------------------------

function drawArrow(s) {
  const head = Math.max(10, s.stroke * 4);
  const angle = Math.atan2(s.y2 - s.y1, s.x2 - s.x1);
  const len = Math.hypot(s.x2 - s.x1, s.y2 - s.y1);
  if (len < 1) return;
  // Stop the shaft short of the tip so the stroke cap doesn't poke through it.
  const backoff = Math.min(head * 0.9, len);
  ctx.strokeStyle = s.color;
  ctx.fillStyle = s.color;
  ctx.lineWidth = s.stroke;
  ctx.lineCap = "round";
  ctx.beginPath();
  ctx.moveTo(s.x1, s.y1);
  ctx.lineTo(s.x2 - Math.cos(angle) * backoff, s.y2 - Math.sin(angle) * backoff);
  ctx.stroke();
  ctx.beginPath();
  ctx.moveTo(s.x2, s.y2);
  ctx.lineTo(
    s.x2 - Math.cos(angle - 0.42) * head,
    s.y2 - Math.sin(angle - 0.42) * head
  );
  ctx.lineTo(
    s.x2 - Math.cos(angle + 0.42) * head,
    s.y2 - Math.sin(angle + 0.42) * head
  );
  ctx.closePath();
  ctx.fill();
}

function drawBlur(s) {
  if (s.w < 2 || s.h < 2) return;
  // Sample the pristine image, never the canvas: that keeps redaction
  // reversible and stops an earlier blur from being blurred again.
  const block = Math.max(4, s.stroke * 3);
  const tw = Math.max(1, Math.round(s.w / block));
  const th = Math.max(1, Math.round(s.h / block));
  const tmp = document.createElement("canvas");
  tmp.width = tw;
  tmp.height = th;
  const tctx = tmp.getContext("2d");
  tctx.imageSmoothingEnabled = false;
  tctx.drawImage(img, s.x, s.y, s.w, s.h, 0, 0, tw, th);
  ctx.save();
  ctx.imageSmoothingEnabled = false;
  ctx.drawImage(tmp, 0, 0, tw, th, s.x, s.y, s.w, s.h);
  ctx.restore();
}

function drawShape(s) {
  ctx.save();
  ctx.lineJoin = "round";
  ctx.lineCap = "round";
  ctx.strokeStyle = s.color;
  ctx.fillStyle = s.color;
  ctx.lineWidth = s.stroke;

  switch (s.type) {
    case "arrow":
      drawArrow(s);
      break;
    case "line":
      ctx.beginPath();
      ctx.moveTo(s.x1, s.y1);
      ctx.lineTo(s.x2, s.y2);
      ctx.stroke();
      break;
    case "box":
      ctx.strokeRect(s.x, s.y, s.w, s.h);
      break;
    case "ellipse":
      ctx.beginPath();
      ctx.ellipse(s.x + s.w / 2, s.y + s.h / 2, s.w / 2, s.h / 2, 0, 0, Math.PI * 2);
      ctx.stroke();
      break;
    case "highlight":
      // Multiply keeps the text under the wash readable, the way a real
      // highlighter pen works — plain alpha just fogs it.
      ctx.globalCompositeOperation = "multiply";
      ctx.globalAlpha = 0.45;
      ctx.fillRect(s.x, s.y, s.w, s.h);
      break;
    case "blur":
      drawBlur(s);
      break;
    case "text": {
      const size = s.size;
      ctx.font = `600 ${size}px system-ui, -apple-system, "Segoe UI", sans-serif`;
      ctx.textBaseline = "top";
      // A dark rim keeps light text legible over a light screenshot.
      ctx.lineWidth = Math.max(2, size / 8);
      ctx.strokeStyle = "rgba(0,0,0,0.55)";
      s.text.split("\n").forEach((line, i) => {
        const y = s.y + i * size * 1.2;
        ctx.strokeText(line, s.x, y);
        ctx.fillText(line, s.x, y);
      });
      break;
    }
    case "step": {
      const r = s.radius;
      ctx.beginPath();
      ctx.arc(s.x, s.y, r, 0, Math.PI * 2);
      ctx.fill();
      ctx.lineWidth = Math.max(2, r / 8);
      ctx.strokeStyle = "rgba(255,255,255,0.9)";
      ctx.stroke();
      ctx.fillStyle = "#fff";
      ctx.font = `700 ${Math.round(r * 1.15)}px system-ui, -apple-system, sans-serif`;
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      ctx.fillText(String(s.n), s.x, s.y + r * 0.04);
      break;
    }
  }
  ctx.restore();
}

function render() {
  if (!img) return;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(img, 0, 0);
  for (const s of shapes) drawShape(s);
  if (draft) drawShape(draft);
}

// ---- history ----------------------------------------------------------------

function commit(shape) {
  shapes.push(shape);
  redoStack.length = 0; // a new mark forks history
  dirty = true;
  render();
}

function undo() {
  const s = shapes.pop();
  if (!s) return;
  redoStack.push(s);
  if (s.type === "step") stepNext = Math.max(1, stepNext - 1);
  dirty = true;
  render();
}

function redo() {
  const s = redoStack.pop();
  if (!s) return;
  shapes.push(s);
  if (s.type === "step") stepNext += 1;
  dirty = true;
  render();
}

// ---- pointer ----------------------------------------------------------------

let start = null;

canvas.addEventListener("pointerdown", (e) => {
  if (e.button !== 0) return;
  if (!textInput.hidden) { commitText(); return; }
  const p = toImage(e);

  if (tool === "text") { openTextInput(p); return; }
  if (tool === "step") {
    commit({
      type: "step",
      x: p.x,
      y: p.y,
      radius: Math.max(14, stroke * 5),
      n: stepNext++,
      color,
      stroke,
    });
    return;
  }

  start = p;
  canvas.setPointerCapture(e.pointerId);
});

canvas.addEventListener("pointermove", (e) => {
  if (!start) return;
  draft = buildShape(start, toImage(e));
  render();
});

canvas.addEventListener("pointerup", (e) => {
  if (!start) return;
  const shape = buildShape(start, toImage(e));
  start = null;
  draft = null;
  if (isTooSmall(shape)) { render(); return; }
  commit(shape);
});

function buildShape(a, b) {
  if (tool === "arrow" || tool === "line") {
    return { type: tool, x1: a.x, y1: a.y, x2: b.x, y2: b.y, color, stroke };
  }
  const r = normalize(a, b);
  return { type: tool, ...r, color, stroke };
}

/// A click that barely moved is a slip, not a shape.
function isTooSmall(s) {
  if (s.type === "arrow" || s.type === "line") {
    return Math.hypot(s.x2 - s.x1, s.y2 - s.y1) < 6;
  }
  return s.w < 4 || s.h < 4;
}

// ---- text tool ---------------------------------------------------------------

let textAt = null;

function openTextInput(p) {
  textAt = p;
  const size = Math.max(16, stroke * 8);
  const s = scale();
  textInput.value = "";
  textInput.hidden = false;
  textInput.style.left = `${p.x * s}px`;
  textInput.style.top = `${p.y * s}px`;
  textInput.style.fontSize = `${size * s}px`;
  textInput.style.color = color;
  textInput.rows = 1;
  textInput.focus();
}

function commitText() {
  const value = textInput.value.replace(/\s+$/, "");
  textInput.hidden = true;
  if (!textAt || !value) { textAt = null; return; }
  commit({
    type: "text",
    x: textAt.x,
    y: textAt.y,
    text: value,
    size: Math.max(16, stroke * 8),
    color,
  });
  textAt = null;
}

textInput.addEventListener("keydown", (e) => {
  // Enter commits; Shift+Enter is a newline, so multi-line callouts still work.
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); commitText(); }
  else if (e.key === "Escape") { e.preventDefault(); textInput.hidden = true; textAt = null; }
  e.stopPropagation(); // don't let tool hotkeys fire while typing
});
textInput.addEventListener("blur", commitText);
textInput.addEventListener("input", () => {
  textInput.rows = textInput.value.split("\n").length;
});

// ---- toolbar ------------------------------------------------------------------

function setTool(next) {
  tool = next;
  document.querySelectorAll(".tool").forEach((b) => {
    b.setAttribute("aria-pressed", String(b.dataset.tool === next));
  });
}
document.querySelectorAll(".tool").forEach((b) => {
  b.addEventListener("click", () => setTool(b.dataset.tool));
});

const swatchBox = $("swatches");
COLORS.forEach((c, i) => {
  const b = document.createElement("button");
  b.className = "swatch";
  b.style.background = c;
  b.setAttribute("role", "radio");
  b.setAttribute("aria-checked", String(i === 0));
  b.title = c;
  b.addEventListener("click", () => {
    color = c;
    swatchBox.querySelectorAll(".swatch").forEach((s) => s.setAttribute("aria-checked", "false"));
    b.setAttribute("aria-checked", "true");
  });
  swatchBox.appendChild(b);
});

$("stroke").addEventListener("input", (e) => { stroke = Number(e.target.value); });

$("btn-undo").addEventListener("click", undo);
$("btn-redo").addEventListener("click", redo);

// ---- output --------------------------------------------------------------------

function flatten() {
  return canvas.toDataURL("image/png");
}

async function save() {
  try {
    await invoke("save_image", { path: imagePath, dataUrl: flatten() });
    dirty = false;
    toast(`Saved ${basename(imagePath)}`);
  } catch (e) {
    toast(String(e), "error");
  }
}

async function saveAs() {
  try {
    const path = await invoke("save_image_as", { suggested: imagePath, dataUrl: flatten() });
    if (!path) return; // cancelled
    imagePath = path;
    dirty = false;
    $("ed-name").textContent = basename(path);
    toast(`Saved ${basename(path)}`);
  } catch (e) {
    toast(String(e), "error");
  }
}

async function copy() {
  try {
    await invoke("copy_image", { dataUrl: flatten() });
    toast("Copied to clipboard");
  } catch (e) {
    toast(String(e), "error");
  }
}

$("btn-save").addEventListener("click", save);
$("btn-saveas").addEventListener("click", saveAs);
$("btn-copy").addEventListener("click", copy);

// ---- keyboard --------------------------------------------------------------------

const TOOL_KEYS = { a: "arrow", b: "box", e: "ellipse", l: "line", h: "highlight", x: "blur", t: "text", s: "step" };

window.addEventListener("keydown", (e) => {
  const mod = e.metaKey || e.ctrlKey;
  if (mod && e.key.toLowerCase() === "z") {
    e.preventDefault();
    e.shiftKey ? redo() : undo();
    return;
  }
  if (mod && e.key.toLowerCase() === "s") {
    e.preventDefault();
    e.shiftKey ? saveAs() : save();
    return;
  }
  if (mod && e.key.toLowerCase() === "c") { e.preventDefault(); copy(); return; }
  if (mod) return;

  if (e.key === "Escape") {
    // Abandon whatever is half-drawn.
    start = null; draft = null; render();
    return;
  }
  const t = TOOL_KEYS[e.key.toLowerCase()];
  if (t) setTool(t);
});

// ---- load ----------------------------------------------------------------------

function basename(p) {
  return String(p).split(/[\\/]/).pop();
}

async function load(path) {
  imagePath = path;
  let dataUrl;
  try {
    dataUrl = await invoke("load_image", { path });
  } catch (e) {
    toast(String(e), "error");
    return;
  }
  await new Promise((resolve, reject) => {
    const el = new Image();
    el.onload = () => { img = el; resolve(); };
    el.onerror = () => reject(new Error("image failed to decode"));
    el.src = dataUrl;
  }).catch((e) => toast(String(e), "error"));
  if (!img) return;

  canvas.width = img.naturalWidth;
  canvas.height = img.naturalHeight;
  shapes = [];
  redoStack = [];
  stepNext = 1;
  dirty = false;
  $("ed-name").textContent = basename(path);
  $("ed-dims").textContent = `${img.naturalWidth} × ${img.naturalHeight}`;
  render();
}

// Re-targeting an already-open editor at a fresh capture. Loading replaces the
// canvas outright, so unsaved marks would go with it — refuse and make the
// discard explicit rather than quietly throwing away work.
listen("editor-load", ({ payload }) => {
  if (!payload?.path) return;
  if (dirty && shapes.length) {
    toast(
      `${shapes.length} unsaved mark${shapes.length === 1 ? "" : "s"} on ${basename(imagePath)}.`,
      "error",
      {
        label: "Discard & load new",
        onClick: () => {
          dirty = false;
          load(payload.path);
        },
      }
    );
    return;
  }
  load(payload.path);
});

const params = new URLSearchParams(window.location.search);
const initial = params.get("path");
if (initial) load(initial);
else toast("No image to edit.", "error");

setTool("arrow");
window.addEventListener("resize", () => {
  if (!textInput.hidden) commitText(); // its position is scale-dependent
});
