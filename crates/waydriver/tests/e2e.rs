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
//! cargo test -p waydriver --test e2e -- --ignored --test-threads=1
//! ```
//!
//! The MCP-level e2e test in `crates/waydriver-mcp/tests/e2e.rs` drives
//! the same fixture through the MCP JSON-RPC surface inside a Docker
//! container. Both test layers target the fixture only — no external
//! app dependencies.

use std::sync::Arc;
use std::time::Duration;

use waydriver::{CompositorRuntime, Error, FillMode, InputBackend, Session, SessionConfig};
use waydriver_capture_mutter::MutterCapture;
use waydriver_compositor_mutter::{MutterCompositor, MutterState};
use waydriver_input_mutter::MutterInput;

/// Strip any GStreamer status messages preceding the PNG magic bytes.
fn extract_png(raw: &[u8]) -> Vec<u8> {
    let png_start = raw
        .windows(4)
        .position(|w| w == [0x89, b'P', b'N', b'G'])
        .expect("no PNG magic found in screenshot data");
    raw[png_start..].to_vec()
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
    compositor.start(None).await?;
    let state = compositor.state();
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
        },
    )
    .await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    Ok((Arc::new(session), state))
}

/// Spot-check every name in `expected` appears at least once in the tree.
/// Shared body used by the three per-section diagnostic tests.
async fn assert_widgets_exist(session: &Arc<Session>, section: &str, expected: &[&str]) {
    let tree = session.dump_tree().await.expect("dump_tree");
    eprintln!(
        "── fixture tree ({section}) ─────────────────────────────\n{tree}\n\
         ────────────────────────────────────────────────────────────"
    );
    for expected_name in expected {
        let count = session
            .locate(&format!("//*[@name='{expected_name}']"))
            .count()
            .await
            .unwrap_or(usize::MAX);
        assert!(
            count >= 1,
            "expected named widget '{expected_name}' to be in the tree (count: {count})"
        );
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
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
    .await;
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
            "open-adw-dialog",
            // The main-menu button lives in the header bar and is present
            // regardless of which section is selected.
            "main-menu",
        ],
    )
    .await;
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
    .await;
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

/// Screenshot before/after toggling a ToggleButton — proves that locator
/// actions produce real pixel changes in the compositor, not just AT-SPI
/// state updates.
#[tokio::test]
#[ignore = "spawns mutter + pipewire; run manually with --ignored"]
async fn fixture_toggle_changes_screenshot() -> anyhow::Result<()> {
    init_tracing();
    let (session, _state) = start_fixture_session("gtk4").await?;

    let baseline = extract_png(&session.take_screenshot().await?);
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

    let after = extract_png(&session.take_screenshot().await?);
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

    // Focus-routing handshake. The fixture emits `focus-acquired text-entry`
    // as soon as GTK grabs focus client-side, but the Wayland compositor
    // needs time to redirect keyboard input to the newly-focused surface —
    // key events sent too soon after the focus event get routed to
    // whichever surface held focus before. Fire warmup keystrokes until
    // we see one actually land on the entry; that's the deterministic
    // signal that routing has caught up. Then clear the entry and let
    // the buffer settle so the real test starts from a clean state.
    session
        .wait_for_stdout_line(
            0,
            |l| l.contains("focus-acquired text-entry"),
            Duration::from_secs(5),
        )
        .await?;
    let mut routed = false;
    let warmup_start = session.stdout_cursor();
    for _ in 0..15 {
        session.press_chord("a").await?;
        if session
            .wait_for_stdout_line(
                warmup_start,
                |l| l.contains("text-changed text-entry"),
                Duration::from_millis(250),
            )
            .await
            .is_ok()
        {
            routed = true;
            break;
        }
    }
    assert!(
        routed,
        "keystrokes never reached text-entry despite focus-acquired"
    );
    // Any additional buffered 'a' presses may still be queued — sleep
    // briefly so they flush, then clear, then sleep again so the clear
    // event is the last thing in the buffer before the real test runs.
    tokio::time::sleep(Duration::from_millis(300)).await;
    session.press_chord("Ctrl+A").await?;
    session.press_chord("BackSpace").await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

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

    pointer.pointer_motion_relative(100.0, 100.0).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // BTN_LEFT = 0x110.
    pointer.pointer_button(0x110).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;

    pointer.pointer_motion_relative(-50.0, -50.0).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Confirm session is still functional by taking a screenshot.
    let screenshot = session.take_screenshot().await?;
    let png = extract_png(&screenshot);
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
    let png = extract_png(&screenshot);
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
    entry().fill("hello world", FillMode::CaretNav).await?;
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
    entry().fill("replaced", FillMode::CaretNav).await?;
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
