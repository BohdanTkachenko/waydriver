# Design: event-driven AT-SPI tree cache (issue #11)

**Status:** measured & specified — blueprint for the implementation PR.
**Scope of this document:** answer issue [#11]'s five open questions with
measurements from the real GTK bridge, then specify the cache architecture
those numbers support. No cache is implemented yet; this is the "measure &
design first" precursor the issue's triage recommended.

[#11]: https://github.com/BohdanTkachenko/waydriver/issues/11

## TL;DR

Build an **event-gated snapshot cache**, not a live "source-of-truth" mirror.
Keep the authoritative tree walk; skip it when no AT-SPI mutation event has
invalidated the last snapshot. The measurements support this strongly:

- GTK emits **0 events/s when idle**, so the cache stays warm between actions.
- Every driven mutation produced a matching event within a **0–12 ms** window
  of the app-side change — effectively synchronous.
- A trivial **global-dirty** cache (one bit, any mutation ⇒ re-walk) would have
  served **88% of snapshots** warm over a realistic action→auto-wait cadence.
- Worst-case resident footprint is **~5–6 MiB at 50k nodes** — negligible.

Recommended rollout: land it **opt-in** (off by default), keep the walk as the
source of truth, and flip the default once a Qt fixture validates event
reliability there. The larger subtree-incremental invalidation is a measured
follow-up, not part of the first cut.

## Background

`Locator::snapshot()` ([`locator.rs:1683`]) re-walks the whole AT-SPI tree on
every resolution, and the auto-wait loop (`poll_with_retry`) re-snapshots on
every poll — dozens of times per action. #45 added a reference benchmark and
#49 parallelized the walk (~3.6× faster), but the walk is still O(N) per call.
The next step #11 proposes is to avoid the walk entirely when the tree hasn't
changed, by subscribing to AT-SPI mutation events.

[`locator.rs:1683`]: ../../crates/waydriver/src/locator.rs

## Measurement methodology

Two harnesses, both committed alongside this doc:

1. **Synthetic footprint/cost** — `crates/waydriver/tests/atspi_tree_walk_bench.rs`
   (the #45 bench, extended). Mock AT-SPI server on a private `dbus-daemon`, no
   GTK. Reports the resident snapshot size, per-node bytes, a worst-case
   projection, and a cache-hit re-serve timed against the full walk. Runs in
   plain CI:
   ```sh
   cargo test -p waydriver --test atspi_tree_walk_bench -- --ignored --nocapture
   ```
2. **Real-GTK event behavior** — `crates/waydriver-e2e/tests/e2e.rs::atspi_event_cache_measurement`.
   A background task subscribes to the mutation events a cache would mirror
   (`children-changed`, `state-changed`, `property-change`, `text-changed`) and
   drives the fixture through deterministic mutations, each marked by a
   `fixture-event:` stdout line as app-side ground truth. Diagnostic; runs in the
   Fedora dev-container:
   ```sh
   scripts/dev-container.sh bash -lc \
     'cargo build -p waydriver-fixture-gtk && dbus-run-session -- \
      cargo test -p waydriver-e2e --test e2e atspi_event_cache_measurement \
      -- --ignored --nocapture --test-threads=1'
   ```

The numbers below are from one representative run of each (sandboxed CI host /
Fedora dev-container). Absolute walk latency is environment-bound; the
**scaling and the event behavior** are the portable signals.

## The five open questions, answered

### Q1 — Are AT-SPI mutation events reliable enough?

**On GTK: yes, 100% recall in the test.** Every driven mutation produced events
whose target paths matched a before/after fresh-walk diff:

| mutation | events seen | walk delta | app-marker |
|---|---|---|---|
| check toggle (`state-changed`) | 8 | +0 / −0 nodes (state only) | ✓ |
| dialog open (`children-changed` insert) | 8 | **+13** / −0 nodes | ✓ |
| dialog close (`children-changed` delete) | 21 | +0 / **−13** nodes | ✓ |

Note events are **coarser than nodes**: a 13-node subtree insert surfaced as ~4
`children-changed` events on container parents (class totals for the run:
children +4/−13, state 32, property 0, text 0). That is fine for invalidation
(you learn *which parent* changed and re-walk its subtree) but means events are
not a per-node delta stream.

**Caveat — Qt is unmeasured.** There is no Qt fixture in the suite, so the
"reliable across GTK *and* Qt" half of this question is open. The architecture
below keeps the authoritative walk as the source of truth precisely so this gap
does not become a correctness risk.

### Q2 — Consistency model: how stale can the cache be vs. a fresh walk?

**Window measured at 0–12 ms.** For each mutation the event arrived
essentially in lockstep with the app-side change (`window = event_time −
app_marker_time`): +0 ms, +0 ms, +12 ms. So an event-driven cache trails a
fresh walk by at most a few milliseconds after a mutation — negligible against
the auto-wait cadence (≥50 ms polls, multi-second timeouts).

The residual window only exists for *externally* induced changes. For changes
**waydriver itself** causes (the common case: click → tree updates), we can
close it entirely by marking the cache dirty at the end of every mutating
action, so the next resolve always re-walks.

### Q3 — Subscription cost: per-node? overhead when idle?

**Not per-node, and ~zero when idle.** AT-SPI subscription is per *event type*
(a handful of registry match rules: `children-changed`, `state-changed`, …),
**not** per accessible — there is no per-node cost regardless of tree size. With
all four registered, the fixture emitted **0 events over 3 s** while static
(0.0/s). GTK is silent at rest, so the cache is never spuriously invalidated
between actions — the precondition that makes a single global-dirty bit viable.

### Q4 — Fallback when events are unreliable or dropped?

No drops were observed, but zero-drop cannot be proven for all apps from one
fixture. Because the cache is an **optimization over** the authoritative walk
(not a replacement for it), the fallback is simple and global:

- A **`max_age` reconcile**: force a full re-walk if the cached snapshot is
  older than N (e.g. a few seconds) or after M consecutive hits, recovering
  from any missed event within a bounded window.
- **Re-walk on any transport error** while reading the cache's backing
  connection.

Per-app / per-widget fallback is unnecessary at this granularity — a single
"invalidate everything" path covers it.

### Q5 — Memory footprint: acceptable in CI?

**Yes — ~5–6 MiB worst case.** The cache retains the last snapshot (the XML
string the locator's `evaluate_xpath` consumes), so the snapshot size *is* the
resident cost:

| scenario | nodes | resident | per node | projected @ 50k nodes |
|---|---|---|---|---|
| wide list (GtkListView-like) | 2 502 | 275 KiB | 113 B | **~5.4 MiB** |
| balanced tree | 3 906 | 475 KiB | 125 B | **~5.9 MiB** |

Per-node bytes are stable, so the linear projection holds. A subtree-incremental
cache would additionally retain the parsed node map (same order of magnitude).
Either way the footprint is trivial for CI.

## The money metric

Does the cache actually help, or does *something* always change between calls?
Modeling a global-dirty cache over **6 action→auto-wait rounds** (each: one real
mutation, then 8 polls on a 50 ms cadence like `poll_with_retry`):

> **48 snapshots → 42 warm-cache hits / 6 reconciles = 88% served from cache.**

That is the theoretical maximum for this cadence (one forced reconcile per
round, every other poll warm), achieved because GTK is idle-silent (Q3) so the
cache survives each auto-wait window intact. Each hit re-serves the retained
snapshot in tens of microseconds instead of an O(N) walk (the synthetic bench
timed the re-serve at 58–103 µs).

## Recommended architecture

An **event-gated cache** layered over the existing walk. The walk stays the
source of truth; the cache only lets us *skip* it.

```
            ┌─ background task (one AccessibilityConnection) ─┐
            │  register: children/state/property/text-changed │
            │  on any event ⇒ CacheState.dirty = true          │
            └──────────────────────────────────────────────────┘
                                 │ sets
                                 ▼
  Locator::snapshot():   CacheState { dirty: AtomicBool,
     if enabled && !dirty                snapshot: Mutex<Option<String>>,
        && fresh enough  ──hit──▶         stamped_at: Instant }
        ⇒ return cached XML
     else ──miss──▶ snapshot_tree(); store; dirty=false; stamp
```

Key decisions:

1. **Integration point:** `Locator::snapshot()` — a single chokepoint every
   resolution already funnels through. Drop-in; lazy semantics preserved because
   any mutation forces a fresh authoritative walk.
2. **Invalidation granularity: global-dirty first.** One bit. Justified by Q3
   (no idle noise) and the 88% money metric. **Subtree-incremental** (re-walk
   only the changed parent's subtree, patch `state/property` in place) is a
   follow-up — the coarse `children-changed` events make it tractable, but it is
   only worth the complexity if a real large app shows frequent *localized*
   churn between calls. Defer until measured against such a fixture.
3. **Reconcile / fallback:** `max_age` forced re-walk + re-walk on transport
   error (Q4).
4. **Close the self-induced window:** mark dirty at the end of every mutating
   Session action, so the next resolve after a waydriver-caused change re-walks
   (eliminates the ≤12 ms window for the common case).
5. **Concurrency & lifecycle:** one background task owns the event stream and
   updates an `Arc<CacheState>`; start it when the session's a11y connection is
   established, abort it on `Session::kill()`.
6. **Composition with #27:** events never fire for never-realized libadwaita
   subtrees, but the cache is a pure optimization over the walk — it sees exactly
   what the walk sees and no less. `hidden_accessibles()` / `focus_walk()` remain
   the path for lazy-realized content, unchanged.

### Rollout

1. **Opt-in** (off by default) behind a `Session` option. Default behavior —
   fresh walk every call — is untouched, so existing tests and users are
   unaffected and the mechanism can be measured on real workloads.
2. **Default-on** once trusted, gated on a **Qt fixture** validating Q1 there.

## Risks & open items

- **Qt event reliability unmeasured** — no Qt fixture exists. Blocker for
  default-on; not a blocker for opt-in (walk remains source of truth).
- **No real large-app fixture** (Files / Builder / a big `GtkListView`). The
  synthetic bench covers cost/memory scaling and the diagnostic covers event
  semantics, but dropped-event behavior *under heavy real event load* and true
  worst-case memory want a representative fixture. Useful precursor for the
  subtree-incremental follow-up.
- **Subtree-incremental invalidation** deferred to a measured follow-up.

## Implementation sketch (follow-up PR)

- `CacheState { dirty: AtomicBool, snapshot: Mutex<Option<String>>, stamped_at: Mutex<Instant> }`,
  held as `Option<Arc<CacheState>>` on `Session` (Some only when opted in).
- Background subscriber task (mirrors `atspi_event_cache_measurement`'s
  collector): register the four event types, set `dirty` on any event.
- `Locator::snapshot()`: hit/miss/age logic above.
- Mutating `Session` actions set `dirty = true` after acting.
- Tests: pure unit tests for the hit/miss/age state machine; an e2e correctness
  test asserting a cached resolve still reports a just-removed element as gone
  (i.e. the event invalidation actually fires) and a just-added one as present.
