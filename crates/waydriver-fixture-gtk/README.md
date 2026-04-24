# waydriver-fixture-gtk

A purpose-built GTK4 + libadwaita demo app used as a deterministic
end-to-end testing fixture for waydriver. Run it manually with:

```sh
cargo run -p waydriver-fixture-gtk                    # default: gtk4
cargo run -p waydriver-fixture-gtk -- --section=adw   # just libadwaita
cargo run -p waydriver-fixture-gtk -- --section=dnd   # just drag-and-drop
```

Or use it as an e2e test fixture target:

```rust
let cfg = SessionConfig {
    command: "waydriver-fixture-gtk".into(),            // or absolute path
    args: vec!["--section=gtk4".into()],                // optional, default "gtk4"
    app_name: "waydriver-fixture-gtk".into(),
    ..
};
```

## Layout

The window is an `AdwApplicationWindow` with an `AdwHeaderBar`. The
header bar has a single `main-menu` button; that menu lets you switch
which widget section is visible. The content area holds one section at
a time — no `AdwViewStack`, no hidden siblings. This matters because
AT-SPI doesn't enumerate widgets in inactive `ViewStack` pages, so
anything not currently visible wouldn't be targetable by tests.

Sections:

1. **gtk4** — raw GTK4 widgets (default)
2. **libadwaita** — Adw Row widgets + Adw dialog
3. **dnd** — drag-and-drop source + target + status label

Only one section is ever live in the a11y tree at a time. There is
no "all" view: mixing sections makes selectors ambiguous and widgets
like `AdwPreferencesGroup` are only meaningfully testable when
they're the focused section.

## CLI

| Flag                                        | Effect                          |
|---------------------------------------------|---------------------------------|
| (no flag)                                   | `--section=gtk4`                |
| `--section=gtk4`                            | Only GTK4 widgets in the tree   |
| `--section=adw` / `--section=libadwaita`    | Only Adw widgets                |
| `--section=dnd` / `--section=drag-and-drop` | Only DnD widgets                |

Legacy alias `--tab=<name>` is accepted for backwards compatibility.

Useful for one-shot tests: launch the fixture with the section under
test and the a11y tree contains only that section's widgets, keeping
XPath queries unambiguous.

## Main menu

The header-bar menu button opens a `GtkPopoverMenu` backed by `GMenuModel`
— deliberately the exact widget pattern gnome-calculator uses — which
lets us test whether `MenuItem` widgets show up in the AT-SPI tree on
this GTK version.

Its **visible label** tracks the currently-selected section ("GTK4",
"libadwaita", "Drag and drop") so a human tester can see at a glance
which view is active. Its **accessible name** is pinned to `main-menu`
via `AccessibleExt::update_property` so test selectors don't need to
know which section the fixture booted into.

The items inside the popover are radio-style view-switcher entries
bound to the stateful `app.section` GAction. Clicking one rebuilds the
content area without restarting the app and updates the button label.

## Action events

Every interactive widget prints a line to stdout when its primary signal
fires, flushing after each write. Tests consume these via
`Session::wait_for_stdout_line` to verify that an action actually ran —
AT-SPI can confirm a widget exists but not whether a click or keystroke
actually produced any effect on the application side.

Format: `fixture-event: <kind> <name> [key=value ...]`

Examples:

```
fixture-event: clicked primary-button
fixture-event: toggled mode-toggle active=true
fixture-event: checked agree-check active=true
fixture-event: text-changed text-entry text="hello"
fixture-event: activated text-entry text="hello"
fixture-event: selected flavor-dropdown index=1
fixture-event: selected size-combo active_id="l"
fixture-event: row-selected item-list index=2
fixture-event: text-changed notes-area text="..."
fixture-event: dialog-opened sample-dialog
fixture-event: dialog-closed sample-dialog
fixture-event: text-changed adw-entry-row text="hi"
fixture-event: selected adw-combo-row index=1
fixture-event: toggled adw-switch-row active=true
fixture-event: activated adw-action-row
fixture-event: dialog-opened adw-sample-dialog
fixture-event: dialog-closed adw-sample-dialog
fixture-event: drag-started drag-source payload="dnd-payload-token"
fixture-event: drag-entered drop-target
fixture-event: drag-left drop-target
fixture-event: drag-ended drag-source delete_data=false
fixture-event: dropped drop-target payload="dnd-payload-token"
```

Quoting: string fields use Rust `{:?}` debug formatting (embedded quotes
escaped), integer fields are bare. Don't rely on this being a stable
serialization format across versions — use `line.contains("clicked foo")`
rather than parsing field-by-field.

## Why this exists

The broader e2e suite runs against `gnome-calculator`, which has
significant AT-SPI gaps — no `Component` interface on any widget, no
`MenuItem` exposure in popovers, custom keyshortcut-based button
activation. Each gap either blocks a feature's validation or forces
awkward workarounds. This fixture pins a known-good set of widgets we
can drive deterministically, covering both raw GTK4 and libadwaita so we
can attribute AT-SPI behaviors to either layer.

Existing gnome-calculator tests stay in place as real-world regression
coverage — the fixture is for *feature validation*, not replacement.

## Naming convention

Widgets whose GTK4/Adw class uses their visible label/title as the
accessible name (`Button`, `ToggleButton`, `CheckButton`, Adw `Row`
widgets) have the selector identifier as the visible label itself —
so the button on screen literally reads `primary-button`. Deliberately
ugly on screen; it's a test fixture, and matching visible text to
selector names means they can't drift.

Widgets without intrinsic label text (`Entry`, `TextView`, `ListBox`,
`ListBoxRow`, `ScrolledWindow`, `DropDown`, `ComboBoxText`, `Label`) get
their accessible name set programmatically via
`AccessibleExt::update_property(Property::Label(...))`.

The header-bar `MenuButton` is an intentional exception: its visible
label tracks the active section, but its accessible name is pinned to
`main-menu` programmatically so selectors stay stable.

## GTK4 tab — widget inventory

| Widget type           | Accessible name   | Feature exercised                       |
|-----------------------|-------------------|-----------------------------------------|
| `gtk::Button`         | `primary-button`  | Action interface, click auto-wait       |
| `gtk::ToggleButton`   | `mode-toggle`     | Toggled state, `is_pressed` predicate   |
| `gtk::CheckButton`    | `agree-check`     | Checked state, `is_checked` predicate   |
| `gtk::Entry`          | `text-entry`      | Text interface, focus, fill, set_text   |
| `gtk::DropDown`       | `flavor-dropdown` | Selection interface (GTK4-native)       |
| `gtk::ComboBoxText`   | `size-combo`      | Selection interface (legacy)            |
| `gtk::ListBox`        | `item-list`       | Selection in list context               |
| `gtk::ListBoxRow`     | `item-row-{n}`    | Individual selectable row               |
| `gtk::TextView`       | `notes-area`      | Multi-line text, read/write             |
| `gtk::ScrolledWindow` | `scroll-area`     | Scroll-into-view fallback target        |
| `gtk::Button` (modal) | `open-dialog`     | Classic GtkWindow-as-dialog trigger     |
| `gtk::Window` (dlg)   | `sample-dialog`   | Nested focus scope                      |

`scroll-area` contains 40 `Label` children named `scroll-row-0` through
`scroll-row-39` so tests can force scrolling.

The `gtk::MenuButton` labelled `main-menu` lives in the header bar (not
a tab) — it uses a `GtkPopoverMenu` + `GMenuModel`, the same pattern
gnome-calculator uses. Running this fixture tells us whether the
empty-popover-children gap we see in calc reproduces with raw GTK4 or
only shows up when wrapped by libadwaita.

## libadwaita tab — widget inventory

| Widget type            | Accessible name      | Feature exercised                                 |
|------------------------|----------------------|---------------------------------------------------|
| `adw::PreferencesGroup`| `adw-prefs-group`    | Adw container; group role + title                 |
| `adw::EntryRow`        | `adw-entry-row`      | Adw's replacement for GtkEntry inside forms       |
| `adw::ComboRow`        | `adw-combo-row`      | Adw combobox (different a11y from GTK)            |
| `adw::SwitchRow`       | `adw-switch-row`     | Row-hosted GtkSwitch; nested toggle a11y          |
| `adw::ActionRow`       | `adw-action-row`     | Ubiquitous list-row pattern                       |
| `adw::Dialog` trigger  | `open-adw-dialog`    | Modern dialog primitive (1.5+)                    |
| `adw::Dialog`          | `adw-sample-dialog`  | Dialog window/content a11y                        |

## DnD tab — widget inventory

| Widget          | Accessible name | Purpose                                          |
|-----------------|-----------------|--------------------------------------------------|
| `gtk::Label`    | `drag-source`   | Source zone. `DragSource` controller carrying a  |
|                 |                 | fixed `dnd-payload-token` string.                |
| `gtk::Label`    | `drop-target`   | Drop zone. `DropTarget` accepting strings.       |
| `gtk::Label`    | `drop-status`   | Live status. Reads `drop-status: ready` until a  |
|                 |                 | drop succeeds, then `drop-status: got <payload>`.|

Expected test flow: drag from `drag-source` to `drop-target`, then wait
for `drop-status` text to change from `ready` to `got dnd-payload-token`.
That verifies pointer-based DnD end-to-end (once element bounds +
pointer-based drag primitives land on the waydriver side).

## When to add widgets

Add a new widget the first time a feature needs one that isn't already
covered. Naming convention:

- Single widget: descriptive kebab-case (`text-entry`, `main-menu`)
- Multiple siblings: base name + index (`item-row-0`, `scroll-row-0`)
- Adw variants get an `adw-` prefix when they pair with a GTK4 widget
- DnD widgets are prefixed `drag-` / `drop-` by role

Keep this README's inventory tables in sync with the source.

## Stale-build gotcha

`cargo test -p waydriver-e2e` does *not* rebuild the fixture
binary — the fixture isn't a dep of `waydriver-e2e`. If you've edited
the fixture since its last build, run:

```sh
cargo build -p waydriver-fixture-gtk
```

first, or the test will run against a stale binary and fail in
confusing ways.
