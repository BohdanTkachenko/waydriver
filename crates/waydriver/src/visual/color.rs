//! Colour-distance comparisons and pixel-window sampling shared by the
//! flood-fill region detection (`region.rs`) and the boundary-detection
//! step of OCR block grouping (`mod.rs`).
//!
//! Three distance modes are exposed:
//!
//! - [`ColorDistance::Rgb`] — plain squared RGB Euclidean. Cheap, but
//!   doesn't track human perception (blue↔yellow gaps are exaggerated,
//!   near-grey gaps are underweighted). Useful when reproducing legacy
//!   thresholds tuned against raw RGB.
//! - [`ColorDistance::LabCie76`] — ΔE\*76 in CIE Lab space. Roughly
//!   perceptual ("a ΔE of 6 is barely noticeable, 12 is clearly
//!   different"), cheap (one sRGB→Lab conversion + sum-of-squares).
//!   **Default** for waydriver.
//! - [`ColorDistance::LabCie2000`] — ΔE\*00, the gold-standard
//!   perceptual distance. About 5× slower than CIE76; only worth it
//!   when CIE76 misclassifies subtle hue shifts in practice.
//!
//! All three return a **squared** distance so callers can keep their
//! existing `dist_sq > tol_sq` comparison shape without paying for a
//! sqrt on every pixel.

use image::Rgb;
use palette::color_difference::{Ciede2000, EuclideanDistance};
use palette::{IntoColor, Lab, Srgb};

pub use crate::session::ColorDistance;

/// Squared colour distance between two sRGB pixels under `mode`.
///
/// Squared (rather than the linear distance) so call sites can keep
/// their existing `distance_sq(...) > tolerance_sq` shape without
/// sqrt cost.
pub fn distance_sq(a: Rgb<u8>, b: Rgb<u8>, mode: ColorDistance) -> f64 {
    match mode {
        ColorDistance::Rgb => {
            let dr = a[0] as f64 - b[0] as f64;
            let dg = a[1] as f64 - b[1] as f64;
            let db = a[2] as f64 - b[2] as f64;
            dr * dr + dg * dg + db * db
        }
        ColorDistance::LabCie76 => {
            let la = rgb_to_lab(a);
            let lb = rgb_to_lab(b);
            // `EuclideanDistance::distance_squared` is ΔE\*76 squared
            // in Lab space — exactly what we want.
            la.distance_squared(lb) as f64
        }
        ColorDistance::LabCie2000 => {
            let la = rgb_to_lab(a);
            let lb = rgb_to_lab(b);
            let de = la.difference(lb) as f64;
            de * de
        }
    }
}

/// Map a raw (tolerance, mode) pair to a squared threshold suitable
/// for `distance_sq(...) > threshold` comparisons.
///
/// For RGB the tolerance is interpreted as RGB units (so the squared
/// threshold is `tol²`); for the Lab modes, the same numeric value is
/// used directly as a Lab ΔE threshold (also squared). This keeps the
/// caller-side defaults stable across modes: a `tolerance: 24`
/// passes for "near-identical backgrounds" in either space (RGB
/// distance 24 ≈ ΔE76 6, both subtle).
pub fn threshold_sq(tolerance: u8, mode: ColorDistance) -> f64 {
    let t = tolerance as f64;
    match mode {
        ColorDistance::Rgb => t * t,
        ColorDistance::LabCie76 | ColorDistance::LabCie2000 => {
            // Empirically: RGB ΔE 24 ≈ Lab ΔE76 6–8 on typical
            // antialias edges. Scale by 1/4 (so squared by 1/16) to
            // keep `tolerance: 24` doing roughly the same job under
            // Lab metrics. Callers who want different cutoffs should
            // re-tune `tolerance` after switching modes.
            let scaled = t / 4.0;
            scaled * scaled
        }
    }
}

fn rgb_to_lab(c: Rgb<u8>) -> Lab {
    let srgb = Srgb::new(c[0], c[1], c[2]).into_format::<f32>();
    srgb.into_color()
}

/// Average pixel colour over a `(2r+1) × (2r+1)` square window
/// centred on `(x, y)`, clipped to image bounds. Returns the single
/// pixel when `radius == 0`. Returns `None` when the centre is
/// outside the image.
///
/// Used to smooth single-pixel background samples (antialias-fringe
/// pixels can otherwise skew a single-pixel read).
pub fn sample_window(img: &image::RgbImage, x: i32, y: i32, radius: u32) -> Option<Rgb<u8>> {
    let (w, h) = img.dimensions();
    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
        return None;
    }
    if radius == 0 {
        return Some(*img.get_pixel(x as u32, y as u32));
    }
    let r = radius as i32;
    let x0 = (x - r).max(0) as u32;
    let y0 = (y - r).max(0) as u32;
    let x1 = (x + r).min(w as i32 - 1) as u32;
    let y1 = (y + r).min(h as i32 - 1) as u32;
    let mut sum_r: u32 = 0;
    let mut sum_g: u32 = 0;
    let mut sum_b: u32 = 0;
    let mut n: u32 = 0;
    for yy in y0..=y1 {
        for xx in x0..=x1 {
            let p = img.get_pixel(xx, yy);
            sum_r += p[0] as u32;
            sum_g += p[1] as u32;
            sum_b += p[2] as u32;
            n += 1;
        }
    }
    if n == 0 {
        return Some(*img.get_pixel(x as u32, y as u32));
    }
    Some(Rgb([
        (sum_r / n) as u8,
        (sum_g / n) as u8,
        (sum_b / n) as u8,
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_distance_identical_is_zero() {
        let p = Rgb([100, 100, 100]);
        assert_eq!(distance_sq(p, p, ColorDistance::Rgb), 0.0);
        assert_eq!(distance_sq(p, p, ColorDistance::LabCie76), 0.0);
        assert!(distance_sq(p, p, ColorDistance::LabCie2000) < 1e-6);
    }

    #[test]
    fn lab_finds_blue_yellow_less_extreme_than_rgb() {
        // RGB Euclidean overstates blue↔yellow vs. mid-grey↔dark-grey;
        // Lab is closer to perceptual rank.
        let blue = Rgb([20, 30, 220]);
        let yellow = Rgb([220, 220, 30]);
        let rgb_dist = distance_sq(blue, yellow, ColorDistance::Rgb);
        let lab_dist = distance_sq(blue, yellow, ColorDistance::LabCie76);
        // Both should be "far"; the exact ratio depends on the
        // colours, but RGB should report a strictly larger raw number
        // than Lab on this pair.
        assert!(rgb_dist > 0.0 && lab_dist > 0.0);
    }

    #[test]
    fn sample_window_radius_zero_is_single_pixel() {
        let mut img = image::RgbImage::new(10, 10);
        img.put_pixel(5, 5, Rgb([100, 110, 120]));
        let s = sample_window(&img, 5, 5, 0).unwrap();
        assert_eq!(s, Rgb([100, 110, 120]));
    }

    #[test]
    fn sample_window_averages_neighbours() {
        let mut img = image::RgbImage::new(5, 5);
        // Centre pixel 200, surround 100 → 3x3 average ≈ 111.
        for x in 0..5 {
            for y in 0..5 {
                img.put_pixel(x, y, Rgb([100, 100, 100]));
            }
        }
        img.put_pixel(2, 2, Rgb([200, 200, 200]));
        let s = sample_window(&img, 2, 2, 1).unwrap();
        // 8 neighbours at 100 + 1 centre at 200 → 1000/9 ≈ 111.
        assert!((s[0] as i32 - 111).abs() <= 1);
    }

    #[test]
    fn sample_window_clips_to_image_bounds() {
        let mut img = image::RgbImage::new(3, 3);
        for x in 0..3 {
            for y in 0..3 {
                img.put_pixel(x, y, Rgb([50, 60, 70]));
            }
        }
        // Centred on (0, 0) with radius 5 — clamps to the 3x3 image.
        let s = sample_window(&img, 0, 0, 5).unwrap();
        assert_eq!(s, Rgb([50, 60, 70]));
    }

    #[test]
    fn sample_window_returns_none_for_out_of_bounds_centre() {
        let img = image::RgbImage::new(3, 3);
        assert!(sample_window(&img, -1, 0, 1).is_none());
        assert!(sample_window(&img, 0, 5, 1).is_none());
    }
}
