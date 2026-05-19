//! Visually-distinct enclosing regions around an OCR match.
//!
//! Driven from [`Locator::find_regions`](crate::Locator::find_regions),
//! [`first_region`](crate::Locator::first_region), and
//! [`last_region`](crate::Locator::last_region). The algorithm is a
//! BFS flood-fill that grows a contiguous-colour region from a seed
//! adjacent to an `inner` bounding rectangle (typically an OCR text
//! bbox). One flood gives the **innermost** enclosing region — the
//! button pill / row rectangle / card frame immediately around the
//! text. Repeated outward floods build a chain of enclosures, ending
//! when the region reaches the parent's bounds.
//!
//! Works for any closed shape (rectangles, pills, circles, polygon
//! icons) because the algorithm is colour-region-based; each region
//! exposes both an axis-aligned bounding rectangle and a geometric
//! centroid that's accurate even when the shape isn't rectangular.

use std::collections::VecDeque;
use std::sync::Arc;

use image::{Rgb, RgbImage};

use crate::atspi::Rect;
use crate::error::{Error, Result};
use crate::locator;
use crate::session::{Session, VisualRegionTuning};

/// A flood-filled visual region, ready to click/hover/screenshot.
///
/// `RegionLocator` represents a *visually-distinct shape* on screen —
/// typically the button pill / row / card frame surrounding an OCR
/// match. Distinct from [`VisualLocator`](super::VisualLocator),
/// which represents the text glyphs themselves. The two compose:
/// `find_by_text` → text bbox, then `last_region` → enclosing shape.
#[derive(Clone)]
pub struct RegionLocator {
    session: Arc<Session>,
    bbox: Rect,
    centroid: (i32, i32),
    shape: Shape,
}

impl std::fmt::Debug for RegionLocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegionLocator")
            .field("kind", &"visual-region")
            .field("shape", &self.shape)
            .field("bbox", &self.bbox)
            .field("centroid", &self.centroid)
            .finish()
    }
}

impl RegionLocator {
    /// Axis-aligned bounding rectangle of the region's pixels, in
    /// screen coordinates.
    pub fn bounds(&self) -> Rect {
        self.bbox
    }

    /// Geometric centre of the region's pixel set, in screen coords.
    /// More accurate than `bounds().center_*()` for non-rectangular
    /// shapes (pills, circles) where the bbox centre can land outside
    /// the actual region.
    pub fn centroid(&self) -> (i32, i32) {
        self.centroid
    }

    /// Coarse shape classification of the flood region. Derived from
    /// fill-ratio + a 4-corner-inside test on the bounding rect. See
    /// [`Shape`] for the categories and their interpretation.
    pub fn shape(&self) -> Shape {
        self.shape
    }

    /// Move the pointer to the centroid and click. Uses the same
    /// motion-warmup-then-press pattern as
    /// [`VisualLocator::click`](super::VisualLocator::click) to side-step
    /// headless-mutter's cold-start pointer-routing race.
    pub async fn click(&self) -> Result<()> {
        let (cx, cy) = (self.centroid.0 as f64, self.centroid.1 as f64);
        tracing::debug!(cx, cy, bbox = ?self.bbox, "region: click");
        super::cold_start_click(&self.session, cx, cy).await
    }

    /// Move the pointer to the centroid without clicking.
    pub async fn hover(&self) -> Result<()> {
        self.session
            .pointer_motion_absolute(self.centroid.0 as f64, self.centroid.1 as f64)
            .await?;
        Ok(())
    }

    /// Take a screenshot cropped to this region's bounding rect.
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let raw = self.session.take_screenshot().await?;
        let full = locator::decode_screenshot_png(&raw)?;
        let cropped = locator::crop_to_bounds(full, self.bbox)?;
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        cropped
            .write_with_encoder(encoder)
            .map_err(|e| Error::screenshot_with("encode region PNG", e))?;
        Ok(out)
    }
}

// ── Public entry points called from `Locator` ────────────────────────────

/// Flood-fill chain: inner → outer. The returned `Vec` is in the
/// natural sweep order (inner first). `Locator::find_regions` reverses
/// it for the outer-first public API.
pub(crate) fn sweep_regions(
    session: &Arc<Session>,
    parent_bounds: Rect,
    inner_bbox: Rect,
    full_png: &[u8],
    tuning: VisualRegionTuning,
) -> Result<Vec<RegionLocator>> {
    let cropped = build_cropped_rgb(full_png, parent_bounds)?;
    let inner_in_crop = translate_to_crop(inner_bbox, parent_bounds);
    let img_bounds = crop_bounds(&cropped);

    let Some(initial_seed) = pick_seed_outside(inner_in_crop, img_bounds, &cropped, tuning) else {
        return Err(Error::visual(
            "region sweep: no valid seed pixel adjacent to inner bbox",
        ));
    };

    let mut regions: Vec<RegionLocator> = Vec::new();
    let mut seed = initial_seed;
    let max_pixels = (img_bounds.width as usize) * (img_bounds.height as usize);

    for _ in 0..tuning.max_regions {
        let flood = flood_fill(
            &cropped,
            seed,
            tuning.tolerance,
            max_pixels,
            tuning.color_distance,
        );
        let bbox_screen = translate_to_screen(flood.bbox, parent_bounds);
        let centroid_screen = (
            flood.centroid.0 + parent_bounds.x,
            flood.centroid.1 + parent_bounds.y,
        );

        // Stop if the new region didn't grow over the previous one
        // — happens on single-colour parents and as a fixed-point
        // when the chain reaches the outermost ring.
        if let Some(prev) = regions.last() {
            if prev.bbox == bbox_screen {
                break;
            }
        }

        let shape = classify_shape(flood.bbox, flood.pixel_count, flood.corners_inside, tuning);
        regions.push(RegionLocator {
            session: session.clone(),
            bbox: bbox_screen,
            centroid: centroid_screen,
            shape,
        });

        // Stop when the region covers the entire cropped image.
        if flood.bbox.width >= img_bounds.width && flood.bbox.height >= img_bounds.height {
            break;
        }

        // Seed the next iteration just outside the current region.
        match pixel_just_outside(flood.bbox, img_bounds) {
            Some(next) => seed = next,
            None => break,
        }
    }

    Ok(regions)
}

/// Flood-fill from an explicit screen pixel — no OCR, no AT-SPI
/// parent. The pixel must land inside the target region (any pixel
/// inside is fine; flood-fill recovers the same bbox/centroid/shape
/// regardless of starting point). Returns the [`RegionLocator`] for
/// the contiguous-colour region containing the seed.
///
/// Backs [`Session::region_at`](crate::Session::region_at), the
/// lowest-level entry point in the visual stack — useful when the
/// caller already has coordinates (e.g. from a debugger, a previous
/// screenshot inspection, or a hard-coded layout assumption) and
/// wants to address whatever widget those coordinates land on.
pub(crate) fn region_at_seed(
    session: &Arc<Session>,
    seed: (i32, i32),
    full_png: &[u8],
    tuning: VisualRegionTuning,
) -> Result<RegionLocator> {
    let full = locator::decode_screenshot_png(full_png)
        .map_err(|e| Error::visual(format!("decode screenshot: {e}")))?;
    let rgb = full.into_rgb8();
    let (w, h) = rgb.dimensions();
    if seed.0 < 0 || seed.1 < 0 || seed.0 >= w as i32 || seed.1 >= h as i32 {
        return Err(Error::visual(format!(
            "region_at: seed ({}, {}) outside the {}x{} screenshot",
            seed.0, seed.1, w, h
        )));
    }

    let max_pixels = (w as usize) * (h as usize);
    let flood = flood_fill(
        &rgb,
        seed,
        tuning.tolerance,
        max_pixels,
        tuning.color_distance,
    );
    let shape = classify_shape(flood.bbox, flood.pixel_count, flood.corners_inside, tuning);
    Ok(RegionLocator {
        session: session.clone(),
        // No crop was applied, so flood's coords are already screen
        // coords — no translation needed.
        bbox: flood.bbox,
        centroid: flood.centroid,
        shape,
    })
}

/// Innermost region only — short-circuits after one flood-fill.
pub(crate) fn last_region_only(
    session: &Arc<Session>,
    parent_bounds: Rect,
    inner_bbox: Rect,
    full_png: &[u8],
    tuning: VisualRegionTuning,
) -> Result<RegionLocator> {
    let cropped = build_cropped_rgb(full_png, parent_bounds)?;
    let inner_in_crop = translate_to_crop(inner_bbox, parent_bounds);
    let img_bounds = crop_bounds(&cropped);

    let Some(seed) = pick_seed_outside(inner_in_crop, img_bounds, &cropped, tuning) else {
        return Err(Error::visual(
            "last_region: no valid seed pixel adjacent to inner bbox",
        ));
    };

    let max_pixels = (img_bounds.width as usize) * (img_bounds.height as usize);
    let flood = flood_fill(
        &cropped,
        seed,
        tuning.tolerance,
        max_pixels,
        tuning.color_distance,
    );
    let shape = classify_shape(flood.bbox, flood.pixel_count, flood.corners_inside, tuning);
    Ok(RegionLocator {
        session: session.clone(),
        bbox: translate_to_screen(flood.bbox, parent_bounds),
        centroid: (
            flood.centroid.0 + parent_bounds.x,
            flood.centroid.1 + parent_bounds.y,
        ),
        shape,
    })
}

// ── Internals ────────────────────────────────────────────────────────────

fn build_cropped_rgb(full_png: &[u8], parent_bounds: Rect) -> Result<RgbImage> {
    let full = locator::decode_screenshot_png(full_png)
        .map_err(|e| Error::visual(format!("decode screenshot: {e}")))?;
    let cropped = locator::crop_to_bounds(full, parent_bounds)
        .map_err(|e| Error::visual(format!("crop to parent: {e}")))?;
    Ok(cropped.into_rgb8())
}

fn crop_bounds(img: &RgbImage) -> Rect {
    let (w, h) = img.dimensions();
    Rect {
        x: 0,
        y: 0,
        width: w as i32,
        height: h as i32,
    }
}

fn translate_to_crop(r: Rect, parent: Rect) -> Rect {
    Rect {
        x: r.x - parent.x,
        y: r.y - parent.y,
        width: r.width,
        height: r.height,
    }
}

fn translate_to_screen(r: Rect, parent: Rect) -> Rect {
    Rect {
        x: r.x + parent.x,
        y: r.y + parent.y,
        width: r.width,
        height: r.height,
    }
}

/// Coarse classification of the region's shape, derived from the
/// flood-fill's pixel-count vs bbox-area ratio plus a 4-corner test.
/// Useful for assertions ("the thing I just clicked was a pill, not
/// a checkbox") and for telling logs apart at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// Filled axis-aligned rectangle — all four bbox corners are in
    /// the region and fill ratio is ≈ 1.0. Bare GTK buttons /
    /// `AdwButtonRow` interiors typically classify here.
    Rectangle,
    /// Rounded rectangle / capsule — fill ratio is high but the four
    /// bbox corners sit outside the shape (the corner radius trims
    /// them). Most GTK button pills and Adw row backgrounds end up
    /// here.
    Pill,
    /// Filled ellipse or circle — fill ratio close to π/4 ≈ 0.785,
    /// no bbox corners inside. Round avatar buttons, circular
    /// close-icon buttons.
    Ellipse,
    /// Anything else — polygon icons, regions with holes, shapes
    /// whose fill ratio doesn't fit a primitive. Don't rely on the
    /// bbox center being inside; use the centroid.
    Irregular,
}

#[derive(Debug)]
pub(super) struct FloodResult {
    pub(super) bbox: Rect,
    pub(super) centroid: (i32, i32),
    pub(super) pixel_count: usize,
    /// Number of bbox corners (0..=4) whose colour was close enough
    /// to the seed to count as "inside" the region. A clean filled
    /// rectangle has 4; a rounded pill has 0.
    pub(super) corners_inside: u8,
    /// `visited[(y * width + x)] == true` iff the BFS reached
    /// `(x, y)`. Only populated when [`FloodResult::capped`] matters
    /// to the caller; callers that don't need it can ignore the
    /// field. Tightly coupled to the image's `(width, height)`.
    pub(super) visited: Vec<bool>,
    /// Width of `visited` (== source image width). Needed to index
    /// `visited` by `(x, y)` without re-querying the image.
    pub(super) image_width: u32,
}

/// 4-connected BFS flood-fill. Adds neighbours whose RGB Euclidean
/// distance to the seed colour is ≤ `tolerance`. Caps total pixels
/// visited at `max_pixels` so a degenerate seed (e.g. the entire
/// uniform-grey screen background) can't OOM the test.
pub(super) fn flood_fill(
    img: &RgbImage,
    seed: (i32, i32),
    tolerance: u8,
    max_pixels: usize,
    mode: crate::session::ColorDistance,
) -> FloodResult {
    let (iw, ih) = img.dimensions();
    let (iw_i, ih_i) = (iw as i32, ih as i32);
    let idx = |x: i32, y: i32| (y as usize) * (iw as usize) + (x as usize);

    let mut visited = vec![false; (iw * ih) as usize];
    let mut q: VecDeque<(i32, i32)> = VecDeque::new();
    let seed_color = *img.get_pixel(seed.0 as u32, seed.1 as u32);
    let tol_sq = super::color::threshold_sq(tolerance, mode);

    visited[idx(seed.0, seed.1)] = true;
    q.push_back(seed);

    let (mut minx, mut miny, mut maxx, mut maxy) = (seed.0, seed.1, seed.0, seed.1);
    let mut sum_x: i64 = 0;
    let mut sum_y: i64 = 0;
    let mut count: usize = 0;

    while let Some((x, y)) = q.pop_front() {
        if count >= max_pixels {
            break;
        }
        count += 1;
        sum_x += x as i64;
        sum_y += y as i64;
        if x < minx {
            minx = x;
        }
        if x > maxx {
            maxx = x;
        }
        if y < miny {
            miny = y;
        }
        if y > maxy {
            maxy = y;
        }

        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let nx = x + dx;
            let ny = y + dy;
            if nx < 0 || ny < 0 || nx >= iw_i || ny >= ih_i {
                continue;
            }
            let id = idx(nx, ny);
            if visited[id] {
                continue;
            }
            let p = *img.get_pixel(nx as u32, ny as u32);
            if super::color::distance_sq(p, seed_color, mode) > tol_sq {
                continue;
            }
            visited[id] = true;
            q.push_back((nx, ny));
        }
    }

    // Guard the centroid against an empty pixel set (shouldn't happen
    // — the seed is always pushed — but div-by-zero would be ugly).
    let cx = if count == 0 {
        seed.0
    } else {
        (sum_x / count as i64) as i32
    };
    let cy = if count == 0 {
        seed.1
    } else {
        (sum_y / count as i64) as i32
    };

    let bbox = Rect {
        x: minx,
        y: miny,
        width: maxx - minx + 1,
        height: maxy - miny + 1,
    };
    // Post-flood corner test: did the four bbox corners' colours
    // fall inside the tolerance band? We can't just query `visited`
    // because the flood's max_pixels cap might have prevented it
    // from reaching the corners — so we sample colour directly,
    // which gives the same answer the BFS would have reached.
    let corners_inside = count_corners_inside(img, bbox, &seed_color, tolerance, mode);

    FloodResult {
        bbox,
        centroid: (cx, cy),
        pixel_count: count,
        corners_inside,
        visited,
        image_width: iw,
    }
}

fn count_corners_inside(
    img: &RgbImage,
    bbox: Rect,
    seed: &Rgb<u8>,
    tolerance: u8,
    mode: crate::session::ColorDistance,
) -> u8 {
    let (iw, ih) = img.dimensions();
    let tol_sq = super::color::threshold_sq(tolerance, mode);
    let corners = [
        (bbox.x, bbox.y),
        (bbox.x + bbox.width - 1, bbox.y),
        (bbox.x, bbox.y + bbox.height - 1),
        (bbox.x + bbox.width - 1, bbox.y + bbox.height - 1),
    ];
    let mut hits = 0u8;
    for (x, y) in corners {
        if x < 0 || y < 0 || x >= iw as i32 || y >= ih as i32 {
            continue;
        }
        let p = *img.get_pixel(x as u32, y as u32);
        if super::color::distance_sq(p, *seed, mode) <= tol_sq {
            hits += 1;
        }
    }
    hits
}

/// Classify a flood region from its fill ratio + corner test.
///
/// Thresholds come from geometry:
/// - Perfect rectangle: ratio 1.0, 4 corners inside.
/// - Perfect circle:    ratio π/4 ≈ 0.785, 0 corners inside.
/// - Typical Adw pill:  ratio 0.94–0.99, 0 corners inside (small
///   radius trims the corners off).
/// - Capsule (rounded ends, w >> h): ratio close to 0.96.
///
/// "Pill" covers the broad rounded-rectangle family (Adw rows,
/// modern button pills); "Ellipse" picks up the actually-round
/// shapes. Borderline cases fall to `Irregular` so callers don't
/// silently get the wrong classification.
fn classify_shape(
    bbox: Rect,
    pixel_count: usize,
    corners_inside: u8,
    tuning: VisualRegionTuning,
) -> Shape {
    let area = (bbox.width as f64) * (bbox.height as f64);
    if area <= 0.0 || pixel_count == 0 {
        return Shape::Irregular;
    }
    let ratio = pixel_count as f64 / area;
    let (ell_lo, ell_hi) = tuning.shape_ellipse_ratio_range;
    if corners_inside == 4 && ratio >= tuning.shape_rectangle_min_ratio {
        Shape::Rectangle
    } else if corners_inside == 0 && ratio >= ell_lo && ratio < ell_hi {
        Shape::Ellipse
    } else if corners_inside <= 1 && ratio >= tuning.shape_pill_min_ratio {
        // 0–1 corners inside catches subtle-corner-rounding cases
        // where one corner pixel happens to fall on a darker
        // antialiased pixel.
        Shape::Pill
    } else {
        Shape::Irregular
    }
}

/// Pick a seed pixel adjacent to `inner_bbox` (typically a text bbox)
/// but on what looks like uniform fill rather than glyph antialiasing.
/// Tries the four cardinal directions in order (right, left, below,
/// above); for each candidate, samples a neighbour pixel and accepts
/// the candidate only if the two are colour-close (rules out
/// antialiasing fringe). Falls back to the first in-bounds candidate
/// when none of them looks "uniform".
fn pick_seed_outside(
    inner: Rect,
    img: Rect,
    rgb: &RgbImage,
    tuning: VisualRegionTuning,
) -> Option<(i32, i32)> {
    let cx = inner.center_x();
    let cy = inner.center_y();
    let offset = 4;
    let candidates = [
        (inner.right() + offset, cy),
        (inner.x - offset, cy),
        (cx, inner.bottom() + offset),
        (cx, inner.y - offset),
    ];
    let in_image = |x: i32, y: i32| x >= 0 && y >= 0 && x < img.width && y < img.height;

    let mut fallback = None;
    for (x, y) in candidates {
        if !in_image(x, y) {
            continue;
        }
        if fallback.is_none() {
            fallback = Some((x, y));
        }
        // Sanity-check uniformity against a neighbour 2px further
        // out so we don't accidentally land on a thin border.
        let nx = (x + if x < inner.center_x() { -2 } else { 2 }).clamp(0, img.width - 1);
        let ny = (y + if y < inner.center_y() { -2 } else { 2 }).clamp(0, img.height - 1);
        let p1 = *rgb.get_pixel(x as u32, y as u32);
        let p2 = *rgb.get_pixel(nx as u32, ny as u32);
        // Uniformity threshold is interpreted as squared RGB units
        // regardless of `tuning.color_distance` — the seed-pick is a
        // very-local sanity check (1-px-out from a 4-px-away
        // candidate) where perceptual vs. raw RGB doesn't shift the
        // decision in any meaningful way.
        if super::color::distance_sq(p1, p2, crate::session::ColorDistance::Rgb)
            < tuning.seed_uniformity_threshold_sq as f64
        {
            return Some((x, y));
        }
    }
    fallback
}

/// Pixel just outside `bbox` on the cropped image. Prefers the right
/// edge (typical layout direction); falls back to bottom/left/top.
fn pixel_just_outside(bbox: Rect, img: Rect) -> Option<(i32, i32)> {
    let cx = bbox.center_x();
    let cy = bbox.center_y();
    let pad = 2;
    let candidates = [
        (bbox.right() + pad, cy),
        (cx, bbox.bottom() + pad),
        (bbox.x - pad, cy),
        (cx, bbox.y - pad),
    ];
    candidates
        .into_iter()
        .find(|&(x, y)| x >= 0 && y >= 0 && x < img.width && y < img.height)
}

// ── Unit tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a 100×100 image: grey card background, darker pill in the
    /// middle, white glyph rectangle inside the pill. Mirrors the
    /// dialog → button-pill → text-glyph nesting that real fixtures
    /// produce.
    fn synthetic_card_pill_glyph() -> RgbImage {
        let mut img = RgbImage::new(100, 100);
        // Card fill — light grey.
        for x in 0..100 {
            for y in 0..100 {
                img.put_pixel(x, y, Rgb([200, 200, 200]));
            }
        }
        // Pill — darker grey, 60×30 centred (x: 20..80, y: 35..65).
        for x in 20..80 {
            for y in 35..65 {
                img.put_pixel(x, y, Rgb([120, 120, 120]));
            }
        }
        // Glyph — white, 10×4 centred (x: 45..55, y: 48..52).
        for x in 45..55 {
            for y in 48..52 {
                img.put_pixel(x, y, Rgb([250, 250, 250]));
            }
        }
        img
    }

    #[test]
    fn flood_fill_grows_to_pill_boundary() {
        let img = synthetic_card_pill_glyph();
        // Seed inside the pill but outside the glyph: just left of glyph.
        let res = flood_fill(
            &img,
            (40, 50),
            24,
            100 * 100,
            crate::session::ColorDistance::Rgb,
        );
        // Pill bbox: x ∈ [20, 79], y ∈ [35, 64], so width=60, height=30.
        // Flood may include all pill pixels (it excludes glyph pixels
        // since they're outside tolerance against pill grey).
        assert_eq!(res.bbox.x, 20);
        assert_eq!(res.bbox.y, 35);
        assert_eq!(res.bbox.width, 60);
        assert_eq!(res.bbox.height, 30);
        // Centroid is roughly the pill centre.
        assert!((res.centroid.0 - 49).abs() <= 5, "cx={}", res.centroid.0);
        assert!((res.centroid.1 - 50).abs() <= 5, "cy={}", res.centroid.1);
    }

    #[test]
    fn synthetic_pill_classifies_as_rectangle() {
        // The synthetic image's "pill" is actually a perfectly filled
        // axis-aligned rectangle (no corner rounding), so it should
        // classify as Rectangle, not Pill. This sanity-checks the
        // classifier's "4 corners inside + ratio ≈ 1" branch.
        let img = synthetic_card_pill_glyph();
        let res = flood_fill(
            &img,
            (40, 50),
            24,
            100 * 100,
            crate::session::ColorDistance::Rgb,
        );
        assert_eq!(res.corners_inside, 4);
        let shape = classify_shape(
            res.bbox,
            res.pixel_count,
            res.corners_inside,
            VisualRegionTuning::default(),
        );
        assert_eq!(shape, Shape::Rectangle);
    }

    /// Build a 60×30 axis-aligned filled rectangle with 4-px rounded
    /// corners. Pill territory: 0 bbox corners inside the shape but a
    /// high fill ratio (≈ 0.96 for r=4 on a 60×30 box).
    fn synthetic_rounded_pill(corner_r: i32) -> RgbImage {
        let mut img = RgbImage::new(60, 30);
        // Background contrasts strongly with fill so corners
        // sampled outside the rounded shape don't match the seed.
        for x in 0..60 {
            for y in 0..30 {
                img.put_pixel(x, y, Rgb([255, 255, 255]));
            }
        }
        let fill = Rgb([100, 100, 100]);
        for x in 0..60 {
            for y in 0..30 {
                // Distance from nearest corner-centre. If we're in a
                // corner box and farther than r from its centre, leave
                // as background.
                let in_corner_box =
                    (x < corner_r || x >= 60 - corner_r) && (y < corner_r || y >= 30 - corner_r);
                if in_corner_box {
                    let (cx, cy) = (
                        if x < corner_r {
                            corner_r
                        } else {
                            60 - corner_r - 1
                        },
                        if y < corner_r {
                            corner_r
                        } else {
                            30 - corner_r - 1
                        },
                    );
                    let dx = x - cx;
                    let dy = y - cy;
                    if dx * dx + dy * dy > corner_r * corner_r {
                        continue;
                    }
                }
                img.put_pixel(x as u32, y as u32, fill);
            }
        }
        img
    }

    #[test]
    fn rounded_pill_classifies_as_pill() {
        let img = synthetic_rounded_pill(6);
        // Seed at the centre — definitely inside the fill.
        let res = flood_fill(
            &img,
            (30, 15),
            24,
            60 * 30,
            crate::session::ColorDistance::Rgb,
        );
        // All four bbox corners are background-coloured, so the
        // colour-based corner check should see 0 inside.
        assert_eq!(res.corners_inside, 0);
        let shape = classify_shape(
            res.bbox,
            res.pixel_count,
            res.corners_inside,
            VisualRegionTuning::default(),
        );
        assert_eq!(
            shape,
            Shape::Pill,
            "fill ratio = {} ({} / {})",
            res.pixel_count as f64 / (res.bbox.width * res.bbox.height) as f64,
            res.pixel_count,
            res.bbox.width * res.bbox.height
        );
    }

    /// Build a filled ellipse / circle approximation in a square box.
    fn synthetic_ellipse(diameter: i32) -> RgbImage {
        let d = diameter as u32;
        let mut img = RgbImage::new(d, d);
        for x in 0..d {
            for y in 0..d {
                img.put_pixel(x, y, Rgb([255, 255, 255]));
            }
        }
        let fill = Rgb([100, 100, 100]);
        let r = diameter as f64 / 2.0;
        let cx = r;
        let cy = r;
        for x in 0..d {
            for y in 0..d {
                let dx = x as f64 - cx;
                let dy = y as f64 - cy;
                if dx * dx + dy * dy <= r * r {
                    img.put_pixel(x, y, fill);
                }
            }
        }
        img
    }

    #[test]
    fn flood_from_arbitrary_point_inside_region_matches_centered_seed() {
        // Glyph is at x:45..55, y:48..52; pill is at x:20..80,
        // y:35..65; card is everything else. Seed two different
        // points inside the *pill fill* (avoiding the glyph) and
        // confirm the flood yields the same bbox/centroid/count —
        // the "any point inside the region works" property.
        let img = synthetic_card_pill_glyph();
        // Just below the glyph but still in the pill.
        let near_glyph = flood_fill(
            &img,
            (49, 60),
            24,
            100 * 100,
            crate::session::ColorDistance::Rgb,
        );
        // Top-left interior of the pill.
        let cornerish = flood_fill(
            &img,
            (22, 37),
            24,
            100 * 100,
            crate::session::ColorDistance::Rgb,
        );
        assert_eq!(near_glyph.bbox, cornerish.bbox);
        assert_eq!(near_glyph.centroid, cornerish.centroid);
        assert_eq!(near_glyph.pixel_count, cornerish.pixel_count);
    }

    #[test]
    fn flood_from_glyph_pixel_recovers_glyph_not_pill() {
        // Same image; seed *on the glyph* this time. Confirms that
        // "which region you get" is determined by which region the
        // seed pixel falls in — the corollary to the
        // arbitrary-point-inside-region test above.
        let img = synthetic_card_pill_glyph();
        let on_glyph = flood_fill(
            &img,
            (49, 49),
            24,
            100 * 100,
            crate::session::ColorDistance::Rgb,
        );
        // Glyph rect: x=45..54, y=48..51, so bbox is 10 wide × 4 tall.
        assert_eq!(on_glyph.bbox.x, 45);
        assert_eq!(on_glyph.bbox.y, 48);
        assert_eq!(on_glyph.bbox.width, 10);
        assert_eq!(on_glyph.bbox.height, 4);
    }

    #[test]
    fn circle_classifies_as_ellipse() {
        let img = synthetic_ellipse(40);
        let res = flood_fill(
            &img,
            (20, 20),
            24,
            40 * 40,
            crate::session::ColorDistance::Rgb,
        );
        // Circle corners are all in the background, so 0 inside.
        assert_eq!(res.corners_inside, 0);
        let shape = classify_shape(
            res.bbox,
            res.pixel_count,
            res.corners_inside,
            VisualRegionTuning::default(),
        );
        assert_eq!(shape, Shape::Ellipse);
    }

    #[test]
    fn flood_fill_grows_to_card_when_seeded_outside_pill() {
        let img = synthetic_card_pill_glyph();
        // Seed in the card area outside the pill (top-left corner).
        let res = flood_fill(
            &img,
            (5, 5),
            24,
            100 * 100,
            crate::session::ColorDistance::Rgb,
        );
        // Card minus pill: bbox covers the full 100×100 minus the
        // pill interior. The BBOX, however, is still 100×100 because
        // the flood wraps around the pill.
        assert_eq!(res.bbox.x, 0);
        assert_eq!(res.bbox.y, 0);
        assert_eq!(res.bbox.width, 100);
        assert_eq!(res.bbox.height, 100);
    }

    #[test]
    fn pick_seed_outside_chooses_uniform_neighbour() {
        let img = synthetic_card_pill_glyph();
        // Glyph rect (x: 45..55, y: 48..52) — inner is the text bbox.
        let inner = Rect {
            x: 45,
            y: 48,
            width: 10,
            height: 4,
        };
        let img_bounds = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let seed =
            pick_seed_outside(inner, img_bounds, &img, VisualRegionTuning::default()).unwrap();
        // Seed should land on the pill (grey 120) — i.e. between glyph
        // and pill boundary.
        let p = img.get_pixel(seed.0 as u32, seed.1 as u32);
        assert!(
            (p[0] as i32 - 120).abs() < 10,
            "seed at {seed:?} landed on colour {p:?} — expected pill grey ~120"
        );
    }
}
