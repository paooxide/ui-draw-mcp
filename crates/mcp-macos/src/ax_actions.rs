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

/// `kAXErrorNotificationAlreadyRegistered`: the observer already has it, which
/// is what was asked for.
const AX_ALREADY_REGISTERED: i32 = -25209;

/// Whether an app can be observed, from what registering each notification
/// returned. Apps differ widely in which they accept (`kAXErrorCannotComplete`
/// from one that is busy or not accessible, `NotificationUnsupported` from one
/// that does not post it), so one refusal is not the app refusing: observation
/// stands if any notification registered. If none did the answer is the
/// reason, and the caller polls instead.
pub(crate) fn registration_outcome(results: &[(&str, i32)]) -> Result<usize, String> {
    let ok = results
        .iter()
        .filter(|(_, e)| *e == 0 || *e == AX_ALREADY_REGISTERED)
        .count();
    if ok > 0 {
        return Ok(ok);
    }
    let codes: Vec<String> = results.iter().map(|(n, e)| format!("{n}={e}")).collect();
    Err(format!(
        "the app accepted none of the notifications ({})",
        codes.join(", ")
    ))
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

    #[test]
    fn one_accepted_notification_is_enough_to_observe() {
        let r = [
            ("AXValueChanged", -25204),
            ("AXTitleChanged", 0),
            ("AXLayoutChanged", -25207),
            ("AXWindowCreated", -25209),
        ];
        assert_eq!(registration_outcome(&r), Ok(2));
    }

    #[test]
    fn an_app_that_refuses_every_notification_is_polled_with_the_reason() {
        let r = [("AXValueChanged", -25204), ("AXTitleChanged", -25207)];
        let e = registration_outcome(&r).unwrap_err();
        assert!(e.contains("AXValueChanged=-25204") && e.contains("AXTitleChanged=-25207"));
        assert!(registration_outcome(&[]).is_err());
    }
}
