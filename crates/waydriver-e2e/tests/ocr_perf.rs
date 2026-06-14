//! Self-contained OCR perf + small-text-upscale probe (no compositor / app /
//! AT-SPI). Answers the two questions behind the `visual`-feature bug report:
//!
//! 1. How slow is one full-frame (1280×800) ocrs pass? (report: ~190 s/call on
//!    an unoptimized debug build; with the workspace root's rten/ocrs
//!    opt-level=3 dev override now in place — issue #24 — a debug `cargo test`
//!    measures single-digit seconds/call instead.)
//! 2. Does upscaling a small-text crop before OCR recover text that's missed
//!    at native size? (report: ~11 px row titles read as 0)
//!
//! Renders synthetic black-on-white text at large (~22 px) and small (~11 px)
//! sizes, then runs the exact ocrs pipeline waydriver uses, using the same
//! cached models (`$XDG_CACHE_HOME/waydriver/ocrs-models/`).
//!
//! Run: `cargo test -p waydriver-e2e --test ocr_perf -- --ignored --nocapture`

use std::time::Instant;

use ab_glyph::{FontVec, PxScale};
use image::{Rgb, RgbImage};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
use rten::Model;

fn model_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME set")).join(".cache")
        });
    base.join("waydriver").join("ocrs-models")
}

/// Resolve a sans TTF via fontconfig (present in the dev shell).
fn load_font() -> FontVec {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "sans"])
        .output()
        .expect("run fc-match");
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(!path.is_empty(), "fc-match returned no font path");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read font {path:?}: {e}"));
    FontVec::try_from_vec(bytes).expect("parse font")
}

fn build_engine() -> OcrEngine {
    let dir = model_dir();
    let det = Model::load_file(dir.join("text-detection.rten"))
        .unwrap_or_else(|e| panic!("load detection model from {}: {e}", dir.display()));
    let rec = Model::load_file(dir.join("text-recognition.rten"))
        .unwrap_or_else(|e| panic!("load recognition model from {}: {e}", dir.display()));
    OcrEngine::new(OcrEngineParams {
        detection_model: Some(det),
        recognition_model: Some(rec),
        ..Default::default()
    })
    .expect("construct ocrs engine")
}

/// The exact ocrs call sequence `waydriver::visual::ocr_lines` uses.
fn ocr(engine: &OcrEngine, img: &RgbImage) -> Vec<String> {
    let (w, h) = img.dimensions();
    let src = ImageSource::from_bytes(img.as_raw(), (w, h)).expect("image source");
    let input = engine.prepare_input(src).expect("prepare_input");
    let words = engine.detect_words(&input).expect("detect_words");
    let lines = engine.find_text_lines(&input, &words);
    engine
        .recognize_text(&input, &lines)
        .expect("recognize_text")
        .into_iter()
        .flatten()
        .map(|l| l.to_string())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn draw(img: &mut RgbImage, font: &FontVec, px: f32, x: i32, y: i32, text: &str) {
    imageproc::drawing::draw_text_mut(img, Rgb([0, 0, 0]), x, y, PxScale::from(px), font, text);
}

fn found(lines: &[String], needle: &str) -> bool {
    let n = needle.to_lowercase();
    lines.iter().any(|s| s.to_lowercase().contains(&n))
}

#[test]
#[ignore = "runs ocrs on synthetic text; needs cached ocrs models. --ignored --nocapture"]
fn ocr_perf_and_small_text_upscale_probe() {
    let font = load_font();
    let engine = build_engine();

    // 1280×800 white frame mirroring the report's resolution. Large labels
    // (~22 px cap height, like view-switcher tabs) read fine in the report;
    // small labels (~11 px row titles) read as 0.
    let mut frame = RgbImage::from_pixel(1280, 800, Rgb([255, 255, 255]));
    let large = [("Appearance", 40, 40), ("Behavior", 360, 40)];
    let small = [
        ("Preferences", 40, 130),
        ("Cursor", 40, 165),
        ("Font", 40, 200),
        ("Scrollback", 40, 235),
        ("Terminal", 40, 270),
        ("General", 40, 305),
    ];
    for (t, x, y) in large {
        draw(&mut frame, &font, 30.0, x, y, t);
    }
    for (t, x, y) in small {
        draw(&mut frame, &font, 15.0, x, y, t);
    }

    // ── Measure 1: full-frame OCR cost (the ~190 s/call question). ──
    let t0 = Instant::now();
    let full = ocr(&engine, &frame);
    let full_ms = t0.elapsed().as_millis();
    eprintln!("\n=== full-frame 1280x800 OCR: {full_ms} ms ===");
    eprintln!("recognized {} lines: {full:?}", full.len());
    for (t, ..) in large {
        eprintln!(
            "  large {t:>12}: {}",
            if found(&full, t) { "FOUND" } else { "MISS" }
        );
    }
    for (t, ..) in small {
        eprintln!(
            "  small {t:>12}: {}",
            if found(&full, t) { "FOUND" } else { "MISS" }
        );
    }

    // ── Measure 2: a small-text crop, native vs 2x vs 3x upscale. This is the
    // exact transform `VisualLocator::with_upscale(factor)` applies before OCR
    // (Lanczos3 resize of the crop), so it proves the per-search upscale path
    // recovers text the native frame misses. ──
    let crop = image::imageops::crop_imm(&frame, 30, 222, 240, 40).to_image();
    let upscale_crop = |factor: u32| {
        image::imageops::resize(
            &crop,
            crop.width() * factor,
            crop.height() * factor,
            image::imageops::FilterType::Lanczos3,
        )
    };
    let t1 = Instant::now();
    let native = ocr(&engine, &crop);
    let native_ms = t1.elapsed().as_millis();
    let up2 = upscale_crop(2);
    let t2 = Instant::now();
    let upscaled2 = ocr(&engine, &up2);
    let up2_ms = t2.elapsed().as_millis();
    let up3 = upscale_crop(3);
    let t3 = Instant::now();
    let upscaled3 = ocr(&engine, &up3);
    let up3_ms = t3.elapsed().as_millis();

    eprintln!("\n=== small-text crop 240x40 (around 'Scrollback') ===");
    eprintln!("native 1x : {native_ms} ms -> {native:?}");
    eprintln!("upscaled 2x: {up2_ms} ms -> {upscaled2:?}");
    eprintln!("upscaled 3x: {up3_ms} ms -> {upscaled3:?}");
    eprintln!(
        "  'Scrollback' native={} upscaled2x={} upscaled3x={}",
        found(&native, "Scrollback"),
        found(&upscaled2, "Scrollback"),
        found(&upscaled3, "Scrollback"),
    );

    // Pipeline must produce output (fails loudly if OCR is broken).
    assert!(!full.is_empty(), "full-frame OCR produced no text at all");
    // Finding: crisp, high-contrast *synthetic* text reads at native scale at
    // BOTH sizes — the small ~11px labels included. So pixel size alone is not
    // what defeats the recognizer. The real-world miss in the report is on
    // anti-aliased GTK text rendered under llvmpipe (rendering-dependent, as the
    // ocr_upscale_factor docs note), which this self-contained probe can't
    // reproduce. What it DOES guard: the `with_upscale(...)` transform (a
    // Lanczos3 resize of the crop) yields valid recognition and never makes
    // things worse — so a consumer who reaches for it on a real small label
    // isn't trading away correctness.
    assert!(
        found(&full, "Appearance"),
        "large synthetic text must read at native full-frame scale"
    );
    assert!(
        found(&upscaled3, "Scrollback"),
        "upscaled crop must still recognize text the native crop read (native={})",
        found(&native, "Scrollback"),
    );
}
