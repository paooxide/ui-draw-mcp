use serde_json::{json, Value};
use std::collections::BTreeMap;

use crate::tree::UiNode;

/// Extract structured data (table, form, list, or custom schema) from an accessibility subtree.
pub fn extract_data(root: &UiNode, mode: Option<&str>, schema: Option<&Value>) -> Value {
    let mode_str = mode.unwrap_or_else(|| detect_mode(root));
    match mode_str {
        "table" => extract_table(root),
        "form" => extract_form(root),
        "list" => extract_list(root),
        "schema" => {
            if let Some(s) = schema {
                extract_schema(root, s)
            } else {
                extract_form(root)
            }
        }
        _ => extract_form(root),
    }
}

/// Detect the most likely mode for an unadorned extraction.
fn detect_mode(root: &UiNode) -> &'static str {
    if find_node(root, &|n| {
        matches!(n.role.as_str(), "table" | "outline" | "grid")
    })
    .is_some()
    {
        "table"
    } else if find_node(root, &|n| matches!(n.role.as_str(), "list" | "menu")).is_some() {
        "list"
    } else {
        "form"
    }
}

/// Locate a node in the tree matching predicate.
fn find_node<'a, F>(root: &'a UiNode, pred: &F) -> Option<&'a UiNode>
where
    F: Fn(&UiNode) -> bool,
{
    if pred(root) {
        return Some(root);
    }
    for child in &root.children {
        if let Some(found) = find_node(child, pred) {
            return Some(found);
        }
    }
    None
}

/// Extract tabular data from an `AXTable`, `AXOutline`, or general grid.
fn extract_table(root: &UiNode) -> Value {
    let table_node = find_node(root, &|n| {
        matches!(n.role.as_str(), "table" | "outline" | "grid")
    })
    .unwrap_or(root);

    // Collect column headers
    let mut columns = Vec::new();
    collect_headers(table_node, &mut columns);

    // Collect rows
    let mut rows = Vec::new();
    collect_rows(table_node, &mut rows);

    // If no explicit columns were found, infer from column count of first row
    if columns.is_empty() && !rows.is_empty() {
        let max_cols = rows.iter().map(|r: &Vec<Value>| r.len()).max().unwrap_or(0);
        for i in 0..max_cols {
            columns.push(format!("col_{}", i + 1));
        }
    }

    let count = rows.len();
    json!({
        "type": "table",
        "name": table_node.name,
        "columns": columns,
        "rows": rows,
        "row_count": count
    })
}

fn collect_headers(node: &UiNode, out: &mut Vec<String>) {
    if matches!(node.role.as_str(), "column" | "header") {
        if let Some(name) = &node.name {
            out.push(name.clone());
            return;
        }
    }
    for child in &node.children {
        if child.role != "row" {
            collect_headers(child, out);
        }
    }
}

fn collect_rows(node: &UiNode, out: &mut Vec<Vec<Value>>) {
    if node.role == "row" {
        let mut row_cells = Vec::new();
        collect_cells(node, &mut row_cells);
        out.push(row_cells);
        return;
    }
    for child in &node.children {
        collect_rows(child, out);
    }
}

fn collect_cells(row: &UiNode, out: &mut Vec<Value>) {
    // If row has direct cell children
    let cells: Vec<&UiNode> = row
        .children
        .iter()
        .filter(|c| matches!(c.role.as_str(), "cell" | "static_text" | "text_field"))
        .collect();

    if !cells.is_empty() {
        for cell in cells {
            let val = cell.value.as_deref().or(cell.name.as_deref()).unwrap_or("");
            out.push(json!(val));
        }
    } else {
        // Fallback: traverse row children to find leaf text values
        for child in &row.children {
            let val = child
                .value
                .as_deref()
                .or(child.name.as_deref())
                .unwrap_or("");
            out.push(json!(val));
        }
    }
}

/// Extract form inputs, checkboxes, radios, and popups.
fn extract_form(root: &UiNode) -> Value {
    let mut fields = BTreeMap::new();
    collect_form_fields(root, &mut fields);
    let count = fields.len();
    json!({
        "type": "form",
        "fields": fields,
        "count": count
    })
}

fn collect_form_fields(node: &UiNode, out: &mut BTreeMap<String, Value>) {
    let role = node.role.as_str();
    match role {
        "text_field" | "secure_text_field" => {
            let key = node
                .name
                .clone()
                .unwrap_or_else(|| format!("field_{}", out.len() + 1));
            let val = if node.is_secure() {
                json!("[REDACTED]")
            } else {
                json!(node.value.as_deref().unwrap_or(""))
            };
            out.insert(key, val);
        }
        "checkbox" | "radio_button" | "switch" | "toggle" => {
            let key = node
                .name
                .clone()
                .unwrap_or_else(|| format!("toggle_{}", out.len() + 1));
            let val = json!(node.checked.unwrap_or(false));
            out.insert(key, val);
        }
        "combobox" | "pop_up_button" | "slider" => {
            let key = node
                .name
                .clone()
                .unwrap_or_else(|| format!("select_{}", out.len() + 1));
            let val = json!(node.value.as_deref().unwrap_or(""));
            out.insert(key, val);
        }
        _ => {}
    }

    for child in &node.children {
        collect_form_fields(child, out);
    }
}

/// Extract list or menu items into an array.
fn extract_list(root: &UiNode) -> Value {
    let list_node = find_node(root, &|n| {
        matches!(n.role.as_str(), "list" | "menu" | "outline")
    })
    .unwrap_or(root);

    let mut items = Vec::new();
    collect_list_items(list_node, &mut items);
    let count = items.len();
    json!({
        "type": "list",
        "name": list_node.name,
        "items": items,
        "count": count
    })
}

fn collect_list_items(node: &UiNode, out: &mut Vec<Value>) {
    if matches!(
        node.role.as_str(),
        "list_item" | "menu_item" | "radio_button"
    ) {
        let label = node.name.as_deref().or(node.value.as_deref()).unwrap_or("");
        out.push(json!({
            "name": label,
            "selected": node.selected,
            "checked": node.checked,
            "disabled": node.disabled
        }));
        return;
    }
    for child in &node.children {
        collect_list_items(child, out);
    }
}

/// Extract custom schema fields by matching role or name patterns.
fn extract_schema(root: &UiNode, schema: &Value) -> Value {
    let Some(obj) = schema.as_object() else {
        return json!({ "type": "schema", "data": {} });
    };

    let mut data = BTreeMap::new();
    for (key, spec) in obj {
        let role_filter = spec.get("role").and_then(Value::as_str);
        let name_filter = spec.get("name").and_then(Value::as_str);
        let prop = spec
            .get("property")
            .and_then(Value::as_str)
            .unwrap_or("value");

        let matched = find_node(root, &|n| {
            if let Some(r) = role_filter {
                if !n.role.eq_ignore_ascii_case(r) {
                    return false;
                }
            }
            if let Some(name_sub) = name_filter {
                let n_name = n.name.as_deref().unwrap_or("");
                if !n_name.to_lowercase().contains(&name_sub.to_lowercase()) {
                    return false;
                }
            }
            true
        });

        if let Some(node) = matched {
            let val = match prop {
                "name" => json!(node.name.as_deref().unwrap_or("")),
                "checked" => json!(node.checked.unwrap_or(false)),
                "disabled" => json!(node.disabled),
                "selected" => json!(node.selected),
                "bounds" => json!(node.bounds),
                _ => {
                    if node.is_secure() {
                        json!("[REDACTED]")
                    } else {
                        json!(node.value.as_deref().or(node.name.as_deref()).unwrap_or(""))
                    }
                }
            };
            data.insert(key.clone(), val);
        } else {
            data.insert(key.clone(), Value::Null);
        }
    }

    json!({
        "type": "schema",
        "data": data
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tree() -> UiNode {
        UiNode {
            role: "window".into(),
            name: Some("Settings".into()),
            children: vec![
                UiNode {
                    role: "text_field".into(),
                    name: Some("Username".into()),
                    value: Some("alice".into()),
                    ..Default::default()
                },
                UiNode {
                    role: "secure_text_field".into(),
                    name: Some("Password".into()),
                    value: Some("secret123".into()),
                    secure: true,
                    ..Default::default()
                },
                UiNode {
                    role: "checkbox".into(),
                    name: Some("Remember Me".into()),
                    checked: Some(true),
                    ..Default::default()
                },
                UiNode {
                    role: "table".into(),
                    name: Some("Processes".into()),
                    children: vec![
                        UiNode {
                            role: "column".into(),
                            name: Some("PID".into()),
                            ..Default::default()
                        },
                        UiNode {
                            role: "column".into(),
                            name: Some("Name".into()),
                            ..Default::default()
                        },
                        UiNode {
                            role: "row".into(),
                            children: vec![
                                UiNode {
                                    role: "cell".into(),
                                    value: Some("123".into()),
                                    ..Default::default()
                                },
                                UiNode {
                                    role: "cell".into(),
                                    value: Some("agentctl".into()),
                                    ..Default::default()
                                },
                            ],
                            ..Default::default()
                        },
                        UiNode {
                            role: "row".into(),
                            children: vec![
                                UiNode {
                                    role: "cell".into(),
                                    value: Some("456".into()),
                                    ..Default::default()
                                },
                                UiNode {
                                    role: "cell".into(),
                                    value: Some("bash".into()),
                                    ..Default::default()
                                },
                            ],
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
                UiNode {
                    role: "list".into(),
                    name: Some("Recent".into()),
                    children: vec![
                        UiNode {
                            role: "list_item".into(),
                            name: Some("doc1.txt".into()),
                            selected: true,
                            ..Default::default()
                        },
                        UiNode {
                            role: "list_item".into(),
                            name: Some("doc2.txt".into()),
                            selected: false,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn extract_table_extracts_headers_and_rows() {
        let tree = sample_tree();
        let res = extract_data(&tree, Some("table"), None);
        assert_eq!(res["type"], "table");
        assert_eq!(res["columns"], json!(["PID", "Name"]));
        assert_eq!(res["row_count"], 2);
        assert_eq!(res["rows"][0], json!(["123", "agentctl"]));
        assert_eq!(res["rows"][1], json!(["456", "bash"]));
    }

    #[test]
    fn extract_form_extracts_fields_and_redacts_password() {
        let tree = sample_tree();
        let res = extract_data(&tree, Some("form"), None);
        assert_eq!(res["type"], "form");
        assert_eq!(res["fields"]["Username"], "alice");
        assert_eq!(res["fields"]["Password"], "[REDACTED]");
        assert_eq!(res["fields"]["Remember Me"], true);
    }

    #[test]
    fn extract_list_extracts_items() {
        let tree = sample_tree();
        let res = extract_data(&tree, Some("list"), None);
        assert_eq!(res["type"], "list");
        assert_eq!(res["count"], 2);
        assert_eq!(res["items"][0]["name"], "doc1.txt");
        assert_eq!(res["items"][0]["selected"], true);
    }

    #[test]
    fn extract_schema_extracts_requested_keys() {
        let tree = sample_tree();
        let schema = json!({
            "user": { "role": "text_field", "name": "user" },
            "remember": { "role": "checkbox", "property": "checked" },
            "missing": { "name": "nonexistent" }
        });
        let res = extract_data(&tree, Some("schema"), Some(&schema));
        assert_eq!(res["type"], "schema");
        assert_eq!(res["data"]["user"], "alice");
        assert_eq!(res["data"]["remember"], true);
        assert_eq!(res["data"]["missing"], Value::Null);
    }

    #[test]
    fn extract_table_infers_column_names_when_missing() {
        let tree = UiNode {
            role: "table".into(),
            children: vec![UiNode {
                role: "row".into(),
                children: vec![
                    UiNode {
                        role: "cell".into(),
                        value: Some("val1".into()),
                        ..Default::default()
                    },
                    UiNode {
                        role: "cell".into(),
                        value: Some("val2".into()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let res = extract_data(&tree, Some("table"), None);
        assert_eq!(res["type"], "table");
        assert_eq!(res["columns"], json!(["col_1", "col_2"]));
        assert_eq!(res["row_count"], 1);
        assert_eq!(res["rows"][0], json!(["val1", "val2"]));
    }

    #[test]
    fn extract_form_handles_all_input_types_and_anonymous_fields() {
        let tree = UiNode {
            role: "dialog".into(),
            children: vec![
                UiNode {
                    role: "text_field".into(),
                    value: Some("anonymous_text".into()),
                    ..Default::default()
                },
                UiNode {
                    role: "switch".into(),
                    name: Some("Dark Mode".into()),
                    checked: Some(true),
                    ..Default::default()
                },
                UiNode {
                    role: "combobox".into(),
                    name: Some("Theme".into()),
                    value: Some("Solarized".into()),
                    ..Default::default()
                },
                UiNode {
                    role: "slider".into(),
                    value: Some("75".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let res = extract_data(&tree, Some("form"), None);
        assert_eq!(res["type"], "form");
        assert_eq!(res["count"], 4);
        assert_eq!(res["fields"]["field_1"], "anonymous_text");
        assert_eq!(res["fields"]["Dark Mode"], true);
        assert_eq!(res["fields"]["Theme"], "Solarized");
        assert_eq!(res["fields"]["select_4"], "75");
    }

    #[test]
    fn extract_schema_handles_bounds_and_selected() {
        let mut tree = sample_tree();
        tree.bounds = Some(crate::tree::Bounds {
            x: 10.0,
            y: 20.0,
            w: 300.0,
            h: 400.0,
        });
        let schema = json!({
            "win_bounds": { "role": "window", "property": "bounds" },
            "first_item_selected": { "role": "list_item", "property": "selected" },
            "item_disabled": { "role": "list_item", "property": "disabled" },
            "user_label": { "role": "text_field", "property": "name" }
        });
        let res = extract_data(&tree, Some("schema"), Some(&schema));
        assert_eq!(res["type"], "schema");
        assert_eq!(res["data"]["win_bounds"]["w"], 300.0);
        assert_eq!(res["data"]["first_item_selected"], true);
        assert_eq!(res["data"]["item_disabled"], false);
        assert_eq!(res["data"]["user_label"], "Username");
    }

    #[test]
    fn extract_list_fallback_to_root() {
        let tree = UiNode {
            role: "panel".into(),
            children: vec![
                UiNode {
                    role: "menu_item".into(),
                    name: Some("Open".into()),
                    ..Default::default()
                },
                UiNode {
                    role: "menu_item".into(),
                    name: Some("Save".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let res = extract_data(&tree, Some("list"), None);
        assert_eq!(res["type"], "list");
        assert_eq!(res["count"], 2);
        assert_eq!(res["items"][0]["name"], "Open");
        assert_eq!(res["items"][1]["name"], "Save");
    }
}
