//! Drive GNOME Calculator end-to-end via waydriver.
//!
//! Spawns a headless mutter session, launches `gnome-calculator` inside it,
//! computes `2 + 3 = 5` by clicking buttons via AT-SPI, reads the result
//! back out of the AT-SPI tree, takes a screenshot, and records a WebM of
//! the whole session.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p waydriver-examples --example gnome_calculator
//! ```
//!
//! Requires `gnome-calculator` on `PATH`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use waydriver::{CompositorRuntime, Session, SessionConfig};
use waydriver_capture_mutter::MutterCapture;
use waydriver_compositor_mutter::MutterCompositor;
use waydriver_input_mutter::MutterInput;

async fn save_screenshot(session: &Arc<Session>, out: &Path) -> anyhow::Result<()> {
    let png = session.take_screenshot().await?;
    let png_start = png
        .windows(4)
        .position(|w| w == [0x89, b'P', b'N', b'G'])
        .ok_or_else(|| anyhow::anyhow!("no PNG magic in screenshot data"))?;
    std::fs::write(out, &png[png_start..])?;
    eprintln!("wrote screenshot: {}", out.display());
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut compositor = MutterCompositor::new();
    compositor.start(None).await?;
    let state = compositor
        .state()
        .expect("MutterCompositor::state must be Some immediately after start() succeeded");
    let input = MutterInput::new(state.clone());
    let capture = MutterCapture::new(state);

    let video_path = std::env::temp_dir().join("gnome-calculator-2-plus-3.webm");
    let session = Session::start(
        Box::new(compositor),
        Box::new(input),
        Box::new(capture),
        SessionConfig {
            command: "gnome-calculator".into(),
            args: vec![],
            cwd: None,
            // Accessible name gnome-calculator registers under on the AT-SPI
            // bus. Matched case-insensitively against the registry.
            app_name: "gnome-calculator".into(),
            video_output: Some(video_path.clone()),
            video_bitrate: None,
            video_fps: None,
        },
    )
    .await?;
    let session = Arc::new(session);

    // Readiness gate: wait for the `=` button to be visible. Once any
    // button in the keypad is laid out and clickable, the rest are too —
    // gnome-calculator publishes them as a single TabPanel — and we can
    // start dispatching AT-SPI actions against them.
    session
        .locate("//Button[@name='=']")
        .with_timeout(Duration::from_secs(10))
        .wait_for_visible()
        .await?;

    // Dump the tree so the example output shows the widgets available
    // against this gnome-calculator build.
    let tree = session.dump_tree().await?;
    eprintln!(
        "── gnome-calculator AT-SPI tree ────────────────────────────\n\
         {tree}\n\
         ────────────────────────────────────────────────────────────"
    );

    // Compute `2 + 3` by clicking buttons via AT-SPI. `Locator::click`
    // routes through `Action.DoAction`, which doesn't depend on keyboard
    // routing — every press lands deterministically on the first attempt.
    for label in ["2", "+", "3", "="] {
        session
            .locate(&format!("//Button[@name='{label}']"))
            .click()
            .await?;
    }

    // AT-SPI actions update GTK's internal model but don't always tick
    // the compositor's frame clock in headless mutter. A bare Shift_L
    // (keysym 0xffe1) is a no-op for content but wakes the event loop
    // so the result label gets republished into the AT-SPI tree and
    // the screenshot reflects it.
    session.press_keysym(0xffe1).await?;

    // Verify the calculation lands as expected. `name="2+3"` is the
    // expression label in the history line; `name="5"` matching is
    // ambiguous on its own (the keypad has a `5` button), so we check
    // the more specific expression label as the primary assertion.
    session
        .locate("//Label[@name='2+3']")
        .with_timeout(Duration::from_secs(2))
        .wait_for_visible()
        .await?;
    eprintln!("verified history line: 2+3");

    let post_eval_tree = session.dump_tree().await?;
    eprintln!(
        "── gnome-calculator AT-SPI tree (post-eval) ────────────────\n\
         {post_eval_tree}\n\
         ────────────────────────────────────────────────────────────"
    );

    // Snapshot the post-arithmetic state for the artifact set.
    let arith_png = std::env::temp_dir().join("gnome-calculator-2-plus-3.png");
    save_screenshot(&session, &arith_png).await?;

    // Clear the prior result so the conversion starts from an empty
    // buffer. Escape clears the current entry on gnome-calculator.
    session.press_chord("Escape").await?;

    // ── Chord dispatch demo ────────────────────────────────────────
    //
    // `press_chord` parses strings like "Ctrl+Shift+S", holds modifiers,
    // presses the target key, then releases modifiers in reverse order
    // — even on error, so a panicked chord can't leave keys stuck down.
    //
    // To make the demo *observable*, we pick chords whose effect is
    // directly visible in the calculator: `Shift+9` and `Shift+0`
    // produce `(` and `)` (US keyboard layout). The unshifted `9` and
    // `0` would produce digits — parentheses appearing instead are
    // proof the Shift modifier was held when each keysym was dispatched.
    //
    // We compute `(7-2)*3 = 15` entirely through the keyboard so the
    // chord and the surrounding keystrokes go through one input channel
    // — interleaving with AT-SPI button clicks would race because the
    // two paths reach the app via different routes (compositor input vs.
    // direct D-Bus action) with no ordering guarantee. The parens come
    // from `Shift+9`/`Shift+0` chords; the rest is plain `type_text`,
    // and Return triggers the eval.
    session.press_chord("Shift+9").await?; // (
    session.type_text("7-2").await?;
    session.press_chord("Shift+0").await?; // )
    session.type_text("*3").await?;
    session.press_chord("Return").await?;
    session.press_keysym(0xffe1).await?;

    // gnome-calculator normalizes `-` to the Unicode minus `−` and `*`
    // to `×` in the history line, so the asserted expression label uses
    // those forms. The match on `(7−2)×3` proves both Shift chords
    // landed in the right order and the parens framed the subexpression
    // correctly.
    session
        .locate("//Label[@name='(7−2)×3']")
        .with_timeout(Duration::from_secs(3))
        .wait_for_visible()
        .await?;
    eprintln!("verified chord dispatch: Shift+9 / Shift+0 produced parens in '(7−2)×3'");

    session.press_chord("Escape").await?;

    // ── Unit conversion ────────────────────────────────────────────
    //
    // gnome-calculator's expression parser auto-completes single-letter
    // unit aliases — `F` becomes `Floor()`, `C` can become `Cos()` —
    // so we use the unambiguous `degC`/`degF` forms.
    session.type_text("24 degC in degF").await?;
    session.press_chord("Return").await?;

    // Wake the event loop so the converted value is republished.
    session.press_keysym(0xffe1).await?;

    // 24 °C = 75.2 °F. The converted result lands in a Label whose
    // `name` includes `75.2` — match the unique substring rather than
    // the full formatted string, which can vary by locale (decimal mark)
    // and gnome-calculator's display precision.
    let converted = session
        .locate("//Label[contains(@name, '75.2')]")
        .with_timeout(Duration::from_secs(5))
        .wait_for_visible()
        .await;
    if converted.is_err() {
        let fail_png = std::env::temp_dir().join("gnome-calculator-conversion-failed.png");
        save_screenshot(&session, &fail_png).await?;
        let fail_tree = session.dump_tree().await?;
        eprintln!(
            "── gnome-calculator AT-SPI tree (conversion failed) ────────\n\
             {fail_tree}\n\
             ────────────────────────────────────────────────────────────"
        );
        anyhow::bail!("expected '75.2' in result label after `24 degC in degF`");
    }
    eprintln!("verified conversion: 24 °C ≈ 75.2 °F");

    // Snapshot the post-conversion state.
    let conv_png = std::env::temp_dir().join("gnome-calculator-24c-to-f.png");
    save_screenshot(&session, &conv_png).await?;

    let session = Arc::try_unwrap(session)
        .map_err(|_| anyhow::anyhow!("session arc still referenced when killing"))?;
    // `kill` flushes the recording: it stops the VP8 encoder and sends EOS
    // through webmmux so the resulting WebM is seekable.
    session.kill().await?;
    eprintln!("wrote recording: {}", video_path.display());
    Ok(())
}
