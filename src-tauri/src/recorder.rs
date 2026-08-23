//! Recording state machine.
//!
//!   Idle -> Countdown -> Recording <-> Paused -> Finalizing -> Idle
//!
//! Pause is implemented as "stop this ffmpeg, start a fresh segment on
//! resume", then all segments are losslessly concatenated on stop. ffmpeg is
//! always stopped by writing `q` to its stdin (a hard kill truncates the MP4
//! moov atom); kill is only the 4-second timeout fallback.

use crate::capture::{self, CaptureTarget, ScreenDevice};
use crate::ffmpeg;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Emitter, Manager};

// ---------------------------------------------------------------------------
// Types

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct RecordingConfig {
    pub mode: String, // "fullscreen" | "region"
    pub monitor_index: usize,
    pub fps: u32,
    pub encoder: String, // "auto" | "h264_nvenc" | "h264_amf" | "h264_qsv" | "libx264"
    pub capture_cursor: bool,
    pub mic_enabled: bool,
    pub mic_device: Option<String>,
    pub output_dir: String,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            mode: "fullscreen".into(),
            monitor_index: 0,
            fps: 30,
            encoder: "auto".into(),
            capture_cursor: true,
            mic_enabled: false,
            mic_device: None,
            output_dir: String::new(),
        }
    }
}

/// Payload the region overlay emits (physical pixels, monitor-relative).
#[derive(Clone, Copy, Deserialize, Debug)]
pub struct RegionSel {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub monitor_index: usize,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Idle,
    Countdown,
    Recording,
    Paused,
    Finalizing,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Idle => "idle",
            Status::Countdown => "countdown",
            Status::Recording => "recording",
            Status::Paused => "paused",
            Status::Finalizing => "finalizing",
        }
    }
}

pub struct Recorder {
    pub status: Status,
    pub config: RecordingConfig,
    pub region: Option<Region>,
    /// Which monitor the currently-open overlay covers.
    pub pending_overlay_monitor: usize,
    pub ffmpeg_path: Option<PathBuf>,
    /// Encoders that passed the runtime probe at startup.
    pub encoders: Vec<String>,
    /// Screens as the capture backend enumerates them. Index is the UI's
    /// monitor index; `.id` is the backend-native identifier. Empty when the
    /// backend can't enumerate (Windows), where the two coincide.
    pub screens: Vec<ScreenDevice>,
    child: Option<Child>,
    segments: Vec<PathBuf>,
    session_dir: Option<PathBuf>,
    /// Bumped on every start/cancel so stale countdown threads can bail.
    session_id: u64,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl Recorder {
    /// Adopt a config from the UI, dropping any region it invalidates.
    ///
    /// A region belongs to the display it was drawn on, so switching display
    /// must discard it. The UI knows this, but only the UI enforced it — a
    /// global hotkey goes straight to the recorder and would happily record a
    /// stale region against the wrong screen.
    pub fn apply_config(&mut self, cfg: RecordingConfig) {
        let keep = cfg.mode == "region"
            && self
                .region
                .is_some_and(|r| r.monitor_index == cfg.monitor_index);
        if !keep {
            self.region = None;
        }
        self.config = cfg;
    }

    /// Claim this session for an abort, or refuse because someone else already
    /// owns its ending.
    ///
    /// Must be called in the *same* lock acquisition as the teardown it
    /// authorises, which is the whole point of it existing. The stall watchdog
    /// used to validate the session under one lock, drop it, and then call a
    /// `fail()` that took the lock again to clear `segments` and delete the
    /// session folder. A Stop landing in that gap set `Finalizing` and handed
    /// `finalize()` to another thread, which then read an already-emptied
    /// segment list and told the user "No video was captured" about a
    /// recording that was fine — after deleting it. Milliseconds wide, and only
    /// reachable when the watchdog was about to fire anyway, but real.
    fn claim_failure(
        &mut self,
        session_id: u64,
        expect_last: Option<&Path>,
    ) -> Option<(Option<Child>, Option<PathBuf>)> {
        if self.session_id != session_id {
            return None;
        }
        // Countdown: the first segment never spawned. Recording: the watchdog
        // saw no frames. Every other state — Paused, Finalizing, Idle — has
        // already ended this session or is in the middle of ending it, and
        // zero frames there is expected rather than a fault.
        if !matches!(self.status, Status::Countdown | Status::Recording) {
            return None;
        }
        // Resume rolls a new segment without bumping session_id, so a watchdog
        // armed for an earlier segment must not shoot down a later one.
        if let Some(p) = expect_last {
            if self.segments.last().map(PathBuf::as_path) != Some(p) {
                return None;
            }
        }
        self.status = Status::Idle;
        self.session_id += 1;
        self.segments.clear();
        Some((self.child.take(), self.session_dir.take()))
    }
}

pub struct RecorderHandle(pub Arc<Mutex<Recorder>>);

/// Take a mutex, treating poisoning as noise rather than as an error.
///
/// The policy for every lock in the recorder, deliberately, and the reason is
/// the failure mode: `lock().unwrap()` turns one panic anywhere in the app into
/// a permanently dead one. Every later acquisition panics too, so Stop, the
/// tray and the hotkeys all die at once — mid-recording, with an ffmpeg child
/// still writing to the user's disk and no way left to tell it to finish.
///
/// That trade only makes sense if the data behind the lock can be left in a
/// state worth refusing to read. This data cannot: a status enum, a config
/// struct, a `Child`, a list of segment paths. There is no multi-field
/// invariant a half-finished mutation can break that the next start/stop does
/// not simply overwrite, and every reader already re-checks `session_id` and
/// `status` on the assumption it may be looking at something stale. Carrying on
/// with possibly-stale state is strictly better than bricking the app.
pub(crate) fn lock_recovering<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl RecorderHandle {
    /// The sanctioned way to reach recorder state. See [`lock_recovering`].
    pub fn lock(&self) -> MutexGuard<'_, Recorder> {
        lock_recovering(&self.0)
    }

    pub fn new() -> Self {
        RecorderHandle(Arc::new(Mutex::new(Recorder {
            status: Status::Idle,
            config: RecordingConfig::default(),
            region: None,
            pending_overlay_monitor: 0,
            ffmpeg_path: None,
            encoders: vec!["libx264".into()],
            screens: Vec::new(),
            child: None,
            segments: Vec::new(),
            session_dir: None,
            session_id: 0,
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
        })))
    }
}

// ---------------------------------------------------------------------------
// Event helpers

fn emit_status(app: &AppHandle, status: Status) {
    let _ = app.emit("status", serde_json::json!({ "status": status.as_str() }));
}

fn stderr_tail_string(handle: &RecorderHandle) -> String {
    let tail = handle.lock().stderr_tail.clone();
    let t = lock_recovering(&tail);
    t.iter()
        .rev()
        .take(15)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Abort `session_id` and tell the user why — unless it is no longer the live
/// session, or has already left the states an abort is allowed from. See
/// [`Recorder::claim_failure`]; the guard is inside the teardown's own lock
/// acquisition on purpose.
fn fail(app: &AppHandle, session_id: u64, expect_last: Option<&Path>, message: String) {
    let handle = app.state::<RecorderHandle>();
    let claimed = handle.lock().claim_failure(session_id, expect_last);
    let Some((child, session_dir)) = claimed else {
        return; // a stop, pause or cancel got there first
    };
    if let Some(mut c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
    if let Some(d) = session_dir {
        let _ = std::fs::remove_dir_all(d);
    }
    let log = stderr_tail_string(&handle);
    let _ = app.emit(
        "recording-error",
        serde_json::json!({ "message": message, "log": log }),
    );
    emit_status(app, Status::Idle);
}

// ---------------------------------------------------------------------------
// Region

pub fn set_region(app: &AppHandle, sel: RegionSel) {
    let handle = app.state::<RecorderHandle>();
    let (monitor_index, width, height) = {
        let mut r = handle.lock();
        let (x, y, w, h) = capture::sanitize_region(sel.x, sel.y, sel.width, sel.height);
        let monitor_index = r.pending_overlay_monitor;
        r.region = Some(Region {
            monitor_index,
            x,
            y,
            width: w,
            height: h,
        });
        r.config.mode = "region".into();
        (monitor_index, w, h)
    };
    close_overlay(app);
    let _ = app.emit(
        "region-set",
        serde_json::json!({
            "width": width,
            "height": height,
            "monitorIndex": monitor_index
        }),
    );
}

pub fn close_overlay(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("overlay") {
        let _ = w.close();
    }
}

// ---------------------------------------------------------------------------
// Disk space
//
// Free space is read by shelling out rather than through a crate. The
// dependency-free alternatives are `statvfs` (needs `libc`) and
// `GetDiskFreeSpaceEx` (needs `windows-sys`), and neither earns a place in the
// manifest for one number, read once per start and once every 15s after. `df`
// and PowerShell are both guaranteed present on their platform, and `df -P`
// pins a single-line, locale-independent column layout that has been stable
// since POSIX.2. Cost is a few milliseconds off the recorder's lock.

/// Refuse to start below this. Screen h264 here runs roughly 1 MB/s at 1080p30
/// and several times that on a Retina display, so a gigabyte is minutes, not
/// hours — it is a floor for "this will fail almost immediately", not a budget.
const MIN_FREE_TO_START: u64 = 1024 * 1024 * 1024;

/// Stop ourselves below this, mid-recording.
const MIN_FREE_TO_CONTINUE: u64 = 256 * 1024 * 1024;

const DISK_POLL: Duration = Duration::from_secs(15);

fn human_bytes(n: u64) -> String {
    const GB: u64 = 1024 * 1024 * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else {
        format!("{} MB", n / (1024 * 1024))
    }
}

/// Free bytes on the filesystem holding `path`, or `None` when we cannot tell.
#[cfg(unix)]
pub(crate) fn free_space_bytes(path: &Path) -> Option<u64> {
    let out = ffmpeg::quiet_command(Path::new("df"))
        .args(["-Pk", &path.display().to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_df_kb(&String::from_utf8_lossy(&out.stdout)).map(|kb| kb.saturating_mul(1024))
}

/// Free bytes from `df -P` output.
///
/// Anchored on the capacity column (the one ending in `%`) instead of counting
/// fields from the left: a device named `map auto_home` or a mount point under
/// `/Volumes/My Disk` puts a space in a field and shifts every index after it.
/// Capacity is always the field directly after available, and it is the only
/// one that ends in a percent sign.
#[cfg(unix)]
fn parse_df_kb(stdout: &str) -> Option<u64> {
    for line in stdout.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let Some(cap) = cols.iter().position(|c| c.ends_with('%')) else {
            continue;
        };
        if cap == 0 {
            continue;
        }
        if let Ok(kb) = cols[cap - 1].parse::<u64>() {
            return Some(kb);
        }
    }
    None
}

/// Free bytes on the volume holding `path`. `None` for UNC paths, which have no
/// PSDrive — the caller treats that as "unknown", not as "full".
#[cfg(windows)]
pub(crate) fn free_space_bytes(path: &Path) -> Option<u64> {
    // PowerShell rather than `wmic`, which Windows 11 no longer ships.
    let script = format!(
        "(Get-Item -LiteralPath '{}').PSDrive.Free",
        path.display().to_string().replace('\'', "''")
    );
    let out = ffmpeg::quiet_command(Path::new("powershell"))
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

/// Gate a start on free space. An unknown answer passes: refusing to record
/// because `df` could not be run would be a worse bug than the one this fixes.
fn space_check(free: Option<u64>, needed: u64, dir: &str) -> Result<(), String> {
    match free {
        Some(free) if free < needed => Err(format!(
            "Not enough free space in {dir} — {} left, {} needed to start. \
             Free some space or choose another folder.",
            human_bytes(free),
            human_bytes(needed)
        )),
        _ => Ok(()),
    }
}

/// A disk that fills mid-recording is silent: ffmpeg's writes fail, it keeps
/// going, and the file it leaves has no moov atom, so the entire recording is
/// unplayable. Stopping ourselves while there is still room to write the
/// trailer turns "you lost an hour" into "your hour ends here".
fn spawn_disk_watchdog(app: &AppHandle, session_id: u64, output_dir: PathBuf) {
    let app = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(DISK_POLL);
        {
            let handle = app.state::<RecorderHandle>();
            let r = handle.lock();
            if r.session_id != session_id || !matches!(r.status, Status::Recording | Status::Paused)
            {
                return; // session over; nothing left to protect
            }
        }
        let Some(free) = free_space_bytes(&output_dir) else {
            continue;
        };
        if free >= MIN_FREE_TO_CONTINUE {
            continue;
        }
        let _ = app.emit(
            "recording-error",
            serde_json::json!({
                "message": format!(
                    "Disk almost full ({} left) — stopping now so the recording stays playable.",
                    human_bytes(free)
                ),
                "log": ""
            }),
        );
        // stop() re-validates status under the lock, so losing a race with the
        // user's own Stop just means this call reports "Not recording".
        let _ = stop(&app);
        return;
    });
}

// ---------------------------------------------------------------------------
// Stale session folders

/// Prefix of a session's scratch folder inside the user's output directory.
/// Full shape: `.recap-tmp-<stamp>-<pid>`.
pub(crate) const SESSION_PREFIX: &str = ".recap-tmp-";

/// How long an unclaimed session folder — one whose name carries no pid — must
/// have sat untouched before we believe nobody is writing to it. Only folders
/// from a build older than the pid suffix land here.
const UNCLAIMED_MIN_AGE: Duration = Duration::from_secs(30 * 60);

/// Scratch folder name for a session started now by this process.
///
/// The pid is part of the name, not a file inside the folder, for two reasons:
/// the claim then exists from the instant the folder does, with no crash window
/// in between, and two instances starting in the same second can no longer
/// collide on one folder and interleave their segments.
fn session_dir_name(stamp: &str, pid: u32) -> String {
    format!("{SESSION_PREFIX}{stamp}-{pid}")
}

/// The pid out of a session folder name, if it has one. The timestamp always
/// contains `_` and never `-`, so the last dash-separated field is the pid or
/// nothing — a folder from an older build parses as unowned, which is exactly
/// what it is.
fn session_owner_pid(name: &str) -> Option<u32> {
    name.rsplit('-').next()?.parse().ok()
}

/// Delete session folders left behind by a crash or force-quit.
///
/// Nothing else ever removes these: the normal paths (`finalize`, `fail`,
/// cancel) all delete their own, so a folder that outlives its process stays in
/// the user's video folder forever, holding every segment of a recording that
/// was never finished.
///
/// Called at startup, before this process can create a session folder of its
/// own, so "not from the current session" needs no bookkeeping. A *second*
/// running instance does need it, and that is what the pid in the name is for.
/// Returns the number removed.
pub fn sweep_stale_sessions(output_dir: &Path) -> usize {
    sweep_stale_sessions_with(output_dir, SystemTime::now(), &process_is_alive)
}

fn sweep_stale_sessions_with(
    output_dir: &Path,
    now: SystemTime,
    alive: &dyn Fn(u32) -> bool,
) -> usize {
    let Ok(entries) = std::fs::read_dir(output_dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with(SESSION_PREFIX) || !path.is_dir() {
            continue;
        }
        let abandoned = match session_owner_pid(name) {
            // Someone still owns it. Keeping a folder we could have deleted
            // costs disk; deleting one still being recorded into costs the
            // user their recording. Every ambiguous case resolves this way.
            Some(pid) => !alive(pid),
            None => newest_mtime(&path)
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age >= UNCLAIMED_MIN_AGE),
        };
        if abandoned && std::fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Newest mtime of the folder or anything directly in it. The folder's own
/// mtime is not enough: it only moves when an entry is added or removed, so a
/// long single segment being written for an hour leaves it looking untouched.
fn newest_mtime(dir: &Path) -> Option<SystemTime> {
    let own = dir.metadata().and_then(|m| m.modified()).ok();
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| e.metadata().and_then(|m| m.modified()).ok())
        .chain(own)
        .max()
}

/// Does `pid` name a live process that looks like another copy of Recap?
///
/// Shelled out for the same reason as free space — `kill(pid, 0)` is a `libc`
/// dependency for one call, on a path that runs once at startup. Anything we
/// cannot answer counts as alive.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let Ok(out) = ffmpeg::quiet_command(Path::new("ps"))
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .stdin(Stdio::null())
        .output()
    else {
        return true; // could not ask
    };
    if !out.status.success() {
        return false; // ps exits non-zero when no such process
    }
    let comm = String::from_utf8_lossy(&out.stdout);
    let comm = comm.trim();
    if comm.is_empty() {
        return false;
    }
    match std::env::current_exe().ok().as_deref().and_then(exe_name) {
        Some(me) => comm_matches(comm, &me),
        None => true,
    }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    let Ok(out) = ffmpeg::quiet_command(Path::new("tasklist"))
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .stdin(Stdio::null())
        .output()
    else {
        return true;
    };
    // tasklist exits 0 and prints "INFO: No tasks are running..." for a miss.
    String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
}

#[cfg(unix)]
fn exe_name(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// `ps -o comm=` gives a path on macOS and a bare name on Linux; compare the
/// leaf either way. A live pid running something else is pid reuse, and does
/// not own our folder.
#[cfg(unix)]
fn comm_matches(comm: &str, me: &str) -> bool {
    exe_name(Path::new(comm)).is_some_and(|n| n == me)
}

// ---------------------------------------------------------------------------
// Start / segments

pub fn start(app: &AppHandle) -> Result<(), String> {
    let handle = app.state::<RecorderHandle>();
    // Cheap validation first, then release the lock: `df` on a network mount is
    // not instant, and holding the recorder mutex across it would stall the
    // tray, the hotkeys and the UI with it.
    let output_dir = {
        let r = handle.lock();
        if r.status != Status::Idle {
            return Err("Already recording.".into());
        }
        if r.ffmpeg_path.is_none() {
            return Err(capture::active().ffmpeg_hint().to_string());
        }
        if r.config.mode == "region" && r.region.is_none() {
            return Err("Select a region first.".into());
        }
        if r.config.output_dir.is_empty() {
            return Err("Choose an output folder first.".into());
        }
        r.config.output_dir.clone()
    };
    space_check(
        free_space_bytes(Path::new(&output_dir)),
        MIN_FREE_TO_START,
        &output_dir,
    )?;

    let session_id = {
        let mut r = handle.lock();
        // Re-checked because the lock was open across the space check.
        if r.status != Status::Idle {
            return Err("Already recording.".into());
        }
        let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
        let session_dir =
            PathBuf::from(&r.config.output_dir).join(session_dir_name(&stamp, std::process::id()));
        std::fs::create_dir_all(&session_dir)
            .map_err(|e| format!("cannot create temp folder: {e}"))?;
        r.session_dir = Some(session_dir);
        r.segments.clear();
        lock_recovering(&r.stderr_tail).clear();
        r.session_id += 1;
        r.status = Status::Countdown;
        r.session_id
    };
    emit_status(app, Status::Countdown);

    let app = app.clone();
    std::thread::spawn(move || {
        for n in (1..=3).rev() {
            let _ = app.emit("countdown", n);
            std::thread::sleep(Duration::from_secs(1));
            let handle = app.state::<RecorderHandle>();
            let alive = {
                let r = handle.lock();
                r.session_id == session_id && r.status == Status::Countdown
            };
            if !alive {
                return; // cancelled during countdown
            }
        }
        let _ = app.emit("countdown", 0);
        match spawn_segment(&app, session_id) {
            Ok(()) => {
                {
                    let handle = app.state::<RecorderHandle>();
                    let mut r = handle.lock();
                    r.status = Status::Recording;
                }
                spawn_disk_watchdog(&app, session_id, PathBuf::from(&output_dir));
                emit_status(&app, Status::Recording);
                let _ = app.emit("recording-started", serde_json::json!({}));
            }
            Err(e) => fail(&app, session_id, None, e),
        }
    });
    Ok(())
}

fn spawn_segment(app: &AppHandle, session_id: u64) -> Result<(), String> {
    let handle = app.state::<RecorderHandle>();
    let mut r = handle.lock();
    if r.session_id != session_id {
        return Err("session was cancelled".into());
    }
    let ff = r.ffmpeg_path.clone().ok_or("ffmpeg not found")?;
    let session_dir = r.session_dir.clone().ok_or("no active session")?;
    let seg_path = session_dir.join(format!("seg_{:03}.mp4", r.segments.len()));

    let encoder = resolve_encoder(&r.config.encoder, &r.encoders);
    let region = if r.config.mode == "region" {
        r.region
    } else {
        None
    };
    // Region mode records whichever monitor the overlay was drawn on.
    let monitor_index = region
        .map(|x| x.monitor_index)
        .unwrap_or(r.config.monitor_index);
    let target = CaptureTarget {
        screen_id: resolve_screen_id(&r.screens, monitor_index),
        region,
    };
    let args = capture::active().segment_args(&r.config, &target, &encoder, &seg_path);

    let mut cmd = ffmpeg::quiet_command(&ff);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped()) // -progress stream; see spawn_stall_watchdog
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start ffmpeg: {e}"))?;

    // Frames encoded so far in this segment, fed by the -progress reader below.
    let frames = Arc::new(AtomicU64::new(0));
    if let Some(stdout) = child.stdout.take() {
        let frames = frames.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(n) = line.trim_start().strip_prefix("frame=") {
                    if let Ok(n) = n.trim().parse::<u64>() {
                        frames.store(n, Ordering::Relaxed);
                    }
                }
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        let tail = r.stderr_tail.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut t = lock_recovering(&tail);
                if t.len() >= 80 {
                    t.pop_front();
                }
                t.push_back(line);
            }
        });
    }

    r.child = Some(child);
    r.segments.push(seg_path.clone());
    drop(r); // watchdog thread needs the lock
    spawn_stall_watchdog(app, session_id, seg_path, frames);
    Ok(())
}

/// How long a segment may produce zero frames before we call it dead.
/// Generous on purpose: a cold VideoToolbox session plus the first Retina grab
/// can take a beat, and a false positive kills a real recording.
const STALL_GRACE: Duration = Duration::from_secs(8);

/// A refused screen-capture permission doesn't make ffmpeg exit — it makes it
/// sit there forever, emitting nothing and reporting no error. Left alone the
/// UI would show a happily ticking timer and then produce an empty file, so
/// watch the frame counter and fail loudly instead.
fn spawn_stall_watchdog(
    app: &AppHandle,
    session_id: u64,
    seg_path: PathBuf,
    frames: Arc<AtomicU64>,
) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(STALL_GRACE);
        if frames.load(Ordering::Relaxed) > 0 {
            return; // capturing fine
        }
        // Superseded by a stop, pause, cancel, or a newer segment? Then zero
        // frames is expected and none of our business. `fail` decides that
        // under the same lock it tears the session down with — checking it
        // here, separately, is exactly the race this used to lose.
        fail(
            &app,
            session_id,
            Some(&seg_path),
            capture::active().stall_hint().to_string(),
        );
    });
}

fn resolve_encoder(requested: &str, available: &[String]) -> String {
    if requested != "auto" {
        return requested.to_string();
    }
    for enc in capture::active().hw_encoders() {
        if available.iter().any(|a| a == enc) {
            return enc.to_string();
        }
    }
    "libx264".to_string()
}

/// Map a UI monitor index to the backend's native screen id. Backends that
/// can't enumerate (Windows/ddagrab) return an empty list, where the monitor
/// index *is* the id.
fn resolve_screen_id(screens: &[ScreenDevice], monitor_index: usize) -> u32 {
    screens
        .get(monitor_index)
        .map(|s| s.id)
        .unwrap_or(monitor_index as u32)
}

/// Ask ffmpeg to finish cleanly (writes `q`), fall back to kill after ~4s.
fn graceful_stop(child: &mut Child) {
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"q");
        let _ = stdin.flush();
        // dropping stdin closes the pipe (EOF) as a second stop signal
    }
    for _ in 0..40 {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Pause / resume

pub fn toggle_pause(app: &AppHandle) -> Result<(), String> {
    let handle = app.state::<RecorderHandle>();
    enum Action {
        Pause(Child),
        Resume(u64),
    }
    let action = {
        let mut r = handle.lock();
        match r.status {
            Status::Recording => {
                let child = r.child.take().ok_or("recorder has no process")?;
                r.status = Status::Paused;
                Action::Pause(child)
            }
            Status::Paused => Action::Resume(r.session_id),
            _ => return Err("Not recording.".into()),
        }
    };
    match action {
        Action::Pause(mut child) => {
            graceful_stop(&mut child); // done outside the lock: takes up to 4s
            emit_status(app, Status::Paused);
            let _ = app.emit("recording-paused", serde_json::json!({}));
            Ok(())
        }
        Action::Resume(session_id) => {
            spawn_segment(app, session_id)?;
            {
                let mut r = handle.lock();
                r.status = Status::Recording;
            }
            emit_status(app, Status::Recording);
            let _ = app.emit("recording-resumed", serde_json::json!({}));
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Stop / finalize

pub fn stop(app: &AppHandle) -> Result<(), String> {
    let handle = app.state::<RecorderHandle>();
    enum Action {
        CancelCountdown(Option<PathBuf>),
        Finalize(Option<Child>),
    }
    let action = {
        let mut r = handle.lock();
        match r.status {
            Status::Countdown => {
                r.session_id += 1; // invalidates the countdown thread
                r.status = Status::Idle;
                Action::CancelCountdown(r.session_dir.take())
            }
            Status::Recording => {
                r.status = Status::Finalizing;
                Action::Finalize(r.child.take())
            }
            Status::Paused => {
                r.status = Status::Finalizing;
                Action::Finalize(None)
            }
            _ => return Err("Not recording.".into()),
        }
    };
    match action {
        Action::CancelCountdown(dir) => {
            if let Some(d) = dir {
                let _ = std::fs::remove_dir_all(d);
            }
            let _ = app.emit("recording-cancelled", serde_json::json!({}));
            emit_status(app, Status::Idle);
            Ok(())
        }
        Action::Finalize(child) => {
            emit_status(app, Status::Finalizing);
            let app = app.clone();
            std::thread::spawn(move || {
                if let Some(mut c) = child {
                    graceful_stop(&mut c);
                }
                finalize(&app);
            });
            Ok(())
        }
    }
}

fn finalize(app: &AppHandle) {
    let handle = app.state::<RecorderHandle>();
    let (ff, segments, session_dir, output_dir) = {
        let r = handle.lock();
        let good: Vec<PathBuf> = r
            .segments
            .iter()
            .filter(|p| p.metadata().map(|m| m.len() > 1024).unwrap_or(false))
            .cloned()
            .collect();
        (
            r.ffmpeg_path.clone(),
            good,
            r.session_dir.clone(),
            r.config.output_dir.clone(),
        )
    };

    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let final_path = PathBuf::from(&output_dir).join(format!("Recap_{stamp}.mp4"));

    let result: Result<PathBuf, String> = (|| {
        if segments.is_empty() {
            return Err("No video was captured — ffmpeg produced nothing.".into());
        }
        if segments.len() == 1 {
            std::fs::rename(&segments[0], &final_path)
                .map_err(|e| format!("could not move recording: {e}"))?;
        } else {
            let ff = ff.ok_or("ffmpeg not found")?;
            let dir = session_dir.clone().ok_or("no session folder")?;
            let list = dir.join("list.txt");
            let body: String = segments
                .iter()
                .map(|p| ffmpeg::concat_list_line(p) + "\n")
                .collect();
            std::fs::write(&list, body).map_err(|e| format!("could not write list: {e}"))?;
            ffmpeg::run_concat(&ff, &list, &final_path)?;
        }
        Ok(final_path.clone())
    })();

    if let Some(d) = session_dir {
        let _ = std::fs::remove_dir_all(d);
    }
    {
        let mut r = handle.lock();
        r.status = Status::Idle;
        r.child = None;
        r.segments.clear();
        r.session_dir = None;
    }

    match result {
        Ok(path) => {
            let _ = app.emit(
                "recording-stopped",
                serde_json::json!({ "path": path.display().to_string() }),
            );
        }
        Err(message) => {
            let log = stderr_tail_string(&handle);
            let _ = app.emit(
                "recording-error",
                serde_json::json!({ "message": message, "log": log }),
            );
        }
    }
    emit_status(app, Status::Idle);
}

// ---------------------------------------------------------------------------
// Hotkeys / tray / shutdown entry points

/// Ctrl+Alt+R and the tray "Start / Stop" item: start when idle, stop otherwise.
pub fn toggle_record(app: &AppHandle) {
    let status = {
        let handle = app.state::<RecorderHandle>();
        let r = handle.lock();
        r.status
    };
    let result = match status {
        Status::Idle => start(app),
        _ => stop(app),
    };
    if let Err(e) = result {
        let _ = app.emit(
            "recording-error",
            serde_json::json!({ "message": e, "log": "" }),
        );
    }
}

/// Ctrl+Alt+P: only meaningful mid-recording.
pub fn hotkey_pause(app: &AppHandle) {
    let status = {
        let handle = app.state::<RecorderHandle>();
        let r = handle.lock();
        r.status
    };
    if matches!(status, Status::Recording | Status::Paused) {
        let _ = toggle_pause(app);
    }
}

/// Best-effort cleanup on quit so we never leave an orphaned ffmpeg running.
pub fn shutdown(app: &AppHandle) {
    let handle = app.state::<RecorderHandle>();
    let child = {
        let mut r = handle.lock();
        r.status = Status::Idle;
        r.session_id += 1;
        r.child.take()
    };
    if let Some(mut c) = child {
        graceful_stop(&mut c);
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that removes itself, so a failing assertion cannot
    /// leave test litter in /tmp.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "recap-test-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn session(dir: &TempDir, stamp: &str, owner: Option<u32>) -> PathBuf {
        let name = match owner {
            Some(pid) => session_dir_name(stamp, pid),
            // Pre-pid-suffix layout, i.e. left by an older build.
            None => format!("{SESSION_PREFIX}{stamp}"),
        };
        let p = dir.0.join(name);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("seg_000.mp4"), b"pretend video").unwrap();
        p
    }

    #[test]
    fn a_session_folder_names_its_owner() {
        let name = session_dir_name("20260101_101010", 4242);
        assert_eq!(name, ".recap-tmp-20260101_101010-4242");
        assert_eq!(session_owner_pid(&name), Some(4242));
        // An older build's folder has no pid to read, and its timestamp must
        // not be mistaken for one.
        assert_eq!(session_owner_pid(".recap-tmp-20260101_101010"), None);
    }

    // ---- stale session folders --------------------------------------------

    #[test]
    fn a_folder_whose_owner_died_is_swept() {
        let dir = TempDir::new("sweep-dead");
        let stale = session(&dir, "20260101_101010", Some(4242));
        assert_eq!(
            sweep_stale_sessions_with(&dir.0, SystemTime::now(), &|_| false),
            1
        );
        assert!(!stale.exists());
    }

    /// The one that matters: a second instance is recording into its folder
    /// right now. Sweeping it would destroy a live recording.
    #[test]
    fn a_folder_owned_by_a_live_process_is_left_alone() {
        let dir = TempDir::new("sweep-live");
        let live = session(&dir, "20260101_101010", Some(4242));
        assert_eq!(
            sweep_stale_sessions_with(&dir.0, SystemTime::now(), &|pid| pid == 4242),
            0
        );
        assert!(live.join("seg_000.mp4").exists());
    }

    /// No pid in the name, so it came from an older build. Age is then the only
    /// evidence there is, and recently touched means somebody is still busy.
    #[test]
    fn an_unclaimed_folder_is_swept_only_once_it_has_gone_quiet() {
        let dir = TempDir::new("sweep-unclaimed");
        let fresh = session(&dir, "20260101_101010", None);
        assert_eq!(
            sweep_stale_sessions_with(&dir.0, SystemTime::now(), &|_| false),
            0
        );
        assert!(fresh.exists());

        // Same folder, viewed from an hour later.
        let later = SystemTime::now() + Duration::from_secs(3600);
        assert_eq!(sweep_stale_sessions_with(&dir.0, later, &|_| false), 1);
        assert!(!fresh.exists());
    }

    /// The sweep runs inside the user's own video folder. Touching anything
    /// that is not ours would delete their recordings.
    #[test]
    fn the_sweep_only_ever_touches_its_own_prefix() {
        let dir = TempDir::new("sweep-scope");
        std::fs::write(dir.0.join("Recap_2026-01-01_10-10-10.mp4"), b"keep me").unwrap();
        std::fs::create_dir_all(dir.0.join("Old exports")).unwrap();
        // A file that merely starts with the prefix is not a session folder.
        std::fs::write(dir.0.join(format!("{SESSION_PREFIX}notes")), b"keep me").unwrap();

        let later = SystemTime::now() + Duration::from_secs(3600);
        assert_eq!(sweep_stale_sessions_with(&dir.0, later, &|_| false), 0);
        assert!(dir.0.join("Recap_2026-01-01_10-10-10.mp4").exists());
        assert!(dir.0.join("Old exports").is_dir());
        assert!(dir.0.join(format!("{SESSION_PREFIX}notes")).exists());
    }

    /// A segment being written for an hour never touches the folder's own
    /// mtime, so a naive age check would call a live recording abandoned.
    #[test]
    fn age_follows_the_newest_file_not_the_folder() {
        let dir = TempDir::new("sweep-mtime");
        let path = session(&dir, "20260101_101010", None);
        let newest = newest_mtime(&path).unwrap();
        assert!(newest >= path.metadata().unwrap().modified().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn pid_reuse_does_not_keep_a_folder_forever() {
        assert!(comm_matches(
            "/Applications/Recap.app/Contents/MacOS/Recap",
            "Recap"
        ));
        assert!(comm_matches("recap", "recap"));
        assert!(!comm_matches("/usr/bin/vim", "Recap"));
    }

    // ---- disk space --------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn df_available_column_survives_spaces_in_the_other_fields() {
        let normal = "Filesystem 1024-blocks      Used Available Capacity Mounted on\n\
                      /dev/disk3s5  971350180 123456789 400000000      24% /\n";
        assert_eq!(parse_df_kb(normal), Some(400000000));

        // Both a spacey device and a spacey mount point, which shift every
        // field index left and right of the available column.
        let spacey = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                      map auto_home 100 40 60 40% /Volumes/My Big Disk\n";
        assert_eq!(parse_df_kb(spacey), Some(60));

        assert_eq!(parse_df_kb(""), None);
        assert_eq!(parse_df_kb("df: /nope: No such file or directory\n"), None);
    }

    #[cfg(unix)]
    #[test]
    fn free_space_reads_a_real_filesystem() {
        let free = free_space_bytes(&std::env::temp_dir());
        assert!(free.is_some(), "df gave us nothing for the temp dir");
        // Any mounted filesystem this test can run on has at least a megabyte.
        assert!(free.unwrap() > 1024 * 1024, "implausible: {free:?}");
        assert_eq!(free_space_bytes(Path::new("/no/such/path/here")), None);
    }

    #[test]
    fn a_start_is_refused_only_when_we_know_the_disk_is_too_small() {
        assert!(space_check(Some(MIN_FREE_TO_START), MIN_FREE_TO_START, "/out").is_ok());
        // Unknown must pass: failing to run df is not a reason to refuse.
        assert!(space_check(None, MIN_FREE_TO_START, "/out").is_ok());

        let err = space_check(Some(300 * 1024 * 1024), MIN_FREE_TO_START, "/out").unwrap_err();
        assert!(err.contains("/out"), "{err}");
        assert!(err.contains("300 MB"), "{err}");
        assert!(err.contains("1.0 GB"), "{err}");
    }

    // ---- the stall-watchdog / stop race ------------------------------------

    fn recording_session(seg: &Path) -> RecorderHandle {
        let handle = RecorderHandle::new();
        {
            let mut r = handle.lock();
            r.status = Status::Recording;
            r.segments.push(seg.to_path_buf());
            r.session_dir = Some(PathBuf::from("/tmp/recap-session"));
        }
        handle
    }

    #[test]
    fn the_stall_watchdog_aborts_a_session_that_is_still_live() {
        let seg = PathBuf::from("/tmp/seg_000.mp4");
        let handle = recording_session(&seg);
        let id = handle.lock().session_id;

        let claimed = handle.lock().claim_failure(id, Some(&seg));
        assert!(claimed.is_some());
        let r = handle.lock();
        assert_eq!(r.status, Status::Idle);
        assert!(r.segments.is_empty());
        assert_ne!(r.session_id, id, "a new session must not inherit the abort");
    }

    /// The bug: Stop wins the lock, sets Finalizing and hands the segment list
    /// to finalize() on another thread. The watchdog must then keep its hands
    /// off — clearing `segments` here made finalize report "No video was
    /// captured" and delete a perfectly good recording.
    #[test]
    fn a_stop_that_lands_first_disarms_the_stall_watchdog() {
        let seg = PathBuf::from("/tmp/seg_000.mp4");
        let handle = recording_session(&seg);
        let id = handle.lock().session_id;

        handle.lock().status = Status::Finalizing; // stop() got there first

        assert!(handle.lock().claim_failure(id, Some(&seg)).is_none());
        let r = handle.lock();
        assert_eq!(
            r.status,
            Status::Finalizing,
            "abort must not rewind the state"
        );
        assert_eq!(r.segments, vec![seg], "finalize must still see the segment");
        assert!(
            r.session_dir.is_some(),
            "the folder must still be there to concat from"
        );
    }

    /// Pause writes `q` to ffmpeg and rolls a new segment on resume without
    /// bumping session_id, so an old watchdog is still armed with the same id.
    #[test]
    fn a_watchdog_from_before_a_pause_cannot_shoot_down_the_new_segment() {
        let old = PathBuf::from("/tmp/seg_000.mp4");
        let handle = recording_session(&old);
        let id = handle.lock().session_id;

        let new = PathBuf::from("/tmp/seg_001.mp4");
        handle.lock().segments.push(new.clone());

        assert!(handle.lock().claim_failure(id, Some(&old)).is_none());
        assert_eq!(handle.lock().segments, vec![old, new]);
    }

    #[test]
    fn an_abort_from_a_previous_session_is_ignored() {
        let seg = PathBuf::from("/tmp/seg_000.mp4");
        let handle = recording_session(&seg);
        let stale_id = handle.lock().session_id;
        handle.lock().session_id += 1; // user cancelled and started again

        assert!(handle.lock().claim_failure(stale_id, Some(&seg)).is_none());
        assert_eq!(handle.lock().status, Status::Recording);
    }

    /// Paused means the segment was stopped on purpose, so zero frames is not
    /// a fault; the same holds once we are already Idle.
    #[test]
    fn an_abort_is_refused_from_states_that_have_already_ended_the_session() {
        for status in [Status::Paused, Status::Idle] {
            let seg = PathBuf::from("/tmp/seg_000.mp4");
            let handle = recording_session(&seg);
            let id = handle.lock().session_id;
            handle.lock().status = status;
            assert!(
                handle.lock().claim_failure(id, Some(&seg)).is_none(),
                "{status:?} should refuse"
            );
        }
    }

    // ---- lock poisoning ----------------------------------------------------

    /// The whole app reaches its state through this one mutex. If a panic
    /// anywhere poisons it, `lock().unwrap()` would panic on every later
    /// acquisition — Stop, the tray and the hotkeys all dead at once, while
    /// ffmpeg keeps writing.
    #[test]
    fn a_panic_while_holding_the_lock_does_not_brick_the_recorder() {
        let handle = Arc::new(RecorderHandle::new());
        let inner = handle.0.clone();
        let panicked = std::thread::spawn(move || {
            let mut guard = inner.lock().unwrap();
            guard.status = Status::Recording; // mid-mutation
            panic!("simulated panic while holding the recorder lock");
        })
        .join();
        assert!(panicked.is_err());
        assert!(handle.0.is_poisoned());

        {
            let mut r = handle.lock();
            assert_eq!(r.status, Status::Recording, "the mutation is still visible");
            r.status = Status::Idle;
        }
        // A second acquisition, because the recovery must be permanent rather
        // than a one-off — every later lock has to keep working too.
        assert_eq!(handle.lock().status, Status::Idle, "and it still stops");
    }
}
