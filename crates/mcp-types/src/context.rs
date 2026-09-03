use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A cooperative cancellation flag. The kill switch (and per-call timeout) trip
/// it; long-running engine work polls it and aborts. Cloneable and cheap.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        CancelToken(Arc::new(AtomicBool::new(false)))
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Has cancellation been requested?
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Somewhere to send a server-initiated message mid-call.
///
/// Deliberately synchronous and infallible: it is called from inside engine
/// loops, and a progress report that could block or fail would be a new way for
/// a tool to hang. An implementation that cannot deliver should drop.
pub trait Notifier: Send + Sync {
    fn notify(&self, method: &str, params: serde_json::Value);
}

/// The client's handle for one call's progress reports.
#[derive(Clone)]
pub struct Progress {
    token: serde_json::Value,
    notifier: Arc<dyn Notifier>,
    sent: Arc<std::sync::atomic::AtomicU32>,
}

/// A cap on frames per call. A poll loop that reports every iteration would
/// otherwise be able to flood the client with a message per 150ms, forever.
const MAX_PROGRESS_FRAMES: u32 = 500;

/// Per-call context injected into every engine. Deliberately does **not** carry
/// a policy handle: policy runs in `mcp-core::dispatch` *before* the engine, so
/// an engine cannot re-decide or skip the gate (see `docs/architecture.md` §5).
///
/// Shared OS state (snapshot arena, session registries) will be added here as
/// the perception/session engines land; for now it carries cancellation and the
/// session id for audit correlation.
#[derive(Clone)]
pub struct CallCtx {
    pub cancel: CancelToken,
    pub session_id: String,
    /// Where to report progress, when the client asked for it. `None` is the
    /// normal case and every call to [`CallCtx::progress`] is then free.
    pub progress: Option<Progress>,
}

impl CallCtx {
    pub fn new(session_id: impl Into<String>, cancel: CancelToken) -> Self {
        CallCtx {
            cancel,
            session_id: session_id.into(),
            progress: None,
        }
    }

    /// Attach a progress channel for this call.
    pub fn with_progress(mut self, token: serde_json::Value, notifier: Arc<dyn Notifier>) -> Self {
        self.progress = Some(Progress {
            token,
            notifier,
            sent: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        });
        self
    }

    /// Report progress. A no-op unless the client supplied a token, so an
    /// engine can call it unconditionally.
    pub fn progress(&self, current: f64, total: Option<f64>, message: Option<&str>) {
        let Some(p) = &self.progress else { return };
        if p.sent.fetch_add(1, Ordering::SeqCst) >= MAX_PROGRESS_FRAMES {
            return;
        }
        let mut params = serde_json::json!({
            "progressToken": p.token,
            "progress": current,
        });
        if let Some(t) = total {
            params["total"] = serde_json::json!(t);
        }
        if let Some(m) = message {
            params["message"] = serde_json::json!(m);
        }
        p.notifier.notify("notifications/progress", params);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(String, serde_json::Value)>>);
    impl Notifier for Recorder {
        fn notify(&self, method: &str, params: serde_json::Value) {
            self.0.lock().unwrap().push((method.to_string(), params));
        }
    }

    /// An engine calls `progress` unconditionally; without a token it must cost
    /// nothing and send nothing.
    #[test]
    fn progress_without_a_token_is_a_no_op() {
        let ctx = CallCtx::new("s", CancelToken::new());
        ctx.progress(1.0, Some(2.0), Some("half"));
        assert!(ctx.progress.is_none());
    }

    #[test]
    fn progress_carries_the_token_and_the_optional_fields() {
        let rec = Arc::new(Recorder::default());
        let ctx = CallCtx::new("s", CancelToken::new())
            .with_progress(serde_json::json!("t1"), rec.clone());
        ctx.progress(1.0, Some(4.0), Some("waiting"));
        ctx.progress(2.0, None, None);
        let sent = rec.0.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, "notifications/progress");
        assert_eq!(sent[0].1["progressToken"], serde_json::json!("t1"));
        assert_eq!(sent[0].1["total"], serde_json::json!(4.0));
        assert_eq!(sent[0].1["message"], serde_json::json!("waiting"));
        // Absent rather than null, so a client need not special-case them.
        assert!(sent[1].1.get("total").is_none());
        assert!(sent[1].1.get("message").is_none());
    }

    /// A poll loop reporting every iteration could otherwise flood a client
    /// with a message every 150ms for as long as it runs.
    #[test]
    fn progress_frames_are_capped_per_call() {
        let rec = Arc::new(Recorder::default());
        let ctx =
            CallCtx::new("s", CancelToken::new()).with_progress(serde_json::json!(7), rec.clone());
        for i in 0..(MAX_PROGRESS_FRAMES + 50) {
            ctx.progress(f64::from(i), None, None);
        }
        assert_eq!(rec.0.lock().unwrap().len() as u32, MAX_PROGRESS_FRAMES);
    }

    #[test]
    fn cancel_token_flips_and_shares() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!a.is_cancelled());
        b.cancel();
        assert!(a.is_cancelled());
    }
}
