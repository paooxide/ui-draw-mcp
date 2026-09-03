use std::collections::{HashMap, VecDeque};
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::Mutex;

use async_trait::async_trait;
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::{CFNumber, CFNumberRef};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::base::Boolean;
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionOnScreenOnly, kCGWindowOwnerName, kCGWindowOwnerPID,
};

use mcp_a11y::{
    is_interactive_role, A11yBackend, BackendError, Bounds, RawSnapshot, SnapshotRequest, UiNode,
};
use mcp_input::{
    ClipData, ClipFormat, InputBackend, InputError, MouseKind, ScrollDir, SemanticAction,
};
use mcp_window::{
    DialogInfo, DialogScope, MenuItemInfo, Rect, WindowAction, WindowBackend, WindowError,
    WindowInfo,
};

// ---- AXUIElement FFI (ApplicationServices framework) ------------------------

type AXUIElementRef = CFTypeRef;
type AXError = i32;

const KAX_SUCCESS: AXError = 0;
const KAX_ERROR_API_DISABLED: AXError = -25211;
const KAX_ERROR_INVALID_ELEMENT: AXError = -25202;
const KAX_ERROR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;
const KAX_ERROR_ACTION_UNSUPPORTED: AXError = -25206;
const KAX_ERROR_CANNOT_COMPLETE: AXError = -25204;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCreateApplication(pid: c_int) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> AXError;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut c_int) -> AXError;
    fn AXIsProcessTrusted() -> Boolean;
    fn AXValueGetValue(value: CFTypeRef, the_type: u32, value_ptr: *mut c_void) -> Boolean;
    fn AXValueCreate(the_type: u32, value_ptr: *const c_void) -> CFTypeRef;
}

// AXValue wrapped geometry types.
const KAXVALUE_CGPOINT: u32 = 1;
const KAXVALUE_CGSIZE: u32 = 2;

#[repr(C)]
struct CGPoint {
    x: f64,
    y: f64,
}
#[repr(C)]
struct CGSize {
    width: f64,
    height: f64,
}

// ---- safe-ish helpers over the FFI ------------------------------------------

/// Copy an attribute as a generic CF value (create rule → owned).
/// Which TCC permissions this process actually holds.
///
/// Both are silent when missing: without Accessibility the AX calls return
/// nothing rather than an error, and without Screen Recording a capture comes
/// back as the desktop wallpaper. Checking up front is the difference between
/// "no windows found" and "you never granted the permission".
#[derive(Debug, Clone, Copy)]
pub struct Permissions {
    pub accessibility: bool,
    pub screen_recording: bool,
}

// Preflight, never prompt: `doctor` is a diagnostic, and a diagnostic that
// raises a system permission dialog is a side effect nobody asked for.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> Boolean;
}

pub fn permissions() -> Permissions {
    unsafe {
        Permissions {
            accessibility: AXIsProcessTrusted() != 0,
            screen_recording: CGPreflightScreenCaptureAccess() != 0,
        }
    }
}

unsafe fn copy_attr(elem: CFTypeRef, name: &str) -> Option<CFType> {
    let cfname = CFString::new(name);
    let mut out: CFTypeRef = ptr::null();
    let err = AXUIElementCopyAttributeValue(elem, cfname.as_concrete_TypeRef(), &mut out);
    if err == KAX_SUCCESS && !out.is_null() {
        Some(CFType::wrap_under_create_rule(out))
    } else {
        None
    }
}

/// Copy a string-valued attribute.
unsafe fn copy_attr_string(elem: CFTypeRef, name: &str) -> Option<String> {
    copy_attr(elem, name)?
        .downcast::<CFString>()
        .map(|s| s.to_string())
}

/// Copy the children of an element as owned CF elements.
unsafe fn copy_children(elem: CFTypeRef) -> Vec<CFType> {
    let Some(value) = copy_attr(elem, "AXChildren") else {
        return Vec::new();
    };
    let Some(array) = value.downcast::<CFArray>() else {
        return Vec::new();
    };
    array
        .get_all_values()
        .into_iter()
        .map(|p| CFType::wrap_under_get_rule(p))
        .collect()
}

fn is_terminal(app: &str) -> bool {
    const TERMS: &[&str] = &["terminal", "iterm", "warp", "alacritty", "kitty", "ghostty"];
    let lower = app.to_ascii_lowercase();
    TERMS.iter().any(|t| lower.contains(t))
}

/// Read an `AXValue`-wrapped `CGPoint` attribute (e.g. `AXPosition`).
unsafe fn copy_attr_point(elem: CFTypeRef, name: &str) -> Option<(f64, f64)> {
    let v = copy_attr(elem, name)?;
    let mut p = CGPoint { x: 0.0, y: 0.0 };
    let ok = AXValueGetValue(
        v.as_CFTypeRef(),
        KAXVALUE_CGPOINT,
        &mut p as *mut CGPoint as *mut c_void,
    );
    if ok != 0 {
        Some((p.x, p.y))
    } else {
        None
    }
}

/// Read an `AXValue`-wrapped `CGSize` attribute (e.g. `AXSize`).
unsafe fn copy_attr_size(elem: CFTypeRef, name: &str) -> Option<(f64, f64)> {
    let v = copy_attr(elem, name)?;
    let mut s = CGSize {
        width: 0.0,
        height: 0.0,
    };
    let ok = AXValueGetValue(
        v.as_CFTypeRef(),
        KAXVALUE_CGSIZE,
        &mut s as *mut CGSize as *mut c_void,
    );
    if ok != 0 {
        Some((s.width, s.height))
    } else {
        None
    }
}

/// Read an element's screen bounds from `AXPosition` + `AXSize`.
pub(crate) unsafe fn read_bounds(elem: CFTypeRef) -> Option<Bounds> {
    let (x, y) = copy_attr_point(elem, "AXPosition")?;
    let (w, h) = copy_attr_size(elem, "AXSize")?;
    Some(Bounds { x, y, w, h })
}

fn ax_result(err: AXError, what: &str) -> Result<(), InputError> {
    match err {
        KAX_SUCCESS => Ok(()),
        KAX_ERROR_API_DISABLED => Err(InputError::PermissionDenied(
            "accessibility API disabled (grant permission)".into(),
        )),
        KAX_ERROR_ATTRIBUTE_UNSUPPORTED | KAX_ERROR_ACTION_UNSUPPORTED => Err(
            InputError::Unsupported(format!("{what}: unsupported on this element")),
        ),
        KAX_ERROR_INVALID_ELEMENT => Err(InputError::NotFound(format!("{what}: element is stale"))),
        KAX_ERROR_CANNOT_COMPLETE => Err(InputError::Failed(format!("{what}: cannot complete"))),
        other => Err(InputError::Failed(format!("{what}: AXError {other}"))),
    }
}

// ---- tree walker ------------------------------------------------------------

/// Identity of a snapshotted node: the child-index path from the app root plus
/// enough identity (role + accessible name) to re-find the element if the tree
/// shifted underneath us between snapshot and action.
#[derive(Clone)]
struct NodeRef {
    path: Vec<usize>,
    role: String,
    name: Option<String>,
}

/// How long a single snapshot may spend traversing.
///
/// Every attribute read is an IPC round trip to the target application, so an
/// app that is busy — or that simply has a lot of elements, like Finder with a
/// populated desktop — makes each one slow. Measured on a real machine: 12
/// seconds for 58 refs from Finder.
///
/// This matters beyond the wait itself. The traversal is synchronous FFI, so it
/// never yields, which means `tokio::time::timeout` cannot interrupt it and a
/// `wait_for` with a 1s deadline was overrunning by 12x. The bound has to be
/// *inside* the walk.
const WALK_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

struct Walker {
    counter: u64,
    paths: HashMap<u64, NodeRef>,
    count: usize,
    max_nodes: usize,
    /// When the traversal started, and whether it ran out of time.
    started: std::time::Instant,
    budget: std::time::Duration,
    timed_out: bool,
}

impl Walker {
    /// Whether the traversal has spent its budget. Sticky, so the answer does
    /// not flap while unwinding the recursion.
    fn out_of_time(&mut self) -> bool {
        if self.timed_out {
            return true;
        }
        if self.started.elapsed() >= self.budget {
            self.timed_out = true;
            tracing::warn!(
                nodes = self.count,
                "accessibility traversal ran out of time; the tree is partial"
            );
        }
        self.timed_out
    }

    unsafe fn walk(
        &mut self,
        elem: &CFType,
        depth: usize,
        max_depth: usize,
        path: &mut Vec<usize>,
    ) -> UiNode {
        let eref = elem.as_CFTypeRef();
        let role = copy_attr_string(eref, "AXRole").unwrap_or_default();
        let subrole = copy_attr_string(eref, "AXSubrole");
        let name = copy_attr_string(eref, "AXTitle")
            .filter(|s| !s.is_empty())
            .or_else(|| copy_attr_string(eref, "AXDescription").filter(|s| !s.is_empty()));
        let secure = subrole.as_deref() == Some("AXSecureTextField");
        let value = if secure {
            None
        } else {
            copy_attr_string(eref, "AXValue").filter(|s| !s.is_empty())
        };

        let id = self.counter;
        self.counter += 1;
        self.paths.insert(
            id,
            NodeRef {
                path: path.clone(),
                role: role.clone(),
                name: name.clone(),
            },
        );
        self.count += 1;

        // Read geometry only for interactive nodes (2 AX calls each).
        let bounds = if is_interactive_role(&role) {
            read_bounds(eref)
        } else {
            None
        };

        let mut node = UiNode {
            role,
            name,
            value,
            subrole,
            secure,
            bounds,
            node_id: Some(id),
            ..Default::default()
        };

        if depth < max_depth && self.count < self.max_nodes && !self.out_of_time() {
            for (i, child) in copy_children(eref).iter().enumerate() {
                if self.count >= self.max_nodes || self.out_of_time() {
                    break;
                }
                path.push(i);
                node.children
                    .push(self.walk(child, depth + 1, max_depth, path));
                path.pop();
            }
        }
        node
    }
}

// ---- the backend ------------------------------------------------------------

#[derive(Default)]
struct State {
    pid: Option<c_int>,
    paths: HashMap<u64, NodeRef>,
    /// The app this session is driving. Set by an explicit `app` argument and
    /// then *followed* — later calls stay on it instead of tracking whatever
    /// happens to be frontmost.
    target: Option<AppTarget>,
}

/// A resolved application target.
#[derive(Clone)]
struct AppTarget {
    pid: c_int,
    name: String,
}

/// Real macOS backend implementing perception (`A11yBackend`) and semantic input
/// (`InputBackend`). Element handles are not stored across snapshots; each node
/// keeps a child-index *path* from the app root (Send-safe), re-walked to act.
/// `~/.agentctl/bin`, or the temp directory when there is no home.
fn default_helper_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".agentctl")
        .join("bin")
}

pub struct MacosBackend {
    state: Mutex<State>,
    /// Where the compiled OCR helper is cached. Passed in rather than derived
    /// here: a backend must not depend on the policy crate to find out where
    /// the agentctl state directory is.
    pub(crate) helper_dir: std::path::PathBuf,
    /// Set when something asks in-flight work to stop — the human-override
    /// watcher, for instance. Checked between the steps of a drag, which would
    /// otherwise keep the button held while the person moves the mouse.
    cancel: std::sync::atomic::AtomicBool,
}

impl Default for MacosBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl MacosBackend {
    pub fn new() -> Self {
        MacosBackend {
            state: Mutex::new(State::default()),
            cancel: std::sync::atomic::AtomicBool::new(false),
            helper_dir: default_helper_dir(),
        }
    }

    /// Cache the compiled OCR helper somewhere other than the default.
    pub fn with_helper_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.helper_dir = dir.into();
        self
    }

    /// Resolve which app to drive, and remember it.
    ///
    /// An explicit `app` wins and becomes the sticky target; otherwise we reuse
    /// the sticky target while its process is alive; otherwise we fall back to
    /// the frontmost app. This is what makes the server *follow* the app it is
    /// working on rather than tracking whatever steals focus.
    unsafe fn resolve_target(&self, app: Option<&str>) -> Result<(CFType, c_int, String), String> {
        if let Some(name) = app.map(str::trim).filter(|s| !s.is_empty()) {
            let (pid, resolved) = resolve_app_pid(name)
                .ok_or_else(|| format!("no running application matching '{name}'"))?;
            let elem = app_element(pid).ok_or("cannot open application")?;
            self.set_target(pid, &resolved);
            return Ok((elem, pid, resolved));
        }
        let sticky = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.target.clone()
        };
        if let Some(t) = sticky {
            if pid_alive(t.pid) {
                if let Some(elem) = app_element(t.pid) {
                    return Ok((elem, t.pid, t.name));
                }
            }
            // Target died — drop it and fall through to the frontmost app.
            self.state.lock().unwrap_or_else(|e| e.into_inner()).target = None;
        }
        let elem = frontmost_app().ok_or("no frontmost application")?;
        let mut pid: c_int = 0;
        AXUIElementGetPid(elem.as_CFTypeRef(), &mut pid);
        let name = copy_attr_string(elem.as_CFTypeRef(), "AXTitle").unwrap_or_default();
        self.set_target(pid, &name);
        Ok((elem, pid, name))
    }

    /// Raise the sticky target app before synthetic keyboard input.
    ///
    /// CGEvent keystrokes go to whatever application is **frontmost**, not to
    /// the element we resolved — so without this, typing can land in whatever
    /// the user happens to have focused. We raise the target and verify it came
    /// forward, and refuse to type if it did not.
    unsafe fn ensure_target_frontmost(&self) -> Result<(), InputError> {
        let Some(t) = ({
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.target.clone()
        }) else {
            return Ok(()); // no target chosen — caller accepts the focused app
        };
        if frontmost_pid() == Some(t.pid) {
            return Ok(());
        }
        if let Some(app) = app_element(t.pid) {
            let _ = set_bool_attr(app.as_CFTypeRef(), "AXFrontmost", true);
        }
        // Give the window server a moment to actually raise the app.
        for _ in 0..20 {
            if frontmost_pid() == Some(t.pid) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Err(InputError::Failed(format!(
            "refusing to type: target app '{}' (pid {}) could not be brought to the front, \
             so keystrokes would land in another application",
            t.name, t.pid
        )))
    }

    fn set_target(&self, pid: c_int, name: &str) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.target = Some(AppTarget {
            pid,
            name: name.to_string(),
        });
    }

    /// Forget the sticky target (next call tracks the frontmost app again).
    pub fn clear_target(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).target = None;
    }

    /// The app this session is currently driving, if any.
    pub fn current_target(&self) -> Option<(i32, String)> {
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.target.as_ref().map(|t| (t.pid, t.name.clone()))
    }

    /// Resolve a snapshot ref to a live element.
    ///
    /// Tries the recorded child-index path first, then verifies the element it
    /// landed on still has the recorded role/name. If the path broke or now
    /// points at something else (the tree mutated between snapshot and action),
    /// falls back to searching the app for a node with the same identity. Only
    /// a named element can be re-found this way — matching on role alone would
    /// risk acting on the wrong control.
    unsafe fn element_for(&self, node_id: u64) -> Result<CFType, InputError> {
        let (pid, nref) = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let pid = st
                .pid
                .ok_or_else(|| InputError::NotFound("no snapshot yet".into()))?;
            let nref = st
                .paths
                .get(&node_id)
                .cloned()
                .ok_or_else(|| InputError::NotFound("ref not in the latest snapshot".into()))?;
            (pid, nref)
        };
        let root =
            app_element(pid).ok_or_else(|| InputError::Failed("cannot open application".into()))?;

        if let Some(el) = walk_path(&root, &nref.path) {
            if identity_matches(&el, &nref) {
                return Ok(el);
            }
        }
        if nref.name.is_some() {
            if let Some(el) = find_by_identity(&root, &nref, 0, 40) {
                tracing::debug!(node_id, "ref path was stale; re-found element by identity");
                return Ok(el);
            }
        }
        Err(InputError::NotFound(
            "element path is stale — re-run get_ui_tree to refresh refs".into(),
        ))
    }
}

/// The AX element for a pid.
unsafe fn app_element(pid: c_int) -> Option<CFType> {
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        None
    } else {
        Some(CFType::wrap_under_create_rule(app))
    }
}

/// Follow a child-index path from a root element.
unsafe fn walk_path(root: &CFType, path: &[usize]) -> Option<CFType> {
    let mut cur = CFType::wrap_under_get_rule(root.as_CFTypeRef());
    for &i in path {
        cur = copy_children(cur.as_CFTypeRef()).into_iter().nth(i)?;
    }
    Some(cur)
}

/// The role/name identity of a live element.
unsafe fn identity_of(elem: CFTypeRef) -> (String, Option<String>) {
    let role = copy_attr_string(elem, "AXRole").unwrap_or_default();
    let name = copy_attr_string(elem, "AXTitle")
        .filter(|s| !s.is_empty())
        .or_else(|| copy_attr_string(elem, "AXDescription").filter(|s| !s.is_empty()));
    (role, name)
}

unsafe fn identity_matches(elem: &CFType, want: &NodeRef) -> bool {
    let (role, name) = identity_of(elem.as_CFTypeRef());
    role == want.role && name == want.name
}

/// Depth-bounded search for an element with the recorded identity.
unsafe fn find_by_identity(
    root: &CFType,
    want: &NodeRef,
    depth: usize,
    max_depth: usize,
) -> Option<CFType> {
    if depth > max_depth {
        return None;
    }
    if identity_matches(root, want) {
        return Some(CFType::wrap_under_get_rule(root.as_CFTypeRef()));
    }
    for child in copy_children(root.as_CFTypeRef()) {
        if let Some(found) = find_by_identity(&child, want, depth + 1, max_depth) {
            return Some(found);
        }
    }
    None
}

#[async_trait]
impl A11yBackend for MacosBackend {
    async fn snapshot(&self, req: &SnapshotRequest) -> Result<RawSnapshot, BackendError> {
        // SAFETY: all AX calls are synchronous C FFI; CF values are created and
        // dropped within this call and never held across an await.
        unsafe {
            if AXIsProcessTrusted() == 0 {
                return Err(BackendError::PermissionDenied(
                    "Accessibility permission not granted — enable agentctl's terminal in \
                     System Settings › Privacy & Security › Accessibility"
                        .into(),
                ));
            }

            // An explicit `app` targets that application and becomes sticky;
            // otherwise we follow the app this session is already driving, and
            // only fall back to the frontmost one if there is no target yet.
            let (app, pid, target_name) = self
                .resolve_target(req.app.as_deref())
                .map_err(BackendError::NotFound)?;

            let max_depth = req.max_depth.unwrap_or(40);
            let mut walker = Walker {
                counter: 1,
                paths: HashMap::new(),
                count: 0,
                max_nodes: 4000,
                started: std::time::Instant::now(),
                budget: WALK_BUDGET,
                timed_out: false,
            };
            let mut path = Vec::new();
            let root = walker.walk(&app, 0, max_depth, &mut path);
            let app_name = copy_attr_string(app.as_CFTypeRef(), "AXTitle")
                .filter(|s| !s.is_empty())
                .or(Some(target_name).filter(|s| !s.is_empty()));

            let partial = walker.timed_out || walker.count >= walker.max_nodes;
            {
                let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                st.pid = Some(pid);
                st.paths = walker.paths;
            }

            let terminal = app_name.as_deref().map(is_terminal).unwrap_or(false);
            Ok(RawSnapshot {
                root,
                app: app_name,
                window: None,
                terminal_app: terminal,
                partial,
            })
        }
    }

    fn platform(&self) -> &'static str {
        "macos"
    }
}

#[async_trait]
impl InputBackend for MacosBackend {
    async fn perform(
        &self,
        node_id: u64,
        action: SemanticAction,
        _option: Option<&str>,
    ) -> Result<(), InputError> {
        unsafe {
            let elem = self.element_for(node_id)?;
            let eref = elem.as_CFTypeRef();
            match action {
                SemanticAction::Focus => {
                    let attr = CFString::new("AXFocused");
                    let val = CFBoolean::from(true);
                    let err = AXUIElementSetAttributeValue(
                        eref,
                        attr.as_concrete_TypeRef(),
                        val.as_CFTypeRef(),
                    );
                    ax_result(err, "focus")
                }
                SemanticAction::ScrollIntoView => {
                    let act = CFString::new("AXScrollToVisible");
                    ax_result(
                        AXUIElementPerformAction(eref, act.as_concrete_TypeRef()),
                        "scroll_into_view",
                    )
                }
                // Click / toggle / check / select all map to the AXPress action;
                // AX has no distinct double/right press.
                _ => {
                    let act = CFString::new("AXPress");
                    ax_result(
                        AXUIElementPerformAction(eref, act.as_concrete_TypeRef()),
                        "press",
                    )
                }
            }
        }
    }

    async fn set_value(&self, node_id: u64, text: &str) -> Result<(), InputError> {
        unsafe {
            let elem = self.element_for(node_id)?;
            let attr = CFString::new("AXValue");
            let val = CFString::new(text);
            let err = AXUIElementSetAttributeValue(
                elem.as_CFTypeRef(),
                attr.as_concrete_TypeRef(),
                val.as_CFTypeRef(),
            );
            ax_result(err, "set_value")
        }
    }

    // ---- coordinate/keyboard/clipboard via CGEvent + pbcopy/pbpaste ----------

    async fn type_text(&self, text: &str) -> Result<(), InputError> {
        unsafe { self.ensure_target_frontmost()? };
        crate::event::type_text(text)
    }
    async fn key_combo(&self, combo: &str) -> Result<(), InputError> {
        unsafe { self.ensure_target_frontmost()? };
        crate::event::key_combo(combo)
    }
    async fn mouse(
        &self,
        k: MouseKind,
        x: f64,
        y: f64,
        b: Option<&str>,
        modifiers: &[String],
    ) -> Result<(), InputError> {
        crate::event::mouse(k, x, y, b, modifiers)
    }
    async fn scroll_at(&self, x: f64, y: f64, d: ScrollDir, a: i32) -> Result<(), InputError> {
        crate::event::scroll(x, y, d, a)
    }
    async fn pointer_position(&self) -> Result<Option<(f64, f64)>, InputError> {
        crate::event::pointer_position()
    }
    fn recent_pointer_sets(&self) -> Vec<mcp_input::SetPoint> {
        crate::event::recent_sets()
    }
    fn cancel_pending(&self) {
        self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    async fn hover(&self, x: f64, y: f64) -> Result<(), InputError> {
        crate::event::hover(x, y)
    }
    async fn drag(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        modifiers: &[String],
        steps: u32,
    ) -> Result<(), InputError> {
        use std::sync::atomic::Ordering;
        use tokio::time::{sleep, Duration};
        self.cancel.store(false, Ordering::SeqCst);
        crate::event::drag_begin(from, modifiers)?;
        // Let the press register before motion starts; a drag that begins in
        // the same instant reads as a click to most targets.
        sleep(Duration::from_millis(30)).await;
        let mut last = from;
        for p in crate::event::drag_path(from, to, steps) {
            // A cancelled drag must release the button where it is. Returning
            // with it still held would leave the human dragging a selection
            // around with their own mouse.
            if self.cancel.load(Ordering::SeqCst) {
                let _ = crate::event::drag_end(last, modifiers);
                return Err(InputError::Failed(
                    "drag aborted: a human took over the pointer".into(),
                ));
            }
            crate::event::drag_to(p, modifiers)?;
            last = p;
            sleep(Duration::from_millis(8)).await;
        }
        // Settle at the destination so the drop target can highlight and accept.
        sleep(Duration::from_millis(60)).await;
        crate::event::drag_end(to, modifiers)
    }

    /// Where synthetic input will actually land: the pinned target if the
    /// session has one, else the frontmost on-screen app.
    fn input_target(&self) -> Option<String> {
        if let Some((pid, name)) = self.current_target() {
            if pid_alive(pid) {
                return Some(name);
            }
        }
        unsafe {
            let pid = frontmost_pid()?;
            window_owners()
                .into_iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, n)| n)
        }
    }
    async fn clipboard_read(&self, format: ClipFormat) -> Result<ClipData, InputError> {
        match format {
            ClipFormat::Text => Ok(ClipData {
                format: ClipFormat::Text,
                data: crate::event::clipboard_read_text()?,
            }),
            other => Err(InputError::Unsupported(format!(
                "clipboard_read {other:?} not supported"
            ))),
        }
    }
    async fn clipboard_write(&self, format: ClipFormat, data: &str) -> Result<(), InputError> {
        match format {
            ClipFormat::Text => crate::event::clipboard_write_text(data),
            other => Err(InputError::Unsupported(format!(
                "clipboard_write {other:?} not supported"
            ))),
        }
    }

    fn platform(&self) -> &'static str {
        "macos"
    }
}

// ---- window / app / menu backend (AX + subprocess) --------------------------

/// Read a string entry out of a CGWindowList dictionary.
unsafe fn dict_string(dict: &CFDictionary, key: CFStringRef) -> Option<String> {
    let raw = *dict.find(key.cast::<c_void>())?;
    if raw.is_null() {
        return None;
    }
    Some(CFString::wrap_under_get_rule(raw as CFStringRef).to_string())
}

/// On-screen GUI apps as `(pid, owner name)`, front-to-back, deduplicated.
unsafe fn window_owners() -> Vec<(c_int, String)> {
    let mut out: Vec<(c_int, String)> = Vec::new();
    let Some(arr) = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
        return out;
    };
    for raw in arr.get_all_values() {
        if raw.is_null() {
            continue;
        }
        let dict = CFDictionary::wrap_under_get_rule(raw as _);
        let (Some(pid), Some(name)) = (
            dict_i64(&dict, kCGWindowOwnerPID),
            dict_string(&dict, kCGWindowOwnerName),
        ) else {
            continue;
        };
        let pid = pid as c_int;
        if !out.iter().any(|(p, _)| *p == pid) {
            out.push((pid, name));
        }
    }
    out
}

/// Is this pid still alive?
fn pid_alive(pid: c_int) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve an application name to a running pid. Matches on-screen window
/// owners first (exact, then case-insensitive substring), then falls back to
/// `pgrep` so apps without an on-screen window are still reachable.
unsafe fn resolve_app_pid(name: &str) -> Option<(c_int, String)> {
    let owners = window_owners();
    let want = name.to_ascii_lowercase();
    if let Some((p, n)) = owners.iter().find(|(_, n)| n.to_ascii_lowercase() == want) {
        return Some((*p, n.clone()));
    }
    if let Some((p, n)) = owners
        .iter()
        .find(|(_, n)| n.to_ascii_lowercase().contains(&want))
    {
        return Some((*p, n.clone()));
    }
    let out = std::process::Command::new("/usr/bin/pgrep")
        .args(["-ix", name])
        .output()
        .ok()?;
    let pid = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()?
        .parse::<c_int>()
        .ok()?;
    Some((pid, name.to_string()))
}

/// Read an integer entry out of a CGWindowList dictionary.
unsafe fn dict_i64(dict: &CFDictionary, key: CFStringRef) -> Option<i64> {
    let raw = *dict.find(key.cast::<c_void>())?;
    if raw.is_null() {
        return None;
    }
    CFNumber::wrap_under_get_rule(raw as CFNumberRef).to_i64()
}

/// PID of the frontmost on-screen application, via the CoreGraphics window
/// list. The list is ordered front-to-back, so the first window on the normal
/// layer (0) belongs to the frontmost app; other layers are the menu bar, dock,
/// and overlays.
unsafe fn frontmost_pid() -> Option<c_int> {
    let arr = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    )?;
    for raw in arr.get_all_values() {
        if raw.is_null() {
            continue;
        }
        let dict = CFDictionary::wrap_under_get_rule(raw as _);
        if dict_i64(&dict, kCGWindowLayer) != Some(0) {
            continue;
        }
        if let Some(pid) = dict_i64(&dict, kCGWindowOwnerPID) {
            return Some(pid as c_int);
        }
    }
    None
}

/// The frontmost application's AX element.
///
/// `AXFocusedApplication` on the system-wide element is unreliable on modern
/// macOS — it frequently returns nothing even when an app is plainly frontmost
/// — so we try it first and fall back to resolving the frontmost PID from the
/// CoreGraphics window list. Callers must have checked `AXIsProcessTrusted`.
pub(crate) unsafe fn frontmost_app() -> Option<CFType> {
    let sys = AXUIElementCreateSystemWide();
    if !sys.is_null() {
        let sys = CFType::wrap_under_create_rule(sys);
        if let Some(app) = copy_attr(sys.as_CFTypeRef(), "AXFocusedApplication") {
            return Some(app);
        }
    }
    let app = AXUIElementCreateApplication(frontmost_pid()?);
    if app.is_null() {
        None
    } else {
        Some(CFType::wrap_under_create_rule(app))
    }
}

/// The frontmost application element, as a window-error result. Prefer
/// [`MacosBackend::target_app_element`], which follows the session target.
pub(crate) unsafe fn focused_app_element() -> Result<CFType, WindowError> {
    if AXIsProcessTrusted() == 0 {
        return Err(WindowError::PermissionDenied(
            "Accessibility permission not granted".into(),
        ));
    }
    frontmost_app().ok_or_else(|| WindowError::NotFound("no frontmost application".into()))
}

impl MacosBackend {
    /// The app element this session is driving: the sticky target when set and
    /// alive, else the frontmost app. Window/menu ops go through this so they
    /// stay on the same app as `get_ui_tree`.
    pub(crate) unsafe fn target_app_element(
        &self,
        app: Option<&str>,
    ) -> Result<CFType, WindowError> {
        if AXIsProcessTrusted() == 0 {
            return Err(WindowError::PermissionDenied(
                "Accessibility permission not granted".into(),
            ));
        }
        self.resolve_target(app)
            .map(|(elem, _, _)| elem)
            .map_err(WindowError::NotFound)
    }
}

unsafe fn copy_attr_bool(elem: CFTypeRef, name: &str) -> Option<bool> {
    let b = copy_attr(elem, name)?.downcast::<CFBoolean>()?;
    Some(b.as_concrete_TypeRef() == CFBoolean::true_value().as_concrete_TypeRef())
}

unsafe fn ax_action(elem: CFTypeRef, action: &str, what: &str) -> Result<(), WindowError> {
    let act = CFString::new(action);
    match AXUIElementPerformAction(elem, act.as_concrete_TypeRef()) {
        KAX_SUCCESS => Ok(()),
        KAX_ERROR_API_DISABLED => Err(WindowError::PermissionDenied("AX disabled".into())),
        e => Err(WindowError::Failed(format!("{what}: AXError {e}"))),
    }
}

unsafe fn set_bool_attr(elem: CFTypeRef, name: &str, val: bool) -> Result<(), WindowError> {
    let attr = CFString::new(name);
    let b = CFBoolean::from(val);
    match AXUIElementSetAttributeValue(elem, attr.as_concrete_TypeRef(), b.as_CFTypeRef()) {
        KAX_SUCCESS => Ok(()),
        e => Err(WindowError::Failed(format!("set {name}: AXError {e}"))),
    }
}

unsafe fn set_position(win: CFTypeRef, x: f64, y: f64) -> Result<(), WindowError> {
    let p = CGPoint { x, y };
    let axval = AXValueCreate(KAXVALUE_CGPOINT, &p as *const CGPoint as *const c_void);
    if axval.is_null() {
        return Err(WindowError::Failed("AXValueCreate position".into()));
    }
    let axval = CFType::wrap_under_create_rule(axval);
    let attr = CFString::new("AXPosition");
    match AXUIElementSetAttributeValue(win, attr.as_concrete_TypeRef(), axval.as_CFTypeRef()) {
        KAX_SUCCESS => Ok(()),
        e => Err(WindowError::Failed(format!("move: AXError {e}"))),
    }
}

unsafe fn set_size(win: CFTypeRef, w: f64, h: f64) -> Result<(), WindowError> {
    let s = CGSize {
        width: w,
        height: h,
    };
    let axval = AXValueCreate(KAXVALUE_CGSIZE, &s as *const CGSize as *const c_void);
    if axval.is_null() {
        return Err(WindowError::Failed("AXValueCreate size".into()));
    }
    let axval = CFType::wrap_under_create_rule(axval);
    let attr = CFString::new("AXSize");
    match AXUIElementSetAttributeValue(win, attr.as_concrete_TypeRef(), axval.as_CFTypeRef()) {
        KAX_SUCCESS => Ok(()),
        e => Err(WindowError::Failed(format!("resize: AXError {e}"))),
    }
}

pub(crate) unsafe fn get_windows(app: CFTypeRef) -> Vec<CFType> {
    match copy_attr(app, "AXWindows").and_then(|v| v.downcast::<CFArray>()) {
        Some(arr) => arr
            .get_all_values()
            .into_iter()
            .map(|p| CFType::wrap_under_get_rule(p))
            .collect(),
        None => Vec::new(),
    }
}

unsafe fn pick_window(app: CFTypeRef, title: Option<&str>) -> Result<CFType, WindowError> {
    let wins = get_windows(app);
    if let Some(t) = title {
        wins.into_iter()
            .find(|w| {
                copy_attr_string(w.as_CFTypeRef(), "AXTitle")
                    .as_deref()
                    .is_some_and(|s| s.contains(t))
            })
            .ok_or_else(|| WindowError::NotFound(format!("window '{t}' not found")))
    } else if let Some(mw) = copy_attr(app, "AXMainWindow") {
        Ok(mw)
    } else {
        wins.into_iter()
            .next()
            .ok_or_else(|| WindowError::NotFound("no windows".into()))
    }
}

unsafe fn window_info(el: CFTypeRef, index: u32, app: &Option<String>) -> WindowInfo {
    let bounds = read_bounds(el).map(|b| Rect {
        x: b.x,
        y: b.y,
        w: b.w,
        h: b.h,
    });
    WindowInfo {
        id: index,
        app: app.clone(),
        title: copy_attr_string(el, "AXTitle"),
        bounds,
        minimized: copy_attr_bool(el, "AXMinimized").unwrap_or(false),
    }
}

unsafe fn copy_attr_i64(elem: CFTypeRef, name: &str) -> Option<i64> {
    copy_attr(elem, name)?.downcast::<CFNumber>()?.to_i64()
}

/// Buttons, static text and secure-field presence inside a dialog subtree.
///
/// Bounded on both depth and node count: a dialog is small, and an unbounded
/// walk of something mis-identified as one would stall the whole session.
unsafe fn dialog_details(root: CFTypeRef) -> (Vec<String>, Vec<String>, bool) {
    const MAX_NODES: usize = 400;
    const MAX_DEPTH: u32 = 8;
    let (mut buttons, mut text, mut secure) = (Vec::new(), Vec::new(), false);
    let mut queue: VecDeque<(CFType, u32)> =
        copy_children(root).into_iter().map(|c| (c, 1)).collect();
    let mut seen = 0usize;
    while let Some((el, depth)) = queue.pop_front() {
        seen += 1;
        if seen > MAX_NODES {
            break;
        }
        let r = el.as_CFTypeRef();
        if copy_attr_string(r, "AXSubrole").as_deref() == Some("AXSecureTextField") {
            secure = true;
        }
        match copy_attr_string(r, "AXRole").as_deref() {
            Some("AXButton") => {
                if let Some(t) = copy_attr_string(r, "AXTitle").filter(|t| !t.is_empty()) {
                    if !buttons.contains(&t) {
                        buttons.push(t);
                    }
                }
            }
            Some("AXStaticText") => {
                if let Some(t) = copy_attr_string(r, "AXValue").filter(|t| !t.is_empty()) {
                    if !text.contains(&t) {
                        text.push(t);
                    }
                }
            }
            _ => {}
        }
        if depth < MAX_DEPTH {
            for c in copy_children(r) {
                queue.push_back((c, depth + 1));
            }
        }
    }
    (buttons, text, secure)
}

unsafe fn dialog_info(el: CFTypeRef, app: &Option<String>, kind: &str) -> DialogInfo {
    let (buttons, text, has_secure_field) = dialog_details(el);
    let named_button = |attr: &str| {
        copy_attr(el, attr).and_then(|b| copy_attr_string(b.as_CFTypeRef(), "AXTitle"))
    };
    DialogInfo {
        id: 0,
        app: app.clone(),
        title: copy_attr_string(el, "AXTitle"),
        kind: kind.to_string(),
        bounds: read_bounds(el).map(|b| Rect {
            x: b.x,
            y: b.y,
            w: b.w,
            h: b.h,
        }),
        buttons,
        text,
        default_button: named_button("AXDefaultButton"),
        cancel_button: named_button("AXCancelButton"),
        has_secure_field,
    }
}

/// Sheets, popovers and attached menus inside a window's subtree, paired with
/// the kind name to report.
///
/// These are *children of the window*, not windows themselves, which is why a
/// window-list scan alone never sees them. The `AXSheets` attribute looks like
/// the obvious route and is not: on a live TextEdit save panel it yields
/// nothing while the very same sheet sits in `AXChildren` as the last entry.
/// Bounded on depth and node count — an overlay hangs near the top of the
/// window it belongs to, and a runaway walk would stall the session.
unsafe fn find_overlays(window: CFTypeRef) -> Vec<(&'static str, CFType)> {
    const MAX_DEPTH: u32 = 4;
    const MAX_NODES: usize = 300;
    let mut found = Vec::new();
    let mut queue: VecDeque<(CFType, u32)> =
        copy_children(window).into_iter().map(|c| (c, 1)).collect();
    let mut seen = 0usize;
    while let Some((el, depth)) = queue.pop_front() {
        seen += 1;
        if seen > MAX_NODES {
            break;
        }
        let r = el.as_CFTypeRef();
        let kind = match copy_attr_string(r, "AXRole").as_deref() {
            Some("AXSheet") => Some("sheet"),
            Some("AXPopover") => Some("popover"),
            Some("AXMenu") => Some("menu"),
            _ => None,
        };
        if let Some(k) = kind {
            // Do not descend into an overlay looking for more overlays.
            found.push((k, el));
            continue;
        }
        if depth < MAX_DEPTH {
            for c in copy_children(r) {
                queue.push_back((c, depth + 1));
            }
        }
    }
    found
}

/// Every decision-demanding surface of one application.
///
/// Three sources, because macOS puts them in three different places: open menus
/// hang off the *application* element, sheets and popovers live **inside a
/// window's subtree**, and only free-standing dialogs are windows in their own
/// right. Filtering the window list by subrole — the obvious implementation —
/// sees the last of those three and misses every save panel and every popover.
unsafe fn collect_app_dialogs(app_elem: CFTypeRef, out: &mut Vec<DialogInfo>) {
    let name = copy_attr_string(app_elem, "AXTitle");

    for child in copy_children(app_elem) {
        if copy_attr_string(child.as_CFTypeRef(), "AXRole").as_deref() == Some("AXMenu") {
            out.push(dialog_info(child.as_CFTypeRef(), &name, "menu"));
        }
    }

    for w in get_windows(app_elem) {
        let wr = w.as_CFTypeRef();
        let role = copy_attr_string(wr, "AXRole");
        let sub = copy_attr_string(wr, "AXSubrole");
        let kind = match (role.as_deref(), sub.as_deref()) {
            (Some("AXSheet"), _) => Some("sheet"),
            (_, Some("AXSystemDialog")) => Some("alert"),
            (_, Some("AXDialog")) => Some("dialog"),
            (_, Some("AXSystemFloatingWindow")) => Some("popover"),
            _ => None,
        };
        if let Some(k) = kind {
            out.push(dialog_info(wr, &name, k));
        }
        for (kind, el) in find_overlays(wr) {
            out.push(dialog_info(el.as_CFTypeRef(), &name, kind));
        }
    }
}

/// Distinct PIDs owning an on-screen window, front to back.
///
/// This is how a system-scope sweep finds an authentication prompt: macOS
/// raises those from `SecurityAgent`, a process the calling app knows nothing
/// about.
unsafe fn on_screen_pids() -> Vec<c_int> {
    const MAX_APPS: usize = 32;
    let mut pids: Vec<c_int> = Vec::new();
    let Some(arr) = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
        return pids;
    };
    for raw in arr.get_all_values() {
        if raw.is_null() {
            continue;
        }
        let dict = CFDictionary::wrap_under_get_rule(raw as _);
        if let Some(pid) = dict_i64(&dict, kCGWindowOwnerPID) {
            let pid = pid as c_int;
            if !pids.contains(&pid) {
                pids.push(pid);
            }
        }
        if pids.len() >= MAX_APPS {
            break;
        }
    }
    pids
}

/// Resolve a menu container (the menu bar itself for an empty path, otherwise
/// the `AXMenu` under the named item) without pressing anything.
unsafe fn menu_container(app: CFTypeRef, path: &[String]) -> Result<CFType, WindowError> {
    let mut container =
        copy_attr(app, "AXMenuBar").ok_or_else(|| WindowError::NotFound("no menu bar".into()))?;
    for name in path {
        let item = copy_children(container.as_CFTypeRef())
            .into_iter()
            .find(|el| {
                copy_attr_string(el.as_CFTypeRef(), "AXTitle").as_deref() == Some(name.as_str())
            })
            .ok_or_else(|| WindowError::NotFound(format!("menu item '{name}' not found")))?;
        container = copy_children(item.as_CFTypeRef())
            .into_iter()
            .find(|c| copy_attr_string(c.as_CFTypeRef(), "AXRole").as_deref() == Some("AXMenu"))
            .ok_or_else(|| WindowError::NotFound(format!("'{name}' has no submenu")))?;
    }
    Ok(container)
}

/// A menu item's keyboard equivalent, in `keyboard_shortcut` syntax.
///
/// The Command bit is inverted: bit 3 set means "no Command", so a mask of 0 is
/// a plain Cmd-key chord.
unsafe fn menu_shortcut(item: CFTypeRef) -> Option<String> {
    let ch = copy_attr_string(item, "AXMenuItemCmdChar").filter(|c| !c.is_empty())?;
    let m = copy_attr_i64(item, "AXMenuItemCmdModifiers").unwrap_or(0);
    Some(shortcut_combo(&ch, m))
}

/// Decode an `AXMenuItemCmdModifiers` mask into `keyboard_shortcut` syntax.
///
/// Bit 3 is the trap: it means *no* Command, so the common case (mask 0) is a
/// plain Cmd chord. Reading it as a normal "has modifier" bit inverts every
/// shortcut in the menu.
fn shortcut_combo(ch: &str, modifiers: i64) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if modifiers & 8 == 0 {
        parts.push("cmd");
    }
    if modifiers & 1 != 0 {
        parts.push("shift");
    }
    if modifiers & 2 != 0 {
        parts.push("alt");
    }
    if modifiers & 4 != 0 {
        parts.push("ctrl");
    }
    let lowered = ch.to_lowercase();
    parts.push(&lowered);
    parts.join("+")
}

/// Walk a menu container, recording every item. `depth` counts levels still to
/// descend; 1 means "this level only".
unsafe fn collect_menu(
    container: CFTypeRef,
    prefix: &[String],
    depth: u32,
    out: &mut Vec<MenuItemInfo>,
) {
    const MAX_ITEMS: usize = 600;
    for item in copy_children(container) {
        if out.len() >= MAX_ITEMS {
            return;
        }
        let r = item.as_CFTypeRef();
        // Separators carry no title; they are not addressable, so skip them.
        let Some(title) = copy_attr_string(r, "AXTitle").filter(|t| !t.is_empty()) else {
            continue;
        };
        let submenu = copy_children(r)
            .into_iter()
            .find(|c| copy_attr_string(c.as_CFTypeRef(), "AXRole").as_deref() == Some("AXMenu"));
        let mut path = prefix.to_vec();
        path.push(title.clone());
        out.push(MenuItemInfo {
            title,
            enabled: copy_attr_bool(r, "AXEnabled").unwrap_or(true),
            has_submenu: submenu.is_some(),
            shortcut: menu_shortcut(r),
            path: path.clone(),
        });
        if depth > 1 {
            if let Some(sub) = submenu {
                collect_menu(sub.as_CFTypeRef(), &path, depth - 1, out);
            }
        }
    }
}

/// Fill the window's display, leaving the menu bar visible.
///
/// Not the green button: that toggles *full screen*, which moves the window to
/// its own Space and hides the menu bar — a different thing, and one an agent
/// then has to undo before it can see anything else. Zoom is used only as a
/// fallback when the window refuses to be positioned.
unsafe fn maximize_window(win: CFTypeRef) -> Result<(), WindowError> {
    use core_graphics::display::CGDisplay;

    let bounds = read_bounds(win);
    // Pick the display the window is actually on, by centre point.
    let (cx, cy) = match bounds {
        Some(b) => (b.x + b.w / 2.0, b.y + b.h / 2.0),
        None => (0.0, 0.0),
    };
    let mut target = CGDisplay::main().bounds();
    if let Ok(ids) = CGDisplay::active_displays() {
        for id in ids {
            let r = CGDisplay::new(id).bounds();
            if cx >= r.origin.x
                && cx < r.origin.x + r.size.width
                && cy >= r.origin.y
                && cy < r.origin.y + r.size.height
            {
                target = r;
                break;
            }
        }
    }
    // The menu bar occupies the top of the primary display's coordinate space.
    // AX gives no visible-frame attribute, so the inset is applied explicitly;
    // a window placed under the menu bar has its title bar unreachable.
    const MENU_BAR: f64 = 25.0;
    let on_primary = target.origin.y == 0.0;
    let y = if on_primary {
        MENU_BAR
    } else {
        target.origin.y
    };
    let h = target.size.height - if on_primary { MENU_BAR } else { 0.0 };

    let pos = set_position(win, target.origin.x, y);
    let size = set_size(win, target.size.width, h);
    if pos.is_ok() && size.is_ok() {
        return Ok(());
    }
    // Some windows (sheets, fixed-size panels) refuse to be moved or resized.
    // Their own zoom button is the only thing left that means "make this big".
    if let Some(btn) = copy_attr(win, "AXZoomButton") {
        return ax_action(btn.as_CFTypeRef(), "AXPress", "maximize");
    }
    pos.and(size)
}

unsafe fn navigate_menu(
    app: CFTypeRef,
    path: &[String],
    invoke_last: bool,
) -> Result<(), WindowError> {
    let menubar =
        copy_attr(app, "AXMenuBar").ok_or_else(|| WindowError::NotFound("no menu bar".into()))?;
    let mut container = menubar;
    for (i, name) in path.iter().enumerate() {
        let item = copy_children(container.as_CFTypeRef())
            .into_iter()
            .find(|el| {
                copy_attr_string(el.as_CFTypeRef(), "AXTitle").as_deref() == Some(name.as_str())
            })
            .ok_or_else(|| WindowError::NotFound(format!("menu item '{name}' not found")))?;
        let is_last = i == path.len() - 1;
        if is_last {
            return ax_action(
                item.as_CFTypeRef(),
                "AXPress",
                if invoke_last {
                    "menu_invoke"
                } else {
                    "menu_open"
                },
            );
        }
        ax_action(item.as_CFTypeRef(), "AXPress", "menu_open")?;
        container = copy_children(item.as_CFTypeRef())
            .into_iter()
            .find(|c| copy_attr_string(c.as_CFTypeRef(), "AXRole").as_deref() == Some("AXMenu"))
            .ok_or_else(|| WindowError::NotFound(format!("submenu of '{name}' not found")))?;
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> Result<std::process::Output, WindowError> {
    std::process::Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| WindowError::Failed(format!("{cmd}: {e}")))
}

#[async_trait]
impl WindowBackend for MacosBackend {
    async fn list_windows(&self, app_name: Option<&str>) -> Result<Vec<WindowInfo>, WindowError> {
        unsafe {
            // Honour the requested app. Resolving `None` here would silently
            // answer about whatever happens to be frontmost, so
            // `list_windows { app: "TextEdit" }` returned the caller's own
            // terminal — wrong, and wrong without saying so.
            let app = self.target_app_element(app_name)?;
            let name = copy_attr_string(app.as_CFTypeRef(), "AXTitle");
            Ok(get_windows(app.as_CFTypeRef())
                .iter()
                .enumerate()
                .map(|(i, w)| window_info(w.as_CFTypeRef(), i as u32, &name))
                .collect())
        }
    }

    async fn list_apps(&self) -> Result<Vec<String>, WindowError> {
        let out = run("ps", &["-Axo", "comm="])?;
        let text = String::from_utf8_lossy(&out.stdout);
        let mut apps: Vec<String> = Vec::new();
        for line in text.lines() {
            let name = if let Some(idx) = line.find(".app/") {
                let before = &line[..idx];
                before.rsplit('/').next().unwrap_or(before).to_string()
            } else {
                line.rsplit('/').next().unwrap_or(line).to_string()
            };
            if !name.is_empty() && !apps.contains(&name) {
                apps.push(name);
            }
        }
        apps.sort();
        Ok(apps)
    }

    async fn launch(&self, app: &str) -> Result<(), WindowError> {
        let out = run("open", &["-a", app])?;
        if out.status.success() {
            Ok(())
        } else {
            Err(WindowError::NotFound(format!("could not launch '{app}'")))
        }
    }

    async fn close_app(&self, target: &str) -> Result<(), WindowError> {
        let out = run("pkill", &["-ix", target])?;
        if out.status.success() {
            Ok(())
        } else {
            Err(WindowError::NotFound(format!(
                "no running app matching '{target}'"
            )))
        }
    }

    async fn control_window(
        &self,
        _app: Option<&str>,
        title: Option<&str>,
        action: WindowAction,
        bounds: Option<Rect>,
    ) -> Result<(), WindowError> {
        unsafe {
            let app = self.target_app_element(None)?;
            let win = pick_window(app.as_CFTypeRef(), title)?;
            let wref = win.as_CFTypeRef();
            match action {
                WindowAction::Focus => {
                    let _ = set_bool_attr(wref, "AXMain", true);
                    ax_action(wref, "AXRaise", "focus")
                }
                WindowAction::Minimize => set_bool_attr(wref, "AXMinimized", true),
                WindowAction::Restore => set_bool_attr(wref, "AXMinimized", false),
                WindowAction::Close => {
                    let btn = copy_attr(wref, "AXCloseButton")
                        .ok_or_else(|| WindowError::Unsupported("no close button".into()))?;
                    ax_action(btn.as_CFTypeRef(), "AXPress", "close")
                }
                WindowAction::Move => {
                    let b =
                        bounds.ok_or_else(|| WindowError::Failed("move needs bounds".into()))?;
                    set_position(wref, b.x, b.y)
                }
                WindowAction::Resize => {
                    let b =
                        bounds.ok_or_else(|| WindowError::Failed("resize needs bounds".into()))?;
                    set_size(wref, b.w, b.h)
                }
                WindowAction::Maximize => maximize_window(wref),
            }
        }
    }

    async fn menu_open(&self, _app: Option<&str>, path: &[String]) -> Result<(), WindowError> {
        unsafe {
            let app = self.target_app_element(None)?;
            navigate_menu(app.as_CFTypeRef(), path, false)
        }
    }

    async fn menu_invoke(&self, _app: Option<&str>, path: &[String]) -> Result<(), WindowError> {
        unsafe {
            let app = self.target_app_element(None)?;
            navigate_menu(app.as_CFTypeRef(), path, true)
        }
    }

    async fn focus_app(&self, app: Option<&str>) -> Result<Option<String>, WindowError> {
        let Some(name) = app else {
            self.clear_target();
            return Ok(None);
        };
        unsafe {
            // Resolving with an explicit name pins the session target.
            let elem = self.target_app_element(Some(name))?;
            // Bring it forward so keyboard/coordinate input lands there too.
            let _ = set_bool_attr(elem.as_CFTypeRef(), "AXFrontmost", true);
        }
        Ok(self.current_target().map(|(_, n)| n))
    }

    async fn list_dialogs(
        &self,
        app: Option<&str>,
        scope: DialogScope,
    ) -> Result<Vec<DialogInfo>, WindowError> {
        unsafe {
            let mut out = Vec::new();
            match scope {
                DialogScope::App => {
                    let elem = self.target_app_element(app)?;
                    collect_app_dialogs(elem.as_CFTypeRef(), &mut out);
                }
                DialogScope::System => {
                    if AXIsProcessTrusted() == 0 {
                        return Err(WindowError::PermissionDenied(
                            "Accessibility permission not granted".into(),
                        ));
                    }
                    for pid in on_screen_pids() {
                        let raw = AXUIElementCreateApplication(pid);
                        if raw.is_null() {
                            continue;
                        }
                        let elem = CFType::wrap_under_create_rule(raw);
                        collect_app_dialogs(elem.as_CFTypeRef(), &mut out);
                    }
                }
            }
            for (i, d) in out.iter_mut().enumerate() {
                d.id = i as u32;
            }
            Ok(out)
        }
    }

    async fn menu_list(
        &self,
        app: Option<&str>,
        path: &[String],
        depth: u32,
    ) -> Result<Vec<MenuItemInfo>, WindowError> {
        unsafe {
            let elem = self.target_app_element(app)?;
            let container = menu_container(elem.as_CFTypeRef(), path)?;
            let mut out = Vec::new();
            collect_menu(container.as_CFTypeRef(), path, depth.clamp(1, 5), &mut out);
            Ok(out)
        }
    }

    fn platform(&self) -> &'static str {
        "macos"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_a11y::A11yModule;
    use mcp_types::{CallCtx, CancelToken, ErrorCode, ToolModule};
    use serde_json::json;
    use std::sync::Arc;

    /// Real backend through the real module. Without the TCC Accessibility grant
    /// this returns PERM_DENIED (or NotFound if no focused app); with it, an ok
    /// tree. Either is a valid, non-panicking outcome — proves the FFI is sound.
    #[tokio::test]
    async fn get_ui_tree_returns_wellformed_envelope() {
        let module = A11yModule::new(Arc::new(MacosBackend::new()), 12_000);
        let ctx = CallCtx::new("test", CancelToken::new());
        let env = module.call("get_ui_tree", json!({}), &ctx).await;
        if env.ok {
            assert!(env.data.unwrap().get("text").is_some());
        } else {
            let code = env.error.unwrap().code;
            assert!(
                matches!(code, ErrorCode::PermDenied | ErrorCode::NotFound),
                "unexpected: {code:?}"
            );
        }
    }

    /// Acting on a ref before any snapshot exercises the real `element_for` path
    /// (no pid/path stored) → NotFound. Deterministic, no permission needed.
    #[tokio::test]
    async fn action_before_snapshot_is_not_found() {
        let backend = MacosBackend::new();
        let r = backend.perform(999, SemanticAction::Click, None).await;
        assert!(matches!(r, Err(InputError::NotFound(_))));
    }

    /// An unknown app name must be a clean NotFound, never a panic or a
    /// silent fall-through to whatever app happens to be frontmost (which
    /// would let a typo drive the wrong application).
    /// `list_windows { app }` must answer about *that* app.
    ///
    /// It used to discard the argument and resolve whatever was frontmost, so
    /// asking about TextEdit returned the caller's own terminal window — the
    /// wrong answer, given confidently. `wait_for { window }` is built on this,
    /// so it could never match a window of an app that was not already in
    /// front.
    #[tokio::test]
    async fn list_windows_answers_about_the_requested_app() {
        let b = MacosBackend::new();
        match mcp_window::WindowBackend::list_windows(&b, Some("Finder")).await {
            Ok(ws) => {
                for w in &ws {
                    assert_eq!(
                        w.app.as_deref(),
                        Some("Finder"),
                        "every window returned must belong to the app that was asked about"
                    );
                }
            }
            // No permission, or Finder has no windows open: both are answers.
            Err(WindowError::PermissionDenied(_)) | Err(WindowError::NotFound(_)) => {}
            Err(e) => panic!("unexpected error: {e:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_app_target_is_not_found_not_a_fallback() {
        let backend = MacosBackend::new();
        let r = WindowBackend::focus_app(&backend, Some("NoSuchApplication-zzz")).await;
        match r {
            Err(WindowError::NotFound(_)) | Err(WindowError::PermissionDenied(_)) => {}
            other => panic!("expected NotFound/PermissionDenied, got {other:?}"),
        }
        assert!(
            backend.current_target().is_none(),
            "a failed resolution must not pin a target"
        );
    }

    /// Releasing the pin clears the target so calls track the frontmost app again.
    #[tokio::test]
    async fn focus_app_none_releases_the_pin() {
        let backend = MacosBackend::new();
        backend.set_target(12345, "Fake");
        assert!(backend.current_target().is_some());
        let r = WindowBackend::focus_app(&backend, None).await;
        assert!(matches!(r, Ok(None)));
        assert!(backend.current_target().is_none());
    }

    /// A dead sticky target must not wedge the session: resolution falls back
    /// to the frontmost app (or reports no frontmost app) rather than erroring
    /// on the stale pid forever.
    #[test]
    fn dead_target_is_dropped_rather_than_wedging_the_session() {
        let backend = MacosBackend::new();
        backend.set_target(999_999, "GoneApp"); // pid that cannot be alive
        let _ = unsafe { backend.resolve_target(None) };
        let t = backend.current_target();
        assert!(
            t.map(|(p, _)| p) != Some(999_999),
            "dead target should have been dropped"
        );
    }

    /// Regression test for the frontmost-app fallback. `AXFocusedApplication`
    /// on the system-wide element frequently returns nothing on modern macOS
    /// even when an app is plainly frontmost, which made every a11y/window tool
    /// fail with "no focused application"; we now fall back to the CoreGraphics
    /// window list. This must be memory-safe and yield either a live PID or
    /// `None` (no GUI session).
    /// Mask 0 is a plain Cmd chord — bit 3 means *no* Command, not "has" it.
    /// Getting this backwards silently inverts every shortcut in the menu.
    #[test]
    fn menu_modifier_mask_treats_command_as_inverted() {
        assert_eq!(shortcut_combo("S", 0), "cmd+s");
        assert_eq!(shortcut_combo("S", 1), "cmd+shift+s");
        assert_eq!(shortcut_combo("S", 2), "cmd+alt+s");
        assert_eq!(shortcut_combo("S", 4), "cmd+ctrl+s");
        assert_eq!(shortcut_combo("S", 3), "cmd+shift+alt+s");
        // Bit 3 set: no Command at all (e.g. a bare function-key equivalent).
        assert_eq!(shortcut_combo("S", 8), "s");
        assert_eq!(shortcut_combo("S", 9), "shift+s");
    }

    /// Enumerating a menu bar must not require pressing anything: opening menus
    /// to read them would leave the UI in a changed state after a "read" tool.
    #[tokio::test]
    async fn menu_list_reads_a_live_app_without_opening_menus() {
        let backend = MacosBackend::new();
        if unsafe { AXIsProcessTrusted() } == 0 {
            return; // no Accessibility permission in this environment
        }
        let Ok(items) = WindowBackend::menu_list(&backend, None, &[], 1).await else {
            return; // nothing frontmost / not addressable
        };
        assert!(!items.is_empty(), "a frontmost app must expose a menu bar");
        // The application menu is always first and always has a submenu.
        assert!(items[0].has_submenu, "first menu-bar item must open a menu");
        for it in &items {
            assert_eq!(it.path.len(), 1, "depth 1 must not descend: {:?}", it.path);
            assert!(!it.title.is_empty(), "separators must be skipped");
        }
    }

    /// Depth 2 descends, and the paths it returns are exactly what menu_invoke
    /// consumes — that round-trip is the point of the tool.
    #[tokio::test]
    async fn menu_list_depth_two_yields_invocable_paths() {
        let backend = MacosBackend::new();
        if unsafe { AXIsProcessTrusted() } == 0 {
            return;
        }
        let Ok(items) = WindowBackend::menu_list(&backend, None, &[], 2).await else {
            return;
        };
        let nested: Vec<_> = items.iter().filter(|i| i.path.len() == 2).collect();
        if nested.is_empty() {
            return; // app populates submenus lazily
        }
        assert!(
            nested.iter().any(|i| i.shortcut.is_some()),
            "some menu item must advertise a keyboard equivalent"
        );
        for i in &nested {
            let combo = i.shortcut.as_deref().unwrap_or("cmd+x");
            assert!(!combo.starts_with('+'), "malformed combo '{combo}'");
        }
    }

    /// A system sweep must reach processes the caller never named — that is the
    /// only way an authentication prompt (raised by SecurityAgent, not by the
    /// app that triggered it) is ever visible.
    #[tokio::test]
    async fn system_dialog_scope_does_not_error_and_stays_bounded() {
        let backend = MacosBackend::new();
        if unsafe { AXIsProcessTrusted() } == 0 {
            return;
        }
        let found = WindowBackend::list_dialogs(&backend, None, DialogScope::System).await;
        let dialogs = found.expect("system scope must not error on a live desktop");
        assert!(dialogs.len() < 200, "sweep must stay bounded");
        for (i, d) in dialogs.iter().enumerate() {
            assert_eq!(d.id as usize, i, "ids must be dense and ordered");
            assert!(
                ["dialog", "alert", "sheet", "popover", "menu"].contains(&d.kind.as_str()),
                "unknown dialog kind '{}'",
                d.kind
            );
        }
    }

    /// On-screen PIDs are the sweep's work list: it must be a real, deduplicated
    /// set of live processes, not a growing list of repeats per window.
    #[test]
    fn on_screen_pids_are_unique_and_live() {
        let pids = unsafe { on_screen_pids() };
        let mut sorted = pids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), pids.len(), "pids must be deduplicated");
        assert!(pids.len() <= 32, "sweep must cap the app count");
    }

    #[test]
    fn frontmost_pid_resolves_to_a_live_process() {
        let Some(pid) = (unsafe { frontmost_pid() }) else {
            return; // headless / no on-screen windows — nothing to assert
        };
        assert!(pid > 0, "frontmost pid must be positive, got {pid}");
        let out = std::process::Command::new("/bin/ps")
            .args(["-p", &pid.to_string()])
            .output()
            .expect("run ps");
        assert!(
            out.status.success(),
            "frontmost pid {pid} is not a live process"
        );
    }
}
