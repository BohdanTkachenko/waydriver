//! Typed AT-SPI roles for the ergonomic locator helpers.
//!
//! [`Role`] is a thin, readable shorthand over the element names that appear as
//! the node-test in an XPath selector. It carries no querying logic of its own:
//! [`Session::find_by_role`](crate::Session::find_by_role) and
//! [`find_by_role_id`](crate::Session::find_by_role_id) compile
//! [`Role::element_names`] into a node-test (a union for roles whose snapshot
//! tags differ — see below), the typed counterpart to the string-based
//! [`find_by_role_name`](crate::Session::find_by_role_name).
//!
//! Variant names are idiomatic CamelCase and equal their reference
//! [`element_name`](Role::element_name) — the tag as it appears in a node-test.
//! Those tags are what GTK4 / libadwaita actually expose over AT-SPI (verified
//! by dumping the fixture tree), not the classic AT-SPI role strings, and the
//! toolkit's names are sometimes surprising: `TextBox` for an entry, `Meter`
//! for a level bar, `Radio` for a radio button.
//!
//! waydriver's two snapshot paths disagree on that tag for some roles: the
//! `GetChildren` walk uses GTK4's `GetRoleName` while the `Cache.GetItems` path
//! uses the `atspi` role table (`CheckBox` vs `Checkbox`, `TextBox` vs `Text`,
//! `Tab` vs `PageTab`). [`Role::element_names`] carries both spellings so a
//! typed lookup resolves from the cache directly instead of falling back to the
//! walk.

/// A common GTK4 / libadwaita accessibility role, used as typed shorthand for
/// the element name in a locator query.
///
/// The named variants cover the roles the GTK4/libadwaita widget set exposes.
/// For anything else, use [`Role::Other`] with the element name as it appears
/// in the tree, or fall back to the string API
/// [`Session::find_by_role_name`](crate::Session::find_by_role_name).
///
/// ```no_run
/// use waydriver::Role;
/// # async fn ex(session: &std::sync::Arc<waydriver::Session>) -> waydriver::Result<()> {
/// session.find_by_role(Role::Button, "Sign in").click().await?;
/// session.find_by_role(Role::TextBox, "username").fill("alice").await?;
/// session.find_by_role(Role::Other("Calendar".into()), "May").click().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// A push button (`Button`).
    Button,
    /// A two-state toggle button (`ToggleButton`).
    ToggleButton,
    /// A check box (`CheckBox`).
    CheckBox,
    /// A radio button (`Radio`).
    Radio,
    /// A switch (`Switch`).
    Switch,
    /// A drop-down / combo box (`ComboBox`).
    ComboBox,
    /// A text entry or multi-line text view (`TextBox`).
    TextBox,
    /// A spin button (`SpinButton`).
    SpinButton,
    /// A slider / scale (`Slider`).
    Slider,
    /// A scroll bar (`ScrollBar`).
    ScrollBar,
    /// A progress bar (`ProgressBar`).
    ProgressBar,
    /// A level bar / meter (`Meter`).
    Meter,
    /// A static text label (`Label`).
    Label,
    /// A hyperlink (`Link`).
    Link,
    /// An image (`Image`).
    Image,
    /// A list container (`List`).
    List,
    /// A list item / row (`ListItem`).
    ListItem,
    /// A separator (`Separator`).
    Separator,
    /// A notebook page tab (`Tab`).
    Tab,
    /// The tab strip of a notebook (`TabList`).
    TabList,
    /// The content panel behind a tab (`TabPanel`).
    TabPanel,
    /// A menu item (`MenuItem`).
    MenuItem,
    /// A dialog (`Dialog`).
    Dialog,
    /// A top-level window (`Window`).
    Window,
    /// Escape hatch for any role not named above: its element name used
    /// verbatim as the XPath node-test, e.g. `Role::Other("Calendar".into())`.
    Other(String),
}

impl Role {
    /// The element name this role compiles to in the XPath node-test, e.g.
    /// `Role::Button` → `"Button"`. These are the tags GTK4/libadwaita actually
    /// emit over AT-SPI (verified against the fixture's accessibility tree).
    pub fn element_name(&self) -> &str {
        match self {
            Role::Button => "Button",
            Role::ToggleButton => "ToggleButton",
            Role::CheckBox => "CheckBox",
            Role::Radio => "Radio",
            Role::Switch => "Switch",
            Role::ComboBox => "ComboBox",
            Role::TextBox => "TextBox",
            Role::SpinButton => "SpinButton",
            Role::Slider => "Slider",
            Role::ScrollBar => "ScrollBar",
            Role::ProgressBar => "ProgressBar",
            Role::Meter => "Meter",
            Role::Label => "Label",
            Role::Link => "Link",
            Role::Image => "Image",
            Role::List => "List",
            Role::ListItem => "ListItem",
            Role::Separator => "Separator",
            Role::Tab => "Tab",
            Role::TabList => "TabList",
            Role::TabPanel => "TabPanel",
            Role::MenuItem => "MenuItem",
            Role::Dialog => "Dialog",
            Role::Window => "Window",
            Role::Other(s) => s.as_str(),
        }
    }

    /// Every element tag this role can appear as across waydriver's two
    /// snapshot paths, for use as the XPath node-test.
    ///
    /// waydriver derives a node's tag by PascalCasing the AT-SPI role-name
    /// string, and the two snapshot sources disagree on that string for some
    /// roles: the `GetChildren` walk uses GTK4's `GetRoleName` (e.g.
    /// `"check box"` → `CheckBox`) while the `Cache.GetItems` path uses the
    /// `atspi` role table (e.g. `"checkbox"` → `Checkbox`). A selector built
    /// from a single tag would miss one snapshot and only resolve once the
    /// locator falls back to the walk — defeating cache-first resolution.
    ///
    /// So [`find_by_role`](crate::Session::find_by_role) matches the **union**
    /// of these tags, which lets the cache serve the lookup directly while the
    /// walk still resolves on a cold cache. The first entry is always the
    /// reference [`element_name`](Self::element_name); any extras are the
    /// alternate snapshot spellings. Aliases that never occur are harmless — the
    /// accessible-name predicate is always ANDed in, and the walk remains the
    /// correctness backstop. The divergences (`CheckBox`/`Checkbox`,
    /// `TextBox`/`Text`, `Tab`/`PageTab`, `TabList`/`PageTabList`,
    /// `Radio`/`RadioButton`, `Meter`/`LevelBar`) are verified by dumping the
    /// fixture's walk and cache trees; `Window`/`Frame` tracks the same
    /// walk-vs-`atspi`-table split observed for the toplevel.
    pub fn element_names(&self) -> Vec<&str> {
        match self {
            Role::CheckBox => vec!["CheckBox", "Checkbox"],
            Role::TextBox => vec!["TextBox", "Text", "Entry"],
            Role::Radio => vec!["Radio", "RadioButton"],
            Role::Meter => vec!["Meter", "LevelBar"],
            Role::Tab => vec!["Tab", "PageTab"],
            Role::TabList => vec!["TabList", "PageTabList"],
            Role::Window => vec!["Window", "Frame"],
            _ => vec![self.element_name()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Role;

    #[test]
    fn named_roles_map_to_gtk4_element_names() {
        assert_eq!(Role::Button.element_name(), "Button");
        assert_eq!(Role::TextBox.element_name(), "TextBox");
        // The reference tag equals the (CamelCase) variant name.
        assert_eq!(Role::CheckBox.element_name(), "CheckBox");
        assert_eq!(Role::Radio.element_name(), "Radio");
        assert_eq!(Role::Meter.element_name(), "Meter");
    }

    #[test]
    fn other_passes_element_name_through_verbatim() {
        assert_eq!(Role::Other("Calendar".into()).element_name(), "Calendar");
    }

    #[test]
    fn non_divergent_roles_have_a_single_element_name() {
        // A role whose walk and cache tags agree carries just one tag, so the
        // selector stays a plain node-test rather than a union.
        assert_eq!(Role::Button.element_names(), vec!["Button"]);
        assert_eq!(Role::ComboBox.element_names(), vec!["ComboBox"]);
        assert_eq!(
            Role::Other("Calendar".into()).element_names(),
            vec!["Calendar"]
        );
    }

    #[test]
    fn divergent_roles_carry_reference_tag_first_then_aliases() {
        // The reference tag (== element_name) is always first; the alternate
        // snapshot spellings follow.
        for role in [Role::CheckBox, Role::TextBox, Role::Radio, Role::Meter] {
            let names = role.element_names();
            assert_eq!(names[0], role.element_name());
            assert!(names.len() > 1, "{role:?} should expose an alias");
        }
        assert_eq!(Role::CheckBox.element_names(), vec!["CheckBox", "Checkbox"]);
        assert_eq!(Role::Tab.element_names(), vec!["Tab", "PageTab"]);
    }
}
