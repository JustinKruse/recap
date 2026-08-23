//! OCR / text grab — the fourth Snagit pillar.
//!
//! On macOS this talks to Vision.framework directly through objc2 rather than
//! shelling out to a Swift helper: no Xcode toolchain at build time, and no
//! extra binary to bundle, sign and notarise.
//!
//! Vision is fed an image *file*, which fits how the rest of Recap already
//! works — `still::capture` produces a PNG, and OCR reads it.

use serde::Serialize;
use std::path::Path;

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OcrLine {
    pub text: String,
    /// Vision's own 0..1 score for this reading.
    pub confidence: f32,
    /// Normalised 0..1 box, origin **top-left** — flipped from Vision's
    /// bottom-left origin so it matches every other coordinate in Recap.
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct OcrResult {
    /// Every line joined with newlines, in reading order.
    pub text: String,
    pub lines: Vec<OcrLine>,
}

/// Vision returns observations in no particular order. Sort into reading order:
/// top to bottom, then left to right, treating lines whose vertical spans
/// mostly overlap as the same row so a two-column line doesn't get split apart.
fn sort_reading_order(lines: &mut [OcrLine]) {
    lines.sort_by(|a, b| {
        let same_row = (a.y - b.y).abs() < (a.height.min(b.height) * 0.5).max(0.004);
        if same_row {
            a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal)
        } else {
            a.y.partial_cmp(&b.y).unwrap_or(std::cmp::Ordering::Equal)
        }
    });
}

fn assemble(mut lines: Vec<OcrLine>) -> OcrResult {
    sort_reading_order(&mut lines);
    let text = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    OcrResult { text, lines }
}

#[cfg(target_os = "macos")]
pub fn recognize(path: &Path) -> Result<OcrResult, String> {
    use objc2::AnyThread; // brings `alloc` into scope
    use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };

    if !path.is_file() {
        return Err("image to read text from is missing".into());
    }
    let path_str = path.to_str().ok_or("path is not valid UTF-8")?;

    let lines = unsafe {
        let url = NSURL::fileURLWithPath(&NSString::from_str(path_str));
        let options = NSDictionary::new();
        let handler = VNImageRequestHandler::initWithURL_options(
            VNImageRequestHandler::alloc(),
            &url,
            &options,
        );

        let request = VNRecognizeTextRequest::new();
        // Accurate is the slower neural path; on a screenshot of UI text the
        // difference against Fast is large and the cost is still well under a
        // second, so it's the right default for a text-grab tool.
        request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
        request.setUsesLanguageCorrection(true);

        let as_request: &VNRequest = &request;
        let requests = NSArray::from_slice(&[as_request]);
        handler
            .performRequests_error(&requests)
            .map_err(|e| format!("text recognition failed: {e}"))?;

        let mut out = Vec::new();
        if let Some(observations) = request.results() {
            for observation in observations.iter() {
                let candidates = observation.topCandidates(1);
                let Some(best) = candidates.iter().next() else {
                    continue;
                };
                let text = best.string().to_string();
                if text.trim().is_empty() {
                    continue;
                }
                let b = observation.boundingBox();
                out.push(OcrLine {
                    text,
                    confidence: best.confidence(),
                    // Vision can report a box a hair outside the image; clamp so
                    // consumers never see a negative normalised coordinate.
                    x: b.origin.x.clamp(0.0, 1.0),
                    // Vision's origin is bottom-left; flip to top-left.
                    y: (1.0 - b.origin.y - b.size.height).clamp(0.0, 1.0),
                    width: b.size.width.clamp(0.0, 1.0),
                    height: b.size.height.clamp(0.0, 1.0),
                });
            }
        }
        out
    };

    Ok(assemble(lines))
}

#[cfg(not(target_os = "macos"))]
pub fn recognize(_path: &Path) -> Result<OcrResult, String> {
    // Windows has Windows.Media.Ocr, which needs a language pack present and a
    // WinRT binding; not wired up yet, so fail honestly rather than silently
    // returning nothing.
    Err("Text grab is macOS-only so far.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, x: f64, y: f64) -> OcrLine {
        OcrLine {
            text: text.into(),
            confidence: 1.0,
            x,
            y,
            width: 0.2,
            height: 0.02,
        }
    }

    #[test]
    fn lines_are_sorted_top_to_bottom() {
        let r = assemble(vec![
            line("third", 0.1, 0.9),
            line("first", 0.1, 0.1),
            line("second", 0.1, 0.5),
        ]);
        assert_eq!(r.text, "first\nsecond\nthird");
    }

    #[test]
    fn columns_on_one_row_read_left_to_right() {
        // Same row (y differs by less than half the line height), out of order.
        let r = assemble(vec![line("right", 0.7, 0.502), line("left", 0.1, 0.5)]);
        assert_eq!(r.text, "left\nright");
    }

    #[test]
    fn a_clearly_lower_line_stays_below_even_when_further_left() {
        let r = assemble(vec![line("below", 0.05, 0.60), line("above", 0.80, 0.50)]);
        assert_eq!(r.text, "above\nbelow");
    }

    /// Bench hook, not a real test: reads whatever image `RECAP_OCR_IMAGE`
    /// points at and prints the result. Used to measure recognition changes
    /// against a known-text fixture.
    ///   RECAP_OCR_IMAGE=x.png cargo test ocr_image_from_env -- --nocapture
    #[test]
    fn ocr_image_from_env() {
        let Ok(p) = std::env::var("RECAP_OCR_IMAGE") else {
            return;
        };
        match recognize(Path::new(&p)) {
            Ok(r) => println!("---OCR-BEGIN---\n{}\n---OCR-END---", r.text),
            Err(e) => println!("---OCR-ERROR--- {e}"),
        }
    }

    #[test]
    fn empty_input_produces_empty_text_not_a_stray_newline() {
        let r = assemble(vec![]);
        assert_eq!(r.text, "");
        assert!(r.lines.is_empty());
    }
}
