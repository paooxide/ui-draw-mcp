use std::collections::HashMap;

use crate::arena::{ElementInfo, Snapshot};
use crate::tree::{normalize_role, UiNode};

/// Knobs for flattening a UI tree into agent-readable text.
#[derive(Debug, Clone)]
pub struct FlattenConfig {
    /// Hard character budget for the text. Exceeding it triggers the
    /// skeleton→hard-truncate cascade.
    pub max_chars: usize,
    /// Start in skeleton mode (depth-limited overview).
    pub skeleton: bool,
    /// Max chars kept from an element value.
    pub value_truncate: usize,
    /// Keep the value's *tail* (terminals: new output is at the bottom) instead
    /// of the head.
    pub terminal_app: bool,
    /// Depth at which skeleton mode collapses containers.
    pub skeleton_depth: usize,
}

impl Default for FlattenConfig {
    fn default() -> Self {
        FlattenConfig {
            max_chars: 12_000,
            skeleton: false,
            value_truncate: 200,
            terminal_app: false,
            skeleton_depth: 3,
        }
    }
}

/// The result of flattening: the text an agent reads, plus the ref index for the
/// arena, and whether truncation happened.
#[derive(Debug, Clone)]
pub struct Flattened {
    pub text: String,
    pub snapshot: Snapshot,
    pub ref_count: usize,
    pub truncated: bool,
    pub used_skeleton: bool,
}

/// Flatten a tree, applying the budget cascade: try the requested mode; if it
/// overflows and wasn't skeleton, retry skeleton; if that still overflows,
/// hard-truncate with an explicit marker (never silent).
pub fn flatten(
    root: &UiNode,
    app: Option<&str>,
    window: Option<&str>,
    snapshot_id: &str,
    cfg: &FlattenConfig,
) -> Flattened {
    let first = build(root, app, window, snapshot_id, cfg.skeleton, cfg);
    if first.text.chars().count() <= cfg.max_chars {
        return first;
    }
    if !cfg.skeleton {
        let mut sk = build(root, app, window, snapshot_id, true, cfg);
        if sk.text.chars().count() <= cfg.max_chars {
            sk.text.push_str("\n[truncated: use root=@eN to drill]");
            sk.truncated = true;
            return sk;
        }
        return hard_truncate(sk, cfg.max_chars);
    }
    hard_truncate(first, cfg.max_chars)
}

fn build(
    root: &UiNode,
    app: Option<&str>,
    window: Option<&str>,
    snapshot_id: &str,
    skeleton: bool,
    cfg: &FlattenConfig,
) -> Flattened {
    let mut b = Builder {
        lines: Vec::new(),
        elements: HashMap::new(),
        next_ref: 1,
        skeleton,
        cfg,
    };

    let mut header = format!("snapshot {snapshot_id}");
    if let Some(a) = app {
        header.push_str(&format!(" app=\"{a}\""));
    }
    if let Some(w) = window {
        header.push_str(&format!(" window=\"{w}\""));
    }
    b.lines.push(header);

    for child in &root.children {
        b.walk(child, 0);
    }

    let ref_count = b.next_ref - 1;
    Flattened {
        text: b.lines.join("\n"),
        snapshot: Snapshot {
            id: snapshot_id.to_string(),
            app: app.map(str::to_string),
            window: window.map(str::to_string),
            elements: b.elements,
            skeleton,
        },
        ref_count,
        truncated: false,
        used_skeleton: skeleton,
    }
}

struct Builder<'a> {
    lines: Vec<String>,
    elements: HashMap<String, ElementInfo>,
    next_ref: usize,
    skeleton: bool,
    cfg: &'a FlattenConfig,
}

impl Builder<'_> {
    fn take_ref(&mut self) -> String {
        let r = format!("@e{}", self.next_ref);
        self.next_ref += 1;
        r
    }

    fn walk(&mut self, node: &UiNode, depth: usize) {
        if node.is_interactive() {
            let r = self.take_ref();
            self.lines.push(self.format_interactive(&r, node));
            self.elements.insert(r, element_info(node, self.cfg));
            // Interactive containers (tab groups, etc.) may hold more refs.
            if !(self.skeleton && depth + 1 >= self.cfg.skeleton_depth) {
                for c in &node.children {
                    self.walk(c, depth + 1);
                }
            }
            return;
        }

        // Skeleton: collapse deep non-interactive containers into a drill target.
        if self.skeleton && depth + 1 >= self.cfg.skeleton_depth && !node.children.is_empty() {
            let r = self.take_ref();
            let name = node.name.as_deref().unwrap_or("");
            self.lines.push(format!(
                "[{r} {} \"{name}\" +{} children]",
                normalize_role(&node.role),
                node.children.len()
            ));
            self.elements.insert(r, element_info(node, self.cfg));
            return;
        }

        // Named non-interactive container: one context line, no ref.
        if let Some(name) = node.name.as_deref().filter(|n| !n.is_empty()) {
            self.lines
                .push(format!("[{} \"{name}\"]", normalize_role(&node.role)));
        }
        for c in &node.children {
            self.walk(c, depth + 1);
        }
    }

    fn format_interactive(&self, r: &str, node: &UiNode) -> String {
        let mut s = format!("{r} {}", normalize_role(&node.role));
        if let Some(name) = node.name.as_deref().filter(|n| !n.is_empty()) {
            s.push_str(&format!(" \"{name}\""));
        }
        if node.is_secure() {
            s.push_str(" secure");
        } else if let Some(v) = node.value.as_deref().filter(|v| !v.is_empty()) {
            let t = truncate_value(v, self.cfg.value_truncate, self.cfg.terminal_app);
            s.push_str(&format!(" value=\"{t}\""));
        }
        if let Some(intent) = node.semantic_intent.as_deref().and_then(intent_token) {
            s.push_str(&format!(" intent={intent}"));
        }
        for flag in flags(node) {
            s.push(' ');
            s.push_str(flag);
        }
        s
    }
}

/// Longest semantic intent shown in a snapshot.
const INTENT_MAX: usize = 48;

/// A semantic intent as a bare identifier (`add_to_cart`), or nothing. It is
/// derived from page attributes, so anything outside `[A-Za-z0-9_.-]` is
/// dropped rather than escaped: an intent is a label, not text to quote.
fn intent_token(raw: &str) -> Option<String> {
    let t: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        .take(INTENT_MAX)
        .collect();
    (!t.is_empty()).then_some(t)
}

fn flags(node: &UiNode) -> Vec<&'static str> {
    let mut f = Vec::new();
    if node.focused {
        f.push("focused");
    }
    if node.disabled {
        f.push("disabled");
    }
    if node.selected {
        f.push("selected");
    }
    match node.checked {
        Some(true) => f.push("checked"),
        Some(false) => f.push("unchecked"),
        None => {}
    }
    match node.expanded {
        Some(true) => f.push("expanded"),
        Some(false) => f.push("collapsed"),
        None => {}
    }
    f
}

fn element_info(node: &UiNode, cfg: &FlattenConfig) -> ElementInfo {
    let secure = node.is_secure();
    ElementInfo {
        role: normalize_role(&node.role),
        name: node.name.clone(),
        value_preview: if secure {
            None
        } else {
            node.value
                .as_deref()
                .map(|v| truncate_value(v, cfg.value_truncate, cfg.terminal_app))
        },
        secure,
        bounds: node.bounds,
        node_id: node.node_id,
        state: crate::arena::ElementState {
            focused: node.focused,
            disabled: node.disabled,
            selected: node.selected,
            checked: node.checked,
            expanded: node.expanded,
        },
        semantic_intent: node.semantic_intent.clone(),
        bound_state: node.bound_state.clone(),
    }
}

/// Escape newlines and truncate a value to `max` chars, keeping the tail for
/// terminal apps (new output is at the bottom) and the head otherwise.
fn truncate_value(v: &str, max: usize, tail: bool) -> String {
    let escaped: String = v
        .chars()
        .map(|c| match c {
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            other => other.to_string(),
        })
        .collect();
    let count = escaped.chars().count();
    if count <= max {
        return escaped;
    }
    if tail {
        let s: String = escaped.chars().skip(count - max).collect();
        format!("\u{2026}{s}")
    } else {
        let s: String = escaped.chars().take(max).collect();
        format!("{s}\u{2026}")
    }
}

fn hard_truncate(mut f: Flattened, max: usize) -> Flattened {
    // Cut on a line boundary within the budget, then mark it explicitly.
    let mut kept = String::new();
    for line in f.text.lines() {
        if kept.chars().count() + line.chars().count() + 1 > max {
            break;
        }
        if !kept.is_empty() {
            kept.push('\n');
        }
        kept.push_str(line);
    }
    kept.push_str(&format!(
        "\n[HARD TRUNCATED — {} refs total, output exceeded budget]",
        f.ref_count
    ));
    f.text = kept;
    f.truncated = true;
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::SnapshotArena;

    fn leaf(role: &str, name: &str) -> UiNode {
        UiNode {
            role: role.into(),
            name: Some(name.into()),
            ..Default::default()
        }
    }

    fn root_with(children: Vec<UiNode>) -> UiNode {
        UiNode {
            role: "application".into(),
            children,
            ..Default::default()
        }
    }

    #[test]
    fn flattens_basic_tree_with_refs() {
        let root = root_with(vec![
            leaf("AXButton", "Close"),
            UiNode {
                role: "textarea".into(),
                value: Some("hello".into()),
                focused: true,
                ..Default::default()
            },
        ]);
        let f = flatten(
            &root,
            Some("TextEdit"),
            Some("Untitled"),
            "s1",
            &FlattenConfig::default(),
        );
        assert!(f
            .text
            .starts_with("snapshot s1 app=\"TextEdit\" window=\"Untitled\""));
        assert!(f.text.contains("@e1 button \"Close\""));
        assert!(f.text.contains("@e2 textarea value=\"hello\" focused"));
        assert_eq!(f.ref_count, 2);
        assert!(!f.truncated);
    }

    #[test]
    fn secure_field_value_withheld_everywhere() {
        let root = root_with(vec![UiNode {
            role: "textfield".into(),
            name: Some("Password".into()),
            value: Some("hunter2".into()),
            subrole: Some("AXSecureTextField".into()),
            ..Default::default()
        }]);
        let f = flatten(&root, None, None, "s1", &FlattenConfig::default());
        assert!(f.text.contains("@e1 textfield \"Password\" secure"));
        assert!(!f.text.contains("hunter2"));
        let info = &f.snapshot.elements["@e1"];
        assert!(info.secure);
        assert_eq!(info.value_preview, None);
    }

    #[test]
    fn terminal_app_keeps_value_tail() {
        let value: String = (0..300)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let root = root_with(vec![UiNode {
            role: "textarea".into(),
            value: Some(value.clone()),
            ..Default::default()
        }]);
        let cfg = FlattenConfig {
            terminal_app: true,
            value_truncate: 50,
            ..Default::default()
        };
        let f = flatten(&root, None, None, "s1", &cfg);
        // tail-kept: ends with the last chars of the value, prefixed by an ellipsis
        let last_10: String = value
            .chars()
            .rev()
            .take(10)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        assert!(f.text.contains(&last_10));
        assert!(f.text.contains('\u{2026}'));
    }

    #[test]
    fn refs_resolve_through_arena() {
        let root = root_with(vec![leaf("button", "OK")]);
        let f = flatten(&root, None, None, "s7", &FlattenConfig::default());
        let mut arena = SnapshotArena::new();
        arena.install(f.snapshot);
        assert_eq!(arena.resolve("s7", "@e1").unwrap().role, "button");
        assert!(arena.resolve("sOLD", "@e1").is_err());
    }

    #[test]
    fn budget_overflow_falls_back_to_skeleton_then_hard_truncate() {
        // Wide, deep tree with many interactive nodes.
        let mut kids = Vec::new();
        for i in 0..200 {
            kids.push(UiNode {
                role: "group".into(),
                name: Some(format!("group{i}")),
                children: vec![leaf("button", &format!("btn{i}"))],
                ..Default::default()
            });
        }
        let root = root_with(kids);
        let cfg = FlattenConfig {
            max_chars: 300,
            ..Default::default()
        };
        let f = flatten(&root, None, None, "s1", &cfg);
        assert!(f.truncated);
        assert!(f.text.contains("HARD TRUNCATED"));
        assert!(f.text.chars().count() <= 300 + 60); // budget + marker line
    }

    #[test]
    fn empty_tree_yields_header_only() {
        let root = root_with(vec![]);
        let f = flatten(&root, Some("Finder"), None, "s1", &FlattenConfig::default());
        assert_eq!(f.text, "snapshot s1 app=\"Finder\"");
        assert_eq!(f.ref_count, 0);
        assert!(f.snapshot.elements.is_empty());
    }

    #[test]
    fn skeleton_collapses_deep_containers_with_drill_ref() {
        // depth: root(0) > a(0) > b(1) > c(2) > button(3)
        let deep = UiNode {
            role: "group".into(),
            name: Some("a".into()),
            children: vec![UiNode {
                role: "group".into(),
                name: Some("b".into()),
                children: vec![UiNode {
                    role: "group".into(),
                    name: Some("c".into()),
                    children: vec![leaf("button", "deep")],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let cfg = FlattenConfig {
            skeleton: true,
            skeleton_depth: 3,
            ..Default::default()
        };
        let f = flatten(&root_with(vec![deep]), None, None, "s1", &cfg);
        assert!(f.text.contains("+1 children]"));
        assert!(!f.text.contains("\"deep\""));
    }

    #[test]
    fn flattens_semantic_intent_and_bound_state() {
        let node = UiNode {
            role: "button".into(),
            name: Some("Checkout".into()),
            semantic_intent: Some("checkout_order".into()),
            bound_state: Some(serde_json::json!({ "total": 49.99 })),
            ..Default::default()
        };
        let f = flatten(
            &root_with(vec![node]),
            None,
            None,
            "s1",
            &FlattenConfig::default(),
        );
        assert!(f
            .text
            .contains("@e1 button \"Checkout\" intent=checkout_order"));
        let el = f
            .snapshot
            .elements
            .get("@e1")
            .expect("must have element @e1");
        assert_eq!(el.semantic_intent.as_deref(), Some("checkout_order"));
        assert_eq!(
            el.bound_state
                .as_ref()
                .and_then(|v| v.get("total"))
                .and_then(|t| t.as_f64()),
            Some(49.99)
        );
    }

    #[test]
    fn intent_is_a_capped_identifier() {
        assert_eq!(intent_token("add_to_cart"), Some("add_to_cart".into()));
        assert_eq!(intent_token("<script>"), Some("script".into()));
        assert_eq!(intent_token("\"\n"), None);
        assert_eq!(intent_token(&"a".repeat(500)).unwrap().len(), INTENT_MAX);
    }
}
