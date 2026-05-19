//! Classical template matching — find a known reference image inside
//! a screenshot via normalized cross-correlation (NCC).
//!
//! Trade-off space against [`VisualLocator`](super::VisualLocator):
//!
//! - **`VisualLocator`** (OCR): finds text-bearing widgets by their
//!   on-screen label. Robust to theme changes, font hinting, DPI —
//!   the pipeline reads the text, not the pixels. Costs hundreds of
//!   ms (OCR engine + recognition).
//! - **`ImageLocator`** (template matching): finds *any* widget given
//!   a captured reference PNG. Works for icon-only buttons that have
//!   no text. Brittle: a theme swap, DPI bump, or antialias change
//!   invalidates the reference. Costs ~5–50 ms (one NCC pass).
//!
//! Use `find_image` when you've captured an icon you know is stable
//! across the runs you care about. Use `find_by_text` otherwise.
//!
//! ## What this is and isn't
//!
//! "Basic" template matching: convert target + template to grayscale,
//! slide the template over every position, score by NCC, take the
//! peak. **No** scale invariance, **no** rotation invariance, **no**
//! image-pyramid pre-filtering, **no** feature-based matching. If a
//! workload demonstrably needs any of those, it's a few hundred more
//! lines of `imageproc` to add — but the basic version covers the
//! "I captured a 32×32 icon and want to click it" case, which is
//! the only thing template matching does well in practice.

use std::sync::Arc;
use std::time::Duration;

use image::DynamicImage;
use imageproc::template_matching::{match_template, MatchTemplateMethod};

use crate::atspi::Rect;
use crate::backend::PointerButton;
use crate::error::{Error, Result};
use crate::session::Session;

/// Default NCC score above which a match is considered valid. Picked
/// empirically: 0.85 is tolerant of subpixel antialias differences
/// between capture and replay on the same machine; 0.95+ is needed
/// to reject false positives in busy layouts.
const DEFAULT_THRESHOLD: f32 = 0.85;

/// A pending template-matching query. Build with
/// [`Session::find_image`](crate::Session::find_image) or
/// [`Locator::find_image`](crate::Locator::find_image), then await
/// a terminal method ([`Self::bounds`], [`Self::click`], etc.).
///
/// The reference image is decoded once at construction; the
/// screenshot is taken anew on each call to a terminal method so
/// every match sees the current screen state.
#[derive(Clone)]
pub struct ImageLocator {
    pub(crate) session: Arc<Session>,
    /// Template, decoded once into RGB so each match doesn't pay
    /// PNG-decode cost. Wrapped in `Arc` so `Clone` is cheap.
    template_rgb: Arc<image::RgbImage>,
    /// Optional screen-coord region the search is restricted to —
    /// when set, the screenshot is cropped to this rect before
    /// matching and hits are returned in screen coords.
    region: Option<Rect>,
    /// NCC threshold; matches below this score are rejected.
    threshold: f32,
    /// Per-locator override of [`Session::default_timeout`].
    timeout: Option<Duration>,
}

impl std::fmt::Debug for ImageLocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (tw, th) = self.template_rgb.dimensions();
        f.debug_struct("ImageLocator")
            .field("kind", &"image-template")
            .field("template_size", &format!("{tw}x{th}"))
            .field("threshold", &self.threshold)
            .field("region", &self.region)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl ImageLocator {
    /// Decode the PNG bytes into an RGB template ready for matching.
    /// Errors if the bytes aren't a valid image.
    pub(crate) fn new(
        session: Arc<Session>,
        png_bytes: &[u8],
        region: Option<Rect>,
    ) -> Result<Self> {
        let img = image::load_from_memory(png_bytes)
            .map_err(|e| Error::visual(format!("decode template image: {e}")))?;
        let rgb = img.into_rgb8();
        let (w, h) = rgb.dimensions();
        if w == 0 || h == 0 {
            return Err(Error::visual("template image is empty"));
        }
        Ok(Self {
            session,
            template_rgb: Arc::new(rgb),
            region,
            threshold: DEFAULT_THRESHOLD,
            timeout: None,
        })
    }

    /// Tighten or loosen the NCC threshold (`[0.0, 1.0]`). Default
    /// `0.85`. Raise to reject more false positives; lower if a
    /// known-good match scores below it (typical reason: DPI or
    /// antialias differences from capture-time).
    pub fn with_threshold(mut self, threshold: f32) -> Self {
        self.threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// Per-call timeout override for the auto-wait loops on
    /// [`Self::bounds`] / [`Self::click`] etc.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Restrict the search to a screen-coord region. Hits' bboxes
    /// are still returned in screen coords.
    pub fn within(mut self, region: Rect) -> Self {
        self.region = Some(region);
        self
    }

    /// Take a screenshot, search for the template, and return every
    /// match position above [`Self::threshold`]. No auto-wait — the
    /// answer reflects the screen at call time.
    ///
    /// The returned `Vec` is sorted best-score-first. Multiple hits
    /// are non-maximum-suppressed: a peak suppresses any other peak
    /// closer than `min(template_w, template_h) / 2` to avoid
    /// reporting the same widget many times.
    pub async fn matches(&self) -> Result<Vec<Rect>> {
        let png = self.session.take_screenshot().await?;
        let region = self.region;
        let template = self.template_rgb.clone();
        let threshold = self.threshold;
        tokio::task::spawn_blocking(move || -> Result<Vec<Rect>> {
            let full = crate::locator::decode_screenshot_png(&png)
                .map_err(|e| Error::visual(format!("decode screenshot: {e}")))?;
            let (haystack_rgb, origin_x, origin_y) = if let Some(scope) = region {
                let cropped = crate::locator::crop_to_bounds(full, scope)
                    .map_err(|e| Error::visual(format!("crop to region: {e}")))?;
                (cropped.into_rgb8(), scope.x.max(0), scope.y.max(0))
            } else {
                (full.into_rgb8(), 0, 0)
            };
            find_template_matches(&haystack_rgb, &template, threshold, origin_x, origin_y)
        })
        .await
        .map_err(|e| Error::visual(format!("template-matching task panicked: {e}")))?
    }

    /// Number of matches above the threshold right now. No auto-wait.
    pub async fn count(&self) -> Result<usize> {
        Ok(self.matches().await?.len())
    }

    /// Bounding rectangle of the unique best match. Errors when no
    /// match scores above the threshold after auto-wait, or when
    /// multiple equally-good matches are found.
    pub async fn bounds(&self) -> Result<Rect> {
        let deadline = std::time::Instant::now()
            + self
                .timeout
                .unwrap_or_else(|| self.session.default_timeout());
        loop {
            let hits = self.matches().await?;
            match hits.len() {
                0 => {
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::ElementNotFound {
                            xpath: format!("image-template (threshold={})", self.threshold),
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                1 => return Ok(hits[0]),
                n => {
                    return Err(Error::visual(format!(
                        "found {n} image-template matches at threshold {}; \
                         scope with .within(rect) or raise the threshold to disambiguate",
                        self.threshold,
                    )));
                }
            }
        }
    }

    /// Wait until at least one match exists, or the timeout elapses.
    pub async fn wait_for_visible(&self) -> Result<()> {
        let deadline = std::time::Instant::now()
            + self
                .timeout
                .unwrap_or_else(|| self.session.default_timeout());
        loop {
            if !self.matches().await?.is_empty() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::ElementNotFound {
                    xpath: format!("image-template (threshold={})", self.threshold),
                });
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Click the centre of the unique best match. Uses the same
    /// cold-start workaround as [`VisualLocator::click`](super::VisualLocator::click).
    pub async fn click(&self) -> Result<()> {
        let r = self.bounds().await?;
        let (cx, cy) = (r.center_x() as f64, r.center_y() as f64);
        tracing::debug!(?r, cx, cy, "image: click");
        super::cold_start_click(&self.session, cx, cy).await
    }

    /// Move the pointer to the centre of the unique best match
    /// without clicking.
    pub async fn hover(&self) -> Result<()> {
        let r = self.bounds().await?;
        self.session
            .pointer_motion_absolute(r.center_x() as f64, r.center_y() as f64)
            .await?;
        Ok(())
    }
}

/// Run NCC template matching of `template` over `haystack`. Returns
/// all match rectangles whose score is at or above `threshold`,
/// sorted by score (best first), with simple non-maximum suppression
/// against overlapping peaks. Hit positions are translated to screen
/// coords by adding `(origin_x, origin_y)`.
///
/// Internal grayscale conversion: NCC on a single luminance channel
/// is what `imageproc` exposes today and is good enough for UI
/// targets (which are usually high-contrast against their background).
/// If a future workload needs per-channel matching, the call site
/// can be lifted to RGB without changing the public API.
fn find_template_matches(
    haystack_rgb: &image::RgbImage,
    template_rgb: &image::RgbImage,
    threshold: f32,
    origin_x: i32,
    origin_y: i32,
) -> Result<Vec<Rect>> {
    let (hw, hh) = haystack_rgb.dimensions();
    let (tw, th) = template_rgb.dimensions();
    if tw == 0 || th == 0 {
        return Err(Error::visual("template has zero width or height"));
    }
    if tw > hw || th > hh {
        return Err(Error::visual(format!(
            "template ({tw}x{th}) larger than search area ({hw}x{hh})"
        )));
    }

    let haystack_gray = DynamicImage::ImageRgb8(haystack_rgb.clone()).into_luma8();
    let template_gray = DynamicImage::ImageRgb8(template_rgb.clone()).into_luma8();

    // `CrossCorrelationNormalized` is in `[0, 1]` for normalized
    // non-negative input, peaks at 1.0 for a perfect match.
    let result = match_template(
        &haystack_gray,
        &template_gray,
        MatchTemplateMethod::CrossCorrelationNormalized,
    );

    // Walk the score grid manually so we can apply NMS across all
    // above-threshold peaks rather than just taking the global max.
    // Score grid is `(hw - tw + 1) x (hh - th + 1)`.
    let (rw, rh) = result.dimensions();
    let mut peaks: Vec<(f32, u32, u32)> = Vec::new();
    for y in 0..rh {
        for x in 0..rw {
            let score = result.get_pixel(x, y)[0];
            if score >= threshold {
                peaks.push((score, x, y));
            }
        }
    }
    // Best first.
    peaks.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    // Non-maximum suppression: drop any peak within half the
    // template's smaller dimension of an already-accepted peak.
    let min_dim = tw.min(th) as i32;
    let nms_radius = (min_dim / 2).max(1);
    let mut accepted: Vec<(u32, u32)> = Vec::new();
    let mut out: Vec<Rect> = Vec::new();
    for (_, x, y) in peaks {
        let (xi, yi) = (x as i32, y as i32);
        if accepted.iter().any(|&(ax, ay)| {
            (ax as i32 - xi).abs() <= nms_radius && (ay as i32 - yi).abs() <= nms_radius
        }) {
            continue;
        }
        accepted.push((x, y));
        out.push(Rect {
            x: xi + origin_x,
            y: yi + origin_y,
            width: tw as i32,
            height: th as i32,
        });
    }
    Ok(out)
}

// `cold_start_click` is exposed `pub(crate)` from `visual::mod`.

#[allow(unused_imports)]
use PointerButton as _; // kept for parity with neighbouring modules

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    fn solid(w: u32, h: u32, color: [u8; 3]) -> RgbImage {
        let mut img = RgbImage::new(w, h);
        for x in 0..w {
            for y in 0..h {
                img.put_pixel(x, y, Rgb(color));
            }
        }
        img
    }

    /// Embed `template` at `(x, y)` inside a fresh noisy haystack.
    /// The background is a deterministic pseudo-noise pattern so NCC
    /// only scores high where the *structure* matches (a uniform
    /// background would correlate ambiguously with any template
    /// because NCC measures angle in vector space, not magnitude).
    fn embed(haystack_w: u32, haystack_h: u32, template: &RgbImage, x: u32, y: u32) -> RgbImage {
        let mut hay = RgbImage::new(haystack_w, haystack_h);
        for py in 0..haystack_h {
            for px in 0..haystack_w {
                // Deterministic pseudo-noise pattern that breaks the
                // "flat region correlates with anything" trap.
                let v = ((px.wrapping_mul(73) ^ py.wrapping_mul(31)) & 0xff) as u8;
                hay.put_pixel(px, py, Rgb([v, v.wrapping_add(40), v.wrapping_add(80)]));
            }
        }
        let (tw, th) = template.dimensions();
        for dy in 0..th {
            for dx in 0..tw {
                hay.put_pixel(x + dx, y + dy, *template.get_pixel(dx, dy));
            }
        }
        hay
    }

    #[test]
    fn finds_exact_template_at_known_position() {
        let mut template = solid(20, 10, [50, 100, 200]);
        // Add some internal structure so NCC isn't matching a flat
        // region against a flat region (which scores high
        // everywhere).
        for x in 0..20 {
            template.put_pixel(x, 4, Rgb([255, 255, 255]));
        }
        let haystack = embed(200, 100, &template, 73, 41);
        let hits = find_template_matches(&haystack, &template, 0.95, 0, 0).expect("matching ok");
        assert_eq!(hits.len(), 1, "expected exactly one peak above threshold");
        let r = hits[0];
        assert_eq!(r.x, 73);
        assert_eq!(r.y, 41);
        assert_eq!(r.width, 20);
        assert_eq!(r.height, 10);
    }

    #[test]
    fn returns_empty_when_template_not_present() {
        let mut template = solid(20, 10, [50, 100, 200]);
        for x in 0..20 {
            template.put_pixel(x, 4, Rgb([255, 255, 255]));
        }
        // Haystack is a totally different colour with no structure.
        let haystack = solid(200, 100, [10, 10, 10]);
        let hits = find_template_matches(&haystack, &template, 0.95, 0, 0).expect("matching ok");
        assert!(hits.is_empty(), "expected no peaks; got {hits:?}");
    }

    #[test]
    fn screen_coord_translation_via_origin() {
        let mut template = solid(8, 8, [200, 50, 50]);
        for i in 0..8 {
            template.put_pixel(i, i, Rgb([255, 255, 255]));
        }
        // Embed at (20, 30) inside a cropped haystack; origin
        // claims the crop started at screen (500, 600).
        let haystack = embed(100, 100, &template, 20, 30);
        let hits =
            find_template_matches(&haystack, &template, 0.95, 500, 600).expect("matching ok");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].x, 520);
        assert_eq!(hits[0].y, 630);
    }

    #[test]
    fn errors_when_template_larger_than_haystack() {
        let template = solid(40, 40, [50, 100, 200]);
        let haystack = solid(20, 20, [50, 100, 200]);
        let err = find_template_matches(&haystack, &template, 0.5, 0, 0).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("larger than search area"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn nms_suppresses_neighbouring_peaks() {
        // Construct a haystack with the template embedded at one
        // location. The score map peaks at the exact position but
        // also has slightly-lower side-lobes within ±a-few-pixels.
        // NMS should collapse those into a single reported hit.
        let mut template = solid(20, 20, [80, 80, 80]);
        for i in 0..20 {
            template.put_pixel(i, 10, Rgb([255, 255, 255]));
        }
        let haystack = embed(200, 200, &template, 50, 50);
        // High threshold filters out the pseudo-noise structural
        // false-positives; the only above-0.95 peaks live near the
        // real embedding at (50, 50).
        let hits = find_template_matches(&haystack, &template, 0.95, 0, 0).expect("matching ok");
        assert_eq!(
            hits.len(),
            1,
            "NMS should collapse near-peaks; got {hits:?}"
        );
        assert_eq!(hits[0].x, 50);
        assert_eq!(hits[0].y, 50);
    }
}
