//! Mock D-Bus services that capture an app's "external effects".
//!
//! Some things an app does have **no AT-SPI projection** because they leave the
//! process entirely onto the session D-Bus: it posts a desktop notification, or
//! asks the portal to open a URI in a browser. There's no widget to query and no
//! stdout line to wait on — the effect is a method call to a *daemon* the app
//! expects to be present on the bus.
//!
//! [`ExternalSinks`] stands in for those daemons. It connects to the app's
//! session bus (the same bus AT-SPI and the app share — see
//! [`crate::session`]), serves stub implementations of the relevant interfaces,
//! and records every call so a test can assert on it:
//!
//! - `org.freedesktop.Notifications` — captures `Notify` (the freedesktop
//!   notification spec; what libnotify and most apps call).
//! - `org.freedesktop.portal.Desktop` `org.freedesktop.portal.OpenURI` —
//!   captures `OpenURI` and answers the portal Request/Response handshake so
//!   real callers (e.g. `GtkUriLauncher`) complete cleanly.
//!
//! This is opt-in (see [`crate::session::SessionConfig::capture_external_effects`]):
//! the sinks own well-known names, which is only safe when nothing else owns
//! them. On the per-session / container bus that's always the case; on a shared
//! host bus where a real notification daemon or portal is already running, the
//! name claim no-ops with a warning and capture for that interface stays empty.
//!
//! Not mocked (out of scope for now): the portal `OpenFile` (file-descriptor)
//! path, and the `org.freedesktop.portal.Notification` interface that GLib uses
//! when `GTK_USE_PORTAL=1` reroutes notifications away from the freedesktop one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use zbus::message::Header;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{interface, Connection, ObjectServer};

use crate::error::{Error, Result};

const NOTIFICATIONS_NAME: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

/// A captured `org.freedesktop.Notifications.Notify` call — one desktop
/// notification the app posted.
#[derive(Debug, Clone)]
pub struct CapturedNotification {
    /// Monotonic index within this session's notification log (0-based).
    pub seq: u64,
    /// Sending application's name (the `app_name` Notify argument).
    pub app_name: String,
    /// `replaces_id` — the id of a prior notification this one replaces (0 = new).
    pub replaces_id: u32,
    /// Icon name / path (the `app_icon` argument).
    pub app_icon: String,
    /// Single-line summary / title.
    pub summary: String,
    /// Body text.
    pub body: String,
    /// Action id/label pairs as sent (flat list, spec ordering).
    pub actions: Vec<String>,
    /// Hints rendered as `"key=value"` strings, sorted. The wire type is
    /// `a{sv}`, awkward to clone and inspect; the rendered form carries enough
    /// for assertions (e.g. `urgency=...`, `category=...`).
    pub hints: Vec<String>,
    /// Expiry timeout in ms (`-1` = server default, `0` = never).
    pub expire_timeout: i32,
    /// The notification id the mock returned to the caller.
    pub id: u32,
}

/// A captured `org.freedesktop.portal.OpenURI.OpenURI` request — one URI the
/// app asked the portal to open externally.
#[derive(Debug, Clone)]
pub struct CapturedOpenUri {
    /// Monotonic index within this session's open-URI log (0-based).
    pub seq: u64,
    /// The `parent_window` identifier the caller passed (often empty headless).
    pub parent_window: String,
    /// The requested URI.
    pub uri: String,
    /// Options rendered as `"key=value"` strings, sorted (e.g. `handle_token=...`).
    pub options: Vec<String>,
}

/// A monotonic, append-only capture log with a wakeup [`Notify`] so a waiter can
/// block until a matching entry arrives. Mirrors `AppStdout` in
/// [`crate::session`].
struct Records<T> {
    items: Mutex<Vec<T>>,
    notify: Notify,
    seq: AtomicU64,
}

impl<T> Default for Records<T> {
    fn default() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
            notify: Notify::new(),
            seq: AtomicU64::new(0),
        }
    }
}

impl<T: Clone> Records<T> {
    /// Append an entry built from the next sequence number, then wake waiters.
    fn push(&self, make: impl FnOnce(u64) -> T) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.items.lock().unwrap().push(make(seq));
        self.notify.notify_waiters();
    }

    fn snapshot(&self) -> Vec<T> {
        self.items.lock().unwrap().clone()
    }

    fn len(&self) -> usize {
        self.items.lock().unwrap().len()
    }

    /// Wait for an entry at or after index `after` that satisfies `pred`.
    /// Mirrors [`crate::Session::wait_for_stdout_line`]: returns
    /// [`Error::Timeout`] on the deadline and [`Error::Cancelled`] when
    /// `cancel` fires.
    async fn wait_for<F>(
        &self,
        after: usize,
        pred: F,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<T>
    where
        F: Fn(&T) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Register for the wakeup before scanning so an entry appended
            // between the scan and the wait isn't missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);

            {
                let guard = self.items.lock().unwrap();
                if let Some(found) = guard.iter().skip(after).find(|i| pred(i)) {
                    return Ok(found.clone());
                }
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout(format!(
                    "no captured entry matched within {timeout:?}"
                )));
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(remaining) => {
                    return Err(Error::Timeout(format!(
                        "no captured entry matched within {timeout:?}"
                    )));
                }
                _ = cancel.cancelled() => return Err(Error::Cancelled),
            }
        }
    }
}

/// Render a D-Bus dict (`a{sv}`) into a stable, sorted `Vec<"key=value">` for
/// human-readable assertions and JSON readback.
fn render_dict(dict: &HashMap<String, OwnedValue>) -> Vec<String> {
    let mut rendered: Vec<String> = dict.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
    rendered.sort();
    rendered
}

/// Derive the portal request-handle path the way xdg-desktop-portal does:
/// `/org/freedesktop/portal/desktop/request/<SENDER_ID>/<TOKEN>`, where
/// `SENDER_ID` is the caller's unique bus name with the leading `:` dropped and
/// every `.` turned into `_`. An empty sender (peer connection) falls back to a
/// constant so the path stays a valid object path.
fn request_handle_path(sender: &str, token: &str) -> String {
    let mut sender_id = sender.trim_start_matches(':').replace('.', "_");
    if sender_id.is_empty() {
        sender_id = "wd".to_string();
    }
    format!("/org/freedesktop/portal/desktop/request/{sender_id}/{token}")
}

/// `org.freedesktop.Notifications` stub. Records every `Notify`.
struct NotificationsIface {
    records: Arc<Records<CapturedNotification>>,
    next_id: AtomicU32,
}

#[interface(name = "org.freedesktop.Notifications")]
impl NotificationsIface {
    /// `Notify(app_name, replaces_id, app_icon, summary, body, actions, hints, expire_timeout) -> u32`
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        // Honor replaces_id; otherwise hand out a fresh id starting at 1.
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next_id.fetch_add(1, Ordering::Relaxed)
        };
        let hints = render_dict(&hints);
        self.records.push(|seq| CapturedNotification {
            seq,
            app_name,
            replaces_id,
            app_icon,
            summary,
            body,
            actions,
            hints,
            expire_timeout,
            id,
        });
        id
    }

    /// `CloseNotification(id)` — no-op; the mock never expires notifications.
    fn close_notification(&self, _id: u32) {}

    /// `GetCapabilities() -> as`
    fn get_capabilities(&self) -> Vec<String> {
        vec![
            "body".to_string(),
            "body-markup".to_string(),
            "actions".to_string(),
        ]
    }

    /// `GetServerInformation() -> (name, vendor, version, spec_version)`
    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "waydriver".to_string(),
            "waydriver".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            "1.2".to_string(),
        )
    }
}

/// A minimal `org.freedesktop.portal.Request` object. The portal contract hands
/// the caller a request handle it may `Close` to cancel; the mock answers every
/// request immediately via the `Response` signal, so `Close` is a no-op.
struct PortalRequest;

#[interface(name = "org.freedesktop.portal.Request")]
impl PortalRequest {
    fn close(&self) {}
}

/// `org.freedesktop.portal.OpenURI` stub (served under the
/// `org.freedesktop.portal.Desktop` name). Records every `OpenURI`.
struct OpenUriIface {
    records: Arc<Records<CapturedOpenUri>>,
    /// Fallback token source when the caller doesn't supply a `handle_token`.
    token_counter: AtomicU64,
}

#[interface(name = "org.freedesktop.portal.OpenURI")]
impl OpenUriIface {
    /// Portal interface version. Lowercase `version` per the portal spec (zbus
    /// would otherwise PascalCase it).
    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        4
    }

    /// `OpenURI(parent_window: s, uri: s, options: a{sv}) -> handle: o`
    ///
    /// Renamed explicitly: PascalCasing `open_uri` yields `OpenUri`, but the
    /// spec method is `OpenURI`.
    #[zbus(name = "OpenURI")]
    async fn open_uri(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &Connection,
        #[zbus(object_server)] server: &ObjectServer,
        parent_window: String,
        uri: String,
        options: HashMap<String, OwnedValue>,
    ) -> OwnedObjectPath {
        let sender = header
            .sender()
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        // Honor a caller-supplied handle_token so a client that pre-subscribed
        // to Response on the computed path receives it.
        let token = options
            .get("handle_token")
            .and_then(|v| String::try_from(v.clone()).ok())
            .unwrap_or_else(|| format!("wd{}", self.token_counter.fetch_add(1, Ordering::Relaxed)));
        let handle = request_handle_path(&sender, &token);

        let rendered = render_dict(&options);
        self.records.push(|seq| CapturedOpenUri {
            seq,
            parent_window,
            uri,
            options: rendered,
        });

        let path = OwnedObjectPath::try_from(handle.clone()).unwrap_or_else(|e| {
            tracing::warn!(error = %e, handle, "portal: invalid request handle path; using fallback");
            OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/request/wd/wd")
                .expect("static fallback path is valid")
        });

        // Register a Request object so a caller's Close resolves, then answer
        // immediately with Response(success, {}). The caller pre-matched on this
        // path (from its own handle_token), so the signal is delivered even
        // though we emit it before returning the handle.
        let _ = server.at(&path, PortalRequest).await;
        let results: HashMap<String, OwnedValue> = HashMap::new();
        let dest = (!sender.is_empty()).then_some(sender.as_str());
        if let Err(e) = conn
            .emit_signal(
                dest,
                &path,
                "org.freedesktop.portal.Request",
                "Response",
                &(0u32, results),
            )
            .await
        {
            tracing::warn!(error = %e, "portal: failed to emit Response signal");
        }

        path
    }
}

/// Mock D-Bus services that own well-known names on the app's session bus and
/// capture the effects the app emits there. See the [module docs](self).
pub struct ExternalSinks {
    /// Dedicated connection to the app's session bus. Dropping it releases the
    /// owned names and unregisters the served objects. This bus is the app's
    /// (host/per-session) bus — independent of the compositor's *private* bus —
    /// so the connection has no place in the session shutdown ordering.
    _conn: Connection,
    notifications: Arc<Records<CapturedNotification>>,
    open_uris: Arc<Records<CapturedOpenUri>>,
}

impl ExternalSinks {
    /// Connect to `dbus_address`, register the mock interfaces, and best-effort
    /// claim their well-known names.
    ///
    /// Returns an error only for hard setup failures (bad address, failed
    /// connect, object registration). A *name conflict* — a real daemon already
    /// owns the name on a shared host bus — is logged and tolerated: the
    /// connection still works, capture for that interface simply stays empty.
    pub async fn start(dbus_address: &str) -> Result<Self> {
        let address: zbus::address::Address =
            dbus_address.try_into().map_err(|e: zbus::Error| {
                Error::process_with("external sinks: invalid dbus address", e)
            })?;
        let conn = zbus::connection::Builder::address(address)?.build().await?;

        let notifications: Arc<Records<CapturedNotification>> = Arc::new(Records::default());
        let open_uris: Arc<Records<CapturedOpenUri>> = Arc::new(Records::default());

        // Register objects before claiming names so no early method call is
        // lost (zbus warns otherwise). Accessing object_server() also starts
        // the incoming-call dispatch task.
        conn.object_server()
            .at(
                NOTIFICATIONS_PATH,
                NotificationsIface {
                    records: notifications.clone(),
                    next_id: AtomicU32::new(1),
                },
            )
            .await?;
        conn.object_server()
            .at(
                PORTAL_PATH,
                OpenUriIface {
                    records: open_uris.clone(),
                    token_counter: AtomicU64::new(1),
                },
            )
            .await?;

        request_name_best_effort(&conn, NOTIFICATIONS_NAME).await;
        request_name_best_effort(&conn, PORTAL_NAME).await;

        Ok(Self {
            _conn: conn,
            notifications,
            open_uris,
        })
    }

    /// Snapshot of every notification captured so far.
    pub fn notifications(&self) -> Vec<CapturedNotification> {
        self.notifications.snapshot()
    }

    /// Snapshot of every open-URI request captured so far.
    pub fn open_uri_requests(&self) -> Vec<CapturedOpenUri> {
        self.open_uris.snapshot()
    }

    /// Current notification-log length — a high-water mark to pass as `after`.
    pub fn notification_count(&self) -> usize {
        self.notifications.len()
    }

    /// Current open-URI-log length — a high-water mark to pass as `after`.
    pub fn open_uri_count(&self) -> usize {
        self.open_uris.len()
    }

    /// Wait for a captured notification at/after `after` matching `pred`.
    pub async fn wait_for_notification<F>(
        &self,
        after: usize,
        pred: F,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<CapturedNotification>
    where
        F: Fn(&CapturedNotification) -> bool,
    {
        self.notifications
            .wait_for(after, pred, timeout, cancel)
            .await
    }

    /// Wait for a captured open-URI request at/after `after` matching `pred`.
    pub async fn wait_for_open_uri<F>(
        &self,
        after: usize,
        pred: F,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<CapturedOpenUri>
    where
        F: Fn(&CapturedOpenUri) -> bool,
    {
        self.open_uris.wait_for(after, pred, timeout, cancel).await
    }
}

/// Claim `name` without queueing or replacing an existing owner. A non-primary
/// reply (someone else owns it) or an error is logged and swallowed — capture
/// for that interface is simply inactive.
async fn request_name_best_effort(conn: &Connection, name: &str) {
    use zbus::fdo::{RequestNameFlags, RequestNameReply};
    match conn
        .request_name_with_flags(name, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner) => {
            tracing::info!(name, "external sink claimed bus name");
        }
        Ok(other) => {
            tracing::warn!(
                name,
                reply = %other,
                "external sink could not claim bus name (already owned?); \
                 capture for this interface will be inactive"
            );
        }
        Err(e) => {
            tracing::warn!(
                name,
                error = %e,
                "external sink RequestName failed; capture for this interface will be inactive"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    #[test]
    fn records_push_assigns_monotonic_seq_and_snapshots() {
        let r: Records<u64> = Records::default();
        assert_eq!(r.len(), 0);
        r.push(|seq| seq);
        r.push(|seq| seq * 10);
        assert_eq!(r.snapshot(), vec![0, 10]);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn request_handle_path_strips_unique_name() {
        assert_eq!(
            request_handle_path(":1.42", "tok"),
            "/org/freedesktop/portal/desktop/request/1_42/tok"
        );
        // Empty sender falls back so the path stays valid.
        assert_eq!(
            request_handle_path("", "tok"),
            "/org/freedesktop/portal/desktop/request/wd/tok"
        );
    }

    #[test]
    fn render_dict_is_sorted_key_value() {
        let mut d: HashMap<String, OwnedValue> = HashMap::new();
        d.insert(
            "zeta".to_string(),
            OwnedValue::try_from(Value::from(2u32)).unwrap(),
        );
        d.insert(
            "alpha".to_string(),
            OwnedValue::try_from(Value::from(1u32)).unwrap(),
        );
        let r = render_dict(&d);
        assert_eq!(r.len(), 2);
        assert!(r[0].starts_with("alpha="), "got {r:?}");
        assert!(r[1].starts_with("zeta="), "got {r:?}");
    }

    #[tokio::test]
    async fn wait_for_returns_existing_match() {
        let r: Records<u64> = Records::default();
        r.push(|_| 7);
        let token = CancellationToken::new();
        let got = r
            .wait_for(0, |v| *v == 7, Duration::from_secs(1), &token)
            .await
            .unwrap();
        assert_eq!(got, 7);
    }

    #[tokio::test]
    async fn wait_for_times_out_without_match() {
        let r: Records<u64> = Records::default();
        let token = CancellationToken::new();
        let res = r
            .wait_for(0, |_| false, Duration::from_millis(50), &token)
            .await;
        assert!(matches!(res, Err(Error::Timeout(_))));
    }

    #[tokio::test]
    async fn wait_for_wakes_on_push() {
        let r: Arc<Records<u64>> = Arc::new(Records::default());
        let r2 = r.clone();
        let token = CancellationToken::new();
        let waiter = tokio::spawn(async move {
            r2.wait_for(
                0,
                |v| *v == 99,
                Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .await
        });
        // Give the waiter a moment to register, then push.
        tokio::time::sleep(Duration::from_millis(20)).await;
        r.push(|_| 99);
        let got = waiter.await.unwrap().unwrap();
        assert_eq!(got, 99);
        let _ = token; // token unused beyond construction in this case
    }

    #[tokio::test]
    async fn wait_for_cancels() {
        let r: Records<u64> = Records::default();
        let token = CancellationToken::new();
        token.cancel();
        let res = r
            .wait_for(0, |_| false, Duration::from_secs(5), &token)
            .await;
        assert!(matches!(res, Err(Error::Cancelled)));
    }
}
