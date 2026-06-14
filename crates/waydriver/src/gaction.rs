//! Driving GTK applications through their exported `org.gtk.Actions`
//! D-Bus interface — a surface distinct from AT-SPI.
//!
//! GTK4/libadwaita lazily-realized menu surfaces (`GtkPopoverMenu` items —
//! primary/context/tab menus —, `AdwTabOverview`, and some dialog bodies)
//! never enter the `GetChildren` snapshot tree **or** `Cache.GetItems`, so
//! there is no client-side AT-SPI handle to read or activate them. Their
//! only effective role is "fire a GAction".
//!
//! `GApplication` and `GtkApplicationWindow` export their action groups over
//! the **`org.gtk.Actions`** interface on the application's *own* well-known
//! bus name on the **session bus** — a different bus and addressing model
//! than the a11y bus [`crate::atspi`] talks to. This module connects to that
//! surface and invokes `app.*` / `win.*` actions so a test can drive a
//! GAction-only item and observe the side effect (e.g. via
//! [`crate::Session::wait_for_stdout_line`]).
//!
//! ## Addressing
//!
//! The session bus is shared (parallel sessions are disambiguated by unique
//! GApplication id), so the app is located by **matching the spawned process
//! PID** to the owner of a well-known bus name — the GApplication owns its id
//! as a name whose owning-connection PID is the app's. The object path is
//! then derived the way GLib does
//! (`g_application_id_get_default_dbus_object_path`): `"/" + appid` with `.`
//! mapped to `/` and `-` to `_`. App actions export at that base path; each
//! `GtkApplicationWindow`'s actions export at `<base>/window/<N>`.
//!
//! This is a **test affordance**, not a general locator: firing an action
//! needs out-of-band knowledge of the action name (an app-internal contract
//! that `org.gtk.Actions.DescribeAll` does not map to visible labels).

use std::collections::HashMap;

use zbus::zvariant::Value;

use crate::error::{Error, Result};

/// The interface GTK exports action groups on.
const GTK_ACTIONS_IFACE: &str = "org.gtk.Actions";

/// How many times [`discover_application`] re-scans the bus before giving
/// up. Discovery normally succeeds on the first pass (the app owns its name
/// well before its AT-SPI tree — which `wait_for_app` already waited for —
/// is published), but a couple of retries absorb a slow name handoff on a
/// loaded host.
const DISCOVERY_ATTEMPTS: usize = 5;

/// Delay between [`discover_application`] scans.
const DISCOVERY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// A GTK application's exported `org.gtk.Actions` location on the session bus.
#[derive(Debug, Clone)]
pub struct GtkApplicationAddress {
    /// The well-known bus name the app owns (its GApplication id).
    pub bus_name: String,
    /// Base object path: `"/" + bus_name` with `.`→`/` and `-`→`_`.
    pub base_path: String,
}

/// A live session-bus connection plus the discovered application address.
/// Cached on the [`crate::Session`] after the first GAction call.
pub struct GtkActionsTarget {
    pub conn: zbus::Connection,
    pub addr: GtkApplicationAddress,
}

/// A parsed prefixed action name: `group.action` or `group.action::target`.
struct ParsedAction {
    /// `"app"` or `"win"`.
    group: String,
    /// The action name without its group prefix.
    name: String,
    /// String target from the `::target` detailed-name suffix, if present.
    target: Option<String>,
}

/// Connect to the session bus at `dbus_address` — the same bus the app was
/// spawned against, where it exports `org.gtk.Actions`.
pub async fn connect_session_bus(dbus_address: &str) -> Result<zbus::Connection> {
    let addr: zbus::Address = dbus_address.try_into().map_err(|e: zbus::Error| {
        Error::atspi_with("org.gtk.Actions: invalid session bus address", e)
    })?;
    zbus::connection::Builder::address(addr)
        .map_err(|e| Error::atspi_with("org.gtk.Actions: session bus builder", e))?
        .build()
        .await
        .map_err(|e| Error::atspi_with("org.gtk.Actions: connect session bus", e))
}

/// Derive the default D-Bus object path GLib exports a GApplication at:
/// prefix `/`, then map `.`→`/` and `-`→`_` (D-Bus path elements forbid `-`).
fn base_path_for(bus_name: &str) -> String {
    let mut path = String::with_capacity(bus_name.len() + 1);
    path.push('/');
    for c in bus_name.chars() {
        match c {
            '.' => path.push('/'),
            '-' => path.push('_'),
            other => path.push(other),
        }
    }
    path
}

/// Locate the GApplication owned by `app_pid` on `conn` and return its
/// `org.gtk.Actions` address. Scans well-known bus names, matching each
/// owner's PID (served by the bus daemon, so fast and unhangable) against
/// the spawned app's, then confirms the candidate actually answers
/// `org.gtk.Actions.List` at its derived base path.
pub async fn discover_application(
    conn: &zbus::Connection,
    app_pid: u32,
) -> Result<GtkApplicationAddress> {
    let dbus = zbus::fdo::DBusProxy::new(conn)
        .await
        .map_err(|e| Error::atspi_with("org.gtk.Actions: D-Bus daemon proxy", e))?;

    let mut last_err: Option<Error> = None;
    for attempt in 0..DISCOVERY_ATTEMPTS {
        match find_app_once(&dbus, conn, app_pid).await {
            Ok(Some(addr)) => return Ok(addr),
            Ok(None) => {}
            Err(e) => last_err = Some(e),
        }
        if attempt + 1 < DISCOVERY_ATTEMPTS {
            tokio::time::sleep(DISCOVERY_INTERVAL).await;
        }
    }
    Err(last_err.unwrap_or_else(|| {
        Error::atspi(format!(
            "org.gtk.Actions: no application owned by pid {app_pid} found on the session bus \
             (the app may not be a registered GApplication with an id)"
        ))
    }))
}

/// One discovery pass. Returns `Ok(None)` when no matching app is found yet
/// (caller retries); `Err` only on a hard ListNames failure.
async fn find_app_once(
    dbus: &zbus::fdo::DBusProxy<'_>,
    conn: &zbus::Connection,
    app_pid: u32,
) -> Result<Option<GtkApplicationAddress>> {
    let names = dbus
        .list_names()
        .await
        .map_err(|e| Error::atspi_with("org.gtk.Actions: ListNames", e))?;

    for owned in &names {
        let name = owned.as_str();
        // Skip unique (`:1.23`) names and the bus daemon itself.
        if name.starts_with(':') || name == "org.freedesktop.DBus" {
            continue;
        }
        // Served by the bus daemon, so a stale/unresponsive peer can't hang us.
        let pid = match dbus.get_connection_unix_process_id(owned.into()).await {
            Ok(pid) => pid,
            Err(_) => continue,
        };
        if pid != app_pid {
            continue;
        }
        // PID matches — confirm it actually exports org.gtk.Actions before
        // committing (a process can own a name for unrelated reasons).
        let base_path = base_path_for(name);
        if list_action_names(conn, name, &base_path).await.is_ok() {
            return Ok(Some(GtkApplicationAddress {
                bus_name: name.to_string(),
                base_path,
            }));
        }
    }
    Ok(None)
}

/// Read `org.gtk.Actions.List` at `(bus_name, path)`.
async fn list_action_names(
    conn: &zbus::Connection,
    bus_name: &str,
    path: &str,
) -> Result<Vec<String>> {
    let proxy = zbus::Proxy::new(
        conn,
        bus_name.to_owned(),
        path.to_owned(),
        GTK_ACTIONS_IFACE,
    )
    .await
    .map_err(|e| Error::atspi_with("org.gtk.Actions proxy", e))?;
    let names: Vec<String> = proxy
        .call("List", &())
        .await
        .map_err(|e| Error::atspi_with(format!("org.gtk.Actions.List at {path}"), e))?;
    Ok(names)
}

/// Invoke `org.gtk.Actions.Activate(name, parameter, platform_data)` at
/// `(bus_name, path)`. `target` becomes a single-element `av` parameter
/// (GTK's string-target convention); `None` sends an empty `av`.
async fn activate_at(
    conn: &zbus::Connection,
    bus_name: &str,
    path: &str,
    name: &str,
    target: Option<&str>,
) -> Result<()> {
    let proxy = zbus::Proxy::new(
        conn,
        bus_name.to_owned(),
        path.to_owned(),
        GTK_ACTIONS_IFACE,
    )
    .await
    .map_err(|e| Error::atspi_with("org.gtk.Actions proxy", e))?;

    let parameter: Vec<Value> = match target {
        Some(t) => vec![Value::from(t.to_owned())],
        None => Vec::new(),
    };
    let platform_data: HashMap<String, Value> = HashMap::new();

    let _: () = proxy
        .call("Activate", &(name, parameter, platform_data))
        .await
        .map_err(|e| Error::atspi_with(format!("org.gtk.Actions.Activate {name} at {path}"), e))?;
    Ok(())
}

/// Candidate object paths for the application's window action groups. GTK
/// exports each `GtkApplicationWindow` at `<base>/window/<N>`; introspection
/// of `<base>/window` enumerates the live ones. Falls back to the
/// conventional single-window `<base>/window/1` when introspection yields
/// nothing (e.g. a toolkit that doesn't synthesize the intermediate node).
async fn window_paths(conn: &zbus::Connection, bus_name: &str, base_path: &str) -> Vec<String> {
    let window_root = format!("{base_path}/window");
    let mut paths = Vec::new();
    if let Some(xml) = introspect(conn, bus_name, &window_root).await {
        for child in parse_child_node_names(&xml) {
            paths.push(format!("{window_root}/{child}"));
        }
    }
    if paths.is_empty() {
        paths.push(format!("{window_root}/1"));
    }
    paths
}

/// Introspect `(bus_name, path)`, returning the XML on success. Any failure
/// (path not exported, peer error) maps to `None` so window discovery can
/// fall back rather than abort.
async fn introspect(conn: &zbus::Connection, bus_name: &str, path: &str) -> Option<String> {
    let proxy = zbus::fdo::IntrospectableProxy::builder(conn)
        .destination(bus_name.to_owned())
        .ok()?
        .path(path.to_owned())
        .ok()?
        .build()
        .await
        .ok()?;
    proxy.introspect().await.ok()
}

/// Extract child object-node names from an introspection XML document.
///
/// Picks up `<node name="…"/>` child entries while ignoring the root
/// `<node>` (no `name`) and `<interface name="…">` elements. A minimal
/// hand scan rather than a full XML parse: introspection replies carry a
/// `<!DOCTYPE …>` the crate's `sxd` parser rejects, and the shape we need
/// (child node names) is trivial and well-defined.
fn parse_child_node_names(introspect_xml: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = introspect_xml;
    while let Some(pos) = rest.find("<node") {
        let after = &rest[pos + "<node".len()..];
        rest = after;
        // A child node tag is `<node ...>`; the root is `<node>` (no attrs,
        // so the next char is `>`). Require whitespace before attributes.
        if !matches!(after.chars().next(), Some(c) if c.is_whitespace()) {
            continue;
        }
        let Some(tag_end) = after.find('>') else {
            break;
        };
        if let Some(name) = extract_attr_value(&after[..tag_end], "name") {
            names.push(name);
        }
    }
    names
}

/// Read the value of `attr="…"` (single- or double-quoted) from a tag body.
/// Matches the attribute on a leading-whitespace boundary so it can't be a
/// suffix of a longer attribute name (e.g. `surname=`).
fn extract_attr_value(tag: &str, attr: &str) -> Option<String> {
    let needle = format!(" {attr}=");
    let start = tag.find(&needle)? + needle.len();
    let quote = tag.as_bytes().get(start).copied()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let value_start = start + 1;
    let end = tag[value_start..].find(quote as char)?;
    Some(tag[value_start..value_start + end].to_string())
}

/// Parse a prefixed (and optionally string-targeted) action name.
fn parse_action(action: &str) -> Result<ParsedAction> {
    let (full, target) = match action.split_once("::") {
        Some((full, target)) => (full, Some(target.to_string())),
        None => (action, None),
    };
    let (group, name) = full.split_once('.').ok_or_else(|| {
        Error::atspi(format!(
            "action {action:?} must be prefixed with its group, \
             e.g. \"app.quit\" or \"win.close\""
        ))
    })?;
    if group.is_empty() || name.is_empty() {
        return Err(Error::atspi(format!(
            "action {action:?} has an empty group or action name"
        )));
    }
    Ok(ParsedAction {
        group: group.to_string(),
        name: name.to_string(),
        target,
    })
}

/// Activate `action` (`app.*` / `win.*`, optionally `…::target`) on the
/// discovered application. `app.*` targets the base path; `win.*` targets
/// the first window path that exposes the action.
pub async fn activate(
    conn: &zbus::Connection,
    addr: &GtkApplicationAddress,
    action: &str,
) -> Result<()> {
    let parsed = parse_action(action)?;
    match parsed.group.as_str() {
        "app" => {
            activate_at(
                conn,
                &addr.bus_name,
                &addr.base_path,
                &parsed.name,
                parsed.target.as_deref(),
            )
            .await
        }
        "win" => {
            let paths = window_paths(conn, &addr.bus_name, &addr.base_path).await;
            for path in &paths {
                if let Ok(names) = list_action_names(conn, &addr.bus_name, path).await {
                    if names.iter().any(|n| n == &parsed.name) {
                        return activate_at(
                            conn,
                            &addr.bus_name,
                            path,
                            &parsed.name,
                            parsed.target.as_deref(),
                        )
                        .await;
                    }
                }
            }
            Err(Error::atspi(format!(
                "org.gtk.Actions: win action {:?} not found on any application window \
                 ({} window path(s) checked)",
                parsed.name,
                paths.len()
            )))
        }
        other => Err(Error::atspi(format!(
            "org.gtk.Actions: unsupported action group {other:?} in {action:?}; \
             only \"app\" and \"win\" are addressable"
        ))),
    }
}

/// List the prefixed names of every action the app exposes: `app.*` from the
/// base path and `win.*` from each window action group.
pub async fn list_all(
    conn: &zbus::Connection,
    addr: &GtkApplicationAddress,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for name in list_action_names(conn, &addr.bus_name, &addr.base_path).await? {
        out.push(format!("app.{name}"));
    }
    for path in window_paths(conn, &addr.bus_name, &addr.base_path).await {
        // Fallback window paths may not exist; a read error just means
        // "no window actions here", not a failure of the whole listing.
        if let Ok(names) = list_action_names(conn, &addr.bus_name, &path).await {
            for name in names {
                let prefixed = format!("win.{name}");
                if !out.contains(&prefixed) {
                    out.push(prefixed);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_path_maps_dots_and_hyphens() {
        assert_eq!(
            base_path_for("io.github.bohdantkachenko.waydriver.FixtureGtk"),
            "/io/github/bohdantkachenko/waydriver/FixtureGtk"
        );
        // Hyphens are illegal in D-Bus path elements; GLib maps them to `_`.
        assert_eq!(
            base_path_for("org.gnome.gedit-test"),
            "/org/gnome/gedit_test"
        );
        assert_eq!(base_path_for("a"), "/a");
    }

    #[test]
    fn parse_action_plain() {
        let p = parse_action("app.quit").unwrap();
        assert_eq!(p.group, "app");
        assert_eq!(p.name, "quit");
        assert_eq!(p.target, None);
    }

    #[test]
    fn parse_action_window_group() {
        let p = parse_action("win.close").unwrap();
        assert_eq!(p.group, "win");
        assert_eq!(p.name, "close");
        assert_eq!(p.target, None);
    }

    #[test]
    fn parse_action_string_target() {
        // GMenu detailed-name form, e.g. `app.section::adw`.
        let p = parse_action("app.section::adw").unwrap();
        assert_eq!(p.group, "app");
        assert_eq!(p.name, "section");
        assert_eq!(p.target.as_deref(), Some("adw"));
    }

    #[test]
    fn parse_action_dotted_name_splits_on_first_dot() {
        // Action names may themselves contain dots; only the first `.`
        // separates the group from the name.
        let p = parse_action("app.preferences.open").unwrap();
        assert_eq!(p.group, "app");
        assert_eq!(p.name, "preferences.open");
    }

    #[test]
    fn parse_action_requires_group_prefix() {
        assert!(parse_action("quit").is_err());
        assert!(parse_action("app.").is_err());
        assert!(parse_action(".quit").is_err());
    }

    #[test]
    fn parse_child_nodes_extracts_window_ids() {
        // Representative GDBus introspection reply for `<base>/window`.
        let xml = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <node name="1"/>
  <node name="2"/>
</node>"#;
        assert_eq!(parse_child_node_names(xml), vec!["1", "2"]);
    }

    #[test]
    fn parse_child_nodes_ignores_interfaces_and_root() {
        // Introspecting a leaf object: an `<interface>` with members must not
        // be mistaken for a child node, and the unnamed root is skipped.
        let xml = r#"<node>
  <interface name="org.gtk.Actions">
    <method name="Activate">
      <arg type="s" name="action_name" direction="in"/>
    </method>
  </interface>
  <node name="window"/>
</node>"#;
        assert_eq!(parse_child_node_names(xml), vec!["window"]);
    }

    #[test]
    fn parse_child_nodes_empty_when_no_children() {
        let xml = r#"<node><interface name="org.gtk.Actions"></interface></node>"#;
        assert!(parse_child_node_names(xml).is_empty());
    }

    #[test]
    fn extract_attr_value_handles_both_quote_styles() {
        assert_eq!(
            extract_attr_value(r#" name="1"/"#, "name").as_deref(),
            Some("1")
        );
        assert_eq!(
            extract_attr_value(r#" name='win'/"#, "name").as_deref(),
            Some("win")
        );
    }
}
