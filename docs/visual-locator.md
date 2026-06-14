# Visual locator — OCR + flood-fill region detection

Gated behind the `visual` Cargo feature on the `waydriver` crate. Adds
two coordinated abilities for finding widgets that the AT-SPI tree
doesn't reveal:

1. **OCR-based text matching** — locate a widget by its on-screen text
   when AT-SPI doesn't surface it as an accessible.
2. **Region detection** — once OCR finds the text, walk outward
   through the pixels to find the visually-distinct shape enclosing
   it (a button pill, row, card frame), so clicks land on the
   widget rather than its inner glyphs.

This doc describes both pipelines, how they compose, what they cost,
and when each one is the right tool.

## Why this exists

AT-SPI is the normal interaction path: enumerate the accessibility
tree, find a widget by name/role/state, call `Action.do_action` or
synthesize pointer events at its bounds. waydriver's regular
[`Locator`](https://docs.rs/waydriver/latest/waydriver/struct.Locator.html)
does all that.

But real toolkits have gaps. Two we've hit and confirmed are
genuinely upstream:

- **libadwaita lazy realization** — an `AdwPreferencesGroup`
  constructed with `visible:false` inside an `AdwPreferencesPage` and
  then flipped visible after `present()` never has its accessible
  subtree built. The same happens to a non-initial `AdwPreferencesDialog`
  page. The contained `AdwButtonRow` / `AdwSwitchRow` paints on screen
  but is absent from every AT-SPI surface. We exhaustively tried to
  *force* realization from the client and **none work** (confirmed live
  on mutter 49 / GTK4 4.20 / libadwaita 1.8):
    - parent traversal (`GetChildren`), a `0..ChildCount`
      `GetChildAtIndex(i)` loop, and `Cache.GetItems` on the app bus —
      the widgets are simply never published;
    - a grid of `Component.GetAccessibleAtPoint` hit-tests over the
      dialog and every descendant (thousands of calls) — no change;
    - synthetic compositor pointer-hover across the page — no change;
    - keyboard focus traversal (Tab through the dialog — how Orca
      surfaces them) — no change.

  Libadwaita doesn't register these accessibles, and there's no AT-SPI
  or input path that makes it. The bug is genuinely upstream; the OCR
  visual locator below is the only working way to drive these widgets.
- **AdwButtonRow has no accessible name** — even when the row *is* in
  the tree, its title doesn't surface as an AT-SPI name, so
  `Locator::find_by_name` returns zero.

We can't fix these from the client side: D-Bus enumeration finds
what the toolkit chose to publish. The pixels on screen, however,
are real. The visual locator drives off those pixels.

It's strictly **opt-in**. waydriver's existing `Locator::click` etc.
never silently fall back to OCR — the cost (hundreds of ms) is too
high to hide, and silent fallback would mask real selector bugs. You
reach for `Session::find_by_text` only when you've established that
AT-SPI doesn't see the widget.

## The OCR pipeline

```
                ┌──────────────────────────────────────────────┐
                │  Session::take_screenshot()                  │
                │   PipeWire keepalive stream → PNG bytes      │
                └────────────────────┬─────────────────────────┘
                                     │
                                     v
                ┌──────────────────────────────────────────────┐
                │  image::load_from_memory(...)                │
                │   PNG → DynamicImage                         │
                └────────────────────┬─────────────────────────┘
                                     │
              optional .within(rect) │ crop to parent region
                  + 32px context pad │   (Locator::find_by_text)
                                     v
                ┌──────────────────────────────────────────────┐
                │  ocrs::OcrEngine                             │
                │   prepare_input → detect_words →             │
                │   find_text_lines → recognize_text           │
                │   (pure-Rust, ONNX via rten)                 │
                └────────────────────┬─────────────────────────┘
                                     │
                                     v
                ┌──────────────────────────────────────────────┐
                │  Filter words by `text` (Substring/Exact)    │
                │  Translate bboxes back to screen coords      │
                │  Return Vec<Rect>                            │
                └──────────────────────────────────────────────┘
```

### Engine lifecycle

The `OcrEngine` is loaded **once per session** into a shared
`tokio::sync::OnceCell`. The two `.rten` model files (text-detection
~2.5 MB, text-recognition ~10 MB) are looked up in this order:

1. **Env-var override** — `WAYDRIVER_OCRS_DETECTION_MODEL` and
   `WAYDRIVER_OCRS_RECOGNITION_MODEL` both set.
2. **XDG cache hit** — `$XDG_CACHE_HOME/waydriver/ocrs-models/` (or
   `~/.cache/...`) has both files.
3. **Auto-download** — fetch from the ocrs project's S3 bucket into
   the XDG cache. First call only; subsequent runs hit (2).

Set [`SessionConfig::prewarm_visual = true`](https://docs.rs/waydriver/latest/waydriver/struct.SessionConfig.html#structfield.prewarm_visual)
to spawn the engine load as a background task during `Session::start`
so the first `find_by_text` call doesn't pay the ~1–2 s model load.
On a fresh machine with no XDG cache, the first session also pays
~5–20 s of model download — pre-populate the cache in CI setup if
that matters.

### Cropping to a parent (the Locator::find_by_text path)

`Session::find_by_text(text)` OCR's the full screen. That works but
is slow (~200–500 ms on a 1024×768 frame) and noisy — every word
visible on screen is a candidate, so disambiguation matters.

`Locator::find_by_text(text)` on an AT-SPI parent locator is the
faster, more accurate form:

```rust
let dialog = session.locate("//Dialog[@name='Preferences']");
let text = dialog.find_by_text("lazy-button").await?;
```

This crops the screenshot to the parent's AT-SPI bounds (plus a
32 px padding ring) *before* it reaches ocrs:

- **Speed.** OCR runtime is roughly linear in image area; cropping
  to a typical dialog cuts a search from ~300 ms to ~50 ms.
- **Accuracy.** Less surrounding text means fewer false positives
  and less context that confuses the recognition head.

Why the 32 px context padding? Empirically, a tight crop strips the
visual context that ocrs's recogniser uses to disambiguate ambiguous
glyphs. Without padding, small/low-contrast labels misread (we saw
`lazy-button` → `lazv-button`). The 32 px ring restores the context;
hits inside the ring but outside the original scope are filtered
back out after OCR so the caller sees only matches that genuinely
fall inside the requested region.

### `MatchMode`

- `Substring` (default) — case-insensitive substring match. Tolerant
  of OCR's noise (it'll match `"open"` against `"open-lazy-issue1-dialog"`).
- `Exact` — equality on the full *joined line*, normalized.

Both modes Unicode-normalize haystack and needle before comparing:
NFKD decomposition + case-fold + combining-mark stripping. This
makes matching insensitive to:

- **Case** — `"Add Account"` matches `"add account"`.
- **Diacritics** — `"café"` matches `"cafe"`, `"naïve"` matches `"naive"`.
- **Ligatures and compatibility codepoints** — `"ﬁle"` (U+FB01)
  matches `"file"`, `"ﬂux"` (U+FB02) matches `"flux"`.

Exotic punctuation (e.g. the Unicode minus `−` U+2212 that
gnome-calculator uses in its history line) is **not** auto-mapped
to ASCII equivalents — match it explicitly when needed.

### Block grouping with visual-boundary detection

OCR returns text lines bottom-up via several heuristics, applied in
order:

1. **Geometric clustering**: lines with small y-gap and overlapping
   x-ranges merge into one block (wrapped paragraph behaviour).
2. **Pixel-level boundary checks** (when an image is available):
   even if the geometric tests pass, the merge is vetoed when the
   gap between two lines contains:
   - **A background-colour change** — sample an averaged window of
     pixels just below the upper line and one just above the lower
     line (window radius
     [`VisualTextTuning::background_sample_radius`](https://docs.rs/waydriver/latest/waydriver/struct.VisualTextTuning.html)
     px, default 2 = 5×5); if their colours differ by more than
     [`VisualTextTuning::background_color_tolerance`](https://docs.rs/waydriver/latest/waydriver/struct.VisualTextTuning.html)
     (default 24), the lines sit on different backgrounds. The
     averaged-window sampler smooths over single antialias-fringe
     pixels that would skew a single-pixel read.
   - **A horizontal divider stripe** — scan every row in the gap;
     a row where ≥ `boundary_majority_threshold` (default 0.8) of
     `boundary_samples_per_axis` (default 16) sampled pixels differ
     from both surrounding backgrounds is a horizontal rule.
   - **A vertical divider stripe** — scan every column in the
     x-overlap range; same majority + colour-distance test. Picks
     up split-pane rules that pass through the gap.
3. **Connectivity check** (opt-in, `connectivity_check_enabled =
   false` by default): a bounded BFS in the gap. From the bg pixel
   just below the upper line, flood-fill at most
   `max_connectivity_pixels` (default 4096) pixels and check
   whether the flood reaches the bg pixel just above the lower
   line. If not, the lines are in visually-separated regions
   despite having the same background colour — catches "two cards
   on the same fill, each boxed in by a thin border the divider
   check is too sparse to detect".

All checks consult `VisualTextTuning::color_distance` (default
`LabCie76`, see below) when comparing pixels. The divider checks
toggle together via `divider_detection_enabled` (default `true`);
disable on themes where shadow rasters or anti-aliased streaks
would trip the heuristic.

#### Perceptual colour distance

`ColorDistance` controls how the visual locator compares pixel
colours, both for region detection (flood-fill, seed pick, shape
classification) and the boundary checks:

- `Rgb` — raw RGB Euclidean squared distance. Cheap, not
  perceptual. Use to reproduce legacy thresholds tuned against raw
  RGB.
- `LabCie76` (default) — ΔE\*76 in CIE Lab space. Roughly
  perceptual ("a ΔE of 6 is barely noticeable, 12 is clearly
  different"), cheap (one sRGB→Lab conversion).
- `LabCie2000` — ΔE\*00, perceptual gold standard. ~5× slower
  than CIE76; only worth it when CIE76 misclassifies subtle hue
  shifts in practice.

The default `background_color_tolerance: 24` scales sensibly across
modes — RGB ΔE 24 maps to Lab ΔE76 ~6, both "near-identical
backgrounds". When retuning, re-tune for the mode you switched to.

### Multi-word and multi-line matching

OCR returns text as a tree of `TextLine`s, each containing
`TextWord`s. The matcher joins words with spaces and substring-
matches against the joined string. Two layers of join:

- **Per-line** for `MatchMode::Exact`. A line's words are joined
  with spaces; the needle must equal the whole joined line. Use
  `Exact` to distinguish `"Add account"` from
  `"Add account and continue"`.
- **Per-block** for `MatchMode::Substring`. The grouper builds
  multi-line blocks from geometrically-close lines (see [block
  grouping](#block-grouping-with-visual-boundary-detection)). For
  each block, the matcher tries every joiner-choice variant: at
  each line break, it can use `" "` or `""` independently, giving
  `2^(N−1)` variants for a block of N lines (capped at N = 5; above
  that, fall back to the single space-join). This handles:
  - **Wrapped multi-word labels** — `"Click here to learn more"`
    matches whether the words wrapped onto one row or three (the
    space-join variant covers this).
  - **Hyphenated wraps** — `"needle"` matches an OCR result of
    `["nee", "dle"]` (the no-space variant joins to `"needle"`).
  - **Ligature splits across lines** (rare but possible) — the
    Unicode normalization pass handles ligatures inside a single
    line already; the variants extend the same idea across
    breaks.

When a substring match spans multiple words — on the same line or
across lines — the returned bbox is the **union** of the matched
words' bboxes. For a single-line match this is the tight rectangle
around the matched text. For a multi-line match it's the AABB of
every involved word, which can include vertical gaps between the
text rows; the centroid still lands inside the matched text block,
which is what you want for clicking and region seeding.

**Trade-off of cross-line substring:** unrelated labels on
adjacent lines can spuriously match across the line break (a search
for `"account Remove"` would hit text that read
`"Add account / Remove account"`). In practice nobody writes
selectors that way, and the user opted in to OCR because AT-SPI
couldn't help — they're already using a fuzzy tool. Use `Exact`
when you need line-precise semantics.

### Introspection

Both `VisualLocator` and `RegionLocator` implement `Debug`, so
`tracing::debug!("{loc:?}")` or `dbg!(loc)` shows what the locator
represents:

```text
VisualLocator { kind: "text-label", text: "Add account",
                match_mode: Substring, region: Some(Rect { ... }),
                timeout: None }

RegionLocator { kind: "visual-region",
                bbox: Rect { x: 192, y: 158, width: 640, height: 92 },
                centroid: (512, 204) }
```

The `kind` field is a constant string that makes the role explicit
in logs — `"text-label"` for OCR text matches, `"visual-region"` for
flood-fill shapes — so dumps tell you what the locator means without
having to follow the type back to its constructor.

`VisualLocator` also exposes the constructed-with values via
getters:

- [`text()`](https://docs.rs/waydriver/latest/waydriver/struct.VisualLocator.html#method.text)
  — the search query.
- [`region()`](https://docs.rs/waydriver/latest/waydriver/struct.VisualLocator.html#method.region)
  — the parent scope, if any.
- [`match_mode()`](https://docs.rs/waydriver/latest/waydriver/struct.VisualLocator.html#method.match_mode)
  — current matching strategy.

### What `VisualLocator::click` does today

Click the **centre of the OCR word's bbox**. Works when the text
glyphs sit inside the gesture controller's hit-rect — a centred label
inside an `AdwButtonRow`, for instance.

Doesn't always work:

- Checkboxes / toggles whose label and click target are separate
  widgets.
- Widgets sized much larger than their text, where clicking on the
  glyphs hits the inner label's selection gesture instead of the
  surrounding container's activation gesture.

For those cases, the region pipeline below is the escape hatch.

## The template-matching pipeline

For widgets that have no on-screen text (icon-only buttons, image
links, custom-drawn glyphs), OCR can't help. The
[`ImageLocator`](https://docs.rs/waydriver/latest/waydriver/struct.ImageLocator.html)
path takes a **reference PNG** captured against a known-good
screenshot of the same app, and finds where that patch sits in the
current screen via classical normalized cross-correlation (NCC).

```rust
let icon = std::fs::read("references/save_icon.png")?;
session
    .find_image(&icon)?
    .with_threshold(0.9)
    .click()
    .await?;

// Or scoped to an AT-SPI parent (faster, fewer false positives):
let toolbar = session.locate("//ToolBar[@name='Main']");
toolbar
    .find_image(&icon).await?
    .click()
    .await?;
```

### Algorithm

1. Decode the template PNG once at `find_image` time.
2. On each terminal-method call (`bounds`, `click`, ...), take a
   fresh screenshot, crop to the optional scope rect, convert both
   target and template to grayscale.
3. `imageproc::template_matching::match_template` with method
   `CrossCorrelationNormalized` — slide the template, scoring each
   position by NCC (`Σ(a·b) / sqrt(Σa² · Σb²)`, in `[0, 1]`,
   peaks at 1.0 for a perfect match).
4. Walk the score grid for all peaks above the threshold (default
   `0.85`), sort best-first, apply non-maximum suppression so
   neighbouring peaks within `min(template_w, template_h) / 2`
   px collapse to one hit.
5. Translate hit positions back into screen coords.

### Threshold tuning

- `0.95+` — very strict. Use when the reference was captured on the
  same machine, same theme, same DPI as the test run. Rejects most
  false positives in busy layouts.
- `0.85` (default) — tolerant of subpixel antialias differences and
  minor lighting shifts.
- `<0.70` — likely matches *something*, but in a busy screen will
  probably match the wrong thing. If a known-good reference scores
  below 0.7, recapture it.

### When to use this vs. `find_by_text`

| You want to click... | Use |
|----------------------|-----|
| A button with text   | `find_by_text("Save")` |
| An icon-only button (Save icon, hamburger, X) | `find_image(&icon_png)` |
| A widget AT-SPI surfaces | `Locator` with an XPath selector |
| Something that wraps over multiple lines | `find_by_text("Click here to learn more")` |

OCR is the right choice whenever you can read the on-screen text.
Template matching is the escape hatch for *visual-only* widgets.

### Known failure modes

- **DPI / scale change.** A 32×32 reference captured on a 1× display
  won't match a 64×64 render on a 2× display. The basic matcher
  does no scale search; recapture per DPI, or build an image
  pyramid wrapper if a workload demonstrates the need.
- **Theme swap.** Light → dark mode = all references stale.
- **Antialias / font hinting drift.** Same widget on a different
  GPU / fontconfig stack can score below 0.85. Lower the threshold
  or recapture.
- **Animation / hover / focus mid-capture.** Ripple effects, focus
  rings, hover highlights all change the pixels. Capture references
  in a steady state.
- **Multiple identical icons on screen.** `bounds()` errors out on
  ambiguous matches; use `within(rect)` to disambiguate.

### Cost

One NCC pass over the haystack ≈ O(W·H·w·h) work. For a 1920×1080
screenshot and a 64×64 template, ~8 billion ops naïvely; modern
machines do this in 10–50 ms. Cropping with `within(rect)` cuts
the haystack and is the single best speedup. The implementation
calls `match_template` (single-threaded); if a workload demands
it, swapping to `match_template_parallel` is a one-line change.

## The region detection pipeline

When clicking text glyphs doesn't fire the surrounding widget's
activation, we want a different click target: the **centroid of the
visually-distinct shape that contains the text**. That's typically a
button pill, a row's rounded rectangle, or a card frame.

The algorithm is a **BFS flood-fill** from a seed pixel adjacent to
the OCR text bbox. A "region" is a contiguous block of pixels whose
RGB Euclidean distance to a seed sample is within tolerance — a
button's fill, a row's background, a card's surface. Each iteration
finds one enclosing region; iterating outward builds a chain.

```
                ┌──────────────────────────────────────────────┐
                │  Inputs                                      │
                │   parent_bounds (AT-SPI Rect, screen coords) │
                │   inner_bbox    (OCR text bbox, screen coords)│
                │   full_png      (Session::take_screenshot)   │
                │   tuning        (SessionConfig::visual_      │
                │                  region_tuning)              │
                └────────────────────┬─────────────────────────┘
                                     │
                                     v
                ┌──────────────────────────────────────────────┐
                │  Crop full_png to parent_bounds              │
                │  Translate inner_bbox into crop coords       │
                └────────────────────┬─────────────────────────┘
                                     │
                                     v
                ┌──────────────────────────────────────────────┐
                │  pick_seed_outside(inner_bbox, image)        │
                │   Try right / left / below / above the       │
                │   inner bbox, +4 px offset. Sanity-check     │
                │   uniformity vs a neighbouring pixel so we   │
                │   don't seed on glyph antialiasing fringe.   │
                └────────────────────┬─────────────────────────┘
                                     │
                                     v
                ┌──────────────────────────────────────────────┐
                │  flood_fill(image, seed, tolerance)          │
                │   BFS, Vec<bool> visited grid.               │
                │   Add 4-neighbour pixels where               │
                │     ‖rgb(neighbour) - rgb(seed)‖₂ ≤ tolerance│
                │   Track bbox + centroid as we go.            │
                └────────────────────┬─────────────────────────┘
                                     │
                                     v
                ┌──────────────────────────────────────────────┐
                │  region_0 = { bbox, centroid }               │
                │  Translate back to screen coords.            │
                │  Push into result list.                      │
                └────────────────────┬─────────────────────────┘
                                     │
                                     v (find_regions / first_region only)
                ┌──────────────────────────────────────────────┐
                │  Stop?                                       │
                │   • region == previous region (no growth)    │
                │   • region covers entire crop                │
                │   • iteration count ≥ tuning.max_regions     │
                │   • pixel_just_outside(region) has nowhere   │
                │     to go (region touches all image edges)   │
                └────────────────────┬─────────────────────────┘
                                     │ otherwise
                                     v
                ┌──────────────────────────────────────────────┐
                │  seed = pixel_just_outside(region.bbox)      │
                │  Loop back to flood_fill.                    │
                └──────────────────────────────────────────────┘
```

### Why a centroid, not a bbox centre

For axis-aligned rectangles, the bbox centre and the geometric
centroid coincide. For non-rectangular shapes — pills (rounded
rectangles), circles, polygon icons — the bbox centre can land
*outside* the actual region. The centroid is the mean of every pixel
position in the visited set; it's always inside the shape, which is
where you want to click.

For a 60×30 pill flood-filled from inside, the centroid lands at the
pill's geometric centre. For a circle, same. For an L-shaped
selection or a polygon icon, the centroid is inside the shape and
clicks land on the widget.

### Shape classification

Each `RegionLocator` carries a coarse [`Shape`](https://docs.rs/waydriver/latest/waydriver/enum.Shape.html)
value derived from the flood-fill's pixel-count vs bbox-area ratio
combined with a 4-corner sample. The classifier picks one of:

- **`Rectangle`** — fill ratio ≥ 0.97 and all four bbox corners
  match the seed colour. Bare GTK button interiors, `AdwButtonRow`
  contents.
- **`Pill`** — fill ratio ≥ 0.82 with 0–1 bbox corners inside. The
  corner radius trims the bbox corners off the shape. Most GTK
  button pills and Adw row backgrounds land here.
- **`Ellipse`** — fill ratio in 0.65–0.83 with 0 bbox corners
  inside. Round avatar buttons, circular close icons.
- **`Irregular`** — anything else. Polygon icons, regions with
  holes, shapes whose ratio doesn't fit a primitive. Don't trust
  `bounds().center_*()` here — use `centroid()`.

The classification is **best-effort**, intended for assertions and
log readability, not as a contract. Borderline cases (e.g. a
rectangle with one pixel of antialiased corner darkening) can flip
between categories. If a test branches on shape, treat unexpected
classifications as a soft signal rather than an absolute fail.

The seed for the flood doesn't have to be at the centre of the
target region — flood-fill is a BFS that recovers the same bbox /
centroid / classification regardless of starting point, as long as
the seed lands somewhere inside the region. `pick_seed_outside`
aims ~4 px outside the OCR text bbox specifically to leave the
glyphs (which the flood treats as a separate region) and land on
the surrounding fill.

### Tuning (`SessionConfig::visual_region_tuning`)

Every threshold the region pipeline uses is exposed on
[`VisualRegionTuning`](https://docs.rs/waydriver/latest/waydriver/struct.VisualRegionTuning.html):

- `tolerance: u8` (default `24`) — distance threshold for "same
  region", interpreted under `color_distance`. Glyph antialiasing
  pixels typically jump 60+ (RGB); subtle gradients within a button
  surface stay under 20. Lower the number when flood over-grows
  into adjacent widgets; raise it when flood under-grows because of
  gradients.
- `color_distance: ColorDistance` (default `LabCie76`) — which
  colour-distance metric to use. See [perceptual colour distance](#perceptual-colour-distance).
- `max_regions: usize` (default `16`) — safety cap on the
  iteration chain. Realistic widget tree depth is 3–5; the cap
  protects against pathological banded images.
- `seed_uniformity_threshold_sq: u32` (default `100`) — squared RGB
  distance below which the seed-pick treats a candidate seed and
  its 2-px-out neighbour as "uniform". Raise on noisy backgrounds.
- `shape_rectangle_min_ratio: f64` (default `0.97`),
  `shape_pill_min_ratio: f64` (default `0.82`),
  `shape_ellipse_ratio_range: (f64, f64)` (default `(0.65, 0.83)`)
  — fill-ratio thresholds for [shape classification](#shape-classification).

`MAX_PIXELS_PER_REGION` is implicit and equal to the cropped image's
total pixel count — the flood can't escape it.

### Tuning (`SessionConfig::visual_text_tuning`)

Knobs on
[`VisualTextTuning`](https://docs.rs/waydriver/latest/waydriver/struct.VisualTextTuning.html):

- `multiline_max_gap_factor: f32` (default `0.6`) — see [block
  grouping](#block-grouping-with-visual-boundary-detection).
- `multiline_x_slack_px: i32` (default `4`).
- `background_color_tolerance: u8` (default `24`) — threshold for
  the bg-colour change check.
- `divider_detection_enabled: bool` (default `true`).
- `ocr_context_padding_px: i32` (default `32`) — padding added on
  every side of a cropped element before running OCR; gives the
  recognition head visual context that disambiguates small/low-
  contrast glyphs.
- `boundary_samples_per_axis: usize` (default `16`),
  `boundary_majority_threshold: f32` (default `0.8`) — divider-scan
  density and the majority threshold.
- `background_sample_radius: u32` (default `2`) — radius of the
  averaged window used when sampling the bg colour at each
  boundary check. `0` falls back to a single-pixel sample.
- `color_distance: ColorDistance` (default `LabCie76`).
- `connectivity_check_enabled: bool` (default `false`),
  `max_connectivity_pixels: usize` (default `4096`) — opt-in
  bounded flood-fill check; see [block grouping](#block-grouping-with-visual-boundary-detection).

### Tuning (`SessionConfig::visual_click_tuning`)

Knobs on
[`VisualClickTuning`](https://docs.rs/waydriver/latest/waydriver/struct.VisualClickTuning.html)
control the headless-mutter cold-start pointer workaround applied
by `VisualLocator::click` and `RegionLocator::click`:

- `cold_start_warmup_enabled: bool` (default `true`) — set to
  `false` on real hardware where the cold-start race doesn't apply
  to fall through to a single motion + button-press.
- `cold_start_warmup_offset_px: f64` (default `4.0`) — distance
  of the warmup motion from the target.
- `cold_start_motion_settle: Duration` (default `60 ms`) — sleep
  after each motion call.
- `cold_start_press_settle: Duration` (default `50 ms`) — sleep
  between button-down and button-up.

### Model file verification

The auto-downloaded ocrs `.rten` model files are checksummed
against constants embedded in `crates/waydriver/src/visual/models.rs`:

- Cached file at session start: hashed, refused on mismatch
  (deleted + re-downloaded).
- Fresh download: hashed before the `*.partial → *.rten` rename;
  a corrupted download never becomes a cache hit.
- Env-var overrides (`WAYDRIVER_OCRS_DETECTION_MODEL`,
  `WAYDRIVER_OCRS_RECOGNITION_MODEL`) **bypass** verification — the
  user has explicitly pointed us at a file they control.

If upstream ocrs publishes new model files, the constants will
refuse to load the cache. Capture the new hashes with `sha256sum`
and update `DETECTION_SHA256` / `RECOGNITION_SHA256`; or set the
env-var override at runtime as an escape hatch.

### `Locator::list_text` and `Locator::list_labelled_regions` — enumeration

When you want to *discover* what's on screen rather than search for
a specific label, two enumeration methods produce a complete map of
the text-bearing widgets inside a Locator's scope:

```rust
let dialog = session.locate("//Dialog[@name='Preferences']");

// Every OCR'd line inside the dialog, line text + union bbox.
let hits = dialog.list_text().await?;
for h in &hits {
    println!("{:?} at {:?}", h.text, h.bounds);
}

// Each line paired with its enclosing visual region. One flood-fill
// per label; the screenshot is taken once and reused.
for (label, region) in dialog.list_labelled_regions().await? {
    println!("{} ({:?}) inside {:?} shape", label.text, label.bounds, region.shape());
}
```

`list_text` returns `Vec<TextHit>` where each `TextHit` has the
joined line text and the union bbox of all words in that line.
There's no substring filter — for searches use
[`find_by_text`](#the-ocr-pipeline). Cost is one OCR pass over the
locator's bounds (~50–200 ms cropped, ~200–500 ms full-screen).

`list_labelled_regions` adds a flood-fill per hit on top, returning
`Vec<(TextHit, RegionLocator)>`. Use it for:

- **Test discovery / scaffolding.** Print the full set of clickable
  text-bearing things in a dialog and pick targets interactively.
- **Visual regression.** Compare label set + region shapes between
  runs.
- **Dynamic selection.** "Click the first row whose label starts
  with `Show`" — `list_labelled_regions` then filter then click.

The cost is `list_text` plus N × flood-fill (typically ~10–30 ms
each). A dialog with 15 labels takes ~150–500 ms total.

### `Session::region_at(x, y)` — pixel-based entry point

The lowest level in the visual stack. Skips both OCR and the AT-SPI
parent lookup — just flood-fills from the supplied screen pixel and
returns the `RegionLocator` for whatever contiguous-colour shape
contains that pixel.

```rust
// I already know there's a clickable thing near here.
let region = session.region_at(512, 365).await?;
match region.shape() {
    Shape::Pill | Shape::Rectangle => region.click().await?,
    _ => return Err(anyhow!("expected a button-shaped widget at the cursor")),
}
```

Useful for:

- Coordinate-driven tests (you know the layout because you wrote
  the fixture).
- Visual debugging: "what's at this pixel?" — dump `region` and
  read its bbox/shape/centroid.
- Bridge code that already has coordinates from another source
  (a previous screenshot, a layout assertion, a logged event).

The seed pixel doesn't need to be at the centre of the region.
Flood-fill is deterministic: any pixel inside the target region
recovers the same bbox / centroid / shape. The only thing that
varies with the seed is *which* region you get — a pixel on a text
glyph returns the glyph's bbox; a pixel on the button fill returns
the button's bbox.

### The three Locator methods

All of them resolve `self`'s AT-SPI bounds, take a fresh screenshot,
and call into the region pipeline.

- **`Locator::find_regions(&self, inner: &VisualLocator)`** —
  full sweep. Returns `Vec<RegionLocator>` in **outermost-first**
  order: index 0 is the outermost region inside `self`'s bounds; the
  last element is the tightest region around `inner`. The order
  matches the call-site mental model (start at the parent, walk
  inward).
- **`Locator::first_region(&self, inner)`** — outermost only
  (`find_regions[0]`). Runs the full sweep but skips the
  intermediate `Vec` allocations.
- **`Locator::last_region(&self, inner)`** — innermost only
  (`find_regions[last]`). **One flood-fill, no chain walk.** Cheap.
  This is usually what you want — the button pill adjacent to the
  text.

Plus the convenience on `VisualLocator`:

- **`VisualLocator::parent_region()`** — equivalent to
  `parent.last_region(self)`, but doesn't require the caller to
  remember the parent locator. Requires the `VisualLocator` to have a
  parent scope (constructed via `Locator::find_by_text` or
  `Session::find_by_text(...).within(rect)`).

### `RegionLocator` action surface

Parallels `VisualLocator`'s shape, minus anything that would need
AT-SPI handles:

- `bounds() -> Rect` — axis-aligned bounding rect of the flood.
- `centroid() -> (i32, i32)` — pixel-set centre, the click target.
- `click()` — pointer click at the centroid. Uses the same
  motion-warmup-then-press pattern as `VisualLocator::click` to
  side-step headless mutter's cold-start pointer-routing race.
- `hover()` — pointer move only.
- `screenshot()` — PNG cropped to `bounds()`.

There is deliberately **no `fill`, `set_text`, `focus`,** or any
`is_<state>` predicate. Those need AT-SPI handles; a region is just
a bbox + centroid.

## How they compose

```rust
// AT-SPI sees the parent dialog but not the lazy button inside it.
let dialog = session.locate("//Dialog[@name='Preferences']");

// Find the on-screen text "lazy-button" inside that dialog.
let text = dialog.find_by_text("lazy-button").await?;

// Click the centroid of the pill surrounding the text. One flood-fill
// from a seed adjacent to the OCR bbox — fastest of the three region
// methods because it doesn't walk the enclosure chain.
dialog.last_region(&text).await?.click().await?;
```

Three orthogonal layers:

| Layer                 | Input                              | Output             | Cost              |
|-----------------------|------------------------------------|--------------------|-------------------|
| AT-SPI `Locator`      | XPath                              | accessible refs    | ms                |
| `VisualLocator`       | text + optional parent scope       | text bboxes        | 50–500 ms (OCR)   |
| `RegionLocator`       | text bbox + parent screenshot      | shape + centroid   | ~10–30 ms (flood) |

Each layer is opt-in. You reach down only when the layer above
doesn't work for your widget.

## Cost summary

| Operation                                 | Typical latency             |
|-------------------------------------------|-----------------------------|
| AT-SPI locator (`session.locate`)         | <10 ms                      |
| Session start — model download (first run)| 5–20 s                      |
| Session start — model load (no prewarm)   | 1–2 s on first OCR call     |
| Session start — model load (prewarm)      | parallel with session boot  |
| `Session::find_by_text` (full screen)     | 200–500 ms                  |
| `Locator::find_by_text` (cropped)         | 50–200 ms                   |
| `Locator::last_region`                    | +10–30 ms over OCR          |
| `Locator::find_regions` (full sweep)      | +30–100 ms (depends on chain depth) |

**These latencies assume an optimized build.** rten inference dominates OCR
cost and is roughly **30× slower at the dev profile's opt-level 0**: measured
~5–8 s per full-frame pass with optimized dependencies vs ~50–200 s without,
on CPU-only hosts. Consumers running the visual feature under `cargo test`
must add a dependency-only override to the **workspace root** `Cargo.toml`
(Cargo ignores profile overrides declared anywhere else — a library can't
ship this for you):

```toml
[profile.dev.package."*"]
opt-level = 3
```

(waydriver's own workspace root already applies this to just the rten/ocrs
crates, so in-repo contributors and the e2e suite get optimized OCR in
dev/test builds without the broader `"*"` override. The init warning below
still fires for in-repo debug builds — an `opt-level` override does not clear
`cfg(debug_assertions)` — and is a known false-positive there.)

The engine loader logs a warning at init when it detects a debug build. Two
further cost levers already built in: a scoped `Locator::find_by_text` crops
the frame to the parent's bounds *before* inference (fewer pixels, fewer text
lines — only the unscoped `Session::find_by_text` pays for the full frame),
and the per-frame OCR cache means repeated lookups on an unchanged screen
reuse a single pass.

## When to use what

- **Default path** — `Locator::click` against an XPath. Use this
  unless the widget doesn't surface in AT-SPI.
- **Widget renders text and isn't in AT-SPI** — `Locator::find_by_text`
  on the nearest AT-SPI parent, then `.click()`. Works when the text
  glyphs are inside the gesture-controller's hit-rect (most
  `AdwButtonRow`s, GTK buttons with centred labels).
- **Text-center click doesn't fire activation** — `parent.last_region(&text).click()`.
  Uses the centroid of the enclosing visual shape, which is more
  robust for widgets where the inner label widget eats the click.
- **You want the surrounding card / panel, not the button** —
  `parent.first_region(&text).click()` or walk
  `find_regions` and pick the layer you want.
- **No AT-SPI parent at all** — `Session::find_by_text(text).click()`
  works but pays full-screen OCR cost; prefer constraining via
  `.within(rect)` whenever you can derive a scope.

## Failure modes (known)

- **Sibling-coloured regions merge.** If the button shares its fill
  colour with an adjacent widget, flood-fill spans both. Lower
  `tolerance` and re-test.
- **Gradient fills stop the flood early.** A button with a top-to-
  bottom gradient may have RGB deltas exceeding `tolerance` partway
  down. Raise `tolerance` (carefully — too high and the flood eats
  neighbouring regions).
- **Thin antialiased borders ≤ 2 px** can confuse `pick_seed_outside`
  if the 4-px offset lands inside the border. The seed picker
  validates uniformity against a neighbouring pixel and falls back
  to the next candidate, but pathological cases still exist. Construct
  the `VisualLocator` with a tighter `.within(...)` or supply an
  explicit `Rect` to side-step.
- **OCR misreads on small / low-contrast text.** ocrs's recognition
  head is trained on document text; UI labels at 10–14 px in dark
  themes can read poorly. The 32 px context-padding ring helps
  (tunable via `VisualTextTuning::ocr_context_padding_px`);
  raising the fixture's font size if you control it helps more.
- **Pointer cold-start race.** Headless mutter sometimes drops the
  first pointer event after a fresh session. `VisualLocator::click`
  and `RegionLocator::click` both warmup-motion-then-click to
  side-step it, but a test that triggers many rapid clicks can still
  hit the race on subsequent clicks. Add a 60 ms sleep between
  clicks if you see this — or tune `VisualClickTuning` (disable the
  warmup on real hardware, lengthen the settles on slow CI).
- **Custom theme with shadow rasters between rows.** The divider
  scan can mistake anti-aliased shadow gradients for a horizontal
  rule and refuse to merge wrapped paragraphs. Set
  `VisualTextTuning::divider_detection_enabled = false` to fall
  back to bg-colour-only boundary detection.
- **Stale model cache from upstream rebuild.** SHA-256 verification
  refuses to load model files that don't match the embedded
  hashes. If ocrs publishes new models, either bump the constants
  in `models.rs` or set `WAYDRIVER_OCRS_DETECTION_MODEL` /
  `WAYDRIVER_OCRS_RECOGNITION_MODEL` to point at known-good
  files.
- **Right-to-left scripts and non-LTR reading order.** The block
  grouper and the per-line haystack are built on the assumption
  that words read left-to-right within a line and lines read
  top-to-bottom within a block. Hebrew, Arabic, or any RTL script
  will produce word bboxes in screen-left-to-right order but the
  joined haystack won't reflect logical reading order — substring
  matches against a logical-order needle may miss. Vertical
  scripts (Japanese/Chinese in tategaki) are not supported. If
  you're driving an RTL app, prefer AT-SPI selectors; the visual
  locator's matching semantics aren't right for that case.

## Implementation map

| What                                          | Where                                                                     |
|-----------------------------------------------|---------------------------------------------------------------------------|
| `Session::find_by_text` (root entry)          | `crates/waydriver/src/session.rs`                                         |
| `Locator::find_by_text` (scoped entry)        | `crates/waydriver/src/locator.rs`                                         |
| `VisualLocator` + OCR pipeline                | `crates/waydriver/src/visual/mod.rs`                                      |
| Model resolution + auto-download              | `crates/waydriver/src/visual/models.rs`                                   |
| Engine lifecycle (`OnceCell` shared cache)    | `crates/waydriver/src/visual/engine.rs`                                   |
| Flood-fill, seed picking, `RegionLocator`     | `crates/waydriver/src/visual/region.rs`                                   |
| `Locator::find_regions/first_region/last_region` | `crates/waydriver/src/locator.rs`                                      |
| `SessionConfig::visual_region_tuning`         | `crates/waydriver/src/session.rs`                                         |
| Cargo feature `visual`                        | `crates/waydriver/Cargo.toml`                                             |
| E2E test exercising both pipelines            | `crates/waydriver-e2e/tests/e2e.rs` — `lazy_a11y_*_clickable_via_visual_locator` |
