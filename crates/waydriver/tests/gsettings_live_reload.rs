//! Proof that [`waydriver::gsettings::live_write`] reaches an
//! **already-running** GIO consumer — the core mechanism behind
//! `Session::set_setting` (issue #29's live-GSettings capability).
//!
//! `gsettings monitor` stands in for a GTK app: it uses the identical GIO
//! keyfile backend, so if an in-place keyfile rewrite makes the CLI report the
//! new value, a real app's GSettings `changed` handler fires the same way. This
//! needs no GTK fixture, so it runs anywhere the `gsettings` CLI and the
//! `org.gnome.desktop.interface` schema are installed.
//!
//! Gated behind `#[ignore]` because it spawns the `gsettings` CLI and depends
//! on the desktop schemas being present; run with:
//!
//! ```sh
//! cargo test -p waydriver --test gsettings_live_reload -- --ignored --nocapture
//! ```

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use waydriver::gsettings::{config_dir, live_write, write_keyfile, GSettingEntry, KEYFILE_BACKEND};

const SCHEMA: &str = "org.gnome.desktop.interface";
const KEY: &str = "text-scaling-factor";

#[test]
#[ignore = "spawns the gsettings CLI; run with --ignored in an env with gsettings + desktop schemas"]
fn live_write_fires_gsettings_changed_on_running_consumer() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Seed the keyfile exactly as the compositor does at launch.
    write_keyfile(dir.path(), &[GSettingEntry::new(SCHEMA, KEY, "1.0")]).expect("seed keyfile");

    // `gsettings monitor` prints "<key>: <value>" whenever the key changes.
    // Point it at the per-session store with the same two env vars the app
    // inherits (`GSETTINGS_BACKEND=keyfile` + `XDG_CONFIG_HOME=<cfg>`).
    let mut child = Command::new("gsettings")
        .args(["monitor", SCHEMA, KEY])
        .env("GSETTINGS_BACKEND", KEYFILE_BACKEND)
        .env("XDG_CONFIG_HOME", config_dir(dir.path()))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn `gsettings monitor` — is the gsettings CLI installed?");

    // Pump the child's stdout lines over a channel so the main thread can wait
    // with a timeout instead of blocking forever if nothing is emitted.
    let stdout = child.stdout.take().expect("child stdout piped");
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // Give the monitor a moment to install its GFileMonitor watch before we
    // change the file — otherwise the edit can race ahead of the watch.
    thread::sleep(Duration::from_millis(1000));

    // The live write: an in-place rewrite of the same keyfile, post-launch.
    live_write(dir.path(), &GSettingEntry::new(SCHEMA, KEY, "2.0")).expect("live_write");

    // Expect the monitor to report the new value within a few seconds.
    let deadline = Duration::from_secs(5);
    let start = Instant::now();
    let mut observed = None;
    while start.elapsed() < deadline {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(line) if line.contains("2.0") => {
                observed = Some(line);
                break;
            }
            Ok(_) => continue, // unrelated line; keep waiting
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Tear the monitor down before asserting so a failure still cleans up.
    let _ = child.kill();
    let _ = child.wait();
    drop(rx);
    let _ = reader.join();

    let observed = observed.expect(
        "gsettings monitor never reported text-scaling-factor=2.0 — an in-place keyfile rewrite \
         should make GIO's keyfile backend re-emit `changed` to the running consumer",
    );
    assert!(
        observed.contains("2.0"),
        "monitor line should carry the new value, got: {observed:?}"
    );
}
