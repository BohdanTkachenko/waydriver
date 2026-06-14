//! Purpose-built GTK4 + libadwaita demo app used as a deterministic e2e
//! fixture.
//!
//! Sections (choose via `--section=` CLI flag or the main menu):
//!
//! - **gtk4** — raw `gtk::Button` / `gtk::Entry` / `gtk::PopoverMenu` / etc.
//!   Tests what bare GTK4 exposes to AT-SPI.
//! - **libadwaita** — `adw::EntryRow`, `adw::ComboRow`, `adw::SwitchRow`,
//!   `adw::ActionRow`, `adw::ButtonRow`, `adw::Dialog`. Tests the widget
//!   classes real-world GNOME apps use.
//! - **dnd** — drag-and-drop source + target, for exercising pointer-based
//!   drag flows.
//! - **lazy-a11y** — minimal repros for two libadwaita lazy-realization
//!   bugs that hide widgets from AT-SPI: a `PreferencesGroup` built with
//!   `visible:false` then flipped on, and an `AdwPreferencesDialog` where
//!   `set_visible_page_name` switches to a never-realized page.
//! - **effects** — buttons that emit "external effects" onto the session
//!   bus (a desktop notification and a portal open-URI request) for
//!   waydriver's mock D-Bus sinks to capture. Both call D-Bus directly.
//!
//! Only the selected section's widgets live in the AT-SPI tree — no
//! hidden sibling subtrees and no "show everything at once" mode. That's
//! deliberate: tabs/ViewStacks skip a11y for inactive pages in GTK4, and
//! mixing all sections into one tree makes selectors ambiguous and
//! tests less focused. Focused tests launch the fixture with
//! `--section=<name>` to isolate the widgets under test; human users
//! explore by clicking the main-menu items to swap sections.
//!
//! ## CLI
//!
//! `--section=<name>` (or the legacy alias `--tab=<name>`). Accepts
//! `gtk4`, `adw` / `libadwaita`, `dnd` / `drag-and-drop`, `lazy-a11y`, or
//! `effects` / `external-effects`. Default is `gtk4`.
//!
//! ## Single-instance CLI forwarding
//!
//! The app is a single-instance `GApplication` with `HANDLES_COMMAND_LINE`.
//! Launching a second instance with extra args forwards them to the primary,
//! whose `command-line` handler prints
//! `fixture-event: command-line-forwarded args=[...]`. Our own `--section`
//! flag is stripped before GApplication sees argv, so it never interferes.
//!
//! ## Main menu
//!
//! The header-bar menu button is a `GtkPopoverMenu` backed by `GMenuModel`
//! — the same widget pattern gnome-calculator uses — whose items are
//! radio-style view-switchers bound to the stateful `app.section` GAction.
//! Clicking one rebuilds the content area without restarting the app. The
//! button's visible label reflects the active section; its a11y name is
//! pinned to `main-menu` so selectors stay stable.
//!
//! ## Naming convention
//!
//! Widgets that use their visible label/title as the accessible name
//! (`Button`, `ToggleButton`, `CheckButton`, Adw `Row` widgets) have the
//! selector identifier as the visible label — the button literally reads
//! `primary-button`. Deliberately ugly; it's a test fixture and having
//! selector names drift from visible text would be a footgun. The
//! header-bar `MenuButton` is an intentional exception: its visible
//! label tracks the active section while its a11y name is pinned to
//! `main-menu` programmatically.
//!
//! Widgets without intrinsic label text (`Entry`, `TextView`, `ListBox`,
//! `ListBoxRow`, `ScrolledWindow`, `DropDown`, `ComboBoxText`, `Label`)
//! get their accessible name via
//! `AccessibleExt::update_property(Property::Label)`.
//!
//! Keep the crate README's inventory table synchronized with this file.
//!
//! ## Action events
//!
//! Every interactive widget prints a `fixture-event: <kind> <name> [key=value ...]`
//! line to stdout when its primary signal fires, flushing after each write.
//! These events are the fixture's ground truth: AT-SPI can tell tests that a
//! widget exists, but not whether a signal handler actually ran after a
//! click/keystroke. `Session::wait_for_stdout_line` consumes them so tests
//! can assert "this action actually did something" without polling a11y
//! state for side effects.

use adw::prelude::*;
use gtk4::{
    gdk, gio, glib, Box as GtkBox, Button, CheckButton, ComboBoxText, DragSource, DropDown,
    DropTarget, Entry, EventControllerMotion, GestureClick, Label, ListBox, ListBoxRow, MenuButton,
    Orientation, PopoverMenu, Scale, ScrolledWindow, StringList, TextView, ToggleButton,
};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const APP_ID: &str = "io.github.bohdantkachenko.waydriver.FixtureGtk";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Gtk4,
    Adw,
    Dnd,
    LazyA11y,
    Effects,
}

impl Section {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "gtk4" => Some(Section::Gtk4),
            "adw" | "libadwaita" => Some(Section::Adw),
            "dnd" | "drag-and-drop" => Some(Section::Dnd),
            "lazy-a11y" | "lazy" => Some(Section::LazyA11y),
            "effects" | "external-effects" => Some(Section::Effects),
            _ => None,
        }
    }

    /// Canonical value for the stateful GAction backing the main-menu
    /// radio items — keeps the menu's checkmark in sync with what's
    /// actually visible.
    fn action_value(self) -> &'static str {
        match self {
            Section::Gtk4 => "gtk4",
            Section::Adw => "adw",
            Section::Dnd => "dnd",
            Section::LazyA11y => "lazy-a11y",
            Section::Effects => "effects",
        }
    }

    /// Human-readable label shown on the header-bar menu button so
    /// testers can see which section is currently active without opening
    /// the menu.
    fn display_name(self) -> &'static str {
        match self {
            Section::Gtk4 => "GTK4",
            Section::Adw => "libadwaita",
            Section::Dnd => "Drag and drop",
            Section::LazyA11y => "Lazy a11y",
            Section::Effects => "External effects",
        }
    }
}

fn main() -> glib::ExitCode {
    // Split our own `--section=`/`--tab=` flag out of argv: the primary parses
    // it to pick the initial section, and the *remaining* argv is handed to
    // GApplication. With HANDLES_COMMAND_LINE, a secondary instance's leftover
    // args are forwarded to the primary's `command-line` handler — which is how
    // single-instance CLI forwarding is exercised. Keeping our own flag out of
    // what GApplication parses preserves the existing `--section` behavior.
    let (initial, gtk_argv) = split_section_args(std::env::args());

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();

    // `command-line` fires on the primary for its own launch and again for each
    // forwarded secondary invocation. Build the UI on the first call; report the
    // forwarded args on later ones so tests can observe the forwarding.
    let built = Cell::new(false);
    app.connect_command_line(move |app, cmdline| {
        let argv: Vec<String> = cmdline
            .arguments()
            .into_iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if !built.replace(true) {
            build_ui(app, initial);
        } else {
            // Forwarded from a secondary instance: report what it sent
            // (skipping argv[0], the program name).
            let forwarded: Vec<String> = argv.into_iter().skip(1).collect();
            emit(&format!("command-line-forwarded args={forwarded:?}"));
            if let Some(win) = app.active_window() {
                win.present();
            }
        }
        glib::ExitCode::SUCCESS
    });

    // Safety net: a bare `activate` (no command line) just re-presents.
    app.connect_activate(|app| {
        if let Some(win) = app.active_window() {
            win.present();
        }
    });

    app.run_with_args(&gtk_argv)
}

/// Pull the first `--section=`/`--tab=` flag out of `args`, returning the chosen
/// section plus the remaining argv (our flag removed, `argv[0]` preserved) to
/// hand to GApplication.
fn split_section_args(args: impl IntoIterator<Item = String>) -> (Section, Vec<String>) {
    let mut section = Section::Gtk4;
    let mut rest = Vec::new();
    for arg in args {
        let value = arg
            .strip_prefix("--section=")
            .or_else(|| arg.strip_prefix("--tab="));
        if let Some(value) = value {
            section = Section::from_str(value).unwrap_or_else(|| {
                eprintln!("unknown --section value {value:?}; defaulting to gtk4");
                Section::Gtk4
            });
        } else {
            rest.push(arg);
        }
    }
    (section, rest)
}

fn build_ui(app: &adw::Application, initial: Section) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("waydriver-fixture-gtk")
        .default_width(720)
        .default_height(640)
        .build();

    let toolbar = adw::ToolbarView::new();

    // The content area is swappable — main-menu actions can replace it
    // without rebuilding the whole window. Keep a handle to it in an
    // Rc<RefCell<...>> so the GAction closures can mutate it.
    let content_slot = adw::Bin::new();
    content_slot.set_child(Some(&build_section(initial, &window)));
    let content_slot = Rc::new(content_slot);

    let menu_button = build_menu_button(initial);

    // Stateful "section" action — radio-style menu items bind to it and
    // activating one fires the closure below.
    let section_action = gio::SimpleAction::new_stateful(
        "section",
        Some(glib::VariantTy::STRING),
        &initial.action_value().to_variant(),
    );
    {
        let content_slot = content_slot.clone();
        let window = window.clone();
        let menu_button = menu_button.clone();
        section_action.connect_activate(move |action, target| {
            let Some(name) = target.and_then(|v| v.get::<String>()) else {
                return;
            };
            if let Some(next) = Section::from_str(&name) {
                action.set_state(&next.action_value().to_variant());
                content_slot.set_child(Some(&build_section(next, &window)));
                menu_button.set_label(next.display_name());
            }
        });
    }
    app.add_action(&section_action);

    // GAction test affordances (issue #33). Plain `app.*` / `win.*` actions
    // wired to stdout side-effects so e2e tests can fire them over
    // `org.gtk.Actions` and observe the handler run — the GAction-only
    // class (popover-menu / tab-overview / dialog items) that never enters
    // the AT-SPI tree or cache and so has no `(bus, path)` to activate.
    let app_ping = gio::SimpleAction::new("ping", None);
    app_ping.connect_activate(|_, _| emit("action-activated app.ping"));
    app.add_action(&app_ping);

    // Parameterised variant: exercises a string target (GMenu's
    // `app.echo::<value>` detailed-name form) round-tripping to the handler.
    let app_echo = gio::SimpleAction::new("echo", Some(glib::VariantTy::STRING));
    app_echo.connect_activate(|_, target| {
        let value = target.and_then(|v| v.get::<String>()).unwrap_or_default();
        emit(&format!("action-activated app.echo param={value:?}"));
    });
    app.add_action(&app_echo);

    // Window-scoped action — exported at `<base>/window/<id>`, the `win.*`
    // surface, distinct from the app action group above.
    let win_ping = gio::SimpleAction::new("ping", None);
    win_ping.connect_activate(|_, _| emit("action-activated win.ping"));
    window.add_action(&win_ping);

    let header = adw::HeaderBar::new();
    header.pack_end(&menu_button);
    toolbar.add_top_bar(&header);

    toolbar.set_content(Some(&*content_slot));
    window.set_content(Some(&toolbar));
    window.present();
}

/// Produce the content widget for a given section. Called at startup and
/// again each time the main-menu section action fires.
fn build_section(section: Section, window: &adw::ApplicationWindow) -> ScrolledWindow {
    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);

    let col = GtkBox::new(Orientation::Vertical, 12);
    col.set_margin_top(12);
    col.set_margin_bottom(12);
    col.set_margin_start(12);
    col.set_margin_end(12);

    match section {
        Section::Gtk4 => append_gtk4_widgets(&col, window),
        Section::Adw => append_adw_widgets(&col, window),
        Section::Dnd => append_dnd_widgets(&col),
        Section::LazyA11y => append_lazy_a11y_widgets(&col, window),
        Section::Effects => append_effects_widgets(&col),
    }

    scroll.set_child(Some(&col));
    scroll
}

// ── GTK4 widgets ───────────────────────────────────────────────────────────

fn append_gtk4_widgets(col: &GtkBox, parent: &adw::ApplicationWindow) {
    col.append(&build_buttons_row());
    col.append(&build_text_input_row());
    col.append(&build_selection_row());
    col.append(&build_list_section());
    col.append(&build_notes_area());
    col.append(&build_scroll_area());
    col.append(&build_value_slider());
    col.append(&build_pointer_targets_row());
    col.append(&build_dialog_row(parent));
}

/// Row of three widgets that exercise the element-scoped pointer methods:
/// `hover-target` (emits `pointer-enter`), `dc-target` (emits `double-click`
/// on `n_press == 2` with the primary button), and `ctx-target` (emits
/// `right-click` when clicked with the secondary button).
fn build_pointer_targets_row() -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);

    // Hover target — a Label with a pointer-motion controller. Emits
    // `pointer-enter hover-target` as soon as the cursor crosses into the
    // widget, which is a stronger signal than `query-tooltip` (the latter
    // fires only after the tooltip delay elapses, making tests flaky).
    let hover = Label::builder()
        .label("hover-target")
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(16)
        .margin_end(16)
        .build();
    hover.add_css_class("card");
    hover.set_tooltip_text(Some("hover tooltip"));
    name(&hover, "hover-target");
    let motion = EventControllerMotion::new();
    motion.connect_enter(|_, _, _| emit("pointer-enter hover-target"));
    motion.connect_leave(|_| emit("pointer-leave hover-target"));
    hover.add_controller(motion);
    row.append(&hover);

    // Double-click target — emits only on `n_press == 2` with the primary
    // button. Using `GestureClick` rather than `Button::connect_clicked`
    // so we can distinguish single vs double click and avoid emitting two
    // separate events for a real double-click.
    let dc = Label::builder()
        .label("dc-target")
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(16)
        .margin_end(16)
        .build();
    dc.add_css_class("card");
    name(&dc, "dc-target");
    let dc_gesture = GestureClick::new();
    dc_gesture.set_button(gdk::BUTTON_PRIMARY);
    dc_gesture.connect_pressed(|_, n_press, _, _| {
        if n_press == 2 {
            emit("double-click dc-target");
        }
    });
    dc.add_controller(dc_gesture);
    row.append(&dc);

    // Right-click target — emits on any press of the secondary button.
    let ctx = Label::builder()
        .label("ctx-target")
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(16)
        .margin_end(16)
        .build();
    ctx.add_css_class("card");
    name(&ctx, "ctx-target");
    let ctx_gesture = GestureClick::new();
    ctx_gesture.set_button(gdk::BUTTON_SECONDARY);
    ctx_gesture.connect_pressed(|_, _, _, _| emit("right-click ctx-target"));
    ctx.add_controller(ctx_gesture);
    row.append(&ctx);

    row
}

fn build_buttons_row() -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    row.append(&instrumented_button("primary-button"));
    row.append(&instrumented_toggle("mode-toggle"));
    row.append(&instrumented_check("agree-check"));
    row
}

fn build_text_input_row() -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    row.append(&Label::new(Some("Name:")));
    let entry = Entry::new();
    entry.set_placeholder_text(Some("enter your name"));
    entry.set_hexpand(true);
    name(&entry, "text-entry");
    entry.connect_changed(|e| emit(&format!("text-changed text-entry text={:?}", e.text())));
    entry.connect_activate(|e| emit(&format!("activated text-entry text={:?}", e.text())));
    // Emit focus events so keyboard-driven tests can wait for the entry
    // to actually have focus before they start typing — GTK4 Entry doesn't
    // implement the AT-SPI `Component` interface, so `Locator::focus()`
    // returns `NotSupported` and tests can't use the a11y path. An
    // `EventControllerFocus` on the entry itself fires reliably on every
    // focus-in; `notify::has-focus` on the outer Entry widget doesn't
    // (the property-change mechanism skips the initial transition when
    // the widget is first grabbed).
    let focus_ctrl = gtk4::EventControllerFocus::new();
    focus_ctrl.connect_enter(|_| emit("focus-acquired text-entry"));
    focus_ctrl.connect_leave(|_| emit("focus-lost text-entry"));
    entry.add_controller(focus_ctrl);
    // Grab keyboard focus after a short delay so the window is fully
    // realized first. `idle_add_local_once` can fire before present()
    // completes, so the grab gets no-op'd. A 100ms timeout is
    // conservative but reliable.
    let entry_for_focus = entry.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
        entry_for_focus.grab_focus();
    });
    row.append(&entry);
    row
}

fn build_selection_row() -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);

    let flavors = StringList::new(&["Vanilla", "Chocolate", "Strawberry"]);
    let dropdown = DropDown::new(Some(flavors), gtk4::Expression::NONE);
    name(&dropdown, "flavor-dropdown");
    dropdown.connect_selected_notify(|d| {
        emit(&format!("selected flavor-dropdown index={}", d.selected()));
    });
    row.append(&dropdown);

    let combo = ComboBoxText::new();
    combo.append(Some("s"), "Small");
    combo.append(Some("m"), "Medium");
    combo.append(Some("l"), "Large");
    combo.set_active_id(Some("m"));
    name(&combo, "size-combo");
    combo.connect_changed(|c| {
        let id = c.active_id().map(|s| s.to_string()).unwrap_or_default();
        emit(&format!("selected size-combo active_id={id:?}"));
    });
    row.append(&combo);

    row
}

fn build_list_section() -> ListBox {
    let list = ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::Single);
    name(&list, "item-list");

    for (i, text) in ["Apple", "Banana", "Cherry"].iter().enumerate() {
        let row = ListBoxRow::new();
        let lbl = Label::new(Some(text));
        lbl.set_halign(gtk4::Align::Start);
        row.set_child(Some(&lbl));
        name(&row, &format!("item-row-{i}"));
        list.append(&row);
    }
    list.connect_row_selected(|_l, row| {
        let index = row.map(|r| r.index()).unwrap_or(-1);
        emit(&format!("row-selected item-list index={index}"));
    });
    list
}

fn build_notes_area() -> ScrolledWindow {
    let scroll = ScrolledWindow::new();
    scroll.set_min_content_height(80);
    let view = TextView::new();
    view.buffer()
        .set_text("Notes — editable multi-line text.\nLine two.\nLine three.");
    name(&view, "notes-area");
    view.buffer().connect_changed(|b| {
        let (start, end) = b.bounds();
        let text = b.text(&start, &end, false);
        emit(&format!("text-changed notes-area text={text:?}"));
    });
    scroll.set_child(Some(&view));
    scroll
}

fn build_scroll_area() -> ScrolledWindow {
    let scroll = ScrolledWindow::new();
    scroll.set_min_content_height(100);
    name(&scroll, "scroll-area");
    // `ListBox` + `ListBoxRow` rather than a `Box` of Labels: rows are
    // (a) always present in the AT-SPI tree regardless of scroll
    // position (unlike off-screen `Button`s, which GTK4 filters out),
    // and (b) genuinely focusable at the AT-SPI level (unlike Labels
    // with `set_focusable(true)`, which don't surface `State::Focusable`
    // through the a11y bridge). Both are needed so `Locator::scroll_into_view`
    // tests can (1) find the target row before scrolling and (2) use
    // the focus-grab fallback when AT-SPI `scroll_to` is unimplemented.
    let list = ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::None);
    for i in 0..40 {
        let row = ListBoxRow::new();
        let lbl = Label::new(Some(&format!("Row {i}")));
        lbl.set_halign(gtk4::Align::Start);
        row.set_child(Some(&lbl));
        name(&row, &format!("scroll-row-{i}"));
        list.append(&row);
    }
    scroll.set_child(Some(&list));
    // Emit a `scrolled` event whenever the vertical adjustment changes
    // value. Tests for `Locator::scroll_into_view` use this as ground
    // truth that scrolling actually happened — distinguishing between
    // "AT-SPI claims success but nothing moved" and a real layout change.
    scroll.vadjustment().connect_value_changed(|adj| {
        emit(&format!("scrolled scroll-area value={:.1}", adj.value()));
    });
    scroll
}

/// A labelled slider exposing the AT-SPI `Value` interface, read back by
/// `Locator::value`. Fixed range 0..100 with an initial value of 25 so a test
/// can assert exact `current` / `minimum` / `maximum` numbers; emits
/// `slider value=<n>` on change so a drive can also be confirmed via stdout.
fn build_value_slider() -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    row.append(&Label::new(Some("Volume")));

    let slider = Scale::with_range(Orientation::Horizontal, 0.0, 100.0, 1.0);
    slider.set_value(25.0);
    slider.set_hexpand(true);
    slider.set_size_request(160, -1);
    name(&slider, "value-slider");
    slider.connect_value_changed(|s| {
        emit(&format!("slider value={:.1}", s.value()));
    });
    row.append(&slider);
    row
}

/// The header-bar main menu. Uses `GMenuModel` to match the exact pattern
/// real-world GNOME apps (including gnome-calculator) use — which lets us
/// test whether MenuItems show up in the AT-SPI tree on this GTK version.
///
/// The visible label tracks the currently-selected section so testers can
/// see at a glance which view is active. The accessible name is pinned to
/// `main-menu` via `AccessibleExt::update_property` so selectors don't
/// have to know which section the fixture booted into.
fn build_menu_button(initial: Section) -> MenuButton {
    let menu = gio::Menu::new();
    menu.append_item(&gio::MenuItem::new(Some("GTK4"), Some("app.section::gtk4")));
    menu.append_item(&gio::MenuItem::new(
        Some("libadwaita"),
        Some("app.section::adw"),
    ));
    menu.append_item(&gio::MenuItem::new(
        Some("Drag and drop"),
        Some("app.section::dnd"),
    ));
    menu.append_item(&gio::MenuItem::new(
        Some("Lazy a11y"),
        Some("app.section::lazy-a11y"),
    ));
    menu.append_item(&gio::MenuItem::new(
        Some("External effects"),
        Some("app.section::effects"),
    ));

    let popover = PopoverMenu::from_model(Some(&menu));

    let menu_button = MenuButton::new();
    menu_button.set_label(initial.display_name());
    menu_button.set_popover(Some(&popover));
    // Pin the a11y name so tests can locate the button regardless of
    // which section label is currently showing.
    name(&menu_button, "main-menu");
    menu_button
}

fn build_dialog_row(parent: &adw::ApplicationWindow) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    let open = instrumented_button("open-dialog");
    let parent = parent.clone();
    open.connect_clicked(move |_| {
        let dialog = gtk4::Window::builder()
            .transient_for(&parent)
            .modal(true)
            .title("sample-dialog")
            .default_width(320)
            .default_height(180)
            .build();
        let inner = GtkBox::new(Orientation::Vertical, 8);
        inner.set_margin_top(16);
        inner.set_margin_bottom(16);
        inner.set_margin_start(16);
        inner.set_margin_end(16);
        inner.append(&Label::new(Some("This is a modal dialog.")));
        let close = instrumented_button("dialog-close");
        let dialog_ref = dialog.clone();
        close.connect_clicked(move |_| dialog_ref.close());
        inner.append(&close);
        dialog.set_child(Some(&inner));
        dialog.connect_close_request(|_| {
            emit("dialog-closed sample-dialog");
            gtk4::glib::Propagation::Proceed
        });
        dialog.present();
        emit("dialog-opened sample-dialog");
    });
    row.append(&open);
    row
}

// ── libadwaita widgets ────────────────────────────────────────────────────

fn append_adw_widgets(col: &GtkBox, parent: &adw::ApplicationWindow) {
    col.append(&build_adw_preferences_group());
    col.append(&build_adw_dialog_row(parent));
    col.append(&build_adw_tab_row());
}

/// Middle-click instrumentation: a plain target that reports *which* pointer
/// button GTK actually received (separates "BTN_MIDDLE never delivered" from
/// "AdwTabBar's middle-click-close gesture didn't fire"), plus an
/// `AdwTabBar`/`AdwTabView` with three pages whose built-in middle-click
/// close is observable via the `tab-count` event (n-pages drops on close).
fn build_adw_tab_row() -> GtkBox {
    let col = GtkBox::new(Orientation::Vertical, 4);

    let mid_target = Label::new(Some("mid-target"));
    mid_target.set_size_request(160, 32);
    mid_target.set_can_target(true);
    let click = GestureClick::new();
    // Button 0 = listen to every button; the event reports which arrived.
    click.set_button(0);
    // Capture phase: see the press even if a descendant/ancestor gesture
    // would claim it during bubble.
    click.set_propagation_phase(gtk4::PropagationPhase::Capture);
    click.connect_pressed(|g, n_press, _x, _y| {
        emit(&format!(
            "pressed mid-target button={} n={n_press}",
            g.current_button()
        ));
    });
    mid_target.add_controller(click);
    col.append(&mid_target);

    let view = adw::TabView::new();
    for name in ["tab-one", "tab-two", "tab-three"] {
        let page = view.append(&Label::new(Some(name)));
        page.set_title(name);
    }
    view.connect_n_pages_notify(|v| emit(&format!("tab-count {}", v.n_pages())));
    let bar = adw::TabBar::new();
    bar.set_view(Some(&view));
    // Middle-click close only applies to non-pinned tabs; AdwTabBar enables
    // it by default. Keep the view short — the pages are just labels.
    view.set_size_request(-1, 40);
    col.append(&bar);
    col.append(&view);
    col
}

fn build_adw_preferences_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.set_title("adw-prefs-group");

    let entry_row = adw::EntryRow::builder().title("adw-entry-row").build();
    entry_row.connect_changed(|r| emit(&format!("text-changed adw-entry-row text={:?}", r.text())));
    group.add(&entry_row);

    let combo_row = adw::ComboRow::builder().title("adw-combo-row").build();
    let model = StringList::new(&["Alpha", "Bravo", "Charlie"]);
    combo_row.set_model(Some(&model));
    combo_row.connect_selected_notify(|r| {
        emit(&format!("selected adw-combo-row index={}", r.selected()));
    });
    group.add(&combo_row);

    let switch_row = adw::SwitchRow::builder().title("adw-switch-row").build();
    switch_row.connect_active_notify(|r| {
        emit(&format!("toggled adw-switch-row active={}", r.is_active()));
    });
    group.add(&switch_row);

    let action_row = adw::ActionRow::builder()
        .title("adw-action-row")
        .subtitle("secondary text")
        .activatable(true)
        .build();
    action_row.connect_activated(|_| emit("activated adw-action-row"));
    group.add(&action_row);

    let button_row = adw::ButtonRow::builder().title("adw-button-row").build();
    button_row.connect_activated(|_| emit("activated adw-button-row"));
    group.add(&button_row);

    group
}

fn build_adw_dialog_row(parent: &adw::ApplicationWindow) -> GtkBox {
    let row = GtkBox::new(Orientation::Horizontal, 8);
    let open = instrumented_button("open-adw-dialog");
    let parent = parent.clone();
    open.connect_clicked(move |_| {
        let dialog = adw::Dialog::builder()
            .title("adw-sample-dialog")
            .content_width(320)
            .content_height(180)
            .build();

        let content = GtkBox::new(Orientation::Vertical, 12);
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        content.set_margin_start(24);
        content.set_margin_end(24);
        content.append(&Label::new(Some("This is an AdwDialog.")));
        let close = instrumented_button("adw-dialog-close");
        let dialog_ref = dialog.clone();
        close.connect_clicked(move |_| {
            dialog_ref.close();
        });
        content.append(&close);

        dialog.set_child(Some(&content));
        dialog.connect_closed(|_| emit("dialog-closed adw-sample-dialog"));
        dialog.present(Some(&parent));
        emit("dialog-opened adw-sample-dialog");
    });
    row.append(&open);
    row
}

// ── Drag-and-drop widgets ─────────────────────────────────────────────────

const DND_PAYLOAD: &str = "dnd-payload-token";

fn append_dnd_widgets(col: &GtkBox) {
    col.set_spacing(12);
    col.set_margin_top(24);
    col.set_margin_bottom(24);
    col.set_margin_start(24);
    col.set_margin_end(24);

    let status = Label::new(Some("drop-status: ready"));
    status.set_halign(gtk4::Align::Start);
    name(&status, "drop-status");

    col.append(&build_drag_source());
    col.append(&build_drop_target(&status));
    col.append(&status);
}

/// Draggable Label carrying the fixed [`DND_PAYLOAD`] string. Wrapping in
/// a `Frame` turned out to overwrite the child Label's accessible name
/// with the frame's title (the frame's `Some("Drag from here")` shows up
/// as `name="Drag from here"` on the whole subtree and shadows the
/// child's own name) — so we use a flat Button-sized Label with a
/// visible border via a CSS class instead.
fn build_drag_source() -> Label {
    let label = Label::builder()
        .label("drag-source")
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    label.add_css_class("card");
    name(&label, "drag-source");

    let drag = DragSource::new();
    drag.set_actions(gdk::DragAction::COPY);
    drag.connect_prepare(|_source, _x, _y| {
        Some(gdk::ContentProvider::for_value(&DND_PAYLOAD.to_value()))
    });
    drag.connect_drag_begin(|_source, _drag| {
        emit(&format!("drag-started drag-source payload={DND_PAYLOAD:?}"));
    });
    drag.connect_drag_end(|_source, _drag, delete_data| {
        emit(&format!("drag-ended drag-source delete_data={delete_data}"));
    });
    label.add_controller(drag);

    label
}

fn build_drop_target(status: &Label) -> Label {
    let label = Label::builder()
        .label("drop-target")
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    label.add_css_class("card");
    name(&label, "drop-target");

    // Weak-ref the status label so the drop handler doesn't extend the
    // label's lifetime. The Rc<RefCell<...>> is only needed because
    // `connect_drop` wants an `Fn` closure and we mutate the captured
    // weak ref on each call.
    let status_weak = Rc::new(RefCell::new(status.downgrade()));

    let drop = DropTarget::new(String::static_type(), gdk::DragAction::COPY);
    drop.connect_enter(|_target, _x, _y| {
        emit("drag-entered drop-target");
        gdk::DragAction::COPY
    });
    drop.connect_leave(|_target| {
        emit("drag-left drop-target");
    });
    drop.connect_drop(move |_target, value, _x, _y| {
        let payload = value
            .get::<String>()
            .unwrap_or_else(|_| "<non-string>".to_string());
        if let Some(status) = status_weak.borrow().upgrade() {
            status.set_text(&format!("drop-status: got {payload}"));
        }
        emit(&format!("dropped drop-target payload={payload:?}"));
        true
    });
    label.add_controller(drop);

    label
}

// ── Lazy-a11y repros (libadwaita realization bugs) ───────────────────────
//
// Two minimal reproductions of libadwaita widgets that exist on screen
// but never enter the AT-SPI tree because their accessible subtree is
// built lazily on first realization and not rebuilt on later visibility
// or page-switch changes.
//
// `hidden-group` repro — hidden-then-shown PreferencesGroup *inside an
// AdwPreferencesDialog page*. A naive top-level repro (group → window
// content) does NOT trigger the bug — the group's accessibles surface
// fine. The bug surfaces only when the toggled group is nested inside an
// `AdwPreferencesPage` (the real-world shape: a settings dialog whose
// layout swaps between an empty-state group and a populated-state group).
// Open the dialog via the `open-hidden-group-dialog` button; the dialog
// presents on page1 with a hidden `hidden-group-target` group and flips
// it visible 300ms later. The `lazy-button` `AdwButtonRow` inside remains
// absent from AT-SPI.
//
// `non-initial-page` repro — non-initial AdwPreferencesDialog page. Open
// via `open-non-initial-page-dialog`: the dialog has two pages, calls
// `set_visible_page_name("page2")` right after `present()`. The
// `lazy-switch` `AdwSwitchRow` on page2 is drawn on screen but never enters
// the AT-SPI tree because non-initial pages aren't realized at construction.
//
// The control widget `lazy-control` always appears, so tests can confirm
// the section loaded before opening either dialog.

fn append_lazy_a11y_widgets(col: &GtkBox, parent: &adw::ApplicationWindow) {
    let control = Label::new(Some("lazy-control"));
    name(&control, "lazy-control");
    col.append(&control);

    // hidden-group repro trigger button.
    let open_hidden_group = instrumented_button("open-hidden-group-dialog");
    let parent_for_hidden_group = parent.clone();
    open_hidden_group.connect_clicked(move |_| {
        let dialog = adw::PreferencesDialog::new();

        let page1 = adw::PreferencesPage::builder()
            .name("page1")
            .title("Page 1")
            .build();

        // Visible-from-the-start group acts as a sanity check: its rows
        // do surface in AT-SPI, so tests can confirm the dialog itself
        // is queryable before asserting on the absent lazy widgets.
        let control_group = adw::PreferencesGroup::builder()
            .title("hidden-group-control-group")
            .build();
        let control_row = adw::ButtonRow::builder()
            .title("hidden-group-control-row")
            .build();
        control_row.connect_activated(|_| emit("activated hidden-group-control-row"));
        control_group.add(&control_row);
        page1.add(&control_group);

        // Hidden-at-construction group; flipped visible after present().
        // The `lazy-button` inside is the widget we expect to be missing
        // from AT-SPI even though it renders on screen.
        let target_group = adw::PreferencesGroup::builder()
            .title("hidden-group-target-group")
            .build();
        target_group.set_visible(false);
        let lazy_button = adw::ButtonRow::builder().title("lazy-button").build();
        lazy_button.connect_activated(|_| emit("activated lazy-button"));
        target_group.add(&lazy_button);
        page1.add(&target_group);

        dialog.add(&page1);
        dialog.connect_closed(|_| emit("dialog-closed hidden-group-dialog"));
        dialog.present(Some(&parent_for_hidden_group));
        emit("dialog-opened hidden-group-dialog");

        let group_for_show = target_group.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            group_for_show.set_visible(true);
            emit("lazy-shown hidden-group-target-group");
        });
    });
    col.append(&open_hidden_group);

    // non-initial-page repro trigger button.
    let open_non_initial_page = instrumented_button("open-non-initial-page-dialog");
    let parent_for_non_initial_page = parent.clone();
    open_non_initial_page.connect_clicked(move |_| {
        let dialog = adw::PreferencesDialog::new();

        let page1 = adw::PreferencesPage::builder()
            .name("page1")
            .title("Page 1")
            .build();
        let p1_group = adw::PreferencesGroup::builder()
            .title("non-initial-page-control-group")
            .build();
        page1.add(&p1_group);

        let page2 = adw::PreferencesPage::builder()
            .name("page2")
            .title("Page 2")
            .build();
        let p2_group = adw::PreferencesGroup::builder()
            .title("non-initial-page-target-group")
            .build();
        let switch = adw::SwitchRow::builder().title("lazy-switch").build();
        switch.connect_active_notify(|s| {
            emit(&format!("toggled lazy-switch active={}", s.is_active()));
        });
        p2_group.add(&switch);
        page2.add(&p2_group);

        dialog.add(&page1);
        dialog.add(&page2);
        dialog.connect_closed(|_| emit("dialog-closed non-initial-page-dialog"));
        dialog.present(Some(&parent_for_non_initial_page));
        dialog.set_visible_page_name("page2");
        emit("dialog-opened non-initial-page-dialog");
    });
    col.append(&open_non_initial_page);
}

// ── External-effect widgets ──────────────────────────────────────────────
//
// Two buttons that emit "external effects" onto the session bus — a desktop
// notification and a portal open-URI request — for waydriver's mock sinks to
// capture. Both make the D-Bus call directly via gio (exactly what libnotify /
// the portal helper put on the wire), so the test is deterministic and doesn't
// depend on GLib's notification-backend / portal-routing heuristics.

const EFFECTS_NOTIFY_SUMMARY: &str = "fixture-notification";
const EFFECTS_NOTIFY_BODY: &str = "fixture body text";
const EFFECTS_OPEN_URI: &str = "https://example.com/waydriver";

fn append_effects_widgets(col: &GtkBox) {
    let fire = instrumented_button("fire-notification");
    fire.connect_clicked(|_| match send_fixture_notification() {
        Ok(id) => emit(&format!("notification-sent fire-notification id={id}")),
        Err(e) => emit(&format!("notification-error fire-notification error={e}")),
    });
    col.append(&fire);

    let open = instrumented_button("open-uri");
    open.connect_clicked(|_| match request_open_uri(EFFECTS_OPEN_URI) {
        Ok(()) => emit(&format!(
            "open-uri-requested open-uri uri={EFFECTS_OPEN_URI:?}"
        )),
        Err(e) => emit(&format!("open-uri-error open-uri error={e}")),
    });
    col.append(&open);
}

/// Post a desktop notification via the freedesktop `Notify` method, returning
/// the id the daemon (waydriver's mock sink) assigned.
fn send_fixture_notification() -> Result<u32, glib::Error> {
    let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
    let hints = glib::VariantDict::new(None).end(); // a{sv}
    let actions: Vec<String> = Vec::new();
    let params = glib::Variant::tuple_from_iter([
        "waydriver-fixture".to_variant(),    // app_name: s
        0u32.to_variant(),                   // replaces_id: u
        "dialog-information".to_variant(),   // app_icon: s
        EFFECTS_NOTIFY_SUMMARY.to_variant(), // summary: s
        EFFECTS_NOTIFY_BODY.to_variant(),    // body: s
        actions.to_variant(),                // actions: as
        hints,                               // hints: a{sv}
        (-1i32).to_variant(),                // expire_timeout: i
    ]);
    let reply = conn.call_sync(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        Some(&params),
        None, // reply type unchecked; the reply Variant is still returned
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
    )?;
    Ok(reply.get::<(u32,)>().map(|t| t.0).unwrap_or(0))
}

/// Ask the portal to open `uri` externally via `org.freedesktop.portal.OpenURI`.
fn request_open_uri(uri: &str) -> Result<(), glib::Error> {
    let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
    let options = glib::VariantDict::new(None).end(); // a{sv}
    let params = glib::Variant::tuple_from_iter([
        "".to_variant(),  // parent_window: s
        uri.to_variant(), // uri: s
        options,          // options: a{sv}
    ]);
    conn.call_sync(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.OpenURI",
        "OpenURI",
        Some(&params),
        None, // reply type unchecked; we don't read the returned handle here
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
    )?;
    Ok(())
}

// ── helpers ────────────────────────────────────────────────────────────────

fn name(widget: &impl IsA<gtk4::Accessible>, accessible_name: &str) {
    widget.update_property(&[gtk4::accessible::Property::Label(accessible_name)]);
}

/// Emit a structured event line on stdout, flushing so tests see it
/// without buffering delay. The `fixture-event:` prefix namespaces our
/// lines apart from GTK's warnings (which go to stderr anyway, but the
/// prefix keeps assertions unambiguous).
fn emit(event: &str) {
    use std::io::Write;
    println!("fixture-event: {event}");
    let _ = std::io::stdout().flush();
}

fn instrumented_button(label: &str) -> Button {
    let b = Button::with_label(label);
    let owned_label = label.to_string();
    b.connect_clicked(move |_| emit(&format!("clicked {owned_label}")));
    b
}

fn instrumented_toggle(label: &str) -> ToggleButton {
    let b = ToggleButton::with_label(label);
    let owned_label = label.to_string();
    b.connect_toggled(move |btn| {
        emit(&format!("toggled {owned_label} active={}", btn.is_active()));
    });
    b
}

fn instrumented_check(label: &str) -> CheckButton {
    let b = CheckButton::with_label(label);
    let owned_label = label.to_string();
    b.connect_toggled(move |btn| {
        emit(&format!("checked {owned_label} active={}", btn.is_active()));
    });
    b
}
