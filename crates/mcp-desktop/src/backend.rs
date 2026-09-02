use async_trait::async_trait;
use serde::Serialize;

/// Why a desktop operation failed.
#[derive(Debug, Clone)]
pub enum DesktopError {
    PermissionDenied(String),
    NotFound(String),
    /// The platform genuinely offers no interface for this. Not a placeholder:
    /// the message names what is missing so a caller can stop retrying.
    Unsupported(String),
    Failed(String),
}

/// Session presence.
#[derive(Debug, Clone, Serialize)]
pub struct IdleStatus {
    /// Seconds since the last human input event.
    pub idle_seconds: u64,
    /// Screen is locked.
    pub locked: bool,
    /// Someone is plausibly at the keyboard.
    pub user_present: bool,
}

/// A power-state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Sleep,
    Logout,
    Restart,
    Shutdown,
}

impl PowerAction {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sleep" => Self::Sleep,
            "logout" => Self::Logout,
            "restart" => Self::Restart,
            "shutdown" => Self::Shutdown,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sleep => "sleep",
            Self::Logout => "logout",
            Self::Restart => "restart",
            Self::Shutdown => "shutdown",
        }
    }
    /// Does this end the session (and with it, every running safeguard)?
    pub fn ends_session(self) -> bool {
        !matches!(self, Self::Sleep)
    }
}

/// Media transport control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaAction {
    PlayPause,
    Next,
    Prev,
    Stop,
}

impl MediaAction {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "play_pause" => Self::PlayPause,
            "next" => Self::Next,
            "prev" => Self::Prev,
            "stop" => Self::Stop,
            _ => return None,
        })
    }
}

/// Platform session/power/settings backend.
#[async_trait]
pub trait DesktopBackend: Send + Sync {
    async fn lock_screen(&self) -> Result<(), DesktopError>;
    /// Post a desktop notification. The agent→human channel.
    async fn notify(
        &self,
        title: &str,
        body: &str,
        subtitle: Option<&str>,
        sound: bool,
    ) -> Result<(), DesktopError>;
    async fn idle_status(&self) -> Result<IdleStatus, DesktopError>;
    /// Read a setting. `Ok(None)` means "readable in principle, no value now".
    async fn setting_get(&self, setting: &str) -> Result<serde_json::Value, DesktopError>;
    async fn setting_set(&self, setting: &str, value: &str) -> Result<(), DesktopError>;
    /// Returns the player it reached.
    async fn media(&self, action: MediaAction) -> Result<String, DesktopError>;
    async fn power(&self, action: PowerAction, delay_s: u64) -> Result<(), DesktopError>;
    async fn speak(
        &self,
        text: &str,
        voice: Option<&str>,
        rate: Option<u32>,
    ) -> Result<(), DesktopError>;
    async fn play_audio(&self, path: &std::path::Path) -> Result<(), DesktopError>;
    fn platform(&self) -> &'static str;
}
