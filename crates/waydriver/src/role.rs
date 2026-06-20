//! Typed AT-SPI roles for the ergonomic locator helpers.
//!
//! [`Role`] is a thin, readable shorthand over the element names that appear as
//! the node-test in an XPath selector. It carries no querying logic of its own:
//! [`Session::find_by_role`](crate::Session::find_by_role) and
//! [`find_by_role_id`](crate::Session::find_by_role_id) call
//! [`Role::element_name`] and feed the result into the same XPath builders the
//! string-based [`find_by_role_name`](crate::Session::find_by_role_name) uses.
//!
//! The element names match what GTK4 / libadwaita actually emit over AT-SPI —
//! verified by dumping the real accessibility tree of the test fixture, not
//! the classic AT-SPI role strings. The toolkit's names are sometimes
//! surprising (`Checkbox` not `CheckBox`, `Radio` not `RadioButton`, `TextBox`
//! for an entry, `Meter` for a level bar), so each variant is named after the
//! tag it produces.

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
    /// A check box (`Checkbox`).
    Checkbox,
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
            Role::Checkbox => "Checkbox",
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
}

#[cfg(test)]
mod tests {
    use super::Role;

    #[test]
    fn named_roles_map_to_gtk4_element_names() {
        assert_eq!(Role::Button.element_name(), "Button");
        assert_eq!(Role::TextBox.element_name(), "TextBox");
        // GTK4's toolkit names differ from the classic AT-SPI strings.
        assert_eq!(Role::Checkbox.element_name(), "Checkbox");
        assert_eq!(Role::Radio.element_name(), "Radio");
        assert_eq!(Role::Meter.element_name(), "Meter");
    }

    #[test]
    fn other_passes_element_name_through_verbatim() {
        assert_eq!(Role::Other("Calendar".into()).element_name(), "Calendar");
    }
}
