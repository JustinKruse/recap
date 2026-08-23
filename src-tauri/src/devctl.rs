//! Debug-only control channel.
//!
//! A line-oriented JSON socket on loopback that lets a developer (or an agent)
//! drive and inspect a running Recap: read its state, invoke its commands,
//! evaluate JavaScript inside any window, and ask where a window is on screen
//! so a screenshot can be cropped to it.
//!
//! **This is a remote-code-execution hole by design** — `eval` runs arbitrary
//! JS in a privileged webview. It is compiled only under `debug_assertions`,
//! bound to 127.0.0.1, and must never exist in a release build. `lib.rs` gates
//! the whole module on `#[cfg(debug_assertions)]`; keep it that way.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Listener, Manager};

use crate::recorder::RecorderHandle;
use crate::recorder::{self, lock_recovering};

const ADDR: &str = "127.0.0.1:7333";

/// How long to wait for a webview to answer an `eval` before giving up.
///
/// Generous because WKWebView throttles a window that is hidden or fully
/// occluded: the script runs, but the reply can be deferred for many seconds.
/// A wedged webview still must not wedge the socket thread forever.
const EVAL_TIMEOUT: Duration = Duration::from_secs(25);

type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>;

static PENDING: OnceLock<Pending> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Keep macOS from throttling us while an agent is driving the app.
///
/// A window that is fully covered by another app counts as occluded, and App
/// Nap then throttles the webview's timers — so an `eval` runs but its reply
/// can be deferred for tens of seconds. That made this socket flaky in exactly
/// the situation it exists for: a terminal in front, Recap behind. The activity
/// assertion is held for the life of the process and never taken in release,
/// where this module doesn't exist at all.
#[cfg(target_os = "macos")]
fn disable_app_nap() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiated,
        &NSString::from_str("recap devctl is driving the UI"),
    );
    // The activity ends when this token is released, and it should last as long
    // as the process does. The token isn't Send, so it can't live in a static —
    // leaking the retain is the honest way to say "hold this forever".
    std::mem::forget(token);
}

#[cfg(not(target_os = "macos"))]
fn disable_app_nap() {}

pub fn start(app: &AppHandle) {
    disable_app_nap();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    if PENDING.set(pending.clone()).is_err() {
        return; // already started
    }

    // JS hands results back over the normal event bus, keyed by request id.
    app.listen_any("devctl-result", move |event| {
        let Ok(v) = serde_json::from_str::<Value>(event.payload()) else {
            return;
        };
        let Some(id) = v.get("id").and_then(Value::as_u64) else {
            return;
        };
        if let Some(tx) = lock_recovering(&pending).remove(&id) {
            let _ = tx.send(v.get("value").cloned().unwrap_or(Value::Null));
        }
    });

    let app = app.clone();
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(ADDR) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("devctl: not listening ({e})");
                return;
            }
        };
        eprintln!("devctl: listening on {ADDR}");
        for stream in listener.incoming().flatten() {
            let app = app.clone();
            std::thread::spawn(move || serve(&app, stream));
        }
    });
}

/// One request per connection: read a line of JSON, write a line of JSON back.
fn serve(app: &AppHandle, stream: TcpStream) {
    let mut line = String::new();
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let response = match serde_json::from_str::<Value>(&line) {
        Ok(req) => dispatch(app, &req),
        Err(e) => json!({ "error": format!("bad JSON: {e}") }),
    };
    let mut out = stream;
    let _ = writeln!(out, "{response}");
    let _ = out.flush();
}

fn dispatch(app: &AppHandle, req: &Value) -> Value {
    let cmd = req.get("cmd").and_then(Value::as_str).unwrap_or("");
    let window = req.get("window").and_then(Value::as_str).unwrap_or("main");
    match cmd {
        "ping" => json!({ "ok": true }),
        "state" => state(app),
        "windows" => json!({
            "windows": app.webview_windows().keys().cloned().collect::<Vec<_>>()
        }),
        "bounds" => bounds(app, window),
        "eval" => match req.get("js").and_then(Value::as_str) {
            Some(js) => eval(app, window, js),
            None => json!({ "error": "eval needs a `js` string" }),
        },
        "invoke" => invoke(app, req.get("name").and_then(Value::as_str).unwrap_or("")),
        other => json!({ "error": format!("unknown cmd `{other}`") }),
    }
}

fn state(app: &AppHandle) -> Value {
    let handle = app.state::<RecorderHandle>();
    let r = handle.lock();
    json!({
        "status": r.status.as_str(),
        "config": r.config,
        "region": r.region.map(|x| json!({
            "monitorIndex": x.monitor_index, "x": x.x, "y": x.y,
            "width": x.width, "height": x.height
        })),
        "screens": r.screens,
        "encoders": r.encoders,
        "ffmpeg": r.ffmpeg_path.as_ref().map(|p| p.display().to_string()),
        "backend": crate::capture::active().name(),
    })
}

/// Physical-pixel outer bounds, which is the same space `screencapture` works
/// in on this display — so a caller can grab the screen and crop straight to it.
fn bounds(app: &AppHandle, label: &str) -> Value {
    let Some(w) = app.get_webview_window(label) else {
        return json!({ "error": format!("no window `{label}`") });
    };
    let pos = w.outer_position().ok();
    let size = w.outer_size().ok();
    json!({
        "label": label,
        "visible": w.is_visible().unwrap_or(false),
        "scale": w.scale_factor().unwrap_or(1.0),
        "x": pos.map(|p| p.x), "y": pos.map(|p| p.y),
        "width": size.map(|s| s.width), "height": size.map(|s| s.height),
    })
}

fn eval(app: &AppHandle, label: &str, js: &str) -> Value {
    let Some(w) = app.get_webview_window(label) else {
        return json!({ "error": format!("no window `{label}`") });
    };
    let Some(pending) = PENDING.get() else {
        return json!({ "error": "devctl not started" });
    };

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    lock_recovering(pending).insert(id, tx);

    // The source is passed as a *string* and compiled with `new Function`, so
    // that a syntax error is a catchable exception rather than a script that
    // fails to parse — which would run nothing, emit nothing, and leave the
    // caller waiting out the timeout with no idea why.
    //
    // Expression first (`return (src)`) so `shapes.length` yields a value; if
    // that doesn't parse, the source is compiled as a statement body, so
    // `const x = 1; return x` works too.
    let src = serde_json::to_string(js).unwrap_or_else(|_| "\"\"".into());
    let script = format!(
        r#"(async () => {{
  let out;
  try {{
    const src = {src};
    // AsyncFunction rather than Function: statement bodies routinely want
    // `await`, which a plain Function rejects as a syntax error.
    const AsyncFunction = Object.getPrototypeOf(async function () {{}}).constructor;
    let fn;
    try {{ fn = new AsyncFunction("return (" + src + "\n)"); }}
    catch (_) {{ fn = new AsyncFunction(src); }}
    const v = await fn();
    out = {{ ok: v === undefined ? null : JSON.parse(JSON.stringify(v)) }};
  }} catch (e) {{ out = {{ error: String(e) }}; }}
  window.__TAURI__.event.emit('devctl-result', {{ id: {id}, value: out }});
}})()"#
    );

    if let Err(e) = w.eval(&script) {
        lock_recovering(pending).remove(&id);
        return json!({ "error": format!("eval failed: {e}") });
    }
    match rx.recv_timeout(EVAL_TIMEOUT) {
        Ok(v) => v,
        Err(_) => {
            lock_recovering(pending).remove(&id);
            json!({ "error": "timed out waiting for the webview" })
        }
    }
}

fn invoke(app: &AppHandle, name: &str) -> Value {
    let result: Result<Value, String> = match name {
        "still" => crate::capture_still_flow(app).map(|path| json!({ "path": path })),
        "grab_text" => crate::read_screen_text(app).map(|r| json!(r)),
        "record_start" => recorder::start(app).map(|_| json!({ "started": true })),
        "record_stop" => recorder::stop(app).map(|_| json!({ "stopping": true })),
        "pause" => recorder::toggle_pause(app).map(|_| json!({ "toggled": true })),
        other => Err(format!("unknown invoke `{other}`")),
    };
    match result {
        Ok(v) => json!({ "ok": v }),
        Err(e) => json!({ "error": e }),
    }
}
