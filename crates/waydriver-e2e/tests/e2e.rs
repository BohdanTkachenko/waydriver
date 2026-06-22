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
    CompositorRuntime, Error, FillMode, InputBackend, Role, SelectBy, Session, SessionConfig,
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
    start_fixture_session_opts(section, false).await
}

/// Like [`start_fixture_session`], but lets the caller turn on external-effect
/// capture (mock notification + portal sinks) for the session.
async fn start_fixture_session_opts(
    section: &str,
    capture_external_effects: bool,
) -> anyhow::Result<(Arc<Session>, Arc<MutterState>)> {
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
            capture_external_effects,
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

/// Issue #55: an element's AT-SPI `accessible-description` is readable from the
/// snapshot via `Locator::description`, distinct from its name. The fixture's
/// `hover-target` Label carries both a Label (name "hover-target") and a
/// Description ("hover over me"); this reads back both and asserts the
/// description doesn't leak into the name, and that a widget without a
/// description reads back `None`.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_exposes_accessible_description() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Scope the locators so none outlives the `kill` below (which requires the
    // session Arc to be uniquely held).
    {
        let hover = session.locate("//*[@name='hover-target']");
        assert_eq!(
            hover.name().await?.as_deref(),
            Some("hover-target"),
            "premise: the label's accessible name is its Label property"
        );
        assert_eq!(
            hover.description().await?.as_deref(),
            Some("hover over me"),
            "Locator::description should read the AT-SPI accessible-description"
        );

        // A widget with no Description set reads back None — description must not
        // be backfilled from the name.
        let entry = session.locate("//*[@name='text-entry']");
        assert_eq!(
            entry.description().await?,
            None,
            "an element without an accessible-description should read back None"
        );
    }

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
            // NB: adw-spin-row is intentionally absent here — AdwSpinRow
            // realizes cache-only (tree-invisible), so it's asserted via the
            // cache in `fixture_adw_spin_row_value_readable_by_cache_ref`, not
            // through this tree-based inventory check.
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

/// Issue #53: an `AdwEntryRow`'s text is readable from a cache `(bus, path)`
/// ref via `Session::text_ref`. The editable text lives on a child accessible
/// (a `GtkText`, AT-SPI role `Text`), not the row itself; the test finds that
/// `Text` accessible in the cache and reads back the preset value
/// ("preset-title") — the "the row shows X" readback the issue is about.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_adw_entry_row_text_readable_by_cache_ref() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;

    // Realize the rows into the cache; the entry's editable surfaces as a
    // `Text`-role accessible carrying the LABELLED_BY-resolved row name.
    session.focus_walk(25).await?;
    let cached = session.cached_accessibles().await?;
    let entry = cached
        .iter()
        .find(|c| c.role == "Text")
        .map(|c| c.ref_.clone())
        .unwrap_or_else(|| {
            let shape: Vec<_> = cached
                .iter()
                .map(|c| (c.role.as_str(), c.name.clone()))
                .collect();
            panic!("adw section should expose a Text editable (AdwEntryRow); cache had: {shape:?}")
        });

    let text = session.text_ref(&entry.0, &entry.1).await?;
    assert_eq!(
        text, "preset-title",
        "AdwEntryRow preset text should be readable via text_ref from a cache ref"
    );

    kill(session).await?;
    Ok(())
}

/// Issue #53: an `AdwSpinRow`'s numeric value is readable from its cache `(bus,
/// path)` ref via `Session::value_ref`. The Value interface lives on the inner
/// spin button (AT-SPI role `SpinButton`), so the test finds that accessible in
/// the cache and reads its preset value (1.00) and range (0..10).
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_adw_spin_row_value_readable_by_cache_ref() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;

    // The spin row is below the auto-focused entry row, so it only enters the
    // cache after a focus walk realizes it.
    session.focus_walk(25).await?;
    let cached = session.cached_accessibles().await?;
    let spin = cached
        .iter()
        .find(|c| c.role == "SpinButton")
        .map(|c| c.ref_.clone())
        .unwrap_or_else(|| {
            let shape: Vec<_> = cached
                .iter()
                .map(|c| (c.role.as_str(), c.name.clone()))
                .collect();
            panic!("adw section should expose a SpinButton (AdwSpinRow); cache had: {shape:?}")
        });

    let v = session.value_ref(&spin.0, &spin.1).await?;
    assert_eq!(v.current, 1.0, "AdwSpinRow preset value should be 1.0");
    assert_eq!(v.minimum, 0.0, "AdwSpinRow adjustment lower bound");
    assert_eq!(v.maximum, 10.0, "AdwSpinRow adjustment upper bound");

    kill(session).await?;
    Ok(())
}

/// Issue #53: `Session::selected_text_ref` reads a `Selection` container's
/// current choice from its cache `(bus, path)` ref — the read-side counterpart
/// to `Locator::select_option`. Drives a `GtkListBox` selection with
/// `select_option`, then reads it straight back from the cache ref, with no
/// tree node in between.
///
/// (`GtkListBox` is a real AT-SPI `Selection` container, unlike the
/// dropdown-style combos — `GtkDropDown` / `AdwComboRow` — whose options are
/// realized only on popup-open and which expose no working `Selection`
/// interface while closed. See `selected_text_ref`'s docs.)
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_listbox_selection_readable_by_cache_ref() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Deterministically select a known row so the readback can't pass by
    // coincidence of whatever focus left selected.
    session
        .locate("//*[@name='item-list']")
        .select_option(SelectBy::Label("item-row-1"))
        .await?;

    // The two GtkListBoxes in this section are `item-list` (3 rows) and
    // `scroll-area` (40 rows); child_count picks the former unambiguously.
    let cached = session.cached_accessibles().await?;
    let list = cached
        .iter()
        .find(|c| c.role == "List" && c.child_count == 3)
        .map(|c| c.ref_.clone())
        .unwrap_or_else(|| {
            let shape: Vec<_> = cached
                .iter()
                .filter(|c| c.role == "List")
                .map(|c| (c.role.as_str(), c.child_count))
                .collect();
            panic!("gtk4 section should expose item-list (List, 3 rows); lists were: {shape:?}")
        });

    let selected = session.selected_text_ref(&list.0, &list.1).await?;
    assert_eq!(
        selected, "item-row-1",
        "selected_text_ref should read back the row select_option just selected"
    );

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

    // `stream` (a `Pin<&mut>` borrowing `a11y`) and `a11y` hold no session
    // Arc, so they drop naturally at scope end — in reverse declaration order,
    // i.e. the stream releases its borrow before the connection closes.
    kill(session).await?;
    Ok(())
}

/// OPEN 1 regression: `Cache.GetItems` must tolerate AT-SPI role indices the
/// `atspi` crate's `Role` enum doesn't know (e.g. role 130 from libadwaita's
/// newer AdwPreferences rows). Before the fix the strict enum rejected the
/// *whole* reply, so `cached_accessibles` returned `Err` and blanked everything
/// — not just the one unknown row. The `adw` fixture has `ComboRow`/`SwitchRow`/
/// `ActionRow`/`EntryRow`/`ButtonRow` realized at construction, which is what
/// surfaces the high role index.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn cache_items_tolerate_unknown_role_index() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;

    // Realize the interactive row children (the GtkSwitch inside an
    // AdwSwitchRow is `ATSPI_ROLE_SWITCH` = index 130, which atspi 0.29's enum
    // lacks). They aren't cached until focused, mirroring the reporter's
    // focus_walk-then-read repro.
    session.focus_walk(15).await?;

    // The core regression: the call must succeed (and return the cache) rather
    // than erroring out on the first unrecognised role index.
    let items = session.cached_accessibles().await?;
    assert!(
        !items.is_empty(),
        "cached_accessibles returned no items — the cache read was blanked"
    );

    // Informational: surface which roles the atspi enum didn't know but we kept
    // as `unknown-role-N` instead of failing the parse.
    let unknown: Vec<&str> = items
        .iter()
        .filter(|c| c.role.starts_with("unknown-role-"))
        .map(|c| c.role.as_str())
        .collect();
    eprintln!(
        "cached {} items; {} with unknown role index: {:?}",
        items.len(),
        unknown.len(),
        unknown
    );

    kill(session).await?;
    Ok(())
}

/// OPEN 2 regression (two parts):
///   1. `Locator::activate()` drives the AT-SPI `Action` interface — verified on
///      a `GtkButton`, which exposes it (the class of widget the reporter's
///      *working* cases use: buttons, links, cards).
///   2. An activatable `AdwActionRow` — which exposes **no** AT-SPI Action — is
///      activated by a pixel `click()` scoped to the row (`//ListItem`), the
///      path the Bug 7 fix made land correctly. (`find_by_text(title)`/a bare
///      `//*[@name]` resolve to the row's same-named title `Label` instead, the
///      reporter's likely miss.)
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn activate_drives_action_and_rows_activate_via_click() -> anyhow::Result<()> {
    init_tracing();

    // Part 1: activate() on an Action-exposing GtkButton.
    {
        let (session, _state) = start_fixture_session("gtk4").await?;
        let cursor = session.stdout_cursor();
        session
            .locate("//Button[@name='primary-button']")
            .activate()
            .await?;
        session
            .wait_for_stdout_line(
                cursor,
                |l| l.contains("clicked primary-button"),
                Duration::from_secs(3),
            )
            .await?;
        kill(session).await?;
    }

    // Part 2: AdwActionRow activated via click() on the row (ListItem).
    {
        let (session, _state) = start_fixture_session("adw").await?;
        let cursor = session.stdout_cursor();
        session
            .locate("//ListItem[@name='adw-action-row']")
            .click()
            .await?;
        let line = session
            .wait_for_stdout_line(
                cursor,
                |l| l.contains("activated adw-action-row"),
                Duration::from_secs(3),
            )
            .await?;
        assert!(
            line.contains("activated adw-action-row"),
            "click() on the row should fire connect_activated, got: {line}"
        );
        kill(session).await?;
    }

    Ok(())
}

/// Issue #33 (Fix 3): drive GAction-only items over `org.gtk.Actions`.
///
/// `GtkPopoverMenu` / dialog items whose only role is "fire a GAction" never
/// enter the AT-SPI tree or cache, so there is no `(bus, path)` for
/// `Locator::activate` to grab. `Session::activate_action` instead talks to
/// the `app.*` / `win.*` action groups the app exports on its own session-bus
/// name. Verifies app actions, window actions, a string-target action, and
/// `list_actions`, each confirmed by the fixture's stdout side-effect.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn gaction_activation_fires_app_and_win_actions() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // app-level action (no parameter).
    let cursor = session.stdout_cursor();
    session.activate_action("app.ping").await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("action-activated app.ping"),
            Duration::from_secs(3),
        )
        .await?;

    // window-level action — exported at <base>/window/<id>, a different
    // action group than `app.*`.
    let cursor = session.stdout_cursor();
    session.activate_action("win.ping").await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("action-activated win.ping"),
            Duration::from_secs(3),
        )
        .await?;

    // string-target action via the GMenu detailed-name form.
    let cursor = session.stdout_cursor();
    session.activate_action("app.echo::hello").await?;
    let line = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("action-activated app.echo"),
            Duration::from_secs(3),
        )
        .await?;
    assert!(
        line.contains(r#"param="hello""#),
        "string target should reach the handler, got: {line}"
    );

    // Enumeration covers app.* and win.* groups.
    let actions = session.list_actions().await?;
    for expected in ["app.ping", "app.echo", "app.section", "win.ping"] {
        assert!(
            actions.iter().any(|a| a == expected),
            "list_actions() should include {expected}, got: {actions:?}"
        );
    }

    kill(session).await?;
    Ok(())
}

/// OPEN 2 probe: which path activates an activatable `AdwActionRow`? Compares a
/// screen-space `pointer_click` (pixel) against the AT-SPI `click()`
/// (`Action.do_action` path) — the fixture's `adw-action-row` emits
/// `activated adw-action-row` on `connect_activated`. **Finding:** when scoped
/// to the row (`//ListItem[...]`) *both* fire; the reporter's failure was a
/// selector/OCR resolving to the title `Label` child (same name, on top), where
/// a pixel click doesn't activate the row. `activate()` (AT-SPI do_action) is
/// the deterministic path. Diagnostic — read the ACT lines on stderr.
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn adw_action_row_activation_probe() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;

    // A) pixel pointer_click at the row's screen centre.
    let cursor = session.stdout_cursor();
    session
        .locate("//ListItem[@name='adw-action-row']")
        .pointer_click(waydriver::PointerButton::Left)
        .await?;
    let pix = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("activated adw-action-row"),
            Duration::from_secs(3),
        )
        .await;
    eprintln!("ACT pointer_click fired activation = {}", pix.is_ok());

    // B) AT-SPI click() on the ROW (tries do_action(0), then pixel fallback).
    let cursor = session.stdout_cursor();
    let click_res = session
        .locate("//ListItem[@name='adw-action-row']")
        .click()
        .await;
    let act = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("activated adw-action-row"),
            Duration::from_secs(3),
        )
        .await;
    eprintln!(
        "ACT row click() result={:?} fired activation = {}",
        click_res.map(|_| "ok"),
        act.is_ok()
    );

    // C) pixel pointer_click on the title LABEL child (what find_by_text(title)
    // targets) — does clicking the label activate the enclosing row?
    let cursor = session.stdout_cursor();
    let lbl = session
        .locate("//Label[@name='adw-action-row']")
        .pointer_click(waydriver::PointerButton::Left)
        .await;
    let lbl_act = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("activated adw-action-row"),
            Duration::from_secs(3),
        )
        .await;
    eprintln!(
        "ACT label pointer_click result={:?} fired activation = {}",
        lbl.map(|_| "ok"),
        lbl_act.is_ok()
    );

    // D) does the row expose AT-SPI Action at all? (activate() path)
    let cursor = session.stdout_cursor();
    let act_res = session
        .locate("//ListItem[@name='adw-action-row']")
        .activate()
        .await;
    let act_fired = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("activated adw-action-row"),
            Duration::from_secs(3),
        )
        .await;
    eprintln!(
        "ACT activate() result={:?} fired activation = {}",
        act_res.as_ref().map(|_| "ok").map_err(|e| e.to_string()),
        act_fired.is_ok()
    );

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

    // Fix 1: the row exposes no *direct* name — only its title Label does — so a
    // non-Label cache entry named "lazy-switch" proves the name was backfilled
    // from the LABELLED_BY relation, giving deterministic identity without OCR.
    let labelled = after
        .iter()
        .find(|c| c.name.as_deref() == Some("lazy-switch") && c.role != "Label");
    assert!(
        labelled.is_some(),
        "LABELLED_BY should backfill a non-Label row name 'lazy-switch'; entries named lazy-switch: {:?}",
        after
            .iter()
            .filter(|c| c.name.as_deref() == Some("lazy-switch"))
            .map(|c| (c.role.clone(), c.ref_.1.clone()))
            .collect::<Vec<_>>()
    );

    kill(session).await?;
    Ok(())
}

/// Fix 2 (with Fix 1): fire a row by its cached `(bus, path)` ref, with no tree
/// XPath. The AdwActionRow carries no direct name, so Fix 1's LABELLED_BY
/// backfill is what makes it findable by name in the cache; it also exposes an
/// AT-SPI Action that emits on activation, so `activate_ref` drives the side
/// effect straight from the cache reference.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn lazy_a11y_activate_ref_fires_row_by_cache_ref() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("activate-ref").await?;

    // The action row lives in the main window, so it's already realized into the
    // cache — no focus_walk needed. Find it by its LABELLED_BY-resolved name
    // (Fix 1), scoping to the activatable row: its title Label shares the name
    // but isn't the target. Clone the ref so no borrow of `cached` is held
    // across the activation call below.
    let cached = session.cached_accessibles().await?;
    let row_ref = cached
        .iter()
        .find(|c| c.name.as_deref() == Some("adw-action-row") && c.role == "ListItem")
        .map(|c| c.ref_.clone());
    let (bus, path) = row_ref.unwrap_or_else(|| {
        let named: Vec<_> = cached
            .iter()
            .filter(|c| c.name.as_deref() == Some("adw-action-row"))
            .map(|c| (c.role.as_str(), c.ref_.1.as_str()))
            .collect();
        panic!("Fix 1: action row should be identifiable by LABELLED_BY name; entries named adw-action-row: {named:?}")
    });

    // Fix 2: activate by the cache ref alone — no Locator, no XPath resolution.
    let cursor = session.stdout_cursor();
    session.activate_ref(&bus, &path).await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("activated adw-action-row"),
            Duration::from_secs(3),
        )
        .await?;

    kill(session).await?;
    Ok(())
}

/// Issue #56: actuate a cache-only, activatable row that exposes **no** AT-SPI
/// `Action` interface. `AdwButtonRow` is exactly that case — `do_action` on it
/// returns `NotSupported` (see `Locator::click`'s docs) — so `activate_ref`
/// must fall back to a synthesized pointer click at the row's `Component`
/// bounds rather than hard-erroring. Drives the row by its cache `(bus, path)`
/// ref alone and asserts its `activated` handler fires.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn activate_ref_falls_back_to_pointer_click_when_no_action() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;

    // Realize the prefs rows into the cache; the button row sits below the
    // auto-focused entry row, so a focus walk is what brings it in.
    session.focus_walk(25).await?;
    let cached = session.cached_accessibles().await?;
    let (bus, path) = cached
        .iter()
        .find(|c| c.name.as_deref() == Some("adw-button-row") && c.role != "Label")
        .map(|c| c.ref_.clone())
        .unwrap_or_else(|| {
            let named: Vec<_> = cached
                .iter()
                .filter(|c| c.name.as_deref() == Some("adw-button-row"))
                .map(|c| (c.role.as_str(), c.ref_.1.as_str()))
                .collect();
            panic!("adw section should expose an AdwButtonRow; entries named adw-button-row: {named:?}")
        });

    // `activate_ref` finds no Action interface and synthesizes a pointer click
    // at the row's centre via `click_ref` — the issue-#56 fallback.
    let cursor = session.stdout_cursor();
    session.activate_ref(&bus, &path).await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("activated adw-button-row"),
            Duration::from_secs(3),
        )
        .await?;

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

/// Documented-positive for the reported "unreleasable keyboard grab" bug:
/// with the fixture's `GtkPopoverMenu` open (GTK grab active), every input
/// channel works in this stack — synthesized **Escape dismisses** the
/// popover, **Down+Return navigates into it** and activates an item (the
/// section switches), and an **outside pointer click dismisses** it; the app
/// stays responsive throughout. The reported wedge did not reproduce.
///
/// Two measurement traps this probe corrects (both produced false "wedged"
/// readings in earlier versions — likely the same class of error behind the
/// original report): (1) the snapshot only emits `expanded` when the state is
/// SET, so "closed" must be detected as the *absence* of `expanded='true'` —
/// an `@expanded='false'` XPath never matches anything; (2) selector element
/// names must match snapshot roles (`TextBox`, not `Text`).
///
/// What remains true: the popover's menu items are absent from the AT-SPI
/// tree (`//MenuItem` = 0) — the known lazy-realization gap, now confirmed
/// for `GtkPopoverMenu` content too — so items can't be activated via AT-SPI
/// actions; use keyboard navigation (proven here) or OCR.
/// Diagnostic — read the GRAB lines on stderr.
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn grab_probe_popover_input_routing() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Popover open = the menu button currently carries expanded='true'.
    async fn popover_open(session: &Arc<Session>) -> anyhow::Result<bool> {
        let _ = session.press_keysym(0xffe1).await; // wake frame clock
        tokio::time::sleep(Duration::from_millis(250)).await;
        Ok(session
            .locate("//Button[@name='main-menu' and @expanded='true']")
            .count()
            .await?
            > 0)
    }
    // App liveness: a fresh AT-SPI snapshot succeeds and still contains the
    // header-bar menu button (present in every section). Proves the app's
    // main loop + a11y bridge are responsive, independent of seat input and
    // of which section is currently shown.
    async fn app_alive(session: &Arc<Session>) -> bool {
        session
            .locate("//Button[@name='main-menu']")
            .count()
            .await
            .map(|n| n > 0)
            .unwrap_or(false)
    }
    // Which section is showing (gtk4 has agree-check; a section switch via a
    // menu item activation removes it).
    async fn gtk4_section_present(session: &Arc<Session>) -> anyhow::Result<bool> {
        Ok(session
            .locate("//Checkbox[@name='agree-check']")
            .count()
            .await?
            > 0)
    }

    eprintln!(
        "GRAB step0: baseline alive={} gtk4_section={}",
        app_alive(&session).await,
        gtk4_section_present(&session).await?
    );

    session
        .locate("//ToggleButton[@name='main-menu']")
        .click()
        .await?;
    eprintln!(
        "GRAB step1: after open — open={} alive={} menu_items={}",
        popover_open(&session).await?,
        app_alive(&session).await,
        session.locate("//MenuItem").count().await?
    );

    // Step 2: Escape.
    session.press_chord("Escape").await?;
    eprintln!(
        "GRAB step2: after Escape — open={} alive={} gtk4_section={}",
        popover_open(&session).await?,
        app_alive(&session).await,
        gtk4_section_present(&session).await?
    );

    // Step 3: reopen if needed, then keyboard navigation Down+Return.
    if !popover_open(&session).await? {
        session
            .locate("//ToggleButton[@name='main-menu']")
            .click()
            .await?;
        eprintln!("GRAB step3a: reopened = {}", popover_open(&session).await?);
    }
    session.press_chord("Down").await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    session.press_chord("Return").await?;
    eprintln!(
        "GRAB step3: after Down+Return — open={} alive={} gtk4_section={}",
        popover_open(&session).await?,
        app_alive(&session).await,
        gtk4_section_present(&session).await?
    );

    // Step 4: reopen if needed, then outside pointer click.
    if !popover_open(&session).await? {
        session
            .locate("//ToggleButton[@name='main-menu']")
            .click()
            .await?;
        eprintln!("GRAB step4a: reopened = {}", popover_open(&session).await?);
    }
    session.pointer_motion_absolute(500.0, 600.0).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    session
        .pointer_button(waydriver::PointerButton::Left)
        .await?;
    eprintln!(
        "GRAB step4: after outside click — open={} alive={}",
        popover_open(&session).await?,
        app_alive(&session).await
    );

    // Step 5: AT-SPI re-click of the toggle (D-Bus escape hatch).
    if popover_open(&session).await? {
        session
            .locate("//ToggleButton[@name='main-menu']")
            .click()
            .await?;
        eprintln!(
            "GRAB step5: after AT-SPI toggle re-click — open={} alive={}",
            popover_open(&session).await?,
            app_alive(&session).await
        );
    }

    // Step 6: final keyboard sanity — fill the entry (gtk4 section only).
    if gtk4_section_present(&session).await? {
        let entry = session.locate("//TextBox[@name='text-entry']");
        let typed = match entry.fill("grab-check").await {
            Ok(()) => entry.text().await.unwrap_or_default(),
            Err(e) => format!("<fill failed: {e}>"),
        };
        eprintln!(
            "GRAB step6: keyboard works after experiments = {} (entry {typed:?})",
            typed == "grab-check"
        );
    } else {
        eprintln!("GRAB step6: skipped — section switched away from gtk4 during probe");
    }

    kill(session).await?;
    Ok(())
}

/// Control for the Bug 7 probe: do pointer events land in the gtk4 section
/// at all? Motion over hover-target must emit pointer-enter (pure motion
/// delivery, no click semantics), and a right press on ctx-target must emit
/// its context event. Diagnostic — read the CTRL lines on stderr.
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn pointer_delivery_control_gtk4() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let hb = session
        .locate("//*[@name='hover-target']")
        .first()
        .bounds()
        .await?;
    let cursor = session.stdout_cursor();
    session
        .pointer_motion_absolute((hb.x - 30).max(0) as f64, hb.y as f64)
        .await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    session
        .pointer_motion_absolute((hb.x + hb.width / 2) as f64, (hb.y + hb.height / 2) as f64)
        .await?;
    let enter = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("pointer-enter hover-target"),
            Duration::from_secs(3),
        )
        .await;
    eprintln!("CTRL: motion delivered (hover enter) = {}", enter.is_ok());

    let cb = session
        .locate("//*[@name='ctx-target']")
        .first()
        .bounds()
        .await?;
    let cursor = session.stdout_cursor();
    session
        .pointer_motion_absolute((cb.x + cb.width / 2) as f64, (cb.y + cb.height / 2) as f64)
        .await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    session
        .pointer_button_down(waydriver::PointerButton::Right)
        .await?;
    tokio::time::sleep(Duration::from_millis(120)).await;
    session
        .pointer_button_up(waydriver::PointerButton::Right)
        .await?;
    let ctx = session
        .wait_for_stdout_line(cursor, |l| l.contains("ctx-target"), Duration::from_secs(3))
        .await;
    eprintln!(
        "CTRL: right-click delivered (ctx event) = {} ({:?})",
        ctx.is_ok(),
        ctx.as_deref().unwrap_or("<nothing>")
    );

    kill(session).await?;
    Ok(())
}

/// Bug 7 repro infrastructure (middle-click vs AdwTabBar) + an unresolved
/// confound. The fixture now has an `AdwTabView`/`AdwTabBar` with three pages
/// (middle-click-close observable via the `tab-count` event) and a plain
/// `mid-target` `GestureClick` that reports which button number GTK received.
///
/// **RESOLVED — not a middle-button bug.** `coord_source_confound_probe` and
/// `screen_vs_window_extents_probe` isolated the real cause: `Locator::bounds()`
/// returns *window-relative* coordinates (atspi.rs reads `CoordType::Window`),
/// but `pointer_motion_absolute` consumes *screen-absolute* coordinates. The
/// gap is the toplevel's on-screen origin — and mutter centers the single
/// toplevel, so origin = ((screen − window-content)/2) (measured (151, 63) ≈
/// derived (152, 64) for a 720×640 window on 1024×768). A click at `bounds()`
/// therefore misses by that offset for *every* button; the middle button was
/// never the variable. A click at `bounds()` + origin lands. AT-SPI Screen
/// extents are uniformly (0,0) and the toplevel has no Component interface, so
/// the origin must be derived (centering) — it isn't available raw.
/// Diagnostic — read the MID lines on stderr.
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn middle_click_probe_adw_tab_bar() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;

    async fn click_at(
        session: &Arc<Session>,
        x: f64,
        y: f64,
        button: waydriver::PointerButton,
    ) -> anyhow::Result<()> {
        // Two-step approach motion + settles: same recipe as the visual
        // locator's cold-start click, so pointer focus is bound before the
        // button event.
        session.pointer_motion_absolute(x - 25.0, y).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        session.pointer_motion_absolute(x, y).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Separate press/settle/release — the same shape as the visual
        // locator's cold_start_click; an atomic press+release pair can be
        // coalesced/dropped by GTK's gesture machinery in headless mutter.
        session.pointer_button_down(button).await?;
        tokio::time::sleep(Duration::from_millis(120)).await;
        session.pointer_button_up(button).await?;
        Ok(())
    }

    // ── M0: known-good pointer baseline — left-click the activatable
    // ActionRow and confirm its activation event. Proves pointer clicks
    // land in this section before measuring the middle button.
    let row = session
        .locate("//ListItem[@name='adw-action-row']")
        .first()
        .bounds()
        .await?;
    let cursor = session.stdout_cursor();
    click_at(
        &session,
        (row.x + row.width / 2) as f64,
        (row.y + row.height / 2) as f64,
        waydriver::PointerButton::Left,
    )
    .await?;
    let m0 = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("activated adw-action-row"),
            Duration::from_secs(3),
        )
        .await;
    eprintln!(
        "MID M0: baseline left-click on action row works = {}",
        m0.is_ok()
    );

    // ── M1: which button numbers does GTK receive on a plain target? ──
    let b = session
        .locate("//Label[@name='mid-target']")
        .bounds()
        .await?;
    eprintln!("MID M1: mid-target bounds = {b:?}");
    let (cx, cy) = ((b.x + b.width / 2) as f64, (b.y + b.height / 2) as f64);
    for (button, expect) in [
        (waydriver::PointerButton::Left, "button=1"),
        (waydriver::PointerButton::Middle, "button=2"),
        (waydriver::PointerButton::Right, "button=3"),
    ] {
        let cursor = session.stdout_cursor();
        click_at(&session, cx, cy, button).await?;
        let got = session
            .wait_for_stdout_line(
                cursor,
                |l| l.contains("pressed mid-target"),
                Duration::from_secs(3),
            )
            .await;
        eprintln!(
            "MID M1: {button:?} delivered = {} (expected {expect}, got {:?})",
            got.as_deref().map(|l| l.contains(expect)).unwrap_or(false),
            got.as_deref().unwrap_or("<nothing>")
        );
    }

    // ── M2: middle-click an AdwTabBar tab — does the page close? ──
    // Both the tab and its page-content Label are named tab-two; the tab
    // widget is the one inside the tab bar (smallest y).
    let candidates = session.locate("//*[@name='tab-two']").inspect_all().await?;
    eprintln!(
        "MID M2: tab-two candidates = {:?}",
        candidates
            .iter()
            .map(|c| (c.role.clone(), c.bounds))
            .collect::<Vec<_>>()
    );
    let tab = candidates
        .iter()
        .filter(|c| c.role == "Tab")
        .filter_map(|c| c.bounds.map(|b| (c.role.clone(), b)))
        .next()
        .ok_or_else(|| anyhow::anyhow!("no role=Tab tab-two with bounds in the tree"))?;
    let (tx, ty) = (
        (tab.1.x + tab.1.width / 2) as f64,
        (tab.1.y + tab.1.height / 2) as f64,
    );
    let cursor = session.stdout_cursor();
    click_at(&session, tx, ty, waydriver::PointerButton::Middle).await?;
    let closed = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("tab-count 2"),
            Duration::from_secs(3),
        )
        .await;
    eprintln!(
        "MID M2: middle-click on tab (role {} at {:?}) closed it = {} ({:?})",
        tab.0,
        tab.1,
        closed.is_ok(),
        closed.as_deref().unwrap_or("<no tab-count event>")
    );

    kill(session).await?;
    Ok(())
}

/// **Decisive Bug 7 probe.** Resolves the coordinate-source confound:
/// `Locator::bounds()` returns *window-relative* extents (atspi.rs reads
/// `CoordType::Window` because headless mutter reports `Screen` as `(0,0)`),
/// but `pointer_motion_absolute` consumes *absolute screen* coordinates. If
/// the toplevel isn't pinned at the screen origin, a click placed at
/// `bounds()` lands offset by the window's on-screen position — which is
/// exactly the path the reporter took for AdwTabBar middle-click (no AT-SPI
/// action exists for middle-click-close, so raw pointer events at `bounds()`
/// were the only option).
///
/// Measures the *same* widget (`mid-target`, which has a visible text label
/// AND reports the button number GTK received) two ways on the *same*
/// main-window surface:
///   - A1: AT-SPI window-relative `bounds()` center.
///   - A2: OCR screen-pixel `find_by_text("mid-target").bounds()` center.
/// Then left-clicks at each and reports which fired. The center delta
/// (A2 − A1) should equal the window's on-screen origin; if A2 fires and A1
/// misses by that delta, the confound is coordinate space, not the button.
/// Diagnostic — read the COORD lines on stderr (OCR pass is slow in debug).
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn coord_source_confound_probe() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;

    async fn left_click_at(session: &Arc<Session>, x: f64, y: f64) -> anyhow::Result<()> {
        session.pointer_motion_absolute(x - 25.0, y).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        session.pointer_motion_absolute(x, y).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        session
            .pointer_button_down(waydriver::PointerButton::Left)
            .await?;
        tokio::time::sleep(Duration::from_millis(120)).await;
        session
            .pointer_button_up(waydriver::PointerButton::Left)
            .await?;
        Ok(())
    }

    // ── A1: AT-SPI window-relative bounds ──────────────────────────────
    let atspi = session
        .locate("//Label[@name='mid-target']")
        .bounds()
        .await?;
    eprintln!("COORD: AT-SPI (window-relative) bounds = {atspi:?}");

    // ── A2: OCR screen-pixel bounds for the same label ─────────────────
    let ocr = session.find_by_text("mid-target").bounds().await?;
    eprintln!("COORD: OCR (screen-pixel) bounds      = {ocr:?}");

    let (dx, dy) = (
        ocr.center_x() - atspi.center_x(),
        ocr.center_y() - atspi.center_y(),
    );
    eprintln!("COORD: center delta (OCR - AT-SPI) = ({dx}, {dy})  <- expected window origin");

    // ── Click at the AT-SPI center (the natural-but-wrong path) ────────
    let cursor = session.stdout_cursor();
    left_click_at(&session, atspi.center_x() as f64, atspi.center_y() as f64).await?;
    let at_atspi = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("pressed mid-target"),
            Duration::from_secs(3),
        )
        .await;
    eprintln!(
        "COORD: click at AT-SPI center fired = {} ({:?})",
        at_atspi.is_ok(),
        at_atspi.as_deref().unwrap_or("<nothing>")
    );

    // ── Click at the OCR center (screen-absolute) ──────────────────────
    let cursor = session.stdout_cursor();
    left_click_at(&session, ocr.center_x() as f64, ocr.center_y() as f64).await?;
    let at_ocr = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("pressed mid-target"),
            Duration::from_secs(3),
        )
        .await;
    eprintln!(
        "COORD: click at OCR center fired = {} ({:?})",
        at_ocr.is_ok(),
        at_ocr.as_deref().unwrap_or("<nothing>")
    );

    // ── Click at AT-SPI center + delta (proves the offset correction) ──
    let cursor = session.stdout_cursor();
    left_click_at(
        &session,
        (atspi.center_x() + dx) as f64,
        (atspi.center_y() + dy) as f64,
    )
    .await?;
    let at_corrected = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("pressed mid-target"),
            Duration::from_secs(3),
        )
        .await;
    eprintln!(
        "COORD: click at AT-SPI center + delta fired = {} ({:?})",
        at_corrected.is_ok(),
        at_corrected.as_deref().unwrap_or("<nothing>")
    );

    kill(session).await?;
    Ok(())
}

/// **Bug 7 regression.** Pointer-based `Locator` actions now translate
/// window-relative AT-SPI bounds into screen coordinates
/// (`Session::to_screen_bounds`), so they land on the widget even though
/// mutter centers the toplevel off the screen origin. Before the fix every
/// one of these silently missed. Covers the new `middle_click`/`pointer_click`
/// plus `hover`/`right_click` (which share the now-translated `wait_and_center`).
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn pointer_actions_land_via_screen_translation() -> anyhow::Result<()> {
    init_tracing();

    // ── adw: middle + left pointer_click deliver the right button ──────
    {
        let (session, _state) = start_fixture_session("adw").await?;

        // Scope the Locator so its session Arc clone drops before `kill`
        // takes ownership of the unique reference.
        {
            let mid = session.locate("//Label[@name='mid-target']");

            let cursor = session.stdout_cursor();
            mid.middle_click().await?;
            let l = session
                .wait_for_stdout_line(
                    cursor,
                    |l| l.contains("pressed mid-target"),
                    Duration::from_secs(5),
                )
                .await?;
            assert!(
                l.contains("button=2"),
                "middle_click should deliver pointer button 2, got: {l}"
            );

            let cursor = session.stdout_cursor();
            mid.pointer_click(waydriver::PointerButton::Left).await?;
            let l = session
                .wait_for_stdout_line(
                    cursor,
                    |l| l.contains("pressed mid-target"),
                    Duration::from_secs(5),
                )
                .await?;
            assert!(
                l.contains("button=1"),
                "pointer_click(Left) should deliver pointer button 1, got: {l}"
            );
        }

        kill(session).await?;
    }

    // ── gtk4: pointer_click lands by coordinate (geometry proof) ───────
    // primary-button has an AT-SPI action, but pointer_click deliberately
    // bypasses it and clicks by translated screen coordinate, so a
    // "clicked primary-button" event proves the gtk4 window origin resolved
    // correctly (a different window size than adw).
    {
        let (session, _state) = start_fixture_session("gtk4").await?;
        eprintln!("gtk4 window_origin = {:?}", session.window_origin().await?);

        let cursor = session.stdout_cursor();
        session
            .locate("//Button[@name='primary-button']")
            .pointer_click(waydriver::PointerButton::Left)
            .await?;
        session
            .wait_for_stdout_line(
                cursor,
                |l| l.contains("clicked primary-button"),
                Duration::from_secs(5),
            )
            .await?;

        // hover + right_click share the same translated path.
        let cursor = session.stdout_cursor();
        session.locate("//*[@name='hover-target']").hover().await?;
        session
            .wait_for_stdout_line(
                cursor,
                |l| l.contains("pointer-enter hover-target"),
                Duration::from_secs(5),
            )
            .await?;

        let cursor = session.stdout_cursor();
        session
            .locate("//*[@name='ctx-target']")
            .right_click()
            .await?;
        session
            .wait_for_stdout_line(cursor, |l| l.contains("ctx-target"), Duration::from_secs(5))
            .await?;

        kill(session).await?;
    }

    Ok(())
}

/// Fix-strategy probe for Bug 7: is there a usable window-origin source?
/// `coord_source_confound_probe` proved `bounds()` is window-relative and the
/// pointer API wants screen-absolute, with the gap = the window's on-screen
/// origin. To self-correct, waydriver needs that origin. The `atspi.rs`
/// comment claims headless mutter reports `CoordType::Screen` as `(0,0)`.
/// This dumps Screen *and* Window extents for the toplevel frame and a leaf
/// (`mid-target`) so we can see whether Screen is uniformly `(0,0)` (→ origin
/// must come from the compositor) or whether the toplevel/leaf Screen extents
/// actually carry the offset (→ trivial translate). Read the GEOM lines.
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn screen_vs_window_extents_probe() -> anyhow::Result<()> {
    use atspi::proxy::component::ComponentProxy;
    use atspi::CoordType;

    init_tracing();
    let (session, _state) = start_fixture_session("adw").await?;

    let conn = session
        .a11y_connection
        .as_ref()
        .expect("session has a11y connection")
        .clone();

    async fn dump(
        conn: &zbus::Connection,
        label: &str,
        bus: &str,
        path: &str,
    ) -> anyhow::Result<()> {
        let comp = ComponentProxy::builder(conn)
            .destination(bus.to_string())?
            .path(path.to_string())?
            .build()
            .await?;
        let screen = comp.get_extents(CoordType::Screen).await.ok();
        let window = comp.get_extents(CoordType::Window).await.ok();
        eprintln!("GEOM {label}: screen={screen:?} window={window:?}");
        Ok(())
    }

    // Toplevel frame: the outermost node under the app root.
    let top = session.locate("//*").inspect_all().await?;
    if let Some(frame) = top.first() {
        dump(&conn, "toplevel", &frame.ref_.0, &frame.ref_.1).await?;
    }
    // Any node that reports a role of window/frame/dialog.
    for e in &top {
        if matches!(e.role.as_str(), "frame" | "window" | "dialog") {
            dump(&conn, &format!("role={}", e.role), &e.ref_.0, &e.ref_.1).await?;
        }
    }

    // The leaf we clicked in the confound probe.
    let mid = session
        .locate("//Label[@name='mid-target']")
        .inspect_all()
        .await?;
    if let Some(m) = mid.first() {
        dump(&conn, "mid-target", &m.ref_.0, &m.ref_.1).await?;
    }

    // Top-5 largest window-relative bboxes — to see whether an overflowing
    // scroll child outranks the actual window content box.
    let mut areas: Vec<(i64, waydriver::Rect)> = top
        .iter()
        .filter_map(|e| e.bounds.map(|b| (b.width as i64 * b.height as i64, b)))
        .collect();
    areas.sort_by_key(|(a, _)| std::cmp::Reverse(*a));
    let (sw, sh) = (1024i32, 768i32);
    for (i, (_, b)) in areas.iter().take(5).enumerate() {
        eprintln!(
            "GEOM bbox#{i} = {b:?}  => centered origin = ({}, {})",
            (sw - b.width) / 2,
            (sh - b.height) / 2
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

/// `Locator::compare_to_baseline` against a captured reference: the same
/// static frame matches itself; the frame after a real toggle does not.
/// Proves the perceptual diff reads actual compositor pixels and is a
/// data primitive (it returns a score, it does not assert).
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_compare_to_baseline_detects_change() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let sel = "//ToggleButton[@name='mode-toggle']";

    // Capture a reference crop of the toggle button in its current state.
    // `Locator::screenshot` re-encodes a clean PNG, so the bytes can be
    // fed straight back to `compare_to_baseline` (the consumer would read
    // these from its committed reference file instead).
    let reference = session.locate(sel).screenshot().await?;
    assert!(reference.len() > 100, "reference crop too small");

    // The same static frame matches itself (allow a little antialias jitter).
    let same = session
        .locate(sel)
        .compare_to_baseline(&reference, 0.02)
        .await?;

    // Toggle it — a real repaint of the checked state.
    let cursor = session.stdout_cursor();
    session.locate(sel).click().await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("toggled mode-toggle"),
            Duration::from_secs(3),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The repainted button no longer matches the original reference. Use a
    // zero tolerance: any perceptibly-different pixel is a change.
    let changed = session
        .locate(sel)
        .compare_to_baseline(&reference, 0.0)
        .await?;

    kill(session).await?;

    assert!(
        same.matched,
        "static frame should match its own reference: score={}, meanΔE={}, maxΔE={}",
        same.score, same.mean_delta_e, same.max_delta_e
    );
    assert!(
        same.ncc > 0.9,
        "structural NCC of a static frame vs itself should be ~1.0, got {}",
        same.ncc
    );
    assert!(
        !changed.matched && changed.score > 0.0,
        "toggled button should differ from its reference (score={}, diff_pixels={})",
        changed.score,
        changed.diff_pixels
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

/// `Locator::drag_to_coords` drives the same DnD machinery as `drag_to` but
/// releases at raw screen-absolute coordinates rather than onto a resolved
/// element. Here we feed it the drop-target's own centre (obtained via
/// `screen_bounds`) so the drop still lands — proving the coordinate path
/// reaches GTK4's DnD recognizer end-to-end. The off-window drop case
/// (libadwaita tab drag-out) shares this exact code path; only the endpoint
/// coordinates differ.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_locator_drag_to_coords_drops_payload() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("dnd").await?;

    let cursor = session.stdout_cursor();
    let source = session.locate("//*[@name='drag-source']");
    let target = session.locate("//*[@name='drop-target']");

    // Resolve the target's screen rectangle and aim the drop at its centre,
    // exercising the coordinate API rather than handing it the Locator.
    let bounds = target.screen_bounds().await?;
    source
        .drag_to_coords(bounds.center_x() as f64, bounds.center_y() as f64)
        .await?;

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

/// (#29 b — drive) `Locator::scroll` moves a scroll area's offset. Wheeling
/// down over the `scroll-area` ScrolledWindow must push its vertical adjustment
/// past 0 — the fixture emits `scrolled scroll-area value=<n>` as ground truth
/// — and over-scrolling back up must clamp it to 0.0 (parked at the top).
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn scroll_drives_scroll_area_offset() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Scroll down a few detents; the fixture reports the resulting offset.
    let cursor = session.stdout_cursor();
    session
        .locate("//*[@name='scroll-area']")
        .scroll(waydriver::PointerAxis::Vertical, 5)
        .await?;
    let line = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("scrolled scroll-area value="),
            Duration::from_secs(5),
        )
        .await?;
    let moved: f64 = line
        .rsplit("value=")
        .next()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("couldn't parse scroll offset from {line:?}"))?;
    assert!(
        moved > 0.0,
        "scrolling down should push the offset past 0, got {moved}"
    );

    // Over-scroll back up well past the top; the offset clamps and parks at 0.0.
    let cursor = session.stdout_cursor();
    session
        .locate("//*[@name='scroll-area']")
        .scroll(waydriver::PointerAxis::Vertical, -20)
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("scrolled scroll-area value=0.0"),
            Duration::from_secs(5),
        )
        .await?;

    kill(session).await?;
    Ok(())
}

/// (#29 b — readback) `Locator::value` reads an element's AT-SPI `Value`
/// interface, the half of the scroll/value capability that AT-SPI otherwise
/// hides. The fixture's `value-slider` is a Scale fixed to 0..100 at an initial
/// 25, so the snapshot must report exactly that range and position.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn value_reads_slider_range() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let v = session.locate("//*[@name='value-slider']").value().await?;
    assert!(
        (v.current - 25.0).abs() < 0.01,
        "slider current should read its initial 25, got {}",
        v.current
    );
    assert!(
        (v.minimum - 0.0).abs() < 0.01,
        "slider minimum should be 0, got {}",
        v.minimum
    );
    assert!(
        (v.maximum - 100.0).abs() < 0.01,
        "slider maximum should be 100, got {}",
        v.maximum
    );

    kill(session).await?;
    Ok(())
}

/// External-effect capture: clicking the fixture's `fire-notification` button
/// sends an `org.freedesktop.Notifications.Notify` onto the session bus, which
/// waydriver's mock sink records and exposes via `Session::notifications`.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_captures_notification() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session_opts("effects", true).await?;
    assert!(
        session.external_effects_enabled(),
        "capture should be active for this session"
    );

    let cursor = session.stdout_cursor();
    session
        .locate("//Button[@name='fire-notification']")
        .click()
        .await?;
    // The fixture sends the notification synchronously, then prints this line.
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("notification-sent fire-notification"),
            Duration::from_secs(5),
        )
        .await?;

    let notes = session.notifications()?;
    assert_eq!(
        notes.len(),
        1,
        "exactly one notification captured: {notes:?}"
    );
    assert_eq!(notes[0].summary, "fixture-notification");
    assert_eq!(notes[0].body, "fixture body text");
    assert_eq!(notes[0].app_name, "waydriver-fixture");

    // The wait_for helper sees the same record.
    let waited = session
        .wait_for_notification(
            0,
            |n| n.summary == "fixture-notification",
            Duration::from_secs(1),
        )
        .await?;
    assert_eq!(waited.id, notes[0].id);

    kill(session).await?;
    Ok(())
}

/// External-effect capture: clicking the fixture's `open-uri` button issues a
/// portal `OpenURI` request, recorded by the mock portal sink and exposed via
/// `Session::open_uri_requests`.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_captures_open_uri() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session_opts("effects", true).await?;

    let cursor = session.stdout_cursor();
    session.locate("//Button[@name='open-uri']").click().await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("open-uri-requested open-uri"),
            Duration::from_secs(5),
        )
        .await?;

    let opened = session.open_uri_requests()?;
    assert_eq!(opened.len(), 1, "exactly one open-uri captured: {opened:?}");
    assert_eq!(opened[0].uri, "https://example.com/waydriver");

    kill(session).await?;
    Ok(())
}

/// External-effect capture is opt-in: a session started without it reports
/// disabled and the readback methods error rather than silently returning empty.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_external_capture_off_by_default() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("effects").await?;
    assert!(!session.external_effects_enabled());
    assert!(session.notifications().is_err());
    assert!(session.open_uri_requests().is_err());
    kill(session).await?;
    Ok(())
}

/// Single-instance CLI forwarding: launching a second instance of the fixture
/// with a positional arg forwards it to the running primary, whose
/// `command-line` handler prints it.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_forwards_secondary_command_line() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let cursor = session.stdout_cursor();
    let outcome = session
        .launch_secondary(vec!["forwarded-token-xyz".to_string()])
        .await?;
    eprintln!(
        "secondary exit={:?} stdout={:?}",
        outcome.exit_code, outcome.stdout
    );

    // The *primary* prints what it received from the forwarded command line.
    let line = session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("command-line-forwarded") && l.contains("forwarded-token-xyz"),
            Duration::from_secs(5),
        )
        .await?;
    eprintln!("observed forwarded command line: {line}");

    kill(session).await?;
    Ok(())
}

/// Shared event tallies for [`atspi_event_cache_measurement`]'s background
/// collector. Counters are atomic so the collector task and the driving test
/// touch them lock-free; `paths` records the distinct object paths an event
/// referenced since the last clear, and `dirty` mirrors the single bit a
/// global-dirty cache would keep.
#[derive(Default)]
struct EventTally {
    total: std::sync::atomic::AtomicU64,
    children_insert: std::sync::atomic::AtomicU64,
    children_delete: std::sync::atomic::AtomicU64,
    state_changed: std::sync::atomic::AtomicU64,
    property_change: std::sync::atomic::AtomicU64,
    text_changed: std::sync::atomic::AtomicU64,
    other: std::sync::atomic::AtomicU64,
    dirty: std::sync::atomic::AtomicBool,
    paths: std::sync::Mutex<std::collections::HashSet<String>>,
}

/// Issue #11 measurement: characterize AT-SPI mutation-event behavior so the
/// event-driven-cache design's open questions can be answered with numbers from
/// the real GTK bridge (not the synthetic mock the walk bench uses). A
/// background task subscribes to the mutation events a cache would mirror
/// (children-changed, state-changed, property-change, text-changed); the test
/// then:
///
///   * sits idle and counts events             -> Q3 "overhead when nothing changes"
///   * drives known mutations, each marked by   -> Q1 "are events reliable?"
///     a `fixture-event:` stdout line as the        Q2 "consistency window vs a walk"
///     app-side ground truth
///   * runs an action->auto-wait cadence with a -> the money metric: how often a
///     simulated dirty-flag cache                   global-dirty cache would serve a
///                                                  warm snapshot vs. re-walk
///
/// Diagnostic only (no behavioural asserts); read the RESULT block on stderr.
#[tokio::test]
#[ignore = "diagnostic probe; run manually with --ignored --nocapture"]
async fn atspi_event_cache_measurement() -> anyhow::Result<()> {
    use std::collections::HashSet;
    use std::sync::atomic::Ordering::SeqCst;
    use std::time::Instant;

    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // --- Background event collector on its own a11y connection ---------------
    let tally = Arc::new(EventTally::default());
    let collector = spawn_event_collector(tally.clone());

    // Let registration settle and drain any startup churn.
    tokio::time::sleep(Duration::from_millis(800)).await;

    eprintln!("=== RESULT: AT-SPI event-cache measurement (issue #11) ===");

    // === Q3: idle event volume =============================================
    let idle_before = tally.total.load(SeqCst);
    let idle_secs = 3u64;
    tokio::time::sleep(Duration::from_secs(idle_secs)).await;
    let idle_events = tally.total.load(SeqCst) - idle_before;
    eprintln!(
        "Q3 idle  : {idle_events} events over {idle_secs}s while the UI was static \
         ({:.1}/s) — a global-dirty cache stays warm between actions only if this is ~0",
        idle_events as f64 / idle_secs as f64
    );

    // === Q1 + Q2: per-mutation reliability + consistency window ============
    // (click selector, stdout-marker substring = app-side ground truth, label).
    let mutations: &[(&str, &str, &str)] = &[
        (
            "//Checkbox[@name='agree-check']",
            "checked agree-check",
            "state-changed (check toggle)",
        ),
        (
            "//Button[@name='open-dialog']",
            "dialog-opened sample-dialog",
            "children-changed insert (dialog open)",
        ),
        (
            "//Button[@name='dialog-close']",
            "dialog-closed sample-dialog",
            "children-changed delete (dialog close)",
        ),
    ];

    for (selector, marker, label) in mutations {
        // Baseline tree; clear the per-mutation event window.
        let baseline: HashSet<String> = match session.locate("//*").inspect_all().await {
            Ok(v) => v.into_iter().map(|e| e.ref_.1).collect(),
            Err(_) => HashSet::new(),
        };
        if let Ok(mut g) = tally.paths.lock() {
            g.clear();
        }
        let total_before = tally.total.load(SeqCst);
        tally.dirty.store(false, SeqCst);

        let cursor = session.stdout_cursor();
        let t_drive = Instant::now();
        if let Err(e) = session.locate(selector).click().await {
            eprintln!("  [{label}] drive failed ({e}); skipping");
            continue;
        }
        // App-side ground truth: the fixture flushed its event line.
        let app_ok = session
            .wait_for_stdout_line(cursor, |l| l.contains(marker), Duration::from_secs(3))
            .await
            .is_ok();
        let d_app = t_drive.elapsed();

        // Bounded wait for the first AT-SPI event after the drive.
        let mut d_event: Option<Duration> = None;
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            if tally.total.load(SeqCst) > total_before {
                d_event = Some(t_drive.elapsed());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Settle so late events (subtree realization) are counted.
        tokio::time::sleep(Duration::from_millis(400)).await;

        let events_seen = tally.total.load(SeqCst) - total_before;
        let after: HashSet<String> = match session.locate("//*").inspect_all().await {
            Ok(v) => v.into_iter().map(|e| e.ref_.1).collect(),
            Err(_) => baseline.clone(),
        };
        let added = after.difference(&baseline).count();
        let removed = baseline.difference(&after).count();
        let event_paths = tally.paths.lock().map(|g| g.len()).unwrap_or(0);

        let consistency = match d_event {
            Some(de) => format!(
                "event@{:.0}ms app@{:.0}ms window={:+.0}ms",
                de.as_secs_f64() * 1e3,
                d_app.as_secs_f64() * 1e3,
                (de.as_secs_f64() - d_app.as_secs_f64()) * 1e3
            ),
            None => "NO EVENT within 1.5s".to_string(),
        };
        eprintln!(
            "  [{label}]\n      reliability: {events_seen} events, {event_paths} distinct paths; \
             walk delta +{added}/-{removed} nodes; app_marker={app_ok}\n      consistency: {consistency}"
        );
    }

    // === Money metric: warm-cache hit rate over an action->auto-wait cadence
    // Model a global-dirty cache: a snapshot is a HIT when no mutation event has
    // arrived since the last reconcile, else a MISS (re-walk, which clears
    // dirty). A real Locator re-snapshots many times per auto-wait; the win is
    // the polls after the first where the tree is unchanged. Each round performs
    // a real mutation (so the cache MUST reconcile), then polls on a 50ms cadence
    // like `poll_with_retry`, counting hits vs misses against the live flag.
    let rounds = 6u32;
    let polls_per_wait = 8u32;
    let poll_gap = Duration::from_millis(50);
    let mut hits = 0u64;
    let mut misses = 0u64;
    for _ in 0..rounds {
        let _ = session
            .locate("//Checkbox[@name='agree-check']")
            .click()
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await; // let its event land
        for _ in 0..polls_per_wait {
            if tally.dirty.swap(false, SeqCst) {
                misses += 1; // dirty -> reconcile re-walk
            } else {
                hits += 1; // clean -> serve warm snapshot
            }
            tokio::time::sleep(poll_gap).await;
        }
    }
    let total_snaps = hits + misses;
    let hit_pct = if total_snaps > 0 {
        hits as f64 * 100.0 / total_snaps as f64
    } else {
        0.0
    };
    eprintln!(
        "money    : over {rounds} action->wait rounds, {total_snaps} snapshots: \
         {hits} warm-cache hits / {misses} reconciles ({hit_pct:.0}% served from cache) — \
         each hit avoids a full tree walk"
    );
    eprintln!(
        "classes  : children +{}/-{} state {} property {} text {} other {} (total {})",
        tally.children_insert.load(SeqCst),
        tally.children_delete.load(SeqCst),
        tally.state_changed.load(SeqCst),
        tally.property_change.load(SeqCst),
        tally.text_changed.load(SeqCst),
        tally.other.load(SeqCst),
        tally.total.load(SeqCst),
    );
    eprintln!("=== end RESULT ===");

    collector.abort();
    kill(session).await?;
    Ok(())
}

/// Spawn a background task that subscribes to the AT-SPI mutation events an
/// incremental cache would mirror and accumulates them into `tally`. Shared by
/// the fixture and real-app cache measurements. The task owns its own a11y
/// connection; abort the returned handle to stop it.
fn spawn_event_collector(tally: Arc<EventTally>) -> tokio::task::JoinHandle<()> {
    use atspi::events::object::{
        ChildrenChangedEvent, PropertyChangeEvent, StateChangedEvent, TextChangedEvent,
    };
    use atspi::{AccessibilityConnection, Event, ObjectEvents, Operation};
    use futures_lite::StreamExt;
    use std::sync::atomic::Ordering::SeqCst;

    tokio::spawn(async move {
        let t = tally;
        let a11y = match AccessibilityConnection::new().await {
            Ok(a) => a,
            Err(e) => {
                eprintln!("collector: a11y connect failed: {e}");
                return;
            }
        };
        let _ = a11y.register_event::<ChildrenChangedEvent>().await;
        let _ = a11y.register_event::<StateChangedEvent>().await;
        let _ = a11y.register_event::<PropertyChangeEvent>().await;
        let _ = a11y.register_event::<TextChangedEvent>().await;
        let mut stream = std::pin::pin!(a11y.event_stream());
        while let Some(ev) = stream.next().await {
            let Ok(Event::Object(obj)) = ev else { continue };
            let path = match &obj {
                ObjectEvents::ChildrenChanged(e) => {
                    if e.operation == Operation::Insert {
                        t.children_insert.fetch_add(1, SeqCst);
                    } else {
                        t.children_delete.fetch_add(1, SeqCst);
                    }
                    Some(e.item.path_as_str().to_string())
                }
                ObjectEvents::StateChanged(e) => {
                    t.state_changed.fetch_add(1, SeqCst);
                    Some(e.item.path_as_str().to_string())
                }
                ObjectEvents::PropertyChange(e) => {
                    t.property_change.fetch_add(1, SeqCst);
                    Some(e.item.path_as_str().to_string())
                }
                ObjectEvents::TextChanged(e) => {
                    t.text_changed.fetch_add(1, SeqCst);
                    Some(e.item.path_as_str().to_string())
                }
                _ => {
                    t.other.fetch_add(1, SeqCst);
                    None
                }
            };
            t.total.fetch_add(1, SeqCst);
            t.dirty.store(true, SeqCst);
            if let (Some(p), Ok(mut g)) = (path, t.paths.lock()) {
                g.insert(p);
            }
        }
    })
}

/// Resolve a binary on `PATH`, returning its absolute path, or `None` if it is
/// not installed (so the real-app measurement can skip it gracefully).
fn which(bin: &str) -> Option<String> {
    // Scan PATH directly rather than shelling out to `which` — Fedora's minimal
    // images (including this dev-container) no longer ship the `which` binary.
    if bin.contains('/') {
        return std::path::Path::new(bin).is_file().then(|| bin.to_string());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Launch an arbitrary GTK app (not just the fixture) under a fresh headless
/// mutter session and return the ready [`Session`]. Mirrors
/// [`start_fixture_session`] but takes the app's command / args / a11y name, so
/// the cache measurement can drive real reference apps. AT-SPI-only: no OCR
/// prewarm and no external-effect capture.
async fn start_app_session(
    command: String,
    args: Vec<String>,
    app_name: String,
) -> anyhow::Result<(Arc<Session>, Arc<MutterState>)> {
    let mut compositor = MutterCompositor::new();
    compositor.start(None, None).await?;
    let state = compositor
        .state()
        .expect("MutterCompositor::state must be Some immediately after start() succeeded");
    let input = MutterInput::new(state.clone());
    let capture = MutterCapture::new(state.clone());
    let session = Session::start(
        Box::new(compositor),
        Box::new(input),
        Box::new(capture),
        SessionConfig {
            command,
            args,
            cwd: None,
            app_name,
            video_output: None,
            video_bitrate: None,
            video_fps: None,
            prewarm_visual: false,
            visual_region_tuning: Default::default(),
            visual_text_tuning: Default::default(),
            visual_click_tuning: Default::default(),
            gsettings_isolated: true,
            xdg_isolated: true,
            extra_env: Vec::new(),
            capture_external_effects: false,
        },
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok((Arc::new(session), state))
}

/// Issue #11 measurement against REAL GTK reference apps (gtk4-widget-factory,
/// gtk4-demo) rather than our purpose-built fixture, to confirm the cache
/// design's numbers hold at real tree scale and real event churn. Needs
/// `gtk4-devel-tools` in the env; each app is skipped gracefully if absent.
/// Ground truth is the walk (real apps emit no `fixture-event:` markers): focus
/// is driven with Tab (safe in any app — never opens a modal) to produce clean
/// state-changed mutations. Diagnostic; read the RESULT block on stderr.
#[tokio::test]
#[ignore = "diagnostic probe; needs gtk4-demo + gtk4-widget-factory; run in the Fedora dev-container"]
async fn atspi_event_cache_real_app_measurement() -> anyhow::Result<()> {
    use std::sync::atomic::Ordering::SeqCst;
    use std::time::Instant;

    init_tracing();

    // (binary, args, a11y-name guess, label). `app_name` matches leniently
    // (normalized, bidirectional substring); a miss dumps the live registry.
    let targets: &[(&str, &[&str], &str, &str)] = &[
        (
            "gtk4-widget-factory",
            &[],
            "Widget Factory",
            "widget-factory",
        ),
        // "demo" matches both "GTK Demo" and "gtk4-demo" under the registry's
        // bidirectional substring rule ("gtk4" ≠ "gtk" defeats a tighter guess).
        ("gtk4-demo", &[], "demo", "gtk4-demo"),
    ];

    eprintln!("=== RESULT: AT-SPI event-cache measurement — REAL apps (issue #11) ===");
    for (bin, args, app_name, label) in targets {
        let Some(path) = which(bin) else {
            eprintln!("[{label}] '{bin}' not installed — skipping (dnf install gtk4-devel-tools)");
            continue;
        };
        let args_owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();

        // Retry once: the first mutter launch after a cold container can miss
        // the wayland-socket deadline; a second attempt runs warm.
        let mut started = None;
        for attempt in 0..2 {
            match start_app_session(path.clone(), args_owned.clone(), (*app_name).to_string()).await
            {
                Ok(s) => {
                    started = Some(s);
                    break;
                }
                Err(e) => eprintln!("[{label}] start attempt {attempt} failed: {e}"),
            }
        }
        let Some((session, _state)) = started else {
            eprintln!("[{label}] could not start session — skipping");
            continue;
        };

        let tally = Arc::new(EventTally::default());
        let collector = spawn_event_collector(tally.clone());
        tokio::time::sleep(Duration::from_millis(1200)).await; // settle + registration

        // --- Tree scale + cache footprint (Q5 at real scale) ----------------
        let xml = match session.a11y_connection.as_ref() {
            Some(conn) => {
                waydriver::atspi::snapshot_tree(conn, &session.app_bus_name, &session.app_path)
                    .await
                    .unwrap_or_default()
            }
            None => String::new(),
        };
        let nodes = session.locate("//*").count().await.unwrap_or(0);
        let per_node = if nodes > 0 {
            xml.len() as f64 / nodes as f64
        } else {
            0.0
        };
        eprintln!(
            "[{label}] scale: {nodes} nodes, snapshot {:.1} KiB ({per_node:.0} B/node) — real-scale cache footprint",
            xml.len() as f64 / 1024.0
        );

        // Short structure peek so the design notes can describe the real tree.
        if let Ok(tree) = session.dump_tree().await {
            let head: String = tree.lines().take(10).collect::<Vec<_>>().join("\n");
            eprintln!("[{label}] tree head:\n{head}");
        }

        // --- Idle cost (Q3 at real scale): does a real app emit at rest? -----
        let idle_before = tally.total.load(SeqCst);
        tokio::time::sleep(Duration::from_secs(3)).await;
        let idle = tally.total.load(SeqCst) - idle_before;
        eprintln!(
            "[{label}] idle: {idle} events / 3s ({:.1}/s) while untouched",
            idle as f64 / 3.0
        );

        // --- Reliability, consistency, and before/after timing under driving -
        // Tab moves focus without opening modals -> clean state-changed events;
        // between drives the tree is static, so the poll loop times the
        // status-quo full walk against the event-gated cache at the warm-hit
        // rate (same tree, same cadence — only the cache differs).
        let drives = 10u32;
        let mut drove_with_event = 0u32;
        let mut windows_ms: Vec<f64> = Vec::new();
        let mut hits = 0u64;
        let mut misses = 0u64;
        // Before/after timing: each poll times the status-quo full walk against
        // the event-gated cache (re-serve the retained snapshot when no event
        // landed since the last walk). Same tree, same cadence — only the cache
        // differs.
        let mut cached: Option<String> = None;
        let mut walk_ns_sum: u128 = 0;
        let mut after_ns_sum: u128 = 0;
        let mut reserve_ns_sum: u128 = 0;
        for _ in 0..drives {
            let before = tally.total.load(SeqCst);
            tally.dirty.store(false, SeqCst);
            let t = Instant::now();
            let _ = session.press_chord("Tab").await;
            let deadline = Instant::now() + Duration::from_millis(800);
            let mut first: Option<f64> = None;
            while Instant::now() < deadline {
                if tally.total.load(SeqCst) > before {
                    first = Some(t.elapsed().as_secs_f64() * 1e3);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(120)).await; // settle
            if tally.total.load(SeqCst) > before {
                drove_with_event += 1;
            }
            if let Some(ms) = first {
                windows_ms.push(ms);
            }
            for _ in 0..6 {
                // BEFORE: the production walk waydriver runs on every call today.
                let tw = Instant::now();
                let xml = match session.a11y_connection.as_ref() {
                    Some(conn) => waydriver::atspi::snapshot_tree(
                        conn,
                        &session.app_bus_name,
                        &session.app_path,
                    )
                    .await
                    .unwrap_or_default(),
                    None => String::new(),
                };
                let walk_ns = tw.elapsed().as_nanos();
                walk_ns_sum += walk_ns;
                // AFTER: event-gated cache. A miss must walk (reuse the walk we
                // just timed) and refresh; a hit re-serves the retained String
                // the locator's evaluate_xpath consumes.
                if tally.dirty.swap(false, SeqCst) || cached.is_none() {
                    cached = Some(xml);
                    after_ns_sum += walk_ns;
                    misses += 1;
                } else {
                    let tc = Instant::now();
                    let served = cached.clone();
                    std::hint::black_box(&served);
                    let reserve_ns = tc.elapsed().as_nanos();
                    reserve_ns_sum += reserve_ns;
                    after_ns_sum += reserve_ns;
                    hits += 1;
                }
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        }
        let mean_window = if windows_ms.is_empty() {
            f64::NAN
        } else {
            windows_ms.iter().sum::<f64>() / windows_ms.len() as f64
        };
        let total_snaps = hits + misses;
        let hit_pct = if total_snaps > 0 {
            hits as f64 * 100.0 / total_snaps as f64
        } else {
            0.0
        };
        let denom = total_snaps.max(1) as f64;
        let walk_mean_ms = walk_ns_sum as f64 / denom / 1e6;
        let after_mean_ms = after_ns_sum as f64 / denom / 1e6;
        let walk_mean_us = walk_ns_sum as f64 / denom / 1e3;
        let reserve_mean_us = if hits > 0 {
            reserve_ns_sum as f64 / hits as f64 / 1e3
        } else {
            f64::NAN
        };
        let speedup = if after_ns_sum > 0 {
            walk_ns_sum as f64 / after_ns_sum as f64
        } else {
            f64::NAN
        };
        eprintln!(
            "[{label}] reliability: {drove_with_event}/{drives} focus drives produced an event; \
             consistency: first event ~{mean_window:.0}ms after the drive"
        );
        eprintln!(
            "[{label}] money: {total_snaps} snapshots over the cadence, {hits} warm hits / \
             {misses} reconciles ({hit_pct:.0}% cached)"
        );
        eprintln!(
            "[{label}] before/after: status-quo walk {walk_mean_ms:.2}ms/call vs event-cache \
             {after_mean_ms:.2}ms/call amortized = {speedup:.1}x less walk time"
        );
        eprintln!(
            "[{label}] per cached call: {reserve_mean_us:.1}µs re-serve vs {walk_mean_us:.0}µs walk \
             (~{:.0}x faster)",
            walk_mean_us / reserve_mean_us
        );
        eprintln!(
            "[{label}] classes: children +{}/-{} state {} property {} text {} other {} (total {})",
            tally.children_insert.load(SeqCst),
            tally.children_delete.load(SeqCst),
            tally.state_changed.load(SeqCst),
            tally.property_change.load(SeqCst),
            tally.text_changed.load(SeqCst),
            tally.other.load(SeqCst),
            tally.total.load(SeqCst),
        );

        collector.abort();
        kill(session).await?;
    }
    eprintln!("=== end RESULT ===");
    Ok(())
}

// ── Cache-first locator resolution (issue #11) ───────────────────────────

/// With cache resolution (the default), locators resolve against the bulk
/// `Cache.GetItems` snapshot instead of the per-node walk. Drives the gtk4
/// fixture through that path to prove parity: name/role/state selectors
/// resolve, an action fires, and the lazily enriched bounds (which the
/// cache reply doesn't carry) come back live.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn cache_resolution_drives_widgets_end_to_end() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let sel = "//Button[@name='primary-button']";
    let check = "//Checkbox[@name='agree-check']";

    // Capture walk-mode ground truth (force the walk explicitly), then
    // flip to cache resolution and assert parity — same count and role.
    // (set_cache_resolution toggles the mode per call.)
    session.set_cache_resolution(false);
    let walk_count = session.locate(sel).count().await?;
    let walk_role = session.locate(sel).role().await?;
    assert_eq!(
        walk_count, 1,
        "primary-button should resolve uniquely (walk)"
    );

    session.set_cache_resolution(true);

    let cache_count = session.locate(sel).count().await?;
    let cache_role = session.locate(sel).role().await?;
    assert_eq!(
        cache_count, walk_count,
        "cache resolution must find the same match count as the walk"
    );
    assert_eq!(
        cache_role, walk_role,
        "cache resolution must report the same role as the walk"
    );

    // State comes from the cache snapshot (no live read needed).
    let checked = session.locate(check).is_checked().await?;
    assert!(!checked, "agree-check starts unchecked");

    // Bounds are NOT in the cache reply — exercises lazy live enrichment.
    let bounds = session.locate(sel).bounds().await?;
    assert!(
        bounds.width > 0 && bounds.height > 0,
        "cache-mode bounds must be enriched live and non-empty: {bounds:?}"
    );

    // Action path: resolve via cache, fire the real GTK click handler.
    let cursor = session.stdout_cursor();
    session.locate(sel).click().await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("clicked primary-button"),
            Duration::from_secs(3),
        )
        .await?;

    // Re-resolution reflects state changes: toggle the checkbox and
    // confirm the flipped state reads back through the cache.
    let cursor = session.stdout_cursor();
    session.locate(check).click().await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("checked agree-check active=true"),
            Duration::from_secs(3),
        )
        .await?;
    let checked_after = session.locate(check).is_checked().await?;
    assert!(
        checked_after,
        "agree-check must read checked after the click"
    );

    kill(session).await?;
    Ok(())
}

/// The set of `_ref` node identities in a snapshot — the strongest
/// "same tree" comparison key (identity, not just count).
fn ref_set(xml: &str) -> std::collections::BTreeSet<String> {
    xml.match_indices("_ref=\"")
        .map(|(i, _)| {
            let rest = &xml[i + 6..];
            rest[..rest.find('"').unwrap_or(0)].to_string()
        })
        .collect()
}

/// Assert the walk and the cache see the *same* set of nodes right now.
/// On divergence, bail with the symmetric difference so the failure names
/// the offending nodes instead of just a count mismatch.
async fn assert_tree_parity(session: &Arc<Session>, label: &str) -> anyhow::Result<()> {
    let walk = session.dump_tree().await?;
    let cache = session.dump_tree_cached().await?;
    let walk_refs = ref_set(&walk);
    let cache_refs = ref_set(&cache);
    if walk_refs != cache_refs {
        let only_walk: Vec<_> = walk_refs.difference(&cache_refs).take(8).collect();
        let only_cache: Vec<_> = cache_refs.difference(&walk_refs).take(8).collect();
        anyhow::bail!(
            "[{label}] cache/walk tree divergence: walk={} cache={} \
             only_in_walk(<=8)={:?} only_in_cache(<=8)={:?}",
            walk_refs.len(),
            cache_refs.len(),
            only_walk,
            only_cache
        );
    }
    eprintln!("[parity:{label}] walk == cache: {} nodes", walk_refs.len());
    Ok(())
}

/// The reliability question behind issue #11: is the `Cache.GetItems` tree
/// a faithful stand-in for the `GetChildren` walk — not just at startup
/// but as the UI churns? Drives a modal dialog open and closed, asserting
/// node-set parity at each step, then confirms an attribute selector
/// (which can't be served from the cache) resolves identically in both
/// modes via the walk fallback.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn cache_walk_tree_parity_across_dynamic_states() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    assert_tree_parity(&session, "startup").await?;

    // Open a modal dialog — a whole subtree appears.
    let cursor = session.stdout_cursor();
    session
        .locate("//Button[@name='open-dialog']")
        .click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("dialog-opened sample-dialog"),
            Duration::from_secs(3),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await; // let the cache settle
    assert_tree_parity(&session, "dialog-open").await?;

    // Close it — the subtree goes away.
    let cursor = session.stdout_cursor();
    session
        .locate("//Button[@name='dialog-close']")
        .click()
        .await?;
    session
        .wait_for_stdout_line(
            cursor,
            |l| l.contains("dialog-closed sample-dialog"),
            Duration::from_secs(3),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_tree_parity(&session, "dialog-closed").await?;

    // Attribute selectors aren't serviceable from the cache, so cache mode
    // must transparently fall back to the walk and resolve them identically
    // (here a nonexistent id → 0 in both, exercising the fallback path).
    session.set_cache_resolution(false);
    let walk_id = session.find_by_id("no-such-id").count().await?;
    session.set_cache_resolution(true);
    let cache_id = session.find_by_id("no-such-id").count().await?;
    assert_eq!(
        walk_id, cache_id,
        "attribute selector must resolve identically via the walk fallback"
    );

    kill(session).await?;
    Ok(())
}

/// Issue #12: the typed [`Role`] helpers resolve real GTK4 widgets, and do so
/// identically to the equivalent hand-written XPath. Anchors the `Role` enum's
/// element-name mappings to the toolkit's actual AT-SPI role tags.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn find_by_role_resolves_gtk4_widgets() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    // Role::Button resolves the same element as the equivalent raw XPath.
    let by_role = session
        .find_by_role(Role::Button, "primary-button")
        .count()
        .await?;
    let by_xpath = session
        .locate("//Button[@name='primary-button']")
        .count()
        .await?;
    assert_eq!(
        by_role, 1,
        "find_by_role(Button) must resolve primary-button"
    );
    assert_eq!(by_role, by_xpath, "typed helper must match the raw XPath");

    // Role::TextBox is the entry/text-view tag; the locator is fillable.
    // Scoped so the Locator's session Arc clone drops before `kill`.
    {
        let entry = session.find_by_role(Role::TextBox, "text-entry");
        entry.fill("role-helper").await?;
        assert_eq!(entry.text().await?, "role-helper");
    }

    // Role::Checkbox is a divergent role: the walk tags a check box `Checkbox`
    // but the cache tags it `CheckBox`. The typed helper compiles a union, so
    // it resolves the widget regardless of which snapshot serves the lookup —
    // where a single-tag `//Checkbox` would miss the cache and need a walk.
    let check = session
        .find_by_role(Role::Checkbox, "agree-check")
        .count()
        .await?;
    assert_eq!(check, 1, "find_by_role(Checkbox) must resolve agree-check");

    // The union selector is itself cache-eligible (only `@name`), so cache-first
    // resolution serves it directly. Confirm the cache really does tag the check
    // box `CheckBox` — the spelling the Role::Checkbox union must also match.
    let cache_tree = session.dump_tree_cached().await?;
    assert!(
        cache_tree.contains("<CheckBox") && cache_tree.contains("name=\"agree-check\""),
        "premise: the cache tags the check box `CheckBox`, the spelling the \
         Role::Checkbox union must also match"
    );

    kill(session).await?;
    Ok(())
}
