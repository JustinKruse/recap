//! Headless command line.
//!
//! Recap is a GUI app, but capture and OCR are useful to anything that can run
//! a process — scripts, CI, and coding agents in particular. Handling argv
//! before the event loop starts means those callers get the same capture and
//! recognition code the window uses, with no window, no tray, and no hotkeys.

use crate::{capture, ffmpeg, ocr, still};
use std::path::{Path, PathBuf};

const USAGE: &str = "\
recap — screen capture, annotation and text grab

  recap shot [--display N] [--region X,Y,W,H] <out.png>
        Save a screenshot. Region is in physical pixels, relative to the display.

  recap scroll --region X,Y,W,H [--step N] [--max N] <out.png>
        Scroll-capture a region: grab, scroll, repeat, stitch into one tall
        image. Put the pointer over the scrollable area first.

  recap ocr [--display N] [--region X,Y,W,H] [--json] [image.png]
        Read text off the screen, or out of an image if one is given.
        Prints plain text; --json adds per-line boxes and confidence.

  recap --help

With no arguments, the normal windowed app starts.
";

/// Returns true if this invocation was a CLI run and the GUI should not start.
pub fn maybe_run() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let verb = match args.first().map(String::as_str) {
        Some("shot") => "shot",
        Some("ocr") => "ocr",
        Some("scroll") => "scroll",
        Some("--help") | Some("-h") | Some("help") => {
            print!("{USAGE}");
            return true;
        }
        // Anything else — including no args, and the flags Tauri's dev runner
        // passes through — belongs to the GUI.
        _ => return false,
    };

    let code = match run(verb, &args[1..]) {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(e) => {
            eprintln!("recap: {e}");
            1
        }
    };
    std::process::exit(code);
}

struct Opts {
    display: usize,
    step: u32,
    max_frames: usize,
    region: Option<(u32, u32, u32, u32)>,
    json: bool,
    positional: Vec<String>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        display: 0,
        step: 0,
        max_frames: 40,
        region: None,
        json: false,
        positional: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--display" => {
                o.display = it
                    .next()
                    .ok_or("--display needs a number")?
                    .parse()
                    .map_err(|_| "--display needs a number")?;
            }
            "--region" => {
                let raw = it.next().ok_or("--region needs X,Y,W,H")?;
                let n: Vec<i64> = raw
                    .split(',')
                    .map(|p| p.trim().parse::<i64>().map_err(|_| "--region wants numbers"))
                    .collect::<Result<_, _>>()?;
                let [x, y, w, h] = n[..] else {
                    return Err("--region wants exactly X,Y,W,H".into());
                };
                if w <= 0 || h <= 0 {
                    return Err("--region width and height must be positive".into());
                }
                o.region = Some(capture::sanitize_region(x as i32, y as i32, w as u32, h as u32));
            }
            "--step" => {
                o.step = it
                    .next()
                    .ok_or("--step needs a number")?
                    .parse()
                    .map_err(|_| "--step needs a number")?;
            }
            "--max" => {
                o.max_frames = it
                    .next()
                    .ok_or("--max needs a number")?
                    .parse()
                    .map_err(|_| "--max needs a number")?;
            }
            "--json" => o.json = true,
            other if other.starts_with('-') => return Err(format!("unknown flag `{other}`")),
            other => o.positional.push(other.to_string()),
        }
    }
    Ok(o)
}

fn run(verb: &str, args: &[String]) -> Result<String, String> {
    let o = parse(args)?;
    match verb {
        "shot" => {
            let out = o
                .positional
                .first()
                .ok_or("shot needs an output path\n\n".to_string() + USAGE)?;
            let path = capture_to(&o, Path::new(out))?;
            Ok(format!("{}\n", path.display()))
        }
        "scroll" => {
            let out = o
                .positional
                .first()
                .ok_or("scroll needs an output path\n\n".to_string() + USAGE)?;
            let region = o.region.ok_or("scroll needs --region X,Y,W,H")?;
            let opts = crate::scroll::ScrollOptions {
                display: o.display,
                region,
                step: o.step,
                max_frames: o.max_frames.clamp(2, 200),
                ..Default::default()
            };
            let r = crate::scroll::capture(ffmpeg::locate().as_deref(), &opts, Path::new(out))?;
            Ok(format!(
                "{}\n{} frames, {}x{}{}\n",
                r.path.display(),
                r.frames,
                r.width,
                r.height,
                r.stopped.note(r.frames)
            ))
        }
        "ocr" => {
            // An explicit image is read as-is; otherwise grab the screen first.
            let (image, scratch) = match o.positional.first() {
                Some(p) => (PathBuf::from(p), false),
                None => {
                    let tmp = std::env::temp_dir().join(format!(
                        "recap-cli-{}.png",
                        std::process::id()
                    ));
                    (capture_to(&o, &tmp)?, true)
                }
            };
            let result = ocr::recognize(&image);
            if scratch {
                let _ = std::fs::remove_file(&image);
            }
            let r = result?;
            Ok(if o.json {
                serde_json::to_string_pretty(&r).map_err(|e| e.to_string())? + "\n"
            } else if r.text.is_empty() {
                String::new()
            } else {
                r.text + "\n"
            })
        }
        _ => Err(format!("unknown command `{verb}`")),
    }
}

/// Grab a display, cropping afterwards if a region was asked for — the same
/// full-then-crop approach the GUI uses, for the same coordinate-space reasons.
fn capture_to(o: &Opts, out: &Path) -> Result<PathBuf, String> {
    let backend = capture::active();
    let ff = ffmpeg::locate();
    if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }

    let raw = if o.region.is_some() {
        out.with_extension("raw.png")
    } else {
        out.to_path_buf()
    };
    let ff_arg = ff.clone().unwrap_or_default();
    let (program, cmd_args) = backend.still_command(&ff_arg, o.display, true, &raw);
    if program.as_os_str().is_empty() {
        return Err(backend.ffmpeg_hint().to_string());
    }
    still::run(&program, &cmd_args)?;
    if std::fs::metadata(&raw).map(|m| m.len()).unwrap_or(0) < 512 {
        let _ = std::fs::remove_file(&raw);
        return Err(backend.stall_hint().to_string());
    }

    if let Some((x, y, w, h)) = o.region {
        let ff = ff.ok_or_else(|| backend.ffmpeg_hint().to_string())?;
        let result = still::run(
            &ff,
            &[
                "-hide_banner".into(),
                "-loglevel".into(),
                "error".into(),
                "-y".into(),
                "-i".into(),
                raw.display().to_string(),
                "-vf".into(),
                format!("crop={w}:{h}:{x}:{y}"),
                out.display().to_string(),
            ],
        );
        let _ = std::fs::remove_file(&raw);
        result?;
    }
    Ok(out.to_path_buf())
}
