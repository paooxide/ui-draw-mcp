use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mcp_a11y::matcher::{near_misses, parse_ref, rank_by_name, tied_with_best, Candidate};
use mcp_a11y::{
    flatten, A11yBackend, BackendError, Bounds, ElementInfo, FlattenConfig, RefError,
    SnapshotArena, SnapshotRequest,
};
use mcp_types::args::{as_f64, i64_arg, u64_arg};
use mcp_types::drag::{drag_route, parse_drag_path};
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::backend::{
    BackgroundTarget, ClipFormat, FocusedText, InputBackend, InputError, MouseKind, Reading, Route,
    ScrollDir, Selection, SemanticAction,
};
use crate::combo::{parse_combo, parse_hold, Os};
use crate::hold::{mono_ms, HeldKeys};
use crate::selection::{clip_echo, judge_selection, resolve, RangeSpec};

/// Per-action policy the input engine enforces (the tier/category gate already
/// ran in `mcp-core`; these are the input-specific checks).
#[derive(Debug, Clone)]
pub struct InputPolicy {
    /// Restrict coordinate input to the bounds of the current snapshot.
    pub clamp_input: bool,
    /// App-name fragments considered terminals (destructive gate scope).
    pub terminal_apps: Vec<String>,
    /// No consent channel: destructive matches become denials.
    pub autonomous: bool,
    /// Bypass profile: the destructive gate is off entirely.
    pub bypass: bool,
    /// Destructive-command substrings.
    pub destructive_patterns: Vec<String>,
    /// The judge, consulted after the patterns and only able to add a flag.
    pub judge: Option<std::sync::Arc<mcp_policy::mcp_judge::Judge>>,
    /// `never` refuses every call that would move the real pointer, post
    /// synthetic keys or scroll events, or bring an app forward. See
    /// [`crate::foreground`].
    pub foreground: mcp_types::Foreground,
    /// How long `foreground = "polite"` waits for the person to be idle.
    pub polite: crate::polite::PoliteConfig,
}

/// Apps whose keystrokes may be shell input.
///
/// Editors are in this list because their *integrated terminals* are: a
/// `rm -rf` typed into VS Code's panel runs exactly as it would in Terminal.app,
/// and the accessibility API cannot tell the editor pane from the terminal pane
/// (Electron and GPUI both render their own text). Treating the whole app as a
/// terminal over-triggers on source files that merely contain the string, the
/// safe direction to be wrong in, and it only ever asks rather than refuses.
pub fn default_terminal_apps() -> Vec<String> {
    [
        "Terminal",
        "iTerm",
        "Warp",
        "Alacritty",
        "kitty",
        "Ghostty",
        "WezTerm",
        "Hyper",
        "Tabby",
        // Editors with integrated terminals.
        "Code",
        "Visual Studio Code",
        "VSCodium",
        "Cursor",
        "Windsurf",
        "Zed",
        "Sublime Text",
        "JetBrains",
        "IntelliJ",
        "PyCharm",
        "WebStorm",
        "RustRover",
        "Xcode",
        "Nova",
        "Emacs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Read and validate a `modifiers` array. An unrecognised name is an error
/// rather than a silent drop: a `cmd+click` that quietly became a plain
/// click would look like it worked and do the wrong thing.
#[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope, short-lived
fn parse_modifiers(tool: &str, args: &Value) -> Result<Vec<String>, Envelope> {
    let Some(list) = args.get("modifiers") else {
        return Ok(Vec::new());
    };
    let Some(arr) = list.as_array() else {
        return Err(Envelope::fail(
            tool,
            ErrorCode::InvalidArgs,
            "'modifiers' must be an array of strings",
        ));
    };
    let mut out = Vec::with_capacity(arr.len());
    for m in arr {
        let raw = m.as_str().ok_or_else(|| {
            Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'modifiers' entries must be strings",
            )
        })?;
        // `Cmd` and `cmd` are the same key; the backends only know the latter.
        let lowered = raw.trim().to_lowercase();
        let name = lowered.as_str();
        if !crate::backend::valid_modifier(name) {
            return Err(Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown modifier '{raw}'"),
                "use cmd, shift, opt/alt, ctrl or fn",
            ));
        }
        out.push(name.to_string());
    }
    Ok(out)
}

/// Should text typed at `target` be screened as shell input?
///
/// `None` means the destination could not be determined, and that is treated as
/// a terminal. An unknown destination is not evidence of safety, and the case
/// that produces it is exactly the dangerous one: an app that exposes no
/// accessibility tree, which is what Electron and GPUI editors do.
pub fn screens_as_terminal(target: Option<&str>, terminal_apps: &[String]) -> bool {
    let Some(app) = target else {
        return true;
    };
    let lower = app.to_ascii_lowercase();
    terminal_apps
        .iter()
        .any(|t| lower.contains(&t.to_ascii_lowercase()))
}

impl Default for InputPolicy {
    fn default() -> Self {
        InputPolicy {
            clamp_input: true,
            terminal_apps: default_terminal_apps(),
            autonomous: false,
            bypass: false,
            destructive_patterns: mcp_policy::default_destructive_patterns(),
            judge: None,
            foreground: mcp_types::Foreground::Allow,
            polite: crate::polite::PoliteConfig::default(),
        }
    }
}

/// The `input` engine. Shares the `mcp-a11y` arena so it can act on refs from
/// the latest `get_ui_tree`.
/// Tools that accept an `expect` clause: the ones that change the UI and whose
/// effect is therefore worth confirming in the same call.
const EXPECTING: &[&str] = &[
    "ui_action",
    "set_value",
    "ui_fill_form",
    "keyboard_type",
    "keyboard_shortcut",
    "mouse_action",
    "select_text",
];

/// Tools that report `effects`: those that can change what is on screen. A
/// scroll, hover or drag moves the view or the pointer and is left out; the
/// read tools change nothing.
const EFFECTING: &[&str] = &[
    "ui_action",
    "set_value",
    "ui_fill_form",
    "keyboard_type",
    "keyboard_shortcut",
    "mouse_action",
    "select_text",
];

/// The tier `input_steps` is admitted at. A step above it is refused.
const BATCH_TIER: Tier = Tier::Standard;

/// What the docs say about `input_steps` and `effects`, kept off the wire.
const BATCH_DETAILS: &str = "`steps` is a list of `{tool, args}`, at most 20. The tools that can run in a batch are ui_action, set_value, \
    select_text, ui_fill_form, keyboard_type, keyboard_shortcut, mouse_action, scroll, hover, drag_drop and clipboard_write; \
    anything else, and a step that is not `{tool, args}`, is an INVALID_ARGS error naming the step, before any step runs.\n\n\
    Each step goes through the same gate as a call made on its own: the policy tier, the role's allowed and denied tools, \
    consent, the hard invariants, the kill switch, the denial budget and the audit log (every step is its own entry, and a \
    step flagged `secret` is redacted in it). So a batch can do nothing that the same calls made one at a time could not, \
    and a step the role or `foreground = \"never\"` refuses ends the batch. Under `never` the whole batch is checked first, \
    so one refused step means no step ran. A synthetic personal-data token cannot be passed in a batch; type it with its \
    own call.\n\n\
    Steps run in order and the first failure stops the batch; the steps after it do not run. The result is {ran, total, \
    failed_at (the 0-based step that failed, absent when all succeeded), elapsed_ms, steps: [{i, tool, ok, ...that step's \
    result, error}]}. The batch has 30 seconds: a step still running then is cancelled and the rest are not run. The \
    kill switch, a human taking the pointer and a client cancel are checked between steps. A key held with \
    keyboard_shortcut down stays held across the batch, and a step that fails releases held keys as it would alone. \
    A step that names an element by `name` resolves it against the snapshot at that moment, so an earlier step that \
    changes the screen can change what the next one finds: use `ref`s from the same get_ui_tree for a run of \
    steps on one screen, and put a step that opens a dialog last in the batch.";

/// The `effects` an action reports. Appended to the descriptors of the tools that report them.
const EFFECTS_DETAILS: &str = "`effects` is added to the result of an action when something it changed was seen: `app` (the frontmost \
    application changed), `focus` ({role, name} of the newly focused element), `appeared` and `disappeared` (titles of \
    the target application's windows, at most 5 each), `retitled` ({from, to} when one window was renamed), `overlay` \
    ({kind: sheet|dialog|alert|popover|menu, title, text, buttons} of one that opened) and `overlay_closed`, and `value` \
    or `checked` of the element a `ui_action` pressed by `ref`. It is read from a small fixed set of facts before and \
    after the action, about 120 ms apart, not from a new snapshot, and without raising a window or moving focus, so it \
    works under `foreground = \"never\"`. A fact the platform cannot read is left out, never guessed. No `effects` means \
    nothing was seen to change among those facts, which is not proof that nothing did: an app may react later, and a \
    change inside a window's own contents is not among them. Linux reads the window list, dialogs and the frontmost \
    application only. `effects: false` skips the reads and the wait, and `expect` replaces it with its own delta.";

const RECORD_DETAILS: &str = "Recording listens to the keyboard and mouse through a listen-only event tap (macOS, `kCGEventTapOptionListenOnly`), \
    which needs the Input Monitoring permission and Accessibility for the app that runs agentctl; without them the call \
    returns PERM_DENIED naming which and records nothing. It never acts. Each click is resolved through accessibility to the \
    element under the pointer (role, title, parents up to the window, and the point as a fallback hint); each run of typing \
    becomes one `type` step on the field that had the focus; chords and non-printing keys become `shortcut` steps; a click \
    on a menu item keeps its menu path. Text typed into a secure field is cleared as it is read and the step holds only a \
    `secret_ref`, supplied as `secrets` when the flow runs. Events whose application is not in `allowed_apps` are counted \
    (`dropped_other_apps`) and not stored; so are events with no readable element (`dropped_unlocated`). A session stops \
    by itself after `max_seconds` (at most 600) or 4000 events, and a marker file in the state directory makes \
    `agentctl doctor` report it while it runs. Dangerous tier: it records a person's real input, so it needs to be named in \
    `policy.enable` and is not available by default. Starting does not arm the human-override watcher, but a recording \
    that starts within its grace period (1.5 s) of an agent input call is still inside it. Linux has no listen-only tap in \
    the remote-desktop portal (it can inject input, not observe it), so it returns UNSUPPORTED_OS there; flows written \
    with `desktop_flow save` replay on both.";

const FLOW_DETAILS: &str = "A flow is a list of steps `{op, locator, text | combo | secret_ref, ...}` kept in `desktop_flows.json` beside the \
    config. `run` takes a snapshot of each step's application without pinning it, finds the element whose role and title \
    match and whose parents agree (exact), or whose title merely contains the recorded one or whose surroundings changed \
    (fuzzy), and performs the step as the ordinary call it stands for: `ui_action` for a click (an accessibility press, so \
    it can run with the app in the background), `keyboard_type` or `set_value` for text, `keyboard_shortcut` for a chord. \
    Only when nothing matches does a click fall back to the recorded point with `mouse_action`, which \
    `foreground = \"never\"` refuses like any pointer action. Two elements that fit equally well are an error, not a guess. \
    Each step in the result has `matched` (exact, fuzzy or coordinate), `route` (`ax`, `hid` or `pid_event`, as the tool \
    reported) and the ref used; the first failure stops the run with `failed_at` and what to do. Under `never` the steps \
    that would use the keyboard are refused before anything runs. `value set` steps are written by hand with `save` \
    (op `set`); the recorder cannot see a value being set, only keys.";

const EFFECTS_SEE_ALSO: &str = "The result carries `effects` when the action changed something that was seen (a sheet or window opened, \
    focus moved, ...); see ui_action for what it holds and what its absence means. `effects: false` skips it.";

pub struct InputModule {
    pub(crate) backend: Arc<dyn InputBackend>,
    pub(crate) arena: Arc<Mutex<SnapshotArena>>,
    pub(crate) policy: InputPolicy,
    /// Runs `expect` clauses. `None` where there is no window backend to
    /// observe with, in which case an `expect` is refused rather than ignored.
    verifier: Option<std::sync::Arc<crate::postcondition::Verifier>>,
    /// Marks the session as driving, so the human-override watcher only
    /// samples the pointer when a divergence would mean something.
    activity: Option<std::sync::Arc<crate::Activity>>,
    /// Takes a snapshot when a call names an element before anything has been
    /// observed. `None` where the composition root wired none, in which case
    /// a name needs a prior `get_ui_tree`.
    pub(crate) observer: Option<Arc<dyn A11yBackend>>,
    /// Configured cursor glide for demo/showcase movements.
    glide: Mutex<crate::glide::GlideConfig>,
    /// Keys held across calls, shared with the watchdog that releases them.
    ledger: Arc<Mutex<Ledger>>,
    /// Whether the kill switch is engaged, for the watchdog. The switch also
    /// refuses every later call, `up` included, so a held key could never be
    /// released through the tools once it trips; this is how it still is.
    pub(crate) halt: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Reads the facts `effects` compares. `None` where nothing can, in which
    /// case no result carries `effects`.
    probe: Option<Arc<dyn crate::effects::UiProbe>>,
    /// The way back into the server's gate for the steps of `input_steps`.
    /// `None` refuses the batch: running a step any other way would skip the
    /// checks a call on its own gets.
    pub(crate) relay: Option<Arc<crate::steps::StepRelay>>,
    /// Wall time a batch may spend; [`crate::steps::MAX_BATCH_MS`] outside tests.
    batch_ms: u64,
    /// Set while an `input_steps` batch holds a polite loan, so its steps run
    /// inside that one borrow instead of waiting and restoring each.
    batch_loan: std::sync::atomic::AtomicBool,
    /// Recording and the flows it makes; empty until the composition root
    /// wires a listener and a store.
    pub(crate) rec: crate::record_tools::RecordSupport,
}

/// What the module holds down.
struct Ledger {
    keys: HeldKeys,
    /// Whether a watchdog task is running. Kept under the same lock as the
    /// keys so a `down` can never find the watchdog gone just as it exits.
    watching: bool,
}

/// How often the watchdog looks for a trip or an expiry.
const WATCH_EVERY: std::time::Duration = std::time::Duration::from_millis(100);

/// Why every held key must go now, if it must.
fn release_reason(halted: bool, taken_over: bool) -> Option<&'static str> {
    match (halted, taken_over) {
        (true, _) => Some("the kill switch is engaged"),
        (_, true) => Some("a human took over the pointer"),
        _ => None,
    }
}

/// Release `keys`, last pressed first, trying every one even if some fail.
/// Returns the first failure.
async fn release_keys(backend: &dyn InputBackend, keys: &[String]) -> Option<InputError> {
    let mut first = None;
    for k in keys.iter().rev() {
        if let Err(e) = backend.key_up(k).await {
            tracing::error!(key = %k, error = ?e, "could not release a held key");
            first.get_or_insert(e);
        }
    }
    first
}

fn lock_ledger(l: &Mutex<Ledger>) -> std::sync::MutexGuard<'_, Ledger> {
    l.lock().unwrap_or_else(|e| e.into_inner())
}

impl InputModule {
    pub fn new(
        backend: Arc<dyn InputBackend>,
        arena: Arc<Mutex<SnapshotArena>>,
        policy: InputPolicy,
    ) -> Self {
        InputModule {
            backend,
            arena,
            policy,
            verifier: None,
            activity: None,
            observer: None,
            glide: Mutex::new(crate::glide::GlideConfig::default()),
            ledger: Arc::new(Mutex::new(Ledger {
                keys: HeldKeys::default(),
                watching: false,
            })),
            halt: None,
            probe: None,
            relay: None,
            batch_ms: crate::steps::MAX_BATCH_MS,
            batch_loan: std::sync::atomic::AtomicBool::new(false),
            rec: crate::record_tools::RecordSupport::default(),
        }
    }

    /// Wire recording and replay of human demonstrations (composition root
    /// only): the listener, the flow store, the `allowed_apps` a recording is
    /// held to, and the marker file `agentctl doctor` reads.
    pub fn with_recording(mut self, rec: crate::record_tools::RecordSupport) -> Self {
        self.rec = rec;
        self
    }

    /// Shorten the batch's time, so a test need not wait 30 s.
    #[cfg(test)]
    fn with_batch_limit(mut self, ms: u64) -> Self {
        self.batch_ms = ms;
        self
    }

    /// Attach the reader of the facts behind `effects` (composition root only).
    pub fn with_probe(mut self, p: Arc<dyn crate::effects::UiProbe>) -> Self {
        self.probe = Some(p);
        self
    }

    /// Attach the handle that carries `input_steps` back through the server's
    /// gate (composition root only).
    pub fn with_steps(mut self, r: Arc<crate::steps::StepRelay>) -> Self {
        self.relay = Some(r);
        self
    }

    /// Say how to tell the kill switch is engaged (composition root only), so
    /// held keys are released when it trips.
    pub fn with_halt_probe(mut self, halted: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.halt = Some(halted);
        self
    }

    /// Shorten how long a key may stay held, so a test need not wait 30 s.
    #[cfg(test)]
    fn with_hold_limit(self, ms: u64) -> Self {
        lock_ledger(&self.ledger).keys = HeldKeys::with_ttl(ms);
        self
    }

    /// Attach the driving-activity marker (composition root only).
    pub fn with_activity(mut self, a: std::sync::Arc<crate::Activity>) -> Self {
        self.activity = Some(a);
        self
    }

    /// Attach the source of fresh snapshots (composition root only).
    pub fn with_observer(mut self, o: Arc<dyn A11yBackend>) -> Self {
        self.observer = Some(o);
        self
    }

    /// Attach the postcondition verifier (composition root only).
    pub fn with_verifier(mut self, v: std::sync::Arc<crate::postcondition::Verifier>) -> Self {
        self.verifier = Some(v);
        self
    }

    /// Configure smooth cursor gliding for demos/recordings.
    pub fn with_glide(mut self, g: crate::glide::GlideConfig) -> Self {
        self.glide = Mutex::new(g);
        self
    }

    // ---- shared helpers -----------------------------------------------------

    /// Resolve a target spec to an element: `ref` (any spelling) or `name`
    /// with an optional `role`.
    ///
    /// A name is matched against the latest snapshot, best first (exact, then
    /// prefix, then substring; controls before other elements; then document
    /// order), and a snapshot is taken if there is none. `need` says what the
    /// caller will do with the element, so a candidate that cannot be used for
    /// it is never the one chosen.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope, short-lived
    async fn resolve(&self, tool: &str, spec: &Value, need: Need) -> Result<Target, Envelope> {
        if let Some(raw) = spec.get("ref").filter(|v| !v.is_null()) {
            let Some(reff) = parse_ref(raw) else {
                return Err(Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    "ref must look like @e12 (e12 and 12 also work)",
                ));
            };
            return self.target_by_ref(tool, &reff, need);
        }
        let name = spec
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty());
        let Some(name) = name else {
            return Err(Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "needs a target: 'ref' (@e12) or 'name' (with an optional 'role')",
            ));
        };
        let role = spec
            .get("role")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|r| !r.is_empty());
        let have_snapshot = self
            .arena
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current()
            .is_some();
        if !have_snapshot {
            self.observe(tool).await?;
        }
        self.target_by_name(tool, name, role, need)
    }

    #[allow(clippy::result_large_err)]
    fn target_by_ref(&self, tool: &str, reff: &str, need: Need) -> Result<Target, Envelope> {
        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        match arena.resolve_latest(reff) {
            Ok(info) => Target::from_info(tool, reff, info, need, None).map(|mut t| {
                t.app = arena.current().and_then(|s| s.app.clone());
                t
            }),
            Err(RefError::Stale) => Err(Envelope::fail_with(
                tool,
                ErrorCode::StaleRef,
                "no current snapshot for this ref",
                "call get_ui_tree first",
            )),
            Err(RefError::NotFound) => Err(Envelope::fail(
                tool,
                ErrorCode::NotFound,
                "no such element in the latest snapshot",
            )),
        }
    }

    #[allow(clippy::result_large_err)]
    fn target_by_name(
        &self,
        tool: &str,
        name: &str,
        role: Option<&str>,
        need: Need,
    ) -> Result<Target, Envelope> {
        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        let snap = arena.current().ok_or_else(|| {
            Envelope::fail_with(
                tool,
                ErrorCode::StaleRef,
                "no current snapshot to resolve elements by name",
                "call get_ui_tree first",
            )
        })?;
        let usable = |i: &ElementInfo| need.usable(i);
        let ranked = rank_by_name(snap, name, role, &usable);
        let Some(best) = ranked.first() else {
            let close = near_misses(snap, name, &usable, MAX_CANDIDATES);
            let what = match role {
                Some(r) => format!("no {r} matching name '{name}' in the current snapshot"),
                None => format!("no element matching name '{name}' in the current snapshot"),
            };
            // A sparse tree may just not hold the text, so say where to look
            // instead. Words the advice only: the model makes the OCR call.
            let ocr = mcp_a11y::ocr_fallback_hint(snap, name)
                .map(|h| format!(". {h}"))
                .unwrap_or_default();
            return Err(if close.is_empty() {
                Envelope::fail_with(
                    tool,
                    ErrorCode::NotFound,
                    what,
                    format!("call find_elements or get_ui_tree to see what is on screen{ocr}"),
                )
            } else {
                let lines: Vec<String> = close.iter().map(Candidate::line).collect();
                Envelope::fail_with_data(
                    tool,
                    ErrorCode::NotFound,
                    what,
                    format!("closest: {}{ocr}", lines.join("; ")),
                    json!({ "candidates": close }),
                )
            });
        };
        Target::from_info(
            tool,
            best.reff,
            best.info,
            need,
            Some(tied_with_best(&ranked)),
        )
        .map(|mut t| {
            t.app = snap.app.clone();
            t
        })
    }

    /// Take a fresh snapshot of the target application and install it, for a
    /// call that names an element before anything has been observed.
    #[allow(clippy::result_large_err)]
    async fn observe(&self, tool: &str) -> Result<(), Envelope> {
        let Some(observer) = &self.observer else {
            return Err(Envelope::fail_with(
                tool,
                ErrorCode::StaleRef,
                "no current snapshot to resolve elements by name",
                "call get_ui_tree first",
            ));
        };
        let raw = observer
            .snapshot(&SnapshotRequest::default())
            .await
            .map_err(|e| {
                let (code, msg) = match e {
                    BackendError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
                    BackendError::NotFound(m) => (ErrorCode::NotFound, m),
                    BackendError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
                    BackendError::Failed(m) => (ErrorCode::ActionFailed, m),
                };
                Envelope::fail(tool, code, msg)
            })?;
        let mut arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        let sid = arena.next_id();
        // No character budget: the text is never returned, and truncating the
        // element map would drop the very element being looked for.
        let cfg = FlattenConfig {
            max_chars: usize::MAX,
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
        arena.install(f.snapshot);
        Ok(())
    }

    fn current_app(&self) -> Option<String> {
        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        arena.current().and_then(|s| s.app.clone())
    }

    /// The union of all element bounds in the current snapshot (the coordinate
    /// region we consider "inside the observed app").
    fn allowed_region(&self) -> Option<(f64, f64, f64, f64)> {
        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        let snap = arena.current()?;
        let mut iter = snap.elements.values().filter_map(|e| e.bounds);
        let first = iter.next()?;
        let mut region = (first.x, first.y, first.x + first.w, first.y + first.h);
        for b in iter {
            region.0 = region.0.min(b.x);
            region.1 = region.1.min(b.y);
            region.2 = region.2.max(b.x + b.w);
            region.3 = region.3.max(b.y + b.h);
        }
        Some(region)
    }

    /// Deny a coordinate outside the allowed region when clamping is on. If no
    /// bounds are known, clamping can't be enforced (allowed, logged).
    fn clamp_check(&self, tool: &str, x: f64, y: f64) -> Option<Envelope> {
        if !self.policy.clamp_input {
            return None;
        }
        match self.allowed_region() {
            Some((minx, miny, maxx, maxy)) => {
                if x < minx || y < miny || x > maxx || y > maxy {
                    Some(Envelope::fail(
                        tool,
                        ErrorCode::PolicyDenied,
                        "coordinate outside allowlisted window bounds (clamp_input_to_allowed)",
                    ))
                } else {
                    None
                }
            }
            None => {
                tracing::debug!("clamp on but no element bounds in snapshot; allowing");
                None
            }
        }
    }

    /// Deny/consent destructive text typed into a terminal app.
    ///
    /// The target is whatever will actually receive the keystrokes, not the app
    /// in the last snapshot. Asking the snapshot lets a destructive command
    /// straight through whenever the two disagree, and they always disagree for
    /// an app with no accessibility tree, where the snapshot is empty and the
    /// check silently evaluates to "not a terminal".
    ///
    /// When the target cannot be determined at all, destructive text is treated
    /// as if it *were* headed for a terminal. Unknown destination is not
    /// evidence of safety.
    async fn destructive_check(&self, tool: &str, text: &str, secret: bool) -> Option<Envelope> {
        if self.policy.bypass {
            return None;
        }
        // On the per-process route the text goes to the pinned app whether or
        // not it is in front, so that app is the one screened. It is asked
        // for by name here rather than left to `input_target`, so the gate
        // does not depend on a backend answering both questions the same way.
        let target = self
            .backend
            .background_key_target()
            .map(|b| b.name)
            .or_else(|| self.backend.input_target())
            .or_else(|| self.current_app());
        let is_term = screens_as_terminal(target.as_deref(), &self.policy.terminal_apps);
        if !is_term {
            return None;
        }
        // A secret (a password the operator handed us to type) must never be
        // sent to the remote judge; the offline pattern check still runs.
        let judge = if secret {
            None
        } else {
            self.policy.judge.as_ref()
        };
        let verdict = mcp_policy::judged_destructive(
            text,
            &self.policy.destructive_patterns,
            judge,
            &format!(
                "a terminal ({})",
                target.as_deref().unwrap_or("unknown application")
            ),
        )
        .await;
        if verdict.is_destructive() {
            let reason = verdict.reason();
            return Some(if self.policy.autonomous {
                Envelope::fail(
                    tool,
                    ErrorCode::PolicyDenied,
                    format!("destructive command blocked (autonomous mode): {reason}"),
                )
            } else {
                Envelope::fail_with(
                    tool,
                    ErrorCode::ConsentRequired,
                    format!("destructive command requires human consent: {reason}"),
                    "confirm interactively or run a non-destructive command",
                )
            });
        }
        None
    }

    /// Resolve a point spec to screen coordinates, and the element it was
    /// aimed at if any.
    ///
    /// `{ref}` or `{name}` aims at an element: at its centre, or, when `x`/`y`
    /// are given, that many points from its top-left corner (so the same
    /// numbers work whether the window is moved or not). Without a target,
    /// `x` and `y` are absolute screen coordinates.
    ///
    /// The point is not clamped here; every caller runs it through
    /// [`Self::clamp_check`], so what policy sees is what the pointer gets.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope, short-lived
    async fn parse_point(
        &self,
        tool: &str,
        spec: &Value,
    ) -> Result<((f64, f64), Option<Target>), Envelope> {
        let x = number_arg(tool, spec, "x")?;
        let y = number_arg(tool, spec, "y")?;
        let aimed = spec.get("ref").is_some_and(|v| !v.is_null())
            || spec
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| !n.trim().is_empty());
        if aimed {
            let target = self.resolve(tool, spec, Need::Bounds).await?;
            let b = target.bounds.ok_or_else(|| {
                Envelope::fail(tool, ErrorCode::ActionFailed, "element has no bounds")
            })?;
            let at = (b.x + x.unwrap_or(b.w / 2.0), b.y + y.unwrap_or(b.h / 2.0));
            return Ok((at, Some(target)));
        }
        match (x, y) {
            (Some(x), Some(y)) => Ok(((x, y), None)),
            _ => Err(Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "expected 'ref' or 'name' (x/y then offset from its top-left), or both x and y",
            )),
        }
    }

    /// Read an element from the OS until `done` accepts it or the wait runs
    /// out, returning the last reading.
    ///
    /// Apps apply a change on their own run loop, so the first read after an
    /// action is often the old state. A backend that cannot read at all is not
    /// polled: nothing it says later will be different.
    async fn settle(&self, node_id: u64, done: impl Fn(&Reading) -> bool) -> Option<Reading> {
        let mut last = None;
        for attempt in 0..SETTLE_READS {
            let Ok(r) = self.backend.read_element(node_id).await else {
                break;
            };
            let unreadable = r.value.is_none() && r.checked.is_none();
            let finished = done(&r);
            last = Some(r);
            if finished || unreadable {
                break;
            }
            if attempt + 1 < SETTLE_READS {
                tokio::time::sleep(SETTLE_GAP).await;
            }
        }
        last
    }

    /// Choose an option from a popup or combo box, or from the popup an entry
    /// belongs to, and say what the control shows now.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope
    async fn choose(
        &self,
        tool: &str,
        target: &Target,
        node_id: u64,
        option: Option<&str>,
    ) -> Result<Value, Envelope> {
        // An entry with no option named means "this entry": an option element
        // resolves to the control that owns it, the way an HTML option does
        // to its select.
        let entry = option.is_none() && is_entry_role(&target.role);
        let wanted = match option {
            Some(o) => o.to_string(),
            None if entry => target.name.clone().unwrap_or_default(),
            None => {
                let route = self
                    .backend
                    .perform(node_id, SemanticAction::Select, None)
                    .await
                    .map_err(|e| input_err(tool, e))?;
                return Ok(with_route(json!({ "ok": true }), route));
            }
        };
        match self.backend.choose_option(node_id, &wanted).await {
            // Choosing walks the control's own popup through the
            // accessibility API on every backend; it has no pointer path.
            Ok(c) => Ok(with_route(
                json!({
                    "ok": true,
                    "selected": c.selected.clone().unwrap_or_else(|| c.item.clone()),
                    "changed": c.changed,
                    "verified": c.selected.is_some(),
                }),
                Route::Ax,
            )),
            // A menu item in a menu bar has no popup to resolve to; pressing
            // it is the whole job, as it always was.
            Err(InputError::Unsupported(_)) if entry => {
                let route = self
                    .backend
                    .perform(node_id, SemanticAction::Select, None)
                    .await
                    .map_err(|e| input_err(tool, e))?;
                Ok(with_route(json!({ "ok": true }), route))
            }
            Err(e) => Err(input_err(tool, e)),
        }
    }

    /// Check, uncheck or toggle, reading the state before (so asking for the
    /// state it already has changes nothing) and after (so a press that did
    /// nothing is an error, not an `ok`).
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope
    async fn set_checked(
        &self,
        tool: &str,
        node_id: u64,
        action: SemanticAction,
    ) -> Result<Value, Envelope> {
        let before = self
            .backend
            .read_element(node_id)
            .await
            .ok()
            .and_then(|r| r.checked);
        let want = match action {
            SemanticAction::Check => Some(true),
            SemanticAction::Uncheck => Some(false),
            _ => before.map(|b| !b),
        };
        if action != SemanticAction::Toggle && before.is_some() && before == want {
            return Ok(json!({ "ok": true, "checked": before, "changed": false }));
        }
        let route = self
            .backend
            .perform(node_id, action, None)
            .await
            .map_err(|e| input_err(tool, e))?;
        let after = self
            .settle(node_id, |r| match want {
                Some(w) => r.checked == Some(w),
                None => r.checked.is_some() && r.checked != before,
            })
            .await
            .and_then(|r| r.checked);
        if let (Some(a), Some(w)) = (after, want) {
            if a != w {
                return Err(Envelope::fail_with(
                    tool,
                    ErrorCode::ActionFailed,
                    format!(
                        "pressed it, but it is still {}",
                        if a { "checked" } else { "unchecked" }
                    ),
                    "the control may be disabled or may need a different action; try click",
                ));
            }
        }
        Ok(with_route(
            json!({
                "ok": true,
                "checked": after,
                "changed": before.zip(after).map(|(b, a)| b != a),
            }),
            route,
        ))
    }

    /// One step up or down on a slider, stepper or scroll bar, reading the
    /// value before and after. A step that leaves the value alone is reported
    /// as `changed: false` (the control is at its limit, or ignored the
    /// request), never as a change.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope
    async fn step(
        &self,
        tool: &str,
        node_id: u64,
        action: SemanticAction,
    ) -> Result<Value, Envelope> {
        let before = self
            .backend
            .read_element(node_id)
            .await
            .ok()
            .and_then(|r| r.value);
        let route = self
            .backend
            .perform(node_id, action, None)
            .await
            .map_err(|e| input_err(tool, e))?;
        let after = self
            .settle(node_id, |r| r.value != before)
            .await
            .and_then(|r| r.value);
        let mut data = with_route(json!({ "ok": true }), route);
        match after {
            Some(a) => {
                data["changed"] = json!(before.as_deref() != Some(a.as_str()));
                data["value_after"] = json!(a);
            }
            None => data["verified"] = json!(false),
        }
        Ok(data)
    }

    /// One semantic action on a resolved element, with the read-backs the
    /// action calls for.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope
    async fn act(
        &self,
        tool: &str,
        target: &Target,
        action: SemanticAction,
        option: Option<&str>,
    ) -> Result<Value, Envelope> {
        let node_id = target.node_id.ok_or_else(|| {
            Envelope::fail(
                tool,
                ErrorCode::ActionFailed,
                "element is not actionable (no backend handle)",
            )
        })?;
        match action {
            SemanticAction::Select => self.choose(tool, target, node_id, option).await,
            SemanticAction::Check | SemanticAction::Uncheck | SemanticAction::Toggle => {
                self.set_checked(tool, node_id, action).await
            }
            SemanticAction::Increment | SemanticAction::Decrement => {
                self.step(tool, node_id, action).await
            }
            _ => {
                let route = self
                    .backend
                    .perform(node_id, action, option)
                    .await
                    .map_err(|e| input_err(tool, e))?;
                Ok(with_route(json!({ "ok": true }), route))
            }
        }
    }

    /// Write text into an element and read it back. The write is only `ok`
    /// if the field now shows something other than what it showed before, or
    /// exactly what was asked for.
    #[allow(clippy::result_large_err)] // Err is a ready-to-return Envelope
    async fn write_text(
        &self,
        tool: &str,
        target: &Target,
        node_id: u64,
        text: &str,
        secret: bool,
    ) -> Result<Value, Envelope> {
        let before = self
            .backend
            .read_element(node_id)
            .await
            .ok()
            .and_then(|r| r.value);
        self.backend
            .set_value(node_id, text)
            .await
            .map_err(|e| input_err(tool, e))?;
        let after = self
            .settle(node_id, |r| {
                r.value.as_deref() == Some(text) || r.value != before
            })
            .await
            .and_then(|r| r.value);
        judge_write(
            before.as_deref(),
            after.as_deref(),
            text,
            secret || target.secure,
        )
        .map(|d| with_route(d, Route::Ax))
        .map_err(|m| {
            Envelope::fail_with(
                tool,
                ErrorCode::ActionFailed,
                m,
                "the field may be read-only or may validate input; try keyboard_type",
            )
        })
    }

    // ---- effects ------------------------------------------------------------

    /// The facts to compare against, read before the action.
    async fn facts_before(&self, tool: &str, args: &Value) -> EffectState {
        let facts = match &self.probe {
            Some(p) => p.probe(None).await,
            None => crate::effects::UiFacts::default(),
        };
        let element = match self.effect_element(tool, args) {
            Some((node, secure)) => self
                .backend
                .read_element(node)
                .await
                .ok()
                .map(|reading| (node, crate::effects::ElementFacts { reading, secure })),
            None => None,
        };
        EffectState { facts, element }
    }

    /// The element a `ui_action` pressed by `ref`, for the actions that do not
    /// read their target back themselves. A lookup by `name` is left out: the
    /// element it means is only known once the action has run.
    fn effect_element(&self, tool: &str, args: &Value) -> Option<(u64, bool)> {
        if tool != "ui_action" {
            return None;
        }
        let action = SemanticAction::parse(&snake(args.get("action")?.as_str()?))?;
        let reads_itself = matches!(
            action,
            SemanticAction::Check
                | SemanticAction::Uncheck
                | SemanticAction::Toggle
                | SemanticAction::Increment
                | SemanticAction::Decrement
                | SemanticAction::Select
        );
        if reads_itself {
            return None;
        }
        let reff = parse_ref(args.get("ref").filter(|v| !v.is_null())?)?;
        let arena = self.arena.lock().unwrap_or_else(|e| e.into_inner());
        let info = arena.resolve_latest(&reff).ok()?;
        Some((info.node_id?, info.secure))
    }

    /// Wait for the app to react, read the facts again and add what differs
    /// to the result as `effects`. Adds nothing when nothing differs.
    async fn attach_effects(&self, env: &mut Envelope, before: EffectState) {
        let Some(probe) = &self.probe else { return };
        tokio::time::sleep(std::time::Duration::from_millis(crate::effects::SETTLE_MS)).await;
        let known = before.facts.overlays.clone().unwrap_or_default();
        let after = probe.probe(Some(&known)).await;
        let element_after = match &before.element {
            Some((node, was)) => self.backend.read_element(*node).await.ok().map(|reading| {
                crate::effects::ElementFacts {
                    reading,
                    secure: was.secure,
                }
            }),
            None => None,
        };
        let element = match (&before.element, &element_after) {
            (Some((_, b)), Some(a)) => Some((b, a)),
            _ => None,
        };
        let Some(fx) = crate::effects::diff_facts(&before.facts, &after, element) else {
            return;
        };
        if let Some(Value::Object(d)) = env.data.as_mut() {
            d.insert("effects".into(), fx);
        }
    }

    // ---- input_steps --------------------------------------------------------

    /// Run a batch. The steps are validated first, all of them, and then each
    /// goes back through the server's gate (see [`crate::steps`]); nothing here
    /// runs a step directly.
    async fn input_steps(&self, args: &Value, ctx: &CallCtx) -> Envelope {
        use crate::steps::parse_steps;
        let tool = "input_steps";
        let own = self.descriptors();
        let steps = match parse_steps(
            args,
            &|t| own.iter().find(|d| d.name == t).map(|d| d.tier),
            BATCH_TIER,
        ) {
            Ok(s) => s,
            Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        };
        let Some(dispatch) = self.relay.as_ref().and_then(|r| r.get()) else {
            return Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "input_steps is not connected to this server's gate, so it will not run steps",
                "call the tools one at a time",
            );
        };
        let since = self.backend.takeover_generation();
        let started = std::time::Instant::now();
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(self.batch_ms);
        let mut rows: Vec<Value> = Vec::with_capacity(steps.len());
        let mut ran = 0usize;
        let mut failed: Option<(usize, ErrorCode, String, Option<String>)> = None;

        for (i, step) in steps.iter().enumerate() {
            // Between steps the batch asks, before the gate does, whether it
            // should go on at all.
            let halted = self.halt.as_ref().is_some_and(|h| h());
            let why = if halted {
                Some((ErrorCode::Timeout, "the kill switch is engaged"))
            } else if self.backend.takeover_generation() != since {
                Some((ErrorCode::Timeout, "a human took over the pointer"))
            } else if ctx.cancel.is_cancelled() {
                Some((ErrorCode::Timeout, "the call was cancelled"))
            } else if tokio::time::Instant::now() >= deadline {
                Some((
                    ErrorCode::Timeout,
                    "the batch ran out of time before this step",
                ))
            } else {
                None
            };
            if let Some((code, why)) = why {
                rows.push(json!({
                    "i": i, "tool": step.tool, "ok": false, "skipped": true,
                    "error": { "code": code, "message": why },
                }));
                failed = Some((
                    i,
                    code,
                    format!("stopped before step {i} ({}): {why}", step.tool),
                    Some(format!("steps from {i} on did not run; send them again when it is safe to continue")),
                ));
                break;
            }

            let env = watch(&*dispatch, step, ctx, deadline).await;
            ran += 1;
            let mut row = serde_json::Map::new();
            row.insert("i".into(), json!(i));
            row.insert("tool".into(), json!(step.tool));
            if let Some(Value::Object(d)) = &env.data {
                row.extend(d.clone());
            }
            row.insert("ok".into(), json!(env.ok));
            if let Some(e) = &env.error {
                row.insert(
                    "error".into(),
                    json!({ "code": e.code, "message": e.message }),
                );
            }
            rows.push(Value::Object(row));
            if !env.ok {
                let (code, msg, hint) = env
                    .error
                    .map_or((ErrorCode::ActionFailed, String::new(), None), |e| {
                        (e.code, e.message, e.suggestion)
                    });
                failed = Some((
                    i,
                    code,
                    format!("step {i} ({}) failed: {msg}", step.tool),
                    hint,
                ));
                break;
            }
        }

        let mut data = serde_json::Map::new();
        data.insert("ran".into(), json!(ran));
        data.insert("total".into(), json!(steps.len()));
        if let Some((i, ..)) = &failed {
            data.insert("failed_at".into(), json!(i));
        }
        data.insert(
            "elapsed_ms".into(),
            json!(started.elapsed().as_millis() as u64),
        );
        data.insert("steps".into(), Value::Array(rows));
        match failed {
            None => Envelope::ok(tool, Value::Object(data)),
            Some((i, code, msg, hint)) => Envelope::fail_with_data(
                tool,
                code,
                msg,
                hint.unwrap_or_else(|| {
                    format!("steps before {i} ran; fix step {i} and send it and the rest again")
                }),
                Value::Object(data),
            ),
        }
    }

    // ---- tools --------------------------------------------------------------

    async fn ui_action(&self, args: &Value) -> Envelope {
        let tool = "ui_action";
        let Some(action) = args
            .get("action")
            .and_then(Value::as_str)
            .map(snake)
            .and_then(|a| SemanticAction::parse(&a))
        else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing or invalid 'action'");
        };
        let option = args
            .get("option")
            .and_then(Value::as_str)
            .filter(|o| !o.is_empty());
        let target = match self.resolve(tool, args, Need::Handle).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        match self.act(tool, &target, action, option).await {
            Ok(data) => Envelope::ok(tool, target.annotate(data)),
            Err(e) => e,
        }
    }

    async fn set_value(&self, args: &Value) -> Envelope {
        let tool = "set_value";
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'text'");
        };
        let secret = args.get("secret").and_then(Value::as_bool).unwrap_or(false);
        if let Some(deny) = self.destructive_check(tool, text, secret).await {
            return deny;
        }
        let target = match self.resolve(tool, args, Need::Handle).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        let node_id = target.node_id.unwrap_or_default();
        match self.write_text(tool, &target, node_id, text, secret).await {
            Ok(data) => Envelope::ok(tool, target.annotate(data)),
            Err(e) => e,
        }
    }

    /// Select text in an element through the accessibility API and read the
    /// selection back. The text is read first, so a request made as text or
    /// as offsets means the same characters on every platform, and the range
    /// the element reports afterwards must equal the range asked for.
    async fn select_text(&self, args: &Value) -> Envelope {
        let tool = "select_text";
        let spec = match parse_range(args) {
            Ok(s) => s,
            Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        };
        let target = match self.resolve(tool, args, Need::Handle).await {
            Ok(t) => t,
            Err(e) => return e,
        };
        if target.secure {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "this is a password field; its text is never read or selected",
            );
        }
        let node_id = target.node_id.unwrap_or_default();
        let text = match self.backend.read_element(node_id).await {
            Ok(Reading { value: Some(t), .. }) => t,
            Ok(_) => {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::ActionFailed,
                    "the element's text cannot be read, so a range in it cannot be resolved",
                    "target a text field or text area",
                )
            }
            Err(e) => return input_err(tool, e),
        };
        let (start, end) = match resolve(&text, &spec) {
            Ok(r) => r,
            Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        };
        let got = match self.backend.select_range(node_id, start, end).await {
            Ok(g) => g,
            Err(e) => return input_err(tool, e),
        };
        if let Err(m) = judge_selection((start, end), &got) {
            return Envelope::fail_with(
                tool,
                ErrorCode::ActionFailed,
                m,
                "the element may snap selections to words or lines, or ignore the request",
            );
        }
        Envelope::ok(tool, target.annotate(selection_data(&got)))
    }

    /// The text selected in an element, or in the focused one, unchanged.
    async fn get_selection(&self, args: &Value) -> Envelope {
        let tool = "get_selection";
        let aimed = args.get("ref").is_some_and(|v| !v.is_null())
            || args
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| !n.trim().is_empty());
        let target = if aimed {
            match self.resolve(tool, args, Need::Handle).await {
                Ok(t) => Some(t),
                Err(e) => return e,
            }
        } else {
            None
        };
        if target.as_ref().is_some_and(|t| t.secure) {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "this is a password field; its text is never read",
            );
        }
        let node = target.as_ref().map(|t| t.node_id.unwrap_or_default());
        match self.backend.selection(node).await {
            Ok(sel) => {
                let data = selection_data(&sel);
                Envelope::ok(
                    tool,
                    match target {
                        Some(t) => t.annotate(data),
                        None => data,
                    },
                )
            }
            Err(e) => input_err(tool, e),
        }
    }

    async fn ui_fill_form(&self, args: &Value) -> Envelope {
        let tool = "ui_fill_form";
        let Some(fields_arr) = args.get("fields").and_then(Value::as_array) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'fields' array");
        };
        if fields_arr.is_empty() {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'fields' array must not be empty",
            );
        }

        let mut results: Vec<Value> = Vec::with_capacity(fields_arr.len());

        for field in fields_arr {
            // What went wrong, with what had been done by then: a form half
            // filled is a state the caller needs to see.
            let stop = |filled: usize, results: &[Value], err: Envelope| Envelope {
                ok: false,
                tool: tool.into(),
                data: Some(merge(
                    err.data.clone().unwrap_or_else(|| json!({})),
                    json!({
                        "filled": filled,
                        "results": results,
                        "error_target": field.get("ref").or_else(|| field.get("name")),
                    }),
                )),
                error: err.error,
                image: None,
            };

            let target = match self.resolve(tool, field, Need::Handle).await {
                Ok(t) => t,
                Err(e) => return stop(results.len(), &results, e),
            };
            let node_id = target.node_id.unwrap_or_default();
            let secret = field
                .get("secret")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let action_str = field
                .get("action")
                .and_then(Value::as_str)
                .map(snake)
                .filter(|a| !a.is_empty());
            let option = field
                .get("option")
                .and_then(Value::as_str)
                .filter(|o| !o.is_empty());

            // (action label, value shown in the result, outcome)
            let done: Result<(String, Option<Value>, Value), Envelope> = if let Some(val) =
                field.get("value")
            {
                if let Some(b) = val.as_bool() {
                    let act = match action_str.as_deref() {
                        Some("toggle") => SemanticAction::Toggle,
                        Some("click") => SemanticAction::Click,
                        _ if b => SemanticAction::Check,
                        _ => SemanticAction::Uncheck,
                    };
                    self.act(tool, &target, act, None)
                        .await
                        .map(|d| (format!("{act:?}").to_lowercase(), Some(json!(b)), d))
                } else {
                    let text = val
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| val.to_string());
                    match action_str.as_deref() {
                        Some("select") => {
                            let opt = option.unwrap_or(&text);
                            self.act(tool, &target, SemanticAction::Select, Some(opt))
                                .await
                                .map(|d| {
                                    (
                                        "select".to_string(),
                                        None,
                                        merge(d, json!({ "option": opt })),
                                    )
                                })
                        }
                        Some(act) if SemanticAction::parse(act).is_some() => {
                            let sem = SemanticAction::parse(act).unwrap_or(SemanticAction::Click);
                            self.act(tool, &target, sem, None)
                                .await
                                .map(|d| (act.to_string(), None, d))
                        }
                        _ => {
                            if let Some(deny) = self.destructive_check(tool, &text, secret).await {
                                return stop(results.len(), &results, deny);
                            }
                            let shown = if secret {
                                format!("[REDACTED:len={}]", text.len())
                            } else {
                                text.clone()
                            };
                            self.write_text(tool, &target, node_id, &text, secret)
                                .await
                                .map(|d| ("set_value".to_string(), Some(json!(shown)), d))
                        }
                    }
                }
            } else if let Some(act) = action_str.as_deref() {
                let sem = SemanticAction::parse(act).unwrap_or(SemanticAction::Click);
                self.act(tool, &target, sem, option)
                    .await
                    .map(|d| (act.to_string(), None, d))
            } else {
                return stop(
                    results.len(),
                    &results,
                    Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        "field must provide 'value' or 'action'",
                    ),
                );
            };

            match done {
                Ok((label, value, mut data)) => {
                    // The write path reports what the field now shows under
                    // `value_after`; `value` stays what was asked for.
                    data = target.annotate(data);
                    let mut entry = json!({
                        "target": target.reff,
                        "action": label,
                        "status": "ok",
                    });
                    if let Some(v) = value {
                        entry["value"] = v;
                    }
                    if let Some(obj) = data.as_object_mut() {
                        obj.remove("ok");
                    }
                    results.push(merge(entry, data));
                }
                Err(e) => return stop(results.len(), &results, e),
            }
        }
        let filled = results.len();

        let submitted = if let Some(submit_spec) = args.get("submit") {
            let target = match self.resolve(tool, submit_spec, Need::Handle).await {
                Ok(t) => t,
                Err(e) => return e,
            };
            let act = submit_spec
                .get("action")
                .and_then(Value::as_str)
                .map(snake)
                .and_then(|a| SemanticAction::parse(&a))
                .unwrap_or(SemanticAction::Click);
            if let Err(e) = self.act(tool, &target, act, None).await {
                return e;
            }
            true
        } else {
            false
        };

        Envelope::ok(
            tool,
            json!({
                "filled": filled,
                "submitted": submitted,
                "results": results
            }),
        )
    }

    async fn keyboard_type(&self, args: &Value) -> Envelope {
        let tool = "keyboard_type";
        let Some(text) = args.get("text").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'text'");
        };
        let secret = args.get("secret").and_then(Value::as_bool).unwrap_or(false);
        if let Some(deny) = self.destructive_check(tool, text, secret).await {
            return deny;
        }
        let aimed = args.get("ref").is_some_and(|v| !v.is_null())
            || args
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| !n.trim().is_empty());
        if let Some(bg) = self.backend.background_key_target() {
            return self.type_to_background(args, text, secret, aimed, bg).await;
        }
        let mut target = None;
        let mut before = None;
        if aimed {
            let t = match self.resolve(tool, args, Need::Handle).await {
                Ok(t) => t,
                Err(e) => return e,
            };
            let node_id = t.node_id.unwrap_or_default();
            if let Err(e) = self
                .backend
                .perform(node_id, SemanticAction::Focus, None)
                .await
            {
                return input_err(tool, e);
            }
            before = self
                .backend
                .read_element(node_id)
                .await
                .ok()
                .and_then(|r| r.value);
            target = Some(t);
        }
        if let Err(e) = self.backend.type_text(text).await {
            return input_err(tool, e);
        }
        let typed = text.chars().count();
        let Some(t) = target else {
            return Envelope::ok(
                tool,
                with_route(json!({ "ok": true, "typed": typed }), Route::Hid),
            );
        };
        let hide = secret || t.secure;
        let after = if typed == 0 {
            None
        } else {
            self.settle(t.node_id.unwrap_or_default(), |r| r.value != before)
                .await
                .and_then(|r| r.value)
        };
        // The element was focused through the accessibility API, but the
        // characters are posted as system key events, so the app must be
        // frontmost: this is the HID route.
        let mut data = with_route(json!({ "ok": true, "typed": typed }), Route::Hid);
        if let Some(a) = &after {
            let changed = before.as_deref() != Some(a.as_str());
            // Typing that left a text field exactly as it was did not land:
            // it was refused, or went to some other control.
            if !changed && is_text_role(&t.role) {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::ActionFailed,
                    format!(
                        "typed {typed} characters but the field's value did not change{}",
                        if hide {
                            String::new()
                        } else {
                            format!(" (still {a:?})")
                        }
                    ),
                    "the field may be read-only or full, or focus may have moved; try set_value",
                );
            }
            data["changed"] = json!(changed);
            if !hide {
                data["value_after"] = json!(a);
            }
        }
        Envelope::ok(tool, t.annotate(data))
    }

    async fn keyboard_shortcut(&self, args: &Value, since: u64) -> Envelope {
        let tool = "keyboard_shortcut";
        match shortcut_action(args) {
            Ok(HoldAction::Press) => {}
            Ok(HoldAction::Down) => return self.keys_down(args, since).await,
            Ok(HoldAction::Up) => return self.keys_up(args).await,
            Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        }
        let Some(combo) = args.get("combo").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'combo'");
        };
        let os = Os::from_platform(self.backend.platform());
        let parsed = match parse_combo(combo, os) {
            Ok(c) => c.canonical(),
            Err(msg) => {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::InvalidArgs,
                    msg,
                    "e.g. cmd+shift+n, Control+A, Return, F5, cmd+plus",
                )
            }
        };
        if let Some(bg) = self.backend.background_key_target() {
            return self.press_in_background(&parsed, bg).await;
        }
        match self.backend.key_combo(&parsed).await {
            Ok(()) => Envelope::ok(
                tool,
                with_route(json!({ "ok": true, "pressed": parsed }), Route::Hid),
            ),
            Err(e) => input_err(tool, e),
        }
    }

    /// The error for a key route that cannot be used while keys are held.
    /// Held modifiers are applied to the events agentctl posts to the system;
    /// a per-process event carries none of them, and a route that silently
    /// dropped Shift would type the wrong thing.
    fn held_blocks_pid(&self, tool: &str, bg: &BackgroundTarget) -> Option<Envelope> {
        let held = lock_ledger(&self.ledger).keys.names();
        if held.is_empty() {
            return None;
        }
        Some(Envelope::fail_with(
            tool,
            ErrorCode::ActionFailed,
            format!(
                "keys are held down ({}) and '{}' is in the background: held keys are applied \
                 only to events sent to the frontmost app, so sending to '{}' directly would \
                 drop them",
                held.join(", "),
                bg.name,
                bg.name
            ),
            "release them with keyboard_shortcut action up, or bring the app forward with \
             focus_app (activate true)",
        ))
    }

    /// Wait for the focused element of `pid` to read differently from
    /// `before`, up to the settle window, and return the last read.
    async fn focused_after(&self, pid: i32, before: &FocusedText) -> Option<FocusedText> {
        let mut last = None;
        for attempt in 0..SETTLE_READS {
            let Ok(now) = self.backend.read_focused_text(pid).await else {
                break;
            };
            let moved = crate::pid_route::changed(before, &now) != Some(false);
            last = Some(now);
            if moved {
                break;
            }
            if attempt + 1 < SETTLE_READS {
                tokio::time::sleep(SETTLE_GAP).await;
            }
        }
        last
    }

    /// `keyboard_type` while the pinned app is in the background: per-process
    /// events, read back, and an error rather than a guess when the read-back
    /// cannot show the text arrived. Never raises the app, and never falls
    /// back to the system event stream, which would type into whatever the
    /// person is using.
    async fn type_to_background(
        &self,
        args: &Value,
        text: &str,
        secret: bool,
        aimed: bool,
        bg: BackgroundTarget,
    ) -> Envelope {
        let tool = "keyboard_type";
        if let Some(e) = self.held_blocks_pid(tool, &bg) {
            return e;
        }
        let typed = text.chars().count();
        let mut target = None;
        if aimed {
            let t = match self.resolve(tool, args, Need::Handle).await {
                Ok(t) => t,
                Err(e) => return e,
            };
            // Focus through accessibility, which does not bring the app up;
            // the keys then land in the element and not wherever the app last
            // had its caret.
            if let Err(e) = self
                .backend
                .perform(t.node_id.unwrap_or_default(), SemanticAction::Focus, None)
                .await
            {
                return input_err(tool, e);
            }
            target = Some(t);
        }
        let hide = secret || target.as_ref().is_some_and(|t| t.secure);
        let before = self
            .backend
            .read_focused_text(bg.pid)
            .await
            .unwrap_or_default();
        if typed > 0 {
            if let Err(e) = self.backend.type_text_to_pid(bg.pid, text).await {
                return input_err(tool, e);
            }
        }
        let after = if typed == 0 {
            before.clone()
        } else {
            self.focused_after(bg.pid, &before)
                .await
                .unwrap_or_default()
        };
        match crate::pid_route::judge_typed(&before, &after, text) {
            crate::pid_route::Delivery::Landed => {
                let mut data = with_route(
                    json!({ "ok": true, "typed": typed, "verified": true,
                            "background_app": bg.name }),
                    Route::PidEvent,
                );
                if !hide {
                    if let Some(v) = &after.value {
                        data["value_after"] = json!(v);
                    }
                }
                match target {
                    Some(t) => Envelope::ok(tool, t.annotate(data)),
                    None => Envelope::ok(tool, data),
                }
            }
            crate::pid_route::Delivery::NotLanded => Envelope::fail_with(
                tool,
                ErrorCode::ActionFailed,
                format!(
                    "sent {typed} characters to '{}' without bringing it forward, but its focused \
                     element did not change: the app dropped them or the field refused them",
                    bg.name
                ),
                BACKGROUND_ALTERNATIVES,
            ),
            crate::pid_route::Delivery::Unverifiable => Envelope::fail_with(
                tool,
                ErrorCode::ActionFailed,
                format!(
                    "sent {typed} characters to '{}' without bringing it forward, but its focused \
                     element cannot be read back (a password field, or an app with no readable \
                     text), so it is not known whether they arrived. They may have: check the \
                     app before typing again",
                    bg.name
                ),
                BACKGROUND_ALTERNATIVES,
            ),
        }
    }

    /// `keyboard_shortcut` press while the pinned app is in the background.
    /// There is usually no read-back for a chord, so the result says
    /// `verified: false` unless the focused element visibly changed.
    async fn press_in_background(&self, combo: &str, bg: BackgroundTarget) -> Envelope {
        let tool = "keyboard_shortcut";
        if let Some(e) = self.held_blocks_pid(tool, &bg) {
            return e;
        }
        let before = self.backend.read_focused_text(bg.pid).await.ok();
        if let Err(e) = self.backend.key_combo_to_pid(bg.pid, combo).await {
            return input_err(tool, e);
        }
        let changed = match &before {
            Some(b) => self
                .focused_after(bg.pid, b)
                .await
                .and_then(|a| crate::pid_route::changed(b, &a)),
            None => None,
        };
        let verified = changed == Some(true);
        let mut data = with_route(
            json!({ "ok": true, "pressed": combo, "verified": verified,
                    "background_app": bg.name }),
            Route::PidEvent,
        );
        if !verified {
            data["note"] = json!(format!(
                "sent to '{}' without bringing it forward; the focused element did not change, \
                 and a chord has no other read-back, so it is not known that the app acted on \
                 it. Check its state before relying on it",
                bg.name
            ));
        }
        Envelope::ok(tool, data)
    }

    /// `action: "down"`: press the keys and leave them down.
    ///
    /// Each key goes in the ledger before it is pressed, so there is no moment
    /// at which a key is down and nothing knows to release it.
    async fn keys_down(&self, args: &Value, since: u64) -> Envelope {
        let tool = "keyboard_shortcut";
        let Some(combo) = args.get("combo").and_then(Value::as_str) else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "missing 'combo': the key or keys to hold",
            );
        };
        let os = Os::from_platform(self.backend.platform());
        let keys = match parse_hold(combo, os) {
            Ok(k) => k,
            Err(msg) => {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::InvalidArgs,
                    msg,
                    "e.g. shift, cmd+shift, a",
                )
            }
        };
        let (fresh, start_watchdog, limit) = {
            let mut l = lock_ledger(&self.ledger);
            if let Err(m) = l.keys.check_room(&keys) {
                return Envelope::fail(tool, ErrorCode::InvalidArgs, m);
            }
            let now = mono_ms();
            let fresh: Vec<String> = keys
                .iter()
                .filter(|k| l.keys.hold(k, now))
                .cloned()
                .collect();
            let start = !l.watching;
            l.watching = true;
            (fresh, start, l.keys.ttl_ms())
        };
        if start_watchdog {
            self.spawn_watchdog(since);
        }
        for (i, k) in fresh.iter().enumerate() {
            if let Err(e) = self.backend.key_down(k).await {
                {
                    let mut l = lock_ledger(&self.ledger);
                    for k in &fresh {
                        l.keys.release(k);
                    }
                }
                // The key that failed may be half down; release it too.
                release_keys(&*self.backend, &fresh[..=i]).await;
                return input_err(tool, e);
            }
        }
        if self.backend.takeover_generation() != since {
            let all = lock_ledger(&self.ledger).keys.take_all();
            release_keys(&*self.backend, &all).await;
            return Envelope::fail(
                tool,
                ErrorCode::ActionFailed,
                "a human took over the pointer; held keys were released",
            );
        }
        Envelope::ok(
            tool,
            json!({
                "ok": true,
                "held": lock_ledger(&self.ledger).keys.names(),
                "pressed": fresh,
                "auto_release_ms": limit,
            }),
        )
    }

    /// `action: "up"`: release the named keys, or every held key when none
    /// is named.
    async fn keys_up(&self, args: &Value) -> Envelope {
        let tool = "keyboard_shortcut";
        let named = match args.get("combo").filter(|v| !v.is_null()) {
            None => None,
            Some(c) => {
                let os = Os::from_platform(self.backend.platform());
                match c.as_str().map(|c| parse_hold(c, os)) {
                    Some(Ok(k)) => Some(k),
                    Some(Err(msg)) => {
                        return Envelope::fail_with(
                            tool,
                            ErrorCode::InvalidArgs,
                            msg,
                            "e.g. shift, cmd+shift, a; omit 'combo' to release everything held",
                        )
                    }
                    None => {
                        return Envelope::fail(
                            tool,
                            ErrorCode::InvalidArgs,
                            "'combo' must be a string",
                        )
                    }
                }
            }
        };
        let (released, not_held) = {
            let mut l = lock_ledger(&self.ledger);
            match named {
                None => (l.keys.take_all(), Vec::new()),
                Some(keys) => keys.into_iter().partition(|k| l.keys.release(k)),
            }
        };
        let failure = release_keys(&*self.backend, &released).await;
        if let Some(e) = failure {
            return input_err(tool, e);
        }
        Envelope::ok(
            tool,
            json!({
                "ok": true,
                "released": released,
                "not_held": not_held,
                "held": lock_ledger(&self.ledger).keys.names(),
            }),
        )
    }

    /// Release held keys when the kill switch trips, a human takes the
    /// pointer, or a key outlives its time limit, until nothing is held.
    ///
    /// `baseline` is the takeover generation as read when the first key's call
    /// began, so a takeover that landed since is seen.
    fn spawn_watchdog(&self, baseline: u64) {
        let backend = self.backend.clone();
        let ledger = self.ledger.clone();
        let halt = self.halt.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(WATCH_EVERY).await;
                let reason = release_reason(
                    halt.as_ref().is_some_and(|h| h()),
                    backend.takeover_generation() != baseline,
                );
                let (keys, done) = {
                    let mut l = lock_ledger(&ledger);
                    let keys = match reason {
                        Some(_) => l.keys.take_all(),
                        None => l.keys.take_expired(mono_ms()),
                    };
                    let done = l.keys.is_empty();
                    if done {
                        l.watching = false;
                    }
                    (keys, done)
                };
                if !keys.is_empty() {
                    tracing::warn!(
                        ?keys,
                        reason = reason.unwrap_or("held past the time limit"),
                        "releasing held keys"
                    );
                    release_keys(&*backend, &keys).await;
                }
                if done {
                    break;
                }
            }
        });
    }

    /// After a failed call, let go of whatever is held and say so. A held
    /// Shift that outlives the step it was for changes what the next step
    /// does, so a failure ends the hold rather than leaving it for a retry
    /// that no longer means what it did.
    async fn release_after_failure(&self, env: &mut Envelope) {
        let keys = lock_ledger(&self.ledger).keys.take_all();
        if keys.is_empty() {
            return;
        }
        let failure = release_keys(&*self.backend, &keys).await;
        if let Some(err) = env.error.as_mut() {
            err.message.push_str(&format!(
                " (held keys released: {}{})",
                keys.join(", "),
                if failure.is_some() {
                    "; a release failed"
                } else {
                    ""
                }
            ));
        }
    }

    fn effective_glide(
        &self,
        tool: &str,
        args: &Value,
    ) -> Result<crate::glide::GlideConfig, Box<Envelope>> {
        let base = self.glide.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let speed_arg = args
            .get("speed")
            .and_then(Value::as_str)
            .and_then(crate::glide::GlidePreset::from_speed);
        let preset = speed_arg.unwrap_or(base.preset);
        // An explicit `glide` always wins. Only when it is absent does a
        // `speed` argument imply the answer: a real preset turns gliding on,
        // `instant` turns it off, and no argument keeps the configured state.
        let enabled = match args.get("glide").and_then(Value::as_bool) {
            Some(explicit) => explicit,
            None => match speed_arg {
                Some(crate::glide::GlidePreset::Instant) => false,
                Some(_) => true,
                None => base.enabled,
            },
        };
        let duration_ms = match u64_arg(args, "duration_ms") {
            Some(ms) => Some(
                crate::glide::check_duration_ms(ms)
                    .map_err(|m| Box::new(Envelope::fail(tool, ErrorCode::InvalidArgs, m)))?,
            ),
            None => base.duration_ms,
        };
        let steps = u64_arg(args, "steps")
            .map(|s| s.min(u64::from(u32::MAX)) as u32)
            .or(base.steps);
        Ok(crate::glide::GlideConfig {
            enabled,
            preset,
            duration_ms,
            steps,
            curvature: base.curvature,
        })
    }

    async fn mouse_action(
        &self,
        args: &Value,
        cancel: &mcp_types::CancelToken,
        since: u64,
    ) -> Envelope {
        let tool = "mouse_action";
        let Some(kind) = args
            .get("type")
            .and_then(Value::as_str)
            .map(snake)
            .and_then(|t| MouseKind::parse(&t))
        else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing or invalid 'type'");
        };
        let ((x, y), target) = match self.parse_point(tool, args).await {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(deny) = self.clamp_check(tool, x, y) {
            return deny;
        }
        let button = args.get("button").and_then(Value::as_str);
        let modifiers = match parse_modifiers(tool, args) {
            Ok(m) => m,
            Err(e) => return e,
        };
        let glide = match self.effective_glide(tool, args) {
            Ok(g) => g,
            Err(e) => return *e,
        };
        let waypoints = match crate::glide::execute_glide(
            &*self.backend,
            (x, y),
            &glide,
            cancel,
            since,
        )
        .await
        {
            Ok(n) => n,
            Err(e) => return input_err(tool, e),
        };
        match self.backend.mouse(kind, x, y, button, &modifiers).await {
            Ok(()) => {
                let data = with_route(
                    json!({
                        "ok": true,
                        "at": { "x": x, "y": y },
                        "modifiers": modifiers,
                        "glided": waypoints > 0
                    }),
                    Route::Hid,
                );
                Envelope::ok(
                    tool,
                    match target {
                        Some(t) => t.annotate(data),
                        None => data,
                    },
                )
            }
            Err(e) => input_err(tool, e),
        }
    }

    async fn scroll(&self, args: &Value, cancel: &mcp_types::CancelToken, since: u64) -> Envelope {
        let tool = "scroll";
        let present = |k: &str| args.get(k).is_some_and(|v| !v.is_null());
        if let Some(g) = args.get("gesture").filter(|v| !v.is_null()) {
            let name = g.as_str().map(snake).unwrap_or_default();
            return match crate::trackpad::gesture_unsupported(&name) {
                Ok(m) => Envelope::fail(tool, ErrorCode::UnsupportedOs, m),
                Err(m) => Envelope::fail(tool, ErrorCode::InvalidArgs, m),
            };
        }
        if present("dx") || present("dy") {
            return self.scroll_pixels(args, cancel, since).await;
        }
        let Some(dir) = args
            .get("direction")
            .and_then(Value::as_str)
            .map(snake)
            .and_then(|d| ScrollDir::parse(&d))
        else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "missing or invalid 'direction'",
            );
        };
        let amount = i64_arg(args, "amount").unwrap_or(3).clamp(-10_000, 10_000) as i32;
        let modifiers = match parse_modifiers(tool, args) {
            Ok(m) => m,
            Err(e) => return e,
        };
        let ((x, y), target) = match self.parse_point(tool, args).await {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(deny) = self.clamp_check(tool, x, y) {
            return deny;
        }
        match self.backend.scroll_at(x, y, dir, amount, &modifiers).await {
            Ok(()) => {
                let data = with_route(
                    json!({ "ok": true, "at": { "x": x, "y": y }, "modifiers": modifiers }),
                    Route::Hid,
                );
                Envelope::ok(
                    tool,
                    match target {
                        Some(t) => t.annotate(data),
                        None => data,
                    },
                )
            }
            Err(e) => input_err(tool, e),
        }
    }

    /// `scroll` with `dx` / `dy`: a trackpad-style gesture of pixel events with
    /// phases and optional momentum. The schedule is planned first (pure, and
    /// refused if over the caps) and the events are then posted one by one at
    /// their times. A gesture left open, with no `ended`, would leave the app
    /// waiting for a finger that never lifts, so every way out of the loop
    /// after the first event, a cancel or a person taking the pointer
    /// included, posts the closing phase before it returns.
    async fn scroll_pixels(
        &self,
        args: &Value,
        cancel: &mcp_types::CancelToken,
        since: u64,
    ) -> Envelope {
        use crate::trackpad::{plan, Phase};
        let tool = "scroll";
        if args.get("direction").is_some_and(|v| !v.is_null())
            || args.get("amount").is_some_and(|v| !v.is_null())
        {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "give either direction (with amount, in wheel lines) or dx / dy (in pixels), not both",
            );
        }
        let axis = |k: &str| -> Result<i32, String> {
            match args.get(k).filter(|v| !v.is_null()) {
                None => Ok(0),
                Some(_) => i64_arg(args, k)
                    .and_then(|v| i32::try_from(v).ok())
                    .ok_or_else(|| format!("'{k}' must be a whole number of pixels")),
            }
        };
        let (dx, dy) = match (axis("dx"), axis("dy")) {
            (Ok(dx), Ok(dy)) => (dx, dy),
            (Err(m), _) | (_, Err(m)) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        };
        let duration = match args.get("duration_ms").filter(|v| !v.is_null()) {
            None => None,
            Some(_) => match u64_arg(args, "duration_ms") {
                Some(d) => Some(d),
                None => {
                    return Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        "'duration_ms' must be a whole number of milliseconds",
                    )
                }
            },
        };
        let asked_momentum = args
            .get("momentum")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let momentum = asked_momentum && self.backend.pixel_momentum();
        let plan = match plan(dx, dy, duration, momentum) {
            Ok(p) => p,
            Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        };
        let modifiers = match parse_modifiers(tool, args) {
            Ok(m) => m,
            Err(e) => return e,
        };
        let ((x, y), target) = match self.parse_point(tool, args).await {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(deny) = self.clamp_check(tool, x, y) {
            return deny;
        }
        let start = tokio::time::Instant::now();
        let mut last: Option<Phase> = None;
        let mut failure: Option<InputError> = None;
        for ev in &plan.events {
            tokio::time::sleep_until(start + std::time::Duration::from_millis(ev.at_ms)).await;
            if last.is_some() {
                if cancel.is_cancelled() {
                    failure = Some(InputError::Failed(
                        "scroll aborted: the call was cancelled".into(),
                    ));
                } else if self.backend.takeover_generation() != since {
                    failure = Some(InputError::Failed(
                        "scroll aborted: a human took over the pointer".into(),
                    ));
                }
                if failure.is_some() {
                    break;
                }
            }
            if let Err(e) = self.backend.scroll_pixels(x, y, ev, &modifiers).await {
                failure = Some(e);
                break;
            }
            last = Some(ev.phase);
        }
        if let (Some(phase), Some(_)) = (last, &failure) {
            if !matches!(phase, Phase::Ended | Phase::MomentumEnded) {
                let close = crate::trackpad::PixelScroll {
                    phase: phase.closing(),
                    dx: 0,
                    dy: 0,
                    at_ms: 0,
                };
                // Best effort: the error being reported is the first one.
                let _ = self.backend.scroll_pixels(x, y, &close, &modifiers).await;
            }
        }
        if let Some(e) = failure {
            return input_err(tool, e);
        }
        let mut data = json!({
            "ok": true,
            "at": { "x": x, "y": y },
            "modifiers": modifiers,
            "dx": plan.body_dx,
            "dy": plan.body_dy,
            "events": plan.events.len(),
            "duration_ms": plan.events.last().map_or(0, |e| e.at_ms),
            "momentum": momentum,
            "momentum_dx": plan.momentum_dx,
            "momentum_dy": plan.momentum_dy,
        });
        if asked_momentum && !momentum {
            data["note"] = json!(
                "momentum was not sent: on this platform the toolkit coasts a finished gesture itself"
            );
        }
        let data = with_route(data, Route::Hid);
        Envelope::ok(
            tool,
            match target {
                Some(t) => t.annotate(data),
                None => data,
            },
        )
    }

    async fn hover(&self, args: &Value, cancel: &mcp_types::CancelToken, since: u64) -> Envelope {
        let tool = "hover";
        let ((x, y), target) = match self.parse_point(tool, args).await {
            Ok(p) => p,
            Err(e) => return e,
        };
        if let Some(deny) = self.clamp_check(tool, x, y) {
            return deny;
        }
        let glide = match self.effective_glide(tool, args) {
            Ok(g) => g,
            Err(e) => return *e,
        };
        let waypoints = match crate::glide::execute_glide(
            &*self.backend,
            (x, y),
            &glide,
            cancel,
            since,
        )
        .await
        {
            Ok(n) => n,
            Err(e) => return input_err(tool, e),
        };
        match self.backend.hover(x, y).await {
            Ok(()) => {
                let data = with_route(
                    json!({ "ok": true, "at": { "x": x, "y": y }, "glided": waypoints > 0 }),
                    Route::Hid,
                );
                Envelope::ok(
                    tool,
                    match target {
                        Some(t) => t.annotate(data),
                        None => data,
                    },
                )
            }
            Err(e) => input_err(tool, e),
        }
    }

    async fn drag_drop(&self, args: &Value, since: u64) -> Envelope {
        let tool = "drag_drop";
        let path = match parse_drag_path(args.get("path")) {
            Ok(p) => p,
            Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        };
        let from_spec = match endpoint(tool, args, "from") {
            Ok(s) => s,
            Err(e) => return e,
        };
        // With a path the drag ends at its last point; a second destination
        // would be one the drag silently ignored.
        let to_spec = if path.is_empty() {
            match endpoint(tool, args, "to") {
                Ok(s) => Some(s),
                Err(e) => return e,
            }
        } else {
            if let Some(k) = ["to", "to_ref", "to_name", "to_role", "to_x", "to_y"]
                .into_iter()
                .find(|k| args.get(k).is_some_and(|v| !v.is_null()))
            {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    format!("with 'path', drop '{k}': the drag ends at the last path point"),
                );
            }
            None
        };
        let (from, from_target) = match self.parse_point(tool, &from_spec).await {
            Ok(p) => p,
            Err(e) => return e,
        };
        let (to, to_target) = match &to_spec {
            Some(spec) => match self.parse_point(tool, spec).await {
                Ok((p, t)) => (Some(p), t),
                Err(e) => return e,
            },
            None => (None, None),
        };
        // Waypoints sit in the frame of the start: offsets from the element
        // when it was named, else absolute, the same as `from` itself.
        let origin = from_target
            .as_ref()
            .and_then(|t| t.bounds)
            .map_or((0.0, 0.0), |b| (b.x, b.y));
        let waypoints = drag_waypoints(to, &path, origin);
        for &(x, y) in std::iter::once(&from).chain(&waypoints) {
            if let Some(deny) = self.clamp_check(tool, x, y) {
                return deny;
            }
        }
        let modifiers = match parse_modifiers(tool, args) {
            Ok(m) => m,
            Err(e) => return e,
        };
        // Enough intermediate moves that a drop target sees motion, capped so a
        // caller cannot turn one gesture into thousands of synthetic events.
        let steps = u64_arg(args, "steps").unwrap_or(20).clamp(2, 100) as usize;
        let hold_ms = u64_arg(args, "hold_ms").unwrap_or(0).min(MAX_HOLD_MS);
        let route = drag_route(from, &waypoints, steps);
        let end = *waypoints.last().unwrap_or(&from);
        match self
            .backend
            .drag(from, &route, &modifiers, hold_ms, since)
            .await
        {
            Ok(()) => {
                let at = |p: (f64, f64), t: Option<Target>| match t {
                    Some(t) => t.annotate(json!({ "x": p.0, "y": p.1 })),
                    None => json!({ "x": p.0, "y": p.1 }),
                };
                let mut data = with_route(
                    json!({
                        "ok": true,
                        "from": at(from, from_target),
                        "to": at(end, to_target),
                        "steps": route.len(),
                        "hold_ms": hold_ms,
                        "modifiers": modifiers,
                    }),
                    Route::Hid,
                );
                if !path.is_empty() {
                    data["waypoints"] = json!(path.len());
                }
                Envelope::ok(tool, data)
            }
            Err(e) => input_err(tool, e),
        }
    }

    async fn clipboard_read(&self, args: &Value) -> Envelope {
        let tool = "clipboard_read";
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .and_then(ClipFormat::parse)
            .unwrap_or(ClipFormat::Text);
        match self.backend.clipboard_read(format).await {
            Ok(clip) => Envelope::ok(
                tool,
                json!({ "format": clip.format.as_str(), "data": clip.data }),
            ),
            Err(e) => input_err(tool, e),
        }
    }

    async fn clipboard_write(&self, args: &Value) -> Envelope {
        let tool = "clipboard_write";
        let format = args
            .get("format")
            .and_then(Value::as_str)
            .and_then(ClipFormat::parse)
            .unwrap_or(ClipFormat::Text);
        let Some(data) = args.get("data").and_then(Value::as_str) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'data'");
        };
        let secret = args.get("secret").and_then(Value::as_bool).unwrap_or(false);
        if let Some(deny) = self.destructive_check(tool, data, secret).await {
            return deny;
        }
        match self.backend.clipboard_write(format, data).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true })),
            Err(e) => input_err(tool, e),
        }
    }

    async fn input_showcase(&self, args: &Value) -> Envelope {
        let tool = "input_showcase";
        // Edited on a copy and stored only once every argument has been
        // accepted: a bad `speed` must not leave `enabled` half-applied.
        let mut g = self.glide.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(en) = args.get("enabled").and_then(Value::as_bool) {
            g.enabled = en;
            if en && matches!(g.preset, crate::glide::GlidePreset::Instant) {
                g.preset = crate::glide::GlidePreset::Demo;
            }
        }
        if let Some(spd) = args.get("speed").and_then(Value::as_str) {
            if let Some(preset) = crate::glide::GlidePreset::from_speed(spd) {
                g.preset = preset;
                g.enabled = !matches!(preset, crate::glide::GlidePreset::Instant);
            } else {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    "speed must be cinematic, demo, snappy, or instant",
                );
            }
        }
        if let Some(dur) = u64_arg(args, "duration_ms") {
            match crate::glide::check_duration_ms(dur) {
                Ok(ms) => g.duration_ms = Some(ms),
                Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
            }
        }
        *self.glide.lock().unwrap_or_else(|e| e.into_inner()) = g.clone();
        Envelope::ok(
            tool,
            json!({
                "ok": true,
                "glide": {
                    "enabled": g.enabled,
                    "speed": g.preset.as_str(),
                    "duration_ms": g.duration(),
                    "steps": g.steps()
                }
            }),
        )
    }
}

/// What to offer when keys sent to a background app cannot be shown to have
/// arrived. Not the system event stream: that is the route that takes over
/// the person's keyboard, and an automatic fallback to it would do so unasked.
const BACKGROUND_ALTERNATIVES: &str = "set_value writes the field through accessibility and works \
    in the background; or bring the app forward with focus_app (activate true) and type again, \
    which is refused under foreground = \"never\"";

/// How many candidates a failed name lookup offers.
const MAX_CANDIDATES: usize = 5;
/// Reads while waiting for an app to apply a change, and the gap between
/// them: about 0.4s in all, enough for a run-loop turn or two.
const SETTLE_READS: usize = 10;
const SETTLE_GAP: std::time::Duration = std::time::Duration::from_millis(40);
/// The points a drag passes through after the press: its `path` shifted by
/// `origin`, else just the destination.
fn drag_waypoints(
    to: Option<(f64, f64)>,
    path: &[(f64, f64)],
    origin: (f64, f64),
) -> Vec<(f64, f64)> {
    if path.is_empty() {
        return to.into_iter().collect();
    }
    path.iter()
        .map(|p| (origin.0 + p.0, origin.1 + p.1))
        .collect()
}

/// Longest a drag may hold the button before moving.
const MAX_HOLD_MS: u64 = 2_000;

/// What the caller will do with a resolved element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    /// Act on it through the accessibility API.
    Handle,
    /// Aim a pointer at it.
    Bounds,
}

impl Need {
    fn usable(self, info: &ElementInfo) -> bool {
        match self {
            Need::Handle => info.node_id.is_some(),
            Need::Bounds => info.bounds.is_some(),
        }
    }
}

/// An element a call resolved to, and how.
#[derive(Debug, Clone)]
struct Target {
    reff: String,
    node_id: Option<u64>,
    bounds: Option<Bounds>,
    role: String,
    name: Option<String>,
    secure: bool,
    /// For a lookup by name: how many elements matched as well as this one.
    matches: Option<usize>,
    /// The application the snapshot holding this element is of. A ref belongs
    /// to one application, which need not be the frontmost one, so a result
    /// says which it acted in.
    app: Option<String>,
}

impl Target {
    #[allow(clippy::result_large_err)]
    fn from_info(
        tool: &str,
        reff: &str,
        info: &ElementInfo,
        need: Need,
        matches: Option<usize>,
    ) -> Result<Target, Envelope> {
        if !need.usable(info) {
            return Err(Envelope::fail(
                tool,
                ErrorCode::ActionFailed,
                match need {
                    Need::Handle => "element is not actionable (no backend handle)",
                    Need::Bounds => "element has no bounds",
                },
            ));
        }
        Ok(Target {
            reff: reff.to_string(),
            node_id: info.node_id,
            bounds: info.bounds,
            role: info.role.clone(),
            name: info.name.clone(),
            secure: info.secure,
            matches,
            app: None,
        })
    }

    /// Add `ref` (what was resolved) and, for a lookup by name, `matches` to
    /// a result, so the caller learns which element a name meant.
    fn annotate(&self, data: Value) -> Value {
        let mut extra = json!({ "ref": self.reff });
        if let Some(n) = self.matches {
            extra["matches"] = json!(n);
        }
        if let Some(app) = &self.app {
            extra["app"] = json!(app);
        }
        merge(data, extra)
    }
}

/// `base` with `extra`'s keys added; both must be objects, else `base` as is.
fn merge(mut base: Value, extra: Value) -> Value {
    if let (Some(b), Value::Object(e)) = (base.as_object_mut(), extra) {
        for (k, v) in e {
            b.insert(k, v);
        }
    }
    base
}

/// A selection as a result reports it. The text is echoed up to a limit, so
/// selecting a whole document does not put the document in the result.
fn selection_data(sel: &Selection) -> Value {
    let (text, truncated) = clip_echo(&sel.text);
    let mut data = with_route(
        json!({
            "ok": true,
            "start": sel.start,
            "end": sel.end,
            "length": sel.end.saturating_sub(sel.start),
            "text": text,
        }),
        Route::Ax,
    );
    if truncated {
        data["truncated"] = json!(true);
    }
    if let Some(role) = &sel.role {
        data["role"] = json!(role);
    }
    data
}

/// A non-negative whole number argument, as a number or a numeric string.
fn index_arg(args: &Value, key: &str) -> Result<Option<usize>, String> {
    let Some(v) = args.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    match as_f64(v) {
        Some(n) if n >= 0.0 && n.fract() == 0.0 && n <= 1e9 => Ok(Some(n as usize)),
        _ => Err(format!("'{key}' must be a whole number, 0 or more")),
    }
}

/// A non-empty string argument.
fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Which text `select_text` was asked for. Exactly one of `all`,
/// `start`+`end`, `text` (with `occurrence`), or `from_text`+`to_text`: two
/// ways at once would mean one of them was silently ignored.
fn parse_range(args: &Value) -> Result<RangeSpec, String> {
    const HELP: &str = "give one of: all, start and end, text, or from_text and to_text";
    let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
    let start = index_arg(args, "start")?;
    let end = index_arg(args, "end")?;
    let text = str_arg(args, "text");
    let from = str_arg(args, "from_text");
    let to = str_arg(args, "to_text");
    let occurrence = index_arg(args, "occurrence")?;
    let ways = [
        all,
        start.is_some() || end.is_some(),
        text.is_some(),
        from.is_some() || to.is_some(),
    ];
    match ways.iter().filter(|w| **w).count() {
        0 => return Err(format!("nothing to select: {HELP}")),
        1 => {}
        _ => return Err(format!("more than one way to choose the text: {HELP}")),
    }
    if occurrence.is_some() && text.is_none() {
        return Err("'occurrence' goes with 'text'".into());
    }
    if all {
        return Ok(RangeSpec::All);
    }
    if start.is_some() || end.is_some() {
        return match (start, end) {
            (Some(start), Some(end)) => Ok(RangeSpec::Offsets { start, end }),
            _ => Err("'start' and 'end' go together".into()),
        };
    }
    if let Some(needle) = text {
        return Ok(RangeSpec::Text {
            needle: needle.to_string(),
            occurrence: occurrence.unwrap_or(1),
        });
    }
    match (from, to) {
        (Some(f), Some(t)) => Ok(RangeSpec::Between {
            from: f.to_string(),
            to: t.to_string(),
        }),
        _ => Err("'from_text' and 'to_text' go together".into()),
    }
}

/// `data` with the route that acted on the UI, as `input`.
fn with_route(data: Value, route: Route) -> Value {
    merge(data, json!({ "input": route.as_str() }))
}

/// `Double-Click` and `double click` as `double_click`.
fn snake(s: &str) -> String {
    s.trim().to_lowercase().replace(['-', ' '], "_")
}

/// An option inside a popup, as opposed to the popup itself.
fn is_entry_role(role: &str) -> bool {
    matches!(
        mcp_a11y::normalize_role(role).as_str(),
        "menuitem" | "listitem"
    )
}

/// A control whose value is text the user types.
fn is_text_role(role: &str) -> bool {
    use mcp_a11y::matcher::role_matches;
    role_matches(role, "textfield") || role_matches(role, "combobox")
}

/// An optional numeric argument that must be a number when it is present.
#[allow(clippy::result_large_err)]
fn number_arg(tool: &str, args: &Value, key: &str) -> Result<Option<f64>, Envelope> {
    match args.get(key).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => as_f64(v).map(Some).ok_or_else(|| {
            Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("'{key}' must be a number"),
            )
        }),
    }
}

/// One end of a drag, as a point spec: the `from`/`to` object, a bare ref
/// string, or the flat `from_ref`/`from_x`/... spelling.
#[allow(clippy::result_large_err)]
fn endpoint(tool: &str, args: &Value, side: &str) -> Result<Value, Envelope> {
    match args.get(side) {
        Some(v) if v.is_object() => return Ok(v.clone()),
        Some(Value::String(s)) => {
            // A bare string is a ref when it looks like one, else a name.
            return Ok(match parse_ref(&Value::String(s.clone())) {
                Some(r) if s.trim_start().starts_with('@') => json!({ "ref": r }),
                _ => json!({ "name": s }),
            });
        }
        Some(Value::Number(n)) => return Ok(json!({ "ref": n })),
        Some(Value::Null) | None => {}
        Some(_) => {
            return Err(Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!(
                    "'{side}' must be an object like {{\"ref\":\"@e3\"}} or {{\"x\":10,\"y\":20}}"
                ),
            ))
        }
    }
    let mut spec = serde_json::Map::new();
    for key in ["ref", "name", "role", "x", "y"] {
        if let Some(v) = args.get(format!("{side}_{key}")).filter(|v| !v.is_null()) {
            spec.insert(key.to_string(), v.clone());
        }
    }
    if spec.is_empty() {
        return Err(Envelope::fail(
            tool,
            ErrorCode::InvalidArgs,
            format!("missing '{side}': give {{ref}}, {{name}} or {{x, y}} (or {side}_ref, {side}_x, {side}_y)"),
        ));
    }
    Ok(Value::Object(spec))
}

/// Decide whether a write landed, from what the field showed before, what it
/// shows now, and what was asked for.
///
/// Landing is not equality: a number field turns `1000` into `1,000`, and a
/// field with an input mask shows what the mask made of it. So the only
/// rejection is the one that cannot be argued with: the value is exactly what
/// it was, and not what was asked for.
fn judge_write(
    before: Option<&str>,
    after: Option<&str>,
    wanted: &str,
    hide: bool,
) -> Result<Value, String> {
    let Some(after) = after else {
        // The platform would not say. Neither a success nor a failure can be
        // claimed, so say that.
        return Ok(json!({ "verified": false }));
    };
    let landed = after == wanted;
    let changed = before != Some(after);
    if !landed && !changed {
        return Err(if hide {
            "the field did not take the new text: its value is unchanged".to_string()
        } else {
            format!("the app kept {after:?}; the new text was not accepted")
        });
    }
    let mut data = json!({ "verified": true, "changed": changed });
    if !hide {
        data["value_after"] = json!(after);
        if !landed {
            // Reported rather than hidden: the field reformatted what was typed.
            data["reformatted"] = json!(true);
        }
    }
    Ok(data)
}

/// Docs-only text shared by every tool that takes a target element.
const TARGET_DETAILS: &str = "Target an element by `ref` (`@e12`; `e12` and `12` also work) or by `name`, with an \
    optional `role` to narrow it. A name is matched against the latest snapshot (a fresh one is taken if there is \
    none), ignoring case and extra whitespace: exact name first, then prefix, then substring; the name and the \
    current value both count; controls come before other elements; ties go to document order. The result reports \
    the `ref` it resolved to and, for a name, `matches`: how many elements matched as well as the one used. A \
    name that matches nothing is NOT_FOUND, with up to five elements that share a word with it.";

/// The properties that say which element, shared by every targeting tool.
fn target_props() -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert(
        "ref".into(),
        json!({"type":"string","description":"element ref @eN"}),
    );
    m.insert(
        "name".into(),
        json!({"type":"string","description":"element name or label, instead of ref; best match wins"}),
    );
    m.insert(
        "role".into(),
        json!({"type":"string","description":"narrow a name by role, e.g. button, popup button, text field"}),
    );
    m
}

/// An object schema with the target properties plus `extra`.
fn target_schema(extra: Value, required: &[&str]) -> Value {
    let mut props = target_props();
    if let Value::Object(e) = extra {
        props.extend(e);
    }
    let mut schema = json!({ "type": "object", "properties": props });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

/// One end of a drag.
fn point_schema(description: &str) -> Value {
    let mut props = target_props();
    props.insert("x".into(), json!({"type":"number"}));
    props.insert("y".into(), json!({"type":"number"}));
    json!({ "type": "object", "description": description, "properties": props })
}

/// The `expect` clause the action tools accept.
///
/// It belongs *inside* `properties`, next to the other arguments. Hoisting it
/// to the top level of the schema (which is where it started) leaves it
/// undeclared: a client reading the schema properly never learns the argument
/// exists, and the act-and-confirm round trip silently goes unused.
fn expect_schema() -> serde_json::Value {
    mcp_window::wait_schema(
        "optional: wait for this to become true after the action, and return what changed",
    )
}

/// Marks a batch as holding the loan for as long as it lives.
struct BatchLoan<'a>(&'a std::sync::atomic::AtomicBool);

impl<'a> BatchLoan<'a> {
    fn begin(flag: &'a std::sync::atomic::AtomicBool) -> Self {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        BatchLoan(flag)
    }
}

impl Drop for BatchLoan<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

impl InputModule {
    /// `foreground = "polite"`: a call that needs the pointer, keyboard or
    /// foreground waits for the person to be idle, borrows the desk, and gives
    /// it back on every path out, including an error and a cancelled call
    /// (the loan's `Drop`). A batch borrows once for all its steps and gives
    /// back once at the end; a person who takes the pointer part way ends the
    /// action with a retryable error, and the pointer is left with them.
    async fn call_politely(&self, name: &str, args: Value, ctx: &CallCtx) -> Envelope {
        use crate::polite::Loan;
        if let Some(why) = crate::foreground::polite_refusal(name, &args) {
            return Envelope::fail_with(
                name,
                ErrorCode::PolicyDenied,
                why,
                "press the key instead, or hold it with the foreground setting at allow",
            );
        }
        // A step of a batch that has already borrowed: the batch settles up.
        if name != "input_steps" && self.batch_loan.load(std::sync::atomic::Ordering::SeqCst) {
            return self.run_call(name, args, ctx).await;
        }
        if !crate::foreground::takes_foreground(&*self.backend, name, &args) {
            return self.run_call(name, args, ctx).await;
        }
        let halt = self.halt.clone();
        let cancel = ctx.cancel.clone();
        let loan = match Loan::borrow(self.backend.clone(), self.policy.polite, move || {
            if halt.as_ref().is_some_and(|h| h()) {
                Some("the kill switch is engaged")
            } else if cancel.is_cancelled() {
                Some("the call was cancelled")
            } else {
                None
            }
        })
        .await
        {
            Ok(l) => l,
            Err(e) => return e.envelope(name),
        };
        let _batch = (name == "input_steps").then(|| BatchLoan::begin(&self.batch_loan));
        let since = self.backend.takeover_generation();
        let env = self.run_call(name, args, ctx).await;
        crate::polite::settle(loan, &*self.backend, since, env).await
    }

    /// The call itself, once the foreground setting has had its say.
    async fn run_call(&self, name: &str, args: Value, ctx: &CallCtx) -> Envelope {
        // Read before anything can await: a takeover from here on is this
        // call's to honour, whenever its first pointer move happens to be.
        let since = self.backend.takeover_generation();

        // Held for the whole call, including any `expect` wait: the pointer is
        // ours for that entire window, so a divergence during it is somebody
        // else's hand.
        // Watching a person is not driving: a recording call must not arm the
        // human-override watcher, whose point is that the person's hand on the
        // mouse is the agent's cue to stop, not what is being recorded.
        let _driving = (name != "desktop_record")
            .then(|| self.activity.as_ref().map(crate::Activity::begin))
            .flatten();

        // An `expect` clause is parsed *before* the action: discovering the
        // expectation was malformed after clicking is too late to be useful.
        let spec = if EXPECTING.contains(&name) {
            match crate::postcondition::Verifier::parse(name, &args) {
                Ok(s) => s,
                Err(e) => return *e,
            }
        } else {
            None
        };
        if spec.is_some() && self.verifier.is_none() {
            return Envelope::fail_with(
                name,
                ErrorCode::UnsupportedOs,
                "postconditions need a UI to observe, and none is wired on this platform",
                "drop 'expect' and confirm separately with wait_for",
            );
        }
        // The comparison point has to be captured before the action, not after.
        let before = match (&spec, self.verifier.as_ref()) {
            (Some(_), Some(v)) => v.before(),
            _ => None,
        };
        // `effects` is validated with the rest of the arguments, so a typo is
        // refused before the action instead of discovered after it. An `expect`
        // brings its own delta, which makes the second report redundant.
        let reporting = if EFFECTING.contains(&name) {
            match crate::effects::wanted(&args) {
                Ok(on) => on && spec.is_none() && effects_apply(name, &args),
                Err(m) => return Envelope::fail(name, ErrorCode::InvalidArgs, m),
            }
        } else {
            false
        };
        let seen_before = match (&self.probe, reporting) {
            (Some(_), true) => Some(self.facts_before(name, &args).await),
            _ => None,
        };

        let mut env = match name {
            "ui_action" => self.ui_action(&args).await,
            "set_value" => self.set_value(&args).await,
            "select_text" => self.select_text(&args).await,
            "get_selection" => self.get_selection(&args).await,
            "ui_fill_form" => self.ui_fill_form(&args).await,
            "keyboard_type" => self.keyboard_type(&args).await,
            "keyboard_shortcut" => self.keyboard_shortcut(&args, since).await,
            "mouse_action" => self.mouse_action(&args, &ctx.cancel, since).await,
            "scroll" => self.scroll(&args, &ctx.cancel, since).await,
            "hover" => self.hover(&args, &ctx.cancel, since).await,
            "drag_drop" => self.drag_drop(&args, since).await,
            "clipboard_read" => self.clipboard_read(&args).await,
            "clipboard_write" => self.clipboard_write(&args).await,
            "input_showcase" => self.input_showcase(&args).await,
            "input_steps" => self.input_steps(&args, ctx).await,
            "desktop_record" => self.desktop_record(&args).await,
            "desktop_flow" => self.desktop_flow(&args, ctx).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        };
        if let (Some(before), true) = (seen_before, env.ok) {
            self.attach_effects(&mut env, before).await;
        }

        let mut env = match (spec, self.verifier.as_ref()) {
            (Some(spec), Some(v)) => {
                let verified = v.verify(&spec, before, ctx).await;
                crate::postcondition::attach(env, name, verified)
            }
            _ => env,
        };
        // `down` and `up` settle their own holds, and so does each step of a
        // batch: one that reached the module and failed has released them, and
        // one the gate refused never touched them, as when called alone.
        let manages_holds = name == "input_steps"
            || name == "desktop_flow"
            || (name == "keyboard_shortcut"
                && !matches!(shortcut_action(&args), Ok(HoldAction::Press) | Err(_)));
        if !env.ok && !manages_holds {
            self.release_after_failure(&mut env).await;
        }
        env
    }
}

#[async_trait]
impl ToolModule for InputModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools = vec![
            ToolDescriptor::new(
                "ui_action",
                Category::Input,
                Tier::Standard,
                "Perform a semantic action on an element by ref or name (accessibility action, no cursor, app need not be frontmost). select with option chooses a popup or combo box entry by its text. An action the element does not offer is an error naming the ones it does.",
                target_schema(json!({
                    "action":{"type":"string","enum":["click","double_click","right_click","focus","toggle","check","uncheck","expand","collapse","select","scroll_into_view","show_menu","increment","decrement","pick"]},
                    "option":{"type":"string","description":"entry text for select"},
                    "expect": expect_schema()
                }), &["action"]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                `select` with `option` opens a popup button or combo box, activates the entry whose text matches \
                (exactly, then ignoring extra whitespace, then ignoring case), and reads back what the control \
                shows: `selected` and `changed`. An option that does not exist is an error listing the options (25 \
                at most); a disabled one is an error; and a control that shows something else afterwards fails the \
                call with what it kept. Anything the call opened is closed again on failure. Pointing at a menu \
                item or list entry chooses it through the popup it belongs to. Without `option`, `select` presses \
                the element as before.\n\n\
                `check`, `uncheck` and `toggle` read the state first (checking a checked box changes nothing) and \
                again afterwards, and report `checked` and `changed`; a press that did not take is an error.\n\n\
                macOS opens a popup button with the accessibility press and presses its menu item; a combo box takes \
                the text. Linux invokes the combo box's menu or list item through AT-SPI.\n\n\
                `show_menu` opens the element's context menu (AXShowMenu) without a right click, `increment` and \
                `decrement` take one step on a slider, stepper or scroll bar and report `value_after` and `changed` \
                (`changed: false` means the control is at its limit or ignored the step), `pick` picks the element \
                in a list or picker (AXPick), and `scroll_to_visible` is accepted for `scroll_into_view`. On macOS \
                these four are checked against the element's own action list first, so an element that lacks one \
                gets an error listing what it does support, never an `ok`. Linux has no context-menu action: \
                `show_menu` looks for a menu or popup action on the element and otherwise says what it has; \
                `increment` and `decrement` move an AT-SPI Value by its minimum increment, clamped to its range.\n\n\
                Every result carries `input`: `ax` when the accessibility API acted (no pointer, the app can stay \
                in the background) `pid_event` for keys sent to one background process (`keyboard_type`, `keyboard_shortcut`), or `hid` when \
                synthetic system input was needed (the real cursor moved). \
                `right_click` and `double_click` are an accessibility press on macOS, which has no distinct \
                double or secondary press; on Linux `right_click` clicks at the element's centre, so it is `hid`. \
                `input` is absent when nothing needed doing (checking a checked box)."),
            ToolDescriptor::new(
                "set_value",
                Category::Input,
                Tier::Standard,
                "Set the value of a text element by ref or name (accessibility SetValue). This works on a background window and needs no focus. Set 'secret' for a password.",
                target_schema(json!({
                    "text":{"type":"string"},
                    "secret":{"type":"boolean","description":"the text is a password or other secret: keep it out of the audit log and never send it to the judge"},
                    "expect": expect_schema()}), &["text"]),
            ).idempotent(true).details(TARGET_DETAILS.to_string() + "\n\n\
                The element is read before and after the write. The result carries `value_after` (never for a \
                `secret` or a password field), `changed`, and `reformatted` when the field shows something other \
                than what was written (a number field turning 1000 into 1,000 is fine). A field that keeps its old \
                value and does not show the new text is an error, not an `ok`. `verified: false` means the platform \
                could not read the value back."),
            ToolDescriptor::new(
                "select_text",
                Category::Input,
                Tier::Standard,
                "Select text in a text field or text area by ref or name, through the accessibility API (no drag, no focus needed), and read the selection back. Give one of: all; start and end (character offsets); text (with occurrence); from_text and to_text.",
                target_schema(json!({
                    "all":{"type":"boolean","description":"select everything"},
                    "start":{"type":"integer","description":"first character offset, from 0"},
                    "end":{"type":"integer","description":"offset after the last character; equal to start places the caret"},
                    "text":{"type":"string","description":"select this text"},
                    "occurrence":{"type":"integer","description":"which match of text, from 1 (default 1)"},
                    "from_text":{"type":"string","description":"select from the start of this text..."},
                    "to_text":{"type":"string","description":"...to the end of the next match of this"},
                    "expect": expect_schema()}), &[]),
            ).idempotent(true).details(TARGET_DETAILS.to_string() + "\n\n\
                Offsets count characters (Unicode scalar values) from 0 with `end` exclusive, on every platform: \
                a combining accent is its own character, and a character outside the Basic Multilingual Plane is \
                one, not two. `text` matches exactly (case and accents count), non-overlapping, and `occurrence` \
                picks the Nth. `from_text`/`to_text` select from the start of the first `from_text` through the end \
                of the first `to_text` that follows it. The element's text is read first and the request is \
                resolved against it.\n\n\
                The result carries `start`, `end`, `length` and `text` as the element reports them after the \
                change (`text` is cut at 2000 characters, with `truncated`), and `input: ax`. If the element \
                reports a different range than was asked for (some editors snap to words or ignore the request), \
                the call fails and says what it reports. macOS sets `AXSelectedTextRange`; an element where that \
                is not settable is an `UNSUPPORTED_OS` error. There is no pointer fallback: dragging to select is \
                not attempted, so use `drag_drop` or shift-click with `mouse_action` for those. Linux uses the \
                AT-SPI Text interface (`set_selection`). Password fields are refused. Which apps honour the \
                selection has not been verified beyond TextEdit."),
            ToolDescriptor::new(
                "get_selection",
                Category::Input,
                Tier::Standard,
                "Read the text selected in an element (ref or name), or in the focused element when none is given, without changing it.",
                target_schema(json!({}), &[]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                Returns `start`, `end` (character offsets, as in `select_text`), `length`, `text` (cut at 2000 \
                characters, with `truncated`) and `role`; a caret with nothing selected is `length: 0`. Without a \
                target it reads the focused element of the app being driven (macOS only; on Linux give a ref). \
                Password fields are refused."),
            ToolDescriptor::new(
                "ui_fill_form",
                Category::Input,
                Tier::Standard,
                "Fill multiple native UI fields (text fields, checkboxes, switches, popups, radios) \
                 in one call, and optionally submit and verify postconditions. \
                 Target fields by ref (@eN) or name/label.",
                json!({
                    "type": "object",
                    "properties": {
                        "fields": {
                            "type": "array",
                            "description": "list of fields to set or toggle",
                            "items": target_schema(json!({
                                "value": { "description": "string text, boolean for checkbox/switch, or selection option" },
                                "action": {
                                    "type": "string",
                                    "enum": ["set_value", "click", "toggle", "check", "uncheck", "select", "focus"],
                                    "description": "action to perform (defaults to set_value for text, check/uncheck for boolean)"
                                },
                                "option": { "type": "string", "description": "entry text for a popup or combo box (with action select)" },
                                "secret": { "type": "boolean", "description": "password or secret: redacts value from audit logs and output" }
                            }), &[])
                        },
                        "submit": target_schema(json!({
                            "action": { "type": "string", "enum": ["click", "toggle"] }
                        }), &[]),
                        "expect": expect_schema()
                    },
                    "required": ["fields"]
                }),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                Fields are applied in order and the call stops at the first failure, reporting `filled` and the \
                `results` so far. Each result carries what that field's tool would: `value_after`, `selected`, \
                `checked` and `changed`. A field named by `name` resolves with the same ranking as `ui_action`, so \
                two fields with similar labels are told apart by exactness and then document order."),
            ToolDescriptor::new(
                "keyboard_type",
                Category::Input,
                Tier::Standard,
                "Type Unicode text into the focused window. If a target (ref or name) is given, focus it first and read its value back. Does not press return. When the app pinned with focus_app is not frontmost, the text is sent to that app directly (input \"pid_event\") without raising it. Set 'secret' when typing a password so it is kept out of the audit log and never sent to the judge.",
                target_schema(json!({
                    "text":{"type":"string"},
                    "secret":{"type":"boolean","description":"the text is a password or other secret: keep it out of the audit log and never send it to the judge"},
                    "expect": expect_schema()}), &["text"]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                With a target, the element's value is read before and after. The result carries `value_after` \
                (never for a `secret` or a password field) and `changed`; typing into a text field that then shows \
                the same value is an error. Without a target, the keystrokes go wherever focus is and nothing is \
                read back.\n\n\
                When the app pinned with `focus_app` is not the frontmost one (pin it with `activate: false`), \
                the text is sent to that process with per-process key events, so it does not bring the app \
                forward and the frontmost app receives nothing. The result's `input` is `pid_event` and \
                `verified: true` means the app's focused element changed as typed. Some apps drop these events \
                while they are in the background; then the call is an error that says so (or that delivery \
                could not be read back, for a password field or an app with no readable text) and does not \
                retry through the foreground. Use `set_value` for those, or `focus_app` with `activate: true` \
                and type again. Not available on Linux, where keys always go to the focused window. It is \
                refused while keys are held with `keyboard_shortcut` `down`."),
            ToolDescriptor::new(
                "keyboard_shortcut",
                Category::Input,
                Tier::Standard,
                "Press a key or chord, e.g. Return, Escape, cmd+s, Control+A, cmd+shift+z. action down holds keys (e.g. shift across several clicks) until action up; held keys auto-release after 30 s.",
                json!({"type":"object","properties":{"combo":{"type":"string"},
                    "action":{"type":"string","enum":["press","down","up"],"description":"press (default) taps the chord; down holds the key or modifiers; up releases them (no combo: everything held)"},
                    "expect": expect_schema()},"required":[]}),
            ).details("Modifiers then exactly one key, joined by `+` (or `-` when there is no `+`). Case does not \
                matter: `Control+A`, `ctrl-a` and `CTRL+a` are one chord. Modifiers: cmd/command, ctrl/control, \
                shift, alt/opt/option, fn, super/meta/win; `mod` (also `cmdorctrl`, `primary`) is Command on macOS \
                and Control on Linux, and on Linux `cmd` means Control. Keys: letters, digits, return/enter, tab, \
                space, escape/esc, delete/backspace (erases backwards), forwarddelete/del, insert, home, end, \
                pageup/page_up/pgup, pagedown/page_down/pgdn, up/down/left/right (also ArrowUp...), F1-F20, \
                punctuation as the character or its name (minus, equal, plus, comma, period, slash, backslash, \
                semicolon, quote, grave, leftbracket, rightbracket), and a literal `+` as `plus` or a trailing \
                `cmd++`. More than one key is an error, never a silent drop. The result's `pressed` is the \
                canonical chord that was sent.\n\n\
                `action: \"down\"` takes the same names but also a lone modifier (`shift`, `cmd+shift`) or one key, \
                and leaves them down across later calls: every click, drag, scroll and chord until the matching \
                `action: \"up\"` carries the held modifiers, whatever `modifiers` the call itself names. Holding \
                a key already held renews its time limit and presses nothing. At most 6 keys are held at once. \
                A key is released by `up` (with no `combo`, all of them), after 30 s unless renewed (the result's \
                `auto_release_ms`), when any other input call fails (the error says so), when the kill switch \
                trips or a human takes the mouse, and when the server stops. `keyboard_type` ignores held \
                modifiers: the text carries its own case. On macOS a held modifier is applied to the events \
                agentctl posts and is not a physical key press, so it cannot stay latched in the system; \
                other software that polls the keyboard state does not see it. On Linux it is a real key press.\n\n\
                A chord pressed while the app pinned with `focus_app` is in the background is sent to that \
                process (`input` is `pid_event`) and does not bring it forward. Text-editing chords such as \
                `shift+left` arrive; menu key equivalents such as `cmd+a` and `cmd+s` were not delivered to \
                TextEdit this way. A chord has no read-back, so the result's `verified` is `true` only when \
                the focused element visibly changed and `false` otherwise, with a `note`: `false` means \
                not known, not that it failed. `action: \"down\"` and `up` always use the frontmost app \
                (and `down` brings a pinned app forward); a pid press is refused while keys are held."),
            ToolDescriptor::new(
                "mouse_action",
                Category::Input,
                Tier::Standard,
                "Coordinate pointer action. With a ref or name, x/y are offsets in points from the element's top-left (default: its centre); without one they are absolute screen coordinates. 'modifiers' holds keys down for the click, e.g. [\"cmd\"] or [\"shift\"].",
                target_schema(json!({
                    "type":{"type":"string","enum":["move","click","double","triple","right_click","down","up"]},
                    "x":{"type":"number","description":"screen x, or offset from the element's left"},
                    "y":{"type":"number","description":"screen y, or offset from the element's top"},
                    "button":{"type":"string","description":"left|right|middle"},
                    "modifiers":{"type":"array","items":{"type":"string","enum":["cmd","shift","opt","alt","ctrl","fn"]}},
                    "glide":{"type":"boolean","description":"smooth Bezier gliding to target coordinates"},
                    "speed":{"type":"string","enum":["cinematic","demo","snappy","instant"],"description":"gliding speed preset"},
                    "duration_ms":{"type":"integer","description":"custom gliding duration in milliseconds, 0-2000"},
                    "expect": expect_schema()}), &["type"]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                `x` and `y` may be numbers or numeric strings. Every point, absolute or element-relative, is \
                checked against the allowed window bounds when `clamp_input_to_allowed` is on. The result's `at` \
                is the screen point that was used. Set `glide: true` or `speed` to glide the pointer along a \
                Bezier curve for demos."),
            ToolDescriptor::new(
                "scroll",
                Category::Input,
                Tier::Standard,
                "Scroll at an element (ref or name) or a point: wheel lines by 'direction', or smooth trackpad-style pixels by 'dx'/'dy' with optional momentum. 'modifiers' holds keys down for the wheel, e.g. [\"cmd\"] or [\"ctrl\"] to zoom.",
                target_schema(json!({
                    "x":{"type":"number","description":"screen x, or offset from the element's left"},
                    "y":{"type":"number","description":"screen y, or offset from the element's top"},
                    "direction":{"type":"string","enum":["up","down","left","right","page_up","page_down"]},
                    "amount":{"type":"integer","description":"wheel lines (default 3); a page is ten lines"},
                    "dx":{"type":"integer","description":"smooth scroll right by this many pixels (negative: left); with dy, used instead of direction and amount"},
                    "dy":{"type":"integer","description":"smooth scroll down by this many pixels (negative: up)"},
                    "duration_ms":{"type":"integer","description":"how long the fingers-down part takes with dx/dy (default 250, at most 3000)"},
                    "momentum":{"type":"boolean","description":"with dx/dy, keep coasting after release as a flick does (macOS)"},
                    "gesture":{"type":"string","enum":["pinch","rotate","swipe"],"description":"always UNSUPPORTED_OS; the error names the working equivalent"},
                    "modifiers":{"type":"array","items":{"type":"string","enum":["cmd","shift","opt","alt","ctrl","fn"]}}}), &[]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                With a target, `x` and `y` are offsets from the element's top-left (default its centre); without \
                one they are absolute screen coordinates. `modifiers` takes the same names as `mouse_action` and \
                `drag_drop` and is held for the whole gesture: `[\"cmd\"]` (macOS) or `[\"ctrl\"]` zooms in most \
                documents, maps and canvases, `[\"shift\"]` scrolls sideways where an app binds it. The result's \
                `modifiers` is what was held.\n\n\
                Give `direction` (with `amount`) for wheel lines, or `dx` and/or `dy` for a smooth trackpad scroll in \
                pixels (positive is right and down). That sends a stream of pixel events with gesture phases \
                (began, changed, ended) over `duration_ms`, and with `momentum: true` the coast of a flick after \
                release. Caps: 10000 px per axis, 3000 ms, momentum at most 3000 px and 60 events; over them is \
                INVALID_ARGS. The deltas sum exactly to the request; the result's `dx`, `dy`, `events` and \
                `momentum_dx` / `momentum_dy` say what was sent. Either form moves the real pointer to the target, so \
                `foreground = \"never\"` refuses it (use `ui_action scroll_into_view`, or increment / decrement on a \
                scroll bar) and `polite` waits for the person to be idle. Pinch, rotate and swipe have no public \
                system API to synthesise and are not attempted: `gesture` returns UNSUPPORTED_OS naming the app's own \
                command (zoom: `cmd+plus` / `cmd+minus` or ctrl-scroll; rotate: its menu item; swipe: back / forward \
                shortcut, or `ctrl+left` / `ctrl+right` between spaces)."),
            ToolDescriptor::new(
                "hover",
                Category::Input,
                Tier::Standard,
                "Move the pointer over an element (ref or name) or point (reveals tooltips/hover menus). Supports smooth gliding via 'glide' or 'speed'.",
                target_schema(json!({
                    "x":{"type":"number","description":"screen x, or offset from the element's left"},
                    "y":{"type":"number","description":"screen y, or offset from the element's top"},
                    "glide":{"type":"boolean","description":"smooth Bezier gliding to target coordinates"},
                    "speed":{"type":"string","enum":["cinematic","demo","snappy","instant"],"description":"gliding speed preset"},
                    "duration_ms":{"type":"integer","description":"custom gliding duration in milliseconds, 0-2000"}}), &[]),
            ).details(TARGET_DETAILS.to_string() + "\n\n\
                With a target, `x` and `y` are offsets from the element's top-left (default its centre); without \
                one they are absolute screen coordinates."),
            ToolDescriptor::new(
                "drag_drop",
                Category::Input,
                Tier::Standard,
                "Press-move-release drag from one point/element to another. The pointer travels in 'steps' intermediate moves so targets that track motion register the drag.",
                json!({"type":"object","properties":{
                    "from": point_schema("where to press: {ref}, {name} or {x,y}; with ref/name, x/y offset from its top-left"),
                    "to": point_schema("where to release: same forms as from"),
                    "from_ref":{"type":"string"},"from_name":{"type":"string"},"from_x":{"type":"number"},"from_y":{"type":"number"},
                    "to_ref":{"type":"string"},"to_name":{"type":"string"},"to_x":{"type":"number"},"to_y":{"type":"number"},
                    "steps":{"type":"integer","description":"intermediate moves, 2-100 (default 20)"},
                    "hold_ms":{"type":"integer","description":"hold the button this long before moving, 0-2000"},
                    "path":{"type":"array","description":"up to 200 waypoints to pass through in order, releasing at the last; replaces 'to'. Offsets from the 'from' element's top-left when it is a ref/name, else screen points","items":{"type":"object","properties":{"x":{"type":"number"},"y":{"type":"number"}}}},
                    "modifiers":{"type":"array","items":{"type":"string","enum":["cmd","shift","opt","alt","ctrl","fn"]}}},
                    "required":[]}),
            ).details("Each end is `from`/`to` as an object (`{ref}`, `{name}` with an optional `role`, or \
                `{x, y}`), or the flat `from_ref`/`from_x`/... spelling. With a ref or name, `x` and `y` are offsets \
                in points from the element's top-left (default its centre); otherwise absolute screen coordinates. \
                Both points are checked against the allowed window bounds. `path` is up to 200 waypoints `{x, y}` \
                (the same argument as `browser_act drag`) for brush strokes and freehand shapes: the drag presses at \
                `from`, passes through each waypoint in order and releases at the last, so give no `to`. Waypoints \
                are in the frame of `from`: offsets from its top-left when it is a ref or name, else absolute \
                screen points. `steps` moves are spread over the segments by length, at least one landing on every \
                waypoint; every waypoint is checked against the allowed window bounds before anything moves. The \
                result's `waypoints` is the number of path points and `steps` the moves made. `hold_ms` keeps the button down before \
                the first move, for drag sources (Finder icons, list rows) that wait to tell a drag from a click."),
            ToolDescriptor::new(
                "clipboard_read",
                Category::Input,
                Tier::Standard,
                "Read the clipboard. 'text'/'html' return the string in 'data'; \
                 'image' returns a base64 PNG in 'data'; 'files' returns a \
                 newline-separated list of file:// URIs.",
                json!({"type":"object","properties":{"format":{"type":"string","enum":["text","html","image","files"]}},"required":[]}),
            ).untrusted_output(),
            ToolDescriptor::new(
                "clipboard_write",
                Category::Input,
                Tier::Standard,
                "Write the clipboard. For 'image', 'data' is a base64 PNG; for \
                 'files', it is a newline-separated list of file:// URIs; \
                 otherwise it is the literal text. Set 'secret' if the data is \
                 sensitive.",
                json!({"type":"object","properties":{"format":{"type":"string","enum":["text","html","image","files"]},"data":{"type":"string"},"secret":{"type":"boolean","description":"the text is a password or other secret: keep it out of the audit log and never send it to the judge"}},"required":["data"]}),
            ).idempotent(true),
            ToolDescriptor::new(
                "input_showcase",
                Category::Input,
                Tier::Standard,
                "Glide the real mouse pointer along a smooth Bezier curve to each point instead of jumping there, for demos, screencasts and presentations. It only moves the system cursor: no click ripples, typing HUD or styled cursor are drawn on the desktop (browser_showcase draws those inside a browser tab).",
                json!({"type":"object","properties":{
                    "enabled":{"type":"boolean","description":"enable or disable smooth gliding"},
                    "speed":{"type":"string","enum":["cinematic","demo","snappy","instant","off"],"description":"speed preset (cinematic: 350ms, demo: 200ms, snappy: 100ms, instant: 0ms)"},
                    "duration_ms":{"type":"integer","description":"custom duration in milliseconds, 0-2000"}}}),
            ).idempotent(true),
            ToolDescriptor::new(
                "input_steps",
                Category::Input,
                BATCH_TIER,
                "Run up to 20 input actions in one call, in order, stopping at the first that fails. Each step is {tool, args}: the tool's name and the arguments you would give it alone. Every step is checked, allowed and logged exactly as if you had called it yourself.",
                json!({"type":"object","properties":{
                    "steps":{"type":"array","description":"actions to run in order (max 20)","items":{"type":"object","properties":{
                        "tool":{"type":"string","enum":crate::steps::BATCHABLE},
                        "args":{"type":"object","description":"the tool's own arguments"}
                    },"required":["tool"]}}
                },"required":["steps"]}),
            ).details(BATCH_DETAILS),
            ToolDescriptor::new(
                "desktop_record",
                Category::Input,
                Tier::Dangerous,
                "Watch a person demonstrate a task and turn it into a desktop_flow: start, stop (name saves it) or status. Keeps what was clicked or typed as what it was (role, title, ancestry), never typed secrets. Only allowed_apps; bounded in time and size. Dangerous tier.",
                json!({"type":"object","properties":{
                    "action":{"type":"string","enum":["start","stop","status"],"description":"default status"},
                    "name":{"type":"string","description":"stop: flow name to save"},
                    "max_seconds":{"type":"integer","description":"start: stop listening after this long (default 120, at most 600)"}}}),
            ).details(RECORD_DETAILS),
            ToolDescriptor::new(
                "desktop_flow",
                Category::Input,
                BATCH_TIER,
                "Keep and replay recorded desktop flows: list, get, save, delete, run (name, secrets). Run finds each step's element by role, title and ancestry and performs it through the normal tools, so policy and audit see every step; stops at the first failure.",
                json!({"type":"object","properties":{
                    "action":{"type":"string","enum":["list","get","save","delete","run"],"description":"default list"},
                    "name":{"type":"string","description":"the flow"},
                    "steps":{"type":"array","description":"save: the steps, as desktop_flow get shows them","items":{"type":"object"}},
                    "secrets":{"type":"object","description":"run: values for the flow's secret_refs, used in memory only"}}}),
            ).details(FLOW_DETAILS),
        ];
        // The same optional switch on every tool that reports `effects`, added
        // here so the wording cannot drift between them.
        for t in tools
            .iter_mut()
            .filter(|t| EFFECTING.contains(&t.name.as_str()))
        {
            if let Some(props) = t
                .input_schema
                .get_mut("properties")
                .and_then(Value::as_object_mut)
            {
                props.insert(
                    "effects".into(),
                    json!({"type":"boolean","description":"false: do not report what changed (saves a short wait)"}),
                );
            }
            // The full account once, on the tool an agent reaches for first.
            let fx = if t.name == "ui_action" {
                EFFECTS_DETAILS
            } else {
                EFFECTS_SEE_ALSO
            };
            t.details = Some(match t.details.take() {
                Some(d) => format!("{d}\n\n{fx}"),
                None => fx.to_string(),
            });
        }
        tools
    }

    async fn call(&self, name: &str, args: Value, ctx: &CallCtx) -> Envelope {
        // First, before the activity marker, the `expect` parse or a snapshot:
        // a refused call must not have touched anything, not even to look.
        if self.policy.foreground.is_never() {
            if let Some(refusal) = crate::foreground::tool_refusal(&*self.backend, name, &args) {
                return refusal.envelope(name);
            }
        }
        if self.policy.foreground.is_polite() {
            return self.call_politely(name, args, ctx).await;
        }
        self.run_call(name, args, ctx).await
    }

    /// Server stop: nothing may stay down once the process that pressed it is
    /// gone. Synchronous because it runs on shutdown paths, so the release is
    /// handed to the runtime and waited for a bounded time.
    fn shutdown(&self) {
        // The dispatcher holds a server that holds this module.
        if let Some(r) = &self.relay {
            r.release();
        }
        let keys = lock_ledger(&self.ledger).keys.take_all();
        if keys.is_empty() {
            return;
        }
        let backend = self.backend.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let release = async move {
            release_keys(&*backend, &keys).await;
            let _ = tx.send(());
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(release);
                // Two seconds is long for a key-up and short enough that a
                // wedged backend cannot hold the process open.
                let _ = rx.recv_timeout(std::time::Duration::from_secs(2));
            }
            Err(_) => {
                if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    rt.block_on(release);
                }
            }
        }
    }
}

/// What was read before an action, to compare with after it.
struct EffectState {
    facts: crate::effects::UiFacts,
    element: Option<(u64, crate::effects::ElementFacts)>,
}

/// Whether this call is the kind that reports `effects`: a shortcut that only
/// presses (holding or releasing a key changes nothing on its own) and a
/// pointer action that clicks (a move, a press or a release alone does not).
fn effects_apply(tool: &str, args: &Value) -> bool {
    match tool {
        "keyboard_shortcut" => matches!(shortcut_action(args), Ok(HoldAction::Press)),
        "mouse_action" => matches!(
            args.get("type")
                .and_then(Value::as_str)
                .map(snake)
                .as_deref(),
            Some("click" | "double" | "triple" | "right_click")
        ),
        _ => true,
    }
}

/// Run one step through the gate, watching for a cancel and the batch's time.
///
/// The call is not dropped when either happens: the dispatcher is asked to
/// stop and the step is awaited, so an action is never abandoned half done
/// (a key left down, a drag left pressed) and what it returns is reported.
async fn watch(
    dispatch: &dyn crate::steps::StepDispatch,
    step: &crate::steps::Step,
    ctx: &CallCtx,
    deadline: tokio::time::Instant,
) -> Envelope {
    let mut call = dispatch.dispatch(&step.tool, step.args.clone());
    let mut stopping = false;
    loop {
        match tokio::time::timeout(WATCH_EVERY, &mut call).await {
            Ok(env) => return env,
            Err(_) if !stopping => {
                if ctx.cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
                    stopping = true;
                    dispatch.cancel();
                }
            }
            Err(_) => {}
        }
    }
}

/// One step of a flow through the gate, bounded to a minute.
pub(crate) async fn watch_call(
    dispatch: &dyn crate::steps::StepDispatch,
    step: &crate::steps::Step,
    ctx: &CallCtx,
) -> Envelope {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    watch(dispatch, step, ctx, deadline).await
}

/// What `keyboard_shortcut` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoldAction {
    Press,
    Down,
    Up,
}

/// The `action` of a `keyboard_shortcut`: `press` unless it says otherwise.
fn shortcut_action(args: &Value) -> Result<HoldAction, String> {
    match args.get("action").filter(|v| !v.is_null()) {
        None => Ok(HoldAction::Press),
        Some(v) => match v.as_str().map(str::to_ascii_lowercase).as_deref() {
            Some("press") => Ok(HoldAction::Press),
            Some("down") => Ok(HoldAction::Down),
            Some("up") => Ok(HoldAction::Up),
            _ => Err("'action' must be press, down or up".into()),
        },
    }
}

fn input_err(tool: &str, e: InputError) -> Envelope {
    let (code, msg) = match e {
        InputError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
        InputError::NotFound(m) => (ErrorCode::StaleRef, m),
        InputError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
        InputError::InvalidArgs(m) => (ErrorCode::InvalidArgs, m),
        InputError::Failed(m) => (ErrorCode::ActionFailed, m),
        InputError::Refused(m) => (ErrorCode::PolicyDenied, m),
    };
    Envelope::fail(tool, code, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Choice, ClipData};

    fn apps() -> Vec<String> {
        default_terminal_apps()
    }

    /// The gap this closes: an editor's integrated terminal runs the same shell
    /// as Terminal.app, so `rm -rf` typed into VS Code's panel is exactly as
    /// destructive, but the old list stopped at dedicated terminal emulators.
    #[test]
    fn editors_with_integrated_terminals_are_screened() {
        for app in [
            "Code",
            "Visual Studio Code",
            "Cursor",
            "Zed",
            "Windsurf",
            "IntelliJ IDEA",
            "PyCharm",
            "Xcode",
            "Terminal",
            "iTerm2",
            "Ghostty",
        ] {
            assert!(
                screens_as_terminal(Some(app), &apps()),
                "{app} runs a shell and must be screened"
            );
        }
        for app in ["TextEdit", "Finder", "Safari", "Preview"] {
            assert!(
                !screens_as_terminal(Some(app), &apps()),
                "{app} is not a shell host"
            );
        }
    }

    /// An unknown destination must screen as a terminal. This is the actual
    /// bypass: apps with no accessibility tree yield no target, and treating
    /// "don't know" as "safe" waves the destructive command straight through.
    #[test]
    fn unknown_target_is_screened_not_waved_through() {
        assert!(screens_as_terminal(None, &apps()));
        assert!(
            screens_as_terminal(None, &[]),
            "an empty policy must not turn the check off"
        );
    }

    /// `mcp-policy` carries its own copy of the list because it sits below the
    /// engines; the two must not drift.
    #[test]
    fn policy_default_matches_the_engine_default() {
        assert_eq!(
            mcp_policy::PolicyConfig::default().terminal_apps,
            default_terminal_apps(),
            "policy and input disagree on which apps host a shell"
        );
    }

    #[test]
    fn modifiers_parse_and_reject() {
        let ok = parse_modifiers("t", &json!({ "modifiers": ["cmd", "shift"] })).unwrap();
        assert_eq!(ok, vec!["cmd".to_string(), "shift".to_string()]);
        assert!(parse_modifiers("t", &json!({})).unwrap().is_empty());

        // A silently-dropped modifier is worse than an error: the click still
        // happens, just not the one that was asked for.
        let bad = parse_modifiers("t", &json!({ "modifiers": ["hyper"] })).unwrap_err();
        assert_eq!(bad.error.unwrap().code, ErrorCode::InvalidArgs);
        assert!(parse_modifiers("t", &json!({ "modifiers": "cmd" })).is_err());
        assert!(parse_modifiers("t", &json!({ "modifiers": [1] })).is_err());
    }

    /// from, to, steps, hold_ms.
    type Drag = ((f64, f64), (f64, f64), u32, u64);

    #[derive(Default)]
    struct MockBackend {
        actions: std::sync::Mutex<Vec<(u64, SemanticAction, Option<String>)>>,
        values: std::sync::Mutex<Vec<(u64, String)>>,
        /// Every pointer move, in order: this is pacing logic under test, not a
        /// claim about what an OS does with them.
        moves: std::sync::Mutex<Vec<(f64, f64)>>,
        takeovers: std::sync::atomic::AtomicU64,
        /// Simulates a human grabbing the mouse while the call is already
        /// running: the takeover lands during the pointer read that starts a
        /// glide, after admission and before the first waypoint.
        takeover_during_pointer_read: std::sync::atomic::AtomicBool,
        /// Every key chord handed to the backend, as sent.
        keys: std::sync::Mutex<Vec<String>>,
        /// Every pointer event, in order, of any kind.
        pointer: std::sync::Mutex<Vec<(MouseKind, f64, f64)>>,
        drags: std::sync::Mutex<Vec<Drag>>,
        /// The full route of each drag, parallel to `drags`.
        routes: std::sync::Mutex<Vec<Vec<(f64, f64)>>>,
        /// Key presses and releases in order, as `down:shift` / `up:shift`.
        key_events: std::sync::Mutex<Vec<String>>,
        /// Makes the next `key_down` of this key fail, to exercise the cleanup.
        fail_key_down: std::sync::Mutex<Option<String>>,
        scrolls: std::sync::Mutex<Vec<(f64, f64, i32)>>,
        /// The modifiers handed to each scroll, in the same order as `scrolls`.
        scroll_modifiers: std::sync::Mutex<Vec<Vec<String>>>,
        /// Every pixel-scroll event handed to the backend, as sent.
        pixel_events: std::sync::Mutex<Vec<crate::trackpad::PixelScroll>>,
        /// Whether this backend says it understands momentum phases.
        momentum_ok: std::sync::atomic::AtomicBool,
        /// Cancels the token once this many pixel events have been posted.
        cancel_after_pixels: std::sync::Mutex<Option<(usize, mcp_types::CancelToken)>>,
        typed: std::sync::Mutex<Vec<String>>,
        /// What `read_element` returns per node, one entry per read; the last
        /// one repeats. Scripts the *tool layer's* judging of a read-back, not
        /// anything an OS does.
        readings: std::sync::Mutex<std::collections::HashMap<u64, Vec<Reading>>>,
        reads: std::sync::Mutex<std::collections::HashMap<u64, usize>>,
        /// What `choose_option` answers, when it should not just succeed.
        choice: std::sync::Mutex<Option<Result<Choice, InputError>>>,
        /// The route `perform` reports, when it should not be `Ax`. Scripts
        /// what the tool layer is told, not what an OS did.
        route: std::sync::Mutex<Option<Route>>,
        /// The pinned background app, when the tool layer should take the
        /// per-process route. Scripts what the backend reports, not what an
        /// OS does with the keys: that is measured in the live tests.
        background: std::sync::Mutex<Option<BackgroundTarget>>,
        /// What `read_focused_text` returns, one per read; the last repeats.
        focused: std::sync::Mutex<Vec<FocusedText>>,
        focused_reads: std::sync::atomic::AtomicUsize,
        pid_typed: std::sync::Mutex<Vec<(i32, String)>>,
        pid_keys: std::sync::Mutex<Vec<(i32, String)>>,
    }

    impl MockBackend {
        fn script(&self, node: u64, readings: Vec<Reading>) {
            self.readings.lock().unwrap().insert(node, readings);
        }
    }

    fn shown(v: &str) -> Reading {
        Reading {
            value: Some(v.into()),
            checked: None,
        }
    }

    fn checked(c: bool) -> Reading {
        Reading {
            value: None,
            checked: Some(c),
        }
    }

    #[async_trait]
    impl InputBackend for MockBackend {
        async fn choose_option(&self, node_id: u64, option: &str) -> Result<Choice, InputError> {
            self.actions.lock().unwrap().push((
                node_id,
                SemanticAction::Select,
                Some(option.to_string()),
            ));
            match self.choice.lock().unwrap().clone() {
                Some(r) => r,
                None => Ok(Choice {
                    item: option.to_string(),
                    selected: Some(option.to_string()),
                    changed: true,
                }),
            }
        }
        async fn read_element(&self, node_id: u64) -> Result<Reading, InputError> {
            let script = self.readings.lock().unwrap();
            let Some(list) = script.get(&node_id) else {
                return Ok(Reading::default());
            };
            let mut reads = self.reads.lock().unwrap();
            let n = reads.entry(node_id).or_insert(0);
            let r = list[(*n).min(list.len() - 1)].clone();
            *n += 1;
            Ok(r)
        }
        async fn perform(
            &self,
            node_id: u64,
            action: SemanticAction,
            option: Option<&str>,
        ) -> Result<Route, InputError> {
            self.actions
                .lock()
                .unwrap()
                .push((node_id, action, option.map(|s| s.to_string())));
            Ok(self.route.lock().unwrap().unwrap_or(Route::Ax))
        }
        fn route_for(&self, _: SemanticAction) -> Route {
            self.route.lock().unwrap().unwrap_or(Route::Ax)
        }
        async fn set_value(&self, node_id: u64, text: &str) -> Result<(), InputError> {
            self.values
                .lock()
                .unwrap()
                .push((node_id, text.to_string()));
            Ok(())
        }
        async fn type_text(&self, text: &str) -> Result<(), InputError> {
            self.typed.lock().unwrap().push(text.to_string());
            Ok(())
        }
        async fn key_combo(&self, combo: &str) -> Result<(), InputError> {
            self.keys.lock().unwrap().push(combo.to_string());
            Ok(())
        }
        fn background_key_target(&self) -> Option<BackgroundTarget> {
            self.background.lock().unwrap().clone()
        }
        async fn type_text_to_pid(&self, pid: i32, text: &str) -> Result<(), InputError> {
            self.pid_typed.lock().unwrap().push((pid, text.to_string()));
            Ok(())
        }
        async fn key_combo_to_pid(&self, pid: i32, combo: &str) -> Result<(), InputError> {
            self.pid_keys.lock().unwrap().push((pid, combo.to_string()));
            Ok(())
        }
        async fn read_focused_text(&self, _pid: i32) -> Result<FocusedText, InputError> {
            let script = self.focused.lock().unwrap();
            let n = self
                .focused_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            script
                .get(n.min(script.len().saturating_sub(1)))
                .cloned()
                .ok_or_else(|| InputError::Unsupported("no scripted read".into()))
        }
        async fn mouse(
            &self,
            kind: MouseKind,
            x: f64,
            y: f64,
            _button: Option<&str>,
            _modifiers: &[String],
        ) -> Result<(), InputError> {
            self.pointer.lock().unwrap().push((kind, x, y));
            if matches!(kind, MouseKind::Move) {
                self.moves.lock().unwrap().push((x, y));
            }
            Ok(())
        }
        async fn scroll_at(
            &self,
            x: f64,
            y: f64,
            _dir: ScrollDir,
            amount: i32,
            modifiers: &[String],
        ) -> Result<(), InputError> {
            self.scrolls.lock().unwrap().push((x, y, amount));
            self.scroll_modifiers
                .lock()
                .unwrap()
                .push(modifiers.to_vec());
            Ok(())
        }
        async fn scroll_pixels(
            &self,
            _x: f64,
            _y: f64,
            event: &crate::trackpad::PixelScroll,
            _modifiers: &[String],
        ) -> Result<(), InputError> {
            let mut events = self.pixel_events.lock().unwrap();
            events.push(*event);
            if let Some((n, token)) = &*self.cancel_after_pixels.lock().unwrap() {
                if events.len() == *n {
                    token.cancel();
                }
            }
            Ok(())
        }
        fn pixel_momentum(&self) -> bool {
            self.momentum_ok.load(std::sync::atomic::Ordering::SeqCst)
        }
        async fn hover(&self, _x: f64, _y: f64) -> Result<(), InputError> {
            Ok(())
        }
        async fn pointer_position(&self) -> Result<Option<(f64, f64)>, InputError> {
            if self
                .takeover_during_pointer_read
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                self.cancel_pending();
            }
            Ok(Some((0.0, 0.0)))
        }
        fn cancel_pending(&self) {
            self.takeovers
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn takeover_generation(&self) -> u64 {
            self.takeovers.load(std::sync::atomic::Ordering::SeqCst)
        }
        async fn drag(
            &self,
            from: (f64, f64),
            route: &[(f64, f64)],
            _modifiers: &[String],
            hold_ms: u64,
            _since_takeover: u64,
        ) -> Result<(), InputError> {
            let to = *route.last().expect("a drag route is never empty");
            self.drags
                .lock()
                .unwrap()
                .push((from, to, route.len() as u32, hold_ms));
            self.routes.lock().unwrap().push(route.to_vec());
            Ok(())
        }
        async fn key_down(&self, key: &str) -> Result<(), InputError> {
            if self.fail_key_down.lock().unwrap().as_deref() == Some(key) {
                return Err(InputError::Failed(format!("cannot press {key}")));
            }
            self.key_events.lock().unwrap().push(format!("down:{key}"));
            Ok(())
        }
        async fn key_up(&self, key: &str) -> Result<(), InputError> {
            self.key_events.lock().unwrap().push(format!("up:{key}"));
            Ok(())
        }
        async fn clipboard_read(&self, format: ClipFormat) -> Result<ClipData, InputError> {
            Ok(ClipData { format, data: None })
        }
        async fn clipboard_write(
            &self,
            _format: ClipFormat,
            _data: &str,
        ) -> Result<(), InputError> {
            Ok(())
        }
        fn input_target(&self) -> Option<String> {
            Some("TestApp".into())
        }
        fn platform(&self) -> &'static str {
            "mock"
        }
    }

    fn test_module() -> (InputModule, Arc<MockBackend>, Arc<Mutex<SnapshotArena>>) {
        let backend = Arc::new(MockBackend::default());
        let arena = Arc::new(Mutex::new(SnapshotArena::new()));
        let mut snap = mcp_a11y::Snapshot {
            id: "s1".into(),
            app: Some("Settings".into()),
            window: Some("Preferences".into()),
            elements: std::collections::HashMap::new(),
            skeleton: false,
        };
        snap.elements.insert(
            "@e1".into(),
            mcp_a11y::ElementInfo {
                role: "text_field".into(),
                name: Some("Username".into()),
                value_preview: None,
                secure: false,
                bounds: None,
                node_id: Some(101),
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        snap.elements.insert(
            "@e2".into(),
            mcp_a11y::ElementInfo {
                role: "secure_text_field".into(),
                name: Some("Password".into()),
                value_preview: None,
                secure: true,
                bounds: None,
                node_id: Some(102),
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        snap.elements.insert(
            "@e3".into(),
            mcp_a11y::ElementInfo {
                role: "checkbox".into(),
                name: Some("Enable Notifications".into()),
                value_preview: None,
                secure: false,
                bounds: None,
                node_id: Some(103),
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        snap.elements.insert(
            "@e4".into(),
            mcp_a11y::ElementInfo {
                role: "button".into(),
                name: Some("Save Changes".into()),
                value_preview: None,
                secure: false,
                bounds: None,
                node_id: Some(104),
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        snap.elements.insert(
            "@e5".into(),
            mcp_a11y::ElementInfo {
                role: "combobox".into(),
                name: Some("Theme".into()),
                value_preview: None,
                secure: false,
                bounds: None,
                node_id: Some(105),
                state: Default::default(),
                semantic_intent: None,
                bound_state: None,
            },
        );
        arena.lock().unwrap().install(snap);

        let policy = InputPolicy {
            bypass: true,
            ..InputPolicy::default()
        };
        let module = InputModule::new(backend.clone(), arena.clone(), policy);
        (module, backend, arena)
    }

    #[tokio::test]
    async fn ui_fill_form_batches_inputs_and_submits() {
        let (module, backend, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "ref": "@e1", "value": "alice" },
                    { "ref": "@e2", "value": "secret123", "secret": true },
                    { "ref": "@e3", "value": true }
                ],
                "submit": { "ref": "@e4" }
            }))
            .await;

        assert!(env.ok, "{env:?}");
        let data = env.data.unwrap();
        assert_eq!(data["filled"], 3);
        assert_eq!(data["submitted"], true);

        let values = backend.values.lock().unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], (101, "alice".into()));
        assert_eq!(values[1], (102, "secret123".into()));

        let actions = backend.actions.lock().unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0], (103, SemanticAction::Check, None));
        assert_eq!(actions[1], (104, SemanticAction::Click, None));

        let res = data["results"].as_array().unwrap();
        assert_eq!(res[1]["value"], "[REDACTED:len=9]");
    }

    #[tokio::test]
    async fn ui_fill_form_resolves_fields_by_name() {
        let (module, backend, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "name": "Username", "value": "bob" },
                    { "name": "Notifications", "value": false, "action": "uncheck" }
                ]
            }))
            .await;

        assert!(env.ok, "{env:?}");
        let data = env.data.unwrap();
        assert_eq!(data["filled"], 2);
        assert_eq!(data["submitted"], false);

        let values = backend.values.lock().unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], (101, "bob".into()));

        let actions = backend.actions.lock().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0], (103, SemanticAction::Uncheck, None));
    }

    #[tokio::test]
    async fn ui_fill_form_rejects_empty_fields() {
        let (module, _, _) = test_module();
        let env = module.ui_fill_form(&json!({ "fields": [] })).await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn ui_fill_form_stops_and_reports_missing_target() {
        let (module, backend, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "ref": "@e1", "value": "alice" },
                    { "ref": "@e999", "value": "nonexistent" }
                ],
                "submit": { "ref": "@e4" }
            }))
            .await;

        assert!(!env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["filled"], 1);

        let actions = backend.actions.lock().unwrap();
        assert_eq!(actions.len(), 0, "submission must not run if field fails");
    }

    #[tokio::test]
    async fn ui_fill_form_with_combobox_select_action() {
        let (module, backend, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "ref": "@e5", "action": "select", "option": "Dark Mode" }
                ]
            }))
            .await;

        assert!(env.ok, "{env:?}");
        let data = env.data.unwrap();
        assert_eq!(data["filled"], 1);
        let actions = backend.actions.lock().unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            (105, SemanticAction::Select, Some("Dark Mode".into()))
        );
    }

    #[tokio::test]
    async fn ui_fill_form_rejects_missing_value_and_action() {
        let (module, _, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "ref": "@e1" }
                ]
            }))
            .await;

        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn ui_fill_form_fails_on_missing_submit_target() {
        let (module, backend, _) = test_module();
        let env = module
            .ui_fill_form(&json!({
                "fields": [
                    { "ref": "@e1", "value": "alice" }
                ],
                "submit": { "ref": "@e999" }
            }))
            .await;

        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::NotFound);
        let values = backend.values.lock().unwrap();
        assert_eq!(values.len(), 1, "field was filled before submit failed");
    }

    #[tokio::test]
    async fn ui_fill_form_verifies_expect_clause_rejected_when_malformed() {
        let (module, backend, _) = test_module();
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let env = module
            .call(
                "ui_fill_form",
                json!({
                    "fields": [{ "ref": "@e1", "value": "alice" }],
                    "expect": "malformed_string"
                }),
                &ctx,
            )
            .await;

        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
        let values = backend.values.lock().unwrap();
        assert!(
            values.is_empty(),
            "action must not run if expect clause is malformed"
        );
    }

    #[tokio::test]
    async fn ui_fill_form_verifies_postcondition_unsupported_without_verifier() {
        let (module, backend, _) = test_module();
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let env = module
            .call(
                "ui_fill_form",
                json!({
                    "fields": [{ "ref": "@e1", "value": "alice" }],
                    "expect": { "text": "Saved" }
                }),
                &ctx,
            )
            .await;

        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::UnsupportedOs);
        let values = backend.values.lock().unwrap();
        assert!(
            values.is_empty(),
            "action must not run if verifier is missing"
        );
    }

    #[tokio::test]
    async fn test_input_showcase_toggle_and_presets() {
        let (module, _, _) = test_module();
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());

        // Initial default: disabled
        let env = module.call("input_showcase", json!({}), &ctx).await;
        assert!(env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["glide"]["enabled"], false);
        assert_eq!(data["glide"]["speed"], "instant");

        // Enable with cinematic preset
        let env = module
            .call(
                "input_showcase",
                json!({ "enabled": true, "speed": "cinematic" }),
                &ctx,
            )
            .await;
        assert!(env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["glide"]["enabled"], true);
        assert_eq!(data["glide"]["speed"], "cinematic");
        assert_eq!(data["glide"]["duration_ms"], 350);

        // Custom duration
        let env = module
            .call(
                "input_showcase",
                json!({ "speed": "demo", "duration_ms": 250 }),
                &ctx,
            )
            .await;
        assert!(env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["glide"]["speed"], "demo");
        assert_eq!(data["glide"]["duration_ms"], 250);
    }

    #[tokio::test]
    async fn test_mouse_action_and_hover_with_glide_flag() {
        let (module, _, _) = test_module();
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());

        let env = module
            .call(
                "mouse_action",
                json!({
                    "type": "click",
                    "x": 200.0,
                    "y": 300.0,
                    "glide": true,
                    "speed": "cinematic"
                }),
                &ctx,
            )
            .await;
        assert!(env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["glided"], true);

        let env_hover = module
            .call(
                "hover",
                json!({
                    "x": 400.0,
                    "y": 500.0,
                    "glide": true,
                    "speed": "demo"
                }),
                &ctx,
            )
            .await;
        assert!(env_hover.ok);
        let data_hover = env_hover.data.unwrap();
        assert_eq!(data_hover["glided"], true);
    }

    async fn move_to(module: &InputModule, extra: Value) -> Value {
        let mut args = json!({ "type": "move", "x": 200.0, "y": 300.0 });
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let env = module.call("mouse_action", args, &ctx).await;
        assert!(env.ok, "{env:?}");
        env.data.unwrap()
    }

    /// A takeover that lands after the call was admitted but before its first
    /// waypoint used to be wiped by the glide's own reset.
    #[tokio::test]
    async fn a_takeover_during_the_call_stops_the_glide_before_any_move() {
        let (module, backend, _) = test_module();
        let module = module.with_glide(crate::glide::GlideConfig::demo());
        backend
            .takeover_during_pointer_read
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let env = module
            .call(
                "mouse_action",
                json!({ "type": "move", "x": 400, "y": 300 }),
                &ctx,
            )
            .await;
        assert!(!env.ok, "{env:?}");
        assert!(env.error.unwrap().message.contains("human took over"));
        assert!(backend.moves.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_excessive_glide_duration_is_refused_not_echoed() {
        let (module, backend, _) = test_module();
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let env = module
            .call(
                "mouse_action",
                json!({ "type": "move", "x": 400, "y": 300, "glide": true, "duration_ms": 3_600_000 }),
                &ctx,
            )
            .await;
        assert!(!env.ok);
        assert_eq!(env.error.as_ref().unwrap().code, ErrorCode::InvalidArgs);
        assert!(env.error.unwrap().message.contains("2000"));
        assert!(backend.moves.lock().unwrap().is_empty());

        let env = module
            .call(
                "input_showcase",
                json!({ "enabled": true, "duration_ms": 2001 }),
                &ctx,
            )
            .await;
        assert!(!env.ok);
        // Refused as a whole: showcase is still off.
        let env = module.call("input_showcase", json!({}), &ctx).await;
        assert_eq!(env.data.unwrap()["glide"]["enabled"], false);

        let env = module
            .call("input_showcase", json!({ "duration_ms": 2000 }), &ctx)
            .await;
        assert!(env.ok);
    }

    #[tokio::test]
    async fn instant_move_is_one_move_and_not_glided() {
        let (module, backend, _) = test_module();
        let data = move_to(&module, json!({})).await;
        assert_eq!(data["glided"], false);
        assert_eq!(backend.moves.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn demo_move_glides_sixteen_waypoints_then_moves() {
        let (module, backend, _) = test_module();
        let module = module.with_glide(crate::glide::GlideConfig::demo());
        let data = move_to(&module, json!({})).await;
        assert_eq!(data["glided"], true);
        assert_eq!(backend.moves.lock().unwrap().len(), 17);
    }

    /// The bug: `glide:false` was overridden by a non-instant preset.
    #[tokio::test]
    async fn explicit_glide_false_wins_over_the_configured_preset() {
        let (module, backend, _) = test_module();
        let module = module.with_glide(crate::glide::GlideConfig::demo());
        let data = move_to(&module, json!({ "glide": false })).await;
        assert_eq!(data["glided"], false);
        assert_eq!(backend.moves.lock().unwrap().len(), 1);

        let data = move_to(&module, json!({ "glide": false, "speed": "cinematic" })).await;
        assert_eq!(data["glided"], false);
    }

    #[tokio::test]
    async fn glided_reports_waypoints_not_configuration() {
        let (module, backend, _) = test_module();
        // glide requested, but the preset is instant: nothing is emitted.
        let data = move_to(&module, json!({ "glide": true, "speed": "instant" })).await;
        assert_eq!(data["glided"], false);
        assert_eq!(backend.moves.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn speed_alone_turns_gliding_on() {
        let (module, _, _) = test_module();
        let data = move_to(&module, json!({ "speed": "snappy" })).await;
        assert_eq!(data["glided"], true);
    }

    // ---- keyboard_shortcut --------------------------------------------------

    async fn call(module: &InputModule, tool: &str, args: Value) -> Envelope {
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        module.call(tool, args, &ctx).await
    }

    /// The spellings a model reaches for first used to be refused outright.
    #[tokio::test]
    async fn shortcut_spellings_reach_the_backend_in_canonical_form() {
        let (module, backend, _) = test_module();
        for (given, sent) in [
            ("Control+A", "ctrl+a"),
            ("Cmd+Shift+Z", "cmd+shift+z"),
            ("ctrl-a", "ctrl+a"),
            ("Return", "return"),
            ("Esc", "escape"),
            ("PageUp", "pageup"),
            ("cmd++", "cmd+plus"),
            ("mod+c", "cmd+c"),
        ] {
            let env = call(&module, "keyboard_shortcut", json!({ "combo": given })).await;
            assert!(env.ok, "{given}: {env:?}");
            assert_eq!(env.data.unwrap()["pressed"], sent, "{given}");
            assert_eq!(backend.keys.lock().unwrap().last().unwrap(), sent);
        }
    }

    #[tokio::test]
    async fn two_keys_are_an_error_and_nothing_is_pressed() {
        let (module, backend, _) = test_module();
        let env = call(&module, "keyboard_shortcut", json!({ "combo": "a+b" })).await;
        assert!(!env.ok);
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("2 keys"), "{}", err.message);
        assert!(backend.keys.lock().unwrap().is_empty());

        let env = call(&module, "keyboard_shortcut", json!({ "combo": "cmd+nope" })).await;
        assert!(
            env.error.unwrap().message.contains("pageup"),
            "lists the keys"
        );
    }

    // ---- targeting ----------------------------------------------------------

    fn el(
        role: &str,
        name: &str,
        node: u64,
        at: Option<(f64, f64, f64, f64)>,
    ) -> mcp_a11y::ElementInfo {
        mcp_a11y::ElementInfo {
            role: role.into(),
            name: Some(name.into()),
            value_preview: None,
            secure: false,
            bounds: at.map(|(x, y, w, h)| Bounds { x, y, w, h }),
            node_id: Some(node),
            state: Default::default(),
            semantic_intent: None,
            bound_state: None,
        }
    }

    fn install(arena: &Arc<Mutex<SnapshotArena>>, items: Vec<(&str, mcp_a11y::ElementInfo)>) {
        arena.lock().unwrap().install(mcp_a11y::Snapshot {
            id: "s9".into(),
            app: Some("App".into()),
            window: None,
            elements: items.into_iter().map(|(r, i)| (r.to_string(), i)).collect(),
            skeleton: false,
        });
    }

    #[tokio::test]
    async fn a_ref_may_be_spelled_with_or_without_the_at_or_as_a_number() {
        let (module, backend, _) = test_module();
        for r in [json!("@e4"), json!("e4"), json!("4"), json!(4)] {
            let env = call(&module, "ui_action", json!({ "ref": r, "action": "click" })).await;
            assert!(env.ok, "{r}: {env:?}");
            assert_eq!(env.data.unwrap()["ref"], "@e4");
        }
        assert_eq!(backend.actions.lock().unwrap().len(), 4);
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "button", "action": "click" }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn a_name_resolves_and_the_result_says_which_ref_it_was() {
        let (module, backend, _) = test_module();
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "save changes", "action": "click" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let data = env.data.unwrap();
        assert_eq!(data["ref"], "@e4");
        assert_eq!(data["matches"], 1);
        assert_eq!(
            backend.actions.lock().unwrap()[0],
            (104, SemanticAction::Click, None)
        );
    }

    #[tokio::test]
    async fn the_better_name_wins_and_ties_are_reported() {
        let (module, backend, arena) = test_module();
        install(
            &arena,
            vec![
                ("@e1", el("button", "Save As…", 1, None)),
                ("@e2", el("button", "Save", 2, None)),
                ("@e3", el("button", "Save", 3, None)),
                ("@e4", el("link", "Save", 4, None)),
            ],
        );
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "Save", "action": "click" }),
        )
        .await;
        let data = env.data.unwrap();
        // Exact beats prefix; the exact ones tie, and document order breaks it.
        assert_eq!(data["ref"], "@e2");
        assert_eq!(data["matches"], 3, "e2, e3 and the link tie on name");
        assert_eq!(backend.actions.lock().unwrap()[0].0, 2);

        // The role narrows it, with synonyms.
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "Save", "role": "hyperlink", "action": "click" }),
        )
        .await;
        assert_eq!(env.data.unwrap()["ref"], "@e4");
    }

    /// The old lookup walked a HashMap: which of two equal matches was acted on
    /// changed from run to run.
    #[tokio::test]
    async fn equal_matches_resolve_to_document_order_every_time() {
        for _ in 0..10 {
            let (module, _, arena) = test_module();
            let items: Vec<(String, mcp_a11y::ElementInfo)> = (1..=30)
                .map(|n| (format!("@e{n}"), el("button", "OK", n, None)))
                .collect();
            install(
                &arena,
                items.iter().map(|(r, i)| (r.as_str(), i.clone())).collect(),
            );
            let env = call(
                &module,
                "ui_action",
                json!({ "name": "ok", "action": "click" }),
            )
            .await;
            assert_eq!(env.data.unwrap()["ref"], "@e1");
        }
    }

    #[tokio::test]
    async fn a_name_with_no_match_is_not_found_and_offers_candidates() {
        let (module, backend, arena) = test_module();
        install(
            &arena,
            vec![
                ("@e1", el("button", "Save Draft", 1, None)),
                ("@e2", el("button", "Cancel", 2, None)),
                ("@e3", el("link", "Save and Close", 3, None)),
            ],
        );
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "Save Document", "action": "click" }),
        )
        .await;
        assert!(!env.ok);
        let err = env.error.as_ref().unwrap();
        assert_eq!(err.code, ErrorCode::NotFound);
        let suggestion = err.suggestion.as_deref().unwrap();
        assert!(
            suggestion.contains("@e1 button \"Save Draft\""),
            "{suggestion}"
        );
        assert!(!suggestion.contains("Cancel"), "{suggestion}");
        let cands = env.data.unwrap()["candidates"].as_array().unwrap().clone();
        assert!(cands.len() <= 5 && cands.len() == 2);
        assert_eq!(cands[0]["ref"], "@e1");
        assert!(backend.actions.lock().unwrap().is_empty());
    }

    /// A miss in a nearly empty tree is where a custom-drawn app lands: the
    /// label is on screen, just not in the tree. The error says to read it off
    /// the screen, and nothing is clicked or captured on the model's behalf.
    #[tokio::test]
    async fn a_name_miss_in_a_sparse_tree_points_at_ocr() {
        let (module, backend, arena) = test_module();
        install(&arena, vec![("@e1", el("window", "Canvas", 1, None))]);
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "Save", "action": "click" }),
        )
        .await;
        let err = env.error.as_ref().unwrap();
        assert_eq!(err.code, ErrorCode::NotFound);
        let hint = err.suggestion.as_deref().unwrap();
        assert!(hint.contains("ocr_region with find \"Save\""), "{hint}");
        assert!(hint.contains("mouse_action"), "{hint}");
        assert!(backend.actions.lock().unwrap().is_empty());

        // A populated tree is not blamed on the app.
        let items: Vec<(String, mcp_a11y::ElementInfo)> = (1..=12)
            .map(|i| (format!("@e{i}"), el("button", "Other", i, None)))
            .collect();
        install(
            &arena,
            items.iter().map(|(r, i)| (r.as_str(), i.clone())).collect(),
        );
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "Save", "action": "click" }),
        )
        .await;
        let hint = env.error.as_ref().unwrap().suggestion.clone().unwrap();
        assert!(!hint.contains("ocr_region"), "{hint}");
    }

    /// An unobserved UI is observed on demand rather than refused, when the
    /// composition root wired a way to look.
    #[tokio::test]
    async fn naming_an_element_with_no_snapshot_takes_a_fresh_one() {
        use mcp_a11y::{RawSnapshot, UiNode};
        struct Fresh;
        #[async_trait]
        impl A11yBackend for Fresh {
            async fn snapshot(&self, _: &SnapshotRequest) -> Result<RawSnapshot, BackendError> {
                Ok(RawSnapshot {
                    root: UiNode {
                        role: "application".into(),
                        children: vec![UiNode {
                            role: "button".into(),
                            name: Some("Go".into()),
                            node_id: Some(77),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                    app: Some("Fresh".into()),
                    window: None,
                    terminal_app: false,
                    partial: false,
                    target: None,
                })
            }
            fn platform(&self) -> &'static str {
                "fake"
            }
        }
        let backend = Arc::new(MockBackend::default());
        let arena = Arc::new(Mutex::new(SnapshotArena::new()));
        let module = InputModule::new(backend.clone(), arena.clone(), InputPolicy::default())
            .with_observer(Arc::new(Fresh));
        let env = call(
            &module,
            "ui_action",
            json!({ "name": "go", "action": "click" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["ref"], "@e1");
        assert_eq!(backend.actions.lock().unwrap()[0].0, 77);
        assert!(arena.lock().unwrap().current().is_some());

        // Without an observer, the same call says what to do instead.
        let (bare, _, bare_arena) = test_module();
        *bare_arena.lock().unwrap() = SnapshotArena::new();
        let env = call(
            &bare,
            "ui_action",
            json!({ "name": "go", "action": "click" }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::StaleRef);
    }

    #[tokio::test]
    async fn fill_form_picks_the_same_field_every_time_and_lists_candidates() {
        let (module, backend, arena) = test_module();
        install(
            &arena,
            vec![
                ("@e1", el("textfield", "Name", 1, None)),
                ("@e2", el("textfield", "Name", 2, None)),
            ],
        );
        for _ in 0..5 {
            let env = call(
                &module,
                "ui_fill_form",
                json!({ "fields": [{ "name": "name", "value": "x" }] }),
            )
            .await;
            assert!(env.ok, "{env:?}");
            assert_eq!(env.data.unwrap()["results"][0]["matches"], 2);
        }
        assert!(backend.values.lock().unwrap().iter().all(|(n, _)| *n == 1));

        let env = call(
            &module,
            "ui_fill_form",
            json!({ "fields": [{ "name": "Nome", "value": "x" }, ] }),
        )
        .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::NotFound);
    }

    // ---- element-relative pointer -------------------------------------------

    fn with_button(arena: &Arc<Mutex<SnapshotArena>>) {
        install(
            arena,
            vec![(
                "@e1",
                el("button", "Target", 1, Some((100.0, 200.0, 60.0, 20.0))),
            )],
        );
    }

    #[tokio::test]
    async fn a_pointer_action_aims_at_the_centre_or_an_offset_from_the_corner() {
        let (module, backend, arena) = test_module();
        with_button(&arena);
        let at = |env: Envelope| {
            let d = env.data.unwrap();
            (
                d["at"]["x"].as_f64().unwrap(),
                d["at"]["y"].as_f64().unwrap(),
            )
        };
        let env = call(
            &module,
            "mouse_action",
            json!({ "type": "click", "ref": "@e1" }),
        )
        .await;
        assert_eq!(at(env), (130.0, 210.0));
        let env = call(
            &module,
            "mouse_action",
            json!({ "type": "click", "name": "target", "x": 5, "y": "3" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(at(env), (105.0, 203.0));
        // Only one axis given: the other stays centred.
        let env = call(&module, "hover", json!({ "ref": 1, "x": 10 })).await;
        assert_eq!(at(env), (110.0, 210.0));
        let log = backend.pointer.lock().unwrap();
        assert_eq!(log[0], (MouseKind::Click, 130.0, 210.0));
        assert_eq!(log[1], (MouseKind::Click, 105.0, 203.0));
    }

    #[tokio::test]
    async fn absolute_coordinates_still_work_and_numeric_strings_count() {
        let (module, backend, arena) = test_module();
        with_button(&arena);
        let env = call(
            &module,
            "mouse_action",
            json!({ "type": "click", "x": "120", "y": "205.5" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            backend.pointer.lock().unwrap()[0],
            (MouseKind::Click, 120.0, 205.5)
        );
        // A non-number is an error, not a missing argument.
        let env = call(
            &module,
            "mouse_action",
            json!({ "type": "click", "x": "left", "y": 1 }),
        )
        .await;
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(
            err.message.contains("'x' must be a number"),
            "{}",
            err.message
        );
        let env = call(&module, "mouse_action", json!({ "type": "click", "x": 5 })).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    /// An element-relative point is still a point: it goes through the same
    /// clamp as an absolute one.
    #[tokio::test]
    async fn clamp_input_applies_to_element_relative_points() {
        let backend = Arc::new(MockBackend::default());
        let arena = Arc::new(Mutex::new(SnapshotArena::new()));
        with_button(&arena);
        let module = InputModule::new(backend.clone(), arena, InputPolicy::default());
        let env = call(
            &module,
            "mouse_action",
            json!({ "type": "click", "ref": "@e1", "x": 5000, "y": 5 }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        let env = call(&module, "hover", json!({ "ref": "@e1", "x": -50, "y": 5 })).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        let env = call(
            &module,
            "drag_drop",
            json!({ "from": { "ref": "@e1" }, "to": { "ref": "@e1", "x": 9999 } }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        assert!(backend.pointer.lock().unwrap().is_empty());
        assert!(backend.drags.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn scroll_and_hover_accept_a_name_and_numeric_strings() {
        let (module, backend, arena) = test_module();
        with_button(&arena);
        let env = call(
            &module,
            "scroll",
            json!({ "name": "Target", "direction": "Page-Down", "amount": "4" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(backend.scrolls.lock().unwrap()[0], (130.0, 210.0, 4));
        assert_eq!(env.data.unwrap()["ref"], "@e1");
    }

    #[tokio::test]
    async fn pixel_scroll_posts_the_planned_phases_and_reports_what_was_sent() {
        use crate::trackpad::Phase;
        let (module, backend, _arena) = test_module();
        backend
            .momentum_ok
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let env = call(
            &module,
            "scroll",
            json!({ "x": 10, "y": 20, "dy": 300, "dx": -40, "duration_ms": 64, "momentum": true }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let sent = backend.pixel_events.lock().unwrap().clone();
        assert_eq!(sent.first().unwrap().phase, Phase::Began);
        assert_eq!(sent.last().unwrap().phase, Phase::MomentumEnded);
        assert!(sent.iter().any(|e| e.phase == Phase::Ended));
        let body: i32 = sent
            .iter()
            .filter(|e| !e.phase.is_momentum())
            .map(|e| e.dy)
            .sum();
        assert_eq!(body, 300);
        let data = env.data.unwrap();
        assert_eq!(data["dy"], 300);
        assert_eq!(data["dx"], -40);
        assert_eq!(data["events"], sent.len());
        assert_eq!(data["momentum"], true);
        assert!(data["momentum_dy"].as_i64().unwrap() > 0);
        assert_eq!(data["input"], "hid");
        // Plain wheel lines were not used.
        assert!(backend.scrolls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn momentum_a_backend_cannot_do_is_left_out_and_said_so() {
        let (module, backend, _arena) = test_module();
        let env = call(
            &module,
            "scroll",
            json!({ "x": 1, "y": 1, "dy": 200, "duration_ms": 32, "momentum": true }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert!(backend
            .pixel_events
            .lock()
            .unwrap()
            .iter()
            .all(|e| !e.phase.is_momentum()));
        let data = env.data.unwrap();
        assert_eq!(data["momentum"], false);
        assert!(data["note"].as_str().unwrap().contains("momentum"));
    }

    #[tokio::test]
    async fn pixel_scroll_refuses_bad_requests_before_touching_anything() {
        let (module, backend, _arena) = test_module();
        for args in [
            json!({ "x": 1, "y": 1, "dy": 20000 }),
            json!({ "x": 1, "y": 1, "dy": 10, "duration_ms": 99999 }),
            json!({ "x": 1, "y": 1, "dy": 0, "dx": 0 }),
            json!({ "x": 1, "y": 1, "dy": 10, "direction": "down" }),
            json!({ "x": 1, "y": 1, "dy": "lots" }),
        ] {
            let env = call(&module, "scroll", args.clone()).await;
            assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs, "{args}");
        }
        assert!(backend.pixel_events.lock().unwrap().is_empty());
    }

    /// A cancelled gesture must not leave the app with fingers still down.
    #[tokio::test]
    async fn a_cancelled_pixel_scroll_still_ends_the_gesture() {
        use crate::trackpad::Phase;
        let (module, backend, _arena) = test_module();
        let token = mcp_types::CancelToken::new();
        *backend.cancel_after_pixels.lock().unwrap() = Some((2, token.clone()));
        let ctx = CallCtx::new("t", token);
        let env = module
            .call(
                "scroll",
                json!({ "x": 1, "y": 1, "dy": 500, "duration_ms": 320 }),
                &ctx,
            )
            .await;
        assert!(!env.ok);
        let sent = backend.pixel_events.lock().unwrap().clone();
        assert_eq!(sent.len(), 3, "two events, then the closing one: {sent:?}");
        assert_eq!(sent.last().unwrap().phase, Phase::Ended);
        assert_eq!(sent.last().unwrap().dy, 0);
    }

    #[tokio::test]
    async fn pinch_rotate_and_swipe_are_unsupported_and_name_the_equivalent() {
        let (module, backend, _arena) = test_module();
        for (g, hint) in [
            ("pinch", "cmd+plus"),
            ("rotate", "rotate command"),
            ("swipe", "ctrl+left"),
        ] {
            let env = call(&module, "scroll", json!({ "gesture": g, "x": 1, "y": 1 })).await;
            let e = env.error.unwrap();
            assert_eq!(e.code, ErrorCode::UnsupportedOs);
            assert!(e.message.contains(hint), "{g}: {}", e.message);
        }
        let env = call(&module, "scroll", json!({ "gesture": "shake" })).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
        assert!(backend.pixel_events.lock().unwrap().is_empty());
    }

    /// Cmd+wheel is zoom in most apps, and `scroll` had no way to ask for it.
    /// The names are the ones `mouse_action` and `drag_drop` take, reach the
    /// backend as given, and come back in the result.
    #[tokio::test]
    async fn scroll_passes_modifiers_to_the_backend_and_reports_them() {
        let (module, backend, _arena) = test_module();
        let env = call(
            &module,
            "scroll",
            json!({ "x": 10, "y": 20, "direction": "down", "modifiers": ["cmd"] }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            backend.scroll_modifiers.lock().unwrap()[0],
            vec!["cmd".to_string()]
        );
        assert_eq!(env.data.unwrap()["modifiers"], json!(["cmd"]));

        // Without the field nothing is held, and the result says so.
        let env = call(
            &module,
            "scroll",
            json!({ "x": 10, "y": 20, "direction": "down" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert!(backend.scroll_modifiers.lock().unwrap()[1].is_empty());
        assert_eq!(env.data.unwrap()["modifiers"], json!([]));

        // An unknown name is refused before the wheel turns.
        let env = call(
            &module,
            "scroll",
            json!({ "x": 10, "y": 20, "direction": "down", "modifiers": ["hyper"] }),
        )
        .await;
        assert!(!env.ok);
        assert_eq!(env.error.as_ref().unwrap().code, ErrorCode::InvalidArgs);
        assert_eq!(backend.scrolls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn drag_drop_takes_objects_flat_keys_and_strings() {
        let (module, backend, arena) = test_module();
        install(
            &arena,
            vec![
                (
                    "@e1",
                    el("listitem", "Row A", 1, Some((0.0, 0.0, 100.0, 20.0))),
                ),
                (
                    "@e2",
                    el("listitem", "Row B", 2, Some((0.0, 40.0, 100.0, 20.0))),
                ),
            ],
        );
        let shapes = [
            json!({ "from": { "ref": "@e1" }, "to": { "name": "Row B" } }),
            json!({ "from_ref": "@e1", "to_name": "row b" }),
            json!({ "from": "@e1", "to": "Row B" }),
            json!({ "from": { "x": 50, "y": 10 }, "to": { "x": "50", "y": "50" } }),
            json!({ "from_x": 50, "from_y": 10, "to_x": 50, "to_y": 50 }),
        ];
        for shape in shapes {
            let env = call(&module, "drag_drop", shape.clone()).await;
            assert!(env.ok, "{shape}: {env:?}");
        }
        {
            let drags = backend.drags.lock().unwrap();
            assert_eq!(drags.len(), 5);
            for d in drags.iter() {
                assert_eq!((d.0, d.1), ((50.0, 10.0), (50.0, 50.0)));
            }
        }

        // Offsets from the corner, steps and hold as numeric strings.
        let env = call(
            &module,
            "drag_drop",
            json!({
                "from": { "ref": "@e1", "x": 5, "y": 5 }, "to": { "ref": "@e2", "x": 5, "y": 5 },
                "steps": "7", "hold_ms": "250",
            }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            *backend.drags.lock().unwrap().last().unwrap(),
            ((5.0, 5.0), (5.0, 45.0), 7, 250)
        );
        let d = env.data.unwrap();
        assert_eq!(d["from"]["ref"], "@e1");
        assert_eq!(d["hold_ms"], 250);

        // The hold is capped, and a missing end is named.
        let env = call(
            &module,
            "drag_drop",
            json!({ "from_x": 1, "from_y": 1, "to_x": 2, "to_y": 2, "hold_ms": 999999 }),
        )
        .await;
        assert_eq!(env.data.unwrap()["hold_ms"], MAX_HOLD_MS);
        let env = call(&module, "drag_drop", json!({ "from": { "x": 1, "y": 1 } })).await;
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("'to'"), "{}", err.message);
    }

    // ---- read-back ----------------------------------------------------------

    #[test]
    fn a_write_that_changed_nothing_and_is_not_what_was_asked_fails() {
        let e = judge_write(Some("old"), Some("old"), "new", false).unwrap_err();
        assert!(e.contains("kept \"old\""), "{e}");
        // The same, for a secret: the message never carries the value.
        let e = judge_write(Some("hunter2"), Some("hunter2"), "new", true).unwrap_err();
        assert!(!e.contains("hunter2"), "{e}");
    }

    #[test]
    fn a_write_that_landed_is_ok_even_if_nothing_needed_changing() {
        let d = judge_write(Some("same"), Some("same"), "same", false).unwrap();
        assert_eq!(d["changed"], false);
        assert_eq!(d["value_after"], "same");
        assert!(d.get("reformatted").is_none());
    }

    /// A number field turning 1000 into 1,000 is working, not rejecting.
    #[test]
    fn a_field_that_reformats_what_was_written_is_reported_not_failed() {
        let d = judge_write(Some("0"), Some("1,000"), "1000", false).unwrap();
        assert_eq!(d["changed"], true);
        assert_eq!(d["value_after"], "1,000");
        assert_eq!(d["reformatted"], true);
    }

    #[test]
    fn secrets_never_echo_the_value_and_unreadable_fields_are_unverified() {
        let d = judge_write(Some(""), Some("••••"), "hunter2", true).unwrap();
        assert!(d.get("value_after").is_none() && d.get("reformatted").is_none());
        assert_eq!(d["changed"], true);
        let d = judge_write(Some("a"), None, "b", false).unwrap();
        assert_eq!(d["verified"], false);
        assert!(d.get("value_after").is_none());
    }

    #[tokio::test]
    async fn set_value_fails_when_the_field_keeps_its_old_value() {
        let (module, backend, _) = test_module();
        backend.script(101, vec![shown("old")]);
        let env = call(&module, "set_value", json!({ "ref": "@e1", "text": "new" })).await;
        assert!(!env.ok);
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::ActionFailed);
        assert!(err.message.contains("kept \"old\""), "{}", err.message);
    }

    #[tokio::test]
    async fn set_value_reports_the_value_after_and_hides_a_secret() {
        let (module, backend, _) = test_module();
        backend.script(101, vec![shown("old"), shown("old"), shown("1,000")]);
        let env = call(
            &module,
            "set_value",
            json!({ "ref": "@e1", "text": "1000" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["value_after"], "1,000");
        assert_eq!(d["changed"], true);
        assert_eq!(d["ref"], "@e1");

        backend.script(102, vec![shown(""), shown("••••••")]);
        let env = call(
            &module,
            "set_value",
            json!({ "ref": "@e2", "text": "hunter2", "secret": true }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let text = serde_json::to_string(&env).unwrap();
        assert!(!text.contains("hunter2") && !text.contains("••"), "{text}");
    }

    #[tokio::test]
    async fn keyboard_type_with_a_target_reads_the_field_back() {
        let (module, backend, _) = test_module();
        backend.script(101, vec![shown("ab"), shown("abcd")]);
        let env = call(
            &module,
            "keyboard_type",
            json!({ "ref": "@e1", "text": "cd" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["value_after"], "abcd");
        assert_eq!(d["changed"], true);
        assert_eq!(d["typed"], 2);

        // A text field that shows exactly what it did before did not take it.
        backend.script(101, vec![shown("full")]);
        let env = call(
            &module,
            "keyboard_type",
            json!({ "ref": "@e1", "text": "x" }),
        )
        .await;
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::ActionFailed);
        assert!(err.message.contains("did not change"), "{}", err.message);

        // No target, nothing to read back.
        let env = call(&module, "keyboard_type", json!({ "text": "x" })).await;
        assert!(env.ok);
        assert!(env.data.unwrap().get("value_after").is_none());
    }

    // ---- per-process key delivery (route pid_event) -------------------------

    fn field(value: &str, caret: usize) -> FocusedText {
        FocusedText {
            value: Some(value.into()),
            selection: Some((caret, caret)),
            secure: false,
        }
    }

    /// A module over a mock whose pinned app is `app`, in the background, with
    /// the default (screening) policy: the destructive gate is on.
    fn background_module(app: &str, reads: Vec<FocusedText>) -> (InputModule, Arc<MockBackend>) {
        let backend = Arc::new(MockBackend::default());
        *backend.background.lock().unwrap() = Some(BackgroundTarget {
            pid: 4242,
            name: app.into(),
        });
        *backend.focused.lock().unwrap() = reads;
        let arena = Arc::new(Mutex::new(SnapshotArena::new()));
        let module = InputModule::new(backend.clone(), arena, InputPolicy::default());
        (module, backend)
    }

    #[tokio::test]
    async fn typing_at_a_background_app_goes_by_pid_and_is_read_back() {
        let (module, backend) = background_module(
            "Notes",
            vec![field("ab", 2), field("ab", 2), field("abcd", 4)],
        );
        let env = call(&module, "keyboard_type", json!({ "text": "cd" })).await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["input"], "pid_event");
        assert_eq!(d["verified"], true);
        assert_eq!(d["value_after"], "abcd");
        assert_eq!(d["background_app"], "Notes");
        assert_eq!(
            *backend.pid_typed.lock().unwrap(),
            vec![(4242, "cd".to_string())]
        );
        assert!(
            backend.typed.lock().unwrap().is_empty(),
            "nothing may go to the frontmost app"
        );
    }

    #[tokio::test]
    async fn typing_that_changed_nothing_is_an_error_and_never_falls_back_to_hid() {
        let (module, backend) = background_module("Notes", vec![field("ab", 2)]);
        let env = call(&module, "keyboard_type", json!({ "text": "cd" })).await;
        let err = env.error.expect("an error, not ok");
        assert_eq!(err.code, ErrorCode::ActionFailed);
        assert!(err.message.contains("did not change"), "{}", err.message);
        let hint = err.suggestion.unwrap_or_default();
        assert!(
            hint.contains("set_value") && hint.contains("focus_app"),
            "{hint}"
        );
        assert!(
            backend.typed.lock().unwrap().is_empty()
                && backend.key_events.lock().unwrap().is_empty(),
            "a failed delivery must not retype through the system event stream"
        );
    }

    #[tokio::test]
    async fn typing_into_something_unreadable_is_not_reported_as_delivered() {
        let (module, _) = background_module("Notes", vec![FocusedText::default()]);
        let env = call(&module, "keyboard_type", json!({ "text": "cd" })).await;
        let err = env.error.expect("unverified delivery is an error");
        assert!(err.message.contains("not known whether"), "{}", err.message);
        assert!(err.message.contains("may have"), "{}", err.message);

        let secure = FocusedText {
            secure: true,
            ..Default::default()
        };
        let (module, _) = background_module("Notes", vec![secure]);
        let env = call(
            &module,
            "keyboard_type",
            json!({ "text": "hunter2", "secret": true }),
        )
        .await;
        let err = env.error.expect("a password field cannot be read back");
        assert!(!err.message.contains("hunter2"));
    }

    #[tokio::test]
    async fn a_shortcut_at_a_background_app_says_when_it_could_not_be_verified() {
        let (module, backend) = background_module("Notes", vec![field("abc", 0)]);
        let env = call(&module, "keyboard_shortcut", json!({ "combo": "cmd+a" })).await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["input"], "pid_event");
        assert_eq!(d["verified"], false);
        assert!(d["note"].as_str().unwrap().contains("not known"));
        assert_eq!(
            *backend.pid_keys.lock().unwrap(),
            vec![(4242, "cmd+a".to_string())]
        );
        assert!(backend.keys.lock().unwrap().is_empty());

        let (module, _) = background_module("Notes", vec![field("abc", 0), field("abc", 3)]);
        let env = call(&module, "keyboard_shortcut", json!({ "combo": "End" })).await;
        let d = env.data.unwrap();
        assert_eq!(
            (d["verified"].clone(), d.get("note").is_none()),
            (json!(true), true)
        );
    }

    #[tokio::test]
    async fn keys_held_down_block_the_background_route_rather_than_being_dropped() {
        let (module, backend) = background_module("Notes", vec![field("ab", 2)]);
        for (tool, args) in [
            ("keyboard_type", json!({ "text": "x" })),
            ("keyboard_shortcut", json!({ "combo": "left" })),
        ] {
            // A failed input call releases whatever is held (that is the
            // contract for held keys), so shift is held afresh each time.
            let env = call(
                &module,
                "keyboard_shortcut",
                json!({ "combo": "shift", "action": "down" }),
            )
            .await;
            assert!(env.ok, "{env:?}");
            let env = call(&module, tool, args).await;
            let err = env.error.expect("refused while shift is held");
            assert!(err.message.contains("shift"), "{}", err.message);
        }
        assert!(backend.pid_typed.lock().unwrap().is_empty());
        assert!(backend.pid_keys.lock().unwrap().is_empty());
    }

    /// The destructive screen follows the app the keys are sent to. The mock's
    /// frontmost app is a harmless one; the pinned background app is Terminal.
    #[tokio::test]
    async fn a_destructive_command_at_a_background_terminal_meets_the_same_gate() {
        let (module, backend) = background_module("Terminal", vec![field("$ ", 2)]);
        let env = call(&module, "keyboard_type", json!({ "text": "rm -rf /" })).await;
        let err = env.error.expect("the gate must stop it");
        assert_eq!(err.code, ErrorCode::ConsentRequired, "{}", err.message);
        assert!(backend.pid_typed.lock().unwrap().is_empty());

        // Autonomous: a denial, as on the foreground route.
        let backend = Arc::new(MockBackend::default());
        *backend.background.lock().unwrap() = Some(BackgroundTarget {
            pid: 1,
            name: "iTerm2".into(),
        });
        let arena = Arc::new(Mutex::new(SnapshotArena::new()));
        let policy = InputPolicy {
            autonomous: true,
            ..InputPolicy::default()
        };
        let module = InputModule::new(backend.clone(), arena, policy);
        let env = call(&module, "keyboard_type", json!({ "text": "rm -rf /" })).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        assert!(backend.pid_typed.lock().unwrap().is_empty());

        // The same text at a background app that is not a terminal is let
        // through (it is only text there).
        let (module, backend) =
            background_module("Notes", vec![field("", 0), field("rm -rf /", 8)]);
        let env = call(&module, "keyboard_type", json!({ "text": "rm -rf /" })).await;
        assert!(env.ok, "{env:?}");
        assert_eq!(backend.pid_typed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn with_nothing_in_the_background_keys_still_take_the_ordinary_route() {
        let (module, backend, _) = test_module();
        let env = call(&module, "keyboard_type", json!({ "text": "x" })).await;
        assert_eq!(env.data.unwrap()["input"], "hid");
        assert_eq!(backend.typed.lock().unwrap().len(), 1);
        let env = call(&module, "keyboard_shortcut", json!({ "combo": "a" })).await;
        assert_eq!(env.data.unwrap()["input"], "hid");
    }

    #[tokio::test]
    async fn check_is_idempotent_and_verified() {
        let (module, backend, _) = test_module();
        // Already checked: asking for checked presses nothing.
        backend.script(103, vec![checked(true)]);
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e3", "action": "check" }),
        )
        .await;
        let d = env.data.unwrap();
        assert_eq!(
            (d["checked"].clone(), d["changed"].clone()),
            (json!(true), json!(false))
        );
        assert!(backend.actions.lock().unwrap().is_empty());

        // Unchecked, and the press takes.
        backend.script(103, vec![checked(false), checked(true)]);
        *backend.reads.lock().unwrap() = Default::default();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e3", "action": "check" }),
        )
        .await;
        let d = env.data.unwrap();
        assert_eq!(
            (d["checked"].clone(), d["changed"].clone()),
            (json!(true), json!(true))
        );
        assert_eq!(backend.actions.lock().unwrap().len(), 1);

        // Unchecked, and the press does nothing: an error, not an ok.
        backend.script(103, vec![checked(false)]);
        *backend.reads.lock().unwrap() = Default::default();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e3", "action": "check" }),
        )
        .await;
        assert!(!env.ok);
        assert!(env.error.unwrap().message.contains("still unchecked"));

        // Toggle flips whatever it read.
        backend.script(103, vec![checked(true), checked(false)]);
        *backend.reads.lock().unwrap() = Default::default();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e3", "action": "toggle" }),
        )
        .await;
        assert_eq!(env.data.unwrap()["checked"], false);
    }

    #[tokio::test]
    async fn a_backend_that_cannot_read_gets_the_plain_press() {
        let (module, backend, _) = test_module();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e3", "action": "uncheck" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            backend.actions.lock().unwrap()[0].1,
            SemanticAction::Uncheck
        );
    }

    // ---- choosing options ---------------------------------------------------

    #[tokio::test]
    async fn select_with_an_option_reports_what_the_control_shows() {
        let (module, backend, _) = test_module();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e5", "action": "select", "option": "Dark" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["selected"], "Dark");
        assert_eq!(d["changed"], true);
        assert_eq!(
            backend.actions.lock().unwrap()[0],
            (105, SemanticAction::Select, Some("Dark".into()))
        );
    }

    #[tokio::test]
    async fn a_failed_choice_surfaces_the_backends_reason() {
        let (module, backend, _) = test_module();
        *backend.choice.lock().unwrap() = Some(Err(InputError::InvalidArgs(
            "no option \"Blue\"; the options are: \"Light\", \"Dark\"".into(),
        )));
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e5", "action": "select", "option": "Blue" }),
        )
        .await;
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("\"Light\", \"Dark\""));
    }

    #[tokio::test]
    async fn select_without_an_option_still_just_presses() {
        let (module, backend, _) = test_module();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e5", "action": "select" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            backend.actions.lock().unwrap()[0],
            (105, SemanticAction::Select, None)
        );
        assert!(env.data.unwrap().get("selected").is_none());
    }

    /// Pointing at a menu item chooses it through the popup it belongs to, the
    /// way an HTML option resolves to its select.
    #[tokio::test]
    async fn selecting_a_menu_item_chooses_it_by_its_own_title() {
        let (module, backend, arena) = test_module();
        install(&arena, vec![("@e1", el("menuitem", "Rich Text", 9, None))]);
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e1", "action": "select" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["selected"], "Rich Text");
        assert_eq!(
            backend.actions.lock().unwrap()[0],
            (9, SemanticAction::Select, Some("Rich Text".into()))
        );

        // A menu-bar item has no popup: it falls back to the press it always was.
        *backend.choice.lock().unwrap() = Some(Err(InputError::Unsupported("no popup".into())));
        backend.actions.lock().unwrap().clear();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e1", "action": "select" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let acts = backend.actions.lock().unwrap();
        assert_eq!(acts.last().unwrap(), &(9, SemanticAction::Select, None));
    }

    // ---- modifiers and schemas ----------------------------------------------

    #[test]
    fn pointer_modifiers_are_case_insensitive() {
        let m = parse_modifiers("t", &json!({ "modifiers": ["Cmd", " SHIFT "] })).unwrap();
        assert_eq!(m, vec!["cmd".to_string(), "shift".to_string()]);
    }

    #[test]
    fn schemas_stay_in_the_gemini_safe_subset() {
        /// Schema keywords only: a property may be *named* `format`.
        fn banned(v: &Value, path: &str, hits: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    for (k, child) in m {
                        if path != "properties"
                            && matches!(k.as_str(), "pattern" | "format" | "additionalProperties")
                        {
                            hits.push(k.clone());
                        }
                        banned(child, k, hits);
                    }
                }
                Value::Array(a) => a.iter().for_each(|c| banned(c, "", hits)),
                _ => {}
            }
        }
        let (module, _, _) = test_module();
        for d in module.descriptors() {
            let mut hits = Vec::new();
            banned(&d.input_schema, "", &mut hits);
            assert!(hits.is_empty(), "{} uses {hits:?}", d.name);
        }
    }

    #[test]
    fn every_targeting_tool_declares_ref_name_and_role() {
        let (module, _, _) = test_module();
        let all = module.descriptors();
        for tool in [
            "ui_action",
            "set_value",
            "keyboard_type",
            "mouse_action",
            "scroll",
            "hover",
        ] {
            let d = all.iter().find(|d| d.name == tool).unwrap();
            for p in ["ref", "name", "role"] {
                assert!(
                    d.input_schema["properties"].get(p).is_some(),
                    "{tool} lacks {p}"
                );
            }
        }
        let fill = all.iter().find(|d| d.name == "ui_fill_form").unwrap();
        let item = &fill.input_schema["properties"]["fields"]["items"]["properties"];
        for p in ["ref", "name", "role", "option", "value", "action"] {
            assert!(item.get(p).is_some(), "fill_form field lacks {p}");
        }
        // The drag ends are declared, not bare objects.
        let drag = all.iter().find(|d| d.name == "drag_drop").unwrap();
        let from = &drag.input_schema["properties"]["from"]["properties"];
        for p in ["ref", "name", "x", "y"] {
            assert!(from.get(p).is_some(), "drag from lacks {p}");
        }
        for p in [
            "from_ref", "to_ref", "from_x", "from_y", "to_x", "to_y", "hold_ms",
        ] {
            assert!(
                drag.input_schema["properties"].get(p).is_some(),
                "drag lacks {p}"
            );
        }
    }

    /// The list is re-sent to the model on every turn; long prose belongs in
    /// the docs-only details, which `tools/list` omits.
    #[test]
    fn the_wire_text_stays_lean_and_the_prose_is_documented() {
        let (module, _, _) = test_module();
        let all = module.descriptors();
        let wire = |d: &ToolDescriptor| {
            d.name.len() + d.description.len() + d.input_schema.to_string().len()
        };
        for d in &all {
            assert!(
                wire(d) < 3_000,
                "{} is {} chars on the wire",
                d.name,
                wire(d)
            );
            assert!(d.description.len() < 400, "{} description is long", d.name);
        }
        let act = all.iter().find(|d| d.name == "ui_action").unwrap();
        assert!(act
            .details
            .as_deref()
            .is_some_and(|t| t.contains("reads back")));
        assert!(!act.description.contains("reads back"));
        let key = all.iter().find(|d| d.name == "keyboard_shortcut").unwrap();
        assert!(key
            .details
            .as_deref()
            .is_some_and(|t| t.contains("cmdorctrl")));
    }

    // ---- drag_drop path -----------------------------------------------------

    #[test]
    fn waypoints_are_the_path_shifted_into_the_frame_or_just_the_end() {
        assert_eq!(
            drag_waypoints(Some((9.0, 9.0)), &[], (100.0, 200.0)),
            [(9.0, 9.0)]
        );
        assert_eq!(
            drag_waypoints(None, &[(1.0, 2.0), (3.0, 4.0)], (100.0, 200.0)),
            [(101.0, 202.0), (103.0, 204.0)]
        );
        assert_eq!(
            drag_waypoints(None, &[(1.0, 2.0)], (0.0, 0.0)),
            [(1.0, 2.0)]
        );
    }

    #[tokio::test]
    async fn a_drag_path_passes_every_waypoint_in_order_and_reports_them() {
        let (module, backend, arena) = test_module();
        let way = [(110.0, 10.0), (110.0, 110.0), (10.0, 110.0)];
        let env = call(
            &module,
            "drag_drop",
            json!({
                "from": { "x": 10, "y": 10 },
                "path": [{ "x": 110, "y": 10 }, [110, 110], { "x": "10", "y": "110" }],
                "steps": 12,
            }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["waypoints"], 3);
        assert_eq!(d["steps"], 12);
        assert_eq!(d["to"]["x"], 10.0);
        assert_eq!(d["to"]["y"], 110.0);
        {
            let routes = backend.routes.lock().unwrap();
            let route = &routes[0];
            let hits: Vec<_> = way
                .iter()
                .map(|w| {
                    route
                        .iter()
                        .position(|p| p == w)
                        .expect("waypoint landed on")
                })
                .collect();
            assert!(hits.windows(2).all(|h| h[0] < h[1]), "in order: {hits:?}");
            assert_eq!(route.last(), Some(&(10.0, 110.0)), "released at the last");
        }

        // Named element: waypoints are offsets from its top-left, like `from`.
        install(
            &arena,
            vec![(
                "@e1",
                el("canvas", "Canvas", 1, Some((100.0, 200.0, 300.0, 300.0))),
            )],
        );
        let env = call(
            &module,
            "drag_drop",
            json!({ "from": { "ref": "@e1", "x": 5, "y": 5 }, "path": [[20, 30], [40, 60]] }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let drags = backend.drags.lock().unwrap();
        assert_eq!((drags[1].0, drags[1].1), ((105.0, 205.0), (140.0, 260.0)));
        assert!(backend.routes.lock().unwrap()[1].contains(&(120.0, 230.0)));
    }

    #[tokio::test]
    async fn a_drag_without_a_path_reports_no_waypoints() {
        let (module, _, _) = test_module();
        let env = call(
            &module,
            "drag_drop",
            json!({ "from": { "x": 1, "y": 1 }, "to": { "x": 2, "y": 2 } }),
        )
        .await;
        assert!(env.data.unwrap().get("waypoints").is_none());
    }

    #[tokio::test]
    async fn a_bad_path_is_refused_before_anything_moves() {
        let (module, backend, _) = test_module();
        let from = json!({ "x": 1, "y": 1 });
        let too_many: Vec<_> = (0..201).map(|i| json!({ "x": i, "y": 0 })).collect();
        for (why, args) in [
            ("empty", json!({ "from": from, "path": [] })),
            ("not points", json!({ "from": from, "path": [{ "x": 1 }] })),
            ("too many", json!({ "from": from, "path": too_many })),
            (
                "a second destination",
                json!({ "from": from, "path": [{ "x": 5, "y": 5 }], "to": { "x": 9, "y": 9 } }),
            ),
            (
                "a flat second destination",
                json!({ "from": from, "path": [{ "x": 5, "y": 5 }], "to_x": 9 }),
            ),
        ] {
            let env = call(&module, "drag_drop", args).await;
            assert_eq!(env.error.expect(why).code, ErrorCode::InvalidArgs, "{why}");
        }
        assert!(backend.drags.lock().unwrap().is_empty());
    }

    /// Every waypoint goes through the same bounds check as the endpoints, not
    /// only the last one: a stroke that wanders out of the window is refused
    /// whole, before the button goes down.
    #[tokio::test]
    async fn a_waypoint_outside_the_allowed_bounds_denies_the_whole_drag() {
        let backend = Arc::new(MockBackend::default());
        let arena = Arc::new(Mutex::new(SnapshotArena::new()));
        with_button(&arena);
        let module = InputModule::new(backend.clone(), arena, InputPolicy::default());
        let env = call(
            &module,
            "drag_drop",
            json!({ "from": { "ref": "@e1" }, "path": [[5, 5], [9999, 5], [10, 10]] }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        assert!(backend.drags.lock().unwrap().is_empty());
        let env = call(
            &module,
            "drag_drop",
            json!({ "from": { "ref": "@e1" }, "path": [[5, 5], [10, 10]] }),
        )
        .await;
        assert!(env.ok, "{env:?}");
    }

    // ---- held keys ----------------------------------------------------------

    fn events(b: &MockBackend) -> Vec<String> {
        b.key_events.lock().unwrap().clone()
    }

    async fn until(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..60 {
            if done() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        done()
    }

    fn bare(backend: &Arc<MockBackend>) -> InputModule {
        let arena = Arc::new(Mutex::new(SnapshotArena::new()));
        InputModule::new(backend.clone(), arena, InputPolicy::default())
    }

    #[test]
    fn what_ends_a_hold_is_decided_in_one_place() {
        assert_eq!(release_reason(false, false), None);
        assert!(release_reason(true, false).unwrap().contains("kill switch"));
        assert!(release_reason(false, true).unwrap().contains("human"));
        assert!(release_reason(true, true).is_some());
    }

    #[test]
    fn the_shortcut_action_defaults_to_press() {
        assert_eq!(shortcut_action(&json!({})), Ok(HoldAction::Press));
        assert_eq!(
            shortcut_action(&json!({ "action": null })),
            Ok(HoldAction::Press)
        );
        assert_eq!(
            shortcut_action(&json!({ "action": "Down" })),
            Ok(HoldAction::Down)
        );
        assert_eq!(
            shortcut_action(&json!({ "action": "up" })),
            Ok(HoldAction::Up)
        );
        assert!(shortcut_action(&json!({ "action": "hold" })).is_err());
        assert!(shortcut_action(&json!({ "action": 3 })).is_err());
    }

    #[tokio::test]
    async fn a_key_is_held_across_calls_until_it_is_released() {
        let backend = Arc::new(MockBackend::default());
        let module = bare(&backend);
        let env = call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "Shift", "action": "down" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let d = env.data.unwrap();
        assert_eq!(d["held"], json!(["shift"]));
        assert_eq!(d["auto_release_ms"], 30_000);

        // Another call in between does not disturb it; a chord still presses.
        let env = call(&module, "keyboard_shortcut", json!({ "combo": "cmd+s" })).await;
        assert!(env.ok);
        assert_eq!(*backend.keys.lock().unwrap(), ["cmd+s"]);

        // Re-sending a held key presses nothing new.
        let env = call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "cmd+shift", "action": "down" }),
        )
        .await;
        assert_eq!(env.data.as_ref().unwrap()["pressed"], json!(["cmd"]));
        assert_eq!(env.data.unwrap()["held"], json!(["shift", "cmd"]));
        assert_eq!(events(&backend), ["down:shift", "down:cmd"]);

        let env = call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "shift", "action": "up" }),
        )
        .await;
        let d = env.data.unwrap();
        assert_eq!(
            (&d["released"], &d["held"]),
            (&json!(["shift"]), &json!(["cmd"]))
        );
        let env = call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "shift", "action": "up" }),
        )
        .await;
        assert_eq!(env.data.unwrap()["not_held"], json!(["shift"]));
        assert_eq!(events(&backend), ["down:shift", "down:cmd", "up:shift"]);

        // No combo: everything still held.
        let env = call(&module, "keyboard_shortcut", json!({ "action": "up" })).await;
        assert_eq!(env.data.unwrap()["released"], json!(["cmd"]));
        assert_eq!(events(&backend).last().unwrap(), "up:cmd");
        let env = call(&module, "keyboard_shortcut", json!({ "action": "up" })).await;
        assert_eq!(env.data.unwrap()["released"], json!([]));
    }

    #[tokio::test]
    async fn an_unknown_key_or_action_presses_nothing() {
        let backend = Arc::new(MockBackend::default());
        let module = bare(&backend);
        for args in [
            json!({ "combo": "shift+nope", "action": "down" }),
            json!({ "combo": "a+b", "action": "down" }),
            json!({ "action": "down" }),
            json!({ "combo": "shift", "action": "hold" }),
        ] {
            let env = call(&module, "keyboard_shortcut", args.clone()).await;
            assert_eq!(
                env.error.expect("refused").code,
                ErrorCode::InvalidArgs,
                "{args}"
            );
        }
        let env = call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "nope", "action": "up" }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
        assert!(events(&backend).is_empty());
    }

    #[tokio::test]
    async fn no_more_than_the_cap_may_be_held() {
        let backend = Arc::new(MockBackend::default());
        let module = bare(&backend);
        for k in ["a", "b", "c", "d", "e", "f"] {
            let env = call(
                &module,
                "keyboard_shortcut",
                json!({ "combo": k, "action": "down" }),
            )
            .await;
            assert!(env.ok, "{k}: {env:?}");
        }
        let env = call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "g", "action": "down" }),
        )
        .await;
        let err = env.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("at most 6"), "{}", err.message);
        assert!(!events(&backend).contains(&"down:g".to_string()));
        // The refused call did not release what was already held.
        assert_eq!(events(&backend).len(), 6);
        call(&module, "keyboard_shortcut", json!({ "action": "up" })).await;
    }

    #[tokio::test]
    async fn a_key_that_fails_mid_sequence_releases_the_ones_before_it() {
        let backend = Arc::new(MockBackend::default());
        *backend.fail_key_down.lock().unwrap() = Some("b".into());
        let module = bare(&backend);
        let env = call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "cmd+b", "action": "down" }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::ActionFailed);
        assert_eq!(events(&backend), ["down:cmd", "up:b", "up:cmd"]);
        let env = call(&module, "keyboard_shortcut", json!({ "action": "up" })).await;
        assert_eq!(
            env.data.unwrap()["released"],
            json!([]),
            "nothing left held"
        );
    }

    #[tokio::test]
    async fn a_failed_call_releases_what_is_held_and_says_so() {
        let backend = Arc::new(MockBackend::default());
        let module = bare(&backend);
        call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "shift", "action": "down" }),
        )
        .await;
        let env = call(&module, "mouse_action", json!({ "type": "nonsense" })).await;
        let err = env.error.unwrap();
        assert!(
            err.message.contains("held keys released: shift"),
            "{}",
            err.message
        );
        assert_eq!(events(&backend), ["down:shift", "up:shift"]);
        // A failure while nothing is held adds nothing.
        let env = call(&module, "mouse_action", json!({ "type": "nonsense" })).await;
        assert!(!env.error.unwrap().message.contains("released"));
    }

    #[tokio::test]
    async fn a_human_taking_the_pointer_releases_held_keys() {
        let backend = Arc::new(MockBackend::default());
        let module = bare(&backend);
        call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "shift+a", "action": "down" }),
        )
        .await;
        backend.cancel_pending();
        assert!(
            until(|| events(&backend).len() == 4).await,
            "{:?}",
            events(&backend)
        );
        assert_eq!(events(&backend)[2..], ["up:a", "up:shift"]);
    }

    #[tokio::test]
    async fn the_kill_switch_releases_held_keys_though_every_call_is_refused() {
        let backend = Arc::new(MockBackend::default());
        let tripped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = tripped.clone();
        let module = bare(&backend).with_halt_probe(Arc::new(move || {
            flag.load(std::sync::atomic::Ordering::SeqCst)
        }));
        call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "shift", "action": "down" }),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert_eq!(events(&backend), ["down:shift"], "no trip, no release");
        tripped.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(until(|| events(&backend).len() == 2).await);
        assert_eq!(events(&backend)[1], "up:shift");
    }

    #[tokio::test]
    async fn a_key_is_released_when_its_time_is_up() {
        let backend = Arc::new(MockBackend::default());
        let module = bare(&backend).with_hold_limit(150);
        let env = call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "shift", "action": "down" }),
        )
        .await;
        assert_eq!(env.data.unwrap()["auto_release_ms"], 150);
        assert!(until(|| events(&backend).len() == 2).await);
        assert_eq!(events(&backend)[1], "up:shift");
        let env = call(&module, "keyboard_shortcut", json!({ "action": "up" })).await;
        assert_eq!(env.data.unwrap()["released"], json!([]));
        // The watchdog ends with the last key, and the next hold starts a new one.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "cmd", "action": "down" }),
        )
        .await;
        assert!(until(|| events(&backend).len() == 4).await);
        assert_eq!(events(&backend)[3], "up:cmd");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stopping_the_server_releases_held_keys() {
        let backend = Arc::new(MockBackend::default());
        let module = bare(&backend);
        call(
            &module,
            "keyboard_shortcut",
            json!({ "combo": "cmd+shift", "action": "down" }),
        )
        .await;
        module.shutdown();
        assert_eq!(events(&backend)[2..], ["up:shift", "up:cmd"]);
        // Idempotent.
        module.shutdown();
        assert_eq!(events(&backend).len(), 4);
    }

    #[test]
    fn the_shortcut_descriptor_offers_hold_and_release_within_the_budget() {
        let (module, _, _) = test_module();
        let all = module.descriptors();
        let key = all.iter().find(|d| d.name == "keyboard_shortcut").unwrap();
        assert_eq!(
            key.input_schema["properties"]["action"]["enum"],
            json!(["press", "down", "up"])
        );
        assert_eq!(key.input_schema["required"], json!([]));
        let drag = all.iter().find(|d| d.name == "drag_drop").unwrap();
        assert!(drag.input_schema["properties"]["path"]["description"]
            .as_str()
            .is_some_and(|d| d.contains("200")));
    }

    // ---- input route, new actions, text selection ---------------------------

    /// The route is whatever the backend reports, not something the tool
    /// layer decides: a backend that fell back to the pointer says so.
    #[tokio::test]
    async fn ui_action_reports_the_route_the_backend_took() {
        let (module, backend, _) = test_module();
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e4", "action": "click" }),
        )
        .await;
        assert_eq!(env.data.unwrap()["input"], "ax");

        *backend.route.lock().unwrap() = Some(Route::Hid);
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e4", "action": "right_click" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["input"], "hid");
    }

    /// A ref belongs to one application, which need not be the frontmost one,
    /// so a result says which it acted in.
    #[tokio::test]
    async fn an_action_by_ref_says_which_app_the_ref_belongs_to() {
        let (module, _, _) = test_module();
        let by_ref = call(
            &module,
            "ui_action",
            json!({ "ref": "@e4", "action": "click" }),
        )
        .await;
        let app = by_ref.data.unwrap()["app"].clone();
        assert_eq!(app, "Settings");
        let by_set = call(
            &module,
            "set_value",
            json!({ "ref": "@e4", "text": "hello" }),
        )
        .await;
        assert_eq!(by_set.data.unwrap_or_default()["app"], app);
    }

    #[tokio::test]
    async fn the_new_actions_reach_the_backend_under_their_own_names() {
        let (module, backend, _) = test_module();
        for (name, want) in [
            ("show_menu", SemanticAction::ShowMenu),
            ("increment", SemanticAction::Increment),
            ("decrement", SemanticAction::Decrement),
            ("pick", SemanticAction::Pick),
            ("scroll_to_visible", SemanticAction::ScrollIntoView),
            ("Show-Menu", SemanticAction::ShowMenu),
        ] {
            let env = call(
                &module,
                "ui_action",
                json!({ "ref": "@e4", "action": name }),
            )
            .await;
            assert!(env.ok, "{name}: {env:?}");
            assert_eq!(backend.actions.lock().unwrap().last().unwrap().1, want);
        }
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e4", "action": "squeeze" }),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn a_step_reports_the_value_and_whether_it_moved() {
        let (module, backend, _) = test_module();
        backend.script(104, vec![shown("3"), shown("4")]);
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e4", "action": "increment" }),
        )
        .await;
        let d = env.data.unwrap();
        assert_eq!(
            (d["value_after"].as_str(), d["changed"].as_bool()),
            (Some("4"), Some(true))
        );

        backend.script(104, vec![shown("10")]);
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e4", "action": "increment" }),
        )
        .await;
        let d = env.data.unwrap();
        assert_eq!(
            d["changed"], false,
            "a step that moved nothing is not a change"
        );
        assert_eq!(d["ok"], true);

        // A control that cannot be read back says so rather than claiming a move.
        backend.script(104, vec![Reading::default()]);
        let env = call(
            &module,
            "ui_action",
            json!({ "ref": "@e4", "action": "decrement" }),
        )
        .await;
        let d = env.data.unwrap();
        assert_eq!(d["verified"], false);
        assert!(d.get("changed").is_none());
    }

    #[tokio::test]
    async fn set_value_is_accessibility_and_pointer_tools_are_not() {
        let (module, backend, _) = test_module();
        backend.script(101, vec![shown(""), shown("hi")]);
        let env = call(&module, "set_value", json!({ "ref": "@e1", "text": "hi" })).await;
        assert_eq!(env.data.unwrap()["input"], "ax");

        for (tool, args) in [
            ("mouse_action", json!({ "type": "click", "x": 5, "y": 6 })),
            ("scroll", json!({ "x": 5, "y": 6, "direction": "down" })),
            ("hover", json!({ "x": 5, "y": 6 })),
            (
                "drag_drop",
                json!({ "from": { "x": 1, "y": 1 }, "to": { "x": 9, "y": 9 } }),
            ),
            ("keyboard_shortcut", json!({ "combo": "cmd+s" })),
            ("keyboard_type", json!({ "text": "x" })),
        ] {
            let env = call(&module, tool, args).await;
            assert!(env.ok, "{tool}: {env:?}");
            assert_eq!(env.data.unwrap()["input"], "hid", "{tool}");
        }
    }

    #[tokio::test]
    async fn keyboard_type_into_an_element_is_hid_even_though_it_focused_through_ax() {
        let (module, backend, _) = test_module();
        backend.script(101, vec![shown(""), shown("x")]);
        let env = call(
            &module,
            "keyboard_type",
            json!({ "ref": "@e1", "text": "x" }),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.unwrap()["input"], "hid");
    }

    #[test]
    fn a_range_is_one_of_four_shapes() {
        let p = |v: Value| parse_range(&v);
        assert_eq!(p(json!({ "all": true })), Ok(RangeSpec::All));
        assert_eq!(
            p(json!({ "start": 2, "end": "5" })),
            Ok(RangeSpec::Offsets { start: 2, end: 5 })
        );
        assert_eq!(
            p(json!({ "text": "cat" })),
            Ok(RangeSpec::Text {
                needle: "cat".into(),
                occurrence: 1
            })
        );
        assert_eq!(
            p(json!({ "text": "cat", "occurrence": 3 })),
            Ok(RangeSpec::Text {
                needle: "cat".into(),
                occurrence: 3
            })
        );
        assert_eq!(
            p(json!({ "from_text": "a", "to_text": "b" })),
            Ok(RangeSpec::Between {
                from: "a".into(),
                to: "b".into()
            })
        );
        // `all: false` is not a choice.
        assert!(p(json!({ "all": false })).is_err());
    }

    #[test]
    fn a_range_that_says_two_things_or_half_of_one_is_refused() {
        let bad = |v: Value| parse_range(&v).unwrap_err();
        assert!(bad(json!({})).contains("nothing to select"));
        assert!(bad(json!({ "all": true, "text": "x" })).contains("more than one"));
        assert!(bad(json!({ "start": 1, "text": "x" })).contains("more than one"));
        assert!(bad(json!({ "start": 1 })).contains("go together"));
        assert!(bad(json!({ "end": 1 })).contains("go together"));
        assert!(bad(json!({ "from_text": "a" })).contains("go together"));
        assert!(bad(json!({ "occurrence": 2, "all": true })).contains("goes with 'text'"));
        assert!(bad(json!({ "start": -1, "end": 2 })).contains("whole number"));
        assert!(bad(json!({ "start": 1.5, "end": 2 })).contains("whole number"));
        // An empty string is no string.
        assert!(bad(json!({ "text": "" })).contains("nothing to select"));
    }

    #[test]
    fn a_selection_result_carries_the_range_the_route_and_a_clipped_text() {
        let d = selection_data(&Selection {
            start: 2,
            end: 6,
            text: "word".into(),
            role: Some("TextArea".into()),
        });
        assert_eq!(
            d,
            json!({ "ok": true, "input": "ax", "start": 2, "end": 6, "length": 4,
                    "text": "word", "role": "TextArea" })
        );
        let long = selection_data(&Selection {
            start: 0,
            end: 5000,
            text: "x".repeat(5000),
            role: None,
        });
        assert_eq!(long["truncated"], true);
        assert_eq!(
            long["text"].as_str().unwrap().len(),
            crate::selection::ECHO_LIMIT
        );
        assert!(long.get("role").is_none());
    }

    #[tokio::test]
    async fn select_text_validates_before_it_touches_anything() {
        let (module, backend, _) = test_module();
        let env = call(&module, "select_text", json!({ "ref": "@e1" })).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
        let env = call(&module, "select_text", json!({ "all": true })).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::InvalidArgs);
        assert!(backend.actions.lock().unwrap().is_empty());

        // A password field is refused whatever the range.
        let env = call(&module, "select_text", json!({ "ref": "@e2", "all": true })).await;
        assert!(env.error.unwrap().message.contains("password"));
        let env = call(&module, "get_selection", json!({ "ref": "@e2" })).await;
        assert!(env.error.unwrap().message.contains("password"));
    }

    /// A range that is not in the text is the caller's mistake and is told so
    /// before the element is asked to select anything.
    #[tokio::test]
    async fn select_text_names_a_range_the_text_does_not_have() {
        let (module, backend, _) = test_module();
        backend.script(101, vec![shown("hello world")]);
        let env = call(
            &module,
            "select_text",
            json!({ "ref": "@e1", "text": "mars" }),
        )
        .await;
        let e = env.error.unwrap();
        assert_eq!(e.code, ErrorCode::InvalidArgs);
        assert!(e.message.contains("not in the text"), "{}", e.message);
        let env = call(
            &module,
            "select_text",
            json!({ "ref": "@e1", "start": 0, "end": 99 }),
        )
        .await;
        assert!(env.error.unwrap().message.contains("11 characters"));
    }

    /// With no selection support in the backend the answer is the platform's
    /// refusal, never an ok.
    #[tokio::test]
    async fn select_text_without_backend_support_is_an_error_not_an_ok() {
        let (module, backend, _) = test_module();
        backend.script(101, vec![shown("hello world")]);
        let env = call(
            &module,
            "select_text",
            json!({ "ref": "@e1", "text": "world" }),
        )
        .await;
        assert!(!env.ok);
        assert_eq!(env.error.unwrap().code, ErrorCode::UnsupportedOs);
        let env = call(&module, "get_selection", json!({ "ref": "@e1" })).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::UnsupportedOs);
    }

    #[test]
    fn the_new_tools_are_standard_tier_like_set_value() {
        let (module, _, _) = test_module();
        let all = module.descriptors();
        let tier = |n: &str| all.iter().find(|d| d.name == n).unwrap().tier;
        assert_eq!(tier("select_text"), tier("set_value"));
        assert_eq!(tier("get_selection"), tier("set_value"));
        for n in ["select_text", "get_selection"] {
            let d = all.iter().find(|d| d.name == n).unwrap();
            for p in ["ref", "name", "role"] {
                assert!(
                    d.input_schema["properties"].get(p).is_some(),
                    "{n} lacks {p}"
                );
            }
        }
    }

    // ---- foreground = "never" ------------------------------------------------

    fn never_module() -> (InputModule, Arc<MockBackend>) {
        let (module, backend, _) = test_module();
        let module = InputModule {
            policy: InputPolicy {
                foreground: mcp_types::Foreground::Never,
                ..module.policy.clone()
            },
            ..module
        };
        (module, backend)
    }

    /// True when nothing at all reached the backend that acts: no action, no
    /// write, no pointer event, no key, no scroll, no typing.
    fn untouched(b: &MockBackend) -> bool {
        b.actions.lock().unwrap().is_empty()
            && b.values.lock().unwrap().is_empty()
            && b.moves.lock().unwrap().is_empty()
            && b.keys.lock().unwrap().is_empty()
            && b.pointer.lock().unwrap().is_empty()
            && b.drags.lock().unwrap().is_empty()
            && b.key_events.lock().unwrap().is_empty()
            && b.scrolls.lock().unwrap().is_empty()
            && b.pixel_events.lock().unwrap().is_empty()
            && b.typed.lock().unwrap().is_empty()
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Want {
        /// Refused with POLICY_DENIED before the backend is touched.
        Refused,
        /// Not this setting's business: whatever it returns, it is not a
        /// foreground refusal.
        Runs,
    }

    /// One row per way each tool can be called. A tool with no row here fails
    /// `every_input_tool_has_a_foreground_decision`, so adding a tool forces a
    /// decision about the foreground instead of inheriting one.
    fn foreground_cases() -> Vec<(&'static str, Value, Want)> {
        use Want::{Refused, Runs};
        let e = |r: &str| json!({ "ref": r });
        let with = |r: &str, extra: Value| merge(e(r), extra);
        vec![
            ("ui_action", with("@e4", json!({"action":"click"})), Runs),
            (
                "ui_action",
                with("@e4", json!({"action":"double_click"})),
                Runs,
            ),
            (
                "ui_action",
                with("@e4", json!({"action":"right_click"})),
                Runs,
            ),
            ("ui_action", with("@e3", json!({"action":"toggle"})), Runs),
            ("ui_action", with("@e3", json!({"action":"check"})), Runs),
            (
                "ui_action",
                with("@e4", json!({"action":"scroll_into_view"})),
                Runs,
            ),
            (
                "ui_action",
                with("@e4", json!({"action":"increment"})),
                Runs,
            ),
            ("ui_action", with("@e4", json!({"action":"pick"})), Runs),
            ("ui_action", with("@e4", json!({"action":"select"})), Runs),
            ("ui_action", with("@e4", json!({"action":"nonsense"})), Runs),
            ("ui_action", with("@e1", json!({"action":"focus"})), Refused),
            ("ui_action", with("@e1", json!({"action":"Focus"})), Refused),
            (
                "ui_action",
                with("@e4", json!({"action":"show_menu"})),
                Refused,
            ),
            (
                "ui_action",
                with("@e5", json!({"action":"select","option":"Dark"})),
                Refused,
            ),
            ("set_value", with("@e1", json!({"text":"hello"})), Runs),
            ("select_text", with("@e1", json!({"start":0,"end":1})), Runs),
            ("get_selection", e("@e1"), Runs),
            ("get_selection", json!({}), Runs),
            ("clipboard_read", json!({}), Runs),
            ("clipboard_write", json!({"data":"x"}), Runs),
            (
                "ui_fill_form",
                json!({"fields":[{"ref":"@e1","value":"a"},{"ref":"@e3","value":true}],
                       "submit":{"ref":"@e4"}}),
                Runs,
            ),
            // The refused field is last: the earlier, harmless one must not
            // have been written by the time the call is refused.
            (
                "ui_fill_form",
                json!({"fields":[{"ref":"@e1","value":"a"},{"ref":"@e1","action":"focus"}]}),
                Refused,
            ),
            (
                "ui_fill_form",
                json!({"fields":[{"ref":"@e1","value":"a"},
                                 {"ref":"@e5","action":"select","value":"Dark"}]}),
                Refused,
            ),
            (
                "ui_fill_form",
                json!({"fields":[{"ref":"@e1","value":"a"}],
                       "submit":{"ref":"@e4","action":"show_menu"}}),
                Refused,
            ),
            ("keyboard_type", json!({"text":"hi"}), Refused),
            ("keyboard_type", with("@e1", json!({"text":"hi"})), Refused),
            ("keyboard_shortcut", json!({"combo":"cmd+s"}), Refused),
            (
                "keyboard_shortcut",
                json!({"combo":"cmd+s","action":"press"}),
                Refused,
            ),
            (
                "keyboard_shortcut",
                json!({"combo":"shift","action":"down"}),
                Refused,
            ),
            (
                "keyboard_shortcut",
                json!({"combo":"shift","action":"DOWN"}),
                Refused,
            ),
            ("keyboard_shortcut", json!({"action":"up"}), Runs),
            (
                "keyboard_shortcut",
                json!({"combo":"shift","action":"bogus"}),
                Runs,
            ),
            (
                "mouse_action",
                json!({"type":"click","x":10,"y":10}),
                Refused,
            ),
            (
                "mouse_action",
                with("@e4", json!({"type":"click"})),
                Refused,
            ),
            (
                "mouse_action",
                json!({"type":"move","x":10,"y":10,"glide":true}),
                Refused,
            ),
            ("mouse_action", json!({"type":"down","x":1,"y":1}), Refused),
            (
                "mouse_action",
                json!({"type":"triple","x":1,"y":1}),
                Refused,
            ),
            (
                "mouse_action",
                json!({"type":"right_click","x":1,"y":1}),
                Refused,
            ),
            ("scroll", json!({"direction":"down","x":5,"y":5}), Refused),
            (
                "scroll",
                json!({"dy":300,"x":5,"y":5,"momentum":true}),
                Refused,
            ),
            ("scroll", with("@e1", json!({"direction":"up"})), Refused),
            ("hover", json!({"x":5,"y":5}), Refused),
            ("hover", with("@e4", json!({"glide":true})), Refused),
            (
                "drag_drop",
                json!({"from":{"x":1,"y":1},"to":{"x":50,"y":50}}),
                Refused,
            ),
            (
                "drag_drop",
                json!({"from":{"x":1,"y":1},"path":[{"x":5,"y":5},{"x":9,"y":9}]}),
                Refused,
            ),
            ("input_showcase", json!({"enabled":true}), Refused),
            // A batch is as refused as its least safe step, and is refused
            // whole: the set_value before the keystroke must not run.
            (
                "input_steps",
                json!({"steps":[
                    {"tool":"set_value","args":{"ref":"@e4","text":"x"}},
                    {"tool":"keyboard_type","args":{"text":"y"}}
                ]}),
                Refused,
            ),
            (
                "input_steps",
                json!({"steps":[
                    {"tool":"set_value","args":{"ref":"@e4","text":"x"}},
                    {"tool":"ui_action","args":{"ref":"@e4","action":"click"}}
                ]}),
                Runs,
            ),
            ("desktop_record", json!({ "action": "status" }), Runs),
            ("desktop_flow", json!({ "action": "list" }), Runs),
        ]
    }

    #[tokio::test]
    async fn every_input_tool_has_a_foreground_decision() {
        let (module, _) = never_module();
        let advertised: Vec<String> = module.descriptors().into_iter().map(|d| d.name).collect();
        let classified: Vec<&str> = crate::foreground::TOOLS.iter().map(|(n, _)| *n).collect();
        for name in &advertised {
            assert!(
                classified.contains(&name.as_str()),
                "{name} is advertised but has no entry in foreground::TOOLS: decide whether it \
                 can run with foreground = \"never\" (it is refused until somebody does)"
            );
            assert!(
                foreground_cases().iter().any(|(t, _, _)| t == name),
                "{name} has no case in foreground_cases"
            );
        }
        for name in &classified {
            assert!(
                advertised.iter().any(|a| a == name),
                "foreground::TOOLS lists {name}, which is not a tool"
            );
        }
        // A tool classified as always-refused must be refused in every case.
        for (tool, stance) in crate::foreground::TOOLS {
            if *stance == crate::foreground::Stance::Refused {
                for (t, args, want) in foreground_cases() {
                    assert!(
                        t != *tool || want == Want::Refused,
                        "{tool} is classified Refused but {args} expects to run"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn under_never_every_synthetic_input_call_is_refused_before_anything_happens() {
        for (tool, args, want) in foreground_cases() {
            let (module, backend) = never_module();
            let env = call(&module, tool, args.clone()).await;
            let denied = env
                .error
                .as_ref()
                .is_some_and(|e| e.code == ErrorCode::PolicyDenied);
            match want {
                Want::Refused => {
                    assert!(!env.ok && denied, "{tool} {args} was not refused: {env:?}");
                    let err = env.error.unwrap();
                    assert!(
                        err.message.contains("foreground = \"never\"")
                            && err.message.contains(tool),
                        "{tool}: the refusal must say what and why: {}",
                        err.message
                    );
                    assert!(
                        err.suggestion.is_some_and(|s| !s.is_empty()),
                        "{tool}: the refusal must teach"
                    );
                    assert!(untouched(&backend), "{tool} {args} reached the backend");
                }
                Want::Runs => assert!(!denied, "{tool} {args} was refused: {env:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_default_refuses_nothing() {
        // `allow` is what every existing user has: the same calls, none of
        // them touched by this setting.
        assert_eq!(
            InputPolicy::default().foreground,
            mcp_types::Foreground::Allow
        );
        for (tool, args, _) in foreground_cases() {
            let (module, _, _) = test_module();
            let env = call(&module, tool, args.clone()).await;
            assert!(
                env.error
                    .as_ref()
                    .is_none_or(|e| e.code != ErrorCode::PolicyDenied),
                "{tool} {args} was refused under allow: {env:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_accessibility_tools_still_act_under_never() {
        let (module, backend) = never_module();
        backend.script(101, vec![shown("old"), shown("hello")]);
        let env = call(&module, "set_value", json!({"ref":"@e1","text":"hello"})).await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.as_ref().unwrap()["input"], "ax");
        assert_eq!(backend.values.lock().unwrap().len(), 1);
        let env = call(&module, "ui_action", json!({"ref":"@e4","action":"click"})).await;
        assert!(env.ok, "{env:?}");
        assert_eq!(env.data.as_ref().unwrap()["input"], "ax");
        // Nothing synthetic: no keys, pointer, scroll or typing.
        assert!(backend.keys.lock().unwrap().is_empty());
        assert!(backend.pointer.lock().unwrap().is_empty());
        assert!(backend.typed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_right_click_that_needs_the_pointer_on_this_platform_is_refused() {
        // Linux has no accessibility right click; its backend reports `Hid`
        // for it before acting, and that is what `never` must catch.
        let (module, backend) = never_module();
        *backend.route.lock().unwrap() = Some(Route::Hid);
        let env = call(
            &module,
            "ui_action",
            json!({"ref":"@e4","action":"right_click"}),
        )
        .await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        assert!(untouched(&backend));
        // Every other action is reported as `Hid` by this backend too, so each
        // is refused for that reason, not by name.
        for a in ["click", "toggle", "expand", "scroll_into_view"] {
            let env = call(&module, "ui_action", json!({"ref":"@e4","action":a})).await;
            assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied, "{a}");
        }
        assert!(untouched(&backend));
    }

    #[tokio::test]
    async fn the_backend_wrapper_holds_even_when_the_tool_layer_is_skipped() {
        // A module built with `allow` has no tool-layer check at all, as a
        // future tool without a decision would not; the wrapper still posts
        // nothing.
        let backend = Arc::new(MockBackend::default());
        let guarded = Arc::new(crate::ForegroundInput::new(backend.clone()));
        let (module, _, arena) = test_module();
        let module = InputModule::new(guarded.clone(), arena, module.policy.clone());
        for (tool, args, want) in foreground_cases() {
            if want != Want::Refused {
                continue;
            }
            // The tool layer would have refused these; here they get as far as
            // the backend, which must.
            let env = call(&module, tool, args.clone()).await;
            if tool == "input_showcase" {
                continue; // configuration, not a backend call
            }
            assert!(
                !env.ok,
                "{tool} {args} succeeded through the wrapper: {env:?}"
            );
            if tool == "ui_fill_form" {
                // Its harmless first field is written before the refused one
                // is reached, which is why the tool layer scans the whole
                // form first; the refused action itself must not have run.
                assert!(backend.actions.lock().unwrap().is_empty(), "{args}");
                backend.values.lock().unwrap().clear();
            }
            assert!(
                untouched(&backend),
                "{tool} {args} reached the real backend"
            );
        }
        // A release never fails, and posts nothing: nothing was pressed.
        guarded.key_up("shift").await.unwrap();
        assert!(untouched(&backend));
    }

    #[tokio::test]
    async fn the_wrapper_refuses_each_synthetic_method_and_forwards_the_rest() {
        let backend = Arc::new(MockBackend::default());
        let g = crate::ForegroundInput::new(backend.clone());
        let refused = |r: Result<(), InputError>| matches!(r, Err(InputError::Refused(_)));
        assert!(refused(g.type_text("x").await));
        assert!(refused(g.key_combo("cmd+s").await));
        assert!(refused(g.key_down("a").await));
        assert!(refused(
            g.mouse(MouseKind::Click, 1.0, 1.0, None, &[]).await
        ));
        assert!(refused(
            g.scroll_at(1.0, 1.0, ScrollDir::Down, 1, &[]).await
        ));
        assert!(refused(g.hover(1.0, 1.0).await));
        assert!(refused(g.drag((0.0, 0.0), &[(1.0, 1.0)], &[], 0, 0).await));
        assert!(matches!(
            g.choose_option(1, "x").await,
            Err(InputError::Refused(_))
        ));
        for a in [SemanticAction::Focus, SemanticAction::ShowMenu] {
            assert!(matches!(
                g.perform(1, a, None).await,
                Err(InputError::Refused(_))
            ));
        }
        // Per-process keys are let through only where they were measured (see
        // `pid_route_is_quiet_on`), and this platform is not one of them.
        *backend.background.lock().unwrap() = Some(BackgroundTarget {
            pid: 7,
            name: "Notes".into(),
        });
        assert!(g.background_key_target().is_none());
        assert!(refused(g.type_text_to_pid(7, "x").await));
        assert!(refused(g.key_combo_to_pid(7, "cmd+s").await));
        assert!(backend.pid_typed.lock().unwrap().is_empty());
        assert!(backend.pid_keys.lock().unwrap().is_empty());
        assert!(untouched(&backend));
        // What is background-safe goes through.
        assert_eq!(
            g.perform(1, SemanticAction::Click, None).await.unwrap(),
            Route::Ax
        );
        g.set_value(1, "v").await.unwrap();
        g.read_element(1).await.unwrap();
        g.clipboard_write(ClipFormat::Text, "c").await.unwrap();
        g.clipboard_read(ClipFormat::Text).await.unwrap();
        assert_eq!(backend.actions.lock().unwrap().len(), 1);
        assert_eq!(backend.values.lock().unwrap().len(), 1);
        // The sensors the human-override watcher reads are the inner ones.
        assert_eq!(g.platform(), "mock");
        assert_eq!(g.input_target().as_deref(), Some("TestApp"));
        g.cancel_pending();
        assert_eq!(g.takeover_generation(), 1);
        assert_eq!(backend.takeover_generation(), 1);
        assert_eq!(g.route_for(SemanticAction::Click), Route::Ax);
    }

    #[test]
    fn the_refusals_name_the_background_alternative_each_tool_has() {
        let b = MockBackend::default();
        let said = |tool: &str, args: Value| {
            let r = crate::foreground::tool_refusal(&b, tool, &args).expect("refused");
            format!("{} | {}", r.message(), r.suggestion())
        };
        assert!(
            said("mouse_action", json!({"type":"click","ref":"@e4"})).contains("ui_action click")
        );
        assert!(said("keyboard_type", json!({"text":"x","ref":"@e1"})).contains("set_value"));
        assert!(said("scroll", json!({"direction":"down"})).contains("scroll_into_view"));
        assert!(said("scroll", json!({"dy":300})).contains("scroll_into_view"));
        assert!(said("ui_action", json!({"ref":"@e1","action":"focus"})).contains("select_text"));
        for none in [
            (
                "drag_drop",
                json!({"from":{"x":1,"y":1},"to":{"x":2,"y":2}}),
            ),
            ("hover", json!({"x":1,"y":1})),
            ("mouse_action", json!({"type":"move","x":1,"y":1})),
        ] {
            let r = crate::foreground::tool_refusal(&b, none.0, &none.1).unwrap();
            // A tool with a partial alternative says so; one with none says that.
            if none.0 != "drag_drop" {
                assert!(r.instead.is_none(), "{}", none.0);
                assert!(r.suggestion().contains("no background-safe equivalent"));
            }
        }
    }

    // ---- input_steps --------------------------------------------------------

    /// Hands a step to the module the way the server does once its checks have
    /// passed. It decides nothing, and it is here for the batch's own
    /// mechanics only (order, stopping, the limits): that a step meets the
    /// real gate is proven against the real server in
    /// `agentctl/tests/input_steps.rs`.
    struct Direct {
        module: std::sync::OnceLock<Arc<InputModule>>,
        /// Runs after every step, to stage something that happens mid-batch.
        after: Box<dyn Fn(usize) + Send + Sync>,
        steps: std::sync::atomic::AtomicUsize,
        cancelled: std::sync::atomic::AtomicBool,
        /// Makes the named tool wait until it is cancelled.
        hang_on: Option<&'static str>,
    }

    #[async_trait]
    impl crate::steps::StepDispatch for Direct {
        async fn dispatch(&self, tool: &str, args: Value) -> Envelope {
            if self.hang_on == Some(tool) {
                while !self.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                return Envelope::fail(tool, ErrorCode::Timeout, "cancelled");
            }
            let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
            let env = self.module.get().unwrap().call(tool, args, &ctx).await;
            let n = self.steps.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (self.after)(n);
            env
        }
        fn cancel(&self) {
            self.cancelled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn direct(
        after: impl Fn(usize) + Send + Sync + 'static,
        hang_on: Option<&'static str>,
    ) -> Arc<Direct> {
        Arc::new(Direct {
            module: std::sync::OnceLock::new(),
            after: Box::new(after),
            steps: Default::default(),
            cancelled: Default::default(),
            hang_on,
        })
    }

    /// A module whose batches run through `d`.
    fn batching(
        d: &Arc<Direct>,
        tune: impl FnOnce(InputModule) -> InputModule,
    ) -> (Arc<InputModule>, Arc<MockBackend>) {
        let (module, backend, _) = test_module();
        let relay = crate::steps::StepRelay::new();
        relay.bind(d.clone());
        let module = Arc::new(tune(module.with_steps(relay)));
        d.module.set(module.clone()).ok().expect("set once");
        (module, backend)
    }

    #[tokio::test]
    async fn a_batch_runs_its_steps_in_order_and_reports_each() {
        let d = direct(|_| {}, None);
        let (module, backend) = batching(&d, |m| m);
        let env = call(
            &module,
            "input_steps",
            json!({"steps": [
                {"tool": "set_value", "args": {"ref": "@e1", "text": "alice"}},
                {"tool": "keyboard_shortcut", "args": {"combo": "cmd+s"}},
                {"tool": "ui_action", "args": {"ref": "@e4", "action": "click"}}
            ]}),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let data = env.data.unwrap();
        assert_eq!(data["ran"], 3);
        assert_eq!(data["total"], 3);
        assert!(data.get("failed_at").is_none());
        let rows = data["steps"].as_array().unwrap();
        let tools: Vec<_> = rows.iter().map(|r| r["tool"].as_str().unwrap()).collect();
        assert_eq!(tools, ["set_value", "keyboard_shortcut", "ui_action"]);
        assert!(rows.iter().all(|r| r["ok"] == true));
        assert_eq!(rows[1]["pressed"], "cmd+s");
        assert_eq!(backend.values.lock().unwrap().len(), 1);
        assert_eq!(backend.keys.lock().unwrap().as_slice(), ["cmd+s"]);
        assert_eq!(backend.actions.lock().unwrap().len(), 1);
    }

    /// The semantics of `browser_act` `steps`: the first failure ends the
    /// batch, says which step it was, and nothing after it runs.
    #[tokio::test]
    async fn the_first_failing_step_ends_the_batch_and_later_steps_never_run() {
        let d = direct(|_| {}, None);
        let (module, backend) = batching(&d, |m| m);
        let env = call(
            &module,
            "input_steps",
            json!({"steps": [
                {"tool": "set_value", "args": {"ref": "@e1", "text": "alice"}},
                {"tool": "ui_action", "args": {"ref": "@e999", "action": "click"}},
                {"tool": "keyboard_shortcut", "args": {"combo": "cmd+s"}}
            ]}),
        )
        .await;
        assert!(!env.ok);
        let err = env.error.as_ref().unwrap();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(
            err.message.starts_with("step 1 (ui_action) failed"),
            "{}",
            err.message
        );
        let data = env.data.unwrap();
        assert_eq!(data["failed_at"], 1);
        assert_eq!(data["ran"], 2);
        assert_eq!(data["total"], 3);
        assert_eq!(data["steps"].as_array().unwrap().len(), 2);
        assert_eq!(data["steps"][1]["ok"], false);
        assert_eq!(data["steps"][1]["error"]["code"], "NOT_FOUND");
        assert!(backend.keys.lock().unwrap().is_empty(), "step 2 ran");
        assert_eq!(backend.values.lock().unwrap().len(), 1, "step 0 did run");
    }

    #[tokio::test]
    async fn a_bad_batch_is_refused_whole_before_any_step_runs() {
        let d = direct(|_| {}, None);
        let (module, backend) = batching(&d, |m| m);
        for (args, want) in [
            (
                json!({"steps": [
                    {"tool": "set_value", "args": {"ref": "@e1", "text": "x"}},
                    {"tool": "exec", "args": {"command": "ls"}}
                ]}),
                "step 1",
            ),
            (
                json!({"steps": [
                    {"tool": "set_value", "args": {"ref": "@e1", "text": "x"}},
                    {"tool": "input_steps", "args": {"steps": []}}
                ]}),
                "step 1",
            ),
            (
                json!({"steps": [
                    {"tool": "set_value", "args": {"ref": "@e1", "text": "x"}},
                    "scroll"
                ]}),
                "step 1",
            ),
            (
                json!({"steps": vec![json!({"tool": "scroll"}); 21]}),
                "1 to 20",
            ),
        ] {
            let env = call(&module, "input_steps", args.clone()).await;
            let err = env.error.expect("refused");
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{args}");
            assert!(err.message.contains(want), "{args}: {}", err.message);
        }
        assert!(untouched(&backend), "a refused batch reached the backend");
    }

    /// With nothing to carry a step back through the gate, a batch does not
    /// run: executing the step itself would skip the checks.
    #[tokio::test]
    async fn a_batch_with_no_way_back_through_the_gate_runs_nothing() {
        let (module, backend, _) = test_module();
        let env = call(
            &module,
            "input_steps",
            json!({"steps": [{"tool": "set_value", "args": {"ref": "@e1", "text": "x"}}]}),
        )
        .await;
        assert!(!env.ok);
        assert!(env.error.unwrap().message.contains("not connected"));
        assert!(untouched(&backend));
    }

    #[tokio::test]
    async fn a_cancel_between_steps_stops_the_batch() {
        let ctx = CallCtx::new("t", mcp_types::CancelToken::new());
        let token = ctx.cancel.clone();
        let d = direct(move |_| token.cancel(), None);
        let (module, backend) = batching(&d, |m| m);
        let env = module
            .call(
                "input_steps",
                json!({"steps": [
                    {"tool": "set_value", "args": {"ref": "@e1", "text": "x"}},
                    {"tool": "keyboard_shortcut", "args": {"combo": "cmd+s"}}
                ]}),
                &ctx,
            )
            .await;
        assert!(!env.ok);
        let data = env.data.unwrap();
        assert_eq!(data["ran"], 1);
        assert_eq!(data["failed_at"], 1);
        assert_eq!(data["steps"][1]["skipped"], true);
        assert!(backend.keys.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_kill_switch_engaging_between_steps_stops_the_batch() {
        let halted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = halted.clone();
        let d = direct(
            move |_| flag.store(true, std::sync::atomic::Ordering::SeqCst),
            None,
        );
        let probe = halted.clone();
        let (module, backend) = batching(&d, move |m| {
            m.with_halt_probe(Arc::new(move || {
                probe.load(std::sync::atomic::Ordering::SeqCst)
            }))
        });
        let env = call(
            &module,
            "input_steps",
            json!({"steps": [
                {"tool": "set_value", "args": {"ref": "@e1", "text": "x"}},
                {"tool": "keyboard_shortcut", "args": {"combo": "cmd+s"}}
            ]}),
        )
        .await;
        let err = env.error.as_ref().expect("stopped");
        assert!(err.message.contains("kill switch"), "{}", err.message);
        assert_eq!(env.data.unwrap()["ran"], 1);
        assert!(backend.keys.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_human_taking_the_pointer_between_steps_stops_the_batch() {
        let slot: Arc<std::sync::OnceLock<Arc<MockBackend>>> = Arc::default();
        let s2 = slot.clone();
        let d = direct(
            move |_| {
                s2.get()
                    .unwrap()
                    .takeovers
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
            None,
        );
        let (module, backend) = batching(&d, |m| m);
        slot.set(backend.clone()).ok();
        let env = call(
            &module,
            "input_steps",
            json!({"steps": [
                {"tool": "set_value", "args": {"ref": "@e1", "text": "x"}},
                {"tool": "keyboard_shortcut", "args": {"combo": "cmd+s"}}
            ]}),
        )
        .await;
        assert!(env.error.unwrap().message.contains("took over the pointer"));
        assert!(backend.keys.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_batch_has_a_wall_time_and_a_step_still_running_is_cancelled() {
        let d = direct(|_| {}, Some("scroll"));
        let (module, backend) = batching(&d, |m| m.with_batch_limit(150));
        let started = std::time::Instant::now();
        let env = call(
            &module,
            "input_steps",
            json!({"steps": [
                {"tool": "scroll", "args": {"direction": "down"}},
                {"tool": "keyboard_shortcut", "args": {"combo": "cmd+s"}}
            ]}),
        )
        .await;
        assert!(!env.ok);
        // Cancelled by the batch, not left to hang.
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(d.cancelled.load(std::sync::atomic::Ordering::SeqCst));
        assert!(backend.keys.lock().unwrap().is_empty(), "the next step ran");
        assert_eq!(env.data.unwrap()["failed_at"], 0);
    }

    /// The rule that a failed input call lets go of held keys holds per step.
    #[tokio::test]
    async fn a_failed_step_releases_the_keys_held_before_it() {
        let d = direct(|_| {}, None);
        let (module, backend) = batching(&d, |m| m);
        let env = call(
            &module,
            "input_steps",
            json!({"steps": [
                {"tool": "keyboard_shortcut", "args": {"combo": "shift", "action": "down"}},
                {"tool": "ui_action", "args": {"ref": "@e999", "action": "click"}}
            ]}),
        )
        .await;
        assert!(!env.ok);
        assert_eq!(
            events(&backend),
            ["down:shift", "up:shift"],
            "the failing step let go of what the first held"
        );
    }

    #[test]
    fn the_batchable_tools_are_all_tools_of_this_module_and_the_rest_are_decided() {
        let (module, _, _) = test_module();
        let all: Vec<String> = module.descriptors().into_iter().map(|d| d.name).collect();
        for t in crate::steps::BATCHABLE {
            assert!(all.iter().any(|a| a == t), "{t} is not a tool");
        }
        // Everything left out is left out on purpose; a new tool has to be
        // put on one side or the other.
        let left_out = [
            "get_selection",
            "clipboard_read",
            "input_showcase",
            "input_steps",
            // A flow runs recorded steps, and a recording watches a person.
            "desktop_record",
            "desktop_flow",
        ];
        for a in &all {
            assert!(
                crate::steps::BATCHABLE.contains(&a.as_str()) || left_out.contains(&a.as_str()),
                "{a} is neither batchable nor deliberately left out"
            );
        }
        let batch = module
            .descriptors()
            .into_iter()
            .find(|d| d.name == "input_steps")
            .unwrap();
        assert_eq!(batch.tier, Tier::Standard);
        assert!(
            module
                .descriptors()
                .iter()
                .filter(|d| crate::steps::BATCHABLE.contains(&d.name.as_str()))
                .all(|d| d.tier == batch.tier),
            "a batchable tool above the batch's tier would never run"
        );
    }

    // ---- effects ------------------------------------------------------------

    /// Hands out the facts it was given, one reading per call, and notes what
    /// it was asked. Scripts what the module is *told*; what a real app shows
    /// is read in `mcp-macos`'s own tests.
    struct Scripted {
        readings: std::sync::Mutex<Vec<crate::effects::UiFacts>>,
        asked: std::sync::Mutex<Vec<Option<Vec<crate::effects::Overlay>>>>,
    }

    #[async_trait]
    impl crate::effects::UiProbe for Scripted {
        async fn probe(
            &self,
            describe_new: Option<&[crate::effects::Overlay]>,
        ) -> crate::effects::UiFacts {
            self.asked
                .lock()
                .unwrap()
                .push(describe_new.map(<[_]>::to_vec));
            let mut r = self.readings.lock().unwrap();
            if r.len() > 1 {
                r.remove(0)
            } else {
                r[0].clone()
            }
        }
    }

    fn scripted(readings: Vec<crate::effects::UiFacts>) -> Arc<Scripted> {
        Arc::new(Scripted {
            readings: std::sync::Mutex::new(readings),
            asked: Default::default(),
        })
    }

    fn windows(titles: &[&str]) -> crate::effects::UiFacts {
        crate::effects::UiFacts {
            windows: Some(titles.iter().map(|t| t.to_string()).collect()),
            overlays: Some(Vec::new()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn an_action_reports_what_changed_and_only_that() {
        let (module, backend, _) = test_module();
        let probe = scripted(vec![windows(&["Untitled"]), windows(&["notes.txt"])]);
        let module = module.with_probe(probe.clone());
        backend.script(104, vec![shown("same")]);
        let env = call(
            &module,
            "ui_action",
            json!({"ref": "@e4", "action": "click"}),
        )
        .await;
        assert!(env.ok, "{env:?}");
        let data = env.data.unwrap();
        assert_eq!(
            data["effects"],
            json!({"retitled": {"from": "Untitled", "to": "notes.txt"}})
        );
        // Read twice, and the second reading was told what was already open.
        let asked = probe.asked.lock().unwrap();
        assert_eq!(asked.len(), 2);
        assert_eq!(asked[0], None);
        assert_eq!(asked[1], Some(Vec::new()));
    }

    #[tokio::test]
    async fn an_action_that_changed_nothing_has_no_effects_key() {
        let (module, _, _) = test_module();
        let module = module.with_probe(scripted(vec![windows(&["a"])]));
        let env = call(
            &module,
            "ui_action",
            json!({"ref": "@e4", "action": "click"}),
        )
        .await;
        assert!(env.ok);
        assert!(env.data.unwrap().get("effects").is_none());
    }

    #[tokio::test]
    async fn effects_false_reads_nothing_and_a_typo_is_refused_before_the_action() {
        let (module, backend, _) = test_module();
        let probe = scripted(vec![windows(&["a"]), windows(&["b"])]);
        let module = module.with_probe(probe.clone());
        let env = call(
            &module,
            "ui_action",
            json!({"ref": "@e4", "action": "click", "effects": false}),
        )
        .await;
        assert!(env.ok);
        assert!(probe.asked.lock().unwrap().is_empty(), "the probe was read");
        assert!(env.data.unwrap().get("effects").is_none());
        let bad = call(
            &module,
            "ui_action",
            json!({"ref": "@e4", "action": "click", "effects": "no"}),
        )
        .await;
        assert_eq!(bad.error.unwrap().code, ErrorCode::InvalidArgs);
        assert_eq!(
            backend.actions.lock().unwrap().len(),
            1,
            "only the first acted"
        );
    }

    #[tokio::test]
    async fn a_failed_action_pays_for_no_second_reading() {
        let (module, _, _) = test_module();
        let probe = scripted(vec![windows(&["a"]), windows(&["b"])]);
        let module = module.with_probe(probe.clone());
        let env = call(
            &module,
            "ui_action",
            json!({"ref": "@e999", "action": "click"}),
        )
        .await;
        assert!(!env.ok);
        assert_eq!(probe.asked.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn only_actions_that_can_change_the_screen_are_read_around() {
        let (module, _, _) = test_module();
        let probe = scripted(vec![windows(&["a"]), windows(&["b"])]);
        let module = module.with_probe(probe.clone());
        for (tool, args) in [
            (
                "keyboard_shortcut",
                json!({"combo": "shift", "action": "down"}),
            ),
            (
                "keyboard_shortcut",
                json!({"combo": "shift", "action": "up"}),
            ),
            ("mouse_action", json!({"type": "move", "x": 1, "y": 1})),
            ("scroll", json!({"direction": "down", "x": 1, "y": 1})),
            ("hover", json!({"x": 1, "y": 1})),
            ("clipboard_write", json!({"data": "x"})),
        ] {
            let env = call(&module, tool, args.clone()).await;
            assert!(env.ok, "{tool} {args}: {env:?}");
        }
        assert!(probe.asked.lock().unwrap().is_empty());
        assert!(effects_apply(
            "keyboard_shortcut",
            &json!({"combo": "cmd+s"})
        ));
        assert!(effects_apply(
            "mouse_action",
            &json!({"type": "right_click"})
        ));
        assert!(!effects_apply("mouse_action", &json!({"type": "down"})));
        assert!(!effects_apply("mouse_action", &json!({"type": "up"})));
    }

    #[tokio::test]
    async fn a_pressed_elements_new_value_is_reported_unless_it_is_a_password() {
        let (module, backend, _) = test_module();
        let module = module.with_probe(scripted(vec![windows(&["a"])]));
        backend.script(101, vec![shown("one"), shown("two")]);
        let env = call(
            &module,
            "ui_action",
            json!({"ref": "@e1", "action": "click"}),
        )
        .await;
        assert_eq!(env.data.unwrap()["effects"], json!({"value": "two"}));
        backend.script(102, vec![shown("hunter2"), shown("hunter3")]);
        let env = call(
            &module,
            "ui_action",
            json!({"ref": "@e2", "action": "click"}),
        )
        .await;
        assert!(env.ok);
        assert!(env.data.unwrap().get("effects").is_none());
    }

    /// A batch step reports its own effects like a lone call does.
    #[tokio::test]
    async fn each_step_of_a_batch_carries_its_own_effects() {
        let d = direct(|_| {}, None);
        let (module, _) = {
            let (m, b, _) = test_module();
            let relay = crate::steps::StepRelay::new();
            relay.bind(d.clone());
            let m = Arc::new(
                m.with_steps(relay)
                    .with_probe(scripted(vec![windows(&["a"]), windows(&["b"])])),
            );
            d.module.set(m.clone()).ok().expect("once");
            (m, b)
        };
        let env = call(
            &module,
            "input_steps",
            json!({"steps": [{"tool": "ui_action", "args": {"ref": "@e4", "action": "click"}}]}),
        )
        .await;
        assert!(env.ok, "{env:?}");
        assert_eq!(
            env.data.unwrap()["steps"][0]["effects"],
            json!({"retitled": {"from": "a", "to": "b"}})
        );
    }
}
