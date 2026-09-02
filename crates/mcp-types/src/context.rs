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
}

impl CallCtx {
    pub fn new(session_id: impl Into<String>, cancel: CancelToken) -> Self {
        CallCtx {
            cancel,
            session_id: session_id.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_token_flips_and_shares() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(!a.is_cancelled());
        b.cancel();
        assert!(a.is_cancelled());
    }
}
