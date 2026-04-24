//! Purpose-built GTK4 + libadwaita demo app used as a deterministic e2e
//! fixture.
//!
//! Sections (choose via `--section=` CLI flag or the main menu):
//!
//! - **gtk4** — raw `gtk::Button` / `gtk::Entry` / `gtk::PopoverMenu` / etc.
//!   Tests what bare GTK4 exposes to AT-SPI.
//! - **libadwaita** — `adw::EntryRow`, `adw::ComboRow`, `adw::SwitchRow`,
//!   `adw::ActionRow`, `adw::Dialog`. Tests the widget classes real-world
//!   GNOME apps use.
//! - **dnd** — drag-and-drop source + target, for exercising pointer-based
//!   drag flows.
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
//! `gtk4`, `adw` / `libadwaita`, or `dnd` / `drag-and-drop`. Default is
//! `gtk4`.
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
    DropTarget, Entry, Label, ListBox, ListBoxRow, MenuButton, Orientation, PopoverMenu,
    ScrolledWindow, StringList, TextView, ToggleButton,
};
use libadwaita as adw;
use std::cell::RefCell;
use std::rc::Rc;

const APP_ID: &str = "io.github.bohdantkachenko.waydriver.FixtureGtk";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Gtk4,
    Adw,
    Dnd,
}

impl Section {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "gtk4" => Some(Section::Gtk4),
            "adw" | "libadwaita" => Some(Section::Adw),
            "dnd" | "drag-and-drop" => Some(Section::Dnd),
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
        }
    }
}

fn main() -> glib::ExitCode {
    let initial = parse_section_from_args();
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build_ui(app, initial));
    // Pass empty args to GTK; we already consumed our CLI flag.
    app.run_with_args::<&str>(&[])
}

fn parse_section_from_args() -> Section {
    for arg in std::env::args().skip(1) {
        let value = arg
            .strip_prefix("--section=")
            .or_else(|| arg.strip_prefix("--tab="));
        if let Some(value) = value {
            return Section::from_str(value).unwrap_or_else(|| {
                eprintln!("unknown --section value {value:?}; defaulting to gtk4");
                Section::Gtk4
            });
        }
    }
    Section::Gtk4
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
    col.append(&build_dialog_row(parent));
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
