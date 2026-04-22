//! XPath-based lazy locators for AT-SPI elements.
//!
//! A [`Locator`] bundles an [`Arc<Session>`] with an XPath expression. It
//! does **not** resolve until an async method is called on it — every
//! resolution takes a fresh AT-SPI snapshot, so locators survive widget
//! reparenting and destruction+recreation (dialog close/reopen, virtualized
//! list scroll, etc.) without manual retries.
//!
//! Single-target methods (`click`, `name`, `text`, …) expect the selector to
//! match exactly one element and return [`Error::AmbiguousSelector`]
//! otherwise. Disambiguate with [`Locator::nth`] / [`Locator::first`] /
//! [`Locator::last`] or refine the XPath.

use std::collections::HashMap;
use std::sync::Arc;

use atspi::connection::AccessibilityConnection;

use crate::atspi as atspi_client;
use crate::atspi::ElementInfo;
use crate::error::{Error, Result};
use crate::session::Session;

/// A lazy, re-resolving handle to one or more AT-SPI elements.
///
/// See the [module-level documentation](crate::locator) for the resolution model.
#[derive(Clone)]
pub struct Locator {
    session: Arc<Session>,
    xpath: String,
}

impl Locator {
    pub(crate) fn new(session: Arc<Session>, xpath: String) -> Self {
        Self { session, xpath }
    }

    /// The XPath expression this locator resolves with.
    pub fn xpath(&self) -> &str {
        &self.xpath
    }

    // ── Composition (pure string manipulation, no I/O) ─────────────────────

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
        Locator::new(self.session.clone(), new_xpath)
    }

    /// Return a locator pinned to the `n`-th (0-indexed) match of this one.
    pub fn nth(&self, n: usize) -> Locator {
        Locator::new(self.session.clone(), format!("({})[{}]", self.xpath, n + 1))
    }

    /// Shorthand for `nth(0)`.
    pub fn first(&self) -> Locator {
        self.nth(0)
    }

    /// Locator for the last match of this selector.
    pub fn last(&self) -> Locator {
        Locator::new(self.session.clone(), format!("({})[last()]", self.xpath))
    }

    /// Locator for the parent of the matched element(s).
    pub fn parent(&self) -> Locator {
        Locator::new(self.session.clone(), format!("({})/..", self.xpath))
    }

    // ── Enumeration ─────────────────────────────────────────────────────────

    /// Number of elements matched by this selector.
    pub async fn count(&self) -> Result<usize> {
        Ok(self.resolve_all().await?.len())
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

    // ── Live metadata (requires exactly one match) ─────────────────────────
    //
    // Every read re-snapshots the AT-SPI tree, so data is always as fresh as
    // the current call. The snapshot XML already captures name, role, states,
    // and toolkit attributes — no second D-Bus round-trip per field.

    /// Accessible name of the matched element, or `None` when the element
    /// has no accessible name set.
    pub async fn name(&self) -> Result<Option<String>> {
        Ok(self.resolve_one_info().await?.name)
    }

    /// Raw AT-SPI role name (e.g. `"push button"`, `"menu item"`).
    ///
    /// Falls back to the PascalCase XML element tag only when the snapshot
    /// lacks a `role` attribute — which shouldn't happen for live snapshots,
    /// but can in hand-crafted test XML.
    pub async fn role(&self) -> Result<String> {
        let info = self.resolve_one_info().await?;
        Ok(info.role_raw.unwrap_or(info.role))
    }

    /// Read a single toolkit attribute by key.
    pub async fn attribute(&self, key: &str) -> Result<Option<String>> {
        Ok(self.resolve_one_info().await?.attributes.remove(key))
    }

    /// All toolkit attributes as a map.
    pub async fn attributes(&self) -> Result<HashMap<String, String>> {
        Ok(self.resolve_one_info().await?.attributes)
    }

    /// Whether the matched element currently has the `Showing` state.
    pub async fn is_showing(&self) -> Result<bool> {
        self.has_state("showing").await
    }

    /// Whether the matched element currently has the `Enabled` state.
    pub async fn is_enabled(&self) -> Result<bool> {
        self.has_state("enabled").await
    }

    /// Text contents of the matched element via the AT-SPI Text interface.
    /// Unlike other metadata, text isn't captured in the snapshot — each
    /// call makes a live read through the Text proxy.
    pub async fn text(&self) -> Result<String> {
        let (bus, path) = self.resolve_one().await?;
        let a11y = self.a11y()?;
        atspi_client::read_text_on(a11y, &self.xpath, &bus, &path).await
    }

    // ── Actions ────────────────────────────────────────────────────────────

    /// Invoke the primary action (index 0) on the matched element.
    ///
    /// Requires exactly one match.
    pub async fn click(&self) -> Result<()> {
        let (bus, path) = self.resolve_one().await?;
        let a11y = self.a11y()?;
        atspi_client::do_action_on(a11y, &self.xpath, &bus, &path).await
    }

    /// Replace the contents of an editable text element.
    pub async fn set_text(&self, text: &str) -> Result<()> {
        let (bus, path) = self.resolve_one().await?;
        let a11y = self.a11y()?;
        atspi_client::set_text_on(a11y, &self.xpath, &bus, &path, text).await
    }

    // ── Internals ──────────────────────────────────────────────────────────

    async fn has_state(&self, state: &str) -> Result<bool> {
        Ok(self
            .resolve_one_info()
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

    async fn snapshot(&self) -> Result<String> {
        let a11y = self.a11y()?;
        atspi_client::snapshot_tree(a11y, &self.session.app_bus_name, &self.session.app_path).await
    }

    async fn resolve_all(&self) -> Result<Vec<(String, String)>> {
        let xml = self.snapshot().await?;
        atspi_client::evaluate_xpath(&xml, &self.xpath)
    }

    /// Snapshot + xpath + expect-one, returning the matched element's
    /// `(bus, path)`. Used by action methods that need the live AT-SPI
    /// identity to invoke actions on.
    async fn resolve_one(&self) -> Result<(String, String)> {
        let mut hits = self.resolve_all().await?;
        select_exactly_one(&self.xpath, hits.len())?;
        Ok(hits.pop().unwrap())
    }

    /// Snapshot + xpath + expect-one, returning the full [`ElementInfo`].
    /// Used by metadata methods that read from the snapshot rather than
    /// round-tripping to AT-SPI a second time.
    async fn resolve_one_info(&self) -> Result<ElementInfo> {
        let xml = self.snapshot().await?;
        let mut hits = atspi_client::evaluate_xpath_detailed(&xml, &self.xpath)?;
        select_exactly_one(&self.xpath, hits.len())?;
        Ok(hits.pop().unwrap())
    }
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

#[cfg(test)]
mod tests {
    use super::{select_exactly_one, Error};

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
    use std::sync::Arc;

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
        async fn pointer_motion_relative(&self, _dx: f64, _dy: f64) -> WdResult<()> {
            Ok(())
        }
        async fn pointer_button(&self, _button: u32) -> WdResult<()> {
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

    #[tokio::test]
    async fn session_dump_tree_without_a11y_errors_cleanly() {
        let s = test_session();
        let err = s.dump_tree().await.unwrap_err();
        assert!(matches!(err, Error::Atspi(_)));
        assert!(err.to_string().contains("no AT-SPI connection"));
    }
}
