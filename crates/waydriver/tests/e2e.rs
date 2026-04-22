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
        tree.contains("<PushButton"),
        "tree should contain PushButton elements, got:\n{tree}"
    );

    // Find a known element — the "1" button — with a scoped XPath.
    let button_one = session.locate("//PushButton[@name='1']");
    assert!(button_one.count().await? >= 1, "should find button '1'");

    // A non-existent selector yields ElementNotFound when resolved as single.
    let missing = session.locate("//PushButton[@name='nonexistent_xyz_12345']");
    let err = missing.click().await.unwrap_err();
    assert!(
        matches!(err, Error::ElementNotFound { .. }),
        "expected ElementNotFound, got: {err}"
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
    session.locate("//PushButton[@name='5']").click().await?;
    session.press_keysym(0xffe1).await?; // Shift_L wake
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Press "+" via keysym.
    session.press_keysym(0x2b).await?; // '+'
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Click "3" via locator + wake.
    session.locate("//PushButton[@name='3']").click().await?;
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
