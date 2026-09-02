use mcp_types::ErrorCode;

/// The outcome of a policy gate check. `NeedConsent` is reserved for the
/// per-action checks (destructive-input gate, secure-vault reads) that land with
/// the input/credential engines; the tier gate itself only yields `Allow`/`Deny`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny { code: ErrorCode, reason: String },
    NeedConsent { prompt: String },
}

impl Decision {
    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}
