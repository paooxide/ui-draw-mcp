use async_trait::async_trait;

use crate::tree::UiNode;

/// A snapshot request (subset of the `get_ui_tree` arguments a backend needs).
#[derive(Debug, Clone, Default)]
pub struct SnapshotRequest {
    pub app: Option<String>,
    pub skeleton: bool,
    pub root_ref: Option<String>,
    pub max_depth: Option<usize>,
    pub surface: Option<String>,
    /// A read for waiting or checking, not for acting on. It must not pin the
    /// application it names, and must not replace the elements the refs of
    /// the last `get_ui_tree` point at: a poll of another application would
    /// otherwise leave `@e5` meaning an element of the wrong one.
    pub passive: bool,
    /// Install the refs of this read so `ui_action` can act on them, but
    /// leave the session's pinned application alone. A read for acting on
    /// normally makes the application it reads the one every later call
    /// follows; `observe` reads a window it was merely asked about and must
    /// not change which application the session is driving. Ignored when
    /// `passive` is set, which installs nothing.
    pub without_pin: bool,
}

/// A raw accessibility snapshot from a backend: the root node plus labels. The
/// flattener turns this into text + a ref index.
#[derive(Debug, Clone)]
pub struct RawSnapshot {
    pub root: UiNode,
    pub app: Option<String>,
    pub window: Option<String>,
    /// True for terminal apps (keep value tails, not heads).
    pub terminal_app: bool,
    /// True when the backend stopped traversing early — it ran out of time or
    /// hit a node cap — so the tree is a prefix of the real one.
    ///
    /// This has to be distinguishable from "the app really has this little in
    /// it", because an agent that cannot tell will conclude the button it needs
    /// does not exist.
    pub partial: bool,
    /// The application the snapshot is of and why that one; `None` for a
    /// backend that does not say.
    pub target: Option<SnapshotTarget>,
}

/// Which application a snapshot read, and how the call came to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotTarget {
    pub pid: Option<u32>,
    /// `named` (the call's `app`), `pinned` or `frontmost`.
    pub from: &'static str,
}

/// Why a backend snapshot failed. Mapped to `ErrorCode` by the tool layer.
#[derive(Debug, Clone)]
pub enum BackendError {
    /// OS accessibility permission not granted (macOS TCC, etc.).
    PermissionDenied(String),
    /// The requested app/window/root was not found.
    NotFound(String),
    /// Not supported on this platform.
    Unsupported(String),
    /// The snapshot was attempted but failed.
    Failed(String),
}

/// A platform accessibility backend. macOS (AXUIElement) lands behind the
/// `macos-backend` feature; UIA/AT-SPI follow. All implementations produce a
/// `UiNode` tree so everything downstream stays OS-independent.
#[async_trait]
pub trait A11yBackend: Send + Sync {
    async fn snapshot(&self, req: &SnapshotRequest) -> Result<RawSnapshot, BackendError>;

    /// A short platform tag for diagnostics (e.g. `"macos"`, `"fake"`).
    fn platform(&self) -> &'static str;
}
