//! Integration test for the mock external-effect sinks (`waydriver::sink`).
//!
//! Gated `#[ignore]`: it spawns a private `dbus-daemon` and exercises the real
//! D-Bus path — no GTK, no mutter. Run with:
//!
//! ```sh
//! cargo test -p waydriver --test external_sinks -- --ignored --nocapture
//! ```
//!
//! It proves, against a real bus, that:
//! - `org.freedesktop.Notifications.Notify` is captured and returns an id, and
//! - `org.freedesktop.portal.OpenURI.OpenURI` is captured, returns the expected
//!   request-handle path, and delivers the portal `Response` signal to a
//!   pre-subscribed caller (the handshake `GtkUriLauncher` relies on).

use std::collections::HashMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;
use waydriver::sink::ExternalSinks;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// Spawn a private session `dbus-daemon` at `<dir>/bus` and return its address
/// plus the child handle (killed on drop by the caller). Mirrors how
/// `waydriver-compositor-mutter` launches its per-session bus.
fn spawn_dbus_daemon(dir: &Path) -> (String, Child) {
    let socket = dir.join("bus");
    let address = format!("unix:path={}", socket.display());
    let child = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--nopidfile"])
        .arg(format!("--address={address}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dbus-daemon — is it installed?");

    // Wait for the socket to appear.
    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(socket.exists(), "dbus-daemon socket never appeared");
    (address, child)
}

#[tokio::test]
#[ignore = "spawns a private dbus-daemon; run with --ignored"]
async fn captures_notification_and_open_uri() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (address, mut daemon) = spawn_dbus_daemon(dir.path());

    // The mock services own the well-known names on this fresh private bus.
    let sinks = ExternalSinks::start(&address)
        .await
        .expect("start external sinks");

    // A separate client connection plays the role of the app under test.
    let client = zbus::connection::Builder::address(address.as_str())
        .expect("address")
        .build()
        .await
        .expect("client connect");

    // ── Notifications ────────────────────────────────────────────────────
    let empty_hints: HashMap<String, OwnedValue> = HashMap::new();
    let reply = client
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "waydriver-test",
                0u32,
                "dialog-information",
                "summary-x",
                "body-y",
                Vec::<String>::new(),
                empty_hints,
                -1i32,
            ),
        )
        .await
        .expect("Notify call");
    let id: u32 = reply.body().deserialize().expect("deserialize id");
    assert_eq!(id, 1, "first notification id should be 1");

    let captured = sinks.notifications();
    assert_eq!(captured.len(), 1, "exactly one notification captured");
    let n = &captured[0];
    assert_eq!(n.app_name, "waydriver-test");
    assert_eq!(n.summary, "summary-x");
    assert_eq!(n.body, "body-y");
    assert_eq!(n.expire_timeout, -1);
    assert_eq!(n.id, 1);

    // ── Portal OpenURI (with the Request/Response handshake) ─────────────
    let handle_token = "wdtest1";
    let unique = client
        .unique_name()
        .expect("client has a unique name")
        .as_str()
        .to_string();
    let expected_path = format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        unique.trim_start_matches(':').replace('.', "_"),
        handle_token
    );

    // Subscribe to the Response signal *before* calling, on the path we expect.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.portal.Request")
        .unwrap()
        .member("Response")
        .unwrap()
        .path(expected_path.as_str())
        .unwrap()
        .build();
    let mut responses = zbus::MessageStream::for_match_rule(rule, &client, None)
        .await
        .expect("subscribe Response");

    let mut options: HashMap<String, OwnedValue> = HashMap::new();
    options.insert(
        "handle_token".to_string(),
        OwnedValue::try_from(Value::from(handle_token)).unwrap(),
    );
    let reply = client
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.OpenURI"),
            "OpenURI",
            &("", "https://example.com/waydriver", options),
        )
        .await
        .expect("OpenURI call");
    let handle: OwnedObjectPath = reply.body().deserialize().expect("deserialize handle");
    assert_eq!(handle.as_str(), expected_path, "handle path matches token");

    let open = sinks.open_uri_requests();
    assert_eq!(open.len(), 1, "exactly one open-uri captured");
    assert_eq!(open[0].uri, "https://example.com/waydriver");

    // The Response signal must reach the pre-subscribed caller.
    let sig = tokio::time::timeout(Duration::from_secs(5), responses.next())
        .await
        .expect("Response within 5s")
        .expect("stream item")
        .expect("valid message");
    let (response, _results): (u32, HashMap<String, OwnedValue>) =
        sig.body().deserialize().expect("deserialize Response");
    assert_eq!(response, 0, "portal Response is success (0)");

    // ── wait_for_* helpers see prior + future entries ────────────────────
    let token = CancellationToken::new();
    let found = sinks
        .wait_for_notification(
            0,
            |n| n.summary == "summary-x",
            Duration::from_secs(1),
            &token,
        )
        .await
        .expect("wait_for_notification finds the existing entry");
    assert_eq!(found.id, 1);

    drop(client);
    drop(sinks);
    let _ = daemon.kill();
    let _ = daemon.wait();
}
