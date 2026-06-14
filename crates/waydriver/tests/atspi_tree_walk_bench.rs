//! Benchmark: AT-SPI tree-walk cost (`waydriver::atspi::snapshot_tree`).
//!
//! Issue #11 ("Performance: AT-SPI tree walk cost on large apps") observes that
//! every `Locator` call re-snapshots the whole AT-SPI tree, and that
//! `snapshot_node` issues ~6 *serial* D-Bus round-trips per node
//! (`GetRoleName`, the `Name` property, `GetState`, `GetAttributes`,
//! `Component.GetExtents`, `GetChildren`). For apps with thousands of accessible
//! nodes that is O(N) serial round-trips and degrades noticeably — but there is
//! no large-app fixture in the suite to measure it against.
//!
//! So this test *synthesizes* one. It stands up a mock AT-SPI application on a
//! private `dbus-daemon` that serves a tree of a configurable shape and size,
//! then times the real production walk (`waydriver::atspi::snapshot_tree`)
//! against it. No GTK, no mutter — just the D-Bus surface the walker actually
//! touches, so the numbers reflect the per-node round-trip cost the issue is
//! about and will move when the walk is optimized (e.g. the parallelization
//! suggested in the issue triage).
//!
//! Gated `#[ignore]` (it spawns a `dbus-daemon` and runs for several seconds).
//! Run it and read the printed table with:
//!
//! ```sh
//! cargo test -p waydriver --test atspi_tree_walk_bench -- --ignored --nocapture
//! ```
//!
//! Scale the synthetic tree via env vars (defaults in parens). The defaults are
//! deliberately modest so a routine rerun finishes quickly; crank them up for a
//! "very large" run (e.g. `WAYDRIVER_BENCH_FANOUT=6 WAYDRIVER_BENCH_DEPTH=5`
//! ≈ 9.3k nodes):
//! - `WAYDRIVER_BENCH_LIST_ROWS` (2500) — rows in the wide-list scenario.
//! - `WAYDRIVER_BENCH_FANOUT` (5) / `WAYDRIVER_BENCH_DEPTH` (5) — the balanced
//!   tree shape; node count is `(fanout^(depth+1) - 1) / (fanout - 1)`. Depth is
//!   clamped to the walker's own depth cap (20).
//! - `WAYDRIVER_BENCH_RUNS` (1) — timed walks per scenario; the best is reported.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use waydriver::atspi::{evaluate_xpath, snapshot_tree};
use zbus::zvariant::OwnedObjectPath;
use zbus::{interface, Connection};

// ── private dbus-daemon (same helper shape as tests/external_sinks.rs) ───────

/// Spawn a private session `dbus-daemon` at `<dir>/bus` and return its address
/// plus the child handle (killed on drop by the caller).
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

    for _ in 0..50 {
        if socket.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(socket.exists(), "dbus-daemon socket never appeared");
    (address, child)
}

// ── synthetic tree model ─────────────────────────────────────────────────────

/// One node in the synthetic accessible tree. `children` holds node ids; the
/// object path of node `id` is `/node/{id}`.
struct NodeSpec {
    id: usize,
    role: &'static str,
    name: String,
    children: Vec<usize>,
}

/// A flat list: `frame > list box > {rows} list items`. Models a big
/// `GtkListView` / `GtkTreeView` — shallow but very wide, the shape that makes
/// the sequential per-child walk hurt most.
fn build_wide_list(rows: usize) -> Vec<NodeSpec> {
    let mut nodes = Vec::with_capacity(rows + 2);
    nodes.push(NodeSpec {
        id: 0,
        role: "frame",
        name: "Main Window".to_string(),
        children: vec![1],
    });
    let item_ids: Vec<usize> = (2..rows + 2).collect();
    nodes.push(NodeSpec {
        id: 1,
        role: "list box",
        name: "Files".to_string(),
        children: item_ids.clone(),
    });
    for (row, id) in item_ids.into_iter().enumerate() {
        nodes.push(NodeSpec {
            id,
            role: "list item",
            name: format!("Row {row}"),
            children: Vec::new(),
        });
    }
    nodes
}

/// A balanced k-ary tree: `frame` root, `panel` interior nodes, `push button`
/// leaves. Models a deep widget hierarchy (file manager / IDE outline pane).
fn build_balanced(fanout: usize, depth: usize) -> Vec<NodeSpec> {
    let mut nodes = vec![NodeSpec {
        id: 0,
        role: "frame",
        name: "root".to_string(),
        children: Vec::new(),
    }];
    let mut frontier = vec![0usize];
    for d in 0..depth {
        let leaf_level = d + 1 == depth;
        let mut next = Vec::new();
        for parent in frontier {
            for _ in 0..fanout {
                let id = nodes.len();
                nodes.push(NodeSpec {
                    id,
                    role: if leaf_level { "push button" } else { "panel" },
                    name: format!("node-{id}"),
                    children: Vec::new(),
                });
                nodes[parent].children.push(id);
                next.push(id);
            }
        }
        frontier = next;
    }
    nodes
}

// ── mock AT-SPI interfaces (the exact surface snapshot_node touches) ──────────

/// `org.a11y.atspi.Accessible` for one node.
///
/// Returns the wire shapes the `atspi` proxies expect: `GetState` is `au` (a
/// two-`u32` StateSet bitfield), `GetChildren` is `a(so)` whose name field must
/// be a real *unique* bus name — `ObjectRef`'s deserializer asserts a non-empty
/// name and parses it as a `UniqueName`, so each child ref carries this server
/// connection's unique name; an empty/well-known name would make the walker
/// skip every child.
#[derive(Clone)]
struct AccessibleNode {
    role: &'static str,
    name: Arc<str>,
    attributes: Arc<HashMap<String, String>>,
    children: Arc<Vec<(String, OwnedObjectPath)>>,
}

#[interface(name = "org.a11y.atspi.Accessible")]
impl AccessibleNode {
    #[zbus(property)]
    fn name(&self) -> String {
        self.name.to_string()
    }

    fn get_role_name(&self) -> String {
        self.role.to_string()
    }

    fn get_state(&self) -> Vec<u32> {
        // Two u32s = an empty StateSet bitfield. The walk's per-node state
        // formatting is dwarfed by the round-trip cost either way.
        vec![0u32, 0u32]
    }

    fn get_attributes(&self) -> HashMap<String, String> {
        (*self.attributes).clone()
    }

    fn get_children(&self) -> Vec<(String, OwnedObjectPath)> {
        (*self.children).clone()
    }
}

/// `org.a11y.atspi.Component` for one node — only `GetExtents` is on the walk's
/// path. Non-zero size so the snapshot emits a `bbox` (the `width<=0 &&
/// height<=0` branch in `extents_on` would otherwise drop it).
#[derive(Clone)]
struct ComponentNode {
    extents: (i32, i32, i32, i32),
}

#[interface(name = "org.a11y.atspi.Component")]
impl ComponentNode {
    fn get_extents(&self, _coord_type: u32) -> (i32, i32, i32, i32) {
        self.extents
    }
}

/// Register every node's Accessible + Component interface on `conn`'s object
/// server. Child refs are tagged with `bus` (the server's unique name) so the
/// walker can resolve them. Returns how long registration took — excluded from
/// the walk measurement.
async fn serve_tree(
    conn: &Connection,
    bus: &str,
    nodes: &[NodeSpec],
    attributes: &Arc<HashMap<String, String>>,
) -> Duration {
    let t0 = Instant::now();
    for n in nodes {
        let path = format!("/node/{}", n.id);
        let children: Vec<(String, OwnedObjectPath)> = n
            .children
            .iter()
            .map(|c| {
                (
                    bus.to_string(),
                    OwnedObjectPath::try_from(format!("/node/{c}")).expect("valid object path"),
                )
            })
            .collect();
        let accessible = AccessibleNode {
            role: n.role,
            name: Arc::from(n.name.as_str()),
            attributes: attributes.clone(),
            children: Arc::new(children),
        };
        let component = ComponentNode {
            // x, y vary per node; fixed non-zero size keeps bbox emission on.
            extents: (0, n.id as i32, 200, 24),
        };
        conn.object_server()
            .at(path.as_str(), accessible)
            .await
            .expect("register Accessible");
        conn.object_server()
            .at(path.as_str(), component)
            .await
            .expect("register Component");
    }
    t0.elapsed()
}

// ── scenario runner ──────────────────────────────────────────────────────────

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

async fn run_scenario(
    address: &str,
    label: &str,
    nodes: Vec<NodeSpec>,
    target_xpath: &str,
    runs: usize,
) {
    // Fresh server + client connections per scenario: dropping the server
    // connection at the end unregisters its objects, so scenarios don't leak
    // into one another.
    let server = zbus::connection::Builder::address(address)
        .expect("address")
        .build()
        .await
        .expect("server connect");
    let bus = server
        .unique_name()
        .expect("server has a unique name")
        .as_str()
        .to_string();

    let mut attrs = HashMap::new();
    attrs.insert("toolkit".to_string(), "mock-atspi".to_string());
    let attributes = Arc::new(attrs);

    let setup = serve_tree(&server, &bus, &nodes, &attributes).await;

    let client = zbus::connection::Builder::address(address)
        .expect("address")
        .build()
        .await
        .expect("client connect");

    // Timed walks. Keep the last XML for correctness + XPath measurement.
    let mut best = Duration::MAX;
    let mut total = Duration::ZERO;
    let mut xml = String::new();
    for _ in 0..runs {
        let t0 = Instant::now();
        xml = snapshot_tree(&client, &bus, "/node/0")
            .await
            .expect("snapshot_tree");
        let elapsed = t0.elapsed();
        best = best.min(elapsed);
        total += elapsed;
    }

    // Correctness: the walk must have reached every node. If a child ref were
    // wrong (e.g. an empty bus name), those children would be silently skipped
    // and the emitted count would collapse — assert against that.
    let n = nodes.len();
    let emitted = xml.matches("_ref=\"").count();
    assert_eq!(
        emitted, n,
        "{label}: walk emitted {emitted} nodes but the tree has {n} — children were dropped"
    );

    // XPath cost over the resulting document: select-all, plus a targeted
    // single-element query like a real Locator resolves.
    let t_all = Instant::now();
    let all = evaluate_xpath(&xml, "//*").expect("xpath //*");
    let xpath_all = t_all.elapsed();
    assert_eq!(all.len(), n, "{label}: //* should match every node");

    let t_one = Instant::now();
    let one = evaluate_xpath(&xml, target_xpath).expect("xpath target");
    let xpath_one = t_one.elapsed();
    assert_eq!(
        one.len(),
        1,
        "{label}: target query should match exactly one node"
    );

    let per_node_us = best.as_secs_f64() * 1e6 / n as f64;
    let nodes_per_s = n as f64 / best.as_secs_f64();
    let mean = total / runs as u32;

    println!("[bench] {label}");
    println!("        nodes      : {n}");
    println!("        setup      : {setup:.2?} (object registration, excluded)");
    println!("        walk       : {best:.2?} best of {runs} (mean {mean:.2?})");
    println!(
        "        per node   : {per_node_us:.1} µs  ({nodes_per_s:.0} nodes/s, ~6 D-Bus calls/node)"
    );
    println!(
        "        snapshot   : {:.1} KiB XML",
        xml.len() as f64 / 1024.0
    );
    println!("        xpath //*  : {xpath_all:.2?} ({} hits)", all.len());
    println!("        xpath find : {xpath_one:.2?} (1 hit, `{target_xpath}`)");

    drop(client);
    drop(server);
}

// Multi-threaded runtime so the mock server's call dispatch runs concurrently
// with the client walk — mirroring the real topology, where the AT-SPI server
// (the app) and the client (waydriver) are separate processes. On a
// current-thread runtime the two share one OS thread and every round-trip pays
// an extra scheduling hop, inflating the per-node number well above what a real
// two-process setup sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawns a private dbus-daemon and runs for several seconds; run with --ignored --nocapture"]
async fn atspi_tree_walk_bench() {
    let rows = env_usize("WAYDRIVER_BENCH_LIST_ROWS", 2500).max(1);
    let fanout = env_usize("WAYDRIVER_BENCH_FANOUT", 5).max(1);
    // Clamp to the walker's own depth cap (snapshot_node stops below depth 20).
    let depth = env_usize("WAYDRIVER_BENCH_DEPTH", 5).clamp(1, 20);
    let runs = env_usize("WAYDRIVER_BENCH_RUNS", 1).max(1);

    let dir = tempfile::tempdir().expect("tempdir");
    let (address, mut daemon) = spawn_dbus_daemon(dir.path());

    println!("\n=== AT-SPI tree-walk benchmark (issue #11) ===");

    // Wide list: frame > list box > rows. Two structural nodes + `rows` items.
    let last_row = rows - 1;
    run_scenario(
        &address,
        &format!("wide-list (GtkListView-like): {rows} rows"),
        build_wide_list(rows),
        &format!("//ListItem[@name='Row {last_row}']"),
        runs,
    )
    .await;

    // Balanced tree: deep hierarchy.
    let balanced = build_balanced(fanout, depth);
    let last = balanced.len() - 1;
    run_scenario(
        &address,
        &format!("balanced tree: fanout {fanout}, depth {depth}"),
        balanced,
        &format!("//PushButton[@name='node-{last}']"),
        runs,
    )
    .await;

    // The walk is ~6 serial D-Bus round-trips per node (issue #11), so the
    // headline cost is `per-node × N` and grows linearly with the tree. The
    // absolute per-node figure is environment-bound — each round-trip is routed
    // through the bus daemon, so a constrained/virtualized host (like CI
    // sandboxes) sees much higher latency than bare metal — but the linear
    // scaling and the "re-run on every locator call" multiplier are the points.
    println!(
        "note: cost is ~6 serial D-Bus round-trips/node; per-node latency is \
         environment-bound, the linear scaling is not."
    );
    println!("=== end benchmark ===\n");

    let _ = daemon.kill();
    let _ = daemon.wait();
}
