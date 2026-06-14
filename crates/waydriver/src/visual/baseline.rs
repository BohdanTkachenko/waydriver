//! Perceptual baseline comparison — diff a captured crop against a
//! committed reference image and report a score.
//!
//! This is a **data-returning primitive, not an assertion.** waydriver
//! locates, inspects, and returns information; deciding pass/fail,
//! storing reference PNGs, and re-recording them on intentional UI
//! changes are the consumer test harness's job. The functions here only
//! compute *how different* two images are — the caller compares the
//! score against whatever tolerance it cares about and reacts.
//!
//! It exists in waydriver (rather than each consumer) for one reason:
//! the perceptual stack is already compiled in for the visual locator.
//! [`compare_rgb`] reuses [`super::color::distance_sq`] (palette
//! CIEDE2000) for per-pixel perceptual difference and `imageproc`'s
//! normalized cross-correlation for a global structural-similarity
//! diagnostic — so an MCP/LLM consumer that can't vendor Rust image
//! crates still gets a meaningful diff.
//!
//! ## Score
//!
//! The headline [`BaselineComparison::score`] is the **fraction of
//! pixels** whose CIEDE2000 ΔE exceeds [`DEFAULT_PER_PIXEL_DELTA_E`] (a
//! just-noticeable-difference threshold). This catches both whole-area
//! changes (a red headerbar tint flips ~every pixel) and tiny localized
//! ones (a cursor I-beam vs block glyph flips a small but non-zero
//! fraction) — a mean-ΔE score would dilute the latter into noise.
//! `mean_delta_e`, `max_delta_e`, and `ncc` ride along as diagnostics.

use image::{DynamicImage, Rgb, RgbImage};
use imageproc::template_matching::{match_template, MatchTemplateMethod};

use super::color::distance_sq;
use crate::error::{Error, Result};
use crate::session::ColorDistance;

/// Per-pixel CIEDE2000 ΔE above which a pixel counts as "different".
/// ~2.3 is the textbook just-noticeable-difference; below it a colour
/// shift is imperceptible to a human and almost always capture noise
/// (subpixel antialiasing on glyph edges).
pub const DEFAULT_PER_PIXEL_DELTA_E: f64 = 2.3;

/// Outcome of a baseline comparison. Pure data — `matched` is the
/// caller-supplied `tolerance` applied to `score`, reported for
/// convenience, never an error condition.
#[derive(Clone, Debug)]
pub struct BaselineComparison {
    /// `score <= tolerance`. Informational; a mismatch is not an error.
    pub matched: bool,
    /// Fraction of pixels (`[0, 1]`) whose ΔE exceeds the JND threshold.
    pub score: f64,
    /// Mean CIEDE2000 ΔE across every pixel.
    pub mean_delta_e: f64,
    /// Largest per-pixel CIEDE2000 ΔE.
    pub max_delta_e: f64,
    /// Global normalized cross-correlation of the luma channels in
    /// `[0, 1]` (1.0 = identical structure). Robust to uniform
    /// brightness shifts; weak on pure colour change — a structural
    /// counterpart to the colour-based `score`.
    pub ncc: f32,
    /// Number of pixels counted as different.
    pub diff_pixels: u64,
    /// Total pixels compared (`width * height`).
    pub total_pixels: u64,
    /// Image width (both images share it; they must match).
    pub width: u32,
    /// Image height.
    pub height: u32,
    /// The tolerance `matched` was computed against.
    pub tolerance: f64,
}

/// Decode two PNG byte buffers and compare them. Returns the perceptual
/// [`BaselineComparison`]; errors only on a decode failure or a
/// dimension mismatch — a visual difference is reported via the score,
/// never as an `Err`.
pub fn compare_to_baseline(
    actual_png: &[u8],
    baseline_png: &[u8],
    tolerance: f64,
) -> Result<BaselineComparison> {
    let actual = decode_rgb(actual_png, "captured")?;
    let baseline = decode_rgb(baseline_png, "baseline")?;
    compare_rgb(&actual, &baseline, tolerance, DEFAULT_PER_PIXEL_DELTA_E)
}

/// Render a diff visualization PNG: pixels that exceed the JND threshold
/// are painted solid red, the rest is a dimmed greyscale of `actual_png`
/// so the changed regions stand out against the original layout. Pure
/// data — waydriver does not write it anywhere; the caller decides.
pub fn diff_to_baseline(actual_png: &[u8], baseline_png: &[u8]) -> Result<Vec<u8>> {
    let actual = decode_rgb(actual_png, "captured")?;
    let baseline = decode_rgb(baseline_png, "baseline")?;
    let (aw, ah) = actual.dimensions();
    let (bw, bh) = baseline.dimensions();
    if (aw, ah) != (bw, bh) {
        return Err(Error::visual(format!(
            "baseline dimensions {bw}x{bh} != captured {aw}x{ah}"
        )));
    }
    let jnd_sq = DEFAULT_PER_PIXEL_DELTA_E * DEFAULT_PER_PIXEL_DELTA_E;
    let diff = RgbImage::from_fn(aw, ah, |x, y| {
        let a = actual.get_pixel(x, y);
        let b = baseline.get_pixel(x, y);
        if distance_sq(*a, *b, ColorDistance::LabCie2000) > jnd_sq {
            Rgb([255, 0, 0])
        } else {
            let luma = 0.299 * a[0] as f32 + 0.587 * a[1] as f32 + 0.114 * a[2] as f32;
            let dim = (luma / 2.0) as u8;
            Rgb([dim, dim, dim])
        }
    });
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    DynamicImage::ImageRgb8(diff)
        .write_with_encoder(encoder)
        .map_err(|e| Error::screenshot_with("encode diff PNG", e))?;
    Ok(out)
}

/// Compare two already-decoded RGB images. `per_pixel_jnd` is the
/// CIEDE2000 ΔE above which a pixel counts as different. Errors when the
/// dimensions differ (a resized widget is a real change the caller must
/// see, not a dilutable per-pixel diff) or when an image is empty.
pub(crate) fn compare_rgb(
    actual: &RgbImage,
    baseline: &RgbImage,
    tolerance: f64,
    per_pixel_jnd: f64,
) -> Result<BaselineComparison> {
    let (aw, ah) = actual.dimensions();
    let (bw, bh) = baseline.dimensions();
    if (aw, ah) != (bw, bh) {
        return Err(Error::visual(format!(
            "baseline dimensions {bw}x{bh} != captured {aw}x{ah}"
        )));
    }
    if aw == 0 || ah == 0 {
        return Err(Error::visual("image is empty (zero width or height)"));
    }

    let jnd_sq = per_pixel_jnd * per_pixel_jnd;
    let mut diff_pixels: u64 = 0;
    let mut sum_de: f64 = 0.0;
    let mut max_de: f64 = 0.0;
    for (a, b) in actual.pixels().zip(baseline.pixels()) {
        // `distance_sq` returns the *squared* ΔE, so compare against the
        // squared threshold and only pay the sqrt for the running stats.
        let d2 = distance_sq(*a, *b, ColorDistance::LabCie2000);
        let de = d2.sqrt();
        sum_de += de;
        if de > max_de {
            max_de = de;
        }
        if d2 > jnd_sq {
            diff_pixels += 1;
        }
    }

    let total_pixels = aw as u64 * ah as u64;
    let score = diff_pixels as f64 / total_pixels as f64;
    Ok(BaselineComparison {
        matched: score <= tolerance,
        score,
        mean_delta_e: sum_de / total_pixels as f64,
        max_delta_e: max_de,
        ncc: compute_ncc(actual, baseline),
        diff_pixels,
        total_pixels,
        width: aw,
        height: ah,
        tolerance,
    })
}

/// Global normalized cross-correlation of the two images' luma channels.
/// Reuses the same `imageproc` primitive as the template-match locator;
/// with equal dimensions the score grid collapses to a single cell.
fn compute_ncc(a: &RgbImage, b: &RgbImage) -> f32 {
    let a_gray = DynamicImage::ImageRgb8(a.clone()).into_luma8();
    let b_gray = DynamicImage::ImageRgb8(b.clone()).into_luma8();
    // Dimensions are equal (checked by the caller), so `match_template`
    // produces a 1x1 grid — read the single normalized score. A fully
    // black image has a zero norm and yields NaN; report 0.0 then.
    let result = match_template(
        &a_gray,
        &b_gray,
        MatchTemplateMethod::CrossCorrelationNormalized,
    );
    let v = result.get_pixel(0, 0)[0];
    if v.is_finite() {
        v
    } else {
        0.0
    }
}

fn decode_rgb(bytes: &[u8], label: &str) -> Result<RgbImage> {
    let img = image::load_from_memory(bytes)
        .map_err(|e| Error::visual(format!("decode {label} image: {e}")))?;
    Ok(img.into_rgb8())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn solid(w: u32, h: u32, c: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(w, h, Rgb(c))
    }

    fn encode(img: &RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        let enc = image::codecs::png::PngEncoder::new(&mut out);
        DynamicImage::ImageRgb8(img.clone())
            .write_with_encoder(enc)
            .unwrap();
        out
    }

    #[test]
    fn identical_images_match_with_zero_score() {
        let img = solid(32, 32, [100, 110, 120]);
        let cmp = compare_rgb(&img, &img, 0.0, DEFAULT_PER_PIXEL_DELTA_E).unwrap();
        assert!(cmp.matched);
        assert_eq!(cmp.diff_pixels, 0);
        assert_eq!(cmp.score, 0.0);
        assert!(cmp.mean_delta_e < 1e-6);
        assert!((cmp.ncc - 1.0).abs() < 1e-3, "ncc={}", cmp.ncc);
        assert_eq!(cmp.total_pixels, 32 * 32);
    }

    #[test]
    fn full_tint_change_is_total_mismatch() {
        let red = solid(16, 16, [220, 30, 30]);
        let green = solid(16, 16, [30, 200, 60]);
        let cmp = compare_rgb(&red, &green, 0.0, DEFAULT_PER_PIXEL_DELTA_E).unwrap();
        assert!(!cmp.matched);
        assert_eq!(cmp.score, 1.0);
        assert_eq!(cmp.diff_pixels, cmp.total_pixels);
        assert!(cmp.max_delta_e > 10.0, "maxΔE={}", cmp.max_delta_e);
    }

    #[test]
    fn small_localized_change_is_caught_but_tolerable() {
        // 64x64 base; flip a 4x4 block => 16 of 4096 px (~0.39%).
        let base = solid(64, 64, [128, 128, 128]);
        let mut changed = base.clone();
        for y in 0..4 {
            for x in 0..4 {
                changed.put_pixel(x, y, Rgb([255, 0, 0]));
            }
        }
        // Strict: any differing pixel is a mismatch.
        let strict = compare_rgb(&changed, &base, 0.0, DEFAULT_PER_PIXEL_DELTA_E).unwrap();
        assert!(!strict.matched);
        assert_eq!(strict.diff_pixels, 16);
        assert!((strict.score - 16.0 / 4096.0).abs() < 1e-9);
        // Tolerant: allow 1% of pixels to differ -> passes.
        let lenient = compare_rgb(&changed, &base, 0.01, DEFAULT_PER_PIXEL_DELTA_E).unwrap();
        assert!(lenient.matched);
    }

    #[test]
    fn dimension_mismatch_errors() {
        let a = solid(10, 10, [0, 0, 0]);
        let b = solid(10, 11, [0, 0, 0]);
        let err = compare_rgb(&a, &b, 0.0, DEFAULT_PER_PIXEL_DELTA_E).unwrap_err();
        assert!(format!("{err}").contains("dimensions"), "got: {err}");
    }

    #[test]
    fn compare_to_baseline_round_trips_png_bytes() {
        let a = solid(20, 20, [10, 200, 100]);
        let bytes = encode(&a);
        let cmp = compare_to_baseline(&bytes, &bytes, 0.0).unwrap();
        assert!(cmp.matched);
        assert_eq!(cmp.diff_pixels, 0);
        assert_eq!((cmp.width, cmp.height), (20, 20));
    }

    #[test]
    fn diff_image_marks_changed_pixels_red() {
        let base = solid(8, 8, [128, 128, 128]);
        let mut changed = base.clone();
        changed.put_pixel(2, 3, Rgb([255, 0, 0]));
        let png = diff_to_baseline(&encode(&changed), &encode(&base)).unwrap();
        let out = image::load_from_memory(&png).unwrap().into_rgb8();
        assert_eq!(out.dimensions(), (8, 8));
        assert_eq!(*out.get_pixel(2, 3), Rgb([255, 0, 0]));
        // An unchanged pixel is dimmed greyscale, not red.
        assert_ne!(*out.get_pixel(0, 0), Rgb([255, 0, 0]));
    }

    #[test]
    fn diff_image_dimension_mismatch_errors() {
        let a = encode(&solid(8, 8, [0, 0, 0]));
        let b = encode(&solid(8, 9, [0, 0, 0]));
        assert!(diff_to_baseline(&a, &b).is_err());
    }
}
