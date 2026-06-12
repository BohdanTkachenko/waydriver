//! End-to-end tests for the waydriver library against headless mutter.
//!
//! Each test spins up its own mutter session with the project's
//! `waydriver-fixture-gtk` binary (a purpose-built GTK4 + libadwaita
//! fixture with stable selectors and stdout event emission on every
//! primary signal) and exercises a different part of the API.
//!
//! These tests are `#[ignore]`-gated because they spawn real mutter +
//! pipewire processes and share the host AT-SPI session bus. The
//! fixture has a unique app-id so parallel test instances don't collide
//! on D-Bus, but the shared host bus still means running with
//! `--test-threads=1` is the reliable path.
//!
//! Run them explicitly with:
//!
//! ```sh
//! cargo build -p waydriver-fixture-gtk  # tests don't rebuild the fixture
//! cargo test -p waydriver-e2e -- --ignored --test-threads=1
//! ```
//!
//! The MCP-level e2e test in `crates/waydriver-mcp/tests/e2e.rs` drives
//! the same fixture through the MCP JSON-RPC surface inside a Docker
//! container. Both test layers target the fixture only — no external
//! app dependencies.

use std::sync::Arc;
use std::time::Duration;

use waydriver::{
    CompositorRuntime, Error, FillMode, InputBackend, SelectBy, Session, SessionConfig,
};
use waydriver_capture_mutter::MutterCapture;
use waydriver_compositor_mutter::{MutterCompositor, MutterState};
use waydriver_input_mutter::MutterInput;

/// Strip any GStreamer status messages preceding the PNG magic bytes.
///
/// Returns an error including a hex preview of the input when the magic
/// bytes are absent — the previous `.expect("no PNG magic found")`
/// turned upstream encoder failures into opaque panics with no context
/// about what was actually returned.
fn extract_png(raw: &[u8]) -> anyhow::Result<Vec<u8>> {
    let png_start = raw
        .windows(4)
        .position(|w| w == [0x89, b'P', b'N', b'G'])
        .ok_or_else(|| {
            let preview_len = raw.len().min(64);
            let preview: Vec<String> = raw[..preview_len]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let preview = preview.join(" ");
            anyhow::anyhow!(
                "no PNG magic in screenshot data ({} bytes; first {preview_len} bytes: {preview})",
                raw.len()
            )
        })?;
    Ok(raw[png_start..].to_vec())
}

/// Count pixels that differ between two PNG byte blobs.
fn diff_png_pixels(a: &[u8], b: &[u8]) -> anyhow::Result<usize> {
    let img1 = image::load_from_memory(a)?.to_rgba8();
    let img2 = image::load_from_memory(b)?.to_rgba8();
    Ok(img1
        .pixels()
        .zip(img2.pixels())
        .filter(|(p1, p2)| p1 != p2)
        .count())
}

/// Consume the Arc wrapper and call Session::kill. Any Locator clone from
/// earlier in the test must have been dropped before reaching here.
async fn kill(session: Arc<Session>) -> anyhow::Result<()> {
    let inner = Arc::try_unwrap(session).map_err(|_| {
        anyhow::anyhow!("session arc still referenced when killing — a Locator outlived the test")
    })?;
    inner.kill().await?;
    Ok(())
}

/// Resolve the path to the `waydriver-fixture-gtk` binary based on the
/// test executable's location. Same pattern the MCP e2e uses for
/// `waydriver-mcp`.
///
/// **Important:** `cargo test -p waydriver` does *not* rebuild the
/// fixture crate — the fixture isn't a dep of waydriver. Run `cargo
/// build -p waydriver-fixture-gtk` first (or `cargo build --workspace`)
/// if you've edited the fixture since its last build, otherwise the
/// test runs against a stale binary.
fn fixture_binary() -> std::path::PathBuf {
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent() // deps/
        .unwrap()
        .parent() // debug/
        .unwrap()
        .to_path_buf();
    path.push("waydriver-fixture-gtk");
    path
}

/// Start a session running the repo's own GTK4 fixture binary, pinned to
/// a specific `--section` so the AT-SPI tree contains only that
/// section's widgets.
async fn start_fixture_session(section: &str) -> anyhow::Result<(Arc<Session>, Arc<MutterState>)> {
    let mut compositor = MutterCompositor::new();
    compositor.start(None, None).await?;
    // `state()` returns `Option` post-API-tightening; immediately after
    // a successful `start()` it is always `Some`, but `expect` makes
    // the contract local to the call site rather than implicit in the
    // type.
    let state = compositor
        .state()
        .expect("MutterCompositor::state must be Some immediately after start() succeeded");
    let input = MutterInput::new(state.clone());
    let capture = MutterCapture::new(state.clone());

    let fixture_bin = fixture_binary();
    assert!(
        fixture_bin.exists(),
        "fixture binary missing at {fixture_bin:?}; run `cargo build -p waydriver-fixture-gtk` first"
    );

    let session = Session::start(
        Box::new(compositor),
        Box::new(input),
        Box::new(capture),
        SessionConfig {
            command: fixture_bin.to_string_lossy().into_owned(),
            args: vec![format!("--section={section}")],
            cwd: None,
            app_name: "waydriver-fixture-gtk".into(),
            video_output: None,
            video_bitrate: None,
            video_fps: None,
            // Pre-warm the ocrs engine in the background so the
            // visual-locator tests (notably `lazy_a11y_*`) don't pay
            // the ~1-2s model-load cost on their first `find_by_text`
            // call. Cheap to leave on for the AT-SPI-only tests too:
            // the prewarm task lives in `tokio::spawn`, never blocks
            // session startup, and gets dropped when the session ends.
            prewarm_visual: true,
            visual_region_tuning: Default::default(),
            visual_text_tuning: Default::default(),
            visual_click_tuning: Default::default(),
            gsettings_isolated: true,
            xdg_isolated: true,
            extra_env: Vec::new(),
        },
    )
    .await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    Ok((Arc::new(session), state))
}

/// Spot-check every name in `expected` appears at least once in the tree.
/// Shared body used by the three per-section diagnostic tests.
///
/// Errors from `dump_tree` and `locate(...).count()` propagate as
/// `anyhow::Result` so that AT-SPI / D-Bus failures surface with their
/// full chain in the test report, instead of being collapsed into a
/// terse `expect("dump_tree")` panic or silently masked as
/// `unwrap_or(usize::MAX)` which the `>= 1` assertion would still
/// accept. Missing widgets remain a panicking assertion — that's the
/// scenario the test is *for*.
async fn assert_widgets_exist(
    session: &Arc<Session>,
    section: &str,
    expected: &[&str],
) -> anyhow::Result<()> {
    let tree = session
        .dump_tree()
        .await
        .map_err(|e| anyhow::anyhow!("dump_tree failed for section {section:?}: {e}"))?;
    eprintln!(
        "── fixture tree ({section}) ─────────────────────────────\n{tree}\n\
         ────────────────────────────────────────────────────────────"
    );
    for expected_name in expected {
        let count = session
            .locate(&format!("//*[@name='{expected_name}']"))
            .count()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "locate(name={expected_name:?}).count() failed in section {section:?}: {e}"
                )
            })?;
        assert!(
            count >= 1,
            "expected named widget '{expected_name}' to be in the tree (count: {count})"
        );
    }
    Ok(())
}

fn init_tracing() {
    // `try_init` only fails if a global subscriber is already installed,
    // which happens whenever a test process runs more than one test in
    // sequence. That's the expected steady state, so swallowing the
    // result here is correct — there is nothing meaningful to report.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
}

#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_exposes_gtk4_widgets() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;
    assert_widgets_exist(
        &session,
        "gtk4",
        &[
            "primary-button",
            "mode-toggle",
            "agree-check",
            "text-entry",
            "main-menu",
            "open-dialog",
        ],
    )
    .await?;
    kill(session).await?;
    Ok(())
}

/// Live verification of the externally-reported locator/input bugs, each
/// against its *reported* symptom:
///
/// 1. waits raced an element into existence and errored `ElementNotFound`
///    instead of waiting — now `wait_for_present` started *before* the widget
///    exists must resolve once it appears, and a never-appearing selector must
///    wait out its budget and surface `Timeout` (not `ElementNotFound`).
/// 2. `press_chord("Ctrl+comma")` was rejected as an invalid chord — must now
///    be accepted (GTK accelerator keysym names).
/// 3. a single-target action on an ambiguous selector gave a bare count — the
///    error must now name the colliding elements.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn reported_locator_bugs_are_fixed() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // ── BUG 1a: wait_for_present resolves for an element that appears later.
    // The sample dialog's close button doesn't exist until the dialog opens;
    // start the wait first, then open.
    let target = "//Button[@name='dialog-close']";
    assert_eq!(
        session.locate(target).count().await?,
        0,
        "premise: dialog-close absent before the wait starts"
    );
    let waiter = {
        let s = session.clone();
        let sel = target.to_string();
        tokio::spawn(async move { s.locate(&sel).wait_for_present().await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await; // waiter is polling
    session
        .locate("//Button[@name='open-dialog']")
        .click()
        .await?;
    waiter.await?.map_err(|e| {
        anyhow::anyhow!("wait_for_present should resolve once the dialog appears: {e}")
    })?;
    eprintln!("BUG1a OK: wait_for_present resolved for a later-appearing dialog");

    // ── BUG 1b: a never-appearing selector waits its budget and times out
    // with Timeout (not ElementNotFound), naming the selector.
    let started = std::time::Instant::now();
    let err = session
        .locate("//Label[@name='zzqx-never-exists']")
        .with_timeout(Duration::from_millis(800))
        .wait_for_present()
        .await
        .expect_err("absent element must not resolve");
    let elapsed = started.elapsed();
    assert!(
        matches!(err, waydriver::Error::Timeout(_)),
        "never-appears must surface Timeout, got {err:?}"
    );
    assert!(
        err.to_string().contains("zzqx-never-exists"),
        "timeout should name the selector: {err}"
    );
    assert!(
        elapsed >= Duration::from_millis(700),
        "must wait the budget out (polled, not failed instantly); took {elapsed:?}"
    );
    eprintln!("BUG1b OK: never-appears waited {elapsed:?} then Timeout: {err}");

    // ── BUG 2: GTK accelerator punctuation names round-trip press_chord.
    session.press_chord("Ctrl+comma").await?;
    session.press_chord("Ctrl+minus").await?;
    eprintln!("BUG2 OK: Ctrl+comma / Ctrl+minus accepted and dispatched");

    // ── Minor: ambiguous single-target action names the colliding elements.
    let err = session
        .locate("//Label")
        .click()
        .await
        .expect_err("//Label matches many; single-target click must error");
    let msg = err.to_string();
    assert!(
        msg.contains("matched") && msg.contains("Label"),
        "ambiguity error should list the colliding elements: {msg}"
    );
    eprintln!("MINOR OK: ambiguous error lists matches: {msg}");

    kill(session).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_exposes_adw_widgets() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;
    assert_widgets_exist(
        &session,
        "adw",
        &[
            "adw-prefs-group",
            "adw-entry-row",
            "adw-combo-row",
            "adw-switch-row",
            "adw-action-row",
            "adw-button-row",
            "open-adw-dialog",
            // The main-menu button lives in the header bar and is present
            // regardless of which section is selected.
            "main-menu",
        ],
    )
    .await?;
    kill(session).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_exposes_dnd_widgets() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("dnd").await?;
    assert_widgets_exist(
        &session,
        "dnd",
        &["drag-source", "drop-target", "drop-status", "main-menu"],
    )
    .await?;
    kill(session).await?;
    Ok(())
}

/// Sanity test for `Locator::list_text` — enumerate every OCR'd
/// line inside a scope and confirm at least the labels we know are
/// visible show up.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; downloads ocrs models on first run"]
async fn visual_locator_list_text_enumerates_visible_labels() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Scope the locator in a block so it drops before `kill`
    // takes ownership of the session Arc.
    let hits = {
        // First element with AT-SPI bounds (toplevel widget area).
        let scope = session.locate("//*[@bbox][1]").first();
        scope.list_text().await?
    };
    eprintln!("OCR enumerated {} text hit(s):", hits.len());
    for h in &hits {
        eprintln!("  - {:?} @ {:?}", h.text, h.bounds);
    }

    // The GTK4 section has labelled buttons among other widgets.
    // OCR may misread a few characters, but stock GTK buttons are
    // legible enough that at least one of these substrings should
    // show up.
    let expected = ["primary-button", "agree-check", "mode-toggle"];
    let found_any = hits.iter().any(|h| {
        expected
            .iter()
            .any(|want| h.text.to_lowercase().contains(&want.to_lowercase()))
    });
    assert!(
        found_any,
        "list_text didn't recognise any of {expected:?} in {hits:?}"
    );

    kill(session).await?;
    Ok(())
}

/// Diagnostic: dump everything OCR recognizes on the fixture (via the new
/// `Session::recognized_text`), so a `find_by_text` miss can be told apart from
/// a mis-recognition. On a software-GL box this shows OCR merging adjacent
/// labels ("primary-button mode-toggle") and mis-reading small text
/// ("hover-targel") — i.e. why some visual clicks are unreliable. Run with
/// `--ignored --nocapture` to read the output.
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn visual_recognized_text_dump() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let native = session.recognized_text().await?;
    eprintln!("=== recognized_text (native): {} blocks ===", native.len());
    for h in &native {
        eprintln!("  {:?} @ {:?}", h.text, h.bounds);
    }

    kill(session).await?;
    Ok(())
}

/// A short `with_timeout` must bound a `find_by_text` wait *even though* a
/// single OCR pass is tens of seconds — the caller is released at the deadline
/// (`Timeout`), it does not run a full pass to completion. Guards the "no
/// 20-minute wait" rule: the locator timeout caps the wait, not the OCR cost.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run with --ignored"]
async fn visual_find_by_text_respects_short_timeout() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let start = std::time::Instant::now();
    let res = session
        .find_by_text("zzqx_definitely_absent_label")
        .with_timeout(Duration::from_millis(500))
        .bounds()
        .await;
    let elapsed = start.elapsed();

    kill(session).await?;

    assert!(res.is_err(), "absent text must not resolve");
    // A full OCR pass is tens of seconds here; a 500ms timeout must return well
    // before that. Generous bound to avoid CI flakiness, but far under one pass.
    assert!(
        elapsed < Duration::from_secs(15),
        "500ms-timeout find_by_text must return long before a full OCR pass; took {elapsed:?} ({res:?})"
    );
    Ok(())
}

/// Sanity test for the OCR-backed visual locator against a vanilla
/// `GtkButton` (whose AT-SPI surface is well-behaved). Proves the
/// screenshot → OCR → pointer-click pipeline works end-to-end.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; downloads ocrs models on first run"]
async fn visual_locator_click_fires_gtk_button_activation() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let cursor = session.stdout_cursor();
    session.find_by_text("primary-button").click().await?;
    let line = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("clicked primary-button"),
            Duration::from_secs(5),
        )
        .await?;
    assert!(
        line.contains("clicked primary-button"),
        "unexpected stdout line after visual click: {line}"
    );

    kill(session).await?;
    Ok(())
}

/// Pins libadwaita lazy-realization bug: an `AdwPreferencesGroup`
/// constructed with `visible:false` *inside an `AdwPreferencesPage`*
/// and then flipped to visible after `present()` never has its
/// accessible subtree built. The `lazy-button` `AdwButtonRow` inside
/// is rendered on screen but absent from AT-SPI.
///
/// A naive top-level repro (group → window content, no prefs page in
/// between) does *not* trigger the bug — that variant surfaces fine.
/// The fixture mirrors the real-world shape that reproduces the bug.
///
/// ## Why we count `role="generic" focusable="true"` instead of
/// querying by name
///
/// `AdwButtonRow`'s title doesn't surface as an AT-SPI accessible name
/// in current libadwaita (an independent gap from this bug — see
/// `MEMORY.md`'s `adw_widget_atspi_gaps`). So `find_by_name("lazy-button")`
/// returns 0 *regardless* of whether the lazy-realization bug is fixed.
/// Each `AdwButtonRow` does, however, surface as a focusable Generic
/// inside the dialog. Counting focusable Generics inside the dialog
/// before and after the visibility flip is what actually distinguishes
/// "bug present" (count unchanged) from "bug fixed" (count grew by 1).
///
/// ## Asserts the fixed behavior so the bug shows up as a test failure
///
/// We tried two client-side workarounds in waydriver and neither helped:
///
///   - Replacing `Accessible.GetChildren` with a `0..ChildCount` loop of
///     `Accessible.GetChildAtIndex(i)` calls. Did not surface the missing
///     widgets — `ChildCount` itself reports them missing.
///   - Calling `Cache.GetItems` on the app (bypasses parent traversal).
///     The lazy widgets are not in the cache either; libadwaita simply
///     doesn't register them with AT-SPI when the trigger conditions are
///     present.
///
/// The bug is genuinely upstream and only libadwaita can fix it. This
/// test will start passing again the day that happens.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn lazy_a11y_hidden_then_shown_widget_missing_from_atspi() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("lazy-a11y").await?;

    // Open the issue 1 dialog and wait for it to be present.
    let cursor = session.stdout_cursor();
    session
        .locate("//Button[@name='open-hidden-group-dialog']")
        .click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("dialog-opened hidden-group-dialog"),
            Duration::from_secs(3),
        )
        .await?;

    // Snapshot focusable-Generic count after the dialog is up but
    // *before* the 300ms timer flips the hidden group visible.
    let focusable_selector = "//Dialog//*[@role='generic' and @focusable='true']";
    let count_before = session.locate(focusable_selector).count().await?;
    assert!(
        count_before >= 1,
        "dialog is up but contains zero focusable Generic widgets — the \
         control row should be queryable. count_before={count_before}"
    );

    // Wait for the visibility flip + a settle window for any potential
    // a11y-tree rebuild to land.
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("lazy-shown hidden-group-target-group"),
            Duration::from_secs(3),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let count_after = session.locate(focusable_selector).count().await?;
    assert!(
        count_after > count_before,
        "expected focusable-Generic count to grow after the hidden group \
         was made visible (the new AdwButtonRow should surface in AT-SPI), \
         but stayed at {count_before} → {count_after}. This is the \
         libadwaita lazy-realization bug for hidden-then-shown groups \
         inside an AdwPreferencesPage — see waydriver-fixture-gtk's \
         lazy-a11y section for the minimal repro."
    );

    kill(session).await?;
    Ok(())
}

/// Pins libadwaita lazy-realization bug for `AdwPreferencesDialog`:
/// after opening the dialog and switching to a non-initial page via
/// `set_visible_page_name`, the widgets on that page never enter the
/// AT-SPI tree. The `lazy-switch` `AdwSwitchRow` on page2 is rendered
/// on screen but absent from AT-SPI.
///
/// Asserts on `role='switch'` rather than the row's title — see the
/// rationale on the hidden-group test (`AdwSwitchRow` titles don't surface,
/// so a title-based count can't distinguish "bug present" from "title
/// not exposed"). Same upstream-only conclusion: `Cache.GetItems`
/// confirms page2 widgets aren't registered with AT-SPI at all.
/// Experiment: does driving keyboard *focus* onto a lazily-revealed widget
/// force-realize its AT-SPI context (per the focus-realize proposal)? Captures
/// `object:state-changed:focused` and `object:children-changed` events while
/// Tabbing through the non-initial-page dialog, and re-checks the tree + the
/// app cache. Distinguishes Model A (focus realizes the leaf → carried by the
/// focus *event* only, tree/cache stay empty) from Model B (chain repairs →
/// tree/cache now include it) — or refutes both (no focus event for the
/// target). Diagnostic; read the RESULT block on stderr.
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn lazy_a11y_focus_realization_experiment() -> anyhow::Result<()> {
    use atspi::events::object::{ChildrenChangedEvent, StateChangedEvent};
    use atspi::proxy::accessible::AccessibleProxy;
    use atspi::proxy::cache::CacheProxy;
    use atspi::AccessibilityConnection;
    use atspi::ObjectEvents;
    use futures_lite::StreamExt;
    use std::collections::HashSet;

    init_tracing();
    let (session, _state) = start_fixture_session("lazy-a11y").await?;

    // Open the non-initial-page dialog; its page2 AdwSwitchRow renders but is
    // absent from AT-SPI.
    let cursor = session.stdout_cursor();
    session
        .locate("//Button[@name='open-non-initial-page-dialog']")
        .click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("dialog-opened non-initial-page-dialog"),
            Duration::from_secs(3),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(700)).await;

    let conn = session
        .a11y_connection
        .as_ref()
        .expect("session has a11y connection")
        .clone();
    let app = session.app_bus_name.clone();

    // --- Baseline: tree + cache (both expected to lack the switch) ---
    let baseline = session.locate("//Dialog//*").inspect_all().await?;
    let baseline_paths: HashSet<String> = baseline.iter().map(|e| e.ref_.1.clone()).collect();
    let b1 = session
        .locate("//Dialog//*[@role='switch']")
        .count()
        .await?;

    let cache = CacheProxy::builder(&conn)
        .destination(app.clone())?
        .path("/org/a11y/atspi/cache")?
        .build()
        .await?;
    let before_items = cache.get_items().await?;
    let before_paths: HashSet<String> = before_items
        .iter()
        .map(|i| i.object.path_as_str().to_string())
        .collect();
    eprintln!(
        "BASELINE: dialog descendants={} switch-in-tree={b1} cache-items={}",
        baseline.len(),
        before_items.len()
    );

    // --- Subscribe to events BEFORE any input ---
    let a11y = AccessibilityConnection::new().await?;
    a11y.register_event::<StateChangedEvent>().await?;
    a11y.register_event::<ChildrenChangedEvent>().await?;
    let mut stream = std::pin::pin!(a11y.event_stream());
    // Drain anything already queued.
    while tokio::time::timeout(Duration::from_millis(40), stream.next())
        .await
        .is_ok()
    {}

    // --- Experiment F: Tab through the dialog, collect focus/children events ---
    let mut focus_objs: Vec<(String, String)> = Vec::new();
    let mut children_changed = 0u32;
    for _ in 0..35 {
        session.press_chord("Tab").await?;
        let deadline = std::time::Instant::now() + Duration::from_millis(120);
        while std::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(40), stream.next()).await {
                Ok(Some(Ok(atspi::Event::Object(ObjectEvents::StateChanged(sc))))) => {
                    if sc.state == atspi::State::Focused && sc.enabled {
                        let name = sc.item.name_as_str().unwrap_or("").to_string();
                        focus_objs.push((name, sc.item.path_as_str().to_string()));
                    }
                }
                Ok(Some(Ok(atspi::Event::Object(ObjectEvents::ChildrenChanged(_))))) => {
                    children_changed += 1;
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
    }

    // Resolve each focused object: role + whether it was in the baseline tree.
    let mut realized_outside_tree = 0u32;
    let mut switch_ref: Option<(String, String)> = None;
    for (name, path) in &focus_objs {
        let role = match AccessibleProxy::builder(&conn)
            .destination(name.clone())?
            .path(path.clone())?
            .build()
            .await
        {
            Ok(p) => p
                .get_role_name()
                .await
                .unwrap_or_else(|_| "<unqueryable>".into()),
            Err(_) => "<no-proxy>".into(),
        };
        if role == "switch" {
            switch_ref = Some((name.clone(), path.clone()));
        }
        let in_tree = baseline_paths.contains(path);
        if !in_tree {
            realized_outside_tree += 1;
        }
        eprintln!("  focused: role={role:<14} in_baseline_tree={in_tree} {name}{path}");
    }

    // --- After focus: tree + cache ---
    let f3 = session
        .locate("//Dialog//*[@role='switch']")
        .count()
        .await?;
    // Role-agnostic: does the focused switch's PATH now appear in a fresh tree
    // walk? This is the clean Model A (no) vs Model B (yes) discriminator.
    let after_tree = session.locate("//Dialog//*").inspect_all().await?;
    let after_tree_paths: HashSet<String> = after_tree.iter().map(|e| e.ref_.1.clone()).collect();
    let focus_paths_now_in_tree = focus_objs
        .iter()
        .filter(|(_, p)| after_tree_paths.contains(p))
        .count();
    let after_items = cache.get_items().await?;
    let new_cache: Vec<_> = after_items
        .iter()
        .filter(|i| !before_paths.contains(i.object.path_as_str()))
        .map(|i| format!("{:?}:{}", i.role, i.object.path_as_str()))
        .collect();

    eprintln!("=== RESULT (Case 2: non-initial AdwPreferencesDialog page) ===");
    eprintln!("F1/F2  focus events total={} ; carrying an object NOT in the baseline tree={realized_outside_tree}", focus_objs.len());
    eprintln!(
        "F3     tree switch count after focus = {f3} (baseline {b1}); dialog descendants {} -> {}",
        baseline.len(),
        after_tree.len()
    );
    eprintln!("F3b    focused-object paths now present in a fresh tree walk = {focus_paths_now_in_tree} (Model B if >0, Model A if 0)");
    eprintln!(
        "F4     cache items after = {} (before {}); newly-cached = {} {new_cache:?}",
        after_items.len(),
        before_items.len(),
        new_cache.len()
    );
    eprintln!("F5     children-changed events during focus = {children_changed}");

    // --- DRIVE: can the focus-realized switch be *driven* via AT-SPI (not just
    // queried)? Read its state + bounds, then perform its Action and confirm
    // the fixture toggles. This is the real "does AT-SPI work for it now" test.
    if let Some((name, path)) = switch_ref {
        use atspi::proxy::action::ActionProxy;
        use atspi::proxy::component::ComponentProxy;
        use atspi::CoordType;

        let acc = AccessibleProxy::builder(&conn)
            .destination(name.clone())?
            .path(path.clone())?
            .build()
            .await?;
        let states = acc.get_state().await.ok();
        let comp = ComponentProxy::builder(&conn)
            .destination(name.clone())?
            .path(path.clone())?
            .build()
            .await?;
        let extents = comp.get_extents(CoordType::Screen).await.ok();
        eprintln!("DRIVE  switch states={states:?} extents={extents:?}");

        let act = ActionProxy::builder(&conn)
            .destination(name.clone())?
            .path(path.clone())?
            .build()
            .await?;
        let nactions = act.nactions().await.unwrap_or(-1);
        eprintln!("DRIVE  switch nactions={nactions}");

        let cursor = session.stdout_cursor();
        let did = if nactions > 0 {
            act.do_action(0).await.unwrap_or(false)
        } else {
            false
        };
        eprintln!("DRIVE  do_action(0) returned {did}");
        let toggled = session
            .wait_for_stdout_line(
                cursor,
                |l| l.contains("toggled lazy-switch"),
                Duration::from_secs(3),
            )
            .await;
        eprintln!(
            "DRIVE  via AT-SPI Action = {} ({:?})",
            toggled.is_ok(),
            toggled.as_deref().unwrap_or("<no stdout event>")
        );

        // No Action interface — try the *keyboard* path, which is how a
        // keyboard user / Orca toggles a focused switch: grab_focus via AT-SPI
        // (deterministic, using the realized accessible we discovered), then
        // synthesize Space. AT-SPI as the observability/targeting layer,
        // keyboard as the actuation layer.
        let grabbed = comp.grab_focus().await.unwrap_or(false);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let kbd_cursor = session.stdout_cursor();
        session.press_chord("space").await?;
        let kbd_toggled = session
            .wait_for_stdout_line(
                kbd_cursor,
                |l| l.contains("toggled lazy-switch"),
                Duration::from_secs(3),
            )
            .await;
        eprintln!(
            "DRIVE  via keyboard (grab_focus={grabbed} + Space) = {} ({:?})",
            kbd_toggled.is_ok(),
            kbd_toggled.as_deref().unwrap_or("<no stdout event>")
        );
    } else {
        eprintln!("DRIVE  no focus event ever resolved to role=switch — cannot attempt");
    }

    drop(stream);
    drop(a11y);
    kill(session).await?;
    Ok(())
}

/// The supported read-path for lazily-realized widgets: `focus_walk` to
/// force-realize them, then `hidden_accessibles` to discover/inspect them via
/// the app's AT-SPI cache (the `GetChildren` tree never repairs — see
/// `docs/visual-locator.md`). Pins that the page2 `AdwSwitchRow`, invisible to
/// `dump_tree`, becomes readable (role/states/path) after a focus walk.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn lazy_a11y_hidden_accessibles_readable_after_focus_walk() -> anyhow::Result<()> {
    use std::collections::HashSet;

    init_tracing();
    let (session, _state) = start_fixture_session("lazy-a11y").await?;

    let cursor = session.stdout_cursor();
    session
        .locate("//Button[@name='open-non-initial-page-dialog']")
        .click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("dialog-opened non-initial-page-dialog"),
            Duration::from_secs(3),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(700)).await;

    // The switch never enters the snapshot tree (the upstream bug)...
    let in_tree = session
        .locate("//Dialog//*[@role='switch']")
        .count()
        .await?;
    assert_eq!(in_tree, 0, "premise: switch absent from the snapshot tree");

    // ...and before any focus, it isn't realized into the cache either.
    let before = session.hidden_accessibles().await?;
    let before_paths: HashSet<String> = before.iter().map(|c| c.ref_.1.clone()).collect();

    // Focus-walk the dialog: realizes the focused widgets + ancestor chains.
    session.focus_walk(20).await?;

    let after = session.hidden_accessibles().await?;
    let newly_realized: Vec<_> = after
        .iter()
        .filter(|c| !before_paths.contains(&c.ref_.1))
        .collect();
    eprintln!("newly realized after focus_walk:");
    for c in &newly_realized {
        eprintln!(
            "  {} name={:?} states={:?} path={}",
            c.role, c.name, c.states, c.ref_.1
        );
    }

    // The lazy switch is among them, identifiable by its checkable state.
    // (GTK caches an AdwSwitchRow's switch with a checkbox-family role, so
    // match on the state rather than pinning the exact role string.)
    let switch = newly_realized
        .iter()
        .find(|c| c.states.iter().any(|s| s == "checkable"));
    assert!(
        switch.is_some(),
        "focus_walk should realize the page2 switch into the cache; newly realized: {newly_realized:?}"
    );

    kill(session).await?;
    Ok(())
}

/// An isolated session must give the app private XDG state/data/cache dirs
/// under the session runtime dir — not the host's ~/.local/{state,share} —
/// so persisted app state can't poison later sessions or the host. Reads the
/// spawned app's real environment from /proc to verify what it actually got.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn isolated_session_gets_private_xdg_dirs() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let out = std::process::Command::new("pgrep")
        .args(["-fn", "waydriver-fixture-gtk"])
        .output()?;
    let pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    anyhow::ensure!(!pid.is_empty(), "fixture process not found via pgrep");
    let environ = std::fs::read(format!("/proc/{pid}/environ"))?;
    let environ = String::from_utf8_lossy(&environ);
    let vars: Vec<&str> = environ.split('\0').collect();

    for key in [
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
    ] {
        let val = vars
            .iter()
            .find_map(|v| v.strip_prefix(&format!("{key}=")))
            .unwrap_or_else(|| panic!("{key} not set on the spawned app"));
        assert!(
            val.contains("wd-session-"),
            "{key} must point inside the session runtime dir, got {val:?}"
        );
        eprintln!("XDG ISOLATION OK: {key}={val}");
    }

    kill(session).await?;
    Ok(())
}

/// Probe for reported Bug 8 (keyboard grabs wedge input): open the fixture's
/// main-menu popover (which takes a GTK grab), then check whether synthesized
/// Escape dismisses it and whether keyboard input still reaches the app
/// afterwards. Diagnostic — read the GRAB PROBE lines on stderr.
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn grab_probe_escape_dismisses_popover() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Open the popover (same recipe as fixture_main_menu_opens_auto_waits).
    session
        .locate("//ToggleButton[@name='main-menu']")
        .click()
        .await?;
    session.press_keysym(0xffe1).await?; // Shift_L: wake the frame clock
    session
        .locate("//Button[@name='main-menu' and @expanded='true']")
        .wait_for_visible()
        .await?;
    eprintln!("GRAB PROBE: popover open (grab active)");

    // Try to dismiss with synthesized Escape — per the report this gets
    // swallowed and the session wedges.
    session.press_chord("Escape").await?;
    session.press_keysym(0xffe1).await?; // wake frame clock again
    let dismissed = session
        .locate("//Button[@name='main-menu' and @expanded='false']")
        .with_timeout(Duration::from_secs(3))
        .wait_for_present()
        .await;
    eprintln!(
        "GRAB PROBE: Escape dismissed popover = {}",
        dismissed.is_ok()
    );

    // Whether or not it dismissed, can keyboard input still reach the app?
    // Type into the text entry and read it back. (Scoped so the Locator
    // drops before kill().)
    {
        let entry = session.locate("//Text[@name='text-entry']");
        let typed = match entry.fill("grab-check").await {
            Ok(()) => entry.text().await.unwrap_or_default(),
            Err(e) => format!("<fill failed: {e}>"),
        };
        eprintln!(
            "GRAB PROBE: post-popover keyboard works = {} (entry now {typed:?})",
            typed == "grab-check"
        );
    }

    kill(session).await?;
    Ok(())
}

/// Documented-negative for the libadwaita lazy-realization gap (see
/// `docs/visual-locator.md`). We tried every client-side way to *force* the
/// missing accessibles to register and none work: `GetChildren` /
/// `GetChildAtIndex` / `Cache.GetItems`, a grid of `GetAccessibleAtPoint`
/// hit-tests, synthetic pointer-hover, and — below — keyboard focus traversal
/// (Tab, how Orca surfaces them). All leave the switch count at 0. This probe
/// keeps the focus angle re-runnable so a future libadwaita/GTK bump can be
/// re-checked cheaply; the OCR visual locator is the working path meanwhile.
/// Diagnostic — always passes; read the before/after counts on stderr.
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn lazy_a11y_probe_focus_traversal_realizes_widgets() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("lazy-a11y").await?;

    let cursor = session.stdout_cursor();
    session
        .locate("//Button[@name='open-non-initial-page-dialog']")
        .click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("dialog-opened non-initial-page-dialog"),
            Duration::from_secs(3),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let sel = "//Dialog//*[@role='switch']";
    let before = session.locate(sel).count().await?;
    eprintln!("FOCUS PROBE: switches before = {before}");

    // Tab through the dialog; each focus change should build the focused
    // widget's accessible. Settle briefly between presses.
    for i in 0..40 {
        session.press_chord("Tab").await?;
        if i % 8 == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let n = session.locate(sel).count().await?;
            eprintln!("FOCUS PROBE: after {} Tabs, switches = {n}", i + 1);
            if n >= 1 {
                break;
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = session.locate(sel).count().await?;
    eprintln!("FOCUS PROBE RESULT: switches before={before} after={after}");

    kill(session).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn lazy_a11y_non_initial_prefs_page_missing_from_atspi() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("lazy-a11y").await?;

    let cursor = session.stdout_cursor();
    session
        .locate("//Button[@name='open-non-initial-page-dialog']")
        .click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("dialog-opened non-initial-page-dialog"),
            Duration::from_secs(3),
        )
        .await?;

    // Settle for any potential lazy-realization to land.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let switch_count = session
        .locate("//Dialog//*[@role='switch']")
        .count()
        .await?;
    assert!(
        switch_count >= 1,
        "expected at least one Switch widget inside the dialog after \
         set_visible_page_name(\"page2\") (the page2 AdwSwitchRow should \
         surface in AT-SPI), but found {switch_count}. This is the \
         libadwaita lazy-realization bug for non-initial AdwPreferencesDialog \
         pages — see waydriver-fixture-gtk's lazy-a11y section for the \
         minimal repro."
    );

    kill(session).await?;
    Ok(())
}

/// Visual-locator workaround for the non-initial-page bug: drives
/// `lazy-switch` on the non-initial prefs page via OCR even though the
/// row is absent from AT-SPI. Mirrors the pattern from
/// `visual_locator_click_fires_gtk_button_activation`
/// but targets an `AdwSwitchRow` that doesn't exist in the a11y tree at
/// all. Because the whole row is activatable, clicking anywhere on it
/// (including the title text glyphs OCR finds) toggles the switch.
/// Ground truth is the fixture's `toggled lazy-switch active=true`
/// stdout event.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; downloads ocrs models on first run"]
async fn lazy_a11y_non_initial_prefs_page_switchable_via_visual_locator() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("lazy-a11y").await?;

    let cursor = session.stdout_cursor();
    session
        .locate("//Button[@name='open-non-initial-page-dialog']")
        .click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("dialog-opened non-initial-page-dialog"),
            Duration::from_secs(3),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Scope OCR to the dialog's AT-SPI bounds. The dialog itself
    // surfaces even though its page2 contents don't, so cropping
    // before OCR keeps recognition fast and free of off-dialog text.
    // Scoped in a block so the parent Locator drops before `kill`
    // takes ownership of the session Arc.
    let click_cursor = session.stdout_cursor();
    {
        let dialog = session.locate("//Dialog[@name='Preferences']");
        dialog.find_by_text("lazy-switch").await?.click().await?;
    }

    let line = session
        .wait_for_stdout_line(
            click_cursor,
            |l| l.contains("toggled lazy-switch active=true"),
            Duration::from_secs(5),
        )
        .await?;
    assert!(
        line.contains("toggled lazy-switch active=true"),
        "unexpected stdout line after visual click: {line}"
    );

    kill(session).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_click_emits_stdout_event() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Capture the stdout cursor before the click so any startup noise the
    // fixture printed at boot is excluded from the match window.
    let cursor = session.stdout_cursor();
    session
        .locate("//Button[@name='primary-button']")
        .click()
        .await?;

    let line = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("clicked primary-button"),
            Duration::from_secs(3),
        )
        .await?;
    eprintln!("observed stdout event: {line}");
    assert!(
        line.starts_with("fixture-event: clicked primary-button"),
        "unexpected line: {line}"
    );

    kill(session).await?;
    Ok(())
}

/// Element bounds round-trip: snapshot → XML → parse → `Locator::bounds`.
/// Proves that AT-SPI's `Component::get_extents` reaches the test
/// through the whole pipeline and that the numbers look sane for a
/// 1024×768 virtual display.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_element_bounds_are_sane() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let bounds = session
        .locate("//Button[@name='primary-button']")
        .bounds()
        .await?;
    eprintln!("primary-button bounds: {bounds:?}");

    // Positive, non-zero dimensions.
    assert!(
        bounds.width > 0 && bounds.height > 0,
        "width/height should be positive, got {bounds:?}"
    );
    // Small-ish widget — primary-button is a short-labeled GtkButton,
    // should fit easily within the virtual display.
    assert!(
        bounds.width < 1024 && bounds.height < 768,
        "button shouldn't fill the whole viewport: {bounds:?}"
    );
    // Screen-relative coords fall inside the virtual monitor (with some
    // slack for header bars / decorations).
    assert!(
        bounds.x >= 0 && bounds.x < 1024,
        "x outside viewport: {bounds:?}"
    );
    assert!(
        bounds.y >= 0 && bounds.y < 768,
        "y outside viewport: {bounds:?}"
    );

    // Bounds should also land in the dump_tree XML as a `bbox` attribute.
    let xml = session.dump_tree().await?;
    assert!(
        xml.contains("bbox=\""),
        "tree should contain bbox attributes, got:\n{xml}"
    );

    kill(session).await?;
    Ok(())
}

// Note: a positive e2e test for `scroll_into_view` bringing an off-screen
// row into view would belong here, but doesn't exist yet. In headless
// Mutter, GTK4 rejects AT-SPI's `Component::scroll_to` /
// `scroll_to_point` on scroll children, rejects `grab_focus`, *and*
// Mutter's `NotifyPointerAxisDiscrete` doesn't route wheel events to
// widgets without a real compositor's pointer focus machinery — so
// every path inside `scroll_into_view` dead-ends. The library logic is
// covered by unit tests (`wheel_direction`, `Rect::is_inside`,
// `find_scrollable_ancestor` via `evaluate_xpath_detailed`). Ship
// `scroll_into_view` as-is and validate it when we add a headed-test
// mode or pick up a toolkit that implements `scroll_to` more
// completely.

/// Calling `scroll_into_view()` on an element that's already inside the
/// viewport is a no-op — no `scrolled` event should fire.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_scroll_into_view_noop_when_already_visible() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let cursor = session.stdout_cursor();
    session
        .locate("//ListItem[@name='scroll-row-0']")
        .scroll_into_view()
        .await?;

    // Short wait — if scroll happened it would have fired by now.
    let res = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("scrolled scroll-area"),
            std::time::Duration::from_millis(400),
        )
        .await;
    assert!(
        res.is_err(),
        "scroll_into_view on already-visible row shouldn't fire scroll event, but got: {res:?}"
    );

    kill(session).await?;
    Ok(())
}

/// Screenshot before/after toggling a ToggleButton — proves that locator
/// actions produce real pixel changes in the compositor, not just AT-SPI
/// state updates.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_toggle_changes_screenshot() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let baseline = extract_png(&session.take_screenshot().await?)?;
    assert!(baseline.len() > 1000, "baseline screenshot too small");

    let cursor = session.stdout_cursor();
    session
        .locate("//ToggleButton[@name='mode-toggle']")
        .click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("toggled mode-toggle"),
            Duration::from_secs(3),
        )
        .await?;

    // Extra beat for the compositor to flush the repaint.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let after = extract_png(&session.take_screenshot().await?)?;
    let diff_pixels = diff_png_pixels(&baseline, &after)?;
    eprintln!("pixel diff: {diff_pixels}");

    kill(session).await?;

    assert!(
        diff_pixels > 100,
        "screenshot should change after toggling mode-toggle (only {diff_pixels} pixels differ)"
    );
    Ok(())
}

/// XPath tree inspection, counts, and element-not-found error shape.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_tree_and_locator_features() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let tree = session.dump_tree().await?;
    assert!(!tree.is_empty(), "accessibility tree should not be empty");
    assert!(
        tree.contains("<?xml"),
        "tree should start with XML declaration, got:\n{tree}"
    );
    assert!(
        tree.contains("<Button"),
        "tree should contain Button elements, got:\n{tree}"
    );

    // Known element — primary-button — resolves.
    assert!(
        session
            .locate("//Button[@name='primary-button']")
            .count()
            .await?
            >= 1,
        "should find primary-button"
    );

    // Missing element yields ElementNotFound on a single-target action.
    let err = session
        .locate("//Button[@name='nonexistent_xyz_12345']")
        .with_timeout(Duration::from_millis(250))
        .click()
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::ElementNotFound { .. }),
        "expected ElementNotFound, got: {err}"
    );

    // Auto-wait on an already-visible button returns quickly.
    session
        .locate("//Button[@name='primary-button']")
        .wait_for_visible()
        .await?;

    // wait_for_count accepts the current count as a no-op.
    let button_count = session.locate("//Button").count().await?;
    assert!(button_count > 0, "expected some buttons, got 0");
    session
        .locate("//Button")
        .wait_for_count(button_count)
        .await?;

    kill(session).await?;
    Ok(())
}

/// End-to-end verification of `Session::press_chord`: types characters
/// into the text-entry and asserts the final state via stdout events.
/// Stdout events are ground truth here — the fixture fires
/// `text-changed text-entry text="..."` on every keystroke, which is a
/// much stronger signal than any a11y property read.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_keyboard_chord_dispatches_modifiers() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Wait for the fixture's text-entry to grab focus client-side.
    // `Session::start` already primes mutter's keyboard-focus
    // assignment with a no-op `Shift_L` press, so the first real
    // keystroke after focus-acquired now reliably lands on the entry —
    // the 15-keystroke warmup loop that used to live here is no longer
    // necessary.
    session
        .wait_for_stdout_line(
            0,
            |l| l.contains("focus-acquired text-entry"),
            Duration::from_secs(5),
        )
        .await?;

    // Real test starts here. Cursor captures the state after clear +
    // settle; anything appearing after this point is produced by the
    // presses we're about to issue.

    // Type "hi". Proves single-key chord dispatch lands in the entry.
    let cursor = session.stdout_cursor();
    for ch in ['h', 'i'] {
        session.press_chord(&ch.to_string()).await?;
    }
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("text-changed text-entry") && l.contains("\"hi\""),
            Duration::from_secs(3),
        )
        .await?;

    // Modifier chord: Shift+j should produce an uppercase 'J'. Proves
    // modifiers hold through the target key press and release cleanly
    // on the unwind path. (Ctrl+A + BackSpace would be the more natural
    // "clear" primitive but Ctrl-letter chords end up getting mapped to
    // the control-character keysym — 0x01 for Ctrl+A — before reaching
    // the client, so Ctrl-chords can't be verified by text output alone.)
    let shift_cursor = session.stdout_cursor();
    session.press_chord("Shift+j").await?;
    session
        .wait_for_stdout_line(
            shift_cursor,
            |l| l.contains("text-changed text-entry") && l.contains("\"hiJ\""),
            Duration::from_secs(3),
        )
        .await?;

    // Typing another plain 'k' after the Shift chord should land as
    // lowercase — if Shift stayed stuck, we'd see 'K' and the check
    // below would fail.
    let unstuck_cursor = session.stdout_cursor();
    session.press_chord("k").await?;
    session
        .wait_for_stdout_line(
            unstuck_cursor,
            |l| l.contains("text-changed text-entry") && l.contains("\"hiJk\""),
            Duration::from_secs(3),
        )
        .await?;

    kill(session).await?;
    Ok(())
}

/// Auto-wait exercises a state transition that only shows up after a
/// compositor frame tick: clicking the header-bar menu button opens the
/// popover, whose visible state we observe via a subsequent locator
/// query that polls until the expanded state flips.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_main_menu_opens_auto_waits() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // The main-menu button wraps a ToggleButton with role=toggle-button;
    // AT-SPI exposes the wrapping Button with a `main-menu` name and an
    // inner ToggleButton. Click the ToggleButton to open the popover.
    session
        .locate("//ToggleButton[@name='main-menu']")
        .click()
        .await?;

    // Wake GTK's event loop so the popover actually renders. AT-SPI actions
    // mutate GTK's model but don't tick the frame clock; in headless mutter
    // the popover won't appear in the tree until a compositor event forces
    // a repaint.
    session.press_keysym(0xffe1).await?; // Shift_L

    // Auto-wait: the outer Button's `expanded` state should flip to true
    // once the popover is open. wait_for_visible polls the snapshot until
    // this becomes matchable — exercising the auto-wait retry machinery
    // against a real state transition.
    session
        .locate("//Button[@name='main-menu' and @expanded='true']")
        .wait_for_visible()
        .await?;

    // Dump the tree so the popover structure is visible in CI logs.
    let tree = session.dump_tree().await?;
    eprintln!(
        "── tree after opening menu ─────────────────────────────────\n{tree}\n\
         ────────────────────────────────────────────────────────────"
    );

    kill(session).await?;
    Ok(())
}

/// Exercises pointer-input primitives against a running app — proves
/// the InputBackend pointer_motion_relative / pointer_button dispatch
/// path doesn't error under headless mutter.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_pointer_input_operations() -> anyhow::Result<()> {
    init_tracing();
    let (session, state) = start_fixture_session("gtk4").await?;

    // Verify Session::wayland_display() accessor.
    assert!(
        session.wayland_display().starts_with("wayland-wd-"),
        "unexpected display name: {}",
        session.wayland_display()
    );

    // Create a second InputBackend from the shared compositor state — the
    // pattern tests use when they need both the Session's backend and a
    // directly-owned one for pointer calls.
    let pointer = MutterInput::new(state);
    // Test-scope cancellation token: this second backend isn't owned by
    // the Session so it can't reuse Session's token; a never-cancelled
    // token is the right default for a straight-line input sequence.
    let cancel = tokio_util::sync::CancellationToken::new();

    pointer
        .pointer_motion_relative(100.0, 100.0, &cancel)
        .await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    pointer
        .pointer_button(waydriver::PointerButton::Left, &cancel)
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    pointer
        .pointer_motion_relative(-50.0, -50.0, &cancel)
        .await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Confirm session is still functional by taking a screenshot.
    let screenshot = session.take_screenshot().await?;
    let png = extract_png(&screenshot)?;
    assert!(png.len() > 1000, "screenshot after pointer ops too small");

    kill(session).await?;
    Ok(())
}

/// Smoke test for `Session::pointer_motion_absolute`. We can't assert
/// that the pointer landed on a specific widget — GTK4's AT-SPI bridge
/// often returns widget-local bounds instead of screen coords, so we'd
/// be clicking at unpredictable locations. Instead, verify the call
/// completes without error and the session stays healthy (screenshot
/// still works). Real validation of absolute positioning belongs in
/// backends that expose trustworthy widget geometry.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_pointer_motion_absolute_call_succeeds() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Move to a few different coordinates inside the 1024x768 virtual
    // monitor. All should succeed without D-Bus errors.
    for (x, y) in [(100.0, 100.0), (500.0, 400.0), (800.0, 600.0)] {
        session.pointer_motion_absolute(x, y).await?;
    }

    // Session should still be functional.
    let screenshot = session.take_screenshot().await?;
    let png = extract_png(&screenshot)?;
    assert!(
        png.len() > 1000,
        "screenshot after absolute moves too small"
    );

    kill(session).await?;
    Ok(())
}

/// `Locator::fill` against the Entry, which the fixture auto-focuses
/// at startup. Exercises both `FillMode::CaretNav` and
/// `FillMode::SelectAll` — the latter specifically validates that the
/// clear step actually clears prior content before typing.
///
/// TextView (`notes-area`) isn't exercised here: it has no initial
/// focus, and `fill`'s pointer-click focus fallback depends on
/// screen-accurate bounds which GTK4's AT-SPI bridge doesn't provide.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_locator_fill_on_entry() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Wait for the fixture's startup focus grab on text-entry before we
    // start racing the compositor's keyboard routing.
    session
        .wait_for_stdout_line(
            0,
            |l| l.contains("focus-acquired text-entry"),
            Duration::from_secs(5),
        )
        .await?;
    // Wait for the text-entry to actually appear in the AT-SPI tree —
    // the fixture's GTK process registers widgets asynchronously, so
    // the tree can lag the visible UI. Settled tree is what `fill`
    // needs to resolve its XPath.
    session
        .locate("//*[@name='text-entry']")
        .with_timeout(Duration::from_secs(10))
        .wait_for_visible()
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 1. Fill the Entry with CaretNav. Entry starts empty; caret nav
    // over an empty buffer is a no-op, then Delete is a no-op, then
    // type writes the content. GTK4 sometimes exposes both the wrapper
    // Entry and its inner text model under the same accessible name,
    // so we pin with `.first()` rather than risk the selector matching
    // two elements.
    let entry = || {
        session
            .locate("//*[@name='text-entry']")
            .first()
            .with_timeout(Duration::from_secs(10))
    };

    // Fill the Entry. Entry starts empty, so CaretNav's clear step is
    // a no-op; the typing step produces a `text-changed` event for
    // each character, ending with the full string. SelectAll mode
    // (Ctrl+A) isn't exercised here — mutter's keyboard simulator
    // maps Ctrl+letter chords to control-characters (Ctrl+A → 0x01)
    // rather than dispatching them as a real modifier+key chord, so
    // `FillMode::SelectAll` doesn't actually select-all under
    // headless mutter. It's still the right choice for widgets where
    // caret nav is unreliable — the two modes exist to let callers
    // pick the one their target app honors.
    let cursor = session.stdout_cursor();
    entry()
        .fill_with_opts("hello world", FillMode::CaretNav)
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("text-changed text-entry") && l.contains("\"hello world\""),
            Duration::from_secs(5),
        )
        .await?;

    // Second fill proves that caret-nav clear actually clears the
    // prior content — otherwise we'd get "hello worldreplaced".
    let cursor = session.stdout_cursor();
    entry()
        .fill_with_opts("replaced", FillMode::CaretNav)
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("text-changed text-entry") && l.contains("text=\"replaced\""),
            Duration::from_secs(5),
        )
        .await?;

    kill(session).await?;
    Ok(())
}

/// `Locator::select_option` against the GTK4 fixture's selection widgets.
/// Each assertion reads the fixture's stdout event — ground truth that
/// the selection model actually advanced, not just that the AT-SPI call
/// returned OK.
///
/// GTK4 toolkit note: `GtkDropDown` doesn't expose the AT-SPI Selection
/// interface on the widget (its selected index is surfaced only via the
/// container's accessible name and a `valuetext` attribute, with the
/// item list absent from the a11y tree until the popup opens). Selecting
/// into a DropDown via the pure-AT-SPI path isn't possible until the
/// toolkit fills that gap — the error-shape test below locks in the
/// clean failure mode for now. The widgets below exercise the paths
/// that do work today:
///
/// - `GtkComboBoxText` (`size-combo`) — Selection on the container; the
///   menu items don't appear in the a11y tree with the popup closed, so
///   `SelectBy::Label` would fail here. `Index` mode works.
/// - `GtkListBox` (`item-list`) — rows are always in the a11y tree, so
///   both `Label` and `Index` modes work directly.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_select_option_combos() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // GtkComboBoxText: index-based. Fixture defaults to "Medium" (index
    // 1); pick index 0 (Small) and check the active_id in the event.
    let cursor = session.stdout_cursor();
    session
        .locate("//ComboBox[@name='size-combo']")
        .select_option(SelectBy::Index(0))
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("selected size-combo") && l.contains("active_id=\"s\""),
            Duration::from_secs(3),
        )
        .await?;

    // GtkListBox: pick row 2 by index. No row is selected at startup.
    let cursor = session.stdout_cursor();
    session
        .locate("//List[@name='item-list']")
        .select_option(SelectBy::Index(2))
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("row-selected item-list index=2"),
            Duration::from_secs(3),
        )
        .await?;

    // GtkListBox, Label mode: rows are in the a11y tree with stable
    // accessible names ("item-row-0", "item-row-1", ...), so label
    // dispatch resolves cleanly. Pick "item-row-0" and verify the
    // row-selected event reports index=0.
    let cursor = session.stdout_cursor();
    session
        .locate("//List[@name='item-list']")
        .select_option(SelectBy::Label("item-row-0"))
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("row-selected item-list index=0"),
            Duration::from_secs(3),
        )
        .await?;

    kill(session).await?;
    Ok(())
}

/// Error shape when the located element doesn't implement Selection.
/// A plain Button doesn't — select_option should surface a helpful
/// `Error::Atspi` rather than a cryptic D-Bus panic.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_select_option_errors_on_non_selection_widget() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let err = session
        .locate("//Button[@name='primary-button']")
        .select_option(SelectBy::Index(0))
        .await
        .unwrap_err();
    // A Button has no Selection interface. Either a select_child call
    // returns false (mapped to Error::Atspi), or the D-Bus proxy build
    // fails with NotSupported (also mapped to Error::Atspi). Both are
    // fine — the caller just needs a readable error.
    assert!(
        matches!(err, Error::Atspi { .. } | Error::ElementStale { .. }),
        "expected Atspi-flavored error, got {err:?}"
    );

    kill(session).await?;
    Ok(())
}

/// `Locator::hover`, `double_click`, and `right_click` against the fixture's
/// pointer-targets row. Each widget emits a distinct stdout event when the
/// matching pointer gesture lands on it, so tests can assert the element-
/// scoped method actually routed a real pointer event to the target rather
/// than just completing its D-Bus calls successfully.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_locator_pointer_actions() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Hover → pointer-enter.
    let cursor = session.stdout_cursor();
    session.locate("//*[@name='hover-target']").hover().await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("pointer-enter hover-target"),
            Duration::from_secs(3),
        )
        .await?;

    // Right-click → right-click event on the ctx-target. Move the
    // pointer away from the hover-target first so the previous event
    // controller doesn't fire `pointer-leave` in the middle of the
    // right-click dispatch and confuse the stdout cursor.
    let cursor = session.stdout_cursor();
    session
        .locate("//*[@name='ctx-target']")
        .right_click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("right-click ctx-target"),
            Duration::from_secs(3),
        )
        .await?;

    // Double-click → exactly one `double-click` event. GestureClick with
    // n_press == 2 only fires on the second press of a real double-click,
    // so a single emission proves the two pointer_button calls landed
    // inside the system double-click window.
    let cursor = session.stdout_cursor();
    session
        .locate("//*[@name='dc-target']")
        .double_click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("double-click dc-target"),
            Duration::from_secs(3),
        )
        .await?;

    kill(session).await?;
    Ok(())
}

/// `Locator::drag_to` against the DnD section. Holding the primary button
/// across intermediate pointer moves should drive GTK4's DnD machinery
/// end-to-end: `drag-started` on the source, `drag-entered` on the target,
/// `dropped` on release.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_locator_drag_to_drops_payload() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("dnd").await?;

    let cursor = session.stdout_cursor();
    let source = session.locate("//*[@name='drag-source']");
    let target = session.locate("//*[@name='drop-target']");
    source.drag_to(&target).await?;

    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("dropped drop-target"),
            Duration::from_secs(5),
        )
        .await?;

    kill(session).await?;
    Ok(())
}

/// `Session::cancel` interrupts a stuck `wait_for_visible` against a real
/// mutter + GTK4 fixture within ~1s — the integration counterpart to the
/// mocked-backend tests in `locator::tests::poll_with_retry_*`.
///
/// Without this, a future refactor that quietly drops the cancellation
/// token from `poll_with_retry` (or replaces a method that takes
/// `&CancellationToken`) wouldn't be caught by the unit suite, and a
/// real `kill_session` would silently regress to waiting out the full
/// 30s deadline.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_session_cancel_interrupts_wait_for_visible() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Selector that's structurally valid but will never match — forces the
    // locator into the auto-wait poll loop until the deadline or a cancel.
    let locator = session
        .locate("//*[@name='nonexistent-widget-for-cancel-test']")
        .with_timeout(Duration::from_secs(30));

    // Drive the wait on a separate task so we can fire `cancel()` from the
    // test thread while it's parked inside `poll_with_retry`. The session
    // clone is moved into the task and dropped on its return, so `kill()`
    // below sees a unique Arc.
    let session_for_wait = session.clone();
    let waiter = tokio::spawn(async move {
        let _keep = session_for_wait;
        locator.wait_for_visible().await
    });

    // Give the auto-wait one full poll iteration (initial delay is well
    // under 100ms) so the cancel races a parked sleep, not an empty loop.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let cancel_at = std::time::Instant::now();
    session.cancel();

    // Bound the join so a regression doesn't hang the test process;
    // failure surfaces as the outer Timeout instead.
    let result = tokio::time::timeout(Duration::from_secs(2), waiter).await;
    let elapsed = cancel_at.elapsed();

    let join_outcome = result.map_err(|_| {
        anyhow::anyhow!(
            "wait_for_visible did not return within 2s of cancel — cancellation propagation regressed"
        )
    })?;
    let inner = join_outcome.map_err(|e| anyhow::anyhow!("waiter task panicked: {e}"))?;
    match inner {
        Err(Error::Cancelled) => {}
        Err(other) => anyhow::bail!("expected Error::Cancelled, got {other:?}"),
        Ok(()) => anyhow::bail!("wait_for_visible returned Ok on a never-matching selector"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "cancel should propagate quickly; elapsed = {elapsed:?}"
    );

    kill(session).await?;
    Ok(())
}

/// Regression: cold-start first-keypress drop. `Session::start` primes
/// mutter's keyboard-focus assignment with a no-op `Shift_L` press, so a
/// single `press_chord("a")` after `focus-acquired text-entry` reliably
/// produces a `text-changed text-entry` event. Without the prime, the
/// first keystroke is consumed by mutter's focus-assignment and the wait
/// below times out.
///
/// Each `cargo test` invocation spawns a fresh mutter session per test,
/// so the prior tests in the binary don't change the outcome — but the
/// MCP-server use case is exactly the same single-session cold start
/// this test asserts on.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn cold_first_keypress_lands_without_warmup() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Wait until the fixture's text-entry grabs keyboard focus client-side.
    session
        .wait_for_stdout_line(
            0,
            |l| l.contains("focus-acquired text-entry"),
            Duration::from_secs(5),
        )
        .await?;

    // Single keypress — no warmup loop.
    let cursor = session.stdout_cursor();
    session.press_chord("a").await?;
    let result = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("text-changed text-entry"),
            Duration::from_secs(3),
        )
        .await;

    let outcome = result.is_ok();
    eprintln!(
        "cold first-keypress outcome: {}",
        if outcome { "LANDED" } else { "DROPPED" }
    );
    kill(session).await?;
    assert!(
        outcome,
        "first press_chord after focus-acquired did not produce a text-changed event"
    );
    Ok(())
}

/// Regression: `grab_png_sync` mutates the parent process's
/// `PIPEWIRE_REMOTE` to point `pipewiresrc` at the live session's
/// pipewire socket. After the session is killed, that socket is gone.
/// If the next session's spawned `pipewire`/`wireplumber`/`mutter`
/// inherit the stale env var, mutter's `ScreenCast.Start` fails with
/// "Couldn't connect pipewire context" — deterministically, every
/// time. Two sessions back-to-back in one process, each taking a
/// screenshot, exercises the leak path.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn sequential_sessions_share_no_pipewire_env() -> anyhow::Result<()> {
    init_tracing();

    let (session1, _state1) = start_fixture_session("gtk4").await?;
    let png1 = extract_png(&session1.take_screenshot().await?)?;
    assert!(png1.len() > 1000, "first-session screenshot too small");
    kill(session1).await?;

    let (session2, _state2) = start_fixture_session("gtk4").await?;
    let png2 = extract_png(&session2.take_screenshot().await?)?;
    assert!(png2.len() > 1000, "second-session screenshot too small");
    kill(session2).await?;

    Ok(())
}

/// AdwButtonRow surfaces in AT-SPI as `<Button name="adw-button-row">`
/// but implements neither `Action` nor `Component::grab_focus`, so it
/// can't be driven through the fast AT-SPI paths every other Button
/// accessible accepts. `Locator::click()` papers over this with a
/// pointer-click fallback (parallel to the fill→pointer-click fallback
/// for widgets missing `Component::grab_focus`): when
/// `Action.DoAction(0)` returns `NotSupported`, click moves the pointer
/// to the element's centre and synthesizes a real left-button press.
///
/// Before the fallback shipped, `click()` errored with `No action with
/// index 0` and the caller had to know to drop down to pointer events.
/// This test pins that the error is gone — `click()` resolves without
/// surfacing an AT-SPI Action gap to the caller.
///
/// Asserting the *resulting* `activated adw-button-row` event isn't
/// reliable in this test harness: headless mutter has a documented
/// cold-start pointer race (see `Session::start`) where the first
/// pointer events on a freshly-mapped toplevel get dropped, and the
/// existing `fixture_locator_pointer_actions` test fails on `main`
/// today for the same reason. Real environments don't have that race
/// — once it's fixed (or a reliable per-session pointer warmup lands),
/// this test should grow a positive activation assertion.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_adw_button_row_click_via_pointer_fallback() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;

    // The bare assertion: `click()` resolves without an AT-SPI Action
    // error. Pre-fallback this would have errored with `No action with
    // index 0`; with the fallback it routes through pointer-click.
    session
        .locate("//Button[@name='adw-button-row']")
        .click()
        .await?;

    kill(session).await?;
    Ok(())
}

/// AdwSwitchRow surfaces in AT-SPI as **two** `<Switch name="adw-switch-row">`
/// nodes — the outer activatable row wraps the inner GtkSwitch (with two
/// `Generic` containers in between), and both inherit the row title as
/// their accessible name. Only the inner toggle implements the AT-SPI
/// `Action` interface (`Action.DoAction(0)` on the outer row errors with
/// `No action with index 0`), so the selector has to disambiguate to the
/// inner one.
///
/// The natural way is the descendant-axis selector
/// `//Switch[@name='adw-switch-row']//Switch` — "the Switch nested
/// under the row's Switch" — which uniquely matches the inner toggle.
/// It expresses the parent/child relationship directly, doesn't depend
/// on a `.nth(...)` index, and reads as the intent (target the nested
/// one).
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_adw_switch_row_toggle_emits_event() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;

    // Sanity: confirm the documented duplicate exists and the
    // descendant-axis selector uniquely picks the inner toggle. If
    // either count changes, libadwaita reshaped the AT-SPI tree and
    // the selector advice in the doc comment + memory needs to be
    // revisited.
    assert_eq!(
        session
            .locate("//Switch[@name='adw-switch-row']")
            .count()
            .await?,
        2,
        "AdwSwitchRow should expose two Switch accessibles (row + inner toggle)"
    );
    assert_eq!(
        session
            .locate("//Switch[@name='adw-switch-row']//Switch")
            .count()
            .await?,
        1,
        "the descendant-axis selector should uniquely match the inner Switch"
    );

    let cursor = session.stdout_cursor();
    session
        .locate("//Switch[@name='adw-switch-row']//Switch")
        .click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("toggled adw-switch-row active=true"),
            Duration::from_secs(3),
        )
        .await?;

    kill(session).await?;
    Ok(())
}

/// Regression guard for the pipewire runtime-dir nesting overflow.
///
/// Drives a **real** mutter + pipewire screenshot and recording with no GTK
/// fixture or AT-SPI, and asserts that each capture leaves `XDG_RUNTIME_DIR`
/// exactly as it found it. Leaving it pointed at the live session's runtime
/// dir is what made subsequent sessions nest one level deeper until the
/// AF_UNIX `sun_path` limit wedged pipewire — so this directly exercises the
/// fix (capture's `EnvGuard` restore) against the live stack, while also
/// confirming the restore doesn't break capture itself (valid PNG + non-empty
/// WebM produced).
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn capture_restores_runtime_env_and_still_produces_media() -> anyhow::Result<()> {
    use waydriver::CaptureBackend;

    init_tracing();

    // The value the parent process must be restored to after every capture.
    let before = std::env::var_os("XDG_RUNTIME_DIR");

    let mut compositor = MutterCompositor::new();
    compositor.start(None, None).await?;
    let state = compositor
        .state()
        .expect("MutterCompositor::state is Some immediately after start()");
    let capture = MutterCapture::new(state);

    // ── Screenshot path: grab_png → EnvGuard ──────────────────────────────
    let stream = capture.start_stream().await?;
    let raw = capture.grab_screenshot(&stream).await?;
    let png = extract_png(&raw)?;
    assert!(
        png.len() > 1000,
        "screenshot PNG implausibly small ({} bytes) — capture path broken",
        png.len()
    );
    assert_eq!(
        std::env::var_os("XDG_RUNTIME_DIR"),
        before,
        "XDG_RUNTIME_DIR not restored after screenshot — capture leaked the session dir \
         into the parent env (this is what caused the nesting overflow)"
    );
    capture.stop_stream(stream).await?;

    // ── Recording path: start_recording_sync → wait-for-PLAYING → EnvGuard ─
    let webm = std::env::temp_dir().join(format!(
        "waydriver-capture-regression-{}.webm",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&webm);
    let rec_stream = capture.start_recording_stream().await?;
    let recorder = capture
        .start_recording(&rec_stream, &webm, 2_000_000, 15)
        .await?;
    // Let it encode a couple of seconds of (videorate-duplicated) frames.
    tokio::time::sleep(Duration::from_secs(2)).await;
    capture.stop_recording(recorder).await?;
    capture.stop_recording_stream(rec_stream).await?;

    let webm_len = std::fs::metadata(&webm)
        .map(|m| m.len())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&webm);
    assert!(
        webm_len > 0,
        "recorded WebM is empty — recorder's wait-for-PLAYING/restore broke capture"
    );
    assert_eq!(
        std::env::var_os("XDG_RUNTIME_DIR"),
        before,
        "XDG_RUNTIME_DIR not restored after recording — capture leaked the session dir \
         into the parent env (this is what caused the nesting overflow)"
    );

    // Drop capture (releasing its MutterState Arc) before tearing the
    // compositor down, honoring the shared-state invariant.
    drop(capture);
    CompositorRuntime::stop(&mut compositor).await?;
    Ok(())
}
