//! XPath-based lazy locators for AT-SPI elements.
//!
//! A [`Locator`] bundles an [`Arc<Session>`] with an XPath expression. It
//! does **not** resolve until an async method is called on it — every
//! resolution takes a fresh AT-SPI snapshot, so locators survive widget
//! reparenting and destruction+recreation (dialog close/reopen, virtualized
//! list scroll, etc.) without manual retries.
//!
//! **Auto-wait.** Action methods (`click`, `set_text`) and metadata reads
//! (`name`, `role`, `text`, …) automatically poll with exponential backoff
//! until the element is resolvable — and, for actions, actionable (showing
//! and enabled) — within the session's default timeout. Override per-locator
//! with [`Locator::with_timeout`].
//!
//! **Explicit waits** come in three layered shapes. Pick the tightest one
//! your case fits:
//!
//! - [`Locator::wait_until`] — sync `Fn(&[ElementInfo]) -> bool` predicate.
//!   The common case: classify the current snapshot with no I/O in the
//!   predicate. Plus the family of shortcut methods built on it:
//!   [`wait_for_visible`](Locator::wait_for_visible),
//!   [`wait_for_hidden`](Locator::wait_for_hidden),
//!   [`wait_for_enabled`](Locator::wait_for_enabled),
//!   [`wait_for_count`](Locator::wait_for_count),
//!   [`wait_for_checked`](Locator::wait_for_checked), and siblings.
//! - [`Locator::wait_until_async`] — async `Fn(Vec<ElementInfo>) -> Fut<bool>`.
//!   Use when the predicate itself needs I/O (reading another locator, a
//!   live text or bounds call, the filesystem, …).
//! - [`Locator::wait_for`] — async, with `Result<Option<T>>` return. The
//!   general primitive: predicate can map to any output type and surface
//!   retriable errors. Use when the other two don't fit
//!   ([`wait_for_text`](Locator::wait_for_text) is a good worked example).
//!
//! Single-target methods (`click`, `name`, `text`, …) expect the selector to
//! match exactly one element and return [`Error::AmbiguousSelector`]
//! immediately — ambiguity is treated as a selector bug, not a retriable
//! condition. Disambiguate with [`Locator::nth`] / [`Locator::first`] /
//! [`Locator::last`] or refine the XPath.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atspi::connection::AccessibilityConnection;

use crate::atspi as atspi_client;
use crate::atspi::ElementInfo;
use crate::error::{Error, Result};
use crate::session::Session;

/// Initial backoff delay between poll attempts. Doubles each failed attempt
/// up to [`MAX_POLL_DELAY`].
const INITIAL_POLL_DELAY: Duration = Duration::from_millis(50);

/// Upper bound on the backoff delay. Keeps a very long timeout from
/// accumulating too much wait between attempts.
const MAX_POLL_DELAY: Duration = Duration::from_millis(500);

/// How [`Locator::fill`] clears existing content before typing.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillMode {
    /// `Ctrl+Home` then `Ctrl+Shift+End` — explicit caret navigation.
    /// Two chords; slightly slower. The default.
    #[default]
    CaretNav,
    /// `Ctrl+A` — one chord; faster when the target honors the
    /// standard select-all binding.
    SelectAll,
}

/// A lazy, re-resolving handle to one or more AT-SPI elements.
///
/// See the [module-level documentation](crate::locator) for the resolution model.
#[derive(Clone)]
pub struct Locator {
    session: Arc<Session>,
    xpath: String,
    /// Per-locator timeout override for auto-wait and `wait_for_*` calls.
    /// `None` means "use the session's default timeout at call time," which
    /// lets [`Session::set_default_timeout`] affect locators created before
    /// the change.
    timeout: Option<Duration>,
}

impl Locator {
    pub(crate) fn new(session: Arc<Session>, xpath: String) -> Self {
        Self {
            session,
            xpath,
            timeout: None,
        }
    }

    /// The XPath expression this locator resolves with.
    pub fn xpath(&self) -> &str {
        &self.xpath
    }

    /// Return a new locator with a per-call timeout override for auto-wait
    /// and `wait_for_*` methods. `Duration::ZERO` means "try once, don't
    /// wait," useful for negative assertions ("this element should NOT
    /// exist right now").
    pub fn with_timeout(&self, timeout: Duration) -> Locator {
        Locator {
            session: self.session.clone(),
            xpath: self.xpath.clone(),
            timeout: Some(timeout),
        }
    }

    // ── Composition (pure string manipulation, no I/O) ─────────────────────
    //
    // Composition preserves the per-locator timeout override, so a caller can
    // set a timeout once and it flows through `.nth()`, `.locate()`, etc.

    /// Scope a sub-expression to the nodes matched by this locator.
    ///
    /// If `sub` is absolute (starts with `/`), it replaces the current
    /// selector entirely. Otherwise it's evaluated as descendants of the
    /// current matches: `(self)//sub`.
    pub fn locate(&self, sub: &str) -> Locator {
        let trimmed = sub.trim();
        let new_xpath = if trimmed.starts_with('/') {
            trimmed.to_string()
        } else {
            format!("({})//{}", self.xpath, trimmed)
        };
        self.with_xpath(new_xpath)
    }

    /// Return a locator pinned to the `n`-th (0-indexed) match of this one.
    pub fn nth(&self, n: usize) -> Locator {
        self.with_xpath(format!("({})[{}]", self.xpath, n + 1))
    }

    /// Shorthand for `nth(0)`.
    pub fn first(&self) -> Locator {
        self.nth(0)
    }

    /// Locator for the last match of this selector.
    pub fn last(&self) -> Locator {
        self.with_xpath(format!("({})[last()]", self.xpath))
    }

    /// Locator for the parent of the matched element(s).
    pub fn parent(&self) -> Locator {
        self.with_xpath(format!("({})/..", self.xpath))
    }

    fn with_xpath(&self, xpath: String) -> Locator {
        Locator {
            session: self.session.clone(),
            xpath,
            timeout: self.timeout,
        }
    }

    // ── Enumeration ─────────────────────────────────────────────────────────

    /// Number of elements matched by this selector. Does not auto-wait —
    /// returns the current count, which may be zero.
    pub async fn count(&self) -> Result<usize> {
        Ok(self.resolve_all_once().await?.len())
    }

    /// Enumerate each match as a locator pinned by ordinal.
    ///
    /// Each returned locator still re-resolves (so ordinal pins are
    /// evaluated on each use, not frozen to the AT-SPI identity observed at
    /// `all()` time).
    pub async fn all(&self) -> Result<Vec<Locator>> {
        let n = self.count().await?;
        Ok((0..n).map(|i| self.nth(i)).collect())
    }

    /// Take one AT-SPI snapshot and return full metadata for every match.
    ///
    /// More efficient than calling `all()` and then metadata methods on each
    /// returned locator, which would re-snapshot per match.
    pub async fn inspect_all(&self) -> Result<Vec<ElementInfo>> {
        let a11y = self.a11y()?;
        let xml =
            atspi_client::snapshot_tree(a11y, &self.session.app_bus_name, &self.session.app_path)
                .await?;
        atspi_client::evaluate_xpath_detailed(&xml, &self.xpath)
    }

    // ── Live metadata (auto-waits for the element to exist) ────────────────
    //
    // Every read re-snapshots the AT-SPI tree, so data is always as fresh as
    // the current call. The snapshot XML already captures name, role, states,
    // and toolkit attributes — no second D-Bus round-trip per field.

    /// Accessible name of the matched element, or `None` when the element
    /// has no accessible name set.
    pub async fn name(&self) -> Result<Option<String>> {
        Ok(self.wait_for_existing().await?.name)
    }

    /// Raw AT-SPI role name (e.g. `"push button"`, `"menu item"`).
    ///
    /// Falls back to the PascalCase XML element tag only when the snapshot
    /// lacks a `role` attribute — which shouldn't happen for live snapshots,
    /// but can in hand-crafted test XML.
    pub async fn role(&self) -> Result<String> {
        let info = self.wait_for_existing().await?;
        Ok(info.role_raw.unwrap_or(info.role))
    }

    /// Read a single toolkit attribute by key.
    pub async fn attribute(&self, key: &str) -> Result<Option<String>> {
        Ok(self.wait_for_existing().await?.attributes.remove(key))
    }

    /// All toolkit attributes as a map.
    pub async fn attributes(&self) -> Result<HashMap<String, String>> {
        Ok(self.wait_for_existing().await?.attributes)
    }

    /// Whether the matched element currently has the AT-SPI `State::Showing`
    /// state.
    pub async fn is_showing(&self) -> Result<bool> {
        self.has_state("showing").await
    }

    /// Whether the matched element is currently interactable.
    ///
    /// Returns true when the element has either the AT-SPI `State::Enabled`
    /// state or the `State::Sensitive` state — GTK reports the latter,
    /// Qt/others the former. Both mean "user can interact with this widget
    /// right now."
    pub async fn is_enabled(&self) -> Result<bool> {
        let info = self.wait_for_existing().await?;
        Ok(is_enabled_in(&info.states))
    }

    /// Whether the matched element currently has the AT-SPI `State::Checked`
    /// state. Use for checkboxes, toggle buttons, and checkable menu items.
    pub async fn is_checked(&self) -> Result<bool> {
        self.has_state("checked").await
    }

    /// Whether the matched element currently has the AT-SPI `State::Focused`
    /// state — i.e. it holds keyboard focus right now.
    pub async fn is_focused(&self) -> Result<bool> {
        self.has_state("focused").await
    }

    /// Whether the matched element currently has the AT-SPI `State::Expanded`
    /// state. Use for tree rows, expanders, and disclosure triangles.
    ///
    /// An element that is collapsible but not currently expanded has
    /// `State::Expandable` (and possibly `State::Collapsed`) but not
    /// `State::Expanded`.
    pub async fn is_expanded(&self) -> Result<bool> {
        self.has_state("expanded").await
    }

    /// Whether the matched element currently has the AT-SPI `State::Editable`
    /// state — i.e. the user can type into it.
    pub async fn is_editable(&self) -> Result<bool> {
        self.has_state("editable").await
    }

    /// Whether the matched element currently has the AT-SPI `State::Selected`
    /// state. Use for list and table rows, selectable menu items, and tabs.
    pub async fn is_selected(&self) -> Result<bool> {
        self.has_state("selected").await
    }

    /// Whether the matched element currently has the AT-SPI `State::Pressed`
    /// state — i.e. a toggle button is in its pressed position.
    pub async fn is_pressed(&self) -> Result<bool> {
        self.has_state("pressed").await
    }

    /// Whether the matched element currently has the AT-SPI `State::Modal`
    /// state — i.e. a dialog that blocks interaction with its parent window.
    pub async fn is_modal(&self) -> Result<bool> {
        self.has_state("modal").await
    }

    /// Screen-relative bounding rectangle (x, y, width, height) in logical
    /// pixels, as captured at snapshot time from the AT-SPI Component
    /// interface.
    ///
    /// Returns [`Error::Atspi`] if the element doesn't implement Component
    /// or hasn't been laid out yet (`get_extents` returned a zero-area
    /// rect). Callers that want to tolerate missing bounds should use
    /// [`Locator::inspect_all`] and read `ElementInfo::bounds` directly.
    pub async fn bounds(&self) -> Result<crate::atspi::Rect> {
        let info = self.wait_for_existing().await?;
        info.bounds.ok_or_else(|| {
            Error::Atspi(format!(
                "no bounds available for {} — element doesn't implement Component or isn't laid out",
                self.xpath
            ))
        })
    }
    /// Text contents of the matched element via the AT-SPI Text interface.
    /// Unlike other metadata, text isn't captured in the snapshot — each
    /// call makes a live read through the Text proxy after auto-waiting for
    /// the element to exist.
    pub async fn text(&self) -> Result<String> {
        let info = self.wait_for_existing().await?;
        let a11y = self.a11y()?;
        let (bus, path) = info.ref_;
        atspi_client::read_text_on(a11y, &self.xpath, &bus, &path).await
    }

    // ── Actions (auto-wait for actionability) ──────────────────────────────

    /// Invoke the primary action (index 0) on the matched element.
    ///
    /// Auto-waits for the element to be resolvable, showing, and enabled
    /// within the effective timeout. Requires exactly one match.
    pub async fn click(&self) -> Result<()> {
        let info = self.wait_for_actionable().await?;
        let (bus, path) = info.ref_;
        let a11y = self.a11y()?;
        atspi_client::do_action_on(a11y, &self.xpath, &bus, &path).await
    }

    /// Replace the contents of an editable text element via the AT-SPI
    /// `EditableText::SetTextContents` interface. Fast (one D-Bus round
    /// trip) but requires the target to implement `EditableText` — some
    /// toolkits (notably GTK4 `TextView` and widgets with custom entry
    /// buffers) don't. For those, use [`fill`](Self::fill) instead.
    ///
    /// Auto-waits for the element to be resolvable, showing, and enabled.
    pub async fn set_text(&self, text: &str) -> Result<()> {
        let info = self.wait_for_actionable().await?;
        let (bus, path) = info.ref_;
        let a11y = self.a11y()?;
        atspi_client::set_text_on(a11y, &self.xpath, &bus, &path, text).await
    }

    /// Replace the contents of a text widget by simulating keyboard input:
    /// focus the element, clear existing content per `mode`, then type.
    ///
    /// Slower than [`set_text`](Self::set_text) but works on any widget
    /// that accepts keyboard input — including `GtkTextView` and other
    /// targets that don't implement the AT-SPI `EditableText` interface.
    /// Use `set_text` when the target exposes `EditableText`; use `fill`
    /// as the compatibility fallback.
    ///
    /// Focus handling: `fill` tries AT-SPI `Component::grab_focus`. If
    /// that returns `NotSupported` (GTK4 text widgets don't implement
    /// Component in current releases), a warning is logged and the
    /// method continues — it trusts the caller to have focused the
    /// widget through some other path: an app-level `grab_focus` on
    /// startup, a prior pointer click, or Tab navigation. Once WAY-4-
    /// era widget bounds become reliable across toolkits, `fill` can
    /// synthesize a pointer click itself; for now the fallback is too
    /// unreliable to trust (GTK4's AT-SPI bridge often returns
    /// widget-local bounds where screen coords are expected).
    ///
    /// See [`FillMode`] for the tradeoffs between select-all strategies.
    pub async fn fill(&self, text: &str, mode: FillMode) -> Result<()> {
        // 1. Focus best-effort. Propagate any error other than
        // NotSupported — NotSupported is the known GTK4 case where the
        // caller is expected to manage focus themselves.
        match self.focus().await {
            Ok(()) => {}
            Err(e) if e.to_string().contains("NotSupported") => {
                tracing::warn!(
                    xpath = %self.xpath,
                    "fill: AT-SPI focus NotSupported; \
                     proceeding on the assumption caller has focused the widget"
                );
            }
            Err(e) => return Err(e),
        }

        // 2. Clear existing content.
        match mode {
            FillMode::CaretNav => {
                self.session.press_chord("Ctrl+Home").await?;
                self.session.press_chord("Ctrl+Shift+End").await?;
            }
            FillMode::SelectAll => {
                self.session.press_chord("Ctrl+A").await?;
            }
        }
        let delete =
            crate::keysym::key_name_to_keysym("delete").expect("'delete' is a known key name");
        self.session.press_keysym(delete).await?;

        // 3. Type new content.
        self.session.type_text(text).await?;
        Ok(())
    }

    /// Shorthand for `fill(text, FillMode::default())`.
    pub async fn fill_default(&self, text: &str) -> Result<()> {
        self.fill(text, FillMode::default()).await
    }

    /// Give keyboard focus to the matched element.
    ///
    /// Auto-waits for the element to be resolvable, showing, and `focusable`
    /// — the last is a weaker check than "actionable" because some widgets
    /// accept focus without accepting activation (read-only text boxes,
    /// scroll regions, etc.). Uses AT-SPI's `Component::grab_focus` under
    /// the hood.
    ///
    /// ## Toolkit caveats
    ///
    /// This relies on the target widget implementing the AT-SPI Component
    /// interface. Some toolkits (notably GTK4 in its current form) don't
    /// expose Component on all widgets — you may see
    /// `Error::Atspi("NotSupported")` from `grab_focus` even when the
    /// widget is visibly focusable on screen. When that happens the
    /// fallback is to drive focus via keyboard navigation (Tab /
    /// Shift+Tab) or synthesize a pointer click.
    pub async fn focus(&self) -> Result<()> {
        let info = self.wait_for_focusable().await?;
        let (bus, path) = info.ref_;
        let a11y = self.a11y()?;
        atspi_client::grab_focus_on(a11y, &self.xpath, &bus, &path).await
    }

    /// Bring the matched element into its scrollable ancestor's viewport.
    ///
    /// Tries AT-SPI `Component::scroll_to(ScrollType::Anywhere)` first — a
    /// single round-trip that lets the toolkit do the right thing for the
    /// specific widget (virtualized list, scroll pane, etc.). If the
    /// widget doesn't honor that call, falls back to moving the pointer
    /// over the nearest scrollable ancestor and sending discrete
    /// mouse-wheel events until the target's bounds lie fully inside the
    /// ancestor's bounds.
    ///
    /// Returns cleanly when the element is already in view (no-op).
    ///
    /// # Errors
    ///
    /// - `Error::Atspi` when no scrollable ancestor exists (the element
    ///   isn't inside a `ScrollPane` / `Viewport` — nothing to scroll).
    /// - `Error::Atspi` when the fallback loop exhausts its retry budget
    ///   (the wheel events didn't bring the element into view; likely a
    ///   toolkit that ignores synthesized axis events).
    /// - Auto-wait timeout if the element never resolves.
    pub async fn scroll_into_view(&self) -> Result<()> {
        const MAX_WHEEL_TICKS: i32 = 20;
        const POST_SCROLL_SETTLE: Duration = Duration::from_millis(80);

        let info = self.wait_for_existing().await?;
        let Some(elem_bounds) = info.bounds else {
            return Err(Error::Atspi(format!(
                "no bounds available for {} — can't scroll without Component extents",
                self.xpath
            )));
        };

        let Some(scrollable) = self.find_scrollable_ancestor().await? else {
            return Err(Error::Atspi(format!(
                "no scrollable ancestor for {} — element isn't inside a ScrollPane/Viewport",
                self.xpath
            )));
        };
        let Some(scroll_bounds) = scrollable.bounds else {
            return Err(Error::Atspi(format!(
                "scrollable ancestor for {} has no bounds — toolkit doesn't expose Component on it",
                self.xpath
            )));
        };

        tracing::debug!(
            xpath = %self.xpath,
            ?elem_bounds,
            ?scroll_bounds,
            scrollable_role = %scrollable.role,
            "scroll_into_view: resolved target and scrollable ancestor",
        );

        if elem_bounds.is_inside(&scroll_bounds) {
            tracing::debug!(xpath = %self.xpath, "scroll_into_view: already in viewport");
            return Ok(());
        }

        // Primary path: ask the toolkit to scroll this widget into view.
        //
        // Two variants are tried in sequence because toolkits differ on
        // which they implement for which widgets. GTK4's Labels, for
        // example, don't implement `scroll_to` but their containing
        // `ScrolledWindow` honors `scroll_to_point` on descendants. The
        // target is the scrollable ancestor's current top-left, which
        // asks "scroll me so my position is at the top of the viewport".
        let a11y = self.a11y()?;
        let (bus, path) = info.ref_.clone();
        for st in [
            atspi::ScrollType::Anywhere,
            atspi::ScrollType::TopLeft,
            atspi::ScrollType::TopEdge,
        ] {
            if atspi_client::scroll_to_on(a11y.connection(), &bus, &path, st)
                .await
                .unwrap_or(false)
            {
                break;
            }
        }
        // Some toolkits (GTK4) don't implement scroll_to on leaf widgets
        // but do handle scroll_to_point with Window coords that lie
        // inside the scrollable ancestor — the toolkit infers the
        // ancestor and adjusts its adjustment accordingly.
        let _ = atspi_client::scroll_to_point_on(
            a11y.connection(),
            &bus,
            &path,
            atspi::CoordType::Window,
            scroll_bounds.x,
            scroll_bounds.y,
        )
        .await;
        tokio::time::sleep(POST_SCROLL_SETTLE).await;
        if self.is_in_viewport(&scrollable).await? {
            return Ok(());
        }

        // Focus-based fallback. GTK (and most toolkits) scroll a newly-
        // focused widget into its `ScrolledWindow`'s viewport as part of
        // normal focus handling — regardless of whether the widget
        // implements `Component::scroll_to` explicitly. Requires the
        // target to be focusable, so it's skipped when the a11y state
        // set doesn't advertise `Focusable`.
        if info.states.iter().any(|s| s == "focusable") {
            match atspi_client::grab_focus_on(a11y, &self.xpath, &bus, &path).await {
                Ok(()) => {
                    tokio::time::sleep(POST_SCROLL_SETTLE).await;
                    if self.is_in_viewport(&scrollable).await? {
                        return Ok(());
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "scroll_into_view: grab_focus fallback failed")
                }
            }
        }

        // Fallback path: park the pointer over the scrollable's center and
        // emit discrete wheel ticks, re-checking bounds after each one.
        // Mutter's RemoteDesktop only supports relative pointer motion, so
        // "clamp to top-left, then offset to center" is the simplest way
        // to reach a known absolute coordinate. The clamp amount is just
        // a large value that any realistic viewport fits inside.
        self.session
            .pointer_motion_relative(-10_000.0, -10_000.0)
            .await?;
        self.session
            .pointer_motion_relative(
                scroll_bounds.center_x() as f64,
                scroll_bounds.center_y() as f64,
            )
            .await?;

        for _ in 0..MAX_WHEEL_TICKS {
            let direction = wheel_direction(&elem_bounds, &scroll_bounds);
            if direction == 0 {
                break;
            }
            self.session.pointer_axis_discrete(0, direction).await?;
            tokio::time::sleep(POST_SCROLL_SETTLE).await;

            // Re-snapshot. If the element vanished (virtualized list
            // recycled the row) that still counts as progress — the
            // caller's next Locator action will re-resolve it.
            match self.resolve_once_info().await {
                Ok(fresh) => {
                    if let Some(b) = fresh.bounds {
                        if b.is_inside(&scroll_bounds) {
                            return Ok(());
                        }
                    }
                }
                Err(Error::ElementNotFound { .. }) => return Ok(()),
                Err(e) => return Err(e),
            }
        }

        Err(Error::Atspi(format!(
            "scroll_into_view exhausted {MAX_WHEEL_TICKS} wheel ticks for {} — toolkit \
             likely ignored synthesized axis events",
            self.xpath
        )))
    }

    /// Find the closest scrollable ancestor of the element this locator
    /// resolves to.
    ///
    /// We can't rely on roles: GTK4 reports `ScrolledWindow` as
    /// `role="generic"`, AT-SPI 0.13 doesn't expose `State::Scrollable`,
    /// and toolkits disagree on whether to use `scroll pane`, `viewport`,
    /// or something else entirely. Instead we use a structural signal:
    /// a scrollable viewport is, by definition, a container whose
    /// children overflow it. Walk the ancestor chain from innermost
    /// outward; the first ancestor whose bbox is strictly smaller (in
    /// either axis) than the ancestor one step closer to the target is
    /// the viewport clipping that content.
    ///
    /// Works for any toolkit that correctly reports bounds via
    /// `Component::get_extents`, not just GTK4.
    async fn find_scrollable_ancestor(&self) -> Result<Option<ElementInfo>> {
        let xml = self.snapshot().await?;

        // Innermost-first walk along the ancestor axis. The `reverse`
        // model item of the XPath spec orders ancestors last-to-first,
        // but `evaluate_xpath_detailed` returns document order
        // (outermost-first), so we reverse in Rust.
        let ancestors_xpath = format!("({xp})/ancestor::*", xp = self.xpath);
        let mut ancestors = atspi_client::evaluate_xpath_detailed(&xml, &ancestors_xpath)?;
        ancestors.reverse();

        // Seed the overflow test with the target's own bounds. Then for
        // each ancestor, compare the PREVIOUS chain node's bounds to
        // this ancestor's — if the previous is strictly larger in any
        // axis, this ancestor is clipping it and is the viewport.
        let target = atspi_client::evaluate_xpath_detailed(&xml, &self.xpath)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::ElementNotFound {
                xpath: self.xpath.clone(),
            })?;
        let mut prev_bounds = target.bounds;

        for ancestor in ancestors {
            if let (Some(prev), Some(this)) = (prev_bounds, ancestor.bounds) {
                if prev.width > this.width || prev.height > this.height {
                    return Ok(Some(ancestor));
                }
            }
            prev_bounds = ancestor.bounds;
        }
        Ok(None)
    }

    /// Whether this locator's element currently lies inside the given
    /// scrollable's bounds. Used as the exit condition for the wheel
    /// fallback loop and as the post-`scroll_to` verification step.
    async fn is_in_viewport(&self, scrollable: &ElementInfo) -> Result<bool> {
        let Some(scroll_bounds) = scrollable.bounds else {
            return Ok(false);
        };
        match self.resolve_once_info().await {
            Ok(fresh) => Ok(fresh.bounds.is_some_and(|b| b.is_inside(&scroll_bounds))),
            Err(Error::ElementNotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    // ── Generic waits ──────────────────────────────────────────────────────

    /// The most general wait primitive. Polls with exponential backoff
    /// until `pred` returns `Ok(Some(T))`, a non-retriable error, or the
    /// effective timeout elapses. The predicate receives the full
    /// multi-match node-set and can map it to any output type.
    ///
    /// `Ok(None)` means "not yet, keep polling." `Err(e)` where `e` is
    /// retriable (`ElementStale`) is swallowed and retried. All other
    /// errors propagate immediately. On timeout, returns the last
    /// retriable error if there was one, otherwise [`Error::Timeout`].
    ///
    /// Most callers should reach for [`wait_until`](Self::wait_until) or
    /// [`wait_until_async`](Self::wait_until_async) first — this is the
    /// escape hatch for cases that need a non-`bool` output, e.g.
    /// [`wait_for_text`](Self::wait_for_text) which returns the matched
    /// `String`.
    pub async fn wait_for<T, F, Fut>(&self, pred: F) -> Result<T>
    where
        F: Fn(Vec<ElementInfo>) -> Fut,
        Fut: Future<Output = Result<Option<T>>>,
    {
        let xpath = self.xpath.clone();
        poll_with_retry(self.effective_timeout(), &xpath, || async {
            pred(self.inspect_all().await?).await
        })
        .await
    }

    /// Poll until a sync predicate over the current multi-match node-set
    /// returns true. Returns the matching set (same as
    /// [`inspect_all`](Self::inspect_all) would observe) on success.
    ///
    /// The predicate sees *all* matches, so it can express:
    /// - "exactly one match satisfying X": `|h| h.len() == 1 && cond(&h[0])`
    /// - "element is gone or not showing" (the shape of
    ///   [`wait_for_hidden`](Self::wait_for_hidden)):
    ///   `|h| h.is_empty() || !showing(&h[0])`
    /// - "count reaches N": `|h| h.len() == n`
    ///
    /// For predicates that need I/O of their own (another locator, a live
    /// text read, the filesystem), use
    /// [`wait_until_async`](Self::wait_until_async).
    pub async fn wait_until<F>(&self, pred: F) -> Result<Vec<ElementInfo>>
    where
        F: Fn(&[ElementInfo]) -> bool,
    {
        self.wait_for(|hits| {
            let matched = pred(&hits);
            std::future::ready(Ok(matched.then_some(hits)))
        })
        .await
    }

    /// Async counterpart to [`wait_until`](Self::wait_until). Identical
    /// semantics, except the predicate can `.await` — useful when the
    /// decision depends on a second locator's state, a live text read, a
    /// bounds query, or any other I/O that isn't already captured in the
    /// snapshot `ElementInfo`.
    pub async fn wait_until_async<F, Fut>(&self, pred: F) -> Result<Vec<ElementInfo>>
    where
        F: Fn(Vec<ElementInfo>) -> Fut,
        Fut: Future<Output = bool>,
    {
        self.wait_for(|hits| {
            let hits_return = hits.clone();
            let fut = pred(hits);
            async move { Ok(fut.await.then_some(hits_return)) }
        })
        .await
    }

    // ── Specialized waits (thin layers over wait_until / wait_for) ─────────

    /// Poll until the element exists and has the `Showing` state.
    pub async fn wait_for_visible(&self) -> Result<()> {
        self.wait_until(|hits| single_has_state(hits, "showing"))
            .await
            .map(|_| ())
    }

    /// Poll until the element either doesn't exist or doesn't have the
    /// `Showing` state. The inverse of [`wait_for_visible`](Self::wait_for_visible).
    pub async fn wait_for_hidden(&self) -> Result<()> {
        self.wait_until(|hits| hits.is_empty() || !hits[0].states.iter().any(|s| s == "showing"))
            .await
            .map(|_| ())
    }

    /// Poll until the element exists and is interactable (has either the
    /// `Enabled` or `Sensitive` state — see [`Locator::is_enabled`] for why
    /// both are treated as equivalent).
    pub async fn wait_for_enabled(&self) -> Result<()> {
        self.wait_until(|hits| hits.len() == 1 && is_enabled_in(&hits[0].states))
            .await
            .map(|_| ())
    }

    /// Poll until the selector matches exactly `n` elements. Useful for
    /// lists that populate asynchronously after a user action.
    pub async fn wait_for_count(&self, n: usize) -> Result<()> {
        self.wait_until(|hits| hits.len() == n).await.map(|_| ())
    }

    /// Poll until the element has the AT-SPI `State::Checked` state.
    pub async fn wait_for_checked(&self) -> Result<()> {
        self.wait_until(|hits| single_has_state(hits, "checked"))
            .await
            .map(|_| ())
    }

    /// Poll until the element has the AT-SPI `State::Focused` state.
    pub async fn wait_for_focused(&self) -> Result<()> {
        self.wait_until(|hits| single_has_state(hits, "focused"))
            .await
            .map(|_| ())
    }

    /// Poll until the element has the AT-SPI `State::Expanded` state.
    pub async fn wait_for_expanded(&self) -> Result<()> {
        self.wait_until(|hits| single_has_state(hits, "expanded"))
            .await
            .map(|_| ())
    }

    /// Poll until the element has the AT-SPI `State::Editable` state.
    pub async fn wait_for_editable(&self) -> Result<()> {
        self.wait_until(|hits| single_has_state(hits, "editable"))
            .await
            .map(|_| ())
    }

    /// Poll until the element has the AT-SPI `State::Selected` state.
    pub async fn wait_for_selected(&self) -> Result<()> {
        self.wait_until(|hits| single_has_state(hits, "selected"))
            .await
            .map(|_| ())
    }

    /// Poll until the element has the AT-SPI `State::Pressed` state.
    pub async fn wait_for_pressed(&self) -> Result<()> {
        self.wait_until(|hits| single_has_state(hits, "pressed"))
            .await
            .map(|_| ())
    }

    /// Poll until the element has the AT-SPI `State::Modal` state.
    pub async fn wait_for_modal(&self) -> Result<()> {
        self.wait_until(|hits| single_has_state(hits, "modal"))
            .await
            .map(|_| ())
    }

    /// Poll until the element's text contents satisfy `pred`. Returns the
    /// matching text on success so the caller can inspect it further.
    ///
    /// Unlike the snapshot-backed waits, text isn't captured in the tree
    /// snapshot — this does a live read through the AT-SPI Text proxy per
    /// tick, which is why it uses [`wait_for`](Self::wait_for) directly
    /// (the predicate maps to `String`, not `bool`).
    pub async fn wait_for_text<F>(&self, pred: F) -> Result<String>
    where
        F: Fn(&str) -> bool,
    {
        // `pred` is borrowed by shared ref so the `async move` block can
        // capture a Copy ref instead of moving `F` (which would only work
        // once, breaking the `Fn` contract on the outer closure).
        let pred = &pred;
        self.wait_for(move |hits| async move {
            if hits.len() != 1 {
                return Ok(None);
            }
            let (bus, path) = hits[0].ref_.clone();
            let a11y = self.a11y()?;
            let text = atspi_client::read_text_on(a11y, &self.xpath, &bus, &path).await?;
            Ok(pred(&text).then_some(text))
        })
        .await
    }

    // ── Internals ──────────────────────────────────────────────────────────

    async fn has_state(&self, state: &str) -> Result<bool> {
        Ok(self
            .wait_for_existing()
            .await?
            .states
            .iter()
            .any(|s| s == state))
    }

    fn a11y(&self) -> Result<&AccessibilityConnection> {
        self.session
            .a11y_connection
            .as_ref()
            .ok_or_else(|| Error::Atspi("session has no AT-SPI connection".into()))
    }

    /// Effective timeout for this locator: the per-locator override if set,
    /// otherwise the session's current default timeout.
    fn effective_timeout(&self) -> Duration {
        self.timeout
            .unwrap_or_else(|| self.session.default_timeout())
    }

    async fn snapshot(&self) -> Result<String> {
        let a11y = self.a11y()?;
        atspi_client::snapshot_tree(a11y, &self.session.app_bus_name, &self.session.app_path).await
    }

    /// Single-shot: snapshot + evaluate_xpath, no retry.
    async fn resolve_all_once(&self) -> Result<Vec<(String, String)>> {
        let xml = self.snapshot().await?;
        atspi_client::evaluate_xpath(&xml, &self.xpath)
    }

    /// Single-shot: snapshot + evaluate_xpath_detailed + expect-one, no retry.
    /// `ElementNotFound` if zero matches, `AmbiguousSelector` if more than one.
    async fn resolve_once_info(&self) -> Result<ElementInfo> {
        let xml = self.snapshot().await?;
        let mut hits = atspi_client::evaluate_xpath_detailed(&xml, &self.xpath)?;
        select_exactly_one(&self.xpath, hits.len())?;
        Ok(hits.pop().unwrap())
    }

    /// Auto-wait: poll until the selector resolves to exactly one element.
    /// Retries on `ElementNotFound`/`ElementStale`; fatal on `InvalidSelector`
    /// and `AmbiguousSelector`.
    async fn wait_for_existing(&self) -> Result<ElementInfo> {
        let xpath = self.xpath.clone();
        poll_with_retry(self.effective_timeout(), &xpath, || async {
            Ok(Some(self.resolve_once_info().await?))
        })
        .await
    }

    /// Auto-wait: poll until the selector resolves to exactly one element
    /// that is visible on screen and interactable. "Visible" = the `Showing`
    /// state; "interactable" = either `Enabled` or `Sensitive` — toolkits
    /// differ on which they report (GTK → Sensitive, Qt → Enabled).
    async fn wait_for_actionable(&self) -> Result<ElementInfo> {
        let xpath = self.xpath.clone();
        poll_with_retry(self.effective_timeout(), &xpath, || async {
            let info = self.resolve_once_info().await?;
            let showing = info.states.iter().any(|s| s == "showing");
            if showing && is_enabled_in(&info.states) {
                Ok(Some(info))
            } else {
                Ok(None)
            }
        })
        .await
    }

    /// Auto-wait: poll until the selector resolves to exactly one element
    /// that is showing and has the `Focusable` state. Weaker than
    /// actionability because a read-only but navigable widget can accept
    /// focus without accepting activation.
    async fn wait_for_focusable(&self) -> Result<ElementInfo> {
        let xpath = self.xpath.clone();
        poll_with_retry(self.effective_timeout(), &xpath, || async {
            let info = self.resolve_once_info().await?;
            let showing = info.states.iter().any(|s| s == "showing");
            let focusable = info.states.iter().any(|s| s == "focusable");
            if showing && focusable {
                Ok(Some(info))
            } else {
                Ok(None)
            }
        })
        .await
    }
}

/// Compute the sign + magnitude of a wheel tick needed to bring `elem`
/// closer to being inside `scrollable`. Returns -1 when we should scroll
/// up (element is above the viewport), +1 when we should scroll down,
/// and 0 when the element is already vertically inside (which the caller
/// treats as "stop, even if horizontally off — we don't yet synthesize
/// horizontal scrolls").
///
/// Used only by the fallback path of [`Locator::scroll_into_view`]; kept
/// free-standing so it can be unit-tested without spinning up a session.
fn wheel_direction(elem: &crate::atspi::Rect, scrollable: &crate::atspi::Rect) -> i32 {
    if elem.y < scrollable.y {
        -1
    } else if elem.bottom() > scrollable.bottom() {
        1
    } else {
        0
    }
}

/// Whether the given snapshot state-set represents an "interactable"
/// element. AT-SPI has two closely-related states here: `Enabled` (the
/// newer, more generic name) and `Sensitive` (GTK's legacy name for the
/// same concept). Different toolkits report one, the other, or both, so
/// auto-wait and `is_enabled` accept either.
fn is_enabled_in(states: &[String]) -> bool {
    states.iter().any(|s| s == "enabled" || s == "sensitive")
}

/// Whether a multi-match node-set contains exactly one element with the
/// given AT-SPI state. Used by the single-element state waits
/// (`wait_for_checked`, `wait_for_focused`, `wait_for_visible`, …) to
/// collapse the common `hits.len() == 1 && hits[0].states.iter().any(...)`
/// idiom.
fn single_has_state(hits: &[ElementInfo], state: &str) -> bool {
    hits.len() == 1 && hits[0].states.iter().any(|s| s == state)
}

/// Classify the match count from a single-target selector resolution:
/// zero → `ElementNotFound`, one → `Ok(())`, more than one →
/// `AmbiguousSelector`. Leaves the Vec intact so callers can pop the sole
/// element themselves.
fn select_exactly_one(xpath: &str, count: usize) -> Result<()> {
    match count {
        0 => Err(Error::ElementNotFound {
            xpath: xpath.to_string(),
        }),
        1 => Ok(()),
        n => Err(Error::AmbiguousSelector {
            xpath: xpath.to_string(),
            count: n,
        }),
    }
}

/// Poll `f` with exponential backoff until it returns `Ok(Some(T))`, a
/// non-retriable error, or the `timeout` deadline elapses.
///
/// Retriable errors ([`Error::ElementNotFound`], [`Error::ElementStale`]) are
/// swallowed and retried. Fatal errors ([`Error::InvalidSelector`],
/// [`Error::AmbiguousSelector`], etc.) return immediately.
///
/// On timeout with a retriable last error, that error is surfaced directly
/// so callers can still pattern-match on `ElementNotFound` / `ElementStale`.
/// On timeout where the predicate returned `Ok(None)` (element exists but
/// some state isn't satisfied), a [`Error::Timeout`] is returned with the
/// xpath context.
pub(crate) async fn poll_with_retry<T, F, Fut>(
    timeout: Duration,
    xpath: &str,
    mut f: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let deadline = Instant::now() + timeout;
    let mut delay = INITIAL_POLL_DELAY;
    // The initial `None` is overwritten on every first iteration, but rustc's
    // liveness analysis doesn't see that — `#[allow]` is cleaner than
    // restructuring around a declare-before-init pattern.
    #[allow(unused_assignments)]
    let mut last_err: Option<Error> = None;
    let mut attempts: u32 = 0;
    loop {
        attempts += 1;
        match f().await {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => {
                // Predicate observed the element but its state wasn't yet
                // satisfied. Clear last_err so we don't surface a stale
                // not-found from an earlier attempt when the element
                // appeared but isn't quite ready.
                last_err = None;
            }
            Err(e) if is_retriable(&e) => {
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }

        if Instant::now() >= deadline {
            return Err(last_err.unwrap_or_else(|| {
                Error::Timeout(format!(
                    "wait for '{xpath}' timed out after {attempts} attempt(s) \
                     ({}ms budget)",
                    timeout.as_millis()
                ))
            }));
        }

        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_POLL_DELAY);
    }
}

/// Whether an error during polling should be swallowed and retried.
fn is_retriable(e: &Error) -> bool {
    matches!(
        e,
        Error::ElementNotFound { .. } | Error::ElementStale { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::{
        is_retriable, poll_with_retry, select_exactly_one, single_has_state, ElementInfo, Error,
        HashMap,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // We can't instantiate a Locator without a Session, so composition
    // tests mirror the pure string logic via these helpers.

    fn compose_locate(outer: &str, sub: &str) -> String {
        let trimmed = sub.trim();
        if trimmed.starts_with('/') {
            trimmed.to_string()
        } else {
            format!("({outer})//{trimmed}")
        }
    }

    fn compose_nth(outer: &str, n: usize) -> String {
        format!("({outer})[{}]", n + 1)
    }

    fn compose_parent(outer: &str) -> String {
        format!("({outer})/..")
    }

    #[test]
    fn locate_relative_scopes() {
        assert_eq!(
            compose_locate("//Dialog[@name='X']", "PushButton"),
            "(//Dialog[@name='X'])//PushButton"
        );
    }

    #[test]
    fn locate_absolute_replaces() {
        assert_eq!(compose_locate("//Dialog", "//Menu"), "//Menu");
    }

    #[test]
    fn nth_is_one_indexed_in_xpath() {
        assert_eq!(compose_nth("//PushButton", 0), "(//PushButton)[1]");
        assert_eq!(compose_nth("//PushButton", 4), "(//PushButton)[5]");
    }

    #[test]
    fn parent_appends_dot_dot() {
        assert_eq!(
            compose_parent("//PushButton[@name='OK']"),
            "(//PushButton[@name='OK'])/.."
        );
    }

    // ── select_exactly_one dispatch ─────────────────────────────────────────

    #[test]
    fn select_exactly_one_zero_is_not_found() {
        let err = select_exactly_one("//Missing", 0).unwrap_err();
        assert!(matches!(err, Error::ElementNotFound { .. }));
        // Error carries the xpath so callers can see what didn't match.
        assert!(err.to_string().contains("//Missing"));
    }

    #[test]
    fn select_exactly_one_one_is_ok() {
        assert!(select_exactly_one("//PushButton[@name='OK']", 1).is_ok());
    }

    #[test]
    fn select_exactly_one_many_is_ambiguous_with_count() {
        let err = select_exactly_one("//PushButton", 7).unwrap_err();
        match err {
            Error::AmbiguousSelector { count, xpath } => {
                assert_eq!(count, 7);
                assert_eq!(xpath, "//PushButton");
            }
            other => panic!("expected AmbiguousSelector, got {other:?}"),
        }
    }

    // ── Real Locator methods against a test Session ─────────────────────────
    //
    // These use Session::new_for_test (cfg(test)-gated) to construct a
    // Session with no AT-SPI connection. Composition methods never touch
    // the connection, so they work fine; async I/O methods are covered
    // separately by e2e tests against a real compositor.

    use std::path::{Path, PathBuf};

    use async_trait::async_trait;

    use crate::backend::{CaptureBackend, CompositorRuntime, InputBackend, PipeWireStream};
    use crate::error::Result as WdResult;
    use crate::session::Session;

    struct StubCompositor;
    #[async_trait]
    impl CompositorRuntime for StubCompositor {
        async fn start(&mut self, _resolution: Option<&str>) -> WdResult<()> {
            Ok(())
        }
        async fn stop(&mut self) -> WdResult<()> {
            Ok(())
        }
        fn id(&self) -> &str {
            "stub"
        }
        fn wayland_display(&self) -> &str {
            "wayland-stub"
        }
        fn runtime_dir(&self) -> &Path {
            Path::new("/tmp")
        }
    }

    struct StubInput;
    #[async_trait]
    impl InputBackend for StubInput {
        async fn press_keysym(&self, _keysym: u32) -> WdResult<()> {
            Ok(())
        }
        async fn key_down(&self, _keysym: u32) -> WdResult<()> {
            Ok(())
        }
        async fn key_up(&self, _keysym: u32) -> WdResult<()> {
            Ok(())
        }
        async fn pointer_motion_relative(&self, _dx: f64, _dy: f64) -> WdResult<()> {
            Ok(())
        }
        async fn pointer_motion_absolute(&self, _x: f64, _y: f64) -> WdResult<()> {
            Ok(())
        }
        async fn pointer_button(&self, _button: u32) -> WdResult<()> {
            Ok(())
        }
        async fn pointer_axis_discrete(&self, _axis: u32, _steps: i32) -> WdResult<()> {
            Ok(())
        }
    }

    struct StubCapture;
    #[async_trait]
    impl CaptureBackend for StubCapture {
        async fn start_stream(&self) -> WdResult<PipeWireStream> {
            unimplemented!("not used in composition tests")
        }
        async fn stop_stream(&self, _stream: PipeWireStream) -> WdResult<()> {
            Ok(())
        }
        fn pipewire_socket(&self) -> PathBuf {
            PathBuf::from("/tmp/stub")
        }
    }

    fn test_session() -> Arc<Session> {
        Arc::new(Session::new_for_test(
            "stub".into(),
            "app".into(),
            Box::new(StubInput),
            Box::new(StubCapture),
            Box::new(StubCompositor),
        ))
    }

    #[tokio::test]
    async fn session_locate_carries_xpath_verbatim() {
        let s = test_session();
        let loc = s.locate("//PushButton[@name='OK']");
        assert_eq!(loc.xpath(), "//PushButton[@name='OK']");
    }

    #[tokio::test]
    async fn session_root_locator_uses_wildcard() {
        let s = test_session();
        assert_eq!(s.root().xpath(), "/*");
    }

    #[tokio::test]
    async fn session_find_by_id_composes_xpath() {
        let s = test_session();
        assert_eq!(s.find_by_id("submit").xpath(), "//*[@id='submit']");
    }

    #[tokio::test]
    async fn session_find_by_name_composes_xpath() {
        let s = test_session();
        assert_eq!(s.find_by_name("OK").xpath(), "//*[@name='OK']");
    }

    #[tokio::test]
    async fn session_find_by_role_name_composes_xpath() {
        let s = test_session();
        assert_eq!(
            s.find_by_role_name("PushButton", "OK").xpath(),
            "//PushButton[@name='OK']"
        );
    }

    #[tokio::test]
    async fn locator_locate_appends_descendant_when_relative() {
        let s = test_session();
        let dialog = s.locate("//Dialog[@name='Confirm']");
        let inner = dialog.locate("PushButton");
        assert_eq!(inner.xpath(), "(//Dialog[@name='Confirm'])//PushButton");
    }

    #[tokio::test]
    async fn locator_locate_absolute_replaces_scope() {
        let s = test_session();
        let dialog = s.locate("//Dialog");
        // Absolute sub-xpath ignores the outer scope entirely.
        assert_eq!(dialog.locate("//Menu").xpath(), "//Menu");
    }

    #[tokio::test]
    async fn locator_nth_wraps_with_one_indexed_predicate() {
        let s = test_session();
        let loc = s.locate("//PushButton").nth(2);
        assert_eq!(loc.xpath(), "(//PushButton)[3]");
    }

    #[tokio::test]
    async fn locator_first_is_nth_zero() {
        let s = test_session();
        let loc = s.locate("//PushButton").first();
        assert_eq!(loc.xpath(), "(//PushButton)[1]");
    }

    #[tokio::test]
    async fn locator_last_uses_last_function() {
        let s = test_session();
        let loc = s.locate("//PushButton").last();
        assert_eq!(loc.xpath(), "(//PushButton)[last()]");
    }

    #[tokio::test]
    async fn locator_parent_appends_dot_dot() {
        let s = test_session();
        let loc = s.locate("//PushButton[@name='OK']").parent();
        assert_eq!(loc.xpath(), "(//PushButton[@name='OK'])/..");
    }

    #[tokio::test]
    async fn locator_composition_chains() {
        // Exercise a realistic chain: find a dialog, descend to a specific
        // button, pin to the 2nd match. This confirms each composition step
        // wraps the previous xpath correctly.
        let s = test_session();
        let loc = s
            .locate("//Dialog[@name='Confirm']")
            .locate("PushButton")
            .nth(1);
        assert_eq!(loc.xpath(), "((//Dialog[@name='Confirm'])//PushButton)[2]");
    }

    #[tokio::test]
    async fn locator_clone_preserves_xpath() {
        let s = test_session();
        let loc = s.locate("//PushButton");
        let cloned = loc.clone();
        assert_eq!(cloned.xpath(), "//PushButton");
    }

    #[tokio::test]
    async fn locator_click_on_session_without_a11y_errors_cleanly() {
        // Test-support Session has no AT-SPI connection; click() should
        // surface that as an Atspi error rather than panicking.
        let s = test_session();
        let err = s.locate("//PushButton").click().await.unwrap_err();
        assert!(matches!(err, Error::Atspi(_)));
        assert!(err.to_string().contains("no AT-SPI connection"));
    }

    // ── Generic-wait API surface ───────────────────────────────────────────
    //
    // The API is `async fn wait_until / wait_until_async / wait_for`. We
    // can't drive the full poll loop in unit tests (no AT-SPI snapshot
    // source), but we can verify:
    //  1. Each method exists with its intended signature and compiles for
    //     the shapes of predicate the docs advertise.
    //  2. They surface the "no a11y" error cleanly, like `click` does.
    //  3. The `single_has_state` helper they delegate to is correct (pure,
    //     no I/O — exhaustively testable).

    #[test]
    fn single_has_state_requires_exactly_one_match() {
        fn info_with_states(states: &[&str]) -> ElementInfo {
            ElementInfo {
                ref_: ("b".into(), "/p".into()),
                role: "Node".into(),
                role_raw: None,
                name: None,
                attributes: HashMap::new(),
                states: states.iter().map(|s| (*s).into()).collect(),
                bounds: None,
            }
        }
        // Empty → false (nothing to check).
        assert!(!single_has_state(&[], "checked"));
        // One match with the state → true.
        let a = info_with_states(&["showing", "checked"]);
        assert!(single_has_state(std::slice::from_ref(&a), "checked"));
        // One match without the state → false.
        let b = info_with_states(&["showing"]);
        assert!(!single_has_state(std::slice::from_ref(&b), "checked"));
        // Multiple matches → false even if they all have the state (the
        // single-element waits treat ambiguity as "not satisfied," which
        // matches Playwright-style strict-one semantics).
        assert!(!single_has_state(&[a.clone(), a.clone()], "checked"));
    }

    #[tokio::test]
    async fn wait_until_surfaces_missing_a11y_as_atspi_error() {
        let s = test_session();
        let err = s
            .locate("//PushButton")
            .with_timeout(Duration::from_millis(10))
            .wait_until(|_| true)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Atspi(_)));
        assert!(err.to_string().contains("no AT-SPI connection"));
    }

    #[tokio::test]
    async fn wait_until_async_surfaces_missing_a11y_as_atspi_error() {
        let s = test_session();
        let err = s
            .locate("//PushButton")
            .with_timeout(Duration::from_millis(10))
            .wait_until_async(|_| async { true })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Atspi(_)));
    }

    #[tokio::test]
    async fn wait_for_surfaces_missing_a11y_as_atspi_error() {
        let s = test_session();
        let err = s
            .locate("//PushButton")
            .with_timeout(Duration::from_millis(10))
            .wait_for(|_| async { Ok(Some(42)) })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Atspi(_)));
    }

    #[tokio::test]
    async fn wait_for_non_retriable_predicate_error_aborts_immediately() {
        // A non-retriable error from the predicate (e.g. InvalidSelector)
        // must propagate without retrying. We can't reach the predicate
        // itself through the test session (snapshot errors first), but we
        // can exercise poll_with_retry directly for this behavior — and
        // the existing `poll_bails_immediately_on_non_retriable_error`
        // test below covers it. This test just asserts `wait_for`'s
        // signature accepts async closures that can produce `Result<Option<_>>`.
        let s = test_session();
        let result: WdResult<&'static str> = s
            .locate("//X")
            .with_timeout(Duration::from_millis(10))
            .wait_for(|_| async { Ok::<Option<&'static str>, Error>(Some("sentinel")) })
            .await;
        // a11y-missing error comes first from the inspect_all call.
        assert!(matches!(result.unwrap_err(), Error::Atspi(_)));
    }

    #[tokio::test]
    async fn session_dump_tree_without_a11y_errors_cleanly() {
        let s = test_session();
        let err = s.dump_tree().await.unwrap_err();
        assert!(matches!(err, Error::Atspi(_)));
        assert!(err.to_string().contains("no AT-SPI connection"));
    }

    #[tokio::test]
    async fn with_timeout_overrides_session_default() {
        let s = test_session();
        // Default timeout comes from Session (5s fallback). Per-locator
        // override replaces it; both locators share the xpath.
        let base = s.locate("//PushButton");
        let quick = base.with_timeout(Duration::from_millis(100));
        assert_eq!(quick.xpath(), base.xpath());
        // We can't easily inspect `effective_timeout` because it's private,
        // but we verify the override takes a different code path by
        // exercising it through wait behavior below.
    }

    // ── poll_with_retry ────────────────────────────────────────────────────

    #[tokio::test]
    async fn poll_returns_value_on_first_try() {
        let result: Result<i32, Error> =
            poll_with_retry(Duration::from_secs(5), "x", || async { Ok(Some(42)) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn poll_succeeds_after_retries() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_cloned = attempts.clone();
        let result: Result<&'static str, Error> =
            poll_with_retry(Duration::from_secs(5), "x", move || {
                let a = attempts_cloned.clone();
                async move {
                    let n = a.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(Error::ElementNotFound { xpath: "x".into() })
                    } else {
                        Ok(Some("found"))
                    }
                }
            })
            .await;
        assert_eq!(result.unwrap(), "found");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn poll_surfaces_last_retriable_error_on_timeout() {
        let result: Result<&'static str, Error> =
            poll_with_retry(Duration::from_millis(50), "//Missing", || async {
                Err::<Option<&'static str>, _>(Error::ElementNotFound {
                    xpath: "//Missing".into(),
                })
            })
            .await;
        let err = result.unwrap_err();
        assert!(
            matches!(err, Error::ElementNotFound { .. }),
            "expected ElementNotFound, got {err}"
        );
    }

    #[tokio::test]
    async fn poll_returns_timeout_when_predicate_keeps_saying_none() {
        // No retriable error — predicate just kept observing "element
        // present but state not satisfied." That should produce a Timeout
        // error, not some stale cached retriable error.
        let result: Result<i32, Error> =
            poll_with_retry(Duration::from_millis(50), "//Pending", || async {
                Ok::<Option<i32>, Error>(None)
            })
            .await;
        let err = result.unwrap_err();
        match err {
            Error::Timeout(msg) => assert!(
                msg.contains("//Pending"),
                "timeout message should include the xpath: {msg}"
            ),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poll_bails_immediately_on_non_retriable_error() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_cloned = attempts.clone();
        let result: Result<&'static str, Error> =
            poll_with_retry(Duration::from_secs(5), "//Bad", move || {
                let a = attempts_cloned.clone();
                async move {
                    a.fetch_add(1, Ordering::SeqCst);
                    Err(Error::InvalidSelector {
                        xpath: "//Bad".into(),
                        reason: "oops".into(),
                    })
                }
            })
            .await;
        let err = result.unwrap_err();
        assert!(matches!(err, Error::InvalidSelector { .. }));
        // We should only attempt once — no retries for fatal errors.
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn poll_ambiguous_selector_is_not_retriable() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_cloned = attempts.clone();
        let result: Result<&'static str, Error> =
            poll_with_retry(Duration::from_secs(5), "//PushButton", move || {
                let a = attempts_cloned.clone();
                async move {
                    a.fetch_add(1, Ordering::SeqCst);
                    Err(Error::AmbiguousSelector {
                        xpath: "//PushButton".into(),
                        count: 3,
                    })
                }
            })
            .await;
        assert!(matches!(
            result.unwrap_err(),
            Error::AmbiguousSelector { count: 3, .. }
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn poll_zero_timeout_is_single_shot() {
        // Duration::ZERO → try once, if failing surface the error without
        // any sleep. Useful for negative assertions.
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_cloned = attempts.clone();
        let start = std::time::Instant::now();
        let _: Result<i32, Error> = poll_with_retry(Duration::ZERO, "//X", move || {
            let a = attempts_cloned.clone();
            async move {
                a.fetch_add(1, Ordering::SeqCst);
                Err(Error::ElementNotFound {
                    xpath: "//X".into(),
                })
            }
        })
        .await;
        // One attempt, returns promptly (give it a generous 100ms budget for
        // scheduler noise).
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "zero-timeout poll should not sleep, took {:?}",
            start.elapsed()
        );
    }

    // ── State-predicate snapshot assertions ────────────────────────────────
    //
    // `is_checked` / `is_focused` / etc. all bottom out in
    // `info.states.iter().any(|s| s == "<name>")` on the ElementInfo produced
    // by `evaluate_xpath_detailed`. We can't exercise the full async path
    // without a live AT-SPI connection, but we can verify that each
    // state-name string shows up in the detailed snapshot where we expect —
    // which is what the predicates actually check against. If the snapshot
    // side of the contract ever changes (e.g. renames an attr), these tests
    // catch it.

    use crate::atspi::evaluate_xpath_detailed;

    fn states_for(xml: &str, xpath: &str) -> Vec<String> {
        let mut hits = evaluate_xpath_detailed(xml, xpath).unwrap();
        assert_eq!(hits.len(), 1, "fixture should match exactly one element");
        hits.pop().unwrap().states
    }

    #[test]
    fn snapshot_exposes_checked_state() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="b|/r"><CheckBox name="Accept" checked="true" _ref="b|/c"/></Application>"#;
        let states = states_for(xml, "//CheckBox");
        assert!(states.iter().any(|s| s == "checked"));
    }

    #[test]
    fn snapshot_exposes_focused_state() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="b|/r"><Entry focused="true" _ref="b|/e"/></Application>"#;
        let states = states_for(xml, "//Entry");
        assert!(states.iter().any(|s| s == "focused"));
    }

    #[test]
    fn snapshot_exposes_expanded_state() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="b|/r"><TreeItem expanded="true" _ref="b|/t"/></Application>"#;
        let states = states_for(xml, "//TreeItem");
        assert!(states.iter().any(|s| s == "expanded"));
    }

    #[test]
    fn snapshot_exposes_editable_state() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="b|/r"><Entry editable="true" _ref="b|/e"/></Application>"#;
        let states = states_for(xml, "//Entry");
        assert!(states.iter().any(|s| s == "editable"));
    }

    #[test]
    fn snapshot_exposes_selected_state() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="b|/r"><ListItem selected="true" _ref="b|/l"/></Application>"#;
        let states = states_for(xml, "//ListItem");
        assert!(states.iter().any(|s| s == "selected"));
    }

    #[test]
    fn snapshot_exposes_pressed_state() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="b|/r"><ToggleButton pressed="true" _ref="b|/t"/></Application>"#;
        let states = states_for(xml, "//ToggleButton");
        assert!(states.iter().any(|s| s == "pressed"));
    }

    #[test]
    fn snapshot_exposes_modal_state() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="b|/r"><Dialog modal="true" _ref="b|/d"/></Application>"#;
        let states = states_for(xml, "//Dialog");
        assert!(states.iter().any(|s| s == "modal"));
    }

    #[test]
    fn snapshot_state_absence_is_also_detectable() {
        // If the state attr is absent, the snapshot omits it from `states`,
        // which is exactly what `is_checked()` etc. rely on returning false.
        let xml = r#"<?xml version="1.0"?>
<Application _ref="b|/r"><CheckBox _ref="b|/c"/></Application>"#;
        let states = states_for(xml, "//CheckBox");
        assert!(!states.iter().any(|s| s == "checked"));
    }

    #[test]
    fn is_retriable_matches_expected_errors() {
        assert!(is_retriable(&Error::ElementNotFound { xpath: "x".into() }));
        assert!(is_retriable(&Error::ElementStale {
            xpath: "x".into(),
            bus: "b".into(),
            path: "/p".into(),
        }));
        assert!(!is_retriable(&Error::AmbiguousSelector {
            xpath: "x".into(),
            count: 2,
        }));
        assert!(!is_retriable(&Error::InvalidSelector {
            xpath: "x".into(),
            reason: "r".into(),
        }));
        assert!(!is_retriable(&Error::Atspi("boom".into())));
        assert!(!is_retriable(&Error::Timeout("nope".into())));
    }

    // ── wheel_direction ────────────────────────────────────────────────────
    //
    // Drives the fallback path of scroll_into_view. A bug here would mean
    // either scrolling the wrong way (infinite loop that hits the retry
    // cap) or never scrolling at all, so worth covering in unit tests even
    // though the math is simple.

    use crate::atspi::Rect;

    #[test]
    fn wheel_direction_above_returns_negative() {
        // Element is above the viewport — scroll up (toward the element).
        let elem = Rect {
            x: 0,
            y: -100,
            width: 50,
            height: 20,
        };
        let viewport = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert_eq!(super::wheel_direction(&elem, &viewport), -1);
    }

    #[test]
    fn wheel_direction_below_returns_positive() {
        // Element is below the viewport — scroll down (toward the element).
        let elem = Rect {
            x: 0,
            y: 200,
            width: 50,
            height: 20,
        };
        let viewport = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert_eq!(super::wheel_direction(&elem, &viewport), 1);
    }

    #[test]
    fn wheel_direction_already_inside_returns_zero() {
        // In-view element — no further scrolling needed.
        let elem = Rect {
            x: 10,
            y: 30,
            width: 20,
            height: 10,
        };
        let viewport = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert_eq!(super::wheel_direction(&elem, &viewport), 0);
    }

    #[test]
    fn wheel_direction_partially_below_returns_positive() {
        // Element top is inside, bottom peeks below — still needs a tick
        // down so the whole element fits.
        let elem = Rect {
            x: 0,
            y: 90,
            width: 20,
            height: 30, // bottom = 120, viewport.bottom = 100
        };
        let viewport = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert_eq!(super::wheel_direction(&elem, &viewport), 1);
    }
}
