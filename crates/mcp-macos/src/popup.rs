//! Reading a control back, and choosing from popup buttons and combo boxes.
//!
//! An `AXPopUpButton` answers `AXPress` by opening its menu and returning, so
//! "press it" reports success whether or not anything was chosen. Choosing by
//! text means opening the menu, finding the `AXMenuItem` whose title matches,
//! pressing that, and then asking the control what it shows. The last step is
//! what makes the answer true.
//!
//! Everything here is synchronous and bounded. AX handles are not `Send`, so
//! none can be held across an await; the longest wait is about two seconds.

use std::time::{Duration, Instant};

use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use mcp_input::{
    list_options, missing_message, pick_option, same_option, Choice, InputError, OptionItem, Pick,
    Reading,
};

use crate::imp::{
    ax_result, copy_attr, copy_attr_bool, copy_attr_string, copy_children,
    AXUIElementPerformAction, AXUIElementSetAttributeValue, AXUIElementSetMessagingTimeout,
    KAX_ERROR_CANNOT_COMPLETE, KAX_SUCCESS,
};

/// Controls that show one chosen entry and hold the rest in a menu or list.
const POPUP_ROLES: [&str; 3] = ["AXPopUpButton", "AXMenuButton", "AXComboBox"];
/// How long to wait for a popup's items to appear, and for the control to show
/// the new choice after one is pressed.
const OPEN_WAIT: Duration = Duration::from_millis(1500);
const SETTLE_WAIT: Duration = Duration::from_millis(600);
const POLL: Duration = Duration::from_millis(40);

fn role_of(elem: CFTypeRef) -> String {
    unsafe { copy_attr_string(elem, "AXRole").unwrap_or_default() }
}

/// `AXValue` as display text: strings as they are, numbers and booleans
/// spelled out. `None` when the element has no value to speak of.
unsafe fn value_text(elem: CFTypeRef) -> Option<String> {
    let v = copy_attr(elem, "AXValue")?;
    if let Some(s) = v.downcast::<CFString>() {
        return Some(s.to_string());
    }
    if let Some(n) = v.downcast::<CFNumber>() {
        return n
            .to_i64()
            .map(|i| i.to_string())
            .or_else(|| n.to_f64().map(|f| f.to_string()));
    }
    v.downcast::<CFBoolean>().map(|b| bool::from(b).to_string())
}

/// Whether a checkable control is on. `None` for anything without that notion,
/// and for a mixed state, which is neither.
unsafe fn checked_state(elem: CFTypeRef, role: &str) -> Option<bool> {
    match role {
        "AXCheckBox" | "AXRadioButton" | "AXSwitch" | "AXDisclosureTriangle" | "AXToggle" => {
            let v = copy_attr(elem, "AXValue")?;
            if let Some(n) = v.downcast::<CFNumber>() {
                return match n.to_i64()? {
                    0 => Some(false),
                    1 => Some(true),
                    _ => None,
                };
            }
            v.downcast::<CFBoolean>().map(bool::from)
        }
        // A tick beside a menu item is its mark character.
        "AXMenuItem" => copy_attr_string(elem, "AXMenuItemMarkChar")
            .map(|m| !m.is_empty())
            .or(Some(false)),
        _ => None,
    }
}

/// What the control shows right now.
pub(crate) unsafe fn read(elem: CFTypeRef) -> Reading {
    let role = role_of(elem);
    let checked = checked_state(elem, &role);
    let value = if checked.is_some() || role == "AXMenuItem" {
        // A checkbox's `AXValue` is 0 or 1: that is its state, not text.
        None
    } else if POPUP_ROLES.contains(&role.as_str()) {
        // A pull-down keeps its title and leaves `AXValue` empty.
        shown(elem)
    } else {
        value_text(elem)
    };
    Reading { value, checked }
}

/// The text a popup shows as its current choice.
unsafe fn shown(popup: CFTypeRef) -> Option<String> {
    value_text(popup)
        .filter(|s| !s.is_empty())
        .or_else(|| copy_attr_string(popup, "AXTitle").filter(|s| !s.is_empty()))
}

/// The popup control an element is, or sits inside (menu item -> menu -> popup
/// button). Climbs only through menus and items: a button's parent is not its
/// popup, and a menu-bar item has none.
unsafe fn owner(elem: &CFType) -> Option<CFType> {
    let mut cur = CFType::wrap_under_get_rule(elem.as_CFTypeRef());
    for _ in 0..4 {
        let role = role_of(cur.as_CFTypeRef());
        if POPUP_ROLES.contains(&role.as_str()) {
            return Some(cur);
        }
        if !matches!(role.as_str(), "AXMenuItem" | "AXMenu") {
            return None;
        }
        cur = copy_attr(cur.as_CFTypeRef(), "AXParent")?;
    }
    None
}

/// An entry of an open popup.
struct Entry {
    item: CFType,
    option: OptionItem,
}

/// The entries of a popup's menu, in order. Separators and untitled items are
/// not choices and are left out.
unsafe fn entries(popup: CFTypeRef) -> Vec<Entry> {
    let mut out = Vec::new();
    for child in copy_children(popup) {
        if role_of(child.as_CFTypeRef()) != "AXMenu" {
            continue;
        }
        for item in copy_children(child.as_CFTypeRef()) {
            if role_of(item.as_CFTypeRef()) != "AXMenuItem" {
                continue;
            }
            let Some(title) =
                copy_attr_string(item.as_CFTypeRef(), "AXTitle").filter(|t| !t.is_empty())
            else {
                continue;
            };
            let enabled = copy_attr_bool(item.as_CFTypeRef(), "AXEnabled") != Some(false);
            out.push(Entry {
                item,
                option: OptionItem { title, enabled },
            });
        }
    }
    out
}

unsafe fn wait_for_entries(popup: CFTypeRef, within: Duration) -> Vec<Entry> {
    let until = Instant::now() + within;
    loop {
        let e = entries(popup);
        if !e.is_empty() || Instant::now() >= until {
            return e;
        }
        std::thread::sleep(POLL);
    }
}

unsafe fn perform(elem: CFTypeRef, action: &str) -> i32 {
    let act = CFString::new(action);
    AXUIElementPerformAction(elem, act.as_concrete_TypeRef())
}

/// Dismiss a menu this call opened. `AXCancel` on the menu closes it without
/// a keystroke, which matters: a synthetic Escape goes to whatever is
/// frontmost, and that may not be the app being driven.
unsafe fn close(popup: CFTypeRef) {
    for child in copy_children(popup) {
        if role_of(child.as_CFTypeRef()) == "AXMenu" {
            let _ = perform(child.as_CFTypeRef(), "AXCancel");
        }
    }
}

/// Choose the entry named `option` from the popup `elem` is, or belongs to.
pub(crate) unsafe fn choose(elem: &CFType, option: &str) -> Result<Choice, InputError> {
    let Some(popup) = owner(elem) else {
        return Err(InputError::Unsupported(format!(
            "'{}' is not a popup button or combo box, nor an item in one",
            role_of(elem.as_CFTypeRef()).trim_start_matches("AX")
        )));
    };
    let p = popup.as_CFTypeRef();
    // A press on a popup can run the app's menu tracking; bound the request so
    // a stuck app cannot hold the thread for the system default.
    let _ = AXUIElementSetMessagingTimeout(p, 2.0);
    if copy_attr_bool(p, "AXEnabled") == Some(false) {
        return Err(InputError::Failed("the control is disabled".into()));
    }
    if role_of(p) == "AXComboBox" {
        return choose_in_combo(p, option);
    }
    let previous = shown(p);

    // The menu may already be in the tree while closed, and pressing an item
    // of a closed menu does nothing. Open it, give it a moment, then look.
    let opened = perform(p, "AXPress");
    if opened != KAX_SUCCESS && opened != KAX_ERROR_CANNOT_COMPLETE {
        close(p);
        return Err(ax_result(opened, "open the popup")
            .err()
            .unwrap_or_else(|| InputError::Failed("could not open the popup".into())));
    }
    std::thread::sleep(Duration::from_millis(120));
    let found = wait_for_entries(p, OPEN_WAIT);
    if found.is_empty() {
        close(p);
        return Err(InputError::Failed(
            "the popup opened no items to choose from".into(),
        ));
    }
    let options: Vec<OptionItem> = found.iter().map(|e| e.option.clone()).collect();
    let index = match pick_option(&options, option) {
        Pick::Found(i) => i,
        Pick::Disabled(i) => {
            close(p);
            return Err(InputError::Failed(format!(
                "option {:?} is disabled",
                options[i].title
            )));
        }
        Pick::Missing => {
            close(p);
            return Err(InputError::InvalidArgs(missing_message(&options, option)));
        }
    };
    let want = options[index].title.clone();

    // A control that shows its choice changes its text; an action menu (a
    // pull-down titled "Actions") does not, and must not be failed for it.
    let shows_choice = previous
        .as_deref()
        .is_some_and(|p| options.iter().any(|o| same_option(&o.title, p)));

    let mut pressed = perform(found[index].item.as_CFTypeRef(), "AXPress");
    if pressed != KAX_SUCCESS {
        // The menu can take a moment longer to accept a press than to list.
        std::thread::sleep(Duration::from_millis(150));
        pressed = perform(found[index].item.as_CFTypeRef(), "AXPress");
    }
    if pressed != KAX_SUCCESS {
        close(p);
        return Err(ax_result(pressed, "choose the item")
            .err()
            .unwrap_or_else(|| InputError::Failed("could not press the item".into())));
    }

    let until = Instant::now() + SETTLE_WAIT;
    let mut now = shown(p);
    while Instant::now() < until && !now.as_deref().is_some_and(|s| same_option(s, &want)) {
        std::thread::sleep(POLL);
        now = shown(p);
    }
    // Success closes the menu itself; this catches the case where it did not.
    close(p);

    match now {
        Some(s) if same_option(&s, &want) => Ok(Choice {
            changed: differs(previous.as_deref(), &s),
            item: want,
            selected: Some(s),
        }),
        Some(s) if shows_choice => Err(InputError::Failed(format!(
            "pressed {want:?}, but the control still shows {s:?}: the app kept {s:?}"
        ))),
        // An action menu: there is no selection to read, so none is claimed.
        _ => Ok(Choice {
            item: want,
            selected: None,
            changed: true,
        }),
    }
}

/// Did the control show something else before? Unknown counts as yes.
fn differs(previous: Option<&str>, now: &str) -> bool {
    match previous {
        Some(p) => !same_option(p, now),
        None => true,
    }
}

/// A combo box takes its choice as text. When its list is in the tree the
/// option must be one of the entries; when it is not (most combo boxes only
/// build the list while open) the text is written as typed.
unsafe fn choose_in_combo(combo: CFTypeRef, option: &str) -> Result<Choice, InputError> {
    let previous = shown(combo);
    let known = entries(combo);
    let want = if known.is_empty() {
        option.to_string()
    } else {
        let options: Vec<OptionItem> = known.iter().map(|e| e.option.clone()).collect();
        match pick_option(&options, option) {
            Pick::Found(i) => options[i].title.clone(),
            Pick::Disabled(i) => {
                return Err(InputError::Failed(format!(
                    "option {:?} is disabled",
                    options[i].title
                )))
            }
            Pick::Missing => {
                return Err(InputError::InvalidArgs(missing_message(&options, option)))
            }
        }
    };
    let attr = CFString::new("AXValue");
    let val = CFString::new(&want);
    let err = AXUIElementSetAttributeValue(combo, attr.as_concrete_TypeRef(), val.as_CFTypeRef());
    if err != KAX_SUCCESS {
        return Err(ax_result(err, "set the combo box text")
            .err()
            .unwrap_or_else(|| {
                InputError::Failed("the combo box did not accept the text".into())
            }));
    }
    // Commit it the way Return would; a combo box that has no such action
    // simply ignores the request.
    let _ = perform(combo, "AXConfirm");

    let until = Instant::now() + SETTLE_WAIT;
    let mut now = shown(combo);
    while Instant::now() < until && !now.as_deref().is_some_and(|s| same_option(s, &want)) {
        std::thread::sleep(POLL);
        now = shown(combo);
    }
    match now {
        Some(s) if same_option(&s, &want) => Ok(Choice {
            changed: differs(previous.as_deref(), &s),
            item: want,
            selected: Some(s),
        }),
        Some(s) => Err(InputError::Failed(format!(
            "wrote {want:?}, but the combo box shows {s:?}: the app kept {s:?}{}",
            if known.is_empty() {
                String::new()
            } else {
                format!("; its options are {}", list_options(&options_of(&known)))
            }
        ))),
        None => Ok(Choice {
            item: want,
            selected: None,
            changed: true,
        }),
    }
}

fn options_of(entries: &[Entry]) -> Vec<OptionItem> {
    entries.iter().map(|e| e.option.clone()).collect()
}
