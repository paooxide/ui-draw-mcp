use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::arena::{RefError, SnapshotArena};
use crate::backend::{A11yBackend, BackendError, SnapshotRequest};
use crate::diff::diff_snapshots;
use crate::flatten::{flatten, FlattenConfig};
use crate::query::{parse_query, query_schema, query_snapshot};

/// The `vision` perception engine: `get_ui_tree` (observe) and `get_element`
/// (read one property). Owns the [`SnapshotArena`] so refs from the latest
/// `get_ui_tree` resolve in `get_element` (and, later, in input tools).
/// Below this many refs a tree is reported as `sparse` with a fallback hint.
const SPARSE_TREE_REFS: usize = 5;

pub struct A11yModule {
    backend: Arc<dyn A11yBackend>,
    arena: Arc<Mutex<SnapshotArena>>,
    max_chars: usize,
    judge: Option<Arc<mcp_judge::Judge>>,
}

impl A11yModule {
    pub fn new(backend: Arc<dyn A11yBackend>, max_chars: usize) -> Self {
        A11yModule {
            backend,
            arena: Arc::new(Mutex::new(SnapshotArena::new())),
            max_chars,
            judge: None,
        }
    }

    /// Attach the judge that ranks `describe` queries.
    pub fn with_judge(mut self, judge: Arc<mcp_judge::Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Shared arena handle (input tools will consume this once wired into
    /// `CallCtx`).
    pub fn arena(&self) -> Arc<Mutex<SnapshotArena>> {
        self.arena.clone()
    }

    /// Ids come from the arena so every observation, from whichever tool,
    /// draws from one sequence.
    fn next_snapshot_id(&self) -> String {
        self.arena
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_id()
    }

    async fn get_ui_tree(&self, args: &Value) -> Envelope {
        let req = SnapshotRequest {
            app: str_arg(args, "app"),
            skeleton: args
                .get("skeleton")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            root_ref: str_arg(args, "root"),
            max_depth: args
                .get("max_depth")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            surface: str_arg(args, "surface"),
        };

        let raw = match self.backend.snapshot(&req).await {
            Ok(r) => r,
            Err(e) => return backend_err("get_ui_tree", e),
        };

        let sid = self.next_snapshot_id();
        let cfg = FlattenConfig {
            max_chars: self.max_chars,
            skeleton: req.skeleton,
            terminal_app: raw.terminal_app,
            ..FlattenConfig::default()
        };
        let f = flatten(
            &raw.root,
            raw.app.as_deref(),
            raw.window.as_deref(),
            &sid,
            &cfg,
        );

        let crate::flatten::Flattened {
            text,
            snapshot,
            ref_count,
            truncated,
            used_skeleton,
        } = f;

        // `since`: report what changed rather than everything. After an action
        // almost nothing on screen differs, and re-reading the whole tree to
        // find that out is the cost this avoids.
        let since = str_arg(args, "since");
        let mut delta = None;
        {
            let mut arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(id) = &since {
                match arena.get(id) {
                    Some(before) => delta = Some(diff_snapshots(before, &snapshot).to_json()),
                    None => {
                        let known = arena.known_ids().join(", ");
                        return Envelope::fail_with(
                            "get_ui_tree",
                            ErrorCode::NotFound,
                            format!("snapshot '{id}' is no longer retained"),
                            if known.is_empty() {
                                "call get_ui_tree without 'since' to observe from scratch"
                                    .to_string()
                            } else {
                                format!("retained snapshots: {known}")
                            },
                        );
                    }
                }
            }
            arena.install(snapshot);
        }

        // Some apps (SwiftUI, Electron, canvas/custom-drawn UI, games) expose an
        // almost-empty accessibility tree. Say so explicitly rather than letting
        // the agent conclude the window is empty: screen capture plus
        // coordinate input is the documented fallback.
        let mut data = json!({
            "snapshot_id": sid,
            "app": raw.app,
            "window": raw.window,
            "ref_count": ref_count,
            "truncated": truncated,
            "skeleton": used_skeleton,
        });
        // In delta mode the full text is the thing being avoided, so it is sent
        // only when asked for. Refs in the delta resolve against the snapshot
        // just installed, so they remain actionable either way.
        let want_text = since.is_none()
            || args
                .get("include_text")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        if want_text {
            data["text"] = json!(text);
        }
        if let Some(d) = delta {
            data["since"] = json!(since);
            data["delta"] = d;
        }
        // A partial tree and a genuinely small one look identical, and an agent
        // that cannot tell them apart concludes the control it needs is absent.
        if raw.partial {
            data["partial"] = json!(true);
            data["hint"] = json!(
                "observing this app took too long, so the tree stops early and is incomplete. \
                 Narrow it: pass 'app' to scope to one application, 'surface' to one window or \
                 sheet, 'root' to drill into a container, or use find_elements to search for \
                 what you need instead of reading everything."
            );
        }
        if !raw.partial && since.is_none() && ref_count < SPARSE_TREE_REFS && !used_skeleton {
            data["sparse"] = json!(true);
            data["hint"] = json!(
                "this app exposes few accessibility elements: its UI may be custom-drawn                  (SwiftUI/Electron/canvas). Fall back to capture_screen plus coordinate                  input (mouse_action/scroll), or browser_* if it is web content."
            );
        }
        Envelope::ok("get_ui_tree", data)
    }

    /// Search the UI instead of reading all of it.
    ///
    /// Takes a fresh snapshot and installs it, so every ref returned is valid
    /// for `ui_action` until the next observation. Querying a *retained* older
    /// snapshot would be cheaper and would hand back refs that resolve to
    /// nothing: or, worse, to a different control.
    async fn find_elements(&self, args: &Value) -> Envelope {
        let q = match parse_query(args) {
            Ok(q) => q,
            Err(msg) => return Envelope::fail("find_elements", ErrorCode::InvalidArgs, msg),
        };
        let req = SnapshotRequest {
            app: str_arg(args, "app"),
            skeleton: false,
            root_ref: None,
            max_depth: None,
            surface: str_arg(args, "surface"),
        };
        let raw = match self.backend.snapshot(&req).await {
            Ok(r) => r,
            Err(e) => return backend_err("find_elements", e),
        };
        let sid = self.next_snapshot_id();
        // No character budget here: the text is never returned, and a truncated
        // element map would silently drop matches.
        let cfg = FlattenConfig {
            max_chars: usize::MAX,
            skeleton: false,
            terminal_app: raw.terminal_app,
            ..FlattenConfig::default()
        };
        let f = flatten(
            &raw.root,
            raw.app.as_deref(),
            raw.window.as_deref(),
            &sid,
            &cfg,
        );
        let snapshot = f.snapshot;
        let (mut hits, total) = query_snapshot(&snapshot, &q);
        {
            let mut arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
            arena.install(snapshot);
        }
        let mut ranking = json!(null);
        if let Some(describe) = &q.describe {
            let Some(judge) = self.judge.as_ref().filter(|j| j.enabled()) else {
                return Envelope::fail_with(
                    "find_elements",
                    ErrorCode::UnsupportedOs,
                    "'describe' needs the judge, which is not enabled",
                    "set [judge] enabled = \"true\" and provide TYPESAFE_API_KEY, or query by role, name or near",
                );
            };
            if hits.is_empty() {
                // Nothing to rank; the deterministic answer stands.
            } else {
                let considered = hits.len().min(crate::query::MAX_DESCRIBE_CANDIDATES);
                let candidates: std::collections::BTreeMap<String, String> = hits
                    .iter()
                    .take(considered)
                    .map(|h| {
                        (
                            h.reff.clone(),
                            crate::query::candidate_line(h, raw.window.as_deref()),
                        )
                    })
                    .collect();
                let state = json!({
                    "request": describe,
                    "application": raw.app,
                    "window": raw.window,
                    "candidates": candidates,
                });
                match judge
                    .rank(
                        state,
                        "Which candidate in `candidates` (keyed by element ref) is the user-interface element that `request` describes? Judge by role, name, value and placement in `window`.",
                        &candidates,
                    )
                    .await
                {
                    Ok(r) => {
                        hits = crate::query::apply_ranking(hits, &r.probabilities, q.limit);
                        ranking = json!({
                            "best": r.choice,
                            "confidence": r.confidence,
                            "any_fits": r.any_fits,
                            "considered": considered,
                            "considered_all": considered == total,
                        });
                    }
                    Err(e) => {
                        return Envelope::fail_with(
                            "find_elements",
                            ErrorCode::ActionFailed,
                            format!("'describe' could not be ranked: {}", e.message()),
                            "query by role, name or near instead",
                        );
                    }
                }
            }
        }
        let mut data = json!({
            "snapshot_id": sid,
            "app": raw.app,
            "window": raw.window,
            "count": hits.len(),
            "total_matched": total,
            "truncated": total > hits.len(),
            // The search covered only part of the UI, so "not found" here
            // does not mean "not present".
            "partial": raw.partial,
            "elements": hits,
        });
        if !ranking.is_null() {
            data["ranking"] = ranking;
        }
        Envelope::ok("find_elements", data)
    }

    fn get_element(&self, args: &Value) -> Envelope {
        let Some(reff) = str_arg(args, "ref") else {
            return Envelope::fail("get_element", ErrorCode::InvalidArgs, "missing 'ref'");
        };
        if !valid_ref(&reff) {
            return Envelope::fail(
                "get_element",
                ErrorCode::InvalidArgs,
                "ref must match @e<number>",
            );
        }
        let property = str_arg(args, "property").unwrap_or_else(|| "value".to_string());

        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        let info = match arena.resolve_latest(&reff) {
            Ok(i) => i,
            Err(RefError::Stale) => {
                return Envelope::fail_with(
                    "get_element",
                    ErrorCode::StaleRef,
                    "no current snapshot for this ref",
                    "call get_ui_tree first",
                );
            }
            Err(RefError::NotFound) => {
                return Envelope::fail(
                    "get_element",
                    ErrorCode::NotFound,
                    "no such element in the latest snapshot",
                );
            }
        };

        // Flattening already drops a secure field's value, so none should reach
        // here. This is the second lock on the same door: whatever built the
        // snapshot, `value` and `all` withhold it the same way.
        let mut info = info.clone();
        if info.secure {
            info.value_preview = None;
        }

        let value = match property.as_str() {
            "role" => json!(info.role),
            "name" => json!(info.name),
            "value" => json!(info.value_preview),
            "secure" => json!(info.secure),
            "bounds" => match info.bounds {
                Some(b) => json!({ "x": b.x, "y": b.y, "w": b.w, "h": b.h }),
                None => Value::Null,
            },
            "semantic_intent" => json!(info.semantic_intent),
            "bound_state" => json!(info.bound_state),
            "all" => json!(info),
            other => {
                return Envelope::fail(
                    "get_element",
                    ErrorCode::InvalidArgs,
                    format!("unsupported property '{other}' (use role|name|value|secure|bounds|semantic_intent|bound_state|all)"),
                );
            }
        };
        Envelope::ok(
            "get_element",
            json!({ "ref": reff, "property": property, "value": value }),
        )
    }

    async fn ui_extract(&self, args: &Value) -> Envelope {
        let tool = "ui_extract";
        let req = SnapshotRequest {
            app: str_arg(args, "app"),
            skeleton: false,
            root_ref: str_arg(args, "root"),
            max_depth: None,
            surface: str_arg(args, "surface"),
        };

        let raw = match self.backend.snapshot(&req).await {
            Ok(r) => r,
            Err(e) => return backend_err(tool, e),
        };

        let sid = self.next_snapshot_id();
        let cfg = FlattenConfig {
            max_chars: usize::MAX,
            skeleton: false,
            terminal_app: raw.terminal_app,
            ..FlattenConfig::default()
        };
        let f = flatten(
            &raw.root,
            raw.app.as_deref(),
            raw.window.as_deref(),
            &sid,
            &cfg,
        );
        {
            let mut arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
            arena.install(f.snapshot);
        }

        let mode = str_arg(args, "mode");
        let schema = args.get("schema");
        let extracted = crate::extract::extract_data(&raw.root, mode.as_deref(), schema);

        Envelope::ok(tool, extracted)
    }
}

#[async_trait]
impl ToolModule for A11yModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "get_ui_tree",
                Category::Vision,
                Tier::Read,
                "Observe the UI. Returns interactive elements with refs (@e1, @e2…) as \
                 flattened text. Use skeleton=true first on dense apps, then root=@eN to \
                 drill into one container. Refs are valid only for the latest snapshot.",
                json!({
                    "type": "object",
                    "properties": {
                        "app": { "type": "string", "description": "target application name" },
                        "skeleton": { "type": "boolean", "description": "depth-limited overview" },
                        "root": { "type": "string", "description": "drill into this container ref (@eN)" },
                        "max_depth": { "type": "integer", "description": "limit tree depth" },
                        "surface": {
                            "type": "string",
                            "enum": ["window", "focused", "menu", "menubar", "sheet", "popover", "alert"],
                            "description": "which UI surface to observe"
                        },
                        "since": {
                            "type": "string",
                            "description": "a previous snapshot_id: return only what changed since then, instead of the whole tree"
                        },
                        "include_text": {
                            "type": "boolean",
                            "description": "with 'since', also return the full tree text (default false)"
                        }
                    },
                    "required": []
                }),
            ).untrusted_output(),
            ToolDescriptor::new(
                "find_elements",
                Category::Vision,
                Tier::Read,
                "Find elements without reading the whole UI. Filter by role and/or a \
                 case-insensitive substring of the name, or rank by distance from a \
                 screen point. With 'describe', say what you want in plain language \
                 ('the button that saves the document') and the candidates come back \
                 ranked, each with a probability, plus 'ranking.any_fits' for whether \
                 anything matched at all (needs the judge enabled). Much cheaper than \
                 get_ui_tree on a busy app. Takes a fresh snapshot, so the refs it \
                 returns are usable by ui_action until the next observation.",
                query_schema(),
            ).untrusted_output(),
            ToolDescriptor::new(
                "get_element",
                Category::Vision,
                Tier::Read,
                "Read one property of an element by ref from the latest snapshot.",
                json!({
                    "type": "object",
                    "properties": {
                        "ref": { "type": "string", "description": "element ref, e.g. @e3" },
                        "property": {
                            "type": "string",
                            "enum": ["role", "name", "value", "secure", "bounds"],
                            "description": "which property to read"
                        }
                    },
                    "required": ["ref"]
                }),
            ).untrusted_output(),
            ToolDescriptor::new(
                "ui_extract",
                Category::Vision,
                Tier::Read,
                "Extract structured data (tables, forms, lists, or custom schemas) directly \
                 from native accessibility trees into JSON without parsing raw text dumps.",
                json!({
                    "type": "object",
                    "properties": {
                        "app": { "type": "string", "description": "target application name" },
                        "root": { "type": "string", "description": "drill into this container ref (@eN)" },
                        "surface": {
                            "type": "string",
                            "enum": ["window", "focused", "menu", "menubar", "sheet", "popover", "alert"],
                            "description": "which UI surface to observe"
                        },
                        "mode": {
                            "type": "string",
                            "enum": ["table", "form", "list", "schema"],
                            "description": "extraction mode (table, form, list, schema)"
                        },
                        "schema": {
                            "type": "object",
                            "description": "custom extraction schema (field -> { role, name, property })"
                        }
                    },
                    "required": []
                }),
            ).untrusted_output(),
        ]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "get_ui_tree" => self.get_ui_tree(&args).await,
            "find_elements" => self.find_elements(&args).await,
            "get_element" => self.get_element(&args),
            "ui_extract" => self.ui_extract(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn valid_ref(s: &str) -> bool {
    s.strip_prefix("@e")
        .map(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

fn backend_err(tool: &str, e: BackendError) -> Envelope {
    let (code, msg) = match e {
        BackendError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
        BackendError::NotFound(m) => (ErrorCode::NotFound, m),
        BackendError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
        BackendError::Failed(m) => (ErrorCode::ActionFailed, m),
    };
    Envelope::fail(tool, code, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::{ElementInfo, Snapshot};
    use crate::backend::RawSnapshot;
    use crate::tree::UiNode;

    const SECRET: &str = "hunter2-correct-horse";

    /// A tree source that returns a fixed form: one plain field and one
    /// password field that carries its value, as AX does.
    struct Form;

    #[async_trait]
    impl A11yBackend for Form {
        async fn snapshot(&self, _: &SnapshotRequest) -> Result<RawSnapshot, BackendError> {
            let field = |name: &str, value: &str, subrole: Option<&str>| UiNode {
                role: "textfield".into(),
                name: Some(name.into()),
                value: Some(value.into()),
                subrole: subrole.map(str::to_string),
                ..Default::default()
            };
            Ok(RawSnapshot {
                root: UiNode {
                    role: "application".into(),
                    children: vec![
                        field("Username", "alice", None),
                        field("Password", SECRET, Some("AXSecureTextField")),
                    ],
                    ..Default::default()
                },
                app: Some("Login".into()),
                window: Some("Sign in".into()),
                terminal_app: false,
                partial: false,
            })
        }
        fn platform(&self) -> &'static str {
            "fake"
        }
    }

    fn ctx() -> CallCtx {
        CallCtx::new("t", mcp_types::CancelToken::new())
    }

    async fn get(module: &A11yModule, reff: &str, property: &str) -> Envelope {
        module
            .call(
                "get_element",
                json!({ "ref": reff, "property": property }),
                &ctx(),
            )
            .await
    }

    /// Through the real path: `get_ui_tree` flattens, `get_element` reads.
    /// `property=all` must not carry the password in any field.
    #[tokio::test]
    async fn get_element_all_does_not_leak_a_secure_value_through_the_real_path() {
        let module = A11yModule::new(Arc::new(Form), 10_000);
        let tree = module.call("get_ui_tree", json!({}), &ctx()).await;
        assert!(tree.ok, "{tree:?}");
        // Which ref is the password field?
        let (secret_ref, plain_ref) = {
            let arena = module.arena();
            let arena = arena.lock().unwrap();
            let snap = arena.current().unwrap();
            let find = |name: &str| {
                snap.elements
                    .iter()
                    .find(|(_, i)| i.name.as_deref() == Some(name))
                    .map(|(r, _)| r.clone())
                    .unwrap()
            };
            (find("Password"), find("Username"))
        };

        for property in ["all", "value"] {
            let env = get(&module, &secret_ref, property).await;
            assert!(env.ok);
            let text = serde_json::to_string(&env).unwrap();
            assert!(!text.contains(SECRET), "{property} leaked: {text}");
        }
        let all = get(&module, &secret_ref, "all").await;
        assert_eq!(all.data.as_ref().unwrap()["value"]["secure"], true);
        assert!(all.data.unwrap()["value"]["value_preview"].is_null());

        // The redaction is for secure fields only.
        let plain = get(&module, &plain_ref, "all").await;
        assert_eq!(plain.data.unwrap()["value"]["value_preview"], "alice");
    }

    /// Defence in depth: a snapshot that arrives with a secure element already
    /// carrying a value (flattening bypassed) is still withheld.
    #[tokio::test]
    async fn get_element_withholds_a_secure_value_even_if_the_snapshot_carries_one() {
        let module = A11yModule::new(Arc::new(Form), 10_000);
        let mut snap = Snapshot {
            id: "s1".into(),
            app: None,
            window: None,
            elements: Default::default(),
            skeleton: false,
        };
        snap.elements.insert(
            "@e1".into(),
            ElementInfo {
                role: "secure_text_field".into(),
                name: Some("Password".into()),
                value_preview: Some(SECRET.into()),
                secure: true,
                bounds: None,
                node_id: None,
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        module.arena().lock().unwrap().install(snap);
        for property in ["all", "value"] {
            let env = get(&module, "@e1", property).await;
            let text = serde_json::to_string(&env).unwrap();
            assert!(!text.contains(SECRET), "{property} leaked: {text}");
        }
    }
}
