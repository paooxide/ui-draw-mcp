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
