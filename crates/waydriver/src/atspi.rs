use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::bus::BusProxy;
use atspi::proxy::collection::CollectionProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::editable_text::EditableTextProxy;
use atspi::proxy::text::TextProxy;
use atspi::{State, StateSet};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use sxd_document::parser;
use sxd_xpath::{Context, Factory, Value};
use zbus::proxy::CacheProperties;

use crate::error::{Error, Result};

// ── Proxy builders ──────────────────────────────────────────────────────────

/// Build an [`AccessibleProxy`] for the given bus name and object path.
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

async fn build_editable_text<'a>(
    conn: &'a zbus::Connection,
    bus_name: &str,
    path: &str,
) -> zbus::Result<EditableTextProxy<'a>> {
    EditableTextProxy::builder(conn)
        .destination(bus_name.to_owned())?
        .path(path.to_owned())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn build_text<'a>(
    conn: &'a zbus::Connection,
    bus_name: &str,
    path: &str,
) -> zbus::Result<TextProxy<'a>> {
    TextProxy::builder(conn)
        .destination(bus_name.to_owned())?
        .path(path.to_owned())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn build_component<'a>(
    conn: &'a zbus::Connection,
    bus_name: &str,
    path: &str,
) -> zbus::Result<ComponentProxy<'a>> {
    ComponentProxy::builder(conn)
        .destination(bus_name.to_owned())?
        .path(path.to_owned())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

#[allow(dead_code)]
async fn build_collection<'a>(
    conn: &'a zbus::Connection,
    bus_name: &str,
    path: &str,
) -> zbus::Result<CollectionProxy<'a>> {
    CollectionProxy::builder(conn)
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

// ── Role normalization ──────────────────────────────────────────────────────

/// Convert a raw AT-SPI role name like `"push button"` or `"menu item"` into
/// a PascalCase XML element name like `"PushButton"` or `"MenuItem"`.
///
/// If the role produces a name that isn't a valid XML element name (empty,
/// starts with a digit, or contains characters outside `[A-Za-z0-9_-]`), we
/// return `None` and the caller falls back to emitting a `<Node role="...">`.
fn role_to_element_name(role: &str) -> Option<String> {
    let mut out = String::with_capacity(role.len());
    for word in role.split_whitespace() {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            for c in chars {
                out.extend(c.to_lowercase());
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    let mut it = out.chars();
    let first = it.next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    for c in it {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return None;
        }
    }
    Some(out)
}

/// Sanitize an AT-SPI attribute key into a valid XML attribute name.
/// Returns `None` if the key would produce an empty or reserved name.
fn sanitize_attr_key(key: &str) -> Option<String> {
    let mut out = String::with_capacity(key.len());
    for c in key.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        return None;
    }
    let first = out.chars().next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        out.insert(0, '_');
    }
    // Avoid conflicts with attributes the snapshotter emits itself.
    if matches!(out.as_str(), "name" | "role" | "_ref") {
        out.insert(0, '_');
    }
    Some(out)
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids most control chars; keep TAB/LF/CR, drop the rest.
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

/// States emitted as boolean XML attributes on each node. Absent = false.
const EMITTED_STATES: &[(State, &str)] = &[
    (State::Showing, "showing"),
    (State::Visible, "visible"),
    (State::Enabled, "enabled"),
    (State::Sensitive, "sensitive"),
    (State::Focused, "focused"),
    (State::Focusable, "focusable"),
    (State::Selected, "selected"),
    (State::Selectable, "selectable"),
    (State::Checked, "checked"),
    (State::Checkable, "checkable"),
    (State::Active, "active"),
    (State::Editable, "editable"),
    (State::Expandable, "expandable"),
    (State::Expanded, "expanded"),
    (State::Collapsed, "collapsed"),
    (State::Pressed, "pressed"),
    (State::Modal, "modal"),
];

// ── Snapshot: live AT-SPI tree → XML document ───────────────────────────────

/// Walk the AT-SPI subtree rooted at `(app_bus_name, app_path)` and emit an
/// XML string representation suitable for XPath evaluation.
///
/// Every element carries a `_ref="<bus>|<path>"` attribute; the XPath
/// evaluator reads this after matching to recover the AT-SPI identity of
/// each matched node.
pub async fn snapshot_tree(
    conn: &AccessibilityConnection,
    app_bus_name: &str,
    app_path: &str,
) -> Result<String> {
    let app_root = build_accessible(conn.connection(), app_bus_name, app_path)
        .await
        .map_err(|e| Error::Atspi(format!("failed to get app root: {e}")))?;

    let mut output = String::new();
    output.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    snapshot_node(
        conn.connection(),
        &app_root,
        app_bus_name,
        app_path,
        0,
        &mut output,
    )
    .await;
    Ok(output)
}

type SnapshotFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

fn snapshot_node<'a>(
    conn: &'a zbus::Connection,
    proxy: &'a AccessibleProxy<'a>,
    bus_name: &'a str,
    path: &'a str,
    depth: usize,
    output: &'a mut String,
) -> SnapshotFuture<'a> {
    Box::pin(async move {
        let raw_role = proxy
            .get_role_name()
            .await
            .unwrap_or_else(|_| "unknown".into());
        let name = proxy.name().await.unwrap_or_default();
        let states: StateSet = proxy.get_state().await.unwrap_or_default();
        let attrs: HashMap<String, String> = proxy.get_attributes().await.unwrap_or_default();

        let element_name = role_to_element_name(&raw_role).unwrap_or_else(|| "Node".to_string());

        let indent = "  ".repeat(depth);
        let _ = write!(output, "{indent}<{element_name}");

        // The raw AT-SPI role is always emitted as an attribute so metadata
        // reads (Locator::role, query responses) can read directly from the
        // snapshot without a second round-trip. The element tag doubles as a
        // convenient XPath node-test but loses fidelity for weird roles that
        // fall back to <Node>; the `role` attribute is the source of truth.
        let _ = write!(output, " role=\"{}\"", xml_escape(&raw_role));
        if !name.is_empty() {
            let _ = write!(output, " name=\"{}\"", xml_escape(&name));
        }
        for (state, attr) in EMITTED_STATES {
            if states.contains(*state) {
                let _ = write!(output, " {attr}=\"true\"");
            }
        }
        for (key, value) in &attrs {
            if let Some(safe) = sanitize_attr_key(key) {
                let _ = write!(output, " {}=\"{}\"", safe, xml_escape(value));
            }
        }
        let _ = write!(
            output,
            " _ref=\"{}|{}\"",
            xml_escape(bus_name),
            xml_escape(path)
        );

        if depth > 20 {
            output.push_str("/>\n");
            return;
        }

        let children = match proxy.get_children().await {
            Ok(c) if !c.is_empty() => c,
            _ => {
                output.push_str("/>\n");
                return;
            }
        };

        output.push_str(">\n");
        for child_ref in &children {
            let Some(child_bus) = child_ref.name_as_str() else {
                continue;
            };
            let child_path = child_ref.path_as_str();
            let child = match build_accessible(conn, child_bus, child_path).await {
                Ok(c) => c,
                Err(_) => continue,
            };
            snapshot_node(conn, &child, child_bus, child_path, depth + 1, output).await;
        }
        let _ = writeln!(output, "{indent}</{element_name}>");
    })
}

// ── XPath evaluation ────────────────────────────────────────────────────────

/// Snapshot metadata for an element matched by an XPath query.
///
/// Produced by [`evaluate_xpath_detailed`] — reflects the element's state at
/// the time the snapshot was taken, not live.
#[derive(Debug, Clone)]
pub struct ElementInfo {
    /// AT-SPI `(bus_name, object_path)` identity.
    pub ref_: (String, String),
    /// PascalCase role element name as emitted in the snapshot (e.g.
    /// `"PushButton"`). If the raw role wasn't a valid XML name, this is
    /// `"Node"` and the raw role is stored in `role_raw`.
    pub role: String,
    /// Raw AT-SPI role name when the element fell back to `<Node role="…">`.
    pub role_raw: Option<String>,
    /// Accessible name, if set.
    pub name: Option<String>,
    /// Toolkit attributes (excluding the ones waydriver emits itself).
    pub attributes: HashMap<String, String>,
    /// Lowercase names of the AT-SPI states currently set on the element.
    pub states: Vec<String>,
}

const SNAPSHOT_BUILTINS: &[&str] = &["_ref", "name", "role"];

fn is_state_attr(key: &str) -> bool {
    EMITTED_STATES.iter().any(|(_, attr)| *attr == key)
}

/// Evaluate an XPath expression against a snapshot produced by
/// [`snapshot_tree`] and return the AT-SPI `(bus, path)` tuples of the
/// matching elements, in document order.
pub fn evaluate_xpath(xml: &str, xpath: &str) -> Result<Vec<(String, String)>> {
    let package = parser::parse(xml)
        .map_err(|e| Error::Atspi(format!("failed to parse snapshot XML: {e}")))?;
    let doc = package.as_document();

    let factory = Factory::new();
    let compiled = factory
        .build(xpath)
        .map_err(|e| Error::InvalidSelector {
            xpath: xpath.to_string(),
            reason: e.to_string(),
        })?
        .ok_or_else(|| Error::InvalidSelector {
            xpath: xpath.to_string(),
            reason: "empty xpath".to_string(),
        })?;

    let ctx = Context::new();
    let value = compiled
        .evaluate(&ctx, doc.root())
        .map_err(|e| Error::InvalidSelector {
            xpath: xpath.to_string(),
            reason: e.to_string(),
        })?;

    let nodeset = match value {
        Value::Nodeset(ns) => ns,
        _ => {
            return Err(Error::InvalidSelector {
                xpath: xpath.to_string(),
                reason: "xpath did not return a node-set".to_string(),
            });
        }
    };

    let mut out = Vec::new();
    for node in nodeset.document_order() {
        let Some(elem) = node.element() else { continue };
        let Some(attr) = elem.attribute_value("_ref") else {
            continue;
        };
        if let Some((bus, path)) = attr.split_once('|') {
            out.push((bus.to_string(), path.to_string()));
        }
    }
    Ok(out)
}

/// Evaluate an XPath expression against a snapshot and return full metadata
/// for each matched element, in document order.
pub fn evaluate_xpath_detailed(xml: &str, xpath: &str) -> Result<Vec<ElementInfo>> {
    let package = parser::parse(xml)
        .map_err(|e| Error::Atspi(format!("failed to parse snapshot XML: {e}")))?;
    let doc = package.as_document();

    let factory = Factory::new();
    let compiled = factory
        .build(xpath)
        .map_err(|e| Error::InvalidSelector {
            xpath: xpath.to_string(),
            reason: e.to_string(),
        })?
        .ok_or_else(|| Error::InvalidSelector {
            xpath: xpath.to_string(),
            reason: "empty xpath".to_string(),
        })?;

    let ctx = Context::new();
    let value = compiled
        .evaluate(&ctx, doc.root())
        .map_err(|e| Error::InvalidSelector {
            xpath: xpath.to_string(),
            reason: e.to_string(),
        })?;

    let nodeset = match value {
        Value::Nodeset(ns) => ns,
        _ => {
            return Err(Error::InvalidSelector {
                xpath: xpath.to_string(),
                reason: "xpath did not return a node-set".to_string(),
            });
        }
    };

    let mut out = Vec::new();
    for node in nodeset.document_order() {
        let Some(elem) = node.element() else { continue };
        let Some(ref_attr) = elem.attribute_value("_ref") else {
            continue;
        };
        let Some((bus, path)) = ref_attr.split_once('|') else {
            continue;
        };

        let role = elem.name().local_part().to_string();
        let role_raw = elem.attribute_value("role").map(|s| s.to_string());
        let name = elem.attribute_value("name").map(|s| s.to_string());

        let mut attributes = HashMap::new();
        let mut states = Vec::new();
        for attr in elem.attributes() {
            let key = attr.name().local_part();
            if SNAPSHOT_BUILTINS.contains(&key) {
                continue;
            }
            if is_state_attr(key) {
                if attr.value() == "true" {
                    states.push(key.to_string());
                }
            } else {
                attributes.insert(key.to_string(), attr.value().to_string());
            }
        }

        out.push(ElementInfo {
            ref_: (bus.to_string(), path.to_string()),
            role,
            role_raw,
            name,
            attributes,
            states,
        });
    }
    Ok(out)
}

// ── Actions ─────────────────────────────────────────────────────────────────

fn map_action_err(xpath: &str, bus: &str, path: &str, err: zbus::Error) -> Error {
    if let zbus::Error::MethodError(name, _, _) = &err {
        if is_stale_error_name(name.as_str()) {
            return Error::ElementStale {
                xpath: xpath.to_string(),
                bus: bus.to_string(),
                path: path.to_string(),
            };
        }
    }
    Error::Atspi(err.to_string())
}

/// Classify a D-Bus error-name string as indicating the element is gone.
///
/// Returns true for the three AT-SPI error names that surface when the
/// target widget was destroyed between resolution and action:
/// `org.freedesktop.DBus.Error.UnknownObject`,
/// `org.freedesktop.DBus.Error.ServiceUnknown`, and any `NoReply` variant.
fn is_stale_error_name(name: &str) -> bool {
    name.contains("UnknownObject") || name.contains("ServiceUnknown") || name.contains("NoReply")
}

/// Invoke action index 0 on the element identified by `(bus, path)`.
///
/// NOTE: AT-SPI actions update GTK4's model but don't trigger compositor
/// redraws. Callers driving a test session must follow up with a
/// RemoteDesktop event to force a repaint.
pub async fn do_action_on(
    conn: &AccessibilityConnection,
    xpath: &str,
    bus: &str,
    path: &str,
) -> Result<()> {
    let action = build_action(conn.connection(), bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;

    let n_actions: i32 = action.nactions().await.unwrap_or(0);
    tracing::debug!(%xpath, %bus, %path, n_actions, "do_action(0)");

    let success = action
        .do_action(0)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;

    if !success {
        return Err(Error::Atspi(format!(
            "do_action(0) returned false on {bus}{path} — element may not support activation"
        )));
    }
    Ok(())
}

/// Give keyboard focus to the element identified by `(bus, path)` via the
/// AT-SPI Component interface.
///
/// Returns `Err(Error::Atspi(...))` when the element doesn't implement
/// Component or when `grab_focus` returned false (the toolkit rejected the
/// focus request — typically because the element isn't focusable).
pub async fn grab_focus_on(
    conn: &AccessibilityConnection,
    xpath: &str,
    bus: &str,
    path: &str,
) -> Result<()> {
    let component = build_component(conn.connection(), bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let ok = component
        .grab_focus()
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    if !ok {
        return Err(Error::Atspi(format!(
            "grab_focus returned false on {bus}{path} — element not focusable"
        )));
    }
    Ok(())
}

/// Replace the editable-text contents of the element identified by `(bus, path)`.
pub async fn set_text_on(
    conn: &AccessibilityConnection,
    xpath: &str,
    bus: &str,
    path: &str,
    text: &str,
) -> Result<()> {
    let et = build_editable_text(conn.connection(), bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let ok = et
        .set_text_contents(text)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    if !ok {
        return Err(Error::Atspi(format!(
            "set_text_contents returned false on {bus}{path} — element rejected input"
        )));
    }
    Ok(())
}

/// Read the full text contents of the element identified by `(bus, path)`
/// via the Text interface.
pub async fn read_text_on(
    conn: &AccessibilityConnection,
    xpath: &str,
    bus: &str,
    path: &str,
) -> Result<String> {
    let t = build_text(conn.connection(), bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let n = t
        .character_count()
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let s = t
        .get_text(0, n)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_to_element_name_basic() {
        assert_eq!(
            role_to_element_name("push button").as_deref(),
            Some("PushButton")
        );
        assert_eq!(
            role_to_element_name("menu item").as_deref(),
            Some("MenuItem")
        );
        assert_eq!(role_to_element_name("window").as_deref(), Some("Window"));
        assert_eq!(role_to_element_name("panel").as_deref(), Some("Panel"));
        assert_eq!(
            role_to_element_name("application").as_deref(),
            Some("Application")
        );
    }

    #[test]
    fn role_to_element_name_weird() {
        // Empty → None
        assert_eq!(role_to_element_name(""), None);
        // Role with only whitespace → None
        assert_eq!(role_to_element_name("   "), None);
    }

    #[test]
    fn sanitize_attr_key_clean() {
        assert_eq!(sanitize_attr_key("id").as_deref(), Some("id"));
        assert_eq!(sanitize_attr_key("xml-roles").as_deref(), Some("xml-roles"));
    }

    #[test]
    fn sanitize_attr_key_collides_with_reserved() {
        assert_eq!(sanitize_attr_key("name").as_deref(), Some("_name"));
        assert_eq!(sanitize_attr_key("role").as_deref(), Some("_role"));
        assert_eq!(sanitize_attr_key("_ref").as_deref(), Some("__ref"));
    }

    #[test]
    fn sanitize_attr_key_replaces_bad_chars() {
        assert_eq!(sanitize_attr_key("foo:bar").as_deref(), Some("foo_bar"));
        assert_eq!(sanitize_attr_key("a/b c").as_deref(), Some("a_b_c"));
    }

    #[test]
    fn xml_escape_basic() {
        assert_eq!(xml_escape("<a&b>\"'"), "&lt;a&amp;b&gt;&quot;&apos;");
        assert_eq!(xml_escape("hello"), "hello");
    }

    #[test]
    fn evaluate_xpath_finds_by_name() {
        let xml = r#"<?xml version="1.0"?>
<Application name="calc" _ref="bus|/root">
  <Window name="Calculator" _ref="bus|/w1">
    <PushButton name="7" _ref="bus|/b7"/>
    <PushButton name="+" _ref="bus|/bplus"/>
  </Window>
</Application>"#;
        let hits = evaluate_xpath(xml, "//PushButton[@name='7']").unwrap();
        assert_eq!(hits, vec![("bus".to_string(), "/b7".to_string())]);
    }

    #[test]
    fn evaluate_xpath_multiple_matches() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="bus|/root">
  <PushButton name="OK" _ref="bus|/b1"/>
  <Dialog _ref="bus|/d1">
    <PushButton name="OK" _ref="bus|/b2"/>
  </Dialog>
</Application>"#;
        let hits = evaluate_xpath(xml, "//PushButton[@name='OK']").unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].1, "/b1");
        assert_eq!(hits[1].1, "/b2");
    }

    #[test]
    fn evaluate_xpath_scoped_descendant() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="bus|/root">
  <PushButton name="OK" _ref="bus|/b1"/>
  <Dialog name="Confirm" _ref="bus|/d1">
    <PushButton name="OK" _ref="bus|/b2"/>
  </Dialog>
</Application>"#;
        let hits = evaluate_xpath(xml, "//Dialog[@name='Confirm']//PushButton").unwrap();
        assert_eq!(hits, vec![("bus".to_string(), "/b2".to_string())]);
    }

    #[test]
    fn evaluate_xpath_invalid_syntax() {
        let xml = r#"<?xml version="1.0"?><Application _ref="bus|/root"/>"#;
        let err = evaluate_xpath(xml, "//[").unwrap_err();
        assert!(matches!(err, Error::InvalidSelector { .. }));
    }

    // ── evaluate_xpath_detailed ────────────────────────────────────────────

    #[test]
    fn evaluate_xpath_detailed_extracts_full_metadata() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="bus|/root">
  <PushButton name="Submit" showing="true" enabled="true" id="btn-submit" _ref="bus|/b1"/>
</Application>"#;
        let hits = evaluate_xpath_detailed(xml, "//PushButton").unwrap();
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.ref_, ("bus".to_string(), "/b1".to_string()));
        assert_eq!(h.role, "PushButton");
        assert_eq!(h.role_raw, None);
        assert_eq!(h.name.as_deref(), Some("Submit"));
        assert_eq!(
            h.attributes.get("id").map(String::as_str),
            Some("btn-submit")
        );
        assert!(h.states.iter().any(|s| s == "showing"));
        assert!(h.states.iter().any(|s| s == "enabled"));
    }

    #[test]
    fn evaluate_xpath_detailed_separates_states_from_attrs() {
        // `showing` and `enabled` are emitted state attrs; `id` is a toolkit attr;
        // `xml-roles` is a toolkit attr. Ensure they land in the right bucket.
        let xml = r#"<?xml version="1.0"?>
<Application _ref="bus|/root">
  <PushButton name="X" showing="true" enabled="true" id="x" xml-roles="button" _ref="bus|/b"/>
</Application>"#;
        let hits = evaluate_xpath_detailed(xml, "//PushButton").unwrap();
        let h = &hits[0];
        // Exactly the two state attrs should appear in `states`; no toolkit attrs.
        assert!(h.states.iter().any(|s| s == "showing"));
        assert!(h.states.iter().any(|s| s == "enabled"));
        assert!(!h.states.iter().any(|s| s == "id"));
        assert!(!h.states.iter().any(|s| s == "xml-roles"));
        // Exactly the two toolkit attrs should be in `attributes`; no state names.
        assert_eq!(h.attributes.get("id").map(String::as_str), Some("x"));
        assert_eq!(
            h.attributes.get("xml-roles").map(String::as_str),
            Some("button")
        );
        assert!(!h.attributes.contains_key("showing"));
        assert!(!h.attributes.contains_key("enabled"));
    }

    #[test]
    fn evaluate_xpath_detailed_state_false_not_emitted() {
        // The snapshotter only emits state attrs when they're set. A serialized
        // `showing="false"` (shouldn't happen, but test the read side anyway)
        // must NOT land in `states` because the parser only accepts "true".
        let xml = r#"<?xml version="1.0"?>
<Application _ref="bus|/root">
  <PushButton showing="false" _ref="bus|/b"/>
</Application>"#;
        let hits = evaluate_xpath_detailed(xml, "//PushButton").unwrap();
        assert!(hits[0].states.is_empty());
    }

    #[test]
    fn evaluate_xpath_detailed_node_fallback_preserves_raw_role() {
        // When the snapshotter couldn't turn a role into a valid XML name,
        // it emits `<Node role="...">`. The detailed extractor should surface
        // both `role="Node"` and `role_raw=Some("original")`.
        let xml = r#"<?xml version="1.0"?>
<Application _ref="bus|/root">
  <Node role="0weird" name="odd" _ref="bus|/x"/>
</Application>"#;
        let hits = evaluate_xpath_detailed(xml, "//Node").unwrap();
        assert_eq!(hits[0].role, "Node");
        assert_eq!(hits[0].role_raw.as_deref(), Some("0weird"));
        assert_eq!(hits[0].name.as_deref(), Some("odd"));
    }

    #[test]
    fn evaluate_xpath_detailed_absent_name_is_none() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="bus|/root">
  <Panel _ref="bus|/p"/>
</Application>"#;
        let hits = evaluate_xpath_detailed(xml, "//Panel").unwrap();
        assert_eq!(hits[0].name, None);
    }

    #[test]
    fn evaluate_xpath_detailed_returns_document_order() {
        let xml = r#"<?xml version="1.0"?>
<Application _ref="bus|/root">
  <PushButton name="A" _ref="bus|/a"/>
  <Dialog _ref="bus|/d">
    <PushButton name="B" _ref="bus|/b"/>
  </Dialog>
  <PushButton name="C" _ref="bus|/c"/>
</Application>"#;
        let hits = evaluate_xpath_detailed(xml, "//PushButton").unwrap();
        let names: Vec<&str> = hits.iter().filter_map(|h| h.name.as_deref()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn evaluate_xpath_detailed_invalid_selector() {
        let xml = r#"<?xml version="1.0"?><Application _ref="bus|/root"/>"#;
        let err = evaluate_xpath_detailed(xml, "//[").unwrap_err();
        assert!(matches!(err, Error::InvalidSelector { .. }));
    }

    // ── Staleness classifier ───────────────────────────────────────────────

    #[test]
    fn is_stale_error_name_recognizes_atspi_error_names() {
        // The three D-Bus error names that surface when a widget is gone.
        assert!(is_stale_error_name(
            "org.freedesktop.DBus.Error.UnknownObject"
        ));
        assert!(is_stale_error_name(
            "org.freedesktop.DBus.Error.ServiceUnknown"
        ));
        assert!(is_stale_error_name("org.freedesktop.DBus.Error.NoReply"));
    }

    #[test]
    fn is_stale_error_name_rejects_unrelated_errors() {
        // Real-world non-stale error names shouldn't produce false positives.
        assert!(!is_stale_error_name(
            "org.freedesktop.DBus.Error.InvalidArgs"
        ));
        assert!(!is_stale_error_name(
            "org.freedesktop.DBus.Error.AccessDenied"
        ));
        assert!(!is_stale_error_name("org.a11y.atspi.Error.SomethingElse"));
        assert!(!is_stale_error_name(""));
    }
}
