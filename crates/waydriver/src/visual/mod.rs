//! OCR-backed visual locator. Opt-in escape hatch for finding widgets
//! that are drawn on screen but absent from the AT-SPI tree.
//!
//! When you reach for this module you've already decided that AT-SPI
//! can't see your widget — typically because the toolkit has a
//! lazy-realization bug (libadwaita's hidden-then-shown `PreferencesGroup`
//! inside an `AdwPreferencesPage` is the motivating example) or because
//! the widget renders text without a corresponding accessible. Don't
//! reach for this when AT-SPI works; OCR is two orders of magnitude
//! slower (hundreds of milliseconds per resolve) and offers strictly
//! less metadata than the AT-SPI snapshot.
//!
//! ## Cost
//!
//! - First call per session, no pre-warmup: ~1–2 s engine load +
//!   ~200–500 ms inference.
//! - First call per session, with pre-warmup
//!   ([`SessionConfig::prewarm_visual`](crate::SessionConfig)): the
//!   prewarm task runs in the background during session start, so by
//!   the time the first `find_by_text` is issued the engine is usually
//!   ready. If it isn't, the call awaits the prewarm — never duplicates
//!   the load work.
//! - First-ever call on a machine with no XDG cache: an additional
//!   ~5–20 s for the one-time ocrs model download (~50 MB).
//! - Each subsequent call: ~200–500 ms (screenshot + inference).
//!
//! **Build profile matters enormously.** The figures above assume an
//! *optimized* build. OCR cost is dominated by rten inference, which is
//! roughly **30× slower at the dev profile's opt-level 0** — measured
//! ~5–8 s/full-frame pass optimized vs ~50–200 s unoptimized on CPU-only
//! hosts. `cargo test` consumers should add a dependency-only override to
//! the **workspace root** `Cargo.toml` (Cargo ignores profile overrides
//! elsewhere):
//!
//! ```toml
//! [profile.dev.package."*"]
//! opt-level = 3
//! ```
//!
//! The engine loader logs a warning when it detects a debug build.
//! Scoping also matters: a [`crate::Locator::find_by_text`]-style scoped
//! search crops the frame to the scope *before* inference, so it both
//! recognizes better and pays for fewer text lines; an unscoped
//! [`Session::find_by_text`] OCRs the entire frame. Repeated lookups on an
//! unchanged frame reuse one OCR pass via the per-frame cache.
//!
//! ## Action surface
//!
//! [`VisualLocator`] supports `count`, `bounds`, `click`, `hover`,
//! `wait_for_exists`. It deliberately does *not* implement `fill`,
//! `set_text`, `focus`, or any `is_<state>` predicate — those require
//! AT-SPI handles, and faking them visually would mask real bugs.

mod color;
mod engine;
mod models;
mod region;
mod template;

use std::sync::Arc;
use std::time::Duration;

use crate::atspi::Rect;
use crate::backend::PointerButton;
use crate::error::{Error, Result};
use crate::session::{Session, VisualTextTuning};

/// Cold-start pointer click sequence shared by `VisualLocator::click`
/// and `RegionLocator::click`. Reads
/// [`VisualClickTuning`](crate::session::VisualClickTuning) from the
/// session: when the warmup is enabled, sends a warmup motion at
/// `cold_start_warmup_offset_px` pixels from the target, settles, then
/// motions to `(cx, cy)` and presses+releases the primary button with
/// per-step sleeps. When the warmup is disabled, falls through to a
/// single motion + button-press, no sleeps.
pub(crate) async fn cold_start_click(session: &Arc<Session>, cx: f64, cy: f64) -> Result<()> {
    // Delegates to the shared, button-generic recipe on `Session`
    // (`Locator::pointer_click` uses the same one). Visual clicks are always
    // the primary button.
    session.cold_start_click(cx, cy, PointerButton::Left).await
}

pub(crate) use engine::{ensure_engine, EngineResult};
pub use region::{RegionLocator, Shape};
pub use template::ImageLocator;

// Re-export the region entry points under stable internal names so
// `Locator` can call them without breaking `region`'s pub(crate)
// boundary. The double-underscore signals "implementation detail, not
// part of the public crate API."
pub(crate) use region::last_region_only as __region_last_only;
pub(crate) use region::region_at_seed as __region_at_seed;
pub(crate) use region::sweep_regions as __region_sweep;

// Locator::list_text and Locator::list_labelled_regions reach into
// the visual module through these re-exports.
pub(crate) use list_labelled_regions as __list_labelled_regions;
pub(crate) use list_text as __list_text;
// Session::recognized_text reaches in through this one.
pub(crate) use recognized_text as __recognized_text;

/// A recognised text line on screen — what
/// [`Locator::list_text`](crate::Locator::list_text) returns for each
/// OCR-detected text. The `bounds` rectangle is the union bbox of all
/// words on the line in screen coordinates.
#[derive(Clone, Debug)]
pub struct TextHit {
    /// Full line text, with words joined by spaces. Mirrors what the
    /// `VisualLocator` matcher searches against.
    pub text: String,
    /// Axis-aligned bounding rectangle covering every recognised word
    /// on this line, in screen coordinates.
    pub bounds: Rect,
}

/// How [`VisualLocator`] matches OCR-recognized words against the search
/// text.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    /// Case-insensitive substring. The default — matches OCR's noise
    /// tolerance (homographs, accent stripping) better than `Exact`.
    #[default]
    Substring,
    /// Case-sensitive equality on the full recognized word.
    Exact,
    /// Like [`Substring`](Self::Substring) but tolerant of a few OCR
    /// character errors: a window of recognised words matches when its text
    /// is within a small normalized edit distance of the needle. Use this
    /// when small/low-contrast labels mis-read by a glyph or two (e.g. OCR
    /// reads "Cursor" as "Cursar", "hover-target" as "hover-targel") so an
    /// exact substring search returns nothing. Slightly looser, so prefer
    /// `Substring` when the text reads cleanly.
    Fuzzy,
}

/// Visual (OCR-backed) locator. See [module docs](self) for cost and
/// when to use this vs the AT-SPI [`Locator`](crate::Locator).
///
/// A `VisualLocator` represents *recognised on-screen text* — a label.
/// It always resolves to the bounding rectangle of the matched text
/// glyphs, not the surrounding widget. To address the widget itself
/// (the button pill, row, card frame around the label), call
/// [`parent_region`](Self::parent_region) or one of the region methods
/// on the AT-SPI parent locator.
#[derive(Clone)]
pub struct VisualLocator {
    session: Arc<Session>,
    text: String,
    region: Option<Rect>,
    timeout: Option<Duration>,
    match_mode: MatchMode,
}

impl std::fmt::Debug for VisualLocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisualLocator")
            .field("kind", &"text-label")
            .field("text", &self.text)
            .field("match_mode", &self.match_mode)
            .field("region", &self.region)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Auto-wait default for visual (OCR) locators when no per-locator timeout is
/// set. Deliberately *higher* than the AT-SPI locator default: one OCR pass is
/// tens of seconds on CPU (no GPU), so a 5s-style default would time out before
/// a single pass could finish. Still a hard, overridable bound — set a longer
/// `with_timeout(...)` on genuinely slow hardware, or a short one to fail fast.
const VISUAL_DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

impl VisualLocator {
    pub(crate) fn new(session: Arc<Session>, text: impl Into<String>) -> Self {
        Self {
            session,
            text: text.into(),
            region: None,
            timeout: None,
            match_mode: MatchMode::default(),
        }
    }

    /// Scope subsequent matches to a screen-relative rectangle. Use when
    /// a freeform text match would be ambiguous — e.g. find the parent
    /// dialog via AT-SPI, read its `bounds()`, then search visually inside.
    pub fn within(&self, region: Rect) -> VisualLocator {
        VisualLocator {
            region: Some(region),
            ..self.clone()
        }
    }

    /// Override the auto-wait timeout for this locator.
    pub fn with_timeout(&self, timeout: Duration) -> VisualLocator {
        VisualLocator {
            timeout: Some(timeout),
            ..self.clone()
        }
    }

    /// Switch the text-match strategy. See [`MatchMode`].
    pub fn with_match_mode(&self, mode: MatchMode) -> VisualLocator {
        VisualLocator {
            match_mode: mode,
            ..self.clone()
        }
    }

    /// The text this locator is searching for. A `VisualLocator`
    /// represents a recognised on-screen label, so this is the
    /// label's expected content (or substring, depending on
    /// [`match_mode`](Self::match_mode)).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The parent scope the OCR search is constrained to, if any.
    /// `Some(rect)` when constructed via
    /// [`Locator::find_by_text`](crate::Locator::find_by_text) or
    /// `Session::find_by_text(...).within(rect)`; `None` for a
    /// full-screen search.
    pub fn region(&self) -> Option<Rect> {
        self.region
    }

    /// Current text-matching strategy.
    pub fn match_mode(&self) -> MatchMode {
        self.match_mode
    }

    /// One screenshot + OCR pass, returning every bbox that matches
    /// the search text.
    ///
    /// When `self.region` is set, the screenshot is cropped to that
    /// rectangle *before* it reaches ocrs. Two wins:
    ///
    /// 1. **Speed.** ocrs's detection + recognition runtime scales
    ///    roughly with image area; cropping to a parent's bbox can cut
    ///    a typical scoped search from ~300 ms to ~50 ms.
    /// 2. **Accuracy.** Less surrounding text means fewer false
    ///    positives and less context that confuses the recognition
    ///    head (small / low-contrast labels misread more often when
    ///    crowded by other text).
    ///
    /// Hit bboxes are translated back to screen coordinates so the
    /// caller doesn't need to know about the crop.
    async fn matches(&self) -> Result<Vec<Rect>> {
        let needle = self.text.clone();
        let mode = self.match_mode;
        let ocr = ocr_lines(
            &self.session,
            self.region,
            self.session.take_screenshot().await?,
        )
        .await?;
        let mut out = Vec::new();

        match mode {
            // Per-line: an `Exact` match is "this OCR line equals
            // the needle in full". Cross-line concatenation would
            // make Exact almost never match (the screen-joined
            // text rarely equals any reasonable test selector), so
            // we keep Exact strictly line-scoped.
            MatchMode::Exact => {
                for line in &ocr.lines {
                    for (m_start, m_end) in find_matches(&line.joined, &needle, MatchMode::Exact) {
                        if let Some(rect) =
                            union_bbox_for_match(&line.words, &line.spans, m_start, m_end)
                        {
                            if let Some(scope) = self.region {
                                if !rect.is_inside(&scope) {
                                    continue;
                                }
                            }
                            out.push(rect);
                        }
                    }
                }
            }
            // Substring / Fuzzy: group lines into multi-line blocks via
            // geometric clustering (y-gap + x-overlap) plus pixel-level
            // boundary checks, then search within each block. Both modes
            // share this flow; they differ only inside `find_matches`
            // (exact vs edit-distance-tolerant), so `mode` is threaded
            // through below.
            MatchMode::Substring | MatchMode::Fuzzy => {
                let boundary = BoundaryContext {
                    image: &ocr.image,
                    crop_origin: ocr.crop_origin,
                };
                let blocks = group_lines_into_blocks(
                    ocr.lines.clone(),
                    self.session.visual_text_tuning,
                    Some(boundary),
                );
                for block in &blocks {
                    // Try every joiner-choice variant of the block.
                    // For a 2-line block with words `["nee", "dle"]`
                    // this is `"nee dle"` and `"needle"`; the same
                    // query matches either.
                    //
                    // Hits with the same screen-coord bbox are
                    // de-duplicated below — variants can produce the
                    // same match position (e.g. when the seam doesn't
                    // sit inside the query).
                    let variants = block_haystack_variants(block);
                    for variant in &variants {
                        for (m_start, m_end) in find_matches(&variant.joined, &needle, mode) {
                            if let Some(rect) =
                                union_bbox_for_match(&block.words, &variant.spans, m_start, m_end)
                            {
                                if let Some(scope) = self.region {
                                    if !rect.is_inside(&scope) {
                                        continue;
                                    }
                                }
                                if !out.contains(&rect) {
                                    out.push(rect);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// Number of OCR-matched words right now. No auto-wait.
    pub async fn count(&self) -> Result<usize> {
        Ok(self.matches().await?.len())
    }

    /// Effective auto-wait budget: the per-locator override, else the
    /// visual-specific default — never the (short) AT-SPI session default,
    /// which would time out before one OCR pass.
    fn effective_timeout(&self) -> Duration {
        self.timeout.unwrap_or(VISUAL_DEFAULT_TIMEOUT)
    }

    fn timeout_err(&self) -> Error {
        Error::Timeout(format!(
            "visual: no match for {:?} within {}ms",
            self.text,
            self.effective_timeout().as_millis()
        ))
    }

    /// One OCR pass, bounded by the time left until `deadline` *and* the
    /// session cancellation token. This is the load-bearing guarantee for the
    /// "no 20-minute wait" rule: a single OCR pass is tens of seconds, so
    /// without this the deadline (checked only between passes) wouldn't bound
    /// it. The background OCR thread still runs to completion, but the caller
    /// is released at the deadline (or immediately on `kill`).
    async fn matches_bounded(&self, deadline: std::time::Instant) -> Result<Vec<Rect>> {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(self.timeout_err());
        }
        tokio::select! {
            biased;
            _ = self.session.cancellation_token().cancelled() => Err(Error::Cancelled),
            r = tokio::time::timeout(remaining, self.matches()) => match r {
                Ok(res) => res,
                Err(_) => Err(self.timeout_err()),
            }
        }
    }

    /// Sleep `d`, but wake immediately (as `Cancelled`) if the session is
    /// killed, so a long auto-wait doesn't ignore a kill between OCR passes.
    async fn sleep_or_cancel(&self, d: Duration) -> Result<()> {
        tokio::select! {
            _ = self.session.cancellation_token().cancelled() => Err(Error::Cancelled),
            _ = tokio::time::sleep(d) => Ok(()),
        }
    }

    /// Bounding rectangle of the unique match. Errors when zero matches
    /// after auto-wait (`Timeout`), or when multiple matches found.
    pub async fn bounds(&self) -> Result<Rect> {
        let deadline = std::time::Instant::now() + self.effective_timeout();
        loop {
            let hits = self.matches_bounded(deadline).await?;
            match hits.len() {
                0 => {
                    if std::time::Instant::now() >= deadline {
                        return Err(self.timeout_err());
                    }
                    self.sleep_or_cancel(Duration::from_millis(200)).await?;
                }
                1 => return Ok(hits[0]),
                n => {
                    return Err(Error::visual(format!(
                        "found {n} visual matches for {:?}; scope with .within(rect) \
                         or use a tighter MatchMode",
                        self.text,
                    )));
                }
            }
        }
    }

    /// Wait until at least one match exists, or the timeout elapses
    /// (`Timeout`). Bounded by the cancellation token, so a `kill` interrupts
    /// even a long OCR pass.
    pub async fn wait_for_exists(&self) -> Result<()> {
        let deadline = std::time::Instant::now() + self.effective_timeout();
        loop {
            if !self.matches_bounded(deadline).await?.is_empty() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(self.timeout_err());
            }
            self.sleep_or_cancel(Duration::from_millis(200)).await?;
        }
    }

    /// Move the pointer to the matched word's centre and click.
    ///
    /// Headless mutter has a documented cold-start pointer-routing
    /// race: the first `pointer_motion_absolute` after a fresh
    /// session start can be delivered before the compositor has bound
    /// pointer focus to the target surface, so a single motion+click
    /// pair silently does nothing. (See `session.rs`'s pointer-prime
    /// comment — the keyboard side gets a `Shift_L` warmup at
    /// session start, but no single pointer call reliably wakes up
    /// pointer routing.) Sending two motion calls a short distance
    /// apart, with settles, reliably exits that state: the first
    /// motion is the one that gets eaten by focus-binding; the
    /// second lands cleanly.
    pub async fn click(&self) -> Result<()> {
        let r = self.bounds().await?;
        let (cx, cy) = (r.center_x() as f64, r.center_y() as f64);
        tracing::debug!(text = %self.text, cx, cy, bbox = ?r, "visual: click");
        cold_start_click(&self.session, cx, cy).await
    }

    /// Move the pointer to the matched word's centre without clicking.
    pub async fn hover(&self) -> Result<()> {
        let r = self.bounds().await?;
        self.session
            .pointer_motion_absolute(r.center_x() as f64, r.center_y() as f64)
            .await?;
        Ok(())
    }

    /// Convenience: the immediately-enclosing region around this OCR
    /// match — equivalent to `parent.last_region(self)` (one flood-fill
    /// from the OCR seed, no chain walk). Requires the locator to have
    /// been constructed with a parent scope (i.e. via
    /// [`Locator::find_by_text`](crate::Locator::find_by_text) or
    /// `Session::find_by_text(...).within(rect)`).
    pub async fn parent_region(&self) -> Result<RegionLocator> {
        let parent_bounds = self.region.ok_or_else(|| {
            Error::visual(
                "parent_region: VisualLocator has no parent scope. Construct it via \
                 Locator::find_by_text(...) or Session::find_by_text(...).within(rect).",
            )
        })?;
        let inner_bbox = self.bounds().await?;
        let png = self.session.take_screenshot().await?;
        region::last_region_only(
            &self.session,
            parent_bounds,
            inner_bbox,
            &png,
            self.session.visual_region_tuning,
        )
    }
}

/// One OCR'd line: the joined text, the union bbox of all words on
/// it, and the per-word breakdown the matcher needs to compute
/// substring hits at word granularity.
#[derive(Debug, Clone)]
struct OcrLine {
    /// Words joined by spaces — what the matcher searches against
    /// and what [`Locator::list_text`](crate::Locator::list_text)
    /// exposes as the line's text.
    joined: String,
    /// Axis-aligned bounding rectangle covering every word on this
    /// line, in screen coordinates.
    bbox: Rect,
    /// Per-word `(text, screen-coord rect)`. The matcher uses these
    /// to compute the union bbox for a substring hit that spans
    /// multiple words.
    words: Vec<(String, Rect)>,
    /// Per-word `(start, end)` character offsets in `joined`. Lets
    /// the matcher map a substring hit back to a word range.
    spans: Vec<(usize, usize)>,
}

/// A geometric cluster of consecutive OCR lines that visually form
/// a single multi-line label (a wrapped paragraph, a stacked button
/// label, etc.). Built by [`group_lines_into_blocks`] from a flat
/// `Vec<OcrLine>` using y-gap + x-overlap heuristics tuned by
/// [`VisualTextTuning`](crate::VisualTextTuning).
///
/// All words from every line in the block are flattened into a
/// single search string (lines joined with spaces), so a substring
/// search across the joined string finds matches that span lines.
#[derive(Debug)]
struct OcrBlock {
    /// All words across all lines in the block, joined with spaces.
    /// Newlines from the original layout are not preserved here —
    /// the join treats a wrapped paragraph the way a test author
    /// would write the label, as one space-separated phrase.
    ///
    /// This is the "all-spaces" variant — see
    /// [`block_haystack_variants`] for the alternatives generated
    /// when a query might span a wrapped/hyphenated word.
    joined: String,
    /// Axis-aligned bounding rectangle covering every word in the
    /// block, in screen coordinates. Includes vertical gaps between
    /// the rows.
    bbox: Rect,
    /// Per-word `(text, screen-coord rect)` in reading order
    /// (top-to-bottom, left-to-right within each row).
    words: Vec<(String, Rect)>,
    /// Per-word `(start, end)` character offsets in `joined`.
    spans: Vec<(usize, usize)>,
    /// Word indices at which each line (after the first) begins.
    /// E.g. `[3, 5]` means line 1 starts at `words[3]`, line 2 at
    /// `words[5]`. Used by [`block_haystack_variants`] to know
    /// where to switch the joiner between `" "` and `""`.
    line_break_word_indices: Vec<usize>,
}

/// Image + crop origin needed for pixel-level boundary checks
/// during block grouping. Built from `OcrResult` at call sites that
/// have a real screenshot in hand. Unit tests that exercise the
/// geometric path in isolation pass `None`.
struct BoundaryContext<'a> {
    image: &'a image::RgbImage,
    crop_origin: (i32, i32),
}

/// Sample a pixel in image (crop) coordinates from a screen-coord
/// position. Returns `None` if the position falls outside the
/// image bounds.
fn sample_pixel_at_screen(
    ctx: &BoundaryContext<'_>,
    screen_x: i32,
    screen_y: i32,
) -> Option<image::Rgb<u8>> {
    let crop_x = screen_x - ctx.crop_origin.0;
    let crop_y = screen_y - ctx.crop_origin.1;
    if crop_x < 0 || crop_y < 0 {
        return None;
    }
    let (w, h) = ctx.image.dimensions();
    if crop_x >= w as i32 || crop_y >= h as i32 {
        return None;
    }
    Some(*ctx.image.get_pixel(crop_x as u32, crop_y as u32))
}

/// Boundary-check predicate: should we let `prev` and `next` merge
/// into the same block given the image evidence? Returns `false`
/// when a visual boundary (background-colour change or divider
/// stripe) sits in the gap between them.
fn merge_passes_boundary_check(
    prev: &OcrLine,
    next: &OcrLine,
    ctx: &BoundaryContext<'_>,
    tuning: VisualTextTuning,
) -> bool {
    let mode = tuning.color_distance;
    let tol_sq = color::threshold_sq(tuning.background_color_tolerance, mode);

    // The x-overlap range (with slack) is where we sample
    // backgrounds and scan for dividers. Use the strict overlap
    // here (no slack) so the samples land inside both rows.
    let overlap_left = prev.bbox.x.max(next.bbox.x);
    let overlap_right = (prev.bbox.x + prev.bbox.width).min(next.bbox.x + next.bbox.width);
    if overlap_right <= overlap_left {
        // Should be impossible if the geometric check passed, but
        // bail out cleanly rather than panic.
        return true;
    }
    let mid_x = (overlap_left + overlap_right) / 2;
    let prev_bottom = prev.bbox.y + prev.bbox.height;
    let next_top = next.bbox.y;
    if next_top <= prev_bottom {
        // No gap to sample. Geometric merge would already have
        // happened; let it through.
        return true;
    }

    // 1. Background-colour change: sample a small averaged window
    //    just below prev and just above next at the x-overlap
    //    midpoint. Averaging smooths over single antialias-fringe
    //    pixels that would skew a one-pixel read.
    let top_sample = sample_background_at_screen(ctx, mid_x, prev_bottom, tuning);
    let bot_sample = sample_background_at_screen(ctx, mid_x, next_top - 1, tuning);
    if let (Some(top), Some(bot)) = (top_sample, bot_sample) {
        if color::distance_sq(top, bot, mode) > tol_sq {
            return false;
        }

        // 2. Divider scan (horizontal + vertical). Use both bg
        //    samples as the "this is normal background" baseline:
        //    a divider pixel must differ from BOTH to count. Each
        //    divider-scan sample reads a single pixel (averaging
        //    here would blur a thin divider into the baseline).
        if tuning.divider_detection_enabled {
            let samples_per_axis = tuning.boundary_samples_per_axis.max(1);
            let majority_threshold = tuning.boundary_majority_threshold;

            // Horizontal scan: each gap row, sample columns.
            for row_y in prev_bottom..next_top {
                let mut differing = 0usize;
                let mut sampled = 0usize;
                for i in 0..samples_per_axis {
                    // Evenly spaced x positions across the overlap.
                    let x = overlap_left
                        + ((overlap_right - overlap_left) as usize * i / samples_per_axis) as i32;
                    if let Some(p) = sample_pixel_at_screen(ctx, x, row_y) {
                        sampled += 1;
                        if color::distance_sq(p, top, mode) > tol_sq
                            && color::distance_sq(p, bot, mode) > tol_sq
                        {
                            differing += 1;
                        }
                    }
                }
                if sampled > 0 && (differing as f32) / (sampled as f32) >= majority_threshold {
                    return false; // horizontal divider row
                }
            }

            // Vertical scan: each column in the x-overlap range,
            // sample rows in the gap. Iterating every column (not
            // a subsample) means a 1-px divider can't slip between
            // samples — important because divider stripes in real
            // UIs are often just 1–2 px wide.
            let gap_height = next_top - prev_bottom;
            for col_x in overlap_left..overlap_right {
                let mut differing = 0usize;
                let mut sampled = 0usize;
                for i_y in 0..samples_per_axis {
                    let y = prev_bottom + (gap_height as usize * i_y / samples_per_axis) as i32;
                    if let Some(p) = sample_pixel_at_screen(ctx, col_x, y) {
                        sampled += 1;
                        if color::distance_sq(p, top, mode) > tol_sq
                            && color::distance_sq(p, bot, mode) > tol_sq
                        {
                            differing += 1;
                        }
                    }
                }
                if sampled > 0 && (differing as f32) / (sampled as f32) >= majority_threshold {
                    return false; // vertical divider column
                }
            }
        }
    }

    // 3. Connectivity check (opt-in, default off): bounded flood
    //    from below-prev's bg. If the flood can't reach above-next's
    //    bg, the two lines are visually separated by some feature
    //    the colour and divider checks missed (a thin border boxing
    //    each card, for example). Run as the last check because it's
    //    the most expensive.
    if tuning.connectivity_check_enabled {
        if let (Some(top), Some(bot)) = (
            sample_pixel_at_screen(ctx, mid_x, prev_bottom),
            sample_pixel_at_screen(ctx, mid_x, next_top - 1),
        ) {
            if !connectivity_passes_check(ctx, mid_x, prev_bottom, next_top - 1, top, bot, tuning) {
                return false;
            }
        }
    }

    true
}

/// Bounded BFS in the gap between two candidate-merge lines. Seeds
/// at the prev-line's bottom-bg pixel (in crop coords) and accepts
/// the merge only if the flood reaches the next-line's top-bg pixel
/// before hitting `max_connectivity_pixels`.
///
/// "Reaches" is by 4-connected adjacency on pixels colour-close
/// (under the tuning's `color_distance` mode) to the seed.
///
/// `screen_top_bottom_y` is the prev-line's bottom row in screen
/// coordinates; `screen_next_top_y` is the next-line's top row
/// minus 1 (one row above next).
fn connectivity_passes_check(
    ctx: &BoundaryContext<'_>,
    screen_x: i32,
    screen_top_bottom_y: i32,
    screen_next_top_y: i32,
    seed_color: image::Rgb<u8>,
    target_color: image::Rgb<u8>,
    tuning: VisualTextTuning,
) -> bool {
    let _ = target_color; // reserved for future use as a "must end on bg colour" gate
    let crop_x = screen_x - ctx.crop_origin.0;
    let crop_seed_y = screen_top_bottom_y - ctx.crop_origin.1;
    let crop_target_y = screen_next_top_y - ctx.crop_origin.1;
    let (iw, ih) = ctx.image.dimensions();
    if crop_x < 0
        || crop_seed_y < 0
        || crop_target_y < 0
        || crop_x >= iw as i32
        || crop_seed_y >= ih as i32
        || crop_target_y >= ih as i32
    {
        return true; // out of crop bounds, can't decide — let it through
    }
    let flood = region::flood_fill(
        ctx.image,
        (crop_x, crop_seed_y),
        tuning.background_color_tolerance,
        tuning.max_connectivity_pixels,
        tuning.color_distance,
    );
    let target_idx = (crop_target_y as usize) * (flood.image_width as usize) + (crop_x as usize);
    let _ = seed_color;
    if target_idx < flood.visited.len() && flood.visited[target_idx] {
        return true; // flood reached the target — same connective region
    }
    // Flood ran out (capped or exhausted) before reaching target.
    false
}

/// Sample an averaged background colour at a screen-coord position.
/// Uses [`color::sample_window`] with the tuning's
/// `background_sample_radius` (0 ⇒ single pixel, matching the
/// pre-averaging behaviour).
fn sample_background_at_screen(
    ctx: &BoundaryContext<'_>,
    screen_x: i32,
    screen_y: i32,
    tuning: VisualTextTuning,
) -> Option<image::Rgb<u8>> {
    let crop_x = screen_x - ctx.crop_origin.0;
    let crop_y = screen_y - ctx.crop_origin.1;
    color::sample_window(ctx.image, crop_x, crop_y, tuning.background_sample_radius)
}

/// Group OCR lines into multi-line blocks via geometric clustering
/// plus optional pixel-level boundary checks.
///
/// Two consecutive lines join the same block when all of:
/// - **Y-gap is small**: gap between the upper line's bottom and the
///   lower line's top is at most `tuning.multiline_max_gap_factor`
///   times the upper line's height.
/// - **X-ranges overlap (with slack)**: the two lines' x-ranges
///   intersect when expanded by `tuning.multiline_x_slack_px`.
/// - **No visual boundary in the gap** *(only when `boundary` is
///   `Some`)*: no background-colour change or divider stripe
///   between them.
///
/// Implementation: for each line (in top-to-bottom order), check
/// every existing block's last line. Join the block with the
/// smallest qualifying y-gap. Start a new block when none qualifies.
/// The "every existing block" loop handles multi-column layouts —
/// column A's lines stack into block A even when column B's lines
/// arrive interleaved by y.
fn group_lines_into_blocks(
    lines: Vec<OcrLine>,
    tuning: VisualTextTuning,
    boundary: Option<BoundaryContext<'_>>,
) -> Vec<OcrBlock> {
    if lines.is_empty() {
        return Vec::new();
    }

    // Stable-sort by bbox.y so OCR's iteration order doesn't leak
    // through to the grouper.
    let mut sorted = lines;
    sorted.sort_by_key(|l| l.bbox.y);

    let mut blocks_of_lines: Vec<Vec<OcrLine>> = Vec::new();

    for line in sorted {
        let mut best_idx: Option<usize> = None;
        let mut best_gap = i32::MAX;

        for (idx, block) in blocks_of_lines.iter().enumerate() {
            let prev = block.last().expect("block can never be empty");
            let prev_bottom = prev.bbox.y + prev.bbox.height;
            let gap = line.bbox.y - prev_bottom;
            if gap < 0 {
                continue; // line is above (or overlapping) the prev — different row, new block
            }
            let max_gap = (prev.bbox.height as f32 * tuning.multiline_max_gap_factor) as i32;
            if gap > max_gap {
                continue;
            }
            // X-overlap with slack.
            let slack = tuning.multiline_x_slack_px;
            let line_left = line.bbox.x - slack;
            let line_right = line.bbox.x + line.bbox.width + slack;
            let prev_left = prev.bbox.x;
            let prev_right = prev.bbox.x + prev.bbox.width;
            let x_overlap = line_left < prev_right && prev_left < line_right;
            if !x_overlap {
                continue;
            }
            // Pixel-level boundary veto: when we have the cropped
            // screenshot in hand, refuse the merge if the gap
            // contains a background-colour change or divider stripe.
            if let Some(ref ctx) = boundary {
                if !merge_passes_boundary_check(prev, &line, ctx, tuning) {
                    continue;
                }
            }
            if gap < best_gap {
                best_gap = gap;
                best_idx = Some(idx);
            }
        }

        match best_idx {
            Some(idx) => blocks_of_lines[idx].push(line),
            None => blocks_of_lines.push(vec![line]),
        }
    }

    // Materialise the lines-of-lines into `OcrBlock`s by flattening
    // each cluster: re-build the joined string, recompute spans
    // against it, and union the bboxes.
    blocks_of_lines
        .into_iter()
        .map(|block_lines| {
            let mut joined = String::new();
            let mut words: Vec<(String, Rect)> = Vec::new();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            let mut line_break_word_indices: Vec<usize> = Vec::new();
            let mut min_x = i32::MAX;
            let mut min_y = i32::MAX;
            let mut max_x = i32::MIN;
            let mut max_y = i32::MIN;
            for (line_idx, line) in block_lines.into_iter().enumerate() {
                if line_idx > 0 {
                    line_break_word_indices.push(words.len());
                }
                for (text, rect) in line.words {
                    if !joined.is_empty() {
                        joined.push(' ');
                    }
                    let start = joined.len();
                    joined.push_str(&text);
                    spans.push((start, joined.len()));
                    min_x = min_x.min(rect.x);
                    min_y = min_y.min(rect.y);
                    max_x = max_x.max(rect.x + rect.width);
                    max_y = max_y.max(rect.y + rect.height);
                    words.push((text, rect));
                }
            }
            OcrBlock {
                joined,
                bbox: Rect {
                    x: min_x,
                    y: min_y,
                    width: max_x - min_x,
                    height: max_y - min_y,
                },
                words,
                spans,
                line_break_word_indices,
            }
        })
        .collect()
}

/// Take the screenshot, crop to the scope (with the OCR context
/// padding), run ocrs, and produce one `OcrLine` per recognised
/// `TextLine`. Shared by `VisualLocator::matches`, `list_text`, and
/// `list_labelled_regions`.
/// What `ocr_lines` returns. The cropped RGB image and its
/// screen-coordinate origin are kept alongside the OCR results so
/// the grouper can run pixel-level boundary checks against the
/// same image without re-decoding the PNG.
pub(crate) struct OcrResult {
    lines: Vec<OcrLine>,
    image: image::RgbImage,
    /// Top-left of `image` in screen coordinates. Subtract from a
    /// screen-coord (x, y) to get the corresponding pixel in `image`.
    crop_origin: (i32, i32),
}

/// Per-frame OCR memo held on the `Session` (see `Session::visual_ocr_cache`).
///
/// OCR is the dominant cost in the visual path — tens of seconds per
/// full-frame pass on CPU — so repeated lookups on an *unchanged* frame must
/// not re-run it. Keyed by the captured frame's content hash plus the scoped
/// region; when a new frame hash arrives the whole map is dropped (the old
/// frame's results are stale). `Arc` so a hit is a cheap clone, not a deep
/// copy of the decoded image.
/// Region key for the OCR memo: `(x, y, w, h)` in screen coords, or `None`
/// for a full-frame (unscoped) pass.
type RegionKey = Option<(i32, i32, i32, i32)>;

#[derive(Default)]
pub(crate) struct OcrCache {
    frame_hash: u64,
    /// Region (None = full frame) → OCR result for it on the current frame.
    by_region: std::collections::HashMap<RegionKey, Arc<OcrResult>>,
}

impl OcrCache {
    /// Look up `region` for the frame identified by `hash`. If `hash` differs
    /// from the cached frame, the whole memo is dropped first (the old frame's
    /// results are stale) and this returns `None`.
    fn get(&mut self, hash: u64, region: RegionKey) -> Option<Arc<OcrResult>> {
        if self.frame_hash != hash {
            self.frame_hash = hash;
            self.by_region.clear();
        }
        self.by_region.get(&region).cloned()
    }

    /// Memoize `region`'s result for frame `hash` — but only if `hash` is
    /// still the current frame (a concurrent OCR of a newer frame may have
    /// advanced it, in which case this result is already stale).
    fn put(&mut self, hash: u64, region: RegionKey, result: Arc<OcrResult>) {
        if self.frame_hash == hash {
            self.by_region.insert(region, result);
        }
    }
}

/// Content hash of a captured frame. Identical frames produce byte-identical
/// PNGs (deterministic encoder + identical pixels), so this keys the memo; any
/// change busts it. Non-cryptographic is fine — collisions only cost a
/// re-OCR, never correctness.
fn frame_hash(png_bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    png_bytes.hash(&mut h);
    h.finish()
}

async fn ocr_lines(
    session: &Arc<Session>,
    region: Option<Rect>,
    png_bytes: Vec<u8>,
) -> Result<Arc<OcrResult>> {
    use ocrs::{ImageSource, TextItem};

    // Per-frame memo: if this exact frame was already OCR'd for this region,
    // reuse it instead of paying the ~tens-of-seconds pipeline again. A new
    // frame hash invalidates the whole memo.
    let region_key = region.map(|r| (r.x, r.y, r.width, r.height));
    let hash = frame_hash(&png_bytes);
    if let Some(hit) = session
        .visual_ocr_cache()
        .lock()
        .unwrap()
        .get(hash, region_key)
    {
        return Ok(hit);
    }

    let engine = session
        .visual_engine()
        .get_or_init(ensure_engine)
        .await
        .clone()
        .map_err(Error::visual)?;
    let context_pad_px = session.visual_text_tuning.ocr_context_padding_px;
    // Experimental: upscale the crop before OCR to make tiny glyphs legible.
    // `1` = off. Detected coordinates are divided back down below.
    let upscale = session.visual_text_tuning.ocr_upscale_factor.max(1) as i32;

    let result = tokio::task::spawn_blocking(move || -> Result<OcrResult> {
        let full = crate::locator::decode_screenshot_png(&png_bytes)
            .map_err(|e| Error::visual(format!("decode screenshot: {e}")))?;
        // If the caller scoped to a region, crop to it first.
        // Otherwise OCR the entire screen. `(origin_x, origin_y)` is
        // the top-left of the cropped image in screen coords — we
        // add it back to every word bbox so hits return in screen
        // space regardless of crop.
        //
        // Pad the crop by `context_pad_px` on each side (configured
        // via `VisualTextTuning::ocr_context_padding_px`). A tight
        // crop strips visual context that the ocrs recognition head
        // uses to disambiguate ambiguous glyphs. Without padding,
        // small/low-contrast labels misread (we saw "lazy-button"
        // become "lazv-button"). 32px (the default) is empirically
        // enough.
        //
        // Lines outside the *original* (unpadded) region are
        // filtered out at the call site so callers only see hits
        // they asked for.
        let (cropped, origin_x, origin_y) = if let Some(scope) = region {
            let padded = Rect {
                x: scope.x - context_pad_px,
                y: scope.y - context_pad_px,
                width: scope.width + 2 * context_pad_px,
                height: scope.height + 2 * context_pad_px,
            };
            let cropped = crate::locator::crop_to_bounds(full, padded)
                .map_err(|e| Error::visual(format!("crop to region: {e}")))?;
            (cropped, padded.x.max(0), padded.y.max(0))
        } else {
            (full, 0, 0)
        };

        let rgb = cropped.into_rgb8();
        let (w, h) = rgb.dimensions();
        // Feed ocrs an upscaled copy when requested; keep `rgb` (the native
        // crop) for the boundary sampler, and divide detected coordinates by
        // `upscale` below so hits land in screen space.
        let upscaled_img;
        let (ocr_bytes, ocr_w, ocr_h): (&[u8], u32, u32) = if upscale > 1 {
            let f = upscale as u32;
            upscaled_img =
                image::imageops::resize(&rgb, w * f, h * f, image::imageops::FilterType::Lanczos3);
            (upscaled_img.as_raw(), w * f, h * f)
        } else {
            (rgb.as_raw(), w, h)
        };
        let src = ImageSource::from_bytes(ocr_bytes, (ocr_w, ocr_h))
            .map_err(|e| Error::visual(format!("ocrs ImageSource: {e}")))?;
        let input = engine
            .prepare_input(src)
            .map_err(|e| Error::visual(format!("ocrs prepare_input: {e}")))?;

        let word_rects = engine
            .detect_words(&input)
            .map_err(|e| Error::visual(format!("ocrs detect_words: {e}")))?;
        let line_rects = engine.find_text_lines(&input, &word_rects);
        let lines = engine
            .recognize_text(&input, &line_rects)
            .map_err(|e| Error::visual(format!("ocrs recognize_text: {e}")))?;

        let mut out = Vec::new();
        for line_opt in lines.iter().flatten() {
            let words: Vec<(String, Rect)> = line_opt
                .words()
                .map(|w| {
                    let text: String = w.chars().iter().map(|c| c.char).collect();
                    let r = w.bounding_rect();
                    // Map ocrs pixel coords back to screen space: undo the
                    // upscale (no-op when `upscale == 1`), then add the crop
                    // origin.
                    let rect = Rect {
                        x: r.left() / upscale + origin_x,
                        y: r.top() / upscale + origin_y,
                        width: (r.width() / upscale).max(1),
                        height: (r.height() / upscale).max(1),
                    };
                    (text, rect)
                })
                .collect();
            if words.is_empty() {
                continue;
            }

            // Build the joined line string and remember each
            // word's char span so the matcher can map a substring
            // hit back to a word index range.
            let mut joined = String::new();
            let mut spans: Vec<(usize, usize)> = Vec::with_capacity(words.len());
            for (i, (text, _)) in words.iter().enumerate() {
                if i > 0 {
                    joined.push(' ');
                }
                let start = joined.len();
                joined.push_str(text);
                spans.push((start, joined.len()));
            }

            // Union bbox of all words on the line.
            let mut min_x = i32::MAX;
            let mut min_y = i32::MAX;
            let mut max_x = i32::MIN;
            let mut max_y = i32::MIN;
            for (_, r) in &words {
                min_x = min_x.min(r.x);
                min_y = min_y.min(r.y);
                max_x = max_x.max(r.x + r.width);
                max_y = max_y.max(r.y + r.height);
            }
            let bbox = Rect {
                x: min_x,
                y: min_y,
                width: max_x - min_x,
                height: max_y - min_y,
            };

            tracing::trace!(line = %joined, ?bbox, "visual: OCR line");
            out.push(OcrLine {
                joined,
                bbox,
                words,
                spans,
            });
        }
        Ok(OcrResult {
            lines: out,
            image: rgb,
            crop_origin: (origin_x, origin_y),
        })
    })
    .await
    .map_err(|e| Error::visual(format!("OCR task panicked: {e}")))??;

    // Memoize for this frame so sibling lookups (and the auto-wait retry
    // loop) on the same screen reuse it. Skip if a concurrent call already
    // advanced the frame hash — its result is for a newer frame.
    let arc = Arc::new(result);
    session
        .visual_ocr_cache()
        .lock()
        .unwrap()
        .put(hash, region_key, arc.clone());
    Ok(arc)
}

/// Public-via-`Locator::list_text` enumeration. Drops the
/// matcher's substring filter and returns one `TextHit` per OCR
/// **block** — i.e. per geometric cluster of consecutive lines
/// that form a logical multi-line label. A wrapped paragraph
/// produces one `TextHit` covering all its lines, not one per row.
pub(crate) async fn list_text(
    session: &Arc<Session>,
    scope: Rect,
    png: Vec<u8>,
) -> Result<Vec<TextHit>> {
    let ocr = ocr_lines(session, Some(scope), png).await?;
    let boundary = BoundaryContext {
        image: &ocr.image,
        crop_origin: ocr.crop_origin,
    };
    let blocks = group_lines_into_blocks(
        ocr.lines.clone(),
        session.visual_text_tuning,
        Some(boundary),
    );
    Ok(blocks
        .into_iter()
        .filter(|block| block.bbox.is_inside(&scope))
        .map(|block| TextHit {
            text: block.joined,
            bounds: block.bbox,
        })
        .collect())
}

/// Public-via-[`Session::recognized_text`] full-frame dump. OCRs the *entire*
/// captured frame (no region crop, no substring filter) and returns one
/// `TextHit` per recognised block, in reading order. The diagnostic for a
/// `find_by_text` that returned 0: it shows whether OCR read the target as a
/// near-miss, mis-recognised it, or never detected it at all.
pub(crate) async fn recognized_text(session: &Arc<Session>, png: Vec<u8>) -> Result<Vec<TextHit>> {
    let ocr = ocr_lines(session, None, png).await?;
    let boundary = BoundaryContext {
        image: &ocr.image,
        crop_origin: ocr.crop_origin,
    };
    let blocks = group_lines_into_blocks(
        ocr.lines.clone(),
        session.visual_text_tuning,
        Some(boundary),
    );
    Ok(blocks
        .into_iter()
        .map(|block| TextHit {
            text: block.joined,
            bounds: block.bbox,
        })
        .collect())
}

/// Public-via-`Locator::list_labelled_regions`. Runs OCR + block
/// grouping, then for each block runs `last_region_only` to find
/// the visual container around the label. Heavier than `list_text`
/// — one flood-fill per block — but produces a complete map of
/// "every text-bearing thing in this scope and the shape it sits
/// in." Multi-line wrapped labels produce one pair, not one per
/// row.
pub(crate) async fn list_labelled_regions(
    session: &Arc<Session>,
    scope: Rect,
    png: Vec<u8>,
    tuning: crate::session::VisualRegionTuning,
) -> Result<Vec<(TextHit, RegionLocator)>> {
    let ocr = ocr_lines(session, Some(scope), png.clone()).await?;
    let boundary = BoundaryContext {
        image: &ocr.image,
        crop_origin: ocr.crop_origin,
    };
    let blocks = group_lines_into_blocks(
        ocr.lines.clone(),
        session.visual_text_tuning,
        Some(boundary),
    );
    let mut pairs = Vec::new();
    for block in blocks {
        if !block.bbox.is_inside(&scope) {
            continue;
        }
        let region_loc = region::last_region_only(session, scope, block.bbox, &png, tuning)?;
        pairs.push((
            TextHit {
                text: block.joined,
                bounds: block.bbox,
            },
            region_loc,
        ));
    }
    Ok(pairs)
}

/// Normalize a string for case-fold and accent-insensitive matching:
/// NFKD decomposition + lowercase + strip combining marks.
///
/// - `café` ⇒ `cafe` (decompose `é` ⇒ `e` + combining-acute, strip mark).
/// - `Account` ⇒ `account` (case fold).
/// - `ﬁle` (U+FB01 ligature) ⇒ `file` (NFKD splits the ligature).
/// - `−` (U+2212 minus sign) ⇒ unchanged (no NFKD mapping to ASCII `-`),
///   so the caller is responsible for matching exotic punctuation
///   explicitly when needed.
///
/// Idempotent (running twice yields the same string).
fn normalize_for_match(s: &str) -> String {
    use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
    s.nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// One way to flatten a block's words into a searchable string.
/// `joined` is the haystack; `spans` is `(start, end)` byte offsets
/// per word, aligned with `block.words`.
#[derive(Debug)]
struct BlockVariant {
    joined: String,
    spans: Vec<(usize, usize)>,
}

/// Maximum number of lines for which we generate all 2^(N-1) seam-
/// choice variants. Above this, fall back to single-space-join only.
/// Keeps the variant count bounded (16 at N=5, 32+ at N=6).
const MAX_VARIANT_LINES: usize = 5;

/// Generate every joiner-choice variant of a block: at each line
/// break, pick either `" "` or `""` independently, then join all
/// words under that choice. For N lines (N−1 seams), this produces
/// 2^(N−1) variants; for blocks with more than [`MAX_VARIANT_LINES`]
/// lines, only the single-space-join variant is returned.
///
/// The `""` joins handle:
/// - Hyphenated words wrapped across lines (`"nee"` + `"dle"` →
///   `"needle"`).
/// - Single words OCR'd as two on the same line, where the
///   grouper kept them as separate `OcrLine`s due to line geometry.
///
/// Each variant's `spans` are computed against its own `joined`
/// string so [`union_bbox_for_match`] keeps working unchanged.
fn block_haystack_variants(block: &OcrBlock) -> Vec<BlockVariant> {
    let n_words = block.words.len();
    if n_words == 0 {
        return Vec::new();
    }
    // line_starts: word indices that start each line (0 always; then
    // each entry from line_break_word_indices).
    let mut line_starts: Vec<usize> = vec![0];
    line_starts.extend(block.line_break_word_indices.iter().copied());
    let n_lines = line_starts.len();
    let n_seams = n_lines.saturating_sub(1);

    if n_lines > MAX_VARIANT_LINES {
        // Bail out of variants generation for very long blocks —
        // 2^N grows fast, and a wrap point in such a block is
        // unlikely to be a hyphenation seam.
        return vec![BlockVariant {
            joined: block.joined.clone(),
            spans: block.spans.clone(),
        }];
    }

    let n_variants = 1usize << n_seams;
    let mut variants: Vec<BlockVariant> = Vec::with_capacity(n_variants);
    // Per-word normalized text — computed once, reused across all
    // variants. Variants only differ in the joiner choice at line
    // seams; the word texts themselves are identical.
    let normalized_words: Vec<String> = block
        .words
        .iter()
        .map(|(t, _)| normalize_for_match(t))
        .collect();
    for mask in 0..n_variants {
        // bit i of `mask` = use `""` at seam i; bit clear = `" "`.
        let mut joined = String::new();
        let mut spans = Vec::with_capacity(n_words);
        for (wi, normalized) in normalized_words.iter().enumerate() {
            // Within a line: words are separated by spaces.
            // At a line break (wi == line_starts[k] for k>=1):
            // joiner is `""` if mask bit (k-1) set, else `" "`.
            if wi > 0 {
                let line_break_idx = line_starts.iter().skip(1).position(|&s| s == wi);
                let joiner = match line_break_idx {
                    Some(seam) if (mask >> seam) & 1 == 1 => "",
                    _ => " ",
                };
                joined.push_str(joiner);
            }
            let start = joined.len();
            joined.push_str(normalized);
            spans.push((start, joined.len()));
        }
        variants.push(BlockVariant { joined, spans });
    }
    variants
}

/// Find every (start, end) byte-offset range in `haystack` that
/// matches `needle` under `mode`. Returns all non-overlapping
/// matches so a line containing the needle multiple times produces
/// multiple hits.
///
/// Both `haystack` and `needle` are normalized via
/// [`normalize_for_match`] before comparison, making matching
/// case-fold-and-accent-insensitive. The returned byte ranges are
/// offsets in the **normalized** haystack — callers that need
/// original-string offsets should normalize the haystack before
/// calling so the offsets line up with what they later inspect.
fn find_matches(haystack: &str, needle: &str, mode: MatchMode) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let h = normalize_for_match(haystack);
    let n = normalize_for_match(needle);
    if n.is_empty() {
        return Vec::new();
    }
    match mode {
        MatchMode::Exact => {
            if h == n {
                vec![(0, h.len())]
            } else {
                Vec::new()
            }
        }
        MatchMode::Substring => {
            let mut out = Vec::new();
            let mut start = 0usize;
            while let Some(off) = h[start..].find(&n) {
                let abs = start + off;
                out.push((abs, abs + n.len()));
                // Advance past this match so we don't loop.
                start = abs + n.len().max(1);
                if start > h.len() {
                    break;
                }
            }
            out
        }
        MatchMode::Fuzzy => fuzzy_find(&h, &n),
    }
}

/// Levenshtein edit distance between two strings (char-wise).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Edit-distance-tolerant substring search. Slides a window of
/// `needle.word_count` consecutive haystack words and reports every window
/// whose text is within a small normalized edit budget of the needle (≈1
/// error per 5 chars, min 1). Offsets are byte ranges into the (normalized)
/// haystack, matching the `Substring` path so `union_bbox_for_match` maps them
/// the same way.
fn fuzzy_find(h: &str, n: &str) -> Vec<(usize, usize)> {
    let needle_words = n.split_whitespace().count().max(1);
    let budget = (n.chars().count() / 5).max(1);

    // Byte spans of each haystack word.
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, ch) in h.char_indices() {
        if ch.is_whitespace() {
            if let Some(s) = start.take() {
                words.push((s, idx));
            }
        } else if start.is_none() {
            start = Some(idx);
        }
    }
    if let Some(s) = start.take() {
        words.push((s, h.len()));
    }
    if words.len() < needle_words {
        return Vec::new();
    }

    let mut out = Vec::new();
    for w in 0..=(words.len() - needle_words) {
        let span = (words[w].0, words[w + needle_words - 1].1);
        if levenshtein(&h[span.0..span.1], n) <= budget {
            out.push(span);
        }
    }
    out
}

/// Map a `(match_start, match_end)` char-offset range back to the
/// union bbox of the OCR words it overlaps. `spans` lists each word's
/// `(start, end)` offset in the joined line string. Returns `None`
/// if the match doesn't overlap any word (shouldn't happen for
/// non-empty matches against a joined line, but guards against
/// edge cases).
fn union_bbox_for_match(
    words: &[(String, Rect)],
    spans: &[(usize, usize)],
    m_start: usize,
    m_end: usize,
) -> Option<Rect> {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    let mut hit = false;
    for (i, &(s, e)) in spans.iter().enumerate() {
        // Overlap test: [s, e) intersects [m_start, m_end).
        if s < m_end && m_start < e {
            let r = words[i].1;
            min_x = min_x.min(r.x);
            min_y = min_y.min(r.y);
            max_x = max_x.max(r.x + r.width);
            max_y = max_y.max(r.y + r.height);
            hit = true;
        }
    }
    if !hit {
        return None;
    }
    Some(Rect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    })
}

#[allow(dead_code)] // kept for unit tests; the main matcher now uses find_matches
fn text_matches(haystack: &str, needle: &str, mode: MatchMode) -> bool {
    match mode {
        MatchMode::Exact => haystack == needle,
        MatchMode::Substring => haystack.to_lowercase().contains(&needle.to_lowercase()),
        MatchMode::Fuzzy => !find_matches(haystack, needle, MatchMode::Fuzzy).is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_result() -> Arc<OcrResult> {
        Arc::new(OcrResult {
            lines: Vec::new(),
            image: image::RgbImage::new(1, 1),
            crop_origin: (0, 0),
        })
    }

    #[test]
    fn frame_hash_is_deterministic_and_content_sensitive() {
        assert_eq!(frame_hash(b"same bytes"), frame_hash(b"same bytes"));
        assert_ne!(frame_hash(b"frame a"), frame_hash(b"frame b"));
    }

    #[test]
    fn ocr_cache_hits_same_frame_and_region() {
        let mut cache = OcrCache::default();
        let h = 42;
        assert!(cache.get(h, None).is_none(), "cold cache misses");
        cache.put(h, None, dummy_result());
        assert!(cache.get(h, None).is_some(), "same frame+region hits");
        // Different region on the same frame is a separate entry.
        assert!(cache.get(h, Some((0, 0, 10, 10))).is_none());
    }

    #[test]
    fn ocr_cache_invalidates_on_new_frame() {
        let mut cache = OcrCache::default();
        cache.get(1, None); // establish frame 1 (as ocr_lines' miss path does)
        cache.put(1, None, dummy_result());
        assert!(cache.get(1, None).is_some());
        // A new frame hash drops every prior entry.
        assert!(cache.get(2, None).is_none(), "new frame busts the memo");
        assert!(
            cache.get(1, None).is_none(),
            "old frame's entry is gone after invalidation"
        );
    }

    #[test]
    fn ocr_cache_put_ignored_for_stale_frame() {
        let mut cache = OcrCache::default();
        // Current frame is 2 (set via get); a late put for frame 1 must not
        // land — its result is for a frame we've already moved past.
        assert!(cache.get(2, None).is_none());
        cache.put(1, None, dummy_result());
        assert!(cache.get(2, None).is_none(), "stale-frame put rejected");
    }

    #[test]
    fn find_matches_substring_in_single_word() {
        let hits = find_matches("account", "acc", MatchMode::Substring);
        assert_eq!(hits, vec![(0, 3)]);
    }

    #[test]
    fn find_matches_substring_spans_words() {
        // The matcher is given the *joined* line; "Add account" spans
        // both words in "Add account row".
        let hits = find_matches("Add account row", "Add account", MatchMode::Substring);
        assert_eq!(hits, vec![(0, 11)]);
    }

    #[test]
    fn find_matches_substring_is_case_insensitive() {
        let hits = find_matches("ADD account ROW", "add account", MatchMode::Substring);
        assert_eq!(hits, vec![(0, 11)]);
    }

    #[test]
    fn find_matches_substring_multiple_hits() {
        let hits = find_matches("foo bar foo", "foo", MatchMode::Substring);
        assert_eq!(hits, vec![(0, 3), (8, 11)]);
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("cursor", "cursor"), 0);
        assert_eq!(levenshtein("cursor", "cursar"), 1); // substitution
        assert_eq!(levenshtein("hover-target", "hover-targel"), 1);
        assert_eq!(levenshtein("abc", ""), 3);
    }

    #[test]
    fn fuzzy_matches_single_glyph_ocr_error() {
        // "Cursor" mis-read as "Cursar" should still match within a row.
        let hits = find_matches("cursar font scrollback", "Cursor", MatchMode::Fuzzy);
        assert_eq!(hits, vec![(0, 6)], "fuzzy should locate the mis-read word");
    }

    #[test]
    fn fuzzy_matches_hyphenated_misread() {
        let hits = find_matches(
            "primary-button mode-toggle",
            "hover-targel",
            MatchMode::Fuzzy,
        );
        assert!(hits.is_empty(), "an unrelated word must not fuzzy-match");
        let hits = find_matches("hover-targel dc-target", "hover-target", MatchMode::Fuzzy);
        assert_eq!(hits, vec![(0, 12)]);
    }

    #[test]
    fn fuzzy_multiword_window() {
        // Two-word needle slides a two-word window; a single-glyph error is
        // tolerated. "prefs dialog" sits at bytes 9..21; one substitution
        // (o→e) is within budget.
        let hits = find_matches("open the prefs dialog", "prefs dialeg", MatchMode::Fuzzy);
        assert_eq!(hits, vec![(9, 21)]);
    }

    #[test]
    fn fuzzy_rejects_too_many_errors() {
        // "Cursor" vs "Buffer" — 5 substitutions, well over the budget.
        let hits = find_matches("buffer", "Cursor", MatchMode::Fuzzy);
        assert!(hits.is_empty());
    }

    #[test]
    fn find_matches_exact_full_string_only() {
        assert_eq!(
            find_matches("Add account", "Add account", MatchMode::Exact),
            vec![(0, 11)]
        );
        assert!(find_matches("Add account row", "Add account", MatchMode::Exact).is_empty());
    }

    #[test]
    fn find_matches_empty_needle_yields_nothing() {
        assert!(find_matches("anything", "", MatchMode::Substring).is_empty());
        assert!(find_matches("anything", "", MatchMode::Exact).is_empty());
    }

    #[test]
    fn normalize_strips_diacritics() {
        assert_eq!(normalize_for_match("Café"), "cafe");
        assert_eq!(normalize_for_match("naïve"), "naive");
        assert_eq!(normalize_for_match("ÄÖÜ"), "aou");
    }

    #[test]
    fn normalize_decomposes_ligatures() {
        assert_eq!(normalize_for_match("ﬁle"), "file");
        assert_eq!(normalize_for_match("ﬂux"), "flux");
    }

    #[test]
    fn normalize_is_idempotent() {
        let s = "Café ﬁle ABC";
        let once = normalize_for_match(s);
        let twice = normalize_for_match(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn find_matches_substring_handles_diacritics() {
        let hits = find_matches("Café latte", "cafe", MatchMode::Substring);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn block_variants_handles_single_line_block() {
        // No line breaks → exactly one variant.
        let line = make_line(vec![("Hello", rect(0, 0, 50, 10))]);
        let blocks = group_lines_into_blocks(vec![line], default_text_tuning(), None);
        assert_eq!(blocks.len(), 1);
        let variants = block_haystack_variants(&blocks[0]);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].joined, "hello");
    }

    #[test]
    fn block_variants_two_lines_produce_space_and_no_space_join() {
        // Two lines forced to merge by paragraph spacing. Variants
        // should include both " "-join ("nee dle") and ""-join
        // ("needle") at the line seam.
        let line_a = make_line(vec![("nee", rect(0, 0, 30, 10))]);
        let line_b = make_line(vec![("dle", rect(0, 14, 30, 10))]);
        let blocks = group_lines_into_blocks(vec![line_a, line_b], default_text_tuning(), None);
        assert_eq!(blocks.len(), 1);
        let variants = block_haystack_variants(&blocks[0]);
        assert_eq!(variants.len(), 2);
        let joineds: Vec<&str> = variants.iter().map(|v| v.joined.as_str()).collect();
        assert!(joineds.contains(&"nee dle"));
        assert!(joineds.contains(&"needle"));
    }

    #[test]
    fn block_variants_query_needle_matches_hyphenated_wrap() {
        // Real-world: "needle" wrapped as ["nee", "dle"]. The
        // joined-zero variant lets a plain `needle` query match.
        let line_a = make_line(vec![("nee", rect(0, 0, 30, 10))]);
        let line_b = make_line(vec![("dle", rect(0, 14, 30, 10))]);
        let blocks = group_lines_into_blocks(vec![line_a, line_b], default_text_tuning(), None);
        let variants = block_haystack_variants(&blocks[0]);
        let mut any_match = false;
        for v in &variants {
            if !find_matches(&v.joined, "needle", MatchMode::Substring).is_empty() {
                any_match = true;
                break;
            }
        }
        assert!(any_match, "expected at least one variant to match 'needle'");
    }

    #[test]
    fn block_variants_capped_at_max_lines() {
        // 6 lines → 32 variants would be too many; falls back to
        // the single space-join variant.
        let lines: Vec<OcrLine> = (0..6)
            .map(|i| {
                let y = i * 14;
                make_line(vec![("word", rect(0, y, 40, 10))])
            })
            .collect();
        let blocks = group_lines_into_blocks(lines, default_text_tuning(), None);
        // 6 lines may or may not all merge depending on geometry —
        // but if they do, variants should fall back to the single
        // space-join.
        for block in &blocks {
            let n_lines = block.line_break_word_indices.len() + 1;
            if n_lines > MAX_VARIANT_LINES {
                let variants = block_haystack_variants(block);
                assert_eq!(
                    variants.len(),
                    1,
                    "expected fallback to single-variant for {n_lines}-line block"
                );
            }
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn union_bbox_for_match_single_word() {
        // Joined: "Add account row" (sample spans). Match "account" → spans 1.
        let words = vec![
            ("Add".to_string(), rect(0, 0, 30, 10)),
            ("account".to_string(), rect(40, 0, 60, 10)),
            ("row".to_string(), rect(110, 0, 30, 10)),
        ];
        let spans = vec![(0, 3), (4, 11), (12, 15)];
        let bbox = union_bbox_for_match(&words, &spans, 4, 11).unwrap();
        assert_eq!(bbox, rect(40, 0, 60, 10));
    }

    #[test]
    fn union_bbox_for_match_spans_two_words() {
        let words = vec![
            ("Add".to_string(), rect(0, 0, 30, 10)),
            ("account".to_string(), rect(40, 0, 60, 10)),
            ("row".to_string(), rect(110, 0, 30, 10)),
        ];
        let spans = vec![(0, 3), (4, 11), (12, 15)];
        // "Add account" → bytes 0..11. Spans words 0 and 1.
        let bbox = union_bbox_for_match(&words, &spans, 0, 11).unwrap();
        // Union: x=0 width=100 (rightmost edge of word 1 - left of word 0).
        assert_eq!(bbox, rect(0, 0, 100, 10));
    }

    #[test]
    fn union_bbox_for_match_returns_none_for_no_overlap() {
        let words = vec![("foo".to_string(), rect(0, 0, 30, 10))];
        let spans = vec![(0, 3)];
        // Match range [100, 200) doesn't overlap span [0, 3).
        assert!(union_bbox_for_match(&words, &spans, 100, 200).is_none());
    }

    fn make_line(words: Vec<(&str, Rect)>) -> OcrLine {
        let mut joined = String::new();
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(words.len());
        for (i, (text, _)) in words.iter().enumerate() {
            if i > 0 {
                joined.push(' ');
            }
            let start = joined.len();
            joined.push_str(text);
            spans.push((start, joined.len()));
        }
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;
        for (_, r) in &words {
            min_x = min_x.min(r.x);
            min_y = min_y.min(r.y);
            max_x = max_x.max(r.x + r.width);
            max_y = max_y.max(r.y + r.height);
        }
        let bbox = Rect {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        };
        let words = words.into_iter().map(|(t, r)| (t.to_string(), r)).collect();
        OcrLine {
            joined,
            bbox,
            words,
            spans,
        }
    }

    fn default_text_tuning() -> VisualTextTuning {
        VisualTextTuning::default()
    }

    #[test]
    fn unrelated_rows_split_into_separate_blocks() {
        // Two single-line labels stacked vertically with a normal
        // paragraph gap but on different rows of widgets — the
        // grouper should still cluster them into ONE block when
        // they're paragraph-close. To get separate blocks the gap
        // has to exceed the multiline factor.
        //
        // Here: line A at y=0 height=10 (bottom=10). Line B at
        // y=40 (gap=30). max_gap = height(10) * 0.6 = 6. Gap 30 >
        // 6, so they stay in separate blocks.
        let line_a = make_line(vec![
            ("Add", rect(0, 0, 30, 10)),
            ("account", rect(40, 0, 60, 10)),
        ]);
        let line_b = make_line(vec![
            ("Remove", rect(0, 40, 60, 10)),
            ("account", rect(70, 40, 60, 10)),
        ]);
        let blocks = group_lines_into_blocks(vec![line_a, line_b], default_text_tuning(), None);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].joined, "Add account");
        assert_eq!(blocks[1].joined, "Remove account");
    }

    #[test]
    fn wrapped_paragraph_merges_into_one_block() {
        // Two lines vertically close (paragraph spacing) with
        // overlapping x-ranges. Should merge.
        // Line A bottom = 10, Line B top = 14, gap = 4. max_gap =
        // 10 * 0.6 = 6. 4 <= 6, so they merge.
        let line_a = make_line(vec![
            ("Click", rect(0, 0, 50, 10)),
            ("here", rect(60, 0, 40, 10)),
        ]);
        let line_b = make_line(vec![
            ("to", rect(0, 14, 20, 10)),
            ("learn", rect(30, 14, 50, 10)),
            ("more", rect(90, 14, 40, 10)),
        ]);
        let blocks = group_lines_into_blocks(vec![line_a, line_b], default_text_tuning(), None);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].joined, "Click here to learn more");
        // Block bbox union: x 0..130, y 0..24.
        assert_eq!(blocks[0].bbox.x, 0);
        assert_eq!(blocks[0].bbox.y, 0);
        assert_eq!(blocks[0].bbox.width, 130);
        assert_eq!(blocks[0].bbox.height, 24);
    }

    #[test]
    fn cross_line_match_spans_block() {
        // Wrapped label "to learn" spans the boundary between two
        // lines that should have merged into one block.
        let line_a = make_line(vec![
            ("Click", rect(0, 0, 50, 10)),
            ("here", rect(60, 0, 40, 10)),
        ]);
        let line_b = make_line(vec![
            ("to", rect(0, 14, 20, 10)),
            ("learn", rect(30, 14, 50, 10)),
            ("more", rect(90, 14, 40, 10)),
        ]);
        let blocks = group_lines_into_blocks(vec![line_a, line_b], default_text_tuning(), None);
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        let hits = find_matches(&block.joined, "here to learn", MatchMode::Substring);
        assert_eq!(hits.len(), 1);
        let (s, e) = hits[0];
        let union = union_bbox_for_match(&block.words, &block.spans, s, e).unwrap();
        // Spans "here" (60..100, y 0..10), "to" (0..20, y 14..24),
        // "learn" (30..80, y 14..24). Union: x 0..100, y 0..24.
        assert_eq!(union.x, 0);
        assert_eq!(union.y, 0);
        assert_eq!(union.width, 100);
        assert_eq!(union.height, 24);
    }

    #[test]
    fn parallel_columns_stay_separate() {
        // Two columns at the same y. The grouper sorts by y first,
        // so column-A and column-B lines interleave. Each new line
        // should join the column whose last line shares x-overlap.
        let a1 = make_line(vec![("Alpha", rect(0, 0, 40, 10))]);
        let b1 = make_line(vec![("Beta", rect(200, 0, 40, 10))]);
        let a2 = make_line(vec![("Apple", rect(0, 14, 40, 10))]);
        let b2 = make_line(vec![("Berry", rect(200, 14, 40, 10))]);
        let blocks = group_lines_into_blocks(vec![a1, b1, a2, b2], default_text_tuning(), None);
        assert_eq!(blocks.len(), 2);
        // Order within each block: top-to-bottom. Block order is
        // the order the first line of each block appeared.
        assert_eq!(blocks[0].joined, "Alpha Apple");
        assert_eq!(blocks[1].joined, "Beta Berry");
    }

    // ── Boundary-detection tests ──────────────────────────────────
    //
    // The four tests above exercise the geometric path with no
    // image (boundary=None). The next three build a synthetic
    // RgbImage and pass it as BoundaryContext to verify the
    // pixel-level vetoes:
    //
    // - Different background colour between two paragraph-close
    //   lines → don't merge.
    // - Horizontal divider line in the gap → don't merge.
    // - Vertical divider line in the gap → don't merge.
    //
    // The lines are paragraph-close + x-overlapping, so without
    // the boundary check they WOULD merge — proving the veto did
    // the work, not the geometric path.

    fn solid_image(w: u32, h: u32, color: [u8; 3]) -> image::RgbImage {
        let mut img = image::RgbImage::new(w, h);
        for x in 0..w {
            for y in 0..h {
                img.put_pixel(x, y, image::Rgb(color));
            }
        }
        img
    }

    /// Two paragraph-close lines whose surrounding background pixels
    /// are deliberately different colours. The boundary check should
    /// veto the merge.
    #[test]
    fn boundary_check_vetoes_merge_on_background_colour_change() {
        // Image: top half is grey (200), bottom half is a distinct
        // darker grey (100). The two halves are different "rows" /
        // "cards" sharing the same column.
        let mut img = solid_image(80, 30, [200, 200, 200]);
        for x in 0..80 {
            for y in 15..30 {
                img.put_pixel(x, y, image::Rgb([100, 100, 100]));
            }
        }
        // Line A at y=2..8, line B at y=18..24. Paragraph-close
        // (gap=10, max_gap=6*0.6=3.6 — wait, height=6, so max_gap=3).
        // Bump gap_factor so the geometric test passes.
        let mut tuning = default_text_tuning();
        tuning.multiline_max_gap_factor = 2.0;
        let line_a = make_line(vec![("Top", rect(0, 2, 60, 6))]);
        let line_b = make_line(vec![("Bot", rect(0, 18, 60, 6))]);
        let ctx = BoundaryContext {
            image: &img,
            crop_origin: (0, 0),
        };
        let blocks = group_lines_into_blocks(vec![line_a, line_b], tuning, Some(ctx));
        assert_eq!(
            blocks.len(),
            2,
            "expected separate blocks on bg-colour change"
        );

        // Sanity: without the boundary context, the same two lines
        // DO merge (geometric test alone is permissive).
        let line_a2 = make_line(vec![("Top", rect(0, 2, 60, 6))]);
        let line_b2 = make_line(vec![("Bot", rect(0, 18, 60, 6))]);
        let blocks_no_check = group_lines_into_blocks(vec![line_a2, line_b2], tuning, None);
        assert_eq!(
            blocks_no_check.len(),
            1,
            "regression: the test setup must geometrically merge without the boundary check"
        );
    }

    /// Two paragraph-close lines on the same background but with a
    /// 2-px-tall horizontal divider drawn across the gap.
    #[test]
    fn boundary_check_vetoes_merge_on_horizontal_divider() {
        let mut img = solid_image(80, 30, [200, 200, 200]);
        // 2-px dark divider at y=12..14, spanning the full width.
        for x in 0..80 {
            for y in 12..14 {
                img.put_pixel(x, y, image::Rgb([20, 20, 20]));
            }
        }
        let mut tuning = default_text_tuning();
        tuning.multiline_max_gap_factor = 2.0;
        let line_a = make_line(vec![("Top", rect(0, 2, 60, 6))]);
        let line_b = make_line(vec![("Bot", rect(0, 18, 60, 6))]);
        let ctx = BoundaryContext {
            image: &img,
            crop_origin: (0, 0),
        };
        let blocks = group_lines_into_blocks(vec![line_a, line_b], tuning, Some(ctx));
        assert_eq!(blocks.len(), 2, "horizontal divider should veto merge");
    }

    /// Two paragraph-close lines on the same background but with a
    /// 2-px-wide vertical divider drawn through the gap (e.g. a
    /// split-pane vertical rule that crosses between the rows).
    #[test]
    fn boundary_check_vetoes_merge_on_vertical_divider() {
        let mut img = solid_image(80, 30, [200, 200, 200]);
        // 2-px dark vertical divider at x=39..41, spanning the
        // full height. Crosses the entire image, including the
        // gap between the two text rows.
        for x in 39..41 {
            for y in 0..30 {
                img.put_pixel(x, y, image::Rgb([20, 20, 20]));
            }
        }
        let mut tuning = default_text_tuning();
        tuning.multiline_max_gap_factor = 2.0;
        let line_a = make_line(vec![("Top", rect(0, 2, 60, 6))]);
        let line_b = make_line(vec![("Bot", rect(0, 18, 60, 6))]);
        let ctx = BoundaryContext {
            image: &img,
            crop_origin: (0, 0),
        };
        let blocks = group_lines_into_blocks(vec![line_a, line_b], tuning, Some(ctx));
        assert_eq!(blocks.len(), 2, "vertical divider should veto merge");
    }

    /// Connectivity check (opt-in): two lines on the *same* uniform
    /// background colour but each boxed in by a thin border that the
    /// bg-colour and divider checks both miss (border colour is
    /// different from bg, but the divider scan's "must differ from
    /// BOTH backgrounds" test only sees a few border-coloured pixels
    /// per gap row — not a majority). The bounded flood from below
    /// prev-line stays trapped inside the upper card and can't
    /// reach the next-line's region, so the merge is correctly
    /// vetoed.
    #[test]
    fn connectivity_check_vetoes_merge_when_lines_are_boxed_separately() {
        let mut img = solid_image(80, 40, [200, 200, 200]);
        // Upper box: 1px-thick border around y=0..16.
        for x in 0..80 {
            img.put_pixel(x, 0, image::Rgb([20, 20, 20]));
            img.put_pixel(x, 15, image::Rgb([20, 20, 20]));
        }
        for y in 0..16 {
            img.put_pixel(0, y, image::Rgb([20, 20, 20]));
            img.put_pixel(79, y, image::Rgb([20, 20, 20]));
        }
        // Lower box: 1px-thick border around y=24..40.
        for x in 0..80 {
            img.put_pixel(x, 24, image::Rgb([20, 20, 20]));
            img.put_pixel(x, 39, image::Rgb([20, 20, 20]));
        }
        for y in 24..40 {
            img.put_pixel(0, y, image::Rgb([20, 20, 20]));
            img.put_pixel(79, y, image::Rgb([20, 20, 20]));
        }
        let mut tuning = default_text_tuning();
        // Loosen the geometric gap so the boxes' gap (y=16..24, 8px)
        // still passes the y-gap test.
        tuning.multiline_max_gap_factor = 2.0;
        // Disable divider-detection so we're sure it's the
        // connectivity check doing the vetoing (the thin borders
        // are too sparse to trip the divider majority test on their
        // own in this synthetic image anyway).
        tuning.divider_detection_enabled = false;
        tuning.connectivity_check_enabled = true;
        let line_a = make_line(vec![("Top", rect(10, 4, 60, 8))]);
        let line_b = make_line(vec![("Bot", rect(10, 28, 60, 8))]);
        let ctx = BoundaryContext {
            image: &img,
            crop_origin: (0, 0),
        };
        let blocks = group_lines_into_blocks(vec![line_a, line_b], tuning, Some(ctx));
        assert_eq!(
            blocks.len(),
            2,
            "connectivity check should veto merge across boxed-in lines"
        );
    }

    /// Connectivity check enabled but lines sit in the same uniform
    /// region with no boxing — the flood should freely reach the
    /// target and the merge should still happen.
    #[test]
    fn connectivity_check_allows_merge_on_continuous_background() {
        let img = solid_image(80, 30, [200, 200, 200]);
        let mut tuning = default_text_tuning();
        tuning.multiline_max_gap_factor = 2.0;
        tuning.connectivity_check_enabled = true;
        let line_a = make_line(vec![("Top", rect(0, 2, 60, 6))]);
        let line_b = make_line(vec![("Bot", rect(0, 18, 60, 6))]);
        let ctx = BoundaryContext {
            image: &img,
            crop_origin: (0, 0),
        };
        let blocks = group_lines_into_blocks(vec![line_a, line_b], tuning, Some(ctx));
        assert_eq!(
            blocks.len(),
            1,
            "connectivity check should not veto on continuous bg"
        );
    }

    /// Happy path: two paragraph-close lines on a uniform background
    /// with no dividers. The boundary check should pass and the lines
    /// should still merge.
    #[test]
    fn boundary_check_passes_on_clean_paragraph() {
        let img = solid_image(80, 30, [200, 200, 200]);
        let mut tuning = default_text_tuning();
        tuning.multiline_max_gap_factor = 2.0;
        let line_a = make_line(vec![("Top", rect(0, 2, 60, 6))]);
        let line_b = make_line(vec![("Bot", rect(0, 18, 60, 6))]);
        let ctx = BoundaryContext {
            image: &img,
            crop_origin: (0, 0),
        };
        let blocks = group_lines_into_blocks(vec![line_a, line_b], tuning, Some(ctx));
        assert_eq!(blocks.len(), 1, "clean paragraph should still merge");
        assert_eq!(blocks[0].joined, "Top Bot");
    }
}
