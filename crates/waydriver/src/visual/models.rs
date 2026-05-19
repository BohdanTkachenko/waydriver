//! Resolution of the two `.rten` files ocrs needs to run: detection (which
//! parts of the image are text) and recognition (what those words say).
//!
//! Lookup order:
//!
//! 1. **Env-var override** — both `WAYDRIVER_OCRS_DETECTION_MODEL` and
//!    `WAYDRIVER_OCRS_RECOGNITION_MODEL` set. The caller is responsible
//!    for the files existing. **SHA-256 is NOT verified for env-var
//!    overrides** — the user has explicitly pointed us at a file they
//!    control.
//! 2. **XDG cache hit** — both files already present at
//!    `$XDG_CACHE_HOME/waydriver/ocrs-models/{text-detection,text-recognition}.rten`
//!    (falls back to `$HOME/.cache/...` if `XDG_CACHE_HOME` is unset).
//!    The cached file's SHA-256 is verified against the constants
//!    below; on mismatch the file is treated as corrupted and
//!    re-downloaded.
//! 3. **Auto-download** — fetch from ocrs's published S3 bucket into
//!    the XDG cache directory. The download URL matches what ocrs-cli
//!    uses by default, so the binary content is identical to what an
//!    out-of-band `ocrs` install would have downloaded. The downloaded
//!    bytes are hashed before the `.partial → .rten` rename so a
//!    corrupted download never becomes a cache hit.
//!
//! ## Bumping the hashes
//!
//! If ocrs publishes new model files (these constants will then refuse
//! to load the cached files), capture new hashes by:
//!
//! ```sh
//! curl -sL https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten | sha256sum
//! curl -sL https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten | sha256sum
//! ```
//!
//! and update `DETECTION_SHA256` / `RECOGNITION_SHA256` accordingly.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

const DETECTION_URL: &str = "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten";
const RECOGNITION_URL: &str =
    "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten";

const DETECTION_FILENAME: &str = "text-detection.rten";
const RECOGNITION_FILENAME: &str = "text-recognition.rten";

const DETECTION_ENV: &str = "WAYDRIVER_OCRS_DETECTION_MODEL";
const RECOGNITION_ENV: &str = "WAYDRIVER_OCRS_RECOGNITION_MODEL";

/// Expected SHA-256 of the `text-detection.rten` file as published on
/// ocrs's S3 bucket. Captured 2026-05-16. Bump when upstream rebuilds
/// the model (see module docs).
const DETECTION_SHA256: &str = "f15cfb56bd02c4bf478a20343986504a1f01e1665c2b3a0ad66340f054b1b5ca";
/// Expected SHA-256 of the `text-recognition.rten` file. Captured
/// 2026-05-16. Bump when upstream rebuilds.
const RECOGNITION_SHA256: &str = "e484866d4cce403175bd8d00b128feb08ab42e208de30e42cd9889d8f1735a6e";

/// Resolve both `.rten` paths, downloading them if needed. Blocking I/O —
/// call from `spawn_blocking` rather than directly on the runtime.
///
/// Returns `(detection_path, recognition_path)`.
pub(super) fn ensure_models() -> Result<(PathBuf, PathBuf)> {
    if let (Ok(det), Ok(rec)) = (std::env::var(DETECTION_ENV), std::env::var(RECOGNITION_ENV)) {
        tracing::debug!(
            detection = %det, recognition = %rec,
            "visual: using ocrs model paths from env vars (SHA-256 verification skipped)"
        );
        return Ok((PathBuf::from(det), PathBuf::from(rec)));
    }

    let cache_dir = waydriver_cache_dir()?;
    let detection_path = cache_dir.join(DETECTION_FILENAME);
    let recognition_path = cache_dir.join(RECOGNITION_FILENAME);

    ensure_one_model(&cache_dir, &detection_path, DETECTION_URL, DETECTION_SHA256)?;
    ensure_one_model(
        &cache_dir,
        &recognition_path,
        RECOGNITION_URL,
        RECOGNITION_SHA256,
    )?;

    Ok((detection_path, recognition_path))
}

/// Resolve a single model file, downloading + verifying as needed.
///
/// - If the file exists, verify its SHA-256. On mismatch (corrupted /
///   stale cache), delete and re-download.
/// - If the file doesn't exist, download to a `*.partial` sibling,
///   hash, and only rename to the final name when the hash matches.
fn ensure_one_model(
    cache_dir: &std::path::Path,
    dest: &std::path::Path,
    url: &str,
    expected_sha256: &str,
) -> Result<()> {
    if dest.exists() {
        match verify_sha256(dest, expected_sha256) {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    path = %dest.display(),
                    err = %e,
                    "visual: cached model failed SHA-256 verification, re-downloading"
                );
                let _ = std::fs::remove_file(dest);
            }
        }
    }
    std::fs::create_dir_all(cache_dir).map_err(|e| {
        Error::visual(format!(
            "failed to create cache dir {}: {e}",
            cache_dir.display()
        ))
    })?;
    download_to(url, dest, expected_sha256)
}

/// Read `path` and verify its SHA-256 matches `expected_hex` (32-byte
/// hash hex-encoded, lower-case). Returns `Ok(())` on match, an
/// `Error::Visual` on mismatch or I/O failure.
pub(super) fn verify_sha256(path: &std::path::Path, expected_hex: &str) -> Result<()> {
    let mut f = std::fs::File::open(path).map_err(|e| {
        Error::visual(format!(
            "failed to open {} for SHA-256 verification: {e}",
            path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| {
            Error::visual(format!(
                "read error during SHA-256 of {}: {e}",
                path.display()
            ))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = hex::encode(hasher.finalize());
    if got.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        Err(Error::visual(format!(
            "SHA-256 mismatch for {}: expected {}, got {}. If upstream rebuilt the ocrs model files, \
             bump DETECTION_SHA256 / RECOGNITION_SHA256 in models.rs, or set \
             WAYDRIVER_OCRS_DETECTION_MODEL / WAYDRIVER_OCRS_RECOGNITION_MODEL to point at known-good files.",
            path.display(),
            expected_hex,
            got,
        )))
    }
}

/// `$XDG_CACHE_HOME/waydriver/ocrs-models/`, falling back to
/// `$HOME/.cache/waydriver/ocrs-models/`.
fn waydriver_cache_dir() -> Result<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache")
    } else {
        return Err(Error::visual(
            "neither XDG_CACHE_HOME nor HOME is set; cannot locate cache directory \
             for ocrs model files. Set WAYDRIVER_OCRS_DETECTION_MODEL and \
             WAYDRIVER_OCRS_RECOGNITION_MODEL to override.",
        ));
    };
    Ok(base.join("waydriver").join("ocrs-models"))
}

/// Stream `url` to `dest_path` via ureq, hash on the fly, and refuse
/// to rename `*.partial → *.rten` if the SHA-256 doesn't match
/// `expected_hex`. A corrupted download therefore never becomes a
/// cache hit.
///
/// Writes to a `*.partial` sibling first so a canceled or crashed
/// download doesn't leave a half-written file at `dest_path`.
fn download_to(url: &str, dest_path: &std::path::Path, expected_hex: &str) -> Result<()> {
    let partial = dest_path.with_extension("rten.partial");

    tracing::info!(%url, dest = %dest_path.display(), "visual: downloading ocrs model");
    let started = std::time::Instant::now();

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .new_agent();

    let response = agent
        .get(url)
        .call()
        .map_err(|e| Error::visual(format!("failed to fetch {url}: {e}")))?;

    let mut out = std::fs::File::create(&partial)
        .map_err(|e| Error::visual(format!("failed to create {}: {e}", partial.display())))?;
    let mut reader = response.into_body().into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    let mut hasher = Sha256::new();
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| Error::visual(format!("read error from {url}: {e}")))?;
        if n == 0 {
            break;
        }
        use std::io::Write;
        out.write_all(&buf[..n])
            .map_err(|e| Error::visual(format!("write error: {e}")))?;
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    drop(out);

    let got = hex::encode(hasher.finalize());
    if !got.eq_ignore_ascii_case(expected_hex) {
        let _ = std::fs::remove_file(&partial);
        return Err(Error::visual(format!(
            "downloaded {url} but SHA-256 didn't match: expected {expected_hex}, got {got}. \
             If upstream rebuilt the model files, bump the SHA-256 constants in \
             crates/waydriver/src/visual/models.rs."
        )));
    }

    std::fs::rename(&partial, dest_path).map_err(|e| {
        Error::visual(format!(
            "failed to rename {} → {}: {e}",
            partial.display(),
            dest_path.display()
        ))
    })?;

    tracing::info!(
        bytes = total,
        elapsed_ms = started.elapsed().as_millis(),
        dest = %dest_path.display(),
        "visual: ocrs model download complete"
    );
    Ok(())
}
