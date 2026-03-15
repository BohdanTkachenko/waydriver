use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::bus::BusProxy;
use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;
use zbus::proxy::CacheProperties;

use crate::error::{Error, Result};

/// Boxed future returned by the recursive `search_subtree` walker.
/// Yields `Some((bus_name, path, role))` when an accessible matching the
/// requested name is found, otherwise `None`.
type SearchSubtreeFuture<'a> =
    Pin<Box<dyn Future<Output = Option<(String, String, String)>> + Send + 'a>>;

// ── Proxy builders ──────────────────────────────────────────────────────────

pub async fn build_accessible<'a>(
    conn: &'a zbus::Connection,
    bus_name: &str,
    path: &str,
) -> zbus::Result<AccessibleProxy<'a>> {
    AccessibleProxy::builder(conn)
        .destination(bus_name.to_owned())?
        .path(path.to_owned())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn build_action<'a>(
    conn: &'a zbus::Connection,
    bus_name: &str,
    path: &str,
) -> zbus::Result<ActionProxy<'a>> {
    ActionProxy::builder(conn)
        .destination(bus_name.to_owned())?
        .path(path.to_owned())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

// ── Connection ──────────────────────────────────────────────────────────────

/// Connect to the AT-SPI accessibility bus for a given D-Bus session.
pub async fn connect_a11y(dbus_address: &str) -> Result<AccessibilityConnection> {
    let session_addr: zbus::address::Address = dbus_address
        .try_into()
        .map_err(|e: zbus::Error| Error::Atspi(format!("invalid dbus address: {e}")))?;
    let session_conn = zbus::connection::Builder::address(session_addr)?
        .build()
        .await?;

    let bus_proxy = BusProxy::new(&session_conn).await?;
    let a11y_addr_str = bus_proxy.get_address().await?;

    let a11y_addr: zbus::address::Address = a11y_addr_str
        .as_str()
        .try_into()
        .map_err(|e: zbus::Error| Error::Atspi(format!("invalid a11y bus address: {e}")))?;
    let a11y_conn = AccessibilityConnection::from_address(a11y_addr)
        .await
        .map_err(|e| Error::Atspi(format!("failed to connect to a11y bus: {e}")))?;

    Ok(a11y_conn)
}

/// Get the root accessible node from the AT-SPI registry.
pub async fn get_registry_root(conn: &AccessibilityConnection) -> Result<AccessibleProxy<'_>> {
    build_accessible(
        conn.connection(),
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/accessible/root",
    )
    .await
    .map_err(|e| Error::Atspi(format!("failed to get registry root: {e}")))
}

// ── Tree inspection ─────────────────────────────────────────────────────────

/// Dump the accessibility tree for a specific app (scoped by bus_name/path).
pub async fn dump_app_tree(
    conn: &AccessibilityConnection,
    app_bus_name: &str,
    app_path: &str,
) -> Result<String> {
    let app_root = build_accessible(conn.connection(), app_bus_name, app_path)
        .await
        .map_err(|e| Error::Atspi(format!("failed to get app root: {e}")))?;
    let mut output = String::new();
    dump_node(conn.connection(), &app_root, 0, &mut output).await;
    Ok(output)
}

fn dump_node<'a>(
    conn: &'a zbus::Connection,
    proxy: &'a AccessibleProxy<'a>,
    depth: usize,
    output: &'a mut String,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let name = proxy.name().await.unwrap_or_default();
        let role = proxy
            .get_role_name()
            .await
            .unwrap_or_else(|_| "unknown".into());

        let _ = writeln!(output, "{}{} [{}]", "  ".repeat(depth), name, role);

        if depth > 20 {
            let _ = writeln!(output, "{}  ... (max depth reached)", "  ".repeat(depth));
            return;
        }

        let children = match proxy.get_children().await {
            Ok(c) => c,
            Err(_) => return,
        };

        for child_ref in &children {
            let Some(bus_name) = child_ref.name_as_str() else {
                continue;
            };
            let path = child_ref.path_as_str();

            let child = match build_accessible(conn, bus_name, path).await {
                Ok(c) => c,
                Err(_) => continue,
            };

            dump_node(conn, &child, depth + 1, output).await;
        }
    })
}

// ── Element search ──────────────────────────────────────────────────────────

/// Find an element by accessible name, scoped to a specific app.
pub async fn find_element_by_name(
    conn: &AccessibilityConnection,
    app_bus_name: &str,
    app_path: &str,
    target: &str,
) -> Result<(String, String, String)> {
    let app_root = build_accessible(conn.connection(), app_bus_name, app_path)
        .await
        .map_err(|e| Error::Atspi(format!("failed to get app root: {e}")))?;

    if let Some(found) =
        search_subtree(conn.connection(), &app_root, target, app_bus_name, app_path).await
    {
        return Ok(found);
    }

    Err(Error::ElementNotFound(target.to_string()))
}

fn search_subtree<'a>(
    conn: &'a zbus::Connection,
    proxy: &'a AccessibleProxy<'a>,
    target: &'a str,
    node_bus: &'a str,
    node_path: &'a str,
) -> SearchSubtreeFuture<'a> {
    Box::pin(async move {
        let node_name = proxy.name().await.ok()?;
        let role = proxy.get_role_name().await.unwrap_or_default();

        if node_name == target {
            return Some((node_bus.to_string(), node_path.to_string(), role));
        }

        let children = proxy.get_children().await.ok()?;
        for child_ref in &children {
            let Some(bus_name) = child_ref.name_as_str() else {
                continue;
            };
            let path = child_ref.path_as_str();

            let child = match build_accessible(conn, bus_name, path).await {
                Ok(c) => c,
                Err(_) => continue,
            };

            if let Some(found) = search_subtree(conn, &child, target, bus_name, path).await {
                return Some(found);
            }
        }

        None
    })
}

// ── Actions ─────────────────────────────────────────────────────────────────

/// Click an element by accessible name via AT-SPI action.
/// NOTE: AT-SPI actions update GTK4's model but don't trigger compositor redraws.
/// Caller must follow up with a RemoteDesktop event to force repaint.
pub async fn click_element(
    conn: &AccessibilityConnection,
    app_bus_name: &str,
    app_path: &str,
    name: &str,
) -> Result<String> {
    let (bus_name, path, role) = find_element_by_name(conn, app_bus_name, app_path, name).await?;

    let action = build_action(conn.connection(), &bus_name, &path)
        .await
        .map_err(|e| Error::Atspi(format!("no Action interface on '{}': {e}", name)))?;

    let n_actions: i32 = action.nactions().await.unwrap_or(0);
    tracing::debug!(element = name, %role, n_actions, "attempting do_action(0)");

    let success = action
        .do_action(0)
        .await
        .map_err(|e| Error::Atspi(format!("do_action failed on '{}': {e}", name)))?;

    if !success {
        return Err(Error::Atspi(format!(
            "do_action(0) returned false on '{}' [{}] — element may not support activation",
            name, role
        )));
    }

    Ok(format!("Clicked '{}' [{}] via action", name, role))
}
