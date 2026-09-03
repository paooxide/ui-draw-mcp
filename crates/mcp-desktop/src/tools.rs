use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::{CallCtx, Category, Envelope, ErrorCode, Tier, ToolDescriptor, ToolModule};
use serde_json::{json, Value};

use crate::backend::{DesktopBackend, DesktopError, MediaAction, PowerAction};

/// The `desktop` engine.
pub struct DesktopModule {
    backend: Arc<dyn DesktopBackend>,
    /// Roots `play_audio` may read from. Empty = it plays nothing: an agent
    /// able to name any path could use the speakers to read out a file it was
    /// never allowed to open.
    audio_roots: Vec<PathBuf>,
    max_notify_len: usize,
}

impl DesktopModule {
    pub fn new(backend: Arc<dyn DesktopBackend>, audio_roots: Vec<PathBuf>) -> Self {
        // Canonicalise, as the filesystem jail does — the comparison here is
        // against a resolved path, so the roots must be resolved too.
        let audio_roots = audio_roots
            .into_iter()
            .map(|r| std::fs::canonicalize(&r).unwrap_or(r))
            .collect();
        DesktopModule {
            backend,
            audio_roots,
            max_notify_len: 500,
        }
    }
}

fn err(tool: &str, e: DesktopError) -> Envelope {
    let (code, msg) = match e {
        DesktopError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
        DesktopError::NotFound(m) => (ErrorCode::NotFound, m),
        DesktopError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
        DesktopError::Failed(m) => (ErrorCode::ActionFailed, m),
    };
    Envelope::fail(tool, code, msg)
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

impl DesktopModule {
    async fn notify(&self, args: &Value) -> Envelope {
        let tool = "notify_user";
        let Some(title) = str_arg(args, "title").filter(|t| !t.is_empty()) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'title'");
        };
        let Some(body) = str_arg(args, "body") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'body'");
        };
        // A notification the OS silently truncates is worse than a refusal: the
        // agent believes it delivered a message the human never saw in full.
        if title.chars().count() + body.chars().count() > self.max_notify_len {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!("title+body exceeds {} characters", self.max_notify_len),
                "notifications are a signal, not a transcript — send a summary",
            );
        }
        let urgency = str_arg(args, "urgency").unwrap_or("normal");
        let sound = matches!(urgency, "critical" | "high");
        match self
            .backend
            .notify(title, body, str_arg(args, "subtitle"), sound)
            .await
        {
            Ok(()) => Envelope::ok(tool, json!({ "delivered": true, "urgency": urgency })),
            Err(e) => err(tool, e),
        }
    }

    async fn idle(&self) -> Envelope {
        match self.backend.idle_status().await {
            Ok(s) => Envelope::ok("idle_status", json!(s)),
            Err(e) => err("idle_status", e),
        }
    }

    async fn lock(&self) -> Envelope {
        match self.backend.lock_screen().await {
            Ok(()) => Envelope::ok("lock_screen", json!({ "locked": true })),
            Err(e) => err("lock_screen", e),
        }
    }

    async fn settings(&self, args: &Value) -> Envelope {
        let tool = "system_settings";
        let Some(setting) = str_arg(args, "setting") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'setting'");
        };
        match str_arg(args, "action").unwrap_or("get") {
            "get" => match self.backend.setting_get(setting).await {
                Ok(v) => Envelope::ok(tool, json!({ "setting": setting, "value": v })),
                Err(e) => err(tool, e),
            },
            "set" => {
                let Some(value) = str_arg(args, "value") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "'set' needs 'value'");
                };
                match self.backend.setting_set(setting, value).await {
                    Ok(()) => Envelope::ok(tool, json!({ "setting": setting, "set": value })),
                    Err(e) => err(tool, e),
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown action '{other}' (use get|set)"),
            ),
        }
    }

    async fn media(&self, args: &Value) -> Envelope {
        let tool = "media_control";
        let Some(action) = str_arg(args, "action").and_then(MediaAction::parse) else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "missing or invalid 'action' (play_pause|next|prev|stop)",
            );
        };
        match self.backend.media(action).await {
            Ok(player) => Envelope::ok(tool, json!({ "ok": true, "player": player })),
            Err(e) => err(tool, e),
        }
    }

    async fn power(&self, args: &Value) -> Envelope {
        let tool = "power_control";
        let Some(action) = str_arg(args, "action").and_then(PowerAction::parse) else {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "missing or invalid 'action' (sleep|logout|restart|shutdown)",
            );
        };
        let delay_s = args.get("delay_s").and_then(Value::as_u64).unwrap_or(0);
        match self.backend.power(action, delay_s).await {
            Ok(()) => Envelope::ok(tool, json!({ "ok": true, "action": action.as_str() })),
            Err(e) => err(tool, e),
        }
    }

    async fn speak(&self, args: &Value) -> Envelope {
        let tool = "speak";
        let Some(text) = str_arg(args, "text").filter(|t| !t.is_empty()) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'text'");
        };
        if text.chars().count() > 2000 {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'text' exceeds 2000 characters",
            );
        }
        match self
            .backend
            .speak(
                text,
                str_arg(args, "voice"),
                args.get("rate").and_then(Value::as_u64).map(|r| r as u32),
            )
            .await
        {
            Ok(()) => Envelope::ok(tool, json!({ "spoken": text.chars().count() })),
            Err(e) => err(tool, e),
        }
    }

    async fn play_audio(&self, args: &Value) -> Envelope {
        let tool = "play_audio";
        let Some(raw) = str_arg(args, "path") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'path'");
        };
        let path = match std::fs::canonicalize(raw) {
            Ok(p) => p,
            Err(e) => return Envelope::fail(tool, ErrorCode::NotFound, format!("{raw}: {e}")),
        };
        // Resolve first, then check — the same order the filesystem jail uses,
        // so `..` and symlinks cannot walk out of an allowed root.
        if !self.audio_roots.iter().any(|r| path.starts_with(r)) {
            return Envelope::fail_with(
                tool,
                ErrorCode::PolicyDenied,
                format!("'{}' is outside the configured roots", path.display()),
                "add the directory to fs.roots to allow playback from it",
            );
        }
        match self.backend.play_audio(&path).await {
            Ok(()) => Envelope::ok(tool, json!({ "played": path.display().to_string() })),
            Err(e) => err(tool, e),
        }
    }
}

#[async_trait]
impl ToolModule for DesktopModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::new(
                "notify_user",
                Category::Desktop,
                Tier::Standard,
                "Post a desktop notification. This is the agent→human channel: use it to report \
                 completion, to surface something that needs a person, or to explain why work stopped.",
                json!({"type":"object","properties":{
                    "title":{"type":"string"},"body":{"type":"string"},
                    "subtitle":{"type":"string"},
                    "urgency":{"type":"string","enum":["low","normal","high","critical"],
                               "description":"high/critical also play a sound"}},
                    "required":["title","body"]}),
            ),
            ToolDescriptor::new(
                "idle_status",
                Category::Desktop,
                Tier::Read,
                "Seconds since the last human input, whether the screen is locked, and whether \
                 someone is plausibly present. Check before anything that would interrupt.",
                json!({"type":"object","properties":{},"required":[]}),
            ),
            ToolDescriptor::new(
                "lock_screen",
                Category::Desktop,
                Tier::Standard,
                "Lock the session.",
                json!({"type":"object","properties":{},"required":[]}),
            ),
            ToolDescriptor::new(
                "system_settings",
                Category::Desktop,
                Tier::Standard,
                "Read or change a desktop setting. Settings the platform does not expose return \
                 UNSUPPORTED_OS naming the reason.",
                json!({"type":"object","properties":{
                    "setting":{"type":"string","enum":["volume","brightness","dark_mode","resolution","dnd"]},
                    "action":{"type":"string","enum":["get","set"]},
                    "value":{"type":"string"}},
                    "required":["setting"]}),
            ),
            ToolDescriptor::new(
                "media_control",
                Category::Desktop,
                Tier::Standard,
                "Transport control for the running media player. Never launches one.",
                json!({"type":"object","properties":{
                    "action":{"type":"string","enum":["play_pause","next","prev","stop"]}},
                    "required":["action"]}),
            ),
            ToolDescriptor::new(
                "power_control",
                Category::Desktop,
                Tier::Dangerous,
                "Sleep, log out, restart or shut down. Ending the session ends every running \
                 safeguard with it, so anything beyond sleep asks for confirmation.",
                json!({"type":"object","properties":{
                    "action":{"type":"string","enum":["sleep","logout","restart","shutdown"]},
                    "delay_s":{"type":"integer","description":"wait before acting, max 300"}},
                    "required":["action"]}),
            ),
            ToolDescriptor::new(
                "speak",
                Category::Desktop,
                Tier::Standard,
                "Speak text through the speakers.",
                json!({"type":"object","properties":{
                    "text":{"type":"string"},"voice":{"type":"string"},
                    "rate":{"type":"integer","description":"words per minute, 50-500"}},
                    "required":["text"]}),
            ),
            ToolDescriptor::new(
                "play_audio",
                Category::Desktop,
                Tier::Standard,
                "Play an audio file. The path must resolve inside a configured filesystem root.",
                json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
            ),
        ]
    }

    /// Power transitions that end the session need a human. Sleep does not —
    /// it is reversible and destroys nothing.
    fn consent_prompt(&self, name: &str, args: &Value) -> Option<String> {
        if name != "power_control" {
            return None;
        }
        let action = PowerAction::parse(str_arg(args, "action")?)?;
        action.ends_session().then(|| {
            format!(
                "Allow the agent to {} this machine? Every running task and safeguard stops.",
                action.as_str()
            )
        })
    }

    async fn call(&self, name: &str, args: Value, _ctx: &CallCtx) -> Envelope {
        match name {
            "notify_user" => self.notify(&args).await,
            "idle_status" => self.idle().await,
            "lock_screen" => self.lock().await,
            "system_settings" => self.settings(&args).await,
            "media_control" => self.media(&args).await,
            "power_control" => self.power(&args).await,
            "speak" => self.speak(&args).await,
            "play_audio" => self.play_audio(&args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}
