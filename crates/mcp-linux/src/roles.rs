//! AT-SPI roles and states, mapped onto the OS-independent node vocabulary.
//!
//! The flattener and `find_elements` speak the macOS-derived vocabulary
//! (`button`, `textfield`, `checkbox`, ...) because that is what the tool
//! descriptions promise an agent. AT-SPI names the same things differently
//! (`push button`, `entry`, `check box`), so the mapping lives here, in one
//! place, with the interactive set kept in step with `mcp_a11y::is_interactive_role`.

use atspi::{Role, State, StateSet};

/// The normalized role for an AT-SPI role, plus whether the node is a secure
/// (password) field.
pub fn normalize(role: Role, states: StateSet) -> (String, bool) {
    let secure = role == Role::PasswordText;
    let name = match role {
        Role::Button | Role::PushButtonMenu => "button",
        Role::ToggleButton => "checkbox",
        Role::CheckBox | Role::CheckMenuItem => "checkbox",
        Role::RadioButton | Role::RadioMenuItem => "radiobutton",
        Role::Entry | Role::PasswordText | Role::Autocomplete | Role::Editbar => "textfield",
        Role::Text | Role::DocumentText | Role::Terminal => {
            if states.contains(State::MultiLine) || role != Role::Text {
                "textarea"
            } else {
                "textfield"
            }
        }
        Role::ComboBox => "combobox",
        Role::MenuItem | Role::TearoffMenuItem => "menuitem",
        Role::Menu | Role::PopupMenu => "menu",
        Role::MenuBar => "menubar",
        Role::PageTab => "tab",
        Role::PageTabList => "tabgroup",
        Role::Slider | Role::Dial => "slider",
        Role::SpinButton => "incrementor",
        Role::TreeItem => "treeitem",
        Role::TableCell | Role::TableColumnHeader | Role::TableRowHeader => "cell",
        Role::ListItem => "listitem",
        Role::Link => "link",
        Role::Label | Role::Static | Role::Caption => "statictext",
        Role::Heading => "heading",
        Role::Paragraph | Role::Section | Role::BlockQuote => "text",
        Role::Frame | Role::Window | Role::InternalFrame => "window",
        Role::Dialog | Role::FileChooser | Role::FontChooser | Role::ColorChooser => "dialog",
        Role::Alert | Role::Notification => "alert",
        Role::Application => "application",
        Role::Panel
        | Role::Filler
        | Role::Grouping
        | Role::LayeredPane
        | Role::RootPane
        | Role::GlassPane
        | Role::Viewport
        | Role::ScrollPane
        | Role::SplitPane
        | Role::OptionPane
        | Role::Form
        | Role::Landmark => "group",
        Role::ToolBar => "toolbar",
        Role::StatusBar => "statusbar",
        Role::ScrollBar => "scrollbar",
        Role::ProgressBar | Role::LevelBar => "progressindicator",
        Role::Image | Role::Icon | Role::DesktopIcon => "image",
        Role::List | Role::ListBox => "list",
        Role::Table | Role::TreeTable | Role::Tree => "table",
        Role::TableRow => "row",
        Role::Separator => "splitter",
        Role::ToolTip => "tooltip",
        Role::DocumentWeb | Role::DocumentFrame | Role::HTMLContainer => "webarea",
        Role::Canvas | Role::DrawingArea => "canvas",
        Role::TitleBar => "titlebar",
        Role::InfoBar => "infobar",
        Role::Unknown | Role::Invalid | Role::RedundantObject => "unknown",
        other => return (squash(other.name()), secure),
    };
    (name.to_string(), secure)
}

/// AT-SPI role names contain spaces (`"check box"`); the vocabulary does not.
fn squash(name: &str) -> String {
    name.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The AT-SPI subrole string, kept so the flattener can show what the toolkit
/// actually called the node when the normalized role lost information.
pub fn subrole(role: Role) -> Option<String> {
    match role {
        Role::ToggleButton => Some("toggle button".into()),
        Role::PasswordText => Some("password text".into()),
        Role::Terminal => Some("terminal".into()),
        Role::CheckMenuItem | Role::RadioMenuItem | Role::TearoffMenuItem => {
            Some(role.name().to_string())
        }
        Role::DocumentText | Role::DocumentWeb | Role::DocumentFrame => {
            Some(role.name().to_string())
        }
        _ => None,
    }
}

/// Does this role carry a text value worth reading (an entry, a text view,
/// a terminal)?
pub fn has_text_value(role: Role) -> bool {
    matches!(
        role,
        Role::Entry | Role::Text | Role::Terminal | Role::DocumentText | Role::Autocomplete
    )
}

/// Does this role carry a numeric value (slider, spin button, progress)?
pub fn has_numeric_value(role: Role) -> bool {
    matches!(
        role,
        Role::Slider | Role::SpinButton | Role::ProgressBar | Role::LevelBar | Role::Dial
    )
}

/// Is this a top-level window of some kind?
pub fn is_window(role: Role) -> bool {
    matches!(
        role,
        Role::Frame
            | Role::Window
            | Role::Dialog
            | Role::Alert
            | Role::FileChooser
            | Role::FontChooser
            | Role::ColorChooser
    )
}

/// Is this a dialog-like window: something asking for a decision?
pub fn is_dialog(role: Role, states: StateSet) -> bool {
    matches!(
        role,
        Role::Dialog | Role::Alert | Role::FileChooser | Role::FontChooser | Role::ColorChooser
    ) || (role == Role::Frame && states.contains(State::Modal))
}

/// The `kind` string `list_dialogs` reports.
pub fn dialog_kind(role: Role) -> &'static str {
    match role {
        Role::Alert | Role::Notification => "alert",
        Role::PopupMenu | Role::Menu => "menu",
        _ => "dialog",
    }
}

/// Is the node one a person could act on? Mirrors what gets a ref.
pub fn is_checkable(role: Role, states: StateSet) -> bool {
    states.contains(State::Checkable)
        || matches!(
            role,
            Role::CheckBox
                | Role::CheckMenuItem
                | Role::RadioButton
                | Role::RadioMenuItem
                | Role::ToggleButton
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_a11y::is_interactive_role;

    fn states(v: &[State]) -> StateSet {
        let mut s = enumflags2::BitFlags::<State>::empty();
        for st in v {
            s |= *st;
        }
        StateSet::new(s)
    }

    /// Every role an agent would want to click must land in the interactive
    /// vocabulary, or `ui_action` can never target it.
    #[test]
    fn actionable_roles_map_into_the_interactive_vocabulary() {
        for role in [
            Role::Button,
            Role::ToggleButton,
            Role::CheckBox,
            Role::RadioButton,
            Role::Entry,
            Role::PasswordText,
            Role::ComboBox,
            Role::MenuItem,
            Role::CheckMenuItem,
            Role::PageTab,
            Role::Slider,
            Role::SpinButton,
            Role::TreeItem,
            Role::TableCell,
            Role::Link,
            Role::ListItem,
            Role::Text,
        ] {
            let (name, _) = normalize(role, states(&[]));
            assert!(
                is_interactive_role(&name),
                "{role:?} mapped to '{name}', which gets no ref"
            );
        }
    }

    #[test]
    fn multiline_text_is_a_textarea_and_single_line_a_textfield() {
        assert_eq!(
            normalize(Role::Text, states(&[State::MultiLine])).0,
            "textarea"
        );
        assert_eq!(
            normalize(Role::Text, states(&[State::SingleLine])).0,
            "textfield"
        );
        assert_eq!(normalize(Role::Text, states(&[])).0, "textfield");
        assert_eq!(normalize(Role::Terminal, states(&[])).0, "textarea");
    }

    #[test]
    fn password_fields_are_secure_and_nothing_else_is() {
        assert_eq!(
            normalize(Role::PasswordText, states(&[])),
            ("textfield".into(), true)
        );
        assert_eq!(
            normalize(Role::Entry, states(&[])),
            ("textfield".into(), false)
        );
        assert_eq!(
            subrole(Role::PasswordText).as_deref(),
            Some("password text")
        );
    }

    /// A role this table does not know must still produce a usable token
    /// rather than a panic or a name with spaces the flattener cannot show.
    #[test]
    fn unknown_roles_are_squashed_not_dropped() {
        let (name, secure) = normalize(Role::DateEditor, states(&[]));
        assert_eq!(name, "dateeditor");
        assert!(!secure);
        assert_eq!(normalize(Role::MathFraction, states(&[])).0, "mathfraction");
    }

    #[test]
    fn dialogs_are_recognised_by_role_or_modal_state() {
        assert!(is_dialog(Role::Dialog, states(&[])));
        assert!(is_dialog(Role::FileChooser, states(&[])));
        assert!(is_dialog(Role::Frame, states(&[State::Modal])));
        assert!(!is_dialog(Role::Frame, states(&[])));
        assert!(!is_dialog(Role::Panel, states(&[State::Modal])));
        assert_eq!(dialog_kind(Role::Alert), "alert");
        assert_eq!(dialog_kind(Role::Dialog), "dialog");
    }

    #[test]
    fn windows_and_values() {
        assert!(is_window(Role::Frame));
        assert!(is_window(Role::Dialog));
        assert!(!is_window(Role::Panel));
        assert!(has_text_value(Role::Entry));
        assert!(!has_text_value(Role::Button));
        assert!(has_numeric_value(Role::Slider));
        assert!(is_checkable(Role::ToggleButton, states(&[])));
        assert!(is_checkable(Role::Button, states(&[State::Checkable])));
        assert!(!is_checkable(Role::Button, states(&[])));
    }
}
