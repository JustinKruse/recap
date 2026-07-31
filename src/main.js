// Recap — main window logic (vanilla JS, global Tauri API)

const tauri = window.__TAURI__;
if (!tauri) {
  document.body.innerHTML =
    '<p style="padding:20px;font-family:system-ui">Open this through the Tauri app (cargo tauri dev), not a browser.</p>';
  throw new Error("Tauri API not available");
}
const invoke = tauri.core.invoke;
const { listen } = tauri.event;

// ---- element handles -------------------------------------------------------

const $ = (id) => document.getElementById(id);
const els = {
  statusPill: $("status-pill"),
  banner: $("ffmpeg-banner"),
  modeFullscreen: $("mode-fullscreen"),
  modeRegion: $("mode-region"),
  regionRow: $("region-row"),
  regionLabel: $("region-label"),
  selectRegion: $("btn-select-region"),
  monitor: $("sel-monitor"),
  fps: $("sel-fps"),
  encoder: $("sel-encoder"),
  cursor: $("chk-cursor"),
  mic: $("chk-mic"),
  micDev: $("sel-mic"),
  outDir: $("out-dir"),
  outDirBtn: $("btn-out-dir"),
  snap: $("btn-snap"),
  text: $("btn-text"),
  textSheet: $("text-sheet"),
  textBody: $("text-body"),
  textMeta: $("text-meta"),
  textCopy: $("text-copy"),
  textClose: $("text-close"),
  record: $("btn-record"),
  pause: $("btn-pause"),
  stop: $("btn-stop"),
  timer: $("timer"),
  countdown: $("countdown"),
  countdownNum: $("countdown-num"),
  toast: $("toast"),
  toastMsg: $("toast-msg"),
  toastAction: $("toast-action"),
};

const ENCODER_LABELS = {
  auto: "Auto (best available)",
  h264_videotoolbox: "Apple VideoToolbox",
  h264_nvenc: "NVIDIA NVENC",
  h264_amf: "AMD AMF",
  h264_qsv: "Intel QuickSync",
  libx264: "Software (x264)",
};

// ---- state -----------------------------------------------------------------

let mode = "fullscreen";
let regionSet = false;
let outputDir = "";
let uiState = "idle"; // idle | countdown | recording | paused | finalizing

// timer
let accumulatedMs = 0;
let resumedAt = null;
let timerInterval = null;

function setState(s) {
  uiState = s;
  document.body.dataset.state = s;
  const labels = {
    idle: "Ready",
    countdown: "Starting…",
    recording: "Recording",
    paused: "Paused",
    finalizing: "Saving…",
  };
  els.statusPill.textContent = labels[s] ?? s;
  const busy = s === "recording" || s === "paused";
  els.pause.disabled = !busy;
  els.stop.disabled = !(busy || s === "countdown");
  els.record.disabled = s === "finalizing";
}

// ---- timer -------------------------------------------------------------------

function renderTimer() {
  let ms = accumulatedMs;
  if (resumedAt !== null) ms += Date.now() - resumedAt;
  const total = Math.floor(ms / 1000);
  const mm = String(Math.floor(total / 60)).padStart(2, "0");
  const ss = String(total % 60).padStart(2, "0");
  els.timer.textContent = `${mm}:${ss}`;
}

function timerStart() {
  accumulatedMs = 0;
  resumedAt = Date.now();
  clearInterval(timerInterval);
  timerInterval = setInterval(renderTimer, 250);
  renderTimer();
}
function timerPause() {
  if (resumedAt !== null) accumulatedMs += Date.now() - resumedAt;
  resumedAt = null;
  renderTimer();
}
function timerResume() {
  resumedAt = Date.now();
}
function timerReset() {
  clearInterval(timerInterval);
  timerInterval = null;
  accumulatedMs = 0;
  resumedAt = null;
  renderTimer();
}

// ---- toast -------------------------------------------------------------------

let toastTimeout = null;
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
  clearTimeout(toastTimeout);
  toastTimeout = setTimeout(() => (els.toast.hidden = true), kind === "error" ? 12000 : 7000);
}

// ---- config ------------------------------------------------------------------

function currentConfig() {
  return {
    mode,
    monitorIndex: Number(els.monitor.value || 0),
    fps: Number(els.fps.value),
    encoder: els.encoder.value,
    captureCursor: els.cursor.checked,
    micEnabled: els.mic.checked,
    micDevice: els.micDev.value || null,
    outputDir,
  };
}

function syncConfig() {
  invoke("sync_config", { cfg: currentConfig() }).catch(() => {});
}

// ---- init ---------------------------------------------------------------------

async function init() {
  renderTimer();
  let info;
  try {
    info = await invoke("init_info");
  } catch (e) {
    toast(`Startup failed: ${e}`, "error");
    return;
  }

  if (!info.ffmpegPath) {
    els.banner.hidden = false;
  }

  els.monitor.innerHTML = "";
  info.monitors.forEach((m) => {
    const opt = document.createElement("option");
    opt.value = String(m.index);
    opt.textContent = `${m.name.replace(/^\\\\.\\/, "")} — ${m.width}×${m.height}`;
    els.monitor.appendChild(opt);
  });

  els.encoder.innerHTML = "";
  ["auto", ...info.encoders].forEach((enc) => {
    const opt = document.createElement("option");
    opt.value = enc;
    opt.textContent = ENCODER_LABELS[enc] ?? enc;
    els.encoder.appendChild(opt);
  });

  els.micDev.innerHTML = "";
  if (info.audioDevices.length === 0) {
    const opt = document.createElement("option");
    opt.value = "";
    opt.textContent = "No devices found";
    els.micDev.appendChild(opt);
    els.mic.disabled = true;
  } else {
    // `id` is an opaque backend token (a DirectShow name on Windows, an
    // AVFoundation index on macOS) — display the label, send back the id.
    info.audioDevices.forEach((d) => {
      const opt = document.createElement("option");
      opt.value = d.id;
      opt.textContent = d.label;
      els.micDev.appendChild(opt);
    });
  }

  // Mirror last run's settings onto the controls. Rust has already validated
  // them (dead output folder, unplugged display), so this is what's active —
  // showing anything else would be a lie about what the next capture will do.
  const c = info.config ?? {};
  outputDir = c.outputDir || info.defaultOutputDir;
  els.outDir.textContent = outputDir;
  els.outDir.title = outputDir;
  if (c.fps) els.fps.value = String(c.fps);
  if (c.encoder && [...els.encoder.options].some((o) => o.value === c.encoder)) {
    els.encoder.value = c.encoder;
  }
  if (typeof c.captureCursor === "boolean") els.cursor.checked = c.captureCursor;
  if (typeof c.micEnabled === "boolean") els.mic.checked = c.micEnabled;
  els.micDev.disabled = !els.mic.checked;
  if (c.micDevice && [...els.micDev.options].some((o) => o.value === c.micDevice)) {
    els.micDev.value = c.micDevice;
  }
  if (Number.isInteger(c.monitorIndex) && els.monitor.options[c.monitorIndex]) {
    els.monitor.value = String(c.monitorIndex);
  }
  // Region mode is deliberately not restored: the region itself isn't, so
  // restoring the mode would leave the UI demanding a selection on every launch.

  syncConfig();
}

// ---- wiring --------------------------------------------------------------------

function setMode(next) {
  mode = next;
  els.modeFullscreen.classList.toggle("is-active", mode === "fullscreen");
  els.modeRegion.classList.toggle("is-active", mode === "region");
  els.regionRow.hidden = mode !== "region";
  syncConfig();
}

els.modeFullscreen.addEventListener("click", () => setMode("fullscreen"));
els.modeRegion.addEventListener("click", () => setMode("region"));

els.selectRegion.addEventListener("click", () => {
  invoke("open_region_overlay", { monitorIndex: Number(els.monitor.value || 0) }).catch((e) =>
    toast(String(e), "error")
  );
});

for (const el of [els.monitor, els.fps, els.encoder, els.cursor, els.micDev]) {
  el.addEventListener("change", syncConfig);
}
els.monitor.addEventListener("change", () => {
  // A region belongs to the display it was drawn on.
  regionSet = false;
  els.regionLabel.textContent = "No region selected";
  syncConfig();
});
els.mic.addEventListener("change", () => {
  els.micDev.disabled = !els.mic.checked;
  syncConfig();
});

els.outDirBtn.addEventListener("click", async () => {
  const dir = await invoke("pick_output_dir").catch(() => null);
  if (dir) {
    outputDir = dir;
    els.outDir.textContent = dir;
    els.outDir.title = dir;
    syncConfig();
  }
});

els.snap.addEventListener("click", async () => {
  if (mode === "region" && !regionSet) {
    toast("Select a region first.", "error");
    return;
  }
  // Get out of our own shot. Rust waits ~220 ms after this for the compositor
  // to actually repaint the area the window was covering.
  const win = tauri.window.getCurrentWindow();
  els.snap.disabled = true;
  await win.hide().catch(() => {});
  try {
    const path = await invoke("capture_still", { cfg: currentConfig() });
    const name = path.split(/[\\/]/).pop();
    // Capture straight into the editor — that's the Snagit loop.
    await invoke("open_editor", { path }).catch(() => {});
    toast(`Saved ${name}`, "ok", {
      label: "Open folder",
      onClick: () => invoke("reveal_path", { path }).catch(() => {}),
    });
  } catch (e) {
    toast(String(e), "error");
  } finally {
    els.snap.disabled = false;
    await win.show().catch(() => {});
    await win.setFocus().catch(() => {});
  }
});

// ---- text grab ---------------------------------------------------------------

function showGrabbedText(result) {
  const lines = result?.lines?.length ?? 0;
  if (!lines) {
    toast("No text found in that area.", "error");
    return;
  }
  // Vision reports a 0..1 score per line; the weakest one is what to distrust.
  const worst = Math.min(...result.lines.map((l) => l.confidence));
  els.textBody.value = result.text;
  els.textMeta.textContent = `${lines} line${lines === 1 ? "" : "s"} · ${Math.round(worst * 100)}% min confidence`;
  els.textSheet.hidden = false;
  els.textBody.focus();
  els.textBody.setSelectionRange(0, 0);
}

els.text.addEventListener("click", async () => {
  if (mode === "region" && !regionSet) {
    toast("Select a region first.", "error");
    return;
  }
  const win = tauri.window.getCurrentWindow();
  els.text.disabled = true;
  await win.hide().catch(() => {});
  try {
    showGrabbedText(await invoke("grab_text", { cfg: currentConfig() }));
  } catch (e) {
    toast(String(e), "error");
  } finally {
    els.text.disabled = false;
    await win.show().catch(() => {});
    await win.setFocus().catch(() => {});
  }
});

els.textClose.addEventListener("click", () => (els.textSheet.hidden = true));
els.textCopy.addEventListener("click", async () => {
  try {
    await tauri.clipboardManager.writeText(els.textBody.value);
    toast("Copied");
  } catch {
    // Fall back to the browser clipboard if the plugin isn't reachable.
    try { await navigator.clipboard.writeText(els.textBody.value); toast("Copied"); }
    catch (e) { toast(String(e), "error"); }
  }
});
window.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !els.textSheet.hidden) els.textSheet.hidden = true;
});

listen("text-grabbed", ({ payload }) => showGrabbedText(payload));

els.record.addEventListener("click", () => {
  if (uiState === "idle") {
    if (mode === "region" && !regionSet) {
      toast("Select a region first.", "error");
      return;
    }
    invoke("start_recording", { cfg: currentConfig() }).catch((e) => toast(String(e), "error"));
  } else {
    invoke("stop_recording").catch((e) => toast(String(e), "error"));
  }
});
els.stop.addEventListener("click", () => invoke("stop_recording").catch(() => {}));
els.pause.addEventListener("click", () => invoke("toggle_pause").catch((e) => toast(String(e), "error")));

// ---- events from rust ------------------------------------------------------------

listen("countdown", ({ payload }) => {
  const n = Number(payload);
  if (n > 0) {
    setState("countdown");
    els.countdown.hidden = false;
    els.countdownNum.textContent = String(n);
  } else {
    els.countdown.hidden = true;
  }
});

listen("recording-started", () => {
  setState("recording");
  timerStart();
});

listen("recording-paused", () => {
  setState("paused");
  timerPause();
});

listen("recording-resumed", () => {
  setState("recording");
  timerResume();
});

listen("recording-stopped", ({ payload }) => {
  setState("idle");
  timerReset();
  const path = payload?.path ?? "";
  const name = path.split(/[\\/]/).pop();
  // GIF is the action worth one click here — revealing the folder is a
  // right-click away in any file manager, converting a video isn't.
  toast(`Saved ${name}`, "ok", {
    label: "Make GIF",
    onClick: async () => {
      toast("Building GIF…", "ok");
      try {
        const gif = await invoke("export_gif", { path, fps: 12, width: 900 });
        toast(`Saved ${gif.split(/[\\/]/).pop()}`, "ok", {
          label: "Open folder",
          onClick: () => invoke("reveal_path", { path: gif }).catch(() => {}),
        });
      } catch (e) {
        toast(String(e), "error");
      }
    },
  });
});

listen("recording-cancelled", () => {
  els.countdown.hidden = true;
  setState("idle");
  timerReset();
});

listen("recording-error", ({ payload }) => {
  els.countdown.hidden = true;
  setState("idle");
  timerReset();
  const msg = payload?.message ?? "Recording failed.";
  const log = payload?.log ? ` — ${String(payload.log).split("\n").slice(-2).join(" ")}` : "";
  toast(`${msg}${log}`, "error");
});

// Ctrl+Alt+S fires in Rust, so the toast has to come back over an event.
listen("still-captured", ({ payload }) => {
  const path = payload?.path ?? "";
  const name = path.split(/[\\/]/).pop();
  toast(`Saved ${name}`, "ok", {
    label: "Open folder",
    onClick: () => invoke("reveal_path", { path }).catch(() => {}),
  });
});

listen("region-set", ({ payload }) => {
  regionSet = true;
  setMode("region");
  els.regionLabel.textContent = `${payload.width}×${payload.height} px`;
});

// keep UI honest if state changed from tray/hotkeys while window was hidden
listen("status", ({ payload }) => {
  const s = payload?.status;
  if (s === "finalizing") setState("finalizing");
  if (s === "idle" && uiState === "finalizing") setState("idle");
});

init();
