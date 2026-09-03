use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::arena::{RefError, SnapshotArena};
use crate::backend::{A11yBackend, BackendError, SnapshotRequest};
use crate::flatten::{flatten, FlattenConfig};

/// The `vision` perception engine: `get_ui_tree` (observe) and `get_element`
/// (read one property). Owns the [`SnapshotArena`] so refs from the latest
/// `get_ui_tree` resolve in `get_element` (and, later, in input tools).
/// Below this many refs a tree is reported as `sparse` with a fallback hint.
const SPARSE_TREE_REFS: usize = 5;

pub struct A11yModule {
    backend: Arc<dyn A11yBackend>,
    arena: Arc<Mutex<SnapshotArena>>,
    counter: AtomicU64,
    max_chars: usize,
}

impl A11yModule {
    pub fn new(backend: Arc<dyn A11yBackend>, max_chars: usize) -> Self {
        A11yModule {
            backend,
            arena: Arc::new(Mutex::new(SnapshotArena::new())),
            counter: AtomicU64::new(1),
            max_chars,
        }
    }

    /// Shared arena handle (input tools will consume this once wired into
    /// `CallCtx`).
    pub fn arena(&self) -> Arc<Mutex<SnapshotArena>> {
        self.arena.clone()
    }

    fn next_snapshot_id(&self) -> String {
        format!("s{:x}", self.counter.fetch_add(1, Ordering::SeqCst))
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
        {
            let mut arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
            arena.install(snapshot);
        }

        // Some apps (SwiftUI, Electron, canvas/custom-drawn UI, games) expose an
        // almost-empty accessibility tree. Say so explicitly rather than letting
        // the agent conclude the window is empty — screen capture plus
        // coordinate input is the documented fallback (planning.md §5.2, D10).
        let mut data = json!({
            "snapshot_id": sid,
            "app": raw.app,
            "window": raw.window,
            "text": text,
            "ref_count": ref_count,
            "truncated": truncated,
            "skeleton": used_skeleton,
        });
        if ref_count < SPARSE_TREE_REFS && !used_skeleton {
            data["sparse"] = json!(true);
            data["hint"] = json!(
                "this app exposes few accessibility elements — its UI may be custom-drawn                  (SwiftUI/Electron/canvas). Fall back to capture_screen plus coordinate                  input (mouse_action/scroll), or browser_* if it is web content."
            );
        }
        Envelope::ok("get_ui_tree", data)
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

        let value = match property.as_str() {
            "role" => json!(info.role),
            "name" => json!(info.name),
            "value" => json!(info.value_preview),
            "secure" => json!(info.secure),
            "bounds" => match info.bounds {
                Some(b) => json!({ "x": b.x, "y": b.y, "w": b.w, "h": b.h }),
                None => Value::Null,
            },
            other => {
                return Envelope::fail(
                    "get_element",
                    ErrorCode::InvalidArgs,
                    format!("unsupported property '{other}' (use role|name|value|secure|bounds)"),
                );
            }
        };
        Envelope::ok(
            "get_element",
            json!({ "ref": reff, "property": property, "value": value }),
        )
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
                        }
                    },
                    "required": []
                }),
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
        ]
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "get_ui_tree" => self.get_ui_tree(&args).await,
            "get_element" => self.get_element(&args),
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
