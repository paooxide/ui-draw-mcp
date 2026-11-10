use std::collections::{HashMap, VecDeque};
use std::os::raw::{c_int, c_void};
use std::ptr;
use std::sync::Mutex;

use async_trait::async_trait;
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::{CFNumber, CFNumberRef};
use core_foundation::string::{CFString, CFStringRef};
use core_foundation_sys::base::{Boolean, CFRange};
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowAlpha, kCGWindowBounds, kCGWindowIsOnscreen,
    kCGWindowLayer, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenAboveWindow,
    kCGWindowListOptionOnScreenOnly, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
    kCGWindowOwnerPID,
};

use mcp_a11y::{
    is_interactive_role, A11yBackend, BackendError, Bounds, RawSnapshot, SnapshotRequest, UiNode,
};
use mcp_input::{
    char_range_to_utf16, char_slice, utf16_range_to_char, Choice, ClipData, ClipFormat,
    InputBackend, InputError, MouseKind, Reading, Route, ScrollDir, Selection, SemanticAction,
};
use mcp_window::{
    DialogInfo, DialogScope, MenuItemInfo, Rect, TargetInfo, TargetSource, WindowAction,
    WindowBackend, WindowError, WindowInfo, WindowState,
};

// ---- AXUIElement FFI (ApplicationServices framework) ------------------------

type AXUIElementRef = CFTypeRef;
pub(crate) type AXError = i32;

pub(crate) const KAX_SUCCESS: AXError = 0;
pub(crate) const KAX_ERROR_API_DISABLED: AXError = -25211;
pub(crate) const KAX_ERROR_INVALID_ELEMENT: AXError = -25202;
pub(crate) const KAX_ERROR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;
pub(crate) const KAX_ERROR_ACTION_UNSUPPORTED: AXError = -25206;
pub(crate) const KAX_ERROR_CANNOT_COMPLETE: AXError = -25204;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCreateApplication(pid: c_int) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    pub(crate) fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    pub(crate) fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef)
        -> AXError;
    /// The actions an element advertises (create rule).
    fn AXUIElementCopyActionNames(element: AXUIElementRef, names: *mut CFArrayRef) -> AXError;
    fn AXUIElementIsAttributeSettable(
        element: AXUIElementRef,
        attribute: CFStringRef,
        settable: *mut Boolean,
    ) -> AXError;
    /// Bound how long a request to the target app may take. Opening a popup
    /// can run the app's menu tracking, and without a bound a stuck app would
    /// hold this thread for the system default.
    pub(crate) fn AXUIElementSetMessagingTimeout(
        element: AXUIElementRef,
        timeout_seconds: f32,
    ) -> AXError;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut c_int) -> AXError;
    fn AXIsProcessTrusted() -> Boolean;
    fn AXValueGetValue(value: CFTypeRef, the_type: u32, value_ptr: *mut c_void) -> Boolean;
    fn AXValueCreate(the_type: u32, value_ptr: *const c_void) -> CFTypeRef;
}

// AXValue wrapped geometry types.
const KAXVALUE_CGPOINT: u32 = 1;
const KAXVALUE_CGSIZE: u32 = 2;
const KAXVALUE_CFRANGE: u32 = 4;

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

pub(crate) unsafe fn copy_attr(elem: CFTypeRef, name: &str) -> Option<CFType> {
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
pub(crate) unsafe fn copy_attr_string(elem: CFTypeRef, name: &str) -> Option<String> {
    copy_attr(elem, name)?
        .downcast::<CFString>()
        .map(|s| s.to_string())
}

/// Copy the children of an element as owned CF elements.
pub(crate) unsafe fn copy_children(elem: CFTypeRef) -> Vec<CFType> {
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

pub(crate) fn ax_result(err: AXError, what: &str) -> Result<(), InputError> {
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

/// The actions an element advertises. `None` when the list cannot be read,
/// which is not the same as an empty list.
unsafe fn action_names(elem: CFTypeRef) -> Option<Vec<String>> {
    let mut raw: CFArrayRef = ptr::null();
    let err = AXUIElementCopyActionNames(elem, &mut raw);
    if err != KAX_SUCCESS || raw.is_null() {
        return None;
    }
    // Copy rule: the array is ours to release.
    let array: CFArray = CFArray::wrap_under_create_rule(raw);
    Some(
        array
            .get_all_values()
            .into_iter()
            .filter_map(|p| CFType::wrap_under_get_rule(p).downcast::<CFString>())
            .map(|s| s.to_string())
            .collect(),
    )
}

/// What to call an element in an error: its title, else its description, else
/// its role.
unsafe fn element_label(elem: CFTypeRef) -> String {
    ["AXTitle", "AXDescription", "AXRole"]
        .iter()
        .find_map(|a| copy_attr_string(elem, a).filter(|s| !s.is_empty()))
        .unwrap_or_default()
}

/// Perform an AX action on an element.
///
/// With `check_list`, the element's own action list is consulted first, so an
/// element that lacks the action is told apart from one that failed it. Either
/// way, an element that reports the action unsupported gets an error naming
/// what it does offer.
unsafe fn perform_ax_action(elem: CFTypeRef, ax: &str, check_list: bool) -> Result<(), InputError> {
    if check_list {
        if let Some(offered) = action_names(elem) {
            if !offered.iter().any(|a| a == ax) {
                return Err(InputError::Unsupported(
                    crate::ax_actions::unsupported_message(&element_label(elem), ax, &offered),
                ));
            }
        }
    }
    let act = CFString::new(ax);
    match AXUIElementPerformAction(elem, act.as_concrete_TypeRef()) {
        KAX_ERROR_ACTION_UNSUPPORTED => Err(InputError::Unsupported(
            crate::ax_actions::unsupported_message(
                &element_label(elem),
                ax,
                &action_names(elem).unwrap_or_default(),
            ),
        )),
        err => ax_result(err, ax.trim_start_matches("AX")),
    }
}

/// Read an `AXValue`-wrapped `CFRange` attribute as UTF-16 `(location, length)`.
unsafe fn copy_attr_range(elem: CFTypeRef, name: &str) -> Option<(usize, usize)> {
    let v = copy_attr(elem, name)?;
    let mut r = CFRange {
        location: 0,
        length: 0,
    };
    let ok = AXValueGetValue(
        v.as_CFTypeRef(),
        KAXVALUE_CFRANGE,
        &mut r as *mut CFRange as *mut c_void,
    );
    if ok == 0 || r.location < 0 || r.length < 0 {
        return None;
    }
    Some((r.location as usize, r.length as usize))
}

/// Whether an attribute can be written. A failed query counts as no.
unsafe fn is_settable(elem: CFTypeRef, name: &str) -> bool {
    let attr = CFString::new(name);
    let mut settable: Boolean = 0;
    AXUIElementIsAttributeSettable(elem, attr.as_concrete_TypeRef(), &mut settable) == KAX_SUCCESS
        && settable != 0
}

/// The text selection an element reports, in characters.
///
/// A password field is refused before anything is read from it.
unsafe fn read_selection(elem: CFTypeRef) -> Result<Selection, InputError> {
    if copy_attr_string(elem, "AXSubrole").as_deref() == Some("AXSecureTextField") {
        return Err(InputError::InvalidArgs(
            "this is a password field; its text is never read".into(),
        ));
    }
    let text = copy_attr_string(elem, "AXValue")
        .ok_or_else(|| InputError::Unsupported("the element has no text".into()))?;
    let (loc, len) = copy_attr_range(elem, "AXSelectedTextRange").ok_or_else(|| {
        InputError::Unsupported("the element does not report a selected text range".into())
    })?;
    let (start, end) = utf16_range_to_char(&text, loc, len).ok_or_else(|| {
        InputError::Failed(format!(
            "the element reports a selection ({loc}, {len} in UTF-16 units) that does not fall \
             on character boundaries of its text"
        ))
    })?;
    let selected =
        copy_attr_string(elem, "AXSelectedText").unwrap_or_else(|| char_slice(&text, start, end));
    Ok(Selection {
        start,
        end,
        text: selected,
        role: copy_attr_string(elem, "AXRole").map(|r| r.trim_start_matches("AX").to_string()),
    })
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
    /// The app this session is driving. Set by `focus_app`, or by a
    /// `get_ui_tree`-family call that names an `app` (or that names none and
    /// finds nothing pinned, which pins the frontmost app), and then
    /// *followed* — later calls stay on it instead of tracking whatever
    /// happens to be frontmost. Window, menu and capture calls that name an
    /// app act on it without changing this.
    target: Option<AppTarget>,
}

/// A resolved application target.
#[derive(Clone)]
struct AppTarget {
    pid: c_int,
    name: String,
}

/// An application a call resolved to, and which of name, pin or frontmost
/// chose it.
pub(crate) struct Resolved {
    pub(crate) pid: c_int,
    pub(crate) name: String,
    pub(crate) from: TargetSource,
}

impl Resolved {
    pub(crate) fn info(&self) -> TargetInfo {
        TargetInfo {
            app: self.name.clone(),
            pid: u32::try_from(self.pid).ok(),
            from: self.from,
        }
    }
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
    /// Counts requests to stop in-flight work — the human-override watcher
    /// raises it. A drag compares it with the value its call started at,
    /// between steps; it would otherwise keep the button held while the person
    /// moves the mouse. Never reset: see `InputBackend::takeover_generation`.
    takeovers: std::sync::atomic::AtomicU64,
    /// Which mouse buttons this backend holds down, so a `move` between a
    /// `down` and an `up` goes out as the drag event it is.
    held: crate::event::Held,
    /// Keys held down across calls; their modifiers ride on every event.
    keys: crate::event::HeldKeys,
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
            takeovers: std::sync::atomic::AtomicU64::new(0),
            helper_dir: default_helper_dir(),
            held: crate::event::Held::default(),
            keys: crate::event::HeldKeys::default(),
        }
    }

    /// Cache the compiled OCR helper somewhere other than the default.
    pub fn with_helper_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.helper_dir = dir.into();
        self
    }

    /// The pinned app, if its process is still alive. A pin whose process has
    /// gone is dropped here, so it cannot wedge the session on a stale pid.
    fn live_pin(&self) -> Option<AppTarget> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let t = st.target.clone()?;
        if pid_alive(t.pid) {
            Some(t)
        } else {
            st.target = None;
            None
        }
    }

    /// Which app a call would act on, and why that one, with nothing pinned
    /// or changed. A name given to the call wins; otherwise the pinned app
    /// while it lives; otherwise the frontmost one. A name that matches no
    /// running app is an error, never a fall-through to another app: that is
    /// how a typo used to drive whatever was in front.
    unsafe fn peek_target(&self, app: Option<&str>) -> Result<Resolved, String> {
        let pin = self.live_pin();
        match mcp_window::target_source(app, pin.is_some()) {
            TargetSource::Named => {
                let name = app.unwrap_or_default().trim();
                let (pid, resolved) = resolve_app_pid(name)
                    .ok_or_else(|| format!("no running application matching '{name}'"))?;
                Ok(Resolved {
                    pid,
                    name: resolved,
                    from: TargetSource::Named,
                })
            }
            TargetSource::Pinned => pin
                .map(|t| Resolved {
                    pid: t.pid,
                    name: t.name,
                    from: TargetSource::Pinned,
                })
                .ok_or_else(|| "the pinned application is no longer running".to_string()),
            TargetSource::Frontmost => {
                let (pid, name) = frontmost_identity().ok_or("no frontmost application")?;
                Ok(Resolved {
                    pid,
                    name,
                    from: TargetSource::Frontmost,
                })
            }
        }
    }

    /// Resolve which app to drive, and remember it.
    ///
    /// An explicit `app` wins and becomes the pinned target; otherwise we reuse
    /// the pinned target while its process is alive; otherwise we fall back to
    /// the frontmost app, which then becomes the pin. This is what makes the
    /// server *follow* the app it is working on rather than tracking whatever
    /// steals focus. It is for perception and the actions on its refs; the
    /// window, menu and capture tools use [`Self::peek_app`], which pins nothing.
    unsafe fn resolve_target(&self, app: Option<&str>) -> Result<(CFType, Resolved), String> {
        let r = self.peek_target(app)?;
        let elem = app_element(r.pid).ok_or("cannot open application")?;
        if r.from != TargetSource::Pinned {
            self.set_target(r.pid, &r.name);
        }
        Ok((elem, r))
    }

    /// The app a window, menu or dialog call acts on, as an AX element, with
    /// nothing pinned. Naming an app in one of those calls is saying what to
    /// act on, not choosing to drive it from now on.
    unsafe fn peek_app(&self, app: Option<&str>) -> Result<(CFType, Resolved), WindowError> {
        if AXIsProcessTrusted() == 0 {
            return Err(WindowError::PermissionDenied(
                "Accessibility permission not granted".into(),
            ));
        }
        let r = self.peek_target(app).map_err(WindowError::NotFound)?;
        let elem = app_element(r.pid)
            .ok_or_else(|| WindowError::Failed("cannot open application".into()))?;
        Ok((elem, r))
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
    pub(crate) unsafe fn element_for(&self, node_id: u64) -> Result<CFType, InputError> {
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
            // A passive read (a wait, a check) names the app it reads without
            // becoming it.
            let (app, resolved) = if req.passive {
                let r = self
                    .peek_target(req.app.as_deref())
                    .map_err(BackendError::NotFound)?;
                let elem = app_element(r.pid)
                    .ok_or_else(|| BackendError::NotFound("cannot open application".into()))?;
                (elem, r)
            } else {
                self.resolve_target(req.app.as_deref())
                    .map_err(BackendError::NotFound)?
            };
            let (pid, target_name) = (resolved.pid, resolved.name.clone());

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
            // The refs of this walk are what ui_action and the rest resolve
            // against. A passive read keeps the previous walk's: replacing it
            // would leave `@e5` of the last get_ui_tree pointing into
            // another application.
            if !req.passive {
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
                target: Some(mcp_a11y::SnapshotTarget {
                    pid: u32::try_from(pid).ok(),
                    from: resolved.from.as_str(),
                }),
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
    ) -> Result<Route, InputError> {
        unsafe {
            let elem = self.element_for(node_id)?;
            let eref = elem.as_CFTypeRef();
            // Every branch below goes through the accessibility API: nothing
            // here moves the pointer or needs the app frontmost.
            match crate::ax_actions::ax_action_name(action) {
                None => {
                    let attr = CFString::new("AXFocused");
                    let val = CFBoolean::from(true);
                    let err = AXUIElementSetAttributeValue(
                        eref,
                        attr.as_concrete_TypeRef(),
                        val.as_CFTypeRef(),
                    );
                    ax_result(err, "focus")?;
                }
                // Click / toggle / check / select all map to the AXPress action;
                // AX has no distinct double/right press.
                Some(ax) => {
                    perform_ax_action(eref, ax, crate::ax_actions::must_be_listed(action))?;
                }
            }
            Ok(Route::Ax)
        }
    }

    /// Every action is an accessibility action or attribute write; `perform`
    /// returns [`Route::Ax`] for all of them, including `right_click` and
    /// `double_click`, which are an `AXPress`.
    fn route_for(&self, _action: SemanticAction) -> Route {
        Route::Ax
    }

    async fn selection(&self, node_id: Option<u64>) -> Result<Selection, InputError> {
        unsafe {
            if let Some(id) = node_id {
                let elem = self.element_for(id)?;
                return read_selection(elem.as_CFTypeRef());
            }
            if AXIsProcessTrusted() == 0 {
                return Err(InputError::PermissionDenied(
                    "accessibility API disabled (grant permission)".into(),
                ));
            }
            let (app, resolved) = self.resolve_target(None).map_err(InputError::NotFound)?;
            let name = resolved.name;
            let focused = copy_attr(app.as_CFTypeRef(), "AXFocusedUIElement").ok_or_else(|| {
                InputError::Failed(format!("nothing has keyboard focus in '{name}'"))
            })?;
            read_selection(focused.as_CFTypeRef())
        }
    }

    async fn select_range(
        &self,
        node_id: u64,
        start: usize,
        end: usize,
    ) -> Result<Selection, InputError> {
        // No await inside: the AX handles are not `Send`, and the run is short.
        unsafe {
            let elem = self.element_for(node_id)?;
            let e = elem.as_CFTypeRef();
            // Refuse a password field before reading its text.
            if copy_attr_string(e, "AXSubrole").as_deref() == Some("AXSecureTextField") {
                return Err(InputError::InvalidArgs(
                    "this is a password field; its text is never read or selected".into(),
                ));
            }
            let text = copy_attr_string(e, "AXValue")
                .ok_or_else(|| InputError::Unsupported("the element has no text".into()))?;
            let (location, length) = char_range_to_utf16(&text, start, end).ok_or_else(|| {
                InputError::InvalidArgs(format!(
                    "range {start}..{end} runs past the text, which is {} characters long",
                    text.chars().count()
                ))
            })?;
            if !is_settable(e, "AXSelectedTextRange") {
                return Err(InputError::Unsupported(
                    "this element does not let its selection be set (AXSelectedTextRange is read-only \
                     here); select_text does not fall back to dragging the pointer"
                        .into(),
                ));
            }
            let range = CFRange {
                location: location as isize,
                length: length as isize,
            };
            let raw = AXValueCreate(KAXVALUE_CFRANGE, &range as *const CFRange as *const c_void);
            if raw.is_null() {
                return Err(InputError::Failed(
                    "could not build the selection range".into(),
                ));
            }
            // Create rule: the wrapper releases it.
            let value = CFType::wrap_under_create_rule(raw);
            let attr = CFString::new("AXSelectedTextRange");
            ax_result(
                AXUIElementSetAttributeValue(e, attr.as_concrete_TypeRef(), value.as_CFTypeRef()),
                "select_text",
            )?;
            // An app may apply the range on its next run-loop turn.
            let mut got = read_selection(e)?;
            for _ in 0..5 {
                if (got.start, got.end) == (start, end) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
                got = read_selection(e)?;
            }
            Ok(got)
        }
    }

    async fn choose_option(&self, node_id: u64, option: &str) -> Result<Choice, InputError> {
        // No await inside: the AX handles are not `Send`, and the run is short
        // and bounded (see `popup::choose`).
        unsafe {
            let elem = self.element_for(node_id)?;
            crate::popup::choose(&elem, option)
        }
    }

    async fn read_element(&self, node_id: u64) -> Result<Reading, InputError> {
        unsafe {
            let elem = self.element_for(node_id)?;
            Ok(crate::popup::read(elem.as_CFTypeRef()))
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
        use tokio::time::{sleep, Duration};
        // Read when the call starts, so a takeover that lands at any point
        // after admission is seen. One event carries at most 20 UTF-16 units,
        // so the text goes out in pieces, and a human reaching for the
        // keyboard or mouse stops it between them rather than after the lot.
        let since = self.takeover_generation();
        let total = text.chars().count();
        let mut typed = 0;
        for chunk in crate::chunk::chunks(text) {
            if self.takeover_generation() != since {
                return Err(InputError::Failed(format!(
                    "typing aborted after {typed} of {total} characters: a human took over"
                )));
            }
            crate::event::type_chunk(chunk)?;
            typed += chunk.chars().count();
            // Let the target drain its queue; a burst of events is dropped by
            // some apps, and this is the window in which a takeover can land.
            sleep(Duration::from_millis(3)).await;
        }
        Ok(())
    }
    async fn key_combo(&self, combo: &str) -> Result<(), InputError> {
        unsafe { self.ensure_target_frontmost()? };
        crate::event::key_combo(combo, self.keys.flags())
    }
    async fn key_down(&self, key: &str) -> Result<(), InputError> {
        // Only an ordinary key is typed into the frontmost app, so only it
        // needs the target raised; a modifier is a flag on later events.
        if !crate::event::is_modifier(key) {
            unsafe { self.ensure_target_frontmost()? };
        }
        crate::event::key_down(&self.keys, key)
    }
    async fn key_up(&self, key: &str) -> Result<(), InputError> {
        // Never conditional on where focus is: a release that can be refused
        // is a stuck key.
        crate::event::key_up(&self.keys, key)
    }
    async fn mouse(
        &self,
        k: MouseKind,
        x: f64,
        y: f64,
        b: Option<&str>,
        modifiers: &[String],
    ) -> Result<(), InputError> {
        let modifiers = self.keys.with_modifiers(modifiers);
        crate::event::mouse(&self.held, k, x, y, b, &modifiers)
    }
    async fn scroll_at(
        &self,
        x: f64,
        y: f64,
        d: ScrollDir,
        a: i32,
        modifiers: &[String],
    ) -> Result<(), InputError> {
        let modifiers = self.keys.with_modifiers(modifiers);
        crate::event::scroll(&self.held, x, y, d, a, &modifiers)
    }
    async fn pointer_position(&self) -> Result<Option<(f64, f64)>, InputError> {
        crate::event::pointer_position()
    }
    fn recent_pointer_sets(&self) -> Vec<mcp_input::SetPoint> {
        crate::event::recent_sets()
    }
    fn cancel_pending(&self) {
        self.takeovers
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // The human has the mouse now; whatever button this backend believed
        // it was holding is no longer its gesture, and the next `move` must
        // not go out as a drag of it.
        self.held.clear();
        // Keys too: whoever has the mouse now should not find Shift still on.
        crate::event::release_all(&self.keys);
    }
    fn takeover_generation(&self) -> u64 {
        self.takeovers.load(std::sync::atomic::Ordering::SeqCst)
    }
    async fn hover(&self, x: f64, y: f64) -> Result<(), InputError> {
        let modifiers = self.keys.with_modifiers(&[]);
        crate::event::hover(&self.held, x, y, &modifiers)
    }
    async fn drag(
        &self,
        from: (f64, f64),
        route: &[(f64, f64)],
        modifiers: &[String],
        hold_ms: u64,
        since_takeover: u64,
    ) -> Result<(), InputError> {
        use tokio::time::{sleep, Duration};
        let Some(&to) = route.last() else {
            return Err(InputError::InvalidArgs(
                "a drag needs somewhere to go".into(),
            ));
        };
        let modifiers = self.keys.with_modifiers(modifiers);
        let modifiers = &modifiers[..];
        crate::event::drag_begin(&self.held, from, modifiers)?;
        // Let the press register before motion starts; a drag that begins in
        // the same instant reads as a click to most targets. `hold_ms` adds to
        // that for sources that wait to tell a drag from a click.
        sleep(Duration::from_millis(30 + hold_ms)).await;
        let mut last = from;
        for &p in route {
            // A cancelled drag must release the button where it is. Returning
            // with it still held would leave the human dragging a selection
            // around with their own mouse.
            if self.takeover_generation() != since_takeover {
                let _ = crate::event::drag_end(&self.held, last, modifiers);
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
        crate::event::drag_end(&self.held, to, modifiers)
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

/// Processes that draw the desktop itself. They own normal-layer windows —
/// the Dock's icon surfaces, the window manager's gesture overlays — and none
/// of them is an application anyone means to drive.
const SYSTEM_UI_OWNERS: &[&str] = &[
    "Accessibility",
    "AccessibilityUIServer",
    "AutoFill",
    "Control Centre",
    "Control Center",
    "CursorUIViewService",
    "Dock",
    "Notification Centre",
    "Notification Center",
    "Spotlight",
    "ThemeWidgetControlViewService",
    "UserNotificationCenter",
    "Window Server",
    "WindowManager",
    "loginwindow",
];

/// Below this, a normal-layer surface is a tooltip, a badge or a shadow.
const MIN_WINDOW_W: f64 = 120.0;
const MIN_WINDOW_H: f64 = 80.0;

/// Every window-owning process as `(pid, owner name)`, including ones whose
/// windows are all minimised or on another Space.
///
/// `window_owners` asks only about what is on screen, which is right for
/// "who is frontmost" and wrong for "what is running": an app the user
/// minimised is still running, and is still something an agent can be asked
/// to drive.
unsafe fn window_owners_all() -> Vec<(c_int, String)> {
    let mut out: Vec<(c_int, String)> = Vec::new();
    let Some(arr) = copy_window_info(kCGWindowListExcludeDesktopElements, kCGNullWindowID) else {
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
        if !name.is_empty() && !out.iter().any(|(p, _)| *p == pid) {
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

/// Resolve an application name to a running pid. Matches window owners
/// first (exact, then case-insensitive substring), on-screen ones before the
/// rest, then falls back to `pgrep` so apps without a window are still
/// reachable.
///
/// The off-screen owners matter: an app whose windows are all minimised or on
/// another Space is running and can be driven, and without them it resolved
/// only if its process name happened to equal what was typed.
unsafe fn resolve_app_pid(name: &str) -> Option<(c_int, String)> {
    let want = name.to_ascii_lowercase();
    if want.is_empty() {
        return None;
    }
    let onscreen = window_owners();
    let everything = window_owners_all();
    for owners in [&onscreen, &everything] {
        if let Some((p, n)) = owners.iter().find(|(_, n)| n.to_ascii_lowercase() == want) {
            return Some((*p, n.clone()));
        }
    }
    for owners in [&onscreen, &everything] {
        if let Some((p, n)) = owners
            .iter()
            .find(|(_, n)| n.to_ascii_lowercase().contains(&want))
        {
            return Some((*p, n.clone()));
        }
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

/// Read a floating-point entry out of a CGWindowList dictionary.
unsafe fn dict_f64(dict: &CFDictionary, key: CFStringRef) -> Option<f64> {
    let raw = *dict.find(key.cast::<c_void>())?;
    if raw.is_null() {
        return None;
    }
    CFNumber::wrap_under_get_rule(raw as CFNumberRef).to_f64()
}

/// Read a boolean entry out of a CGWindowList dictionary.
unsafe fn dict_bool(dict: &CFDictionary, key: CFStringRef) -> Option<bool> {
    let raw = *dict.find(key.cast::<c_void>())?;
    if raw.is_null() {
        return None;
    }
    let v = CFType::wrap_under_get_rule(raw);
    Some(v.as_CFTypeRef() == CFBoolean::true_value().as_CFTypeRef())
}

/// Read the `kCGWindowBounds` sub-dictionary, which is a CGRect in the window
/// server's own coordinate space — the one every other tool here uses.
unsafe fn dict_rect(dict: &CFDictionary, key: CFStringRef) -> Option<Rect> {
    let raw = *dict.find(key.cast::<c_void>())?;
    if raw.is_null() {
        return None;
    }
    let bounds: CFDictionary = CFDictionary::wrap_under_get_rule(raw as _);
    let number = |name: &str| -> Option<f64> {
        let k = CFString::new(name);
        let v: CFTypeRef = *bounds.find(k.as_concrete_TypeRef().cast::<c_void>())?;
        if v.is_null() {
            return None;
        }
        CFNumber::wrap_under_get_rule(v as CFNumberRef).to_f64()
    };
    Some(Rect {
        x: number("X")?,
        y: number("Y")?,
        w: number("Width")?,
        h: number("Height")?,
    })
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

/// The frontmost application's pid and name. The name is its accessibility
/// title, else the name the window server gives its windows.
unsafe fn frontmost_identity() -> Option<(c_int, String)> {
    let elem = frontmost_app()?;
    let mut pid: c_int = 0;
    AXUIElementGetPid(elem.as_CFTypeRef(), &mut pid);
    if pid <= 0 {
        return None;
    }
    let name = copy_attr_string(elem.as_CFTypeRef(), "AXTitle")
        .filter(|n| !n.is_empty())
        .or_else(|| {
            window_owners_all()
                .into_iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, n)| n)
        })
        .unwrap_or_default();
    Some((pid, name))
}

/// A window as the window server lists it, with what is needed to capture it.
pub(crate) struct WindowRow {
    pub info: WindowInfo,
    /// The window server's number for it, for asking what is in front of it.
    pub number: i64,
}

/// One normal-layer surface of the window server's list, before it is
/// numbered within its application.
struct ServerWindow {
    pid: c_int,
    owner: String,
    number: i64,
    title: Option<String>,
    bounds: Option<Rect>,
    onscreen: bool,
}

/// What the application itself says about one of its windows.
struct AxWindow {
    title: Option<String>,
    bounds: Option<Rect>,
    minimized: bool,
}

/// Bound on one accessibility read while enriching the window list: a stuck
/// app must cost a fraction of a second, not the system default of several.
const WINDOW_READ_TIMEOUT_S: f32 = 0.25;

impl MacosBackend {
    /// Every window of every application that has one, numbered within its own
    /// application.
    ///
    /// Which windows exist and how they are numbered come from the window
    /// server alone, not from any application's accessibility tree: the server
    /// is authoritative about what exists (Chromium exposes no tree until
    /// something asks it to), and it is the same list whether the caller asked
    /// about one application or all of them and whether or not Accessibility is
    /// granted. That is what makes an id from `list_windows` mean the same
    /// window to `capture_window` and `ocr_region`.
    ///
    /// Titles come from `kCGWindowName`, which is redacted without the Screen
    /// Recording permission. Where it is missing the application's own
    /// accessibility title is used if Accessibility is granted, matched by
    /// frame; otherwise the title is reported as absent rather than guessed.
    /// A window the server does not draw is `minimized` when the application
    /// says so and `offscreen` otherwise (another Space, or a hidden app).
    unsafe fn window_rows(&self) -> Result<Vec<WindowRow>, WindowError> {
        let Some(arr) = copy_window_info(kCGWindowListExcludeDesktopElements, kCGNullWindowID)
        else {
            return Err(WindowError::Failed(
                "the window server returned no window list".into(),
            ));
        };
        let mut raw_windows: Vec<ServerWindow> = Vec::new();
        for raw in arr.get_all_values() {
            if raw.is_null() {
                continue;
            }
            let dict = CFDictionary::wrap_under_get_rule(raw as _);
            // Layer 0 is the normal window layer. Everything else is the menu
            // bar, the Dock, overlays and helper panels — present, but not
            // what anyone means by "a window".
            if dict_i64(&dict, kCGWindowLayer) != Some(0) {
                continue;
            }
            let (Some(owner), Some(pid), Some(number)) = (
                dict_string(&dict, kCGWindowOwnerName),
                dict_i64(&dict, kCGWindowOwnerPID),
                dict_i64(&dict, kCGWindowNumber),
            ) else {
                continue;
            };
            if SYSTEM_UI_OWNERS.contains(&owner.as_str()) {
                continue;
            }
            let bounds = dict_rect(&dict, kCGWindowBounds);
            // Shadows, badges and tooltips live on the normal layer too.
            if bounds.is_some_and(|b| b.w < MIN_WINDOW_W || b.h < MIN_WINDOW_H) {
                continue;
            }
            raw_windows.push(ServerWindow {
                pid: pid as c_int,
                owner,
                number,
                title: dict_string(&dict, kCGWindowName).filter(|t| !t.is_empty()),
                bounds,
                onscreen: dict_bool(&dict, kCGWindowIsOnscreen).unwrap_or(true),
            });
        }

        // An application draws several full-size surfaces per visible window
        // (the frame, the shadow, an overlay), and only the real one is
        // titled. Dropping the untitled ones is what turns 169 entries into
        // the two dozen a person would point at — but only when titles are
        // readable at all. Without Screen Recording every title is redacted,
        // and filtering on it would answer "nothing is open". Membership
        // depends on nothing else, so ids do not shift with who is asked.
        let titles_readable = raw_windows.iter().any(|w| w.title.is_some());
        let kept: Vec<ServerWindow> = raw_windows
            .into_iter()
            .filter(|w| !titles_readable || w.title.is_some())
            .collect();

        let trusted = AXIsProcessTrusted() != 0;
        let mut ax_by_pid: HashMap<c_int, Vec<AxWindow>> = HashMap::new();
        if trusted {
            // Only an application with a window whose state or title the
            // server cannot give is worth asking.
            let mut asked: Vec<c_int> = Vec::new();
            for w in kept.iter().filter(|w| !w.onscreen || w.title.is_none()) {
                if asked.contains(&w.pid) {
                    continue;
                }
                asked.push(w.pid);
                if let Some(app) = app_element(w.pid) {
                    AXUIElementSetMessagingTimeout(app.as_CFTypeRef(), WINDOW_READ_TIMEOUT_S);
                    let wins = get_windows(app.as_CFTypeRef())
                        .iter()
                        .map(|win| {
                            let r = win.as_CFTypeRef();
                            AxWindow {
                                title: copy_attr_string(r, "AXTitle").filter(|t| !t.is_empty()),
                                bounds: read_bounds(r).map(|b| Rect {
                                    x: b.x,
                                    y: b.y,
                                    w: b.w,
                                    h: b.h,
                                }),
                                minimized: copy_attr_bool(r, "AXMinimized").unwrap_or(false),
                            }
                        })
                        .collect();
                    ax_by_pid.insert(w.pid, wins);
                }
            }
        }

        let mut out = Vec::new();
        let mut per_pid: HashMap<c_int, u32> = HashMap::new();
        let mut pids: Vec<c_int> = Vec::new();
        for w in &kept {
            if !pids.contains(&w.pid) {
                pids.push(w.pid);
            }
        }
        // Grouped by application in order of first appearance, each group in
        // the server's order, which is what the ids count.
        for pid in pids {
            let group: Vec<&ServerWindow> = kept.iter().filter(|w| w.pid == pid).collect();
            let ax = ax_by_pid.get(&pid);
            let frames: Vec<Option<Rect>> = group.iter().map(|w| w.bounds).collect();
            let ax_frames: Vec<Option<Rect>> = ax
                .map(|a| a.iter().map(|w| w.bounds).collect())
                .unwrap_or_default();
            let paired = crate::geometry::pair_frames(&frames, &ax_frames);
            for (w, pair) in group.iter().zip(paired) {
                let ax_win = pair.and_then(|i| ax.and_then(|a| a.get(i)));
                let state = if w.onscreen {
                    WindowState::Onscreen
                } else if ax_win.is_some_and(|a| a.minimized) {
                    WindowState::Minimized
                } else {
                    WindowState::Offscreen
                };
                let index = per_pid.entry(pid).or_insert(0);
                out.push(WindowRow {
                    info: WindowInfo {
                        id: *index,
                        app: Some(w.owner.clone()),
                        pid: u32::try_from(pid).ok(),
                        title: w
                            .title
                            .clone()
                            .or_else(|| ax_win.and_then(|a| a.title.clone())),
                        bounds: w.bounds,
                        minimized: state == WindowState::Minimized,
                        state,
                    },
                    number: w.number,
                });
                *index += 1;
            }
        }
        Ok(out)
    }

    /// What is on screen in front of a window, front to back: the other
    /// applications' surfaces that would be in a screen capture of its area.
    /// Menu bar, Dock and other furniture are above layer 0 or system owners
    /// and are not counted; neither are clear surfaces.
    unsafe fn windows_above(&self, number: i64) -> Vec<crate::geometry::Above> {
        let mut out = Vec::new();
        let Some(arr) = copy_window_info(
            kCGWindowListOptionOnScreenAboveWindow | kCGWindowListExcludeDesktopElements,
            number as u32,
        ) else {
            return out;
        };
        for raw in arr.get_all_values() {
            if raw.is_null() {
                continue;
            }
            let dict = CFDictionary::wrap_under_get_rule(raw as _);
            let (Some(owner), Some(pid), Some(layer), Some(bounds)) = (
                dict_string(&dict, kCGWindowOwnerName),
                dict_i64(&dict, kCGWindowOwnerPID),
                dict_i64(&dict, kCGWindowLayer),
                dict_rect(&dict, kCGWindowBounds),
            ) else {
                continue;
            };
            // Floating panels and dialogs sit above layer 0 but below the
            // menu bar (24) and Dock (20 and up); those are not content.
            if !(0..20).contains(&layer)
                || SYSTEM_UI_OWNERS.contains(&owner.as_str())
                || bounds.w < MIN_WINDOW_W
                || bounds.h < MIN_WINDOW_H
                || dict_f64(&dict, kCGWindowAlpha).is_some_and(|a| a < 0.05)
            {
                continue;
            }
            out.push(crate::geometry::Above {
                pid: pid as i32,
                owner,
                bounds,
            });
        }
        out
    }

    /// One window of an application, by its id in that application's list,
    /// with the reason it could not be captured from the screen if there is
    /// one. The application is the one `app` names, else the pinned one, else
    /// the frontmost; nothing is pinned. An id the application does not have
    /// is an error saying which application and how many windows it has.
    pub(crate) unsafe fn window_for_capture(
        &self,
        window_id: u32,
        app: Option<&str>,
    ) -> Result<(Resolved, WindowInfo, Rect), WindowError> {
        let target = self.peek_target(app).map_err(WindowError::NotFound)?;
        let rows: Vec<WindowRow> = self
            .window_rows()?
            .into_iter()
            .filter(|r| r.info.pid == u32::try_from(target.pid).ok())
            .collect();
        let infos: Vec<WindowInfo> = rows.iter().map(|r| r.info.clone()).collect();
        let pid = u32::try_from(target.pid).ok();
        let info = mcp_window::window_at(&infos, &target.name, pid, window_id)
            .map_err(WindowError::NotFound)?
            .clone();
        let bounds = info.bounds.ok_or_else(|| {
            WindowError::Failed(format!(
                "window {window_id} of '{}' reports no bounds",
                target.name
            ))
        })?;
        let number = rows[window_id as usize].number;
        let covered = if info.state == WindowState::Onscreen {
            crate::geometry::coverage(&bounds, target.pid, &self.windows_above(number))
        } else {
            Vec::new()
        };
        if let Some(why) =
            crate::geometry::capture_obstacle(&target.name, window_id, info.state, &covered)
        {
            return Err(WindowError::Failed(why));
        }
        Ok((target, info, bounds))
    }
}

pub(crate) unsafe fn copy_attr_bool(elem: CFTypeRef, name: &str) -> Option<bool> {
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

/// The window of `app` whose title contains `title`, else its main window.
/// The errors name the application and how many windows it has: the usual
/// cause of a miss is a title meant for a different application.
unsafe fn pick_window(
    app: CFTypeRef,
    who: &str,
    title: Option<&str>,
) -> Result<CFType, WindowError> {
    let wins = get_windows(app);
    if let Some(t) = title {
        let count = wins.len();
        wins.into_iter()
            .find(|w| {
                copy_attr_string(w.as_CFTypeRef(), "AXTitle")
                    .as_deref()
                    .is_some_and(|s| s.contains(t))
            })
            .ok_or_else(|| {
                WindowError::NotFound(format!(
                    "'{who}' has {count} window{}, none with a title containing '{t}'",
                    if count == 1 { "" } else { "s" }
                ))
            })
    } else if let Some(mw) = copy_attr(app, "AXMainWindow") {
        Ok(mw)
    } else {
        wins.into_iter()
            .next()
            .ok_or_else(|| WindowError::NotFound(format!("'{who}' has no windows")))
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
            let rows = self.window_rows()?;
            // With no app named and no pinned target, answer about the whole
            // machine. Resolving to the frontmost app instead makes the
            // obvious opening question — "what is open?" — return one app's
            // windows, or none at all, with nothing to say it was narrowed.
            let named = app_name.is_some_and(|a| !a.trim().is_empty());
            if !named && self.live_pin().is_none() {
                return Ok(rows.into_iter().map(|r| r.info).collect());
            }
            // Honour the requested app. Resolving `None` here would silently
            // answer about whatever happens to be frontmost, so
            // `list_windows { app: "TextEdit" }` returned the caller's own
            // terminal — wrong, and wrong without saying so. An app that is
            // running with no windows is an empty list; one that is not
            // running is an error.
            let target = self.peek_target(app_name).map_err(WindowError::NotFound)?;
            let pid = u32::try_from(target.pid).ok();
            Ok(rows
                .into_iter()
                .map(|r| r.info)
                .filter(|w| w.pid == pid)
                .collect())
        }
    }

    /// Applications with windows, as a person would name them.
    ///
    /// This used to be `ps -Axo comm=`, which answered with every process on
    /// the machine: `AudioComponentRegistrar`, `BTLEServer`, `-zsh`. An agent
    /// asking what is running got several hundred names, almost none of which
    /// `launch` or `focus_app` would accept, and the real applications were
    /// indistinguishable from the daemons. The window server already knows
    /// which processes own windows, and it knows them by their display names.
    async fn list_apps(&self) -> Result<Vec<String>, WindowError> {
        let mut apps: Vec<String> = unsafe { window_owners_all() }
            .into_iter()
            .map(|(_, name)| name)
            .filter(|name| !SYSTEM_UI_OWNERS.contains(&name.as_str()))
            .collect();
        apps.sort();
        apps.dedup();
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

    /// `open -g`: launch without bringing the app to the foreground. The app
    /// can still activate itself once it is running.
    async fn launch_in_background(&self, app: &str) -> Result<(), WindowError> {
        let out = run("open", &["-g", "-a", app])?;
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
        app: Option<&str>,
        title: Option<&str>,
        action: WindowAction,
        bounds: Option<Rect>,
    ) -> Result<(), WindowError> {
        unsafe {
            // The app the call names, not whichever is in front: this used to
            // read the frontmost app whatever `app` said, so closing a window
            // of one application closed a window of another.
            let (app, who) = self.peek_app(app)?;
            let win = pick_window(app.as_CFTypeRef(), &who.name, title)?;
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

    /// Focus raises the window and un-minimising brings it back to the front.
    /// Every other action is an attribute write or a button press on the
    /// window and leaves the app where it was.
    fn control_takes_foreground(&self, action: WindowAction) -> bool {
        matches!(action, WindowAction::Focus | WindowAction::Restore)
    }

    async fn menu_open(&self, app: Option<&str>, path: &[String]) -> Result<(), WindowError> {
        unsafe {
            let (app, _) = self.peek_app(app)?;
            navigate_menu(app.as_CFTypeRef(), path, false)
        }
    }

    async fn menu_invoke(&self, app: Option<&str>, path: &[String]) -> Result<(), WindowError> {
        unsafe {
            let (app, _) = self.peek_app(app)?;
            navigate_menu(app.as_CFTypeRef(), path, true)
        }
    }

    async fn focus_app(&self, app: Option<&str>) -> Result<Option<String>, WindowError> {
        let Some(name) = app else {
            self.clear_target();
            return Ok(None);
        };
        unsafe {
            let (elem, r) = self.peek_app(Some(name))?;
            self.set_target(r.pid, &r.name);
            // Bring it forward so keyboard/coordinate input lands there too.
            let _ = set_bool_attr(elem.as_CFTypeRef(), "AXFrontmost", true);
        }
        Ok(self.current_target().map(|(_, n)| n))
    }

    /// The pin of `focus_app` without its raise: no attribute is written to
    /// the app, so nothing in front changes.
    async fn pin_app(&self, app: &str) -> Result<TargetInfo, WindowError> {
        unsafe {
            let r = self.peek_target(Some(app)).map_err(WindowError::NotFound)?;
            self.set_target(r.pid, &r.name);
            Ok(r.info())
        }
    }

    async fn describe_target(&self, app: Option<&str>) -> Result<TargetInfo, WindowError> {
        unsafe {
            self.peek_target(app)
                .map(|r| r.info())
                .map_err(WindowError::NotFound)
        }
    }

    async fn pinned(&self) -> Option<TargetInfo> {
        self.live_pin().map(|t| TargetInfo {
            app: t.name,
            pid: u32::try_from(t.pid).ok(),
            from: TargetSource::Pinned,
        })
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
                    let (elem, _) = self.peek_app(app)?;
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
            let (elem, _) = self.peek_app(app)?;
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

// ---- the facts behind `effects` ---------------------------------------------

/// How long one read may wait on an app that is not answering. A probe runs
/// twice around every input action, so a stuck app must cost a fraction of a
/// second, not the system default of several.
const PROBE_TIMEOUT_S: f32 = 0.25;

/// Windows examined for sheets and dialogs. A sheet is on an app's front
/// windows; a long tail of palettes would only add cost.
const PROBE_WINDOWS: usize = 12;

/// The kind of overlay a window or a window's child is, if it is one.
///
/// Sheets are children of the window they slide from (a save panel is the last
/// entry of the window's `AXChildren`; `AXSheets` reports nothing for it), and
/// alerts and dialogs are windows of their own. Floating palettes carry the
/// `AXSystemFloatingWindow` subrole and are not decisions to make, so they are
/// not overlays here, unlike in `list_dialogs`.
unsafe fn overlay_kind(el: CFTypeRef, is_window: bool) -> Option<&'static str> {
    let role = copy_attr_string(el, "AXRole")?;
    if role == "AXSheet" {
        return Some("sheet");
    }
    if role == "AXPopover" && !is_window {
        return Some("popover");
    }
    if !is_window {
        return None;
    }
    match copy_attr_string(el, "AXSubrole").as_deref() {
        Some("AXSystemDialog") => Some("alert"),
        Some("AXDialog") => Some("dialog"),
        _ => None,
    }
}

impl MacosBackend {
    /// Read the facts `effects` compares, passively: attribute reads only, no
    /// action, no raise, and the sticky target is left as it was.
    ///
    /// The application read is the one the session is driving: the pinned
    /// target, else the app of the last snapshot, else the frontmost app.
    unsafe fn read_facts(&self, describe_new: Option<&[mcp_input::Overlay]>) -> mcp_input::UiFacts {
        use mcp_input::{Focus, Overlay, UiFacts};
        let mut facts = UiFacts::default();
        if AXIsProcessTrusted() == 0 {
            return facts;
        }
        let front = frontmost_pid();
        facts.frontmost = front.and_then(|pid| app_element(pid)).and_then(|a| {
            AXUIElementSetMessagingTimeout(a.as_CFTypeRef(), PROBE_TIMEOUT_S);
            copy_attr_string(a.as_CFTypeRef(), "AXTitle").filter(|n| !n.is_empty())
        });
        let pid = {
            let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.target.as_ref().map(|t| t.pid).or(st.pid)
        }
        .or(front);
        let Some(app) = pid.and_then(|p| app_element(p)) else {
            return facts;
        };
        let app_ref = app.as_CFTypeRef();
        AXUIElementSetMessagingTimeout(app_ref, PROBE_TIMEOUT_S);

        facts.focus = copy_attr(app_ref, "AXFocusedUIElement").and_then(|f| {
            let r = f.as_CFTypeRef();
            Some(Focus {
                role: mcp_a11y::normalize_role(&copy_attr_string(r, "AXRole")?),
                name: ["AXTitle", "AXDescription"]
                    .iter()
                    .find_map(|a| copy_attr_string(r, a).filter(|s| !s.is_empty())),
            })
        });

        // `AXWindows` that cannot be read is not an app without windows.
        let wins: Option<Vec<CFType>> = copy_attr(app_ref, "AXWindows")
            .and_then(|v| v.downcast::<CFArray>())
            .map(|arr| {
                arr.get_all_values()
                    .into_iter()
                    .map(|p| CFType::wrap_under_get_rule(p))
                    .collect()
            });
        let mut overlays: Vec<Overlay> = Vec::new();
        let mut found: Vec<(&'static str, CFType)> = Vec::new();
        // Open menus hang off the application, not a window.
        for child in copy_children(app_ref) {
            if copy_attr_string(child.as_CFTypeRef(), "AXRole").as_deref() == Some("AXMenu") {
                found.push(("menu", child));
            }
        }
        let mut titles = Vec::new();
        if let Some(wins) = &wins {
            for w in wins.iter().take(PROBE_WINDOWS) {
                let wr = w.as_CFTypeRef();
                match overlay_kind(wr, true) {
                    // Reported as an overlay, not also as a window.
                    Some(kind) => found.push((kind, w.clone())),
                    None => titles.push(copy_attr_string(wr, "AXTitle").unwrap_or_default()),
                }
                for c in copy_children(wr) {
                    if let Some(kind) = overlay_kind(c.as_CFTypeRef(), false) {
                        found.push((kind, c));
                    }
                }
            }
            // Past the examined windows only titles are read.
            for w in wins.iter().skip(PROBE_WINDOWS) {
                titles.push(copy_attr_string(w.as_CFTypeRef(), "AXTitle").unwrap_or_default());
            }
            facts.windows = Some(titles);
        }
        for (kind, el) in found {
            let r = el.as_CFTypeRef();
            let mut o = Overlay {
                kind: kind.to_string(),
                title: copy_attr_string(r, "AXTitle").filter(|t| !t.is_empty()),
                text: None,
                buttons: Vec::new(),
            };
            // The contents are read once, when the overlay first shows.
            if describe_new
                .is_some_and(|known| !known.iter().any(|k| k.kind == o.kind && k.title == o.title))
            {
                let (buttons, text, _) = dialog_details(r);
                o.buttons = buttons;
                o.text = text.into_iter().next();
            }
            overlays.push(o);
        }
        // Menus and windows are readable or the app is not; a failed window
        // read means the overlay list is incomplete too.
        if wins.is_some() {
            facts.overlays = Some(overlays);
        }
        facts
    }
}

#[async_trait]
impl mcp_input::UiProbe for MacosBackend {
    async fn probe(&self, describe_new: Option<&[mcp_input::Overlay]>) -> mcp_input::UiFacts {
        unsafe { self.read_facts(describe_new) }
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

    /// The facts behind `effects`, read from whatever is on screen: attribute
    /// reads only, so it neither raises a window nor changes the session's
    /// target, and it answers promptly (a probe runs twice around every
    /// action). Without the Accessibility grant it reads nothing, which is an
    /// answer too.
    #[tokio::test]
    async fn the_facts_behind_effects_are_read_passively_and_quickly() {
        use mcp_input::UiProbe;
        let b = MacosBackend::new();
        let started = std::time::Instant::now();
        let facts = b.probe(None).await;
        let again = b.probe(Some(&[])).await;
        let spent = started.elapsed();
        assert!(
            b.current_target().is_none(),
            "reading set the sticky target"
        );
        assert!(
            spent < std::time::Duration::from_secs(3),
            "two readings took {spent:?}"
        );
        if permissions().accessibility {
            assert!(facts.frontmost.is_some(), "{facts:?}");
            // Two readings of an idle screen agree on what is open.
            assert_eq!(facts.frontmost, again.frontmost);
        } else {
            assert_eq!(facts, mcp_input::UiFacts::default());
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

    /// `list_windows` with nothing pinned and no app named must answer about
    /// the whole machine.
    ///
    /// It used to resolve the frontmost application, so the obvious opening
    /// question — what is open? — came back as one app's windows, or as an
    /// empty list when the front app had none, with nothing to say it had been
    /// narrowed. A model given that answer starts guessing app names one at a
    /// time, which is exactly what one did.
    #[tokio::test]
    async fn list_windows_with_no_app_covers_the_whole_machine() {
        let b = MacosBackend::new();
        let Ok(all) = mcp_window::WindowBackend::list_windows(&b, None).await else {
            return; // Headless, or no permission.
        };
        if all.is_empty() {
            return; // A desktop with nothing open is a legitimate answer.
        }
        for w in &all {
            let app = w.app.as_deref().unwrap_or_default();
            assert!(
                !SYSTEM_UI_OWNERS.contains(&app),
                "'{app}' is desktop furniture, not an application window"
            );
            if let Some(b) = w.bounds {
                assert!(
                    b.w >= MIN_WINDOW_W && b.h >= MIN_WINDOW_H,
                    "{app} window {}x{} is a tooltip or a shadow",
                    b.w,
                    b.h
                );
            }
        }
        // Naming an app must narrow the answer, not change its meaning: every
        // window it returns has to be one the machine-wide list would agree
        // belongs to that app.
        let one = all[0].app.clone().unwrap_or_default();
        if let Ok(scoped) = mcp_window::WindowBackend::list_windows(&b, Some(&one)).await {
            for w in &scoped {
                assert!(
                    w.app
                        .as_deref()
                        .is_some_and(|a| a.contains(&one) || one.contains(a)),
                    "asked about '{one}', got a window belonging to {:?}",
                    w.app
                );
            }
        }
    }

    /// `list_apps` must name applications, not processes.
    ///
    /// It used to be `ps -Axo comm=`: several hundred entries, including
    /// `AudioComponentRegistrar` and `-zsh`, almost none of which `launch` or
    /// `focus_app` would accept.
    #[tokio::test]
    async fn list_apps_names_applications_not_processes() {
        let b = MacosBackend::new();
        let Ok(apps) = mcp_window::WindowBackend::list_apps(&b).await else {
            return;
        };
        if apps.is_empty() {
            return; // Headless.
        }
        assert!(
            apps.len() < 100,
            "{} entries — this is a process list again, not an app list",
            apps.len()
        );
        for name in &apps {
            assert!(!name.is_empty());
            assert!(
                !SYSTEM_UI_OWNERS.contains(&name.as_str()),
                "'{name}' is part of the desktop, not an application"
            );
            assert!(
                !name.starts_with('-') && !name.contains('/'),
                "'{name}' looks like a process, not an application"
            );
        }
        let mut sorted = apps.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted, apps,
            "the list must be sorted and free of duplicates"
        );
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
    /// `foreground = "never"` trusts this answer to refuse a pointer click
    /// before it is posted, so it must agree with what `perform` returns: it
    /// returns `Ax` on every path.
    #[test]
    fn every_semantic_action_is_an_accessibility_route_on_macos() {
        let b = MacosBackend::new();
        for name in [
            "click",
            "double_click",
            "right_click",
            "focus",
            "toggle",
            "check",
            "uncheck",
            "expand",
            "collapse",
            "select",
            "scroll_into_view",
            "show_menu",
            "increment",
            "decrement",
            "pick",
        ] {
            let action = SemanticAction::parse(name).expect(name);
            assert_eq!(b.route_for(action), Route::Ax, "{name}");
        }
    }

    #[test]
    fn only_focus_and_restore_take_the_foreground_in_a_macos_window_action() {
        let b = MacosBackend::new();
        for (name, takes) in [
            ("focus", true),
            ("restore", true),
            ("minimize", false),
            ("maximize", false),
            ("close", false),
            ("move", false),
            ("resize", false),
        ] {
            let action = WindowAction::parse(name).expect(name);
            assert_eq!(b.control_takes_foreground(action), takes, "{name}");
        }
    }

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

/// Driving an application that is not frontmost, against the real OS and
/// real running applications, by passive reads only: a pin is a change to this
/// process's own state, and everything else reads the window server and the
/// accessibility tree. None of these may raise a window, change the
/// frontmost application, move the pointer or type, and each says so by
/// comparing the desktop before and after. Finder is always running; it may
/// have no window open, which is an answer and is not opened here.
///
/// Where Accessibility is not granted the calls answer `PermissionDenied`
/// rather than succeed, and the tests accept that answer and nothing else.
#[cfg(test)]
mod background_app_tests {
    use super::*;
    use mcp_types::{CallCtx, CancelToken, ErrorCode, ToolModule};
    use serde_json::json;
    use std::sync::Arc;

    const NO_SUCH_APP: &str = "NoSuchApplication-zzz-4471";

    /// What the person at the machine would notice: which app is in front and
    /// where the pointer is.
    fn desk() -> (Option<c_int>, Option<(f64, f64)>) {
        let pointer = crate::event::pointer_position().ok().flatten();
        (unsafe { frontmost_pid() }, pointer)
    }

    fn finder_pid() -> Option<u32> {
        let out = std::process::Command::new("/usr/bin/pgrep")
            .args(["-x", "Finder"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    }

    fn name_of(pid: Option<c_int>) -> String {
        unsafe {
            window_owners_all()
                .into_iter()
                .find(|(p, _)| Some(*p) == pid)
                .map(|(_, n)| n)
                .unwrap_or_else(|| format!("{pid:?}"))
        }
    }

    #[tokio::test]
    async fn pinning_finder_changes_nothing_the_person_would_see() {
        let b = MacosBackend::new();
        let (front, pointer) = desk();
        let pinned = WindowBackend::pin_app(&b, "Finder").await;
        let (front_after, pointer_after) = desk();
        eprintln!(
            "frontmost before/after: {} / {}; pin: {pinned:?}",
            name_of(front),
            name_of(front_after)
        );
        assert_eq!(front, front_after, "the frontmost app changed");
        assert_eq!(pointer, pointer_after, "the pointer moved");
        let t = pinned.expect("Finder is always running; a pin reads no permission");
        assert_eq!(t.app, "Finder");
        assert_eq!(t.from, TargetSource::Named);
        if let Some(want) = finder_pid() {
            assert_eq!(t.pid, Some(want), "pinned another process than Finder");
        }
        let pinned = WindowBackend::pinned(&b).await.expect("the pin is set");
        assert_eq!(
            (pinned.app.as_str(), pinned.from),
            ("Finder", TargetSource::Pinned)
        );
        // Releasing it is as quiet.
        WindowBackend::focus_app(&b, None).await.unwrap();
        assert!(WindowBackend::pinned(&b).await.is_none());
        assert_eq!(desk().0, front);
    }

    #[tokio::test]
    async fn with_finder_pinned_every_read_is_finders_and_nothing_moves() {
        let b = MacosBackend::new();
        let (front, pointer) = desk();
        WindowBackend::pin_app(&b, "Finder").await.unwrap();
        let want = finder_pid();

        // Windows: whatever the pin lists is Finder's, numbered from 0.
        let ws = WindowBackend::list_windows(&b, None).await.unwrap();
        eprintln!(
            "Finder (pid {want:?}) has {} window(s): {:?}",
            ws.len(),
            ws.iter()
                .map(|w| (w.id, &w.title, w.state))
                .collect::<Vec<_>>()
        );
        for (i, w) in ws.iter().enumerate() {
            assert_eq!(w.app.as_deref(), Some("Finder"));
            assert_eq!(w.pid, want, "a window of another process in Finder's list");
            assert_eq!(w.id as usize, i, "ids are Finder's own, in order");
        }

        // The accessibility tree: Finder's, said to come from the pin. Finder
        // may have no window; its application element is still readable.
        let req = SnapshotRequest::default();
        match A11yBackend::snapshot(&b, &req).await {
            Ok(raw) => {
                let t = raw.target.expect("a macOS snapshot says what it read");
                assert_eq!(raw.app.as_deref(), Some("Finder"));
                assert_eq!(t.from, "pinned");
                assert_eq!(t.pid, want);
            }
            Err(BackendError::PermissionDenied(_)) => eprintln!("no Accessibility: tree skipped"),
            Err(e) => panic!("unexpected: {e:?}"),
        }

        // A window id Finder does not have is an error naming Finder and its
        // window count, never a window of the frontmost app.
        let err = unsafe { b.window_for_capture(9_999, None) }
            .err()
            .expect("no window 9999");
        match err {
            WindowError::NotFound(m) => {
                assert!(m.contains("'Finder'"), "{m}");
                assert!(m.contains("window"), "{m}");
                eprintln!("out-of-range id: {m}");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        // Each id it does have is either capturable and Finder's, or refused
        // with the reason; it is never somebody else's window.
        for w in &ws {
            match unsafe { b.window_for_capture(w.id, None) } {
                Ok((target, info, _)) => {
                    assert_eq!(info.pid, want);
                    assert_eq!(target.from, TargetSource::Pinned);
                    eprintln!("Finder window {} would capture", w.id);
                }
                Err(WindowError::Failed(m)) => {
                    eprintln!("Finder window {} refused: {m}", w.id);
                    assert!(
                        m.contains("covered by")
                            || m.contains("minimised")
                            || m.contains("not on screen"),
                        "{m}"
                    );
                }
                Err(e) => panic!("unexpected: {e:?}"),
            }
        }

        assert_eq!(
            desk(),
            (front, pointer),
            "reading Finder disturbed the desktop"
        );
    }

    /// A name that matches nothing is an error that names it, from every
    /// operation, and pins nothing.
    #[tokio::test]
    async fn an_app_that_is_not_running_is_an_error_naming_it_never_a_fallback() {
        let b = MacosBackend::new();
        let (front, pointer) = desk();
        let not_found = |what: &str, r: Result<(), WindowError>| match r {
            Err(WindowError::NotFound(m)) => {
                assert!(m.contains(NO_SUCH_APP), "{what}: {m}");
            }
            // The operations that read the tree check the permission first.
            Err(WindowError::PermissionDenied(_)) => {}
            other => panic!("{what}: expected NotFound, got {other:?}"),
        };
        not_found(
            "pin",
            WindowBackend::pin_app(&b, NO_SUCH_APP).await.map(|_| ()),
        );
        not_found(
            "describe",
            WindowBackend::describe_target(&b, Some(NO_SUCH_APP))
                .await
                .map(|_| ()),
        );
        not_found(
            "list_windows",
            WindowBackend::list_windows(&b, Some(NO_SUCH_APP))
                .await
                .map(|_| ()),
        );
        not_found(
            "control_window",
            WindowBackend::control_window(
                &b,
                Some(NO_SUCH_APP),
                None,
                WindowAction::Minimize,
                None,
            )
            .await,
        );
        not_found(
            "menu_list",
            WindowBackend::menu_list(&b, Some(NO_SUCH_APP), &[], 1)
                .await
                .map(|_| ()),
        );
        not_found(
            "list_dialogs",
            WindowBackend::list_dialogs(&b, Some(NO_SUCH_APP), DialogScope::App)
                .await
                .map(|_| ()),
        );
        not_found(
            "menu_invoke",
            WindowBackend::menu_invoke(&b, Some(NO_SUCH_APP), &["File".into(), "Close".into()])
                .await,
        );
        let cap = unsafe { b.window_for_capture(0, Some(NO_SUCH_APP)) };
        assert!(matches!(cap, Err(WindowError::NotFound(m)) if m.contains(NO_SUCH_APP)));
        let req = SnapshotRequest {
            app: Some(NO_SUCH_APP.into()),
            ..Default::default()
        };
        match A11yBackend::snapshot(&b, &req).await {
            Err(BackendError::NotFound(m)) => assert!(m.contains(NO_SUCH_APP), "{m}"),
            Err(BackendError::PermissionDenied(_)) => {}
            other => panic!("a snapshot of a missing app: {other:?}"),
        }
        assert!(
            b.current_target().is_none(),
            "a failed lookup pinned something"
        );
        assert_eq!(desk(), (front, pointer));
    }

    /// A control_window naming an app acts on that app. Finder is asked for a
    /// title nothing has, so the answer is the error and no window is touched.
    #[tokio::test]
    async fn control_window_looks_in_the_named_app_and_says_so_when_it_has_no_such_window() {
        let b = MacosBackend::new();
        let (front, pointer) = desk();
        let r = WindowBackend::control_window(
            &b,
            Some("Finder"),
            Some("no-window-has-this-title-9187"),
            WindowAction::Minimize,
            None,
        )
        .await;
        match r {
            Err(WindowError::NotFound(m)) => {
                assert!(m.contains("'Finder'"), "{m}");
                eprintln!("{m}");
            }
            Err(WindowError::PermissionDenied(_)) => {}
            other => panic!("expected NotFound naming Finder, got {other:?}"),
        }
        assert!(b.current_target().is_none(), "a window call pinned the app");
        assert_eq!(desk(), (front, pointer));
    }

    /// `list_windows { app }` is a read: it must not leave that app pinned,
    /// or a later call that names nothing would silently act on it.
    #[tokio::test]
    async fn naming_an_app_in_a_window_call_does_not_pin_it() {
        let b = MacosBackend::new();
        let _ = WindowBackend::list_windows(&b, Some("Finder")).await;
        let _ = WindowBackend::menu_list(&b, Some("Finder"), &[], 1).await;
        let _ = WindowBackend::list_dialogs(&b, Some("Finder"), DialogScope::App).await;
        let _ = unsafe { b.window_for_capture(0, Some("Finder")) };
        assert!(b.current_target().is_none());
        let t = WindowBackend::describe_target(&b, Some("Finder"))
            .await
            .unwrap();
        assert_eq!(t.from, TargetSource::Named);
        assert!(b.current_target().is_none());
    }

    /// With nothing named and nothing pinned the call acts on the frontmost
    /// app, and says that is what it did.
    #[tokio::test]
    async fn with_nothing_pinned_the_frontmost_app_is_the_target_and_is_reported_as_such() {
        let b = MacosBackend::new();
        let (front, pointer) = desk();
        match WindowBackend::describe_target(&b, None).await {
            Ok(t) => {
                eprintln!(
                    "frontmost reported as {t:?}; CG frontmost is {}",
                    name_of(front)
                );
                assert_eq!(t.from, TargetSource::Frontmost);
                assert!(t.pid.is_some_and(|p| pid_alive(p as c_int)));
            }
            Err(WindowError::NotFound(m)) => eprintln!("no frontmost app: {m}"),
            Err(e) => panic!("unexpected: {e:?}"),
        }
        assert!(
            b.current_target().is_none(),
            "describing pinned the frontmost app"
        );
        assert_eq!(desk(), (front, pointer));
    }

    /// The same id means the same window whether the list was asked for one
    /// application or for all of them.
    #[tokio::test]
    async fn a_window_id_means_the_same_window_in_the_whole_list_and_in_the_apps_own() {
        let b = MacosBackend::new();
        let Ok(all) = WindowBackend::list_windows(&b, None).await else {
            return;
        };
        let mut names: Vec<(u32, String)> = all
            .iter()
            .filter_map(|w| Some((w.pid?, w.app.clone()?)))
            .collect();
        names.sort();
        names.dedup();
        let mut checked = 0;
        for (pid, name) in &names {
            // Two processes with one name cannot be told apart by name.
            if names.iter().filter(|(_, n)| n == name).count() > 1 {
                continue;
            }
            let Ok(own) = WindowBackend::list_windows(&b, Some(name)).await else {
                continue;
            };
            let from_all: Vec<_> = all
                .iter()
                .filter(|w| w.pid == Some(*pid))
                .map(|w| (w.id, w.title.clone(), w.state))
                .collect();
            let from_own: Vec<_> = own
                .iter()
                .map(|w| (w.id, w.title.clone(), w.state))
                .collect();
            assert_eq!(
                from_all, from_own,
                "'{name}' (pid {pid}) is numbered two ways"
            );
            checked += 1;
        }
        eprintln!("{} apps listed, {checked} compared", names.len());
    }

    /// Waiting on or checking another application is not choosing to drive it.
    #[tokio::test]
    async fn a_passive_snapshot_names_an_app_without_pinning_it_or_replacing_the_refs() {
        let b = MacosBackend::new();
        let (front, pointer) = desk();
        let before = b.state.lock().unwrap().paths.len();
        let req = SnapshotRequest {
            app: Some("Finder".into()),
            passive: true,
            max_depth: Some(2),
            ..Default::default()
        };
        match A11yBackend::snapshot(&b, &req).await {
            Ok(raw) => {
                assert_eq!(raw.target.expect("target").from, "named");
                eprintln!("passive read of Finder: {:?}", raw.app);
            }
            Err(BackendError::PermissionDenied(_)) => eprintln!("no Accessibility: skipped"),
            Err(e) => panic!("unexpected: {e:?}"),
        }
        assert!(
            b.current_target().is_none(),
            "a passive read pinned the app"
        );
        assert_eq!(
            b.state.lock().unwrap().paths.len(),
            before,
            "refs were replaced"
        );
        assert!(b.state.lock().unwrap().pid.is_none());
        assert_eq!(desk(), (front, pointer));

        // A reading for acting on does pin, as get_ui_tree { app } always did.
        let req = SnapshotRequest {
            app: Some("Finder".into()),
            max_depth: Some(2),
            ..Default::default()
        };
        if A11yBackend::snapshot(&b, &req).await.is_ok() {
            assert_eq!(
                b.current_target().map(|(_, n)| n).as_deref(),
                Some("Finder")
            );
            assert_eq!(desk(), (front, pointer), "reading Finder raised it");
        }
    }

    /// `allowed_apps` through the real backend: an app outside the list cannot
    /// be listed, pinned or driven by name, and nothing is pinned by trying.
    #[tokio::test]
    async fn an_app_outside_allowed_apps_cannot_be_listed_or_pinned_by_name() {
        let b = Arc::new(MacosBackend::new());
        let m = mcp_window::WindowModule::new(b.clone(), b.clone(), vec!["TextEdit".into()]);
        let ctx = CallCtx::new("t", CancelToken::new());
        for (tool, args) in [
            ("list_windows", json!({"app":"Finder"})),
            ("focus_app", json!({"app":"Finder","activate":false})),
            ("focus_app", json!({"app":"Finder"})),
            ("menu_list", json!({"app":"Finder"})),
            ("handle_dialogs", json!({"app":"Finder"})),
            (
                "control_window",
                json!({"app":"Finder","action":"minimize"}),
            ),
        ] {
            let env = m.call(tool, args.clone(), &ctx).await;
            assert_eq!(
                env.error.map(|e| e.code),
                Some(ErrorCode::PolicyDenied),
                "{tool} {args}"
            );
        }
        assert!(b.current_target().is_none(), "a refused call pinned an app");
    }
}
