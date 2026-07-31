//! Scrolling capture — the last Snagit pillar.
//!
//! Grab the region, scroll it, grab again, repeat until the content stops
//! changing, then stitch the frames into one tall image.
//!
//! The stitch does **not** trust the scroll distance. A synthetic scroll event
//! is a request, not a promise: pages apply momentum, snap to element
//! boundaries, lazy-load rows that shift the layout, or simply refuse to scroll
//! further at the bottom. So each pair of consecutive frames is aligned by
//! matching their pixels, and the requested step is only ever a hint for where
//! to start looking.

use image::{GenericImageView, RgbaImage};
use std::path::{Path, PathBuf};

pub struct ScrollOptions {
    pub display: usize,
    /// Physical pixels, relative to the display.
    pub region: (u32, u32, u32, u32),
    /// How far to ask the page to scroll between frames.
    pub step: u32,
    pub max_frames: usize,
    /// Time for the page to repaint (and finish any smooth scrolling) before
    /// the next grab.
    pub settle_ms: u64,
}

impl Default for ScrollOptions {
    fn default() -> Self {
        Self {
            display: 0,
            region: (0, 0, 800, 600),
            step: 0, // 0 means "about two thirds of the region height"
            max_frames: 40,
            settle_ms: 550,
        }
    }
}

pub struct ScrollResult {
    pub path: PathBuf,
    pub frames: usize,
    pub width: u32,
    pub height: u32,
    /// True if capture stopped because the page stopped moving (the good case)
    /// rather than because it hit `max_frames`.
    pub reached_end: bool,
}

// ---------------------------------------------------------------------------
// Row signatures and alignment

/// Number of horizontal samples kept per row. Enough to distinguish lines of
/// text; small enough that comparing every candidate offset stays cheap.
const SAMPLES: usize = 48;

/// A row is "the same" if its mean per-sample difference is under this. Screen
/// content is not noise-free — subpixel antialiasing and cursor blink shift a
/// few values even when nothing scrolled.
const ROW_TOLERANCE: f32 = 6.0;

fn signatures(img: &RgbaImage) -> Vec<[u8; SAMPLES]> {
    let (w, h) = img.dimensions();
    let mut out = Vec::with_capacity(h as usize);
    for y in 0..h {
        let mut row = [0u8; SAMPLES];
        for (i, slot) in row.iter_mut().enumerate() {
            let x = (i as u32 * w.saturating_sub(1)) / (SAMPLES as u32 - 1);
            let p = img.get_pixel(x.min(w - 1), y);
            // Luma; colour adds nothing for alignment and costs three compares.
            *slot = ((p[0] as u32 * 30 + p[1] as u32 * 59 + p[2] as u32 * 11) / 100) as u8;
        }
        out.push(row);
    }
    out
}

fn row_distance(a: &[u8; SAMPLES], b: &[u8; SAMPLES]) -> f32 {
    let mut sum = 0u32;
    for i in 0..SAMPLES {
        sum += a[i].abs_diff(b[i]) as u32;
    }
    sum as f32 / SAMPLES as f32
}

/// How far `b` has scrolled past `a`, in pixels, by finding the offset where
/// `a`'s lower rows best match `b`'s upper rows.
///
/// Returns `None` when nothing lines up well enough — a hard cut, a page that
/// jumped somewhere unrelated, or content that changed completely.
fn find_shift(a: &[[u8; SAMPLES]], b: &[[u8; SAMPLES]], min_overlap: usize) -> Option<(usize, f32)> {
    let h = a.len().min(b.len());
    if h <= min_overlap {
        return None;
    }
    let mut best: Option<(usize, f32)> = None;
    for shift in 1..=(h - min_overlap) {
        let overlap = h - shift;
        // Every third row is plenty to score an alignment and keeps this loop
        // from being quadratic in practice.
        let mut sum = 0.0;
        let mut n = 0u32;
        for y in (0..overlap).step_by(3) {
            sum += row_distance(&a[y + shift], &b[y]);
            n += 1;
        }
        let cost = sum / n.max(1) as f32;
        if best.is_none_or(|(_, c)| cost < c) {
            best = Some((shift, cost));
        }
    }
    best.filter(|&(_, cost)| cost <= ROW_TOLERANCE)
}

/// True when two frames are the same picture — i.e. the page didn't move.
fn looks_identical(a: &[[u8; SAMPLES]], b: &[[u8; SAMPLES]]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut sum = 0.0;
    let mut n = 0u32;
    for y in (0..a.len()).step_by(3) {
        sum += row_distance(&a[y], &b[y]);
        n += 1;
    }
    sum / n.max(1) as f32 <= ROW_TOLERANCE / 2.0
}

/// Paste each frame's *new* rows below what came before.
fn stitch(frames: &[RgbaImage], shifts: &[usize]) -> RgbaImage {
    let w = frames[0].width();
    let h = frames[0].height();
    let total = h + shifts.iter().sum::<usize>() as u32;
    let mut out = RgbaImage::new(w, total);

    image::imageops::replace(&mut out, &frames[0], 0, 0);
    let mut y = h as i64;
    for (frame, &shift) in frames[1..].iter().zip(shifts) {
        if shift == 0 {
            continue;
        }
        // The last `shift` rows of this frame are the ones not already present.
        let new_part = frame.view(0, h - shift as u32, w, shift as u32).to_image();
        image::imageops::replace(&mut out, &new_part, 0, y);
        y += shift as i64;
    }
    out
}

// ---------------------------------------------------------------------------
// Synthetic scrolling

#[cfg(target_os = "macos")]
mod input {
    use objc2_core_foundation::CGPoint;
    use objc2_core_graphics::{
        CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, CGScrollEventUnit,
    };

    pub fn cursor_position() -> Option<CGPoint> {
        CGEvent::new(None).map(|e| CGEvent::location(Some(&e)))
    }

    pub fn move_cursor(p: CGPoint) {
        // Scroll events are delivered to whatever is under the pointer, so the
        // pointer has to be inside the thing we want to scroll.
        if let Some(e) = CGEvent::new_mouse_event(None, CGEventType::MouseMoved, p, CGMouseButton::Left)
        {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&e));
        }
    }

    /// Negative `dy` scrolls the content down (the same sign convention as a
    /// physical wheel pushed away from you with natural scrolling off).
    pub fn scroll_by(dy: i32) {
        if let Some(e) = CGEvent::new_scroll_wheel_event2(
            None,
            CGScrollEventUnit::Pixel,
            1,
            dy,
            0,
            0,
        ) {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&e));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod input {
    pub struct CGPoint {
        pub x: f64,
        pub y: f64,
    }
    pub fn cursor_position() -> Option<CGPoint> {
        None
    }
    pub fn move_cursor(_p: CGPoint) {}
    pub fn scroll_by(_dy: i32) {}
}

// ---------------------------------------------------------------------------

/// Grab one frame of the region, cropping in memory rather than shelling out to
/// ffmpeg for every frame.
fn grab(ff: Option<&Path>, opts: &ScrollOptions, scratch: &Path) -> Result<RgbaImage, String> {
    let backend = crate::capture::active();
    let ff_arg = ff.map(|p| p.to_path_buf()).unwrap_or_default();
    let (program, args) = backend.still_command(&ff_arg, opts.display, scratch);
    if program.as_os_str().is_empty() {
        return Err(backend.ffmpeg_hint().to_string());
    }
    crate::still::run(&program, &args)?;

    let full = image::open(scratch).map_err(|e| format!("cannot read capture: {e}"))?;
    let (x, y, w, h) = opts.region;
    let (fw, fh) = full.dimensions();
    if x >= fw || y >= fh {
        return Err("region is outside the display".into());
    }
    Ok(full
        .view(x, y, w.min(fw - x), h.min(fh - y))
        .to_image()
        .into())
}

pub fn capture(ff: Option<&Path>, opts: &ScrollOptions, out: &Path) -> Result<ScrollResult, String> {
    let (rx, ry, rw, rh) = opts.region;
    if rw < 32 || rh < 64 {
        return Err("region is too small to scroll-capture".into());
    }
    let step = if opts.step == 0 {
        (rh * 2 / 3).max(40)
    } else {
        opts.step.min(rh.saturating_sub(20)).max(20)
    };
    // Keep at least a third of a frame overlapping so alignment has something
    // to lock onto even if the page overshoots.
    let min_overlap = (rh / 3).max(24) as usize;

    let scratch = std::env::temp_dir().join(format!("recap-scroll-{}.png", std::process::id()));
    let restore = input::cursor_position();
    input::move_cursor(objc_point(
        (rx + rw / 2) as f64,
        (ry + rh / 2) as f64,
    ));
    std::thread::sleep(std::time::Duration::from_millis(120));

    let mut frames: Vec<RgbaImage> = Vec::new();
    let mut sigs: Vec<Vec<[u8; SAMPLES]>> = Vec::new();
    let mut shifts: Vec<usize> = Vec::new();
    let mut reached_end = false;

    let result = (|| -> Result<(), String> {
        for i in 0..opts.max_frames {
            let frame = grab(ff, opts, &scratch)?;
            let sig = signatures(&frame);

            if let Some(prev) = sigs.last() {
                if looks_identical(prev, &sig) {
                    reached_end = true; // page didn't move: we're at the bottom
                    break;
                }
                match find_shift(prev, &sig, min_overlap) {
                    Some((shift, _)) => shifts.push(shift),
                    None => {
                        // Nothing matched. Better to stop with a correct image
                        // than to guess an offset and emit a visible seam.
                        if i == 1 {
                            return Err(
                                "the region didn't scroll — is the pointer over a scrollable area?"
                                    .into(),
                            );
                        }
                        break;
                    }
                }
            }
            frames.push(frame);
            sigs.push(sig);

            input::scroll_by(-(step as i32));
            std::thread::sleep(std::time::Duration::from_millis(opts.settle_ms));
        }
        Ok(())
    })();

    if let Some(p) = restore {
        input::move_cursor(p);
    }
    let _ = std::fs::remove_file(&scratch);
    result?;

    if frames.is_empty() {
        return Err("captured nothing".into());
    }
    // shifts describes gaps *between* frames, so it must be one shorter.
    shifts.truncate(frames.len().saturating_sub(1));

    let stitched = stitch(&frames, &shifts);
    let (width, height) = (stitched.width(), stitched.height());
    stitched
        .save(out)
        .map_err(|e| format!("cannot write {}: {e}", out.display()))?;

    Ok(ScrollResult {
        path: out.to_path_buf(),
        frames: frames.len(),
        width,
        height,
        reached_end,
    })
}

#[cfg(target_os = "macos")]
fn objc_point(x: f64, y: f64) -> objc2_core_foundation::CGPoint {
    objc2_core_foundation::CGPoint { x, y }
}
#[cfg(not(target_os = "macos"))]
fn objc_point(x: f64, y: f64) -> input::CGPoint {
    input::CGPoint { x, y }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a tall synthetic "page" with distinct horizontal bands, then take
    /// windows of it as if scrolling — the ground-truth shift is known exactly.
    fn page(h: u32) -> RgbaImage {
        let mut img = RgbaImage::new(200, h);
        for y in 0..h {
            let v = ((y * 7) % 251) as u8;
            for x in 0..200 {
                img.put_pixel(x, y, image::Rgba([v, v.wrapping_add(40), v ^ 0x5a, 255]));
            }
        }
        img
    }

    fn window(src: &RgbaImage, top: u32, h: u32) -> RgbaImage {
        src.view(0, top, src.width(), h).to_image()
    }

    #[test]
    fn shift_is_recovered_exactly() {
        let p = page(1200);
        let a = signatures(&window(&p, 0, 400));
        let b = signatures(&window(&p, 137, 400));
        let (shift, _) = find_shift(&a, &b, 100).expect("frames should align");
        assert_eq!(shift, 137);
    }

    #[test]
    fn an_unmoved_page_is_detected_rather_than_aligned() {
        let p = page(1200);
        let a = signatures(&window(&p, 300, 400));
        let b = signatures(&window(&p, 300, 400));
        assert!(looks_identical(&a, &b));
    }

    #[test]
    fn unrelated_frames_do_not_produce_a_confident_alignment() {
        let a = signatures(&RgbaImage::from_pixel(200, 400, image::Rgba([0, 0, 0, 255])));
        let mut noise = RgbaImage::new(200, 400);
        for y in 0..400u32 {
            for x in 0..200u32 {
                let v = ((x * 31 + y * 97) % 255) as u8;
                noise.put_pixel(x, y, image::Rgba([v, v, v, 255]));
            }
        }
        assert!(find_shift(&a, &signatures(&noise), 100).is_none());
    }

    #[test]
    fn stitching_reassembles_the_original_page() {
        let p = page(1000);
        let h = 400;
        let frames = vec![window(&p, 0, h), window(&p, 250, h), window(&p, 500, h)];
        let out = stitch(&frames, &[250, 250]);
        assert_eq!(out.height(), 900);
        // Every row of the stitched image must equal the source page's row.
        for y in 0..900u32 {
            assert_eq!(out.get_pixel(10, y), p.get_pixel(10, y), "row {y} differs");
        }
    }
}
