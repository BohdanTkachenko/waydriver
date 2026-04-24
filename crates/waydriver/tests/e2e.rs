//! End-to-end tests for the waydriver library against headless mutter.
//!
//! Each test spins up its own mutter session with gnome-calculator (isolated
//! from the user's settings via keyfile GSettings backend), exercises a
//! different part of the API, and tears the session down.
//!
//! These tests are `#[ignore]`-gated because they currently depend on the
//! host AT-SPI session bus, and `gnome-calculator`'s singleton D-Bus
//! activation causes parallel test sessions to latch onto a shared calculator
//! instance — tests then race on its UI state. See the tracking issue for
//! the session-isolation fix.
//!
//! Run them explicitly with:
//!
//! ```sh
//! cargo test -p waydriver --test e2e -- --ignored --test-threads=1
//! ```

use std::sync::Arc;

use waydriver::{CompositorRuntime, Error, InputBackend, Session, SessionConfig};
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

/// Start a gnome-calculator session, returning the Session wrapped in Arc
/// (so callers can use the XPath Locator API that lives behind `&Arc<Session>`)
/// and the shared MutterState for constructing extra InputBackends.
async fn start_calculator_session() -> anyhow::Result<(Arc<Session>, Arc<MutterState>)> {
    let mut compositor = MutterCompositor::new();
    compositor.start(None).await?;
    let state = compositor.state();
    let input = MutterInput::new(state.clone());
    let capture = MutterCapture::new(state.clone());

    let session = Session::start(
        Box::new(compositor),
        Box::new(input),
        Box::new(capture),
        SessionConfig {
            command: "gnome-calculator".into(),
            args: vec![],
            cwd: None,
            app_name: "gnome-calculator".into(),
            video_output: None,
            video_bitrate: None,
        },
    )
    .await?;

    // Let the app render its initial frame.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Dismiss any startup dialog.
    session.press_keysym(0xff1b).await?; // Escape
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    Ok((Arc::new(session), state))
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

#[tokio::test]
#[ignore = "flaky: shared gnome-calculator instance on host a11y bus"]
async fn calculator_screenshots_change() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let (session, _state) = start_calculator_session().await?;

    // Baseline screenshot.
    let baseline = extract_png(&session.take_screenshot().await?);
    assert!(baseline.len() > 1000, "baseline screenshot too small");

    // Type "6" then "=" via RemoteDesktop keysym input.
    for keysym in [0x36 /* '6' */, 0x3d /* '=' */] {
        session.press_keysym(keysym).await?;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // After-input screenshot.
    let after_input = extract_png(&session.take_screenshot().await?);

    // Decode PNGs and compare actual pixel data.
    let img1 = image::load_from_memory(&baseline)?.to_rgba8();
    let img2 = image::load_from_memory(&after_input)?.to_rgba8();
    let diff_pixels = img1
        .pixels()
        .zip(img2.pixels())
        .filter(|(a, b)| a != b)
        .count();
    eprintln!("pixel diff: {diff_pixels} / {} pixels", img1.pixels().len());

    kill(session).await?;

    assert!(
        diff_pixels > 100,
        "screenshot should change after typing 6 = (only {diff_pixels} pixels differ)"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "flaky: shared gnome-calculator instance on host a11y bus"]
async fn accessibility_tree_inspection() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let (session, _state) = start_calculator_session().await?;

    // Dump the accessibility tree as XML and verify shape.
    let tree = session.dump_tree().await?;
    assert!(!tree.is_empty(), "accessibility tree should not be empty");
    assert!(
        tree.contains("<?xml"),
        "tree should start with an XML declaration, got:\n{tree}"
    );
    assert!(
        tree.contains("<Button"),
        "tree should contain Button elements, got:\n{tree}"
    );

    // Find a known element — the "1" button — with a scoped XPath.
    let button_one = session.locate("//Button[@name='1']");
    assert!(button_one.count().await? >= 1, "should find button '1'");

    // A non-existent selector yields ElementNotFound when resolved as single.
    // Use a short timeout so the auto-wait doesn't stretch the test by 5s
    // while it polls for an element we know won't appear.
    let missing = session
        .locate("//Button[@name='nonexistent_xyz_12345']")
        .with_timeout(std::time::Duration::from_millis(250));
    let err = missing.click().await.unwrap_err();
    assert!(
        matches!(err, Error::ElementNotFound { .. }),
        "expected ElementNotFound, got: {err}"
    );

    // wait_for_visible on an already-visible button returns quickly (the
    // auto-wait path), exercising the positive branch of poll_with_retry.
    session
        .locate("//Button[@name='1']")
        .wait_for_visible()
        .await?;

    // wait_for_count: the calculator keypad has a known number of digit
    // buttons. We don't hard-code the exact count (it varies across GNOME
    // Calculator versions) — just verify the selector resolves non-zero
    // and wait_for_count accepts the current count as a no-op.
    let digit_count = session.locate("//Button").count().await?;
    assert!(digit_count > 0, "expected some buttons, got 0");
    session
        .locate("//Button")
        .wait_for_count(digit_count)
        .await?;

    kill(session).await?;
    Ok(())
}

// NOTE: a positive e2e for Locator::focus against gnome-calculator would
// belong here, but calc 49 doesn't implement the AT-SPI Component
// interface on any widget — both Button and TextBox return D-Bus
// NotSupported on grab_focus. That's a documented GTK4 gap (see
// Locator::focus docs). The API is covered by unit tests in the locator
// module; real-world validation waits for a test fixture that implements
// Component properly (gnome-text-editor is a candidate).

#[tokio::test]
#[ignore = "flaky: shared gnome-calculator instance on host a11y bus"]
async fn keyboard_chord_dispatches_modifiers() -> anyhow::Result<()> {
    // Exercises Session::press_chord end-to-end. We commit an
    // expression two different ways — via single-key chord calls ("2", "+",
    // "3", "=") and via a modifier chord ("Ctrl+A", though its effect on
    // calc's entry isn't what we assert on). The meaningful assertion is
    // the "2+3" expression landing in the history list, which proves the
    // chord dispatch path delivers single-key presses correctly.
    //
    // Deeper verification of the Ctrl+A select-all behavior is covered by
    // the unit test `press_chord_issues_modifiers_then_target_then_releases_in_reverse`
    // in the session module — we can't reliably assert select-all behavior
    // via AT-SPI in gnome-calculator 49 because its editable TextBox
    // doesn't expose current input through the Text interface.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let (session, _state) = start_calculator_session().await?;

    // Single-key chords dispatch through the same path as multi-key ones —
    // if these work, modifier routing works too (modifiers go through the
    // extra key_down/key_up primitives which the unit tests cover).
    for token in ["2", "+", "3", "="] {
        session.press_chord(token).await?;
    }

    // History's last row should be the computation we just committed.
    session
        .locate("//ListItem//Label[1]")
        .wait_for_text(|t| t == "2+3")
        .await?;
    session
        .locate("//ListItem//Label[last()]")
        .wait_for_text(|t| t == "5")
        .await?;

    // Also exercise a modifier chord — we can't assert its effect visibly,
    // but we can assert it doesn't panic or stuck a modifier. If Ctrl got
    // stuck, subsequent digit entries would be misinterpreted and the next
    // calculation would break.
    session.press_chord("Ctrl+A").await?;
    session.press_chord("BackSpace").await?;
    for token in ["7", "+", "1", "="] {
        session.press_chord(token).await?;
    }
    // The most recent row's expression should now be "7+1". If Ctrl stayed
    // down (or Ctrl+A+BackSpace left random state), we'd see a different
    // expression or no new row at all.
    session
        .locate("//ListItem[last()]//Label[1]")
        .wait_for_text(|t| t == "7+1")
        .await?;

    kill(session).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "flaky: shared gnome-calculator instance on host a11y bus"]
async fn menu_interaction_auto_waits() -> anyhow::Result<()> {
    // Exercises the auto-wait machinery end-to-end against gnome-calculator:
    //
    //   1. type 2+3= via keyboard → populates the history list
    //   2. wait_for_text on the last history label → polls until "5" appears
    //   3. click the Main Menu toggle → opens the hamburger popover
    //   4. wait_for state change on the menu button (expanded=true) →
    //      demonstrates auto-wait picking up async state transitions
    //
    // Known limitation (documented here so the test's scope is clear):
    // gnome-calculator 49's GtkPopoverMenu children aren't exposed in the
    // AT-SPI tree — the Menu element appears but its MenuItem children
    // remain an empty Generic/TabPanel. Clicking a specific menu item like
    // "Clear History" by accessible name isn't possible today in this
    // particular app. Other GTK4 apps with properly-annotated popovers
    // (text editor, system settings) expose their menu items and would
    // work through this same auto-wait path.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let (session, _state) = start_calculator_session().await?;

    // Compute 2+3= via keyboard. GTK4 calculator's digit buttons don't
    // expose the AT-SPI Action interface (activation comes from GtkShortcut,
    // not a registered action), so `locator.click()` on them returns
    // "no action with index 0". Keyboard input is the idiomatic path for
    // number entry anyway.
    for keysym in [0x32, 0x2b, 0x33, 0x3d] {
        session.press_keysym(keysym).await?;
    }

    // The calculation result shows up as the final Label in the history
    // ListItem. wait_for_text polls until the label's text is "5" — the
    // auto-wait case of "element exists but content not yet ready." Without
    // it, the test would need a manual sleep here.
    let result = session
        .locate("//ListItem//Label[last()]")
        .wait_for_text(|t| t == "5")
        .await?;
    assert_eq!(result, "5", "expected history result '5', got {result:?}");

    // Open the primary ("hamburger") menu via its ToggleButton. The outer
    // <Button> wrapper doesn't carry an Action interface in GTK4; the inner
    // ToggleButton does. Name "Main Menu" matches GNOME Calculator 49 —
    // older versions may differ and this selector would need updating.
    session
        .locate("//ToggleButton[@name='Main Menu']")
        .click()
        .await?;
    // Wake GTK's event loop so the popover actually renders. AT-SPI actions
    // mutate GTK's model but don't tick the frame clock; in headless mutter
    // the popover won't appear in the tree until a compositor event forces
    // a repaint.
    session.press_keysym(0xffe1).await?; // Shift_L

    // Auto-wait: the Menu element only appears in the tree once the popover
    // is actually open. No manual sleep — `wait_for_visible` polls until
    // it shows up.
    session
        .locate("//Menu[@name='Main Menu']")
        .wait_for_visible()
        .await?;

    // And the wrapping Button now reports expanded=true. We assert via an
    // XPath predicate on the state attribute; once an `is_expanded()`
    // state predicate lands, the equivalent check becomes a direct call.
    session
        .locate("//Button[@name='Main Menu' and @expanded='true']")
        .wait_for_visible()
        .await?;

    // Dump the tree so the limitation noted at the top is visible in CI logs.
    let tree = session.dump_tree().await?;
    eprintln!(
        "── tree after opening menu ─────────────────────────────────\n{tree}\n\
         ────────────────────────────────────────────────────────────"
    );

    kill(session).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "flaky: shared gnome-calculator instance on host a11y bus"]
async fn click_element_changes_display() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let (session, _state) = start_calculator_session().await?;

    // Baseline screenshot.
    let baseline = extract_png(&session.take_screenshot().await?);

    // Click "5" via the XPath locator, then wake GTK's event loop.
    session.locate("//Button[@name='5']").click().await?;
    session.press_keysym(0xffe1).await?; // Shift_L wake
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Press "+" via keysym.
    session.press_keysym(0x2b).await?; // '+'
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Click "3" via locator + wake.
    session.locate("//Button[@name='3']").click().await?;
    session.press_keysym(0xffe1).await?; // Shift_L wake
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Press "=" via keysym.
    session.press_keysym(0x3d).await?;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // After-click screenshot.
    let after_click = extract_png(&session.take_screenshot().await?);

    let img1 = image::load_from_memory(&baseline)?.to_rgba8();
    let img2 = image::load_from_memory(&after_click)?.to_rgba8();
    let diff_pixels = img1
        .pixels()
        .zip(img2.pixels())
        .filter(|(a, b)| a != b)
        .count();
    eprintln!("pixel diff after click: {diff_pixels}");

    kill(session).await?;

    assert!(
        diff_pixels > 100,
        "display should change after clicking 5 + 3 = (only {diff_pixels} pixels differ)"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "flaky: shared gnome-calculator instance on host a11y bus"]
async fn pointer_input_operations() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let (session, state) = start_calculator_session().await?;

    // Verify Session::wayland_display() accessor.
    assert!(
        session.wayland_display().starts_with("wayland-wd-"),
        "unexpected display name: {}",
        session.wayland_display()
    );

    // Create a second InputBackend from the shared state for pointer tests.
    let pointer = MutterInput::new(state);

    // Move pointer — should succeed without error.
    pointer.pointer_motion_relative(100.0, 100.0).await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Click (BTN_LEFT = 0x110) — should succeed without error.
    pointer.pointer_button(0x110).await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Move pointer again with negative offsets.
    pointer.pointer_motion_relative(-50.0, -50.0).await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Confirm session is still functional by taking a screenshot.
    let screenshot = session.take_screenshot().await?;
    let png = extract_png(&screenshot);
    assert!(png.len() > 1000, "screenshot after pointer ops too small");

    kill(session).await?;
    Ok(())
}
