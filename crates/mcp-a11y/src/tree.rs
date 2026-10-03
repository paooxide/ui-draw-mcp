use serde::{Deserialize, Serialize};

/// Screen bounds of an element (device points).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// An OS-independent accessibility node. A backend (macOS AX, later UIA/AT-SPI)
/// maps its native tree into these; the flattener and arena consume them.
///
/// `role` is expected normalized (lowercase, no `AX`/`UIA` prefix), but
/// [`normalize_role`] is applied defensively so raw platform roles also work.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiNode {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Platform subrole (e.g. `AXSecureTextField`), used for secure detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subrole: Option<String>,
    /// True for password/secure fields: the value is withheld from output.
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub selected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expanded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_state: Option<serde_json::Value>,
    /// Backend token to re-locate the native element for actions (input tools).
    /// Scoped to the current snapshot; never persisted to memory/disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<u64>,
    #[serde(default)]
    pub children: Vec<UiNode>,
}

impl UiNode {
    /// Is this node secure (subrole says so, or the flag is set)? Secure nodes
    /// never expose their value (arch §8, LLM06).
    pub fn is_secure(&self) -> bool {
        self.secure
            || self
                .subrole
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("AXSecureTextField"))
    }

    /// Does this node get an element ref (`@eN`)? Interactive roles do.
    pub fn is_interactive(&self) -> bool {
        is_interactive_role(&self.role)
    }
}

/// Normalize a platform role to a lowercase, prefix-free token
/// (`AXButton` -> `button`, `UIA_ButtonControlTypeId` is not handled here).
pub fn normalize_role(role: &str) -> String {
    let trimmed = role
        .strip_prefix("AX")
        .or_else(|| role.strip_prefix("ax"))
        .unwrap_or(role);
    trimmed.to_ascii_lowercase()
}

/// Interactive roles receive refs. Matched against the
/// normalized role so both `button` and `AXButton` work.
pub fn is_interactive_role(role: &str) -> bool {
    const INTERACTIVE: &[&str] = &[
        "button",
        "textfield",
        "textarea",
        "checkbox",
        "link",
        "menuitem",
        "menubaritem",
        "tab",
        "slider",
        "combobox",
        "popupbutton",
        "treeitem",
        "cell",
        "radiobutton",
        "incrementor",
        "menubutton",
        "switch",
        "colorwell",
        "dockitem",
        "disclosuretriangle",
        // AT-SPI toolkits: rows in a GTK list are pressed as rows, and a menu
        // must be pressed to open before its items exist.
        "listitem",
        "menu",
        // Hybrid Canvas and WebGL interactive targets
        "canvas",
        "canvas-child",
        "canvas-element",
    ];
    let norm = normalize_role(role);
    INTERACTIVE.contains(&norm.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_ax_prefix() {
        assert_eq!(normalize_role("AXButton"), "button");
        assert_eq!(normalize_role("AXTextArea"), "textarea");
        assert_eq!(normalize_role("button"), "button");
    }

    #[test]
    fn interactive_matches_raw_and_normalized() {
        assert!(is_interactive_role("AXButton"));
        assert!(is_interactive_role("button"));
        assert!(is_interactive_role("AXTextField"));
        assert!(!is_interactive_role("AXGroup"));
        assert!(!is_interactive_role("staticText"));
    }

    #[test]
    fn secure_detected_by_subrole_or_flag() {
        let mut n = UiNode {
            role: "textfield".into(),
            ..Default::default()
        };
        assert!(!n.is_secure());
        n.subrole = Some("AXSecureTextField".into());
        assert!(n.is_secure());
        let f = UiNode {
            role: "textfield".into(),
            secure: true,
            ..Default::default()
        };
        assert!(f.is_secure());
    }
}
