use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::bus::BusProxy;
use atspi::proxy::collection::CollectionProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::editable_text::EditableTextProxy;
use atspi::proxy::selection::SelectionProxy;
use atspi::proxy::text::TextProxy;
use atspi::proxy::value::ValueProxy;
use atspi::{CoordType, ScrollType, State, StateSet};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::time::Duration;
use sxd_document::parser;
use sxd_xpath::{Context, Factory, Value};
use tokio::task::JoinSet;
use zbus::proxy::CacheProperties;

use crate::error::{Error, Result};

/// Per-method reply timeout applied to every proxy on the a11y bus.
///
/// AT-SPI calls target the *target application's* bridge, so when the
/// app crashes after a Locator has resolved a `(bus, path)` reference,
/// any in-flight call against that bridge waits out the connection's
/// reply timeout. zbus' default (~25s) is a long way to hang
/// `kill_session`, and it dominates [`Locator`](crate::Locator)
/// cancellation latency: `poll_with_retry` checks the cancellation
/// token only at iteration boundaries, so a single stuck call adds the
/// full reply timeout to the kill latency before the next poll
/// observes the cancel.
///
/// 2s is short enough that a worst-case `kill_session` waits at most
/// one in-flight call (the rest short-circuit on the token), and long
/// enough that a momentarily-busy live widget rarely trips it. Calls
/// that *do* time out surface as `MethodError(NoReply)`, which
/// `is_stale_error_name` already classifies as retriable — so the
/// behavior matches the existing "widget went away" path.
///
/// Compositor (mutter RemoteDesktop) and PipeWire connections keep the
/// zbus default: their slow paths (`CreateSession`, ScreenCast
/// negotiation) are bursty by design, and shrinking their timeout
/// would risk false `NoReply`s on a healthy session.
const A11Y_METHOD_TIMEOUT: Duration = Duration::from_secs(2);

/// Screen-relative rectangle for an accessibility element, in logical
/// pixels. All four fields are i32 to match AT-SPI's native types (which
/// permit negative coordinates, e.g. when an element is scrolled off the
/// top of the viewport).
///
/// Produced by [`extents_on`], serialized into the snapshot XML as a
/// `bbox="x,y,width,height"` attribute, and re-parsed into
/// [`ElementInfo::bounds`] by [`evaluate_xpath_detailed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    /// Format as `"x,y,width,height"` — the exact shape stored in the
    /// snapshot's `bbox` attribute.
    pub fn to_bbox_string(&self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.width, self.height)
    }

    /// X coordinate of the right edge (exclusive).
    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width)
    }

    /// Y coordinate of the bottom edge (exclusive).
    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height)
    }

    /// X coordinate of the center (horizontal midpoint).
    pub fn center_x(&self) -> i32 {
        self.x.saturating_add(self.width / 2)
    }

    /// Y coordinate of the center (vertical midpoint).
    pub fn center_y(&self) -> i32 {
        self.y.saturating_add(self.height / 2)
    }

    /// Whether `self` lies entirely within `outer`. Used by
    /// `scroll_into_view` to decide whether an element is already visible
    /// in its scrollable ancestor — if so, scrolling is a no-op.
    pub fn is_inside(&self, outer: &Rect) -> bool {
        self.x >= outer.x
            && self.y >= outer.y
            && self.right() <= outer.right()
            && self.bottom() <= outer.bottom()
    }

    /// Parse a `"x,y,width,height"` string. Returns `None` on any parse
    /// error so callers can treat malformed bounds as "no bounds here"
    /// rather than failing the whole XPath evaluation.
    pub fn parse_bbox(s: &str) -> Option<Self> {
        let mut parts = s.split(',');
        let x = parts.next()?.parse().ok()?;
        let y = parts.next()?.parse().ok()?;
        let width = parts.next()?.parse().ok()?;
        let height = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Rect {
            x,
            y,
            width,
            height,
        })
    }
}

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

async fn build_selection<'a>(
    conn: &'a zbus::Connection,
    bus_name: &str,
    path: &str,
) -> zbus::Result<SelectionProxy<'a>> {
    SelectionProxy::builder(conn)
        .destination(bus_name.to_owned())?
        .path(path.to_owned())?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn build_value<'a>(
    conn: &'a zbus::Connection,
    bus_name: &str,
    path: &str,
) -> zbus::Result<ValueProxy<'a>> {
    ValueProxy::builder(conn)
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
///
/// Returns the raw [`zbus::Connection`] (not [`atspi::connection::AccessibilityConnection`])
/// so we can configure [`A11Y_METHOD_TIMEOUT`] on it via the connection
/// builder — the upstream wrapper offers no public hook for that, and
/// we don't use its registry/event-stream sugar anywhere.
pub async fn connect_a11y(dbus_address: &str) -> Result<zbus::Connection> {
    let session_addr: zbus::address::Address = dbus_address
        .try_into()
        .map_err(|e: zbus::Error| Error::atspi_with("invalid dbus address", e))?;
    let session_conn = zbus::connection::Builder::address(session_addr)?
        .build()
        .await?;

    let bus_proxy = BusProxy::new(&session_conn).await?;
    let a11y_addr_str = bus_proxy.get_address().await?;

    let a11y_addr: zbus::address::Address = a11y_addr_str
        .as_str()
        .try_into()
        .map_err(|e: zbus::Error| Error::atspi_with("invalid a11y bus address", e))?;
    let a11y_conn = zbus::connection::Builder::address(a11y_addr)?
        .method_timeout(A11Y_METHOD_TIMEOUT)
        .build()
        .await
        .map_err(|e| Error::atspi_with("failed to connect to a11y bus", e))?;

    Ok(a11y_conn)
}

/// Get the root accessible node from the AT-SPI registry.
pub async fn get_registry_root(conn: &zbus::Connection) -> Result<AccessibleProxy<'_>> {
    build_accessible(
        conn,
        "org.a11y.atspi.Registry",
        "/org/a11y/atspi/accessible/root",
    )
    .await
    .map_err(|e| Error::atspi_with("failed to get registry root", e))
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
    let first = it
        .next()
        .expect("invariant: out.is_empty() returned false above");
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

/// Resolve a raw AT-SPI cache role *index* into a display role string,
/// tolerating indices the bundled `atspi` crate's `Role` enum doesn't know.
///
/// atspi 0.29 only covers indices 0..=129; newer at-spi2 cores expose higher
/// ones (e.g. 130 = `ATSPI_ROLE_SWITCH`). Rather than let the strict enum
/// reject the whole `Cache.GetItems` reply, [`cache_items`] reads the role as a
/// `u32` and calls this: known indices map through [`role_to_element_name`],
/// unknown ones become a stable `unknown-role-<n>` label so the item survives.
fn cache_role_name(role_idx: u32) -> String {
    match atspi::Role::try_from(role_idx) {
        Ok(r) => {
            let n = r.name();
            role_to_element_name(n).unwrap_or_else(|| n.to_string())
        }
        Err(_) => format!("unknown-role-{role_idx}"),
    }
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
    let first = out
        .chars()
        .next()
        .expect("invariant: out.is_empty() returned false above");
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
    conn: &zbus::Connection,
    app_bus_name: &str,
    app_path: &str,
) -> Result<String> {
    // Validate the root up front so a bad app reference still surfaces as the
    // same error as before. The concurrent fetch below tolerates per-node build
    // failures by skipping the node — right for children, but for the root it
    // would silently yield an empty snapshot instead of a clear error.
    build_accessible(conn, app_bus_name, app_path)
        .await
        .map_err(|e| Error::atspi_with("failed to get app root", e))?;

    let nodes = fetch_subtree(conn, app_bus_name, app_path).await;

    let mut output = String::new();
    output.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    let root_key = (app_bus_name.to_string(), app_path.to_string());
    if nodes.contains_key(&root_key) {
        serialize_node(&nodes, &root_key, 0, &mut output);
    }
    Ok(output)
}

/// Per-node data captured by the concurrent fetch ([`fetch_subtree`]) and later
/// rendered to XML by [`serialize_node`]. Splitting capture (I/O-bound, run
/// concurrently) from rendering (pure, ordered) is what lets the walk fan out
/// both the per-node reads and the child recursion while still emitting a
/// document-ordered snapshot identical to the old sequential walk.
struct RawNode {
    /// AT-SPI `(bus, path)` identity — also this node's key in the fetch map.
    bus: String,
    path: String,
    /// Recursion depth at first discovery (root = 0). Drives the depth cap.
    depth: usize,
    /// Raw AT-SPI role, with `"unknown"` substituted on a failed read (same
    /// fallback the old walk used, so the emitted element tag is unchanged).
    raw_role: String,
    name: String,
    description: String,
    states: StateSet,
    attrs: HashMap<String, String>,
    bounds: Option<Rect>,
    /// Whether `GetChildren` returned a non-empty list. Mirrors the old walk's
    /// open-tag-vs-self-close decision, which keyed off the *raw* reply being
    /// non-empty — independent of whether those children then resolved.
    had_children: bool,
    /// Child `(bus, path)` refs in document order (only those whose ref carried
    /// a resolvable bus name; the rest are dropped, exactly as before).
    children: Vec<(String, String)>,
}

/// Depth cap: nodes deeper than this are emitted self-closed and their children
/// are not fetched — the same `depth > 20` guard the sequential walk used to
/// bound pathological / cyclic trees.
const MAX_DEPTH: usize = 20;

/// How many node fetches run concurrently. Each in-flight node issues up to 7
/// D-Bus calls at once, so this bounds outstanding calls on the a11y connection
/// at 8 × 7 = 56 — well under a session bus daemon's default
/// `max_replies_per_connection` of 128, leaving headroom even if a session runs
/// a couple of snapshots at once (exceeding the cap would make `GetChildren`
/// fail and silently drop a subtree). The synthetic benchmark saturates by this
/// point anyway: a real toolkit answers a11y queries on a single main-loop
/// thread, so wider fan-out mostly just hides transport latency.
const WALK_CONCURRENCY: usize = 8;

/// Snapshot policy: per-node D-Bus introspection calls
/// (`name`, `get_role_name`, `get_state`, `get_attributes`) that
/// return an error are mapped to their default value so the
/// snapshot of the surrounding tree still succeeds — the invariant
/// is "one poisoned node doesn't abort the snapshot."
///
/// The substituted default is indistinguishable on the wire from a
/// genuinely empty value (a node really *can* have name=""), so
/// this helper logs the swallowed error at `warn` to make flakiness
/// recoverable post-hoc via the trace stream. Modeling the
/// fallibility on the snapshot itself (e.g. a `partial="true"`
/// attribute) would make it observable to consumers but adds churn
/// to every locator that reads metadata; this hybrid keeps the
/// snapshot shape stable while putting the flakiness signal where
/// it's most actionable — the operator's logs, not the test's
/// assertion path.
fn snapshot_default_on_err<T, E>(bus: &str, path: &str, op: &'static str, err: E) -> T
where
    T: Default,
    E: std::fmt::Display,
{
    tracing::warn!(
        %bus, %path, op, error = %err,
        "snapshot: per-node introspection call failed; substituting default"
    );
    T::default()
}

/// Concurrently capture the AT-SPI subtree rooted at `(root_bus, root_path)`
/// into a `(bus, path) -> RawNode` map.
///
/// A bounded worker pool ([`WALK_CONCURRENCY`] nodes in flight) fetches each
/// node — its per-node reads fanned out via `join!` — then enqueues that node's
/// children. A `seen` set dedups, so a malformed tree that reaches the same
/// accessible from two parents (or forms a cycle) is fetched at most once; the
/// depth cap is then enforced during rendering, which is what terminates a
/// cyclic tree. Turning the old ~6N *serial* round-trips into a bounded-width
/// fan-out is the whole point (issue #11).
async fn fetch_subtree(
    conn: &zbus::Connection,
    root_bus: &str,
    root_path: &str,
) -> HashMap<(String, String), RawNode> {
    let mut nodes: HashMap<(String, String), RawNode> = HashMap::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut queue: VecDeque<(String, String, usize)> = VecDeque::new();
    let mut workers: JoinSet<Option<RawNode>> = JoinSet::new();

    seen.insert((root_bus.to_string(), root_path.to_string()));
    queue.push_back((root_bus.to_string(), root_path.to_string(), 0));

    loop {
        // Top up the in-flight set from the queue.
        while workers.len() < WALK_CONCURRENCY {
            let Some((bus, path, depth)) = queue.pop_front() else {
                break;
            };
            // zbus connections are cheap Arc-backed handles; cloning lets each
            // fetch be a `'static` spawned task without borrowing `conn`.
            let conn = conn.clone();
            workers.spawn(fetch_one(conn, bus, path, depth));
        }

        // Nothing running and nothing queued ⇒ done.
        let Some(joined) = workers.join_next().await else {
            break;
        };
        let node = match joined {
            Ok(Some(node)) => node,
            // A node whose proxy couldn't be built is skipped — exactly as the
            // sequential walk skipped a child it failed to `build_accessible`.
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "snapshot: node fetch task failed; skipping");
                continue;
            }
        };

        for (cbus, cpath) in &node.children {
            let key = (cbus.clone(), cpath.clone());
            // `insert` returns false if already queued/fetched — the dedup.
            if seen.insert(key.clone()) {
                queue.push_back((key.0, key.1, node.depth + 1));
            }
        }
        nodes.insert((node.bus.clone(), node.path.clone()), node);
    }

    nodes
}

/// Fetch one node's metadata and child refs, with the per-node reads issued
/// concurrently (`join!`) instead of one-after-another. Returns `None` when the
/// accessible proxy can't be built, so the caller skips the node — matching the
/// sequential walk's child handling.
async fn fetch_one(
    conn: zbus::Connection,
    bus: String,
    path: String,
    depth: usize,
) -> Option<RawNode> {
    let proxy = build_accessible(&conn, &bus, &path).await.ok()?;

    // role keeps its own non-Default "unknown" fallback: an empty role string
    // would change the emitted element tag (`role_to_element_name` -> `<Node>`),
    // so it can't share `snapshot_default_on_err`.
    let role_fut = async {
        proxy.get_role_name().await.unwrap_or_else(|e| {
            tracing::warn!(
                %bus, %path, error = %e,
                "snapshot: get_role_name failed; substituting \"unknown\""
            );
            "unknown".to_string()
        })
    };
    let name_fut = async {
        proxy
            .name()
            .await
            .unwrap_or_else(|e| snapshot_default_on_err(&bus, &path, "name", e))
    };
    let description_fut = async {
        proxy
            .description()
            .await
            .unwrap_or_else(|e| snapshot_default_on_err(&bus, &path, "description", e))
    };
    let states_fut = async {
        proxy
            .get_state()
            .await
            .unwrap_or_else(|e| snapshot_default_on_err(&bus, &path, "get_state", e))
    };
    let attrs_fut = async {
        proxy
            .get_attributes()
            .await
            .unwrap_or_else(|e| snapshot_default_on_err(&bus, &path, "get_attributes", e))
    };
    // Window-relative bounds via the Component interface. Any error — no
    // Component, toolkit refused, D-Bus NoReply — maps to "no bounds available"
    // rather than aborting the snapshot. `Window` over `Screen`: headless mutter
    // reports `(0, 0)` for screen-relative positions, which would defeat the
    // bounds-based overflow check in `Locator::scroll_into_view`.
    let bounds_fut = async {
        extents_on(&conn, &bus, &path, CoordType::Window)
            .await
            .ok()
            .flatten()
    };
    // Children are only fetched within the depth cap. `had_children` tracks the
    // *raw* reply being non-empty (it drives open-vs-self-close); the returned
    // refs are filtered to those carrying a resolvable bus name.
    let children_fut = async {
        if depth > MAX_DEPTH {
            return (false, Vec::new());
        }
        match proxy.get_children().await {
            Ok(c) if !c.is_empty() => {
                let refs = c
                    .iter()
                    .filter_map(|child| {
                        child
                            .name_as_str()
                            .map(|b| (b.to_string(), child.path_as_str().to_string()))
                    })
                    .collect();
                (true, refs)
            }
            _ => (false, Vec::new()),
        }
    };

    let (raw_role, name, description, states, attrs, bounds, (had_children, children)) = tokio::join!(
        role_fut,
        name_fut,
        description_fut,
        states_fut,
        attrs_fut,
        bounds_fut,
        children_fut
    );

    Some(RawNode {
        bus,
        path,
        depth,
        raw_role,
        name,
        description,
        states,
        attrs,
        bounds,
        had_children,
        children,
    })
}

/// Render the subtree at `key` into `output` in document order — the pure,
/// sequential half of the walk. Emits byte-for-byte the same XML the old
/// `snapshot_node` wrote; the depth cap is enforced here (self-close at
/// `depth > MAX_DEPTH`), which terminates rendering of a cyclic tree.
fn serialize_node(
    nodes: &HashMap<(String, String), RawNode>,
    key: &(String, String),
    depth: usize,
    output: &mut String,
) {
    // A child ref that isn't in the map failed to build and was skipped during
    // the fetch — the same outcome as the old walk's `continue`.
    let Some(node) = nodes.get(key) else {
        return;
    };

    let element_name = role_to_element_name(&node.raw_role).unwrap_or_else(|| "Node".to_string());

    let indent = "  ".repeat(depth);
    let _ = write!(output, "{indent}<{element_name}");

    // The raw AT-SPI role is always emitted as an attribute so metadata reads
    // (Locator::role, query responses) can read directly from the snapshot. The
    // element tag doubles as a convenient XPath node-test but loses fidelity for
    // weird roles that fall back to <Node>; the `role` attribute is the truth.
    let _ = write!(output, " role=\"{}\"", xml_escape(&node.raw_role));
    if !node.name.is_empty() {
        let _ = write!(output, " name=\"{}\"", xml_escape(&node.name));
    }
    if !node.description.is_empty() {
        let _ = write!(output, " description=\"{}\"", xml_escape(&node.description));
    }
    for (state, attr) in EMITTED_STATES {
        if node.states.contains(*state) {
            let _ = write!(output, " {attr}=\"true\"");
        }
    }
    if let Some(bb) = node.bounds {
        let _ = write!(output, " bbox=\"{}\"", bb.to_bbox_string());
    }
    for (attr_key, value) in &node.attrs {
        if let Some(safe) = sanitize_attr_key(attr_key) {
            let _ = write!(output, " {}=\"{}\"", safe, xml_escape(value));
        }
    }
    let _ = write!(
        output,
        " _ref=\"{}|{}\"",
        xml_escape(&node.bus),
        xml_escape(&node.path)
    );

    if depth > MAX_DEPTH || !node.had_children {
        output.push_str("/>\n");
        return;
    }

    output.push_str(">\n");
    for child_key in &node.children {
        serialize_node(nodes, child_key, depth + 1, output);
    }
    let _ = writeln!(output, "{indent}</{element_name}>");
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
    /// Accessible description (AT-SPI `accessible-description`), if set.
    pub description: Option<String>,
    /// Toolkit attributes (excluding the ones waydriver emits itself).
    pub attributes: HashMap<String, String>,
    /// Lowercase names of the AT-SPI states currently set on the element.
    pub states: Vec<String>,
    /// Screen-relative bounds (x, y, width, height) in logical pixels,
    /// as read from `Component::get_extents` at snapshot time. `None` when
    /// the element doesn't implement Component or isn't laid out yet.
    pub bounds: Option<Rect>,
}

const SNAPSHOT_BUILTINS: &[&str] = &["_ref", "name", "description", "role", "bbox"];

fn is_state_attr(key: &str) -> bool {
    EMITTED_STATES.iter().any(|(_, attr)| *attr == key)
}

/// Whether a cache-derived snapshot ([`snapshot_tree_from_cache`]) carries
/// the attribute `key`, so an XPath predicate matching on it resolves
/// correctly without falling back to the full `GetChildren` walk.
///
/// True for the builtins the cache reply supplies — `name`, `role`, and
/// the emitted state flags (`checked`, `focused`, …) — plus `_ref`. It is
/// deliberately **false** for `bbox` and any toolkit attribute from
/// `Accessible.GetAttributes` (e.g. `id`): the cache doesn't carry those,
/// so a selector touching them must use the walk. Cache-resolution code
/// uses this to decide, per selector, whether the cache can serve it.
pub fn snapshot_cache_has_attr(key: &str) -> bool {
    matches!(key, "name" | "role" | "_ref") || is_state_attr(key)
}

/// Evaluate an XPath expression against a snapshot produced by
/// [`snapshot_tree`] and return the AT-SPI `(bus, path)` tuples of the
/// matching elements, in document order.
pub fn evaluate_xpath(xml: &str, xpath: &str) -> Result<Vec<(String, String)>> {
    let package =
        parser::parse(xml).map_err(|e| Error::atspi_with("failed to parse snapshot XML", e))?;
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
    let package =
        parser::parse(xml).map_err(|e| Error::atspi_with("failed to parse snapshot XML", e))?;
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
        let description = elem.attribute_value("description").map(|s| s.to_string());
        let bounds = elem.attribute_value("bbox").and_then(Rect::parse_bbox);

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
            description,
            attributes,
            states,
            bounds,
        });
    }
    Ok(out)
}

// ── Actions ─────────────────────────────────────────────────────────────────

fn map_action_err(xpath: &str, bus: &str, path: &str, err: zbus::Error) -> Error {
    if let zbus::Error::MethodError(name, _, _) = &err {
        if is_stale_error_name(name.as_str()) {
            // Log every classify-as-stale so post-hoc analysis can
            // see when the heuristic fires and on which error name.
            // If a future toolkit (Qt6, KWin's a11y bridge) starts
            // surfacing a name that *should* count as stale but
            // doesn't yet, the log will show the gap; conversely a
            // name that gets classified as stale but shouldn't will
            // show up here too.
            tracing::debug!(
                %xpath, %bus, %path, error_name = %name.as_str(),
                "classified D-Bus error as ElementStale"
            );
            return Error::ElementStale {
                xpath: xpath.to_string(),
                bus: bus.to_string(),
                path: path.to_string(),
            };
        }
    }
    // A transport-level I/O timeout is indistinguishable from a not-yet-ready
    // a11y bridge: notably a second top-level window whose Text/Value/etc.
    // interface hasn't finished registering on the bus by the time we call it,
    // even though the element is already in the snapshot tree. Unlike a
    // method-level `NoReply`, this surfaces as a transport `InputOutput`
    // (`dbus: I/O error: timed out`), so the per-call method timeout never
    // applies. Classify it as stale so retry-aware callers (`poll_with_retry`)
    // re-resolve and give the bridge time to come up instead of leaking a
    // one-shot timeout.
    if let zbus::Error::InputOutput(io) = &err {
        if io.kind() == std::io::ErrorKind::TimedOut {
            tracing::debug!(
                %xpath, %bus, %path, error = %io,
                "classified D-Bus transport I/O timeout as ElementStale"
            );
            return Error::ElementStale {
                xpath: xpath.to_string(),
                bus: bus.to_string(),
                path: path.to_string(),
            };
        }
    }
    Error::atspi_with("dbus", err)
}

/// Classify a D-Bus error-name string as indicating the element is gone.
///
/// Returns true for error names that surface when the target widget
/// was destroyed between resolution and action:
/// - `org.freedesktop.DBus.Error.UnknownObject` — service still
///   alive, object path no longer registered.
/// - `org.freedesktop.DBus.Error.ServiceUnknown` — whole bus name is
///   gone (the app exited).
/// - `…NoReply` — the call timed out waiting for a response, which
///   for AT-SPI means the peer queue has drained because the widget
///   is being torn down.
/// - `…Disconnected` — toolkit-emitted variant (notably surfaced by
///   `org.a11y.atspi.Error.Disconnected` and analogous bridges in
///   Qt's a11y stack) when the underlying object is no longer
///   reachable. Substring-match keeps the classifier
///   toolkit-agnostic; if a new bridge introduces yet another name
///   it lands as a non-stale error and the tracing log in
///   `map_action_err` makes the gap visible without requiring code
///   changes to discover it.
fn is_stale_error_name(name: &str) -> bool {
    name.contains("UnknownObject")
        || name.contains("ServiceUnknown")
        || name.contains("NoReply")
        || name.contains("Disconnected")
}

/// Invoke action index 0 on the element identified by `(bus, path)`.
///
/// NOTE: AT-SPI actions update GTK4's model but don't trigger compositor
/// redraws. Callers driving a test session must follow up with a
/// RemoteDesktop event to force a repaint.
pub async fn do_action_on(
    conn: &zbus::Connection,
    xpath: &str,
    bus: &str,
    path: &str,
) -> Result<()> {
    let action = build_action(conn, bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;

    let n_actions: i32 = action.nactions().await.unwrap_or(0);
    tracing::debug!(%xpath, %bus, %path, n_actions, "do_action(0)");

    let success = action
        .do_action(0)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;

    if !success {
        return Err(Error::atspi(format!(
            "do_action(0) returned false on {bus}{path} — element may not support activation"
        )));
    }
    Ok(())
}

/// Outcome of an AT-SPI action invocation that wants to distinguish
/// "the widget's a11y bridge doesn't expose this" from "the bridge
/// rejected this specific request".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    /// `Action.DoAction(0)` returned true — the widget reports it
    /// performed the action.
    Performed,
    /// `Action.DoAction(0)` returned false — widget exists, exposes
    /// Action, but rejected the request.
    Refused,
    /// The widget's a11y bridge doesn't implement the Action interface
    /// or `do_action(0)` is missing (no action with index 0). Notably
    /// `AdwButtonRow` and the outer accessible of `AdwSwitchRow` —
    /// activation has to be driven through the input layer (a real
    /// pointer click) instead of AT-SPI.
    NotSupported,
}

/// Like [`do_action_on`] but maps a missing Action interface or "no
/// action with index 0" `MethodError` to [`ActionOutcome::NotSupported`]
/// instead of an error.
///
/// Used by `Locator::click` so a missing-Action bridge doesn't fail the
/// click — the caller can then fall back to a pointer click. Stale-element
/// D-Bus errors still propagate as [`Error::ElementStale`] via
/// [`map_action_err`]; transport-level failures still propagate as
/// [`Error::Atspi`].
pub async fn try_do_action_on(
    conn: &zbus::Connection,
    xpath: &str,
    bus: &str,
    path: &str,
) -> Result<ActionOutcome> {
    // Building the Action proxy itself doesn't issue a method call —
    // it only verifies the address shape — so a "this widget doesn't
    // support Action" error surfaces on `do_action()`.
    let action = build_action(conn, bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;

    let n_actions: i32 = action.nactions().await.unwrap_or(0);
    tracing::debug!(%xpath, %bus, %path, n_actions, "try_do_action(0)");

    match action.do_action(0).await {
        Ok(true) => Ok(ActionOutcome::Performed),
        Ok(false) => Ok(ActionOutcome::Refused),
        Err(zbus::Error::MethodError(name, _, _)) => {
            // Stale-element names (UnknownObject / ServiceUnknown /
            // NoReply / Disconnected) still surface as ElementStale —
            // those mean the target is gone, not that activation isn't
            // supported. Everything else (including
            // `org.freedesktop.DBus.Error.NotSupported`,
            // `UnknownMethod`, and the GTK-emitted "No action with
            // index 0" GError that AdwButtonRow surfaces) maps to
            // NotSupported because the widget exists but doesn't
            // expose a primary action through AT-SPI.
            if is_stale_error_name(name.as_str()) {
                tracing::debug!(
                    %xpath, %bus, %path, error_name = %name.as_str(),
                    "classified D-Bus error as ElementStale during try_do_action"
                );
                Err(Error::ElementStale {
                    xpath: xpath.to_string(),
                    bus: bus.to_string(),
                    path: path.to_string(),
                })
            } else {
                Ok(ActionOutcome::NotSupported)
            }
        }
        Err(e) => Err(Error::atspi_with("dbus", e)),
    }
}

/// Read an element's raw AT-SPI role name live (`Accessible.GetRoleName`)
/// — the authoritative role string the snapshot walk uses. Used to
/// correct cache-derived roles: the cache stores only a role *index*,
/// which is mapped through the bundled `atspi` crate's `Role` enum and
/// can be stale relative to the running at-spi2 core (e.g. a role the
/// core renamed). A live read always matches the toolkit. Falls back to
/// `"unknown"` on error, exactly like the walk's per-node role read.
pub async fn role_name_on(conn: &zbus::Connection, bus: &str, path: &str) -> Result<String> {
    let proxy = build_accessible(conn, bus, path)
        .await
        .map_err(|e| Error::atspi_with("role: build accessible", e))?;
    Ok(proxy.get_role_name().await.unwrap_or_else(|e| {
        tracing::warn!(%bus, %path, error = %e, "role: get_role_name failed; substituting \"unknown\"");
        "unknown".into()
    }))
}

/// Map a raw AT-SPI role name (as [`role_name_on`] / `get_role_name`
/// returns) to the `(element_tag, role_raw)` pair an [`ElementInfo`]
/// carries — the same mapping the snapshot emits, so a corrected role is
/// indistinguishable from a walk-derived one.
pub fn element_role_fields(raw: &str) -> (String, Option<String>) {
    let tag = role_to_element_name(raw).unwrap_or_else(|| "Node".to_string());
    (tag, Some(raw.to_string()))
}

/// Read an element's toolkit attributes live (the AT-SPI
/// `Accessible.GetAttributes` map). Used to enrich cache-derived
/// snapshots, whose `Cache.GetItems` source doesn't carry attributes.
///
/// Mirrors the snapshot walk's tolerance: a `MethodError` (element
/// doesn't expose the attribute interface) maps to an empty map rather
/// than an error, so callers can blanket-enrich without per-element
/// capability checks. Transport-level failures propagate as `Err`.
pub async fn attributes_on(
    conn: &zbus::Connection,
    bus: &str,
    path: &str,
) -> Result<HashMap<String, String>> {
    let proxy = build_accessible(conn, bus, path)
        .await
        .map_err(|e| Error::atspi_with("attributes: build accessible", e))?;
    match proxy.get_attributes().await {
        Ok(attrs) => Ok(attrs),
        Err(zbus::Error::MethodError(_, _, _)) => Ok(HashMap::new()),
        Err(e) => Err(Error::atspi_with("attributes: get_attributes", e)),
    }
}

/// Read screen/window-relative bounds for the element identified by
/// `(bus, path)` via the AT-SPI Component interface.
///
/// Returns `Ok(None)` when the element doesn't implement Component, when
/// Component exists but `get_extents` reports a zero-area rect (used by
/// some toolkits to mean "not laid out yet"), or when the D-Bus call
/// fails in a way that shouldn't abort snapshot capture. Hard errors
/// (connection dead) propagate as `Err`.
pub async fn extents_on(
    conn: &zbus::Connection,
    bus: &str,
    path: &str,
    coord_type: CoordType,
) -> zbus::Result<Option<Rect>> {
    let component = build_component(conn, bus, path).await?;
    match component.get_extents(coord_type).await {
        Ok((x, y, width, height)) => {
            if width <= 0 && height <= 0 {
                // GTK returns (0,0,0,0) for widgets that exist in the a11y
                // tree but haven't been realized/mapped yet. Surface that
                // as "no bounds" rather than a nonsense rect.
                Ok(None)
            } else {
                Ok(Some(Rect {
                    x,
                    y,
                    width,
                    height,
                }))
            }
        }
        Err(zbus::Error::MethodError(_, _, _)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Force lazy AT-SPI realization by hit-testing inside the element
/// identified by `(bus, path)`.
///
/// Calls `Component::GetAccessibleAtPoint(x, y, CoordType::Window)` and
/// discards the result. The point is the side effect on the toolkit:
/// GTK has to call `gtk_widget_pick` to find the widget at `(x, y)` and
/// then `get_accessible()` on it, which is the call path that triggers
/// libadwaita's lazy accessible-subtree build for non-initial
/// `AdwPreferencesDialog` pages and hidden→shown `AdwPreferencesGroup`
/// instances.
///
/// `(x, y)` are interpreted as window-relative — i.e. relative to the
/// toplevel containing the element. For a dialog that is itself the
/// toplevel, that means `(0, 0)` is the dialog's top-left corner and
/// the legal range is `(0..dialog.width, 0..dialog.height)`.
///
/// `MethodError` from the toolkit (Component interface not implemented,
/// or point outside the widget) is swallowed as a successful no-op so
/// callers can blanket-probe a grid without per-cell capability checks.
/// Transport-level failures propagate as `Err`.
pub async fn hit_test_at_point_on(
    conn: &zbus::Connection,
    bus: &str,
    path: &str,
    x: i32,
    y: i32,
) -> zbus::Result<()> {
    let component = build_component(conn, bus, path).await?;
    match component
        .get_accessible_at_point(x, y, CoordType::Window)
        .await
    {
        Ok(_) => Ok(()),
        Err(zbus::Error::MethodError(_, _, _)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// One entry from the application's AT-SPI cache (`Cache.GetItems`),
/// in waydriver's vocabulary: PascalCase role, lowercase state names —
/// the same conventions as the snapshot tree.
///
/// The cache is a *different surface* than the `GetChildren` tree the
/// snapshot walks: GTK populates it whenever an accessible's context is
/// realized. For lazily-realized libadwaita widgets (hidden→shown
/// `AdwPreferencesGroup` content, non-initial `AdwPreferencesDialog`
/// pages) the parent→child tree links are never repaired, but a focus
/// nudge realizes the focused widget *and its ancestor chain* into the
/// cache — making this the only AT-SPI surface where those widgets can
/// be discovered and inspected. See `Session::hidden_accessibles`.
#[derive(Debug, Clone)]
pub struct CachedAccessible {
    /// AT-SPI `(bus_name, object_path)` identity — the same reference
    /// shape as [`ElementInfo::ref_`].
    pub ref_: (String, String),
    /// PascalCase role (e.g. `"CheckBox"`), normalized exactly like the
    /// snapshot's element names. Falls back to the raw role name when it
    /// doesn't form a valid identifier.
    pub role: String,
    /// Accessible name. When the entry exposes no direct name, this is
    /// backfilled from its `LABELLED_BY` relation (e.g. a libadwaita row's
    /// title `Label`), so cache-only rows stay identifiable. `None` only when
    /// neither source yields text.
    pub name: Option<String>,
    /// Accessible description (AT-SPI `accessible-description`) as carried in
    /// the cache item, or `None` when the entry exposes no description. Mirrors
    /// the snapshot's [`ElementInfo::description`].
    pub description: Option<String>,
    /// Lowercase names of the AT-SPI states set on the entry, filtered
    /// to the same set the snapshot emits.
    pub states: Vec<String>,
    /// Object path of the parent accessible, when the cache knows it.
    pub parent_path: Option<String>,
    /// Child count as reported by the cache.
    pub child_count: i32,
}

/// Resolve the accessible name an entry exposes via its `LABELLED_BY`
/// relation. libadwaita rows (AdwActionRow/SwitchRow/ComboRow/SpinRow)
/// carry no direct `name`; their accessible name lives on a title `Label`
/// referenced through this relation. Returns the (space-joined) text of the
/// relation targets, or `None` when there is no such relation or the targets
/// carry no text.
///
/// Tolerant by design: any D-Bus error along the way maps to `None`, so a
/// nameless entry simply stays nameless rather than failing the whole cache
/// read. Inherits the connection's [`A11Y_METHOD_TIMEOUT`], so a stuck bridge
/// bounds each call rather than hanging the read.
async fn labelled_by_name(conn: &zbus::Connection, bus: &str, path: &str) -> Option<String> {
    let accessible = build_accessible(conn, bus, path).await.ok()?;
    let targets = accessible
        .get_relation_set()
        .await
        .ok()?
        .into_iter()
        .find(|(kind, _)| *kind == atspi::RelationType::LabelledBy)
        .map(|(_, targets)| targets)?;
    let mut label = String::new();
    for target in targets {
        // Relation targets within the same app carry an empty bus name; fall
        // back to this entry's own bus, exactly like `cache_items` does.
        let target_bus = target.name_as_str().unwrap_or(bus);
        let Ok(proxy) = build_accessible(conn, target_bus, target.path_as_str()).await else {
            continue;
        };
        if let Ok(text) = proxy.name().await {
            if !text.is_empty() {
                if !label.is_empty() {
                    label.push(' ');
                }
                label.push_str(&text);
            }
        }
    }
    (!label.is_empty()).then_some(label)
}

/// Read the application's AT-SPI cache (`Cache.GetItems` on the app's
/// bus name). Returns every cached accessible mapped into waydriver's
/// conventions. The cache reflects realized contexts, not tree
/// membership — see [`CachedAccessible`].
///
/// Entries with no direct `name` get one resolution attempt through their
/// `LABELLED_BY` relation (see [`labelled_by_name`]), so libadwaita rows that
/// label themselves via a title `Label` come back identifiable instead of
/// nameless.
pub async fn cache_items(conn: &zbus::Connection, app_bus: &str) -> Result<Vec<CachedAccessible>> {
    // Deserialize the `Cache.GetItems` reply into a *tolerant* shape: the role
    // is read as a raw `u32` rather than the `atspi` crate's `Role` enum.
    //
    // atspi 0.29's `Role` enum (via `CacheItem`) only covers indices 0..=129
    // and its derived serde impl rejects the **entire** reply on the first
    // index it doesn't recognise — observed with role 130, which libadwaita's
    // newer AdwPreferences rows (AdwSpinRow/AdwComboRow/AdwExpanderRow) expose.
    // One unknown role must not blank the whole cache read, so we bypass the
    // strict enum at the wire boundary and convert per-item below.
    //
    // The tuple matches `atspi_common::CacheItem` field-for-field —
    // signature `a((so)(so)(so)iiassusau)` — except `role: u32`.
    //
    // The two `String` fields are, in wire order, the accessible **name**
    // (before the role) and the accessible **description** (after it) — the
    // order at-spi2-core's client and GTK4's `gtkatspicache.c` both emit
    // (`name`, `role`, `description`). atspi 0.29 mislabels them `short_name`
    // and `name` on its `CacheItem`, but the field that holds the AT-SPI name
    // is the one *before* the role; the one after is the description.
    type RawCacheItem = (
        atspi::ObjectRefOwned, // object   (so)
        atspi::ObjectRefOwned, // app      (so)
        atspi::ObjectRefOwned, // parent   (so)
        i32,                   // index in parent  i
        i32,                   // child count      i
        atspi::InterfaceSet,   // interfaces       as
        String,                // name             s
        u32,                   // role (tolerant)  u
        String,                // description      s
        atspi::StateSet,       // states           au
    );

    let proxy = zbus::Proxy::new(
        conn,
        app_bus.to_string(),
        "/org/a11y/atspi/cache",
        "org.a11y.atspi.Cache",
    )
    .await
    .map_err(|e| Error::atspi_with("cache proxy build", e))?;

    let items: Vec<RawCacheItem> = proxy
        .call("GetItems", &())
        .await
        .map_err(|e| Error::atspi_with("Cache.GetItems", e))?;

    // `map`'s closure can't be async, and resolving LABELLED_BY needs a D-Bus
    // round-trip per nameless entry, so collect with an explicit async loop.
    let mut cached = Vec::with_capacity(items.len());
    for (object, _app, parent, _index, children, _ifaces, name, role_idx, description, states) in
        items
    {
        let role = cache_role_name(role_idx);
        let emitted_states = EMITTED_STATES
            .iter()
            .filter(|(state, _)| states.contains(*state))
            .map(|(_, attr)| (*attr).to_string())
            .collect();
        let parent_path = match parent.path_as_str() {
            "/org/a11y/atspi/null" | "" => None,
            p => Some(p.to_string()),
        };
        let ref_ = (
            object.name_as_str().unwrap_or(app_bus).to_string(),
            object.path_as_str().to_string(),
        );
        // libadwaita rows expose their name via LABELLED_BY, not a direct
        // `name`; resolve it so cache-only rows are identifiable. Named entries
        // skip the round-trips, and a failed resolution leaves the entry
        // nameless rather than erroring the whole read.
        let name = if name.is_empty() {
            labelled_by_name(conn, &ref_.0, &ref_.1).await
        } else {
            Some(name)
        };
        let description = (!description.is_empty()).then_some(description);
        cached.push(CachedAccessible {
            ref_,
            role,
            name,
            description,
            states: emitted_states,
            parent_path,
            child_count: children,
        });
    }
    Ok(cached)
}

/// Raw (non-PascalCased) AT-SPI role name for a cache role *index* —
/// the form the snapshot walk puts in the `role="…"` attribute (e.g.
/// `"push button"`, not `"PushButton"`). Unknown indices fall back to the
/// same `unknown-role-<n>` label [`cache_role_name`] uses, so a cache
/// snapshot and a walk snapshot carry identical `role` attributes.
fn cache_raw_role_name(role_idx: u32) -> String {
    match atspi::Role::try_from(role_idx) {
        Ok(r) => r.name().to_string(),
        Err(_) => format!("unknown-role-{role_idx}"),
    }
}

/// One node of a cache-derived snapshot, carrying exactly the fields the
/// AT-SPI cache (`Cache.GetItems`) supplies that the snapshot XML needs:
/// identity, parent link + sibling index (for reconstructing child
/// order), raw role, name, and states. Deliberately *no* `bbox` or
/// toolkit attributes — the cache does not report them; see
/// [`snapshot_tree_from_cache`].
#[derive(Debug, Clone)]
struct CacheSnapshotNode {
    bus: String,
    path: String,
    parent_path: Option<String>,
    index_in_parent: i32,
    raw_role: String,
    name: String,
    states: StateSet,
}

/// Render a snapshot-format XML document from a flat set of cache nodes,
/// rooted at `root_path`. Pure (no I/O) so the tree-reconstruction and
/// formatting logic is unit-testable without a live D-Bus cache.
///
/// The output matches [`snapshot_tree`]'s element shape — same tag
/// names, same `role` / `name` / state attributes, same `_ref` — with
/// two deliberate omissions the cache can't fill: `bbox` and toolkit
/// attributes. Consumers that need those (bounds-based actions,
/// attribute selectors) must enrich matched nodes with a live per-node
/// read; structural and role/name/state selectors resolve directly.
///
/// Children are ordered by `index_in_parent` to reproduce the AT-SPI
/// child order XPath positional predicates depend on. A `visited` guard
/// and a depth cap (matching the walk's `depth > 20`) keep a cyclic or
/// pathological cache from looping forever.
fn render_cache_snapshot(nodes: Vec<CacheSnapshotNode>, root_path: &str) -> String {
    use std::collections::{HashMap, HashSet};

    let by_path: HashMap<&str, &CacheSnapshotNode> =
        nodes.iter().map(|n| (n.path.as_str(), n)).collect();
    let mut children: HashMap<&str, Vec<&CacheSnapshotNode>> = HashMap::new();
    for n in &nodes {
        if let Some(parent) = n.parent_path.as_deref() {
            children.entry(parent).or_default().push(n);
        }
    }
    for kids in children.values_mut() {
        kids.sort_by_key(|n| n.index_in_parent);
    }

    let mut output = String::new();
    output.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    if let Some(root) = by_path.get(root_path) {
        let mut visited = HashSet::new();
        render_cache_node(root, &children, 0, &mut visited, &mut output);
    }
    output
}

fn render_cache_node<'a>(
    node: &'a CacheSnapshotNode,
    children: &HashMap<&'a str, Vec<&'a CacheSnapshotNode>>,
    depth: usize,
    visited: &mut std::collections::HashSet<&'a str>,
    output: &mut String,
) {
    let element_name = role_to_element_name(&node.raw_role).unwrap_or_else(|| "Node".to_string());
    let indent = "  ".repeat(depth);
    let _ = write!(output, "{indent}<{element_name}");
    let _ = write!(output, " role=\"{}\"", xml_escape(&node.raw_role));
    if !node.name.is_empty() {
        let _ = write!(output, " name=\"{}\"", xml_escape(&node.name));
    }
    for (state, attr) in EMITTED_STATES {
        if node.states.contains(*state) {
            let _ = write!(output, " {attr}=\"true\"");
        }
    }
    let _ = write!(
        output,
        " _ref=\"{}|{}\"",
        xml_escape(&node.bus),
        xml_escape(&node.path)
    );

    // Guard against cycles / re-entry (a malformed cache could point two
    // parents at one child) and runaway depth, mirroring the walk's cap.
    let kids = if depth > 20 || !visited.insert(node.path.as_str()) {
        &[][..]
    } else {
        children
            .get(node.path.as_str())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    };
    if kids.is_empty() {
        output.push_str("/>\n");
        return;
    }
    output.push_str(">\n");
    for child in kids {
        render_cache_node(child, children, depth + 1, visited, output);
    }
    let _ = writeln!(output, "{indent}</{element_name}>");
}

/// Build a snapshot-format XML document from the application's AT-SPI
/// cache (`Cache.GetItems`) instead of walking `GetChildren` node by
/// node. One bulk D-Bus round-trip returns the entire realized tree, so
/// for large apps this is dramatically cheaper than [`snapshot_tree`] —
/// the GTK accessibility bridge largely serializes per-call introspection
/// on its main loop, so the walk's cost grows with *node count* while the
/// cache read grows with *reply size*.
///
/// **Fidelity caveat:** the cache reply carries role, name, states, and
/// tree structure, but **not** `Component` bounds (`bbox`) or arbitrary
/// toolkit attributes. The returned XML therefore omits both. It is a
/// faithful drop-in for structural and role/name/state XPath selectors;
/// selectors that match on toolkit attributes, and any consumer reading
/// `bbox`, must enrich the matched node with a live per-element read
/// ([`extents_on`], `get_attributes`). See issue #11.
pub async fn snapshot_tree_from_cache(
    conn: &zbus::Connection,
    app_bus: &str,
    app_path: &str,
) -> Result<String> {
    // Field order matches `cache_items`' `RawCacheItem`: the `String`
    // before the role is the accessible **name**, the one after it is the
    // **description** (atspi 0.29 mislabels them). We only need the name.
    type RawCacheItem = (
        atspi::ObjectRefOwned, // object   (so)
        atspi::ObjectRefOwned, // app      (so)
        atspi::ObjectRefOwned, // parent   (so)
        i32,                   // index in parent  i
        i32,                   // child count      i
        atspi::InterfaceSet,   // interfaces       as
        String,                // name             s
        u32,                   // role (tolerant)  u
        String,                // description      s
        atspi::StateSet,       // states           au
    );

    let proxy = zbus::Proxy::new(
        conn,
        app_bus.to_string(),
        "/org/a11y/atspi/cache",
        "org.a11y.atspi.Cache",
    )
    .await
    .map_err(|e| Error::atspi_with("cache proxy build", e))?;

    let items: Vec<RawCacheItem> = proxy
        .call("GetItems", &())
        .await
        .map_err(|e| Error::atspi_with("Cache.GetItems", e))?;

    let nodes = items
        .into_iter()
        .map(
            |(
                object,
                _app,
                parent,
                index,
                _children,
                _ifaces,
                name,
                role_idx,
                _description,
                states,
            )| {
                let parent_path = match parent.path_as_str() {
                    "/org/a11y/atspi/null" | "" => None,
                    p => Some(p.to_string()),
                };
                CacheSnapshotNode {
                    bus: object.name_as_str().unwrap_or(app_bus).to_string(),
                    path: object.path_as_str().to_string(),
                    parent_path,
                    index_in_parent: index,
                    raw_role: cache_raw_role_name(role_idx),
                    name,
                    states,
                }
            },
        )
        .collect();

    Ok(render_cache_snapshot(nodes, app_path))
}

/// Ask the toolkit to scroll the element identified by `(bus, path)` into
/// view via the AT-SPI `Component::scroll_to` method.
///
/// Returns `Ok(true)` when the widget honored the request, `Ok(false)`
/// when it declined (returned false — usually meaning the widget's
/// toolkit hasn't implemented scroll_to for this role), and
/// `Ok(false)` also when the D-Bus call fails with a MethodError
/// (typically "interface not supported"). Only propagates `Err` for
/// transport-level failures that signal a broken session.
pub async fn scroll_to_on(
    conn: &zbus::Connection,
    bus: &str,
    path: &str,
    scroll_type: ScrollType,
) -> zbus::Result<bool> {
    let component = build_component(conn, bus, path).await?;
    match component.scroll_to(scroll_type).await {
        Ok(ok) => Ok(ok),
        Err(zbus::Error::MethodError(_, _, _)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Ask the toolkit to scroll the element identified by `(bus, path)` so
/// its position lands at `(x, y)` in the given coordinate frame — the
/// AT-SPI `Component::scroll_to_point` method.
///
/// Same error-mapping contract as [`scroll_to_on`]: any MethodError
/// (the widget doesn't implement it, or rejected the request) becomes
/// `Ok(false)`.
pub async fn scroll_to_point_on(
    conn: &zbus::Connection,
    bus: &str,
    path: &str,
    coord_type: CoordType,
    x: i32,
    y: i32,
) -> zbus::Result<bool> {
    let component = build_component(conn, bus, path).await?;
    match component.scroll_to_point(coord_type, x, y).await {
        Ok(ok) => Ok(ok),
        Err(zbus::Error::MethodError(_, _, _)) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Outcome of an AT-SPI focus request that wants to distinguish "the
/// widget's a11y bridge doesn't expose this" from "the bridge said no
/// to this specific request".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusOutcome {
    /// `Component::grab_focus` returned true — the widget reports it
    /// took focus.
    Granted,
    /// `Component::grab_focus` returned false — widget exists, exposes
    /// Component, but rejected the request (typically because it
    /// isn't focusable in its current state).
    Rejected,
    /// The widget's a11y bridge doesn't implement the Component
    /// interface or the `grab_focus` method on it. Common on GTK4
    /// `Entry` / `Text` widgets, where focus has to be driven through
    /// the input layer (Tab navigation, pointer click) instead of
    /// AT-SPI. The keystrokes that follow will land on whatever
    /// currently holds keyboard focus.
    NotSupported,
}

/// Give keyboard focus to the element identified by `(bus, path)` via the
/// AT-SPI Component interface.
///
/// Returns `Err(Error::Atspi(...))` when the element doesn't implement
/// Component or when `grab_focus` returned false (the toolkit rejected the
/// focus request — typically because the element isn't focusable).
pub async fn grab_focus_on(
    conn: &zbus::Connection,
    xpath: &str,
    bus: &str,
    path: &str,
) -> Result<()> {
    let component = build_component(conn, bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let ok = component
        .grab_focus()
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    if !ok {
        return Err(Error::atspi(format!(
            "grab_focus returned false on {bus}{path} — element not focusable"
        )));
    }
    Ok(())
}

/// Like [`grab_focus_on`] but maps `MethodError` (the Component
/// interface or `grab_focus` method isn't implemented for this widget)
/// to [`FocusOutcome::NotSupported`] instead of an error.
///
/// Used by `Locator::fill_with_opts` so a missing-Component bridge
/// doesn't fail the whole fill — that's the documented GTK4 quirk
/// behind `Entry` and `Text`. Stale-element D-Bus errors still
/// propagate as [`Error::ElementStale`] via [`map_action_err`];
/// transport-level failures still propagate as [`Error::Atspi`].
pub async fn try_grab_focus_on(
    conn: &zbus::Connection,
    xpath: &str,
    bus: &str,
    path: &str,
) -> Result<FocusOutcome> {
    // Building the Component proxy itself doesn't issue a method
    // call — it only verifies the address shape — so a "this widget
    // doesn't support Component" error surfaces on `grab_focus()`.
    let component = build_component(conn, bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    match component.grab_focus().await {
        Ok(true) => Ok(FocusOutcome::Granted),
        Ok(false) => Ok(FocusOutcome::Rejected),
        Err(zbus::Error::MethodError(name, _, _)) => {
            // Stale-element names (UnknownObject / ServiceUnknown /
            // NoReply / Disconnected) still surface as ElementStale —
            // those mean the target is gone, not that focus isn't
            // supported. Everything else (including
            // `org.freedesktop.DBus.Error.NotSupported` and
            // `UnknownMethod`) maps to NotSupported because the
            // widget exists but doesn't expose this AT-SPI method.
            if is_stale_error_name(name.as_str()) {
                tracing::debug!(
                    %xpath, %bus, %path, error_name = %name.as_str(),
                    "classified D-Bus error as ElementStale during try_grab_focus"
                );
                Err(Error::ElementStale {
                    xpath: xpath.to_string(),
                    bus: bus.to_string(),
                    path: path.to_string(),
                })
            } else {
                Ok(FocusOutcome::NotSupported)
            }
        }
        Err(e) => Err(Error::atspi_with("dbus", e)),
    }
}

/// Replace the editable-text contents of the element identified by `(bus, path)`.
pub async fn set_text_on(
    conn: &zbus::Connection,
    xpath: &str,
    bus: &str,
    path: &str,
    text: &str,
) -> Result<()> {
    let et = build_editable_text(conn, bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let ok = et
        .set_text_contents(text)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    if !ok {
        return Err(Error::atspi(format!(
            "set_text_contents returned false on {bus}{path} — element rejected input"
        )));
    }
    Ok(())
}

/// Select the child at `index` on a container that implements the AT-SPI
/// Selection interface — the core primitive behind `Locator::select_option`.
///
/// Maps a `select_child` call that returns `Ok(false)` into an
/// `Error::Atspi` with a diagnostic suggesting the most likely causes
/// (no Selection interface on this element, or the widget rejected the
/// request — e.g. the index is out of range for the model). MethodError
/// from `(bus, path)` going stale between resolution and the call
/// produces `Error::ElementStale` via [`map_action_err`].
pub async fn select_child_on(
    conn: &zbus::Connection,
    xpath: &str,
    bus: &str,
    path: &str,
    index: i32,
) -> Result<()> {
    let sel = build_selection(conn, bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let ok = sel
        .select_child(index)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    if !ok {
        return Err(Error::atspi(format!(
            "select_child({index}) returned false on {bus}{path} — element \
             may not implement the Selection interface or the index is out \
             of range"
        )));
    }
    Ok(())
}

/// Read the full text contents of the element identified by `(bus, path)`
/// via the Text interface.
pub async fn read_text_on(
    conn: &zbus::Connection,
    xpath: &str,
    bus: &str,
    path: &str,
) -> Result<String> {
    let t = build_text(conn, bus, path)
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

/// Snapshot of an element's AT-SPI `Value` interface: its current position
/// plus the range and step it moves within.
///
/// Backs [`read_value_on`] and [`crate::Locator::value`]. The headline use is
/// reading a scrolled view's offset — locate the `scroll bar` inside the
/// scrolled window and read [`current`](Self::current); [`minimum`](Self::minimum)
/// / [`maximum`](Self::maximum) bound the travel. The same fields describe
/// sliders, progress bars, and spin buttons.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ValueInfo {
    /// Current value (`CurrentValue`) — e.g. a scroll bar's offset.
    pub current: f64,
    /// Lower bound of the range (`MinimumValue`).
    pub minimum: f64,
    /// Upper bound of the range (`MaximumValue`).
    pub maximum: f64,
    /// Smallest step the value changes by (`MinimumIncrement`); `0.0` when the
    /// toolkit doesn't advertise one.
    pub minimum_increment: f64,
}

/// Read the AT-SPI `Value` interface of the element identified by `(bus, path)`
/// — current position, range, and minimum increment.
///
/// Mirrors [`read_text_on`]: a live read after the caller has resolved the
/// reference. A `(bus, path)` gone stale between resolution and the call maps
/// to [`Error::ElementStale`] via [`map_action_err`]; an element that doesn't
/// implement `Value` surfaces as an [`Error::Atspi`] from the same mapper.
pub async fn read_value_on(
    conn: &zbus::Connection,
    xpath: &str,
    bus: &str,
    path: &str,
) -> Result<ValueInfo> {
    let v = build_value(conn, bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let current = v
        .current_value()
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let minimum = v
        .minimum_value()
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let maximum = v
        .maximum_value()
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let minimum_increment = v
        .minimum_increment()
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    Ok(ValueInfo {
        current,
        minimum,
        maximum,
        minimum_increment,
    })
}

/// Read the visible text of the currently-selected child of a container that
/// implements the AT-SPI `Selection` interface — the read-side counterpart to
/// [`select_child_on`].
///
/// The selected option's label doesn't live on the container itself: AT-SPI
/// exposes it on a *child* accessible. `Selection.GetSelectedChild(0)` returns
/// that child and we read its accessible `name`, falling back to its `Text`
/// interface when the name is empty (some toolkits put the string there
/// instead). The selected-child reference carries an empty bus name when it
/// lives in the same app — the usual case — so we fall back to the container's
/// own bus, exactly like [`cache_items`] / [`labelled_by_name`] do for
/// intra-app references.
///
/// **Scope.** This is the right read for containers whose selected child is a
/// realized accessible — `GtkListBox`, `GtkListView`, tree/table selections.
/// It does **not** cover the dropdown-style combos whose option widgets are
/// created lazily on popup-open: `GtkDropDown`, `GtkComboBox`, and
/// `AdwComboRow` expose no working `Selection` interface while closed
/// (`GetSelectedChild` errors with `UnknownMethod`/`NotSupported`), so their
/// current choice is reachable only as an inline display `Label` child — read
/// that label's text via [`read_text_on`] instead.
///
/// A `(bus, path)` gone stale between resolution and the call maps to
/// [`Error::ElementStale`] via [`map_action_err`]; a container that doesn't
/// implement `Selection`, or has nothing selected (so `GetSelectedChild(0)` is
/// out of range), surfaces as an [`Error::Atspi`] from the same mapper.
pub async fn read_selected_name_on(
    conn: &zbus::Connection,
    xpath: &str,
    bus: &str,
    path: &str,
) -> Result<String> {
    let sel = build_selection(conn, bus, path)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;
    let child = sel
        .get_selected_child(0)
        .await
        .map_err(|e| map_action_err(xpath, bus, path, e))?;

    // `get_selected_child` usually returns the child carrying the app's bus
    // name, but an intra-app reference can arrive with an empty name (not
    // `None` — `ObjectRef` stores `Some("")`), and building a proxy with an
    // empty destination fails. Fall back to the container's own bus in both the
    // empty and null cases.
    let child_bus = match child.name_as_str() {
        Some(name) if !name.is_empty() => name,
        _ => bus,
    };
    let child_path = child.path_as_str();
    let proxy = build_accessible(conn, child_bus, child_path)
        .await
        .map_err(|e| map_action_err(xpath, child_bus, child_path, e))?;
    let name = proxy
        .name()
        .await
        .map_err(|e| map_action_err(xpath, child_bus, child_path, e))?;
    if !name.is_empty() {
        return Ok(name);
    }
    // No accessible name on the selected child — try its Text interface before
    // giving up, mirroring the "read its name (or Text)" resolution order.
    read_text_on(conn, xpath, child_bus, child_path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_role_name_unknown_index_is_labelled_not_rejected() {
        // The reporter's exact case: atspi 0.29's Role enum stops at 129, so a
        // newer at-spi2 core's role 130 (`ATSPI_ROLE_SWITCH`) must round-trip
        // as a stable label instead of blanking the whole Cache.GetItems reply.
        assert_eq!(cache_role_name(130), "unknown-role-130");
        assert_eq!(cache_role_name(9999), "unknown-role-9999");
    }

    #[test]
    fn cache_role_name_known_index_resolves_through_element_table() {
        let frame = cache_role_name(atspi::Role::Frame as u32);
        assert_eq!(frame, "Frame");
        assert!(!frame.starts_with("unknown-role-"));
    }

    fn cnode(
        path: &str,
        parent: Option<&str>,
        index: i32,
        role: &str,
        name: &str,
        states: StateSet,
    ) -> CacheSnapshotNode {
        CacheSnapshotNode {
            bus: ":1.5".to_string(),
            path: path.to_string(),
            parent_path: parent.map(str::to_string),
            index_in_parent: index,
            raw_role: role.to_string(),
            name: name.to_string(),
            states,
        }
    }

    #[test]
    fn render_cache_snapshot_builds_ordered_tree() {
        // Children supplied out of order; renderer must sort by
        // index_in_parent so XPath positional predicates stay correct.
        let nodes = vec![
            cnode("/root", None, 0, "frame", "win", StateSet::default()),
            cnode(
                "/b",
                Some("/root"),
                1,
                "push button",
                "second",
                [State::Focused].into_iter().collect(),
            ),
            cnode(
                "/a",
                Some("/root"),
                0,
                "push button",
                "first",
                StateSet::default(),
            ),
        ];
        let xml = render_cache_snapshot(nodes, "/root");

        // Format parity with the walk: PascalCase tag, raw role attr,
        // name attr, _ref, and the focused state surfaced as an attribute.
        assert!(xml.contains(r#"<Frame role="frame" name="win" _ref=":1.5|/root">"#));
        assert!(xml.contains(r#"name="first" _ref=":1.5|/a"#));
        assert!(xml.contains(r#"focused="true""#));
        // Sibling order follows index, not insertion order.
        let first = xml.find("first").expect("first present");
        let second = xml.find("second").expect("second present");
        assert!(first < second, "children must render in index order");
        // The cache carries no bounds or toolkit attrs — none emitted.
        assert!(!xml.contains("bbox="));
    }

    #[test]
    fn render_cache_snapshot_tolerates_parent_cycle() {
        // Two nodes naming each other as parent must not infinite-loop;
        // the visited-guard breaks the cycle and the render terminates.
        let nodes = vec![
            cnode("/x", Some("/y"), 0, "panel", "x", StateSet::default()),
            cnode("/y", Some("/x"), 0, "panel", "y", StateSet::default()),
        ];
        let xml = render_cache_snapshot(nodes, "/x");
        assert!(xml.contains("_ref=\":1.5|/x\""));
    }

    #[test]
    fn render_cache_snapshot_unknown_root_yields_header_only() {
        let nodes = vec![cnode("/a", None, 0, "panel", "a", StateSet::default())];
        let xml = render_cache_snapshot(nodes, "/missing");
        assert!(xml.contains("<?xml"));
        assert!(!xml.contains("_ref="));
    }

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

    // ── serialize_node: the rendering half of the (now parallel) walk ────────
    //
    // The concurrent fetch (fetch_subtree/fetch_one) is covered structurally by
    // `tests/atspi_tree_walk_bench.rs` and behaviorally by the GTK e2e suite;
    // these pin the *output format* byte-for-byte so the parallelization can't
    // silently change the snapshot a Locator's XPath runs against.

    #[allow(clippy::too_many_arguments)]
    fn raw(
        bus: &str,
        path: &str,
        depth: usize,
        role: &str,
        name: &str,
        states: StateSet,
        attrs: &[(&str, &str)],
        bounds: Option<Rect>,
        had_children: bool,
        children: &[(&str, &str)],
    ) -> RawNode {
        RawNode {
            bus: bus.to_string(),
            path: path.to_string(),
            depth,
            raw_role: role.to_string(),
            name: name.to_string(),
            // The format tests don't exercise description; the dedicated
            // `serialize_node_emits_description` test builds a node directly.
            description: String::new(),
            states,
            attrs: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            bounds,
            had_children,
            children: children
                .iter()
                .map(|(b, p)| (b.to_string(), p.to_string()))
                .collect(),
        }
    }

    fn key(bus: &str, path: &str) -> (String, String) {
        (bus.to_string(), path.to_string())
    }

    #[test]
    fn serialize_node_matches_sequential_format() {
        // A frame with two resolvable children and one that failed to build
        // (absent from the map → skipped, exactly as the old walk's `continue`).
        let mut nodes: HashMap<(String, String), RawNode> = HashMap::new();
        nodes.insert(
            key("app", "/root"),
            raw(
                "app",
                "/root",
                0,
                "frame",
                "Main",
                StateSet::empty(),
                &[],
                Some(Rect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 50,
                }),
                true,
                &[("app", "/a"), ("app", "/b"), ("app", "/missing")],
            ),
        );
        nodes.insert(
            key("app", "/a"),
            raw(
                "app",
                "/a",
                1,
                "push button",
                "OK",
                StateSet::new(State::Showing | State::Enabled),
                &[("toolkit", "gtk")],
                None,
                false,
                &[],
            ),
        );
        nodes.insert(
            key("app", "/b"),
            raw(
                "app",
                "/b",
                1,
                "label",
                "",
                StateSet::empty(),
                &[],
                None,
                false,
                &[],
            ),
        );

        let mut out = String::new();
        serialize_node(&nodes, &key("app", "/root"), 0, &mut out);

        // Note the exact shape: role always present; name only when non-empty;
        // states in EMITTED_STATES order; bbox when bounds present; toolkit
        // attrs after; `_ref` last; 2-space indent per depth; self-closing
        // leaves; the unresolved `/missing` child contributes nothing.
        let expected = concat!(
            "<Frame role=\"frame\" name=\"Main\" bbox=\"0,0,100,50\" _ref=\"app|/root\">\n",
            "  <PushButton role=\"push button\" name=\"OK\" showing=\"true\" enabled=\"true\" toolkit=\"gtk\" _ref=\"app|/a\"/>\n",
            "  <Label role=\"label\" _ref=\"app|/b\"/>\n",
            "</Frame>\n",
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn serialize_node_emits_description_after_name() {
        // The `accessible-description` is emitted as a `description=` attribute,
        // positioned right after `name` and only when non-empty (issue #55).
        let mut nodes: HashMap<(String, String), RawNode> = HashMap::new();
        let mut node = raw(
            "app",
            "/btn",
            0,
            "push button",
            "Close Search",
            StateSet::empty(),
            &[],
            None,
            false,
            &[],
        );
        node.description = "Close the search bar".to_string();
        nodes.insert(key("app", "/btn"), node);

        let mut out = String::new();
        serialize_node(&nodes, &key("app", "/btn"), 0, &mut out);
        assert_eq!(
            out,
            "<PushButton role=\"push button\" name=\"Close Search\" \
             description=\"Close the search bar\" _ref=\"app|/btn\"/>\n"
        );

        // Detailed XPath evaluation surfaces it on ElementInfo, and the
        // description attribute is treated as a builtin (not a toolkit attr).
        let infos = evaluate_xpath_detailed(&out, "//PushButton").unwrap();
        assert_eq!(infos.len(), 1);
        assert_eq!(
            infos[0].description.as_deref(),
            Some("Close the search bar")
        );
        assert!(!infos[0].attributes.contains_key("description"));
    }

    #[test]
    fn serialize_node_opens_and_closes_when_children_unresolved() {
        // had_children=true but every child ref is absent from the map (all
        // failed to build) → an open+close pair, never self-closed. Mirrors the
        // old walk, whose self-close decision keyed off the *raw* GetChildren
        // reply being non-empty, not on the children resolving.
        let mut nodes: HashMap<(String, String), RawNode> = HashMap::new();
        nodes.insert(
            key("app", "/p"),
            raw(
                "app",
                "/p",
                0,
                "panel",
                "",
                StateSet::empty(),
                &[],
                None,
                true,
                &[("app", "/gone")],
            ),
        );
        let mut out = String::new();
        serialize_node(&nodes, &key("app", "/p"), 0, &mut out);
        assert_eq!(out, "<Panel role=\"panel\" _ref=\"app|/p\">\n</Panel>\n");
    }

    #[test]
    fn serialize_node_self_closes_past_depth_cap() {
        // Rendered past the depth cap, a node self-closes and does not recurse
        // even though it has children in the map — the guard that terminates a
        // cyclic tree (the fetch keys on path, so a cycle is captured once and
        // rendering, not fetching, bounds the expansion).
        let mut nodes: HashMap<(String, String), RawNode> = HashMap::new();
        nodes.insert(
            key("app", "/deep"),
            raw(
                "app",
                "/deep",
                MAX_DEPTH + 1,
                "panel",
                "",
                StateSet::empty(),
                &[],
                None,
                true,
                &[("app", "/child")],
            ),
        );
        nodes.insert(
            key("app", "/child"),
            raw(
                "app",
                "/child",
                MAX_DEPTH + 2,
                "label",
                "childname",
                StateSet::empty(),
                &[],
                None,
                false,
                &[],
            ),
        );

        let mut out = String::new();
        serialize_node(&nodes, &key("app", "/deep"), MAX_DEPTH + 1, &mut out);

        assert!(
            out.trim_end().ends_with("/>"),
            "node past the depth cap should self-close: {out:?}"
        );
        assert!(
            !out.contains("/child") && !out.contains("childname"),
            "must not recurse past the depth cap: {out:?}"
        );
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

    #[test]
    fn map_action_err_treats_io_timeout_as_stale() {
        // A transport-level I/O timeout (a not-yet-ready a11y bridge, e.g. a
        // second window's Text interface) must map to a retriable ElementStale,
        // not a terminal Atspi error.
        let err = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "timed out",
        )));
        let mapped = map_action_err(
            "(//Terminal)[2]",
            ":1.42",
            "/org/a11y/atspi/accessible/7",
            err,
        );
        assert!(
            matches!(mapped, Error::ElementStale { .. }),
            "got {mapped:?}"
        );
    }

    #[test]
    fn map_action_err_keeps_non_timeout_io_terminal() {
        // A hard transport failure (connection reset) isn't recoverable by
        // retrying, so it stays a terminal Atspi error.
        let err = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        )));
        let mapped = map_action_err("//Terminal", ":1.7", "/org/a11y/atspi/accessible/3", err);
        assert!(
            !matches!(mapped, Error::ElementStale { .. }),
            "got {mapped:?}"
        );
    }

    // ── Rect / bbox ────────────────────────────────────────────────────────

    #[test]
    fn rect_bbox_roundtrip() {
        let r = Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 30,
        };
        assert_eq!(r.to_bbox_string(), "10,20,100,30");
        assert_eq!(Rect::parse_bbox("10,20,100,30"), Some(r));
    }

    #[test]
    fn rect_bbox_handles_negative_coords() {
        // Scrolled-off-screen elements report negative offsets.
        let r = Rect::parse_bbox("-50,-10,200,40").unwrap();
        assert_eq!(r.x, -50);
        assert_eq!(r.y, -10);
        assert_eq!(r.width, 200);
        assert_eq!(r.height, 40);
    }

    #[test]
    fn rect_bbox_rejects_malformed() {
        // Missing components — treated as "no bounds" rather than a panic
        // so a malformed snapshot attribute doesn't poison downstream callers.
        assert_eq!(Rect::parse_bbox(""), None);
        assert_eq!(Rect::parse_bbox("10,20,30"), None);
        assert_eq!(Rect::parse_bbox("10,20,30,40,50"), None);
        assert_eq!(Rect::parse_bbox("a,b,c,d"), None);
        assert_eq!(Rect::parse_bbox("10;20;30;40"), None);
    }

    #[test]
    fn evaluate_xpath_detailed_populates_bounds_when_bbox_present() {
        let xml = r#"<?xml version="1.0"?>
<Application name="app" _ref="bus|/app">
  <Button role="button" name="ok" showing="true" bbox="12,34,100,28"
          _ref="bus|/ok"/>
  <Button role="button" name="no-bbox" _ref="bus|/none"/>
</Application>"#;
        let matches = evaluate_xpath_detailed(xml, "//Button").unwrap();
        assert_eq!(matches.len(), 2);
        let ok = &matches[0];
        assert_eq!(ok.name.as_deref(), Some("ok"));
        assert_eq!(
            ok.bounds,
            Some(Rect {
                x: 12,
                y: 34,
                width: 100,
                height: 28,
            })
        );
        let no_bbox = &matches[1];
        assert!(no_bbox.bounds.is_none());
        // bbox attribute should not leak into the generic attributes map
        // (it's in SNAPSHOT_BUILTINS).
        assert!(!ok.attributes.contains_key("bbox"));
    }

    #[test]
    fn rect_is_inside_fully_contained() {
        let outer = Rect {
            x: 0,
            y: 0,
            width: 1024,
            height: 768,
        };
        let inner = Rect {
            x: 100,
            y: 200,
            width: 50,
            height: 20,
        };
        assert!(inner.is_inside(&outer));
    }

    #[test]
    fn rect_is_inside_partial_overlap_left() {
        let outer = Rect {
            x: 10,
            y: 10,
            width: 100,
            height: 100,
        };
        // Starts before outer.x — partially off to the left.
        let straddles = Rect {
            x: 0,
            y: 20,
            width: 30,
            height: 20,
        };
        assert!(!straddles.is_inside(&outer));
    }

    #[test]
    fn rect_is_inside_partial_overlap_bottom() {
        let outer = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        // Bottom edge (y=110) extends past outer's bottom (100).
        let straddles = Rect {
            x: 10,
            y: 90,
            width: 50,
            height: 20,
        };
        assert!(!straddles.is_inside(&outer));
    }

    #[test]
    fn rect_is_inside_exact_match() {
        // Edge-touching counts as inside — a widget flush with its
        // viewport's edges is "in view," not partially clipped.
        let r = Rect {
            x: 5,
            y: 5,
            width: 20,
            height: 20,
        };
        assert!(r.is_inside(&r));
    }

    #[test]
    fn rect_is_inside_disjoint() {
        let outer = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let far = Rect {
            x: 500,
            y: 500,
            width: 10,
            height: 10,
        };
        assert!(!far.is_inside(&outer));
    }

    #[test]
    fn rect_geometry_accessors() {
        let r = Rect {
            x: 10,
            y: 20,
            width: 40,
            height: 80,
        };
        assert_eq!(r.right(), 50);
        assert_eq!(r.bottom(), 100);
        assert_eq!(r.center_x(), 30);
        assert_eq!(r.center_y(), 60);
    }

    #[test]
    fn evaluate_xpath_detailed_malformed_bbox_yields_no_bounds() {
        // Parse errors on bbox fall through to `bounds: None` without
        // aborting the whole evaluation — a strict failure here would
        // make one bad node poison the whole snapshot.
        let xml = r#"<?xml version="1.0"?>
<Application _ref="bus|/app">
  <Button role="button" bbox="not-a-rect" _ref="bus|/b"/>
</Application>"#;
        let matches = evaluate_xpath_detailed(xml, "//Button").unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].bounds.is_none());
    }
}
