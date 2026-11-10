//! Which accessibility action a semantic action is, and how to say an element
//! does not offer it. Pure, so it is tested without an app.

use mcp_input::SemanticAction;

/// The AX action behind a semantic action. `None` for `Focus`, which sets an
/// attribute instead of performing an action.
///
/// Click, double click, right click, toggle, check, uncheck, expand, collapse
/// and select are all `AXPress`: AX has no distinct double or secondary press.
pub(crate) fn ax_action_name(action: SemanticAction) -> Option<&'static str> {
    Some(match action {
        SemanticAction::Focus => return None,
        SemanticAction::ScrollIntoView => "AXScrollToVisible",
        SemanticAction::ShowMenu => "AXShowMenu",
        SemanticAction::Increment => "AXIncrement",
        SemanticAction::Decrement => "AXDecrement",
        SemanticAction::Pick => "AXPick",
        SemanticAction::Click
        | SemanticAction::DoubleClick
        | SemanticAction::RightClick
        | SemanticAction::Toggle
        | SemanticAction::Check
        | SemanticAction::Uncheck
        | SemanticAction::Expand
        | SemanticAction::Collapse
        | SemanticAction::Select => "AXPress",
    })
}

/// Whether the element's own action list is consulted before acting.
///
/// The actions that are new to `ui_action` are checked up front so a missing
/// one is reported with what the element does offer. `AXPress` and
/// `AXScrollToVisible` have always been attempted directly, and an element
/// that under-reports its list must not start refusing them.
pub(crate) fn must_be_listed(action: SemanticAction) -> bool {
    matches!(
        action,
        SemanticAction::ShowMenu
            | SemanticAction::Increment
            | SemanticAction::Decrement
            | SemanticAction::Pick
    )
}

/// The `ui_action` name that performs an AX action, for the ones that have one.
fn ui_name_of(ax: &str) -> Option<&'static str> {
    Some(match ax {
        "AXPress" => "click",
        "AXShowMenu" => "show_menu",
        "AXIncrement" => "increment",
        "AXDecrement" => "decrement",
        "AXPick" => "pick",
        "AXScrollToVisible" => "scroll_into_view",
        _ => return None,
    })
}

/// The error for an action an element does not offer, naming the ones it does
/// in both the AX spelling and the `ui_action` one.
pub(crate) fn unsupported_message(label: &str, wanted: &str, offered: &[String]) -> String {
    let list = if offered.is_empty() {
        "no actions".to_string()
    } else {
        let items: Vec<String> = offered
            .iter()
            .map(|a| match ui_name_of(a) {
                Some(n) => format!("{a} ({n})"),
                None => a.clone(),
            })
            .collect();
        items.join(", ")
    };
    format!("'{label}' does not offer {wanted}; it offers {list}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_new_action_is_its_own_ax_action() {
        assert_eq!(ax_action_name(SemanticAction::ShowMenu), Some("AXShowMenu"));
        assert_eq!(
            ax_action_name(SemanticAction::Increment),
            Some("AXIncrement")
        );
        assert_eq!(
            ax_action_name(SemanticAction::Decrement),
            Some("AXDecrement")
        );
        assert_eq!(ax_action_name(SemanticAction::Pick), Some("AXPick"));
        assert_eq!(
            ax_action_name(SemanticAction::ScrollIntoView),
            Some("AXScrollToVisible")
        );
    }

    #[test]
    fn the_press_family_is_ax_press_and_focus_is_not_an_action() {
        for a in [
            SemanticAction::Click,
            SemanticAction::DoubleClick,
            SemanticAction::RightClick,
            SemanticAction::Toggle,
            SemanticAction::Check,
            SemanticAction::Uncheck,
            SemanticAction::Expand,
            SemanticAction::Collapse,
            SemanticAction::Select,
        ] {
            assert_eq!(ax_action_name(a), Some("AXPress"), "{a:?}");
        }
        assert_eq!(ax_action_name(SemanticAction::Focus), None);
    }

    #[test]
    fn only_the_new_actions_are_checked_against_the_list() {
        assert!(must_be_listed(SemanticAction::ShowMenu));
        assert!(must_be_listed(SemanticAction::Increment));
        assert!(must_be_listed(SemanticAction::Decrement));
        assert!(must_be_listed(SemanticAction::Pick));
        assert!(!must_be_listed(SemanticAction::Click));
        assert!(!must_be_listed(SemanticAction::ScrollIntoView));
    }

    #[test]
    fn the_error_names_what_the_element_does_offer() {
        let offered = vec!["AXPress".to_string(), "AXCancel".to_string()];
        let m = unsupported_message("Volume", "AXIncrement", &offered);
        assert_eq!(
            m,
            "'Volume' does not offer AXIncrement; it offers AXPress (click), AXCancel"
        );
    }

    #[test]
    fn an_element_with_no_actions_says_so() {
        let m = unsupported_message("Label", "AXShowMenu", &[]);
        assert!(m.ends_with("it offers no actions"), "{m}");
    }
}
