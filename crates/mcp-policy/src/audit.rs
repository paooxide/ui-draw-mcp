use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// One cryptographically chained audit record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditRecord {
    #[serde(default)]
    pub seq: u64,
    pub phase: String,
    pub ts_ms: u128,
    pub session_id: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args_redacted: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default)]
    pub prev_hash: String,
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub sig: String,
}

impl AuditRecord {
    pub fn pre(session_id: &str, tool: &str) -> Self {
        AuditRecord {
            seq: 0,
            phase: "pre".to_string(),
            ts_ms: now_ms(),
            session_id: session_id.to_string(),
            tool: tool.to_string(),
            tier: None,
            decision: None,
            args_redacted: None,
            ok: None,
            error_code: None,
            latency_ms: None,
            public_key: None,
            role: None,
            prev_hash: String::new(),
            hash: String::new(),
            sig: String::new(),
        }
    }

    pub fn post(session_id: &str, tool: &str) -> Self {
        AuditRecord {
            phase: "post".to_string(),
            ..AuditRecord::pre(session_id, tool)
        }
    }

    /// Computes the SHA-256 hash of this audit record's canonical data and previous hash.
    pub fn compute_hash(&self) -> String {
        let canonical_args = self
            .args_redacted
            .as_ref()
            .map(canonical_json)
            .unwrap_or_default();
        let raw = format!(
            "seq={}|phase={}|ts_ms={}|session_id={}|tool={}|tier={}|decision={}|args={}|ok={}|err={}|lat={}|pk={}|role={}|prev={}",
            self.seq,
            self.phase,
            self.ts_ms,
            self.session_id,
            self.tool,
            self.tier.as_deref().unwrap_or(""),
            self.decision.as_deref().unwrap_or(""),
            canonical_args,
            self.ok.map(|b| if b { "true" } else { "false" }).unwrap_or(""),
            self.error_code.as_deref().unwrap_or(""),
            self.latency_ms.map(|l| l.to_string()).unwrap_or_default(),
            self.public_key.as_deref().unwrap_or(""),
            self.role.as_deref().unwrap_or(""),
            self.prev_hash,
        );
        hex::encode(Sha256::digest(raw.as_bytes()))
    }
}

/// Produces deterministic canonical JSON without whitespace and with alphabetically sorted object keys.
pub fn canonical_json(val: &Value) -> String {
    match val {
        Value::Null => "null".to_string(),
        Value::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\"")),
        Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(k, _)| *k);
            let inner: Vec<String> = entries
                .into_iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canonical_json(v)
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

/// Computes the SHA-256 hash of an audit record's canonical data and previous hash.
pub fn compute_record_hash(record: &AuditRecord) -> String {
    record.compute_hash()
}

/// Verification report detailing cryptographic integrity status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditVerificationReport {
    pub valid: bool,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub total_records: usize,
    pub public_key: String,
    pub root_hash: String,
    pub leaf_hash: String,
    /// Whether the signer was checked against a key the verifier already
    /// trusted. Without that, a valid chain only shows the log is
    /// self-consistent: anyone able to rewrite it can re-sign it with a fresh
    /// key and embed that key in record 0.
    #[serde(default)]
    pub key_pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AuditTamperError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditTamperError {
    pub seq: u64,
    pub reason: String,
}

struct ChainState {
    seq: u64,
    prev_hash: String,
    signing_key: SigningKey,
    public_key_hex: String,
    role: Option<String>,
    root_hash: Option<String>,
    leaf_hash: Option<String>,
}

enum Inner {
    File(PathBuf),
    Memory(Vec<Value>),
}

/// Cryptographically chained append-only audit sink.
pub struct AuditSink {
    inner: Mutex<Inner>,
    chain: Mutex<ChainState>,
}

impl AuditSink {
    /// Create a file-backed sink at `<dir>/<session_id>.jsonl`, signed with a
    /// fresh key that exists only for this session.
    pub fn file(dir: PathBuf, session_id: &str) -> std::io::Result<Self> {
        Self::file_with_key(dir, session_id, None)
    }

    /// As [`Self::file`], signing with `key` when the operator provisioned one
    /// (see [`load_signing_key`]). Only a provisioned key lets a verifier tell
    /// this log from one rewritten and re-signed after the fact.
    pub fn file_with_key(
        dir: PathBuf,
        session_id: &str,
        key: Option<SigningKey>,
    ) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{session_id}.jsonl"));
        let pub_key_path = dir.join(format!("{session_id}.pub.key"));
        let signing_key = key.unwrap_or_else(|| SigningKey::generate(&mut rand::rngs::OsRng));
        let public_key_hex = hex::encode(signing_key.verifying_key().as_bytes());
        let _ = fs::write(&pub_key_path, &public_key_hex);
        Ok(AuditSink {
            inner: Mutex::new(Inner::File(path)),
            chain: Mutex::new(ChainState {
                seq: 0,
                prev_hash: GENESIS_PREV_HASH.to_string(),
                signing_key,
                public_key_hex,
                role: None,
                root_hash: None,
                leaf_hash: None,
            }),
        })
    }

    /// Create an in-memory sink with an ephemeral Ed25519 signing keypair (tests).
    pub fn memory() -> Self {
        let signing_key = SigningKey::generate(&mut rand::rngs::OsRng);
        let public_key_hex = hex::encode(signing_key.verifying_key().as_bytes());
        AuditSink {
            inner: Mutex::new(Inner::Memory(Vec::new())),
            chain: Mutex::new(ChainState {
                seq: 0,
                prev_hash: GENESIS_PREV_HASH.to_string(),
                signing_key,
                public_key_hex,
                role: None,
                root_hash: None,
                leaf_hash: None,
            }),
        }
    }

    /// Configure a specific signing key (e.g. for deterministic testing or corporate PKI).
    pub fn with_signing_key(self, signing_key: SigningKey) -> Self {
        let public_key_hex = hex::encode(signing_key.verifying_key().as_bytes());
        let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        chain.signing_key = signing_key;
        chain.public_key_hex = public_key_hex;
        drop(chain);
        self
    }

    /// Bind an active role profile to this audit session.
    pub fn with_role(self, role: impl Into<String>) -> Self {
        let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        chain.role = Some(role.into());
        drop(chain);
        self
    }

    /// The active role bound to this audit session, if any.
    pub fn role(&self) -> Option<String> {
        self.chain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .role
            .clone()
    }

    /// Public key (hex) for this session's Ed25519 signer.
    pub fn public_key_hex(&self) -> String {
        self.chain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .public_key_hex
            .clone()
    }

    /// Root hash of the first recorded audit record.
    pub fn root_hash(&self) -> Option<String> {
        self.chain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .root_hash
            .clone()
    }

    /// Leaf hash of the most recent audit record.
    pub fn leaf_hash(&self) -> Option<String> {
        self.chain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .leaf_hash
            .clone()
    }

    /// Monotonic sequence count of records written so far.
    pub fn current_seq(&self) -> u64 {
        self.chain.lock().unwrap_or_else(|e| e.into_inner()).seq
    }

    /// Write one cryptographically chained and signed audit record.
    pub fn write(&self, record: &AuditRecord) {
        let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        let mut rec = record.clone();
        rec.seq = chain.seq;
        rec.prev_hash = chain.prev_hash.clone();
        if rec.role.is_none() {
            rec.role = chain.role.clone();
        }
        if chain.seq == 0 {
            rec.public_key = Some(chain.public_key_hex.clone());
        } else {
            rec.public_key = None;
        }

        let hash = rec.compute_hash();

        let sig = hex::encode(chain.signing_key.sign(hash.as_bytes()).to_bytes());
        rec.hash = hash.clone();
        rec.sig = sig;

        if chain.seq == 0 {
            chain.root_hash = Some(hash.clone());
        }
        chain.leaf_hash = Some(hash.clone());
        chain.prev_hash = hash;
        chain.seq += 1;

        let value = match serde_json::to_value(&rec) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "failed to serialize audit record");
                return;
            }
        };

        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match &mut *guard {
            Inner::Memory(buf) => buf.push(value),
            Inner::File(path) => {
                let line = value.to_string();
                match OpenOptions::new().create(true).append(true).open(&*path) {
                    Ok(mut f) => {
                        if let Err(e) = writeln!(f, "{line}") {
                            tracing::error!(error = %e, "failed to write audit record");
                        }
                    }
                    Err(e) => tracing::error!(error = %e, "failed to open audit log"),
                }
            }
        }
    }

    /// The records written so far (in-memory sinks only; empty for file sinks).
    pub fn memory_records(&self) -> Vec<Value> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match &*guard {
            Inner::Memory(buf) => buf.clone(),
            Inner::File(_) => Vec::new(),
        }
    }

    /// The log path, for file sinks.
    pub fn path(&self) -> Option<PathBuf> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match &*guard {
            Inner::File(p) => Some(p.clone()),
            Inner::Memory(_) => None,
        }
    }
}

/// Load an operator-provisioned audit signing key: a file holding the 32-byte
/// Ed25519 seed as 64 hex characters.
///
/// Refused when the file is readable by group or others, as ssh does with
/// private keys: a key other accounts can read can sign forged logs.
pub fn load_signing_key(path: &Path) -> Result<SigningKey, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "{}: audit signing key is readable by other users (mode {mode:o}); run chmod 600",
                path.display()
            ));
        }
    }
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bytes = hex::decode(text.trim())
        .map_err(|e| format!("{}: not a hex Ed25519 seed: {e}", path.display()))?;
    let seed: [u8; 32] = bytes.try_into().map_err(|b: Vec<u8>| {
        format!(
            "{}: expected a 32-byte seed (64 hex characters), got {} bytes",
            path.display(),
            b.len()
        )
    })?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Create a new audit signing key at `path` (mode 0600, never overwriting) and
/// return its public half as hex, which verifiers pin with `--pubkey`.
pub fn generate_signing_key_file(path: &Path) -> Result<String, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writeln!(f, "{}", hex::encode(key.to_bytes()))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(hex::encode(key.verifying_key().as_bytes()))
}

/// Parse a pinned verifying key given as 64 hex characters.
pub fn verifying_key_from_hex(hex_str: &str) -> Result<VerifyingKey, String> {
    let bytes = hex::decode(hex_str.trim()).map_err(|e| format!("public key is not hex: {e}"))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|b: Vec<u8>| format!("public key must be 32 bytes, got {}", b.len()))?;
    VerifyingKey::from_bytes(&arr).map_err(|e| format!("invalid Ed25519 public key: {e}"))
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Parse a JSONL text stream into typed `AuditRecord`s.
pub fn parse_audit_records(jsonl: &str) -> Result<Vec<AuditRecord>, String> {
    let mut records = Vec::new();
    for (idx, line) in jsonl.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rec: AuditRecord = serde_json::from_str(trimmed)
            .map_err(|e| format!("line {}: failed to parse audit record: {e}", idx + 1))?;
        records.push(rec);
    }
    Ok(records)
}

/// Cryptographically verify the integrity of an array of audit records.
///
/// The signer is taken from record 0 of the log itself, so a valid result
/// shows only that the log is self-consistent. Use
/// [`verify_audit_records_pinned`] with the operator's public key to show it
/// was signed by that key.
pub fn verify_audit_records(records: &[AuditRecord]) -> AuditVerificationReport {
    verify_audit_records_pinned(records, None)
}

/// As [`verify_audit_records`], additionally requiring that the log was signed
/// by `pinned` when one is given.
pub fn verify_audit_records_pinned(
    records: &[AuditRecord],
    pinned: Option<&VerifyingKey>,
) -> AuditVerificationReport {
    let mut report = verify_chain(records);
    let Some(pinned) = pinned else {
        return report;
    };
    report.key_pinned = true;
    let pinned_hex = hex::encode(pinned.as_bytes());
    if report.valid && !records.is_empty() && !report.public_key.eq_ignore_ascii_case(&pinned_hex) {
        report.valid = false;
        report.error = Some(AuditTamperError {
            seq: 0,
            reason: format!(
                "log is signed by key {} but the pinned key is {pinned_hex}; \
                 it was not written by this operator's agentctl, or it was re-signed",
                report.public_key
            ),
        });
    }
    report
}

fn verify_chain(records: &[AuditRecord]) -> AuditVerificationReport {
    if records.is_empty() {
        return AuditVerificationReport {
            valid: true,
            session_id: String::new(),
            role: None,
            total_records: 0,
            public_key: String::new(),
            root_hash: String::new(),
            leaf_hash: String::new(),
            key_pinned: false,
            error: None,
        };
    }

    let first = &records[0];
    let session_id = first.session_id.clone();
    let role = first.role.clone();

    // 1. Genesis record validation (seq == 0)
    if first.seq != 0 {
        return AuditVerificationReport {
            valid: false,
            session_id,
            role,
            total_records: records.len(),
            public_key: String::new(),
            root_hash: String::new(),
            leaf_hash: String::new(),
            key_pinned: false,
            error: Some(AuditTamperError {
                seq: first.seq,
                reason: format!("expected sequence 0 for first record, got {}", first.seq),
            }),
        };
    }

    if first.prev_hash != GENESIS_PREV_HASH {
        return AuditVerificationReport {
            valid: false,
            session_id,
            role,
            total_records: records.len(),
            public_key: String::new(),
            root_hash: String::new(),
            leaf_hash: String::new(),
            key_pinned: false,
            error: Some(AuditTamperError {
                seq: 0,
                reason: format!(
                    "first record does not point to genesis prev_hash (got '{}')",
                    first.prev_hash
                ),
            }),
        };
    }

    let Some(public_key_hex) = &first.public_key else {
        return AuditVerificationReport {
            valid: false,
            session_id,
            role,
            total_records: records.len(),
            public_key: String::new(),
            root_hash: String::new(),
            leaf_hash: String::new(),
            key_pinned: false,
            error: Some(AuditTamperError {
                seq: 0,
                reason: "missing public_key in record 0".to_string(),
            }),
        };
    };

    let pub_bytes = match hex::decode(public_key_hex) {
        Ok(b) => b,
        Err(_) => {
            return AuditVerificationReport {
                valid: false,
                session_id,
                role,
                total_records: records.len(),
                public_key: public_key_hex.clone(),
                root_hash: String::new(),
                leaf_hash: String::new(),
                key_pinned: false,
                error: Some(AuditTamperError {
                    seq: 0,
                    reason: "invalid hex encoding for public_key".to_string(),
                }),
            };
        }
    };

    let pub_array: [u8; 32] = match pub_bytes.try_into() {
        Ok(arr) => arr,
        Err(_) => {
            return AuditVerificationReport {
                valid: false,
                session_id,
                role,
                total_records: records.len(),
                public_key: public_key_hex.clone(),
                root_hash: String::new(),
                leaf_hash: String::new(),
                key_pinned: false,
                error: Some(AuditTamperError {
                    seq: 0,
                    reason: "public_key must be 32 bytes".to_string(),
                }),
            };
        }
    };

    let verifying_key = match VerifyingKey::from_bytes(&pub_array) {
        Ok(vk) => vk,
        Err(e) => {
            return AuditVerificationReport {
                valid: false,
                session_id,
                role,
                total_records: records.len(),
                public_key: public_key_hex.clone(),
                root_hash: String::new(),
                leaf_hash: String::new(),
                key_pinned: false,
                error: Some(AuditTamperError {
                    seq: 0,
                    reason: format!("invalid Ed25519 verifying key: {e}"),
                }),
            };
        }
    };

    let mut expected_prev_hash = GENESIS_PREV_HASH.to_string();
    let root_hash = first.hash.clone();
    let mut leaf_hash = String::new();

    for (idx, rec) in records.iter().enumerate() {
        let expected_seq = idx as u64;
        if rec.seq != expected_seq {
            return AuditVerificationReport {
                valid: false,
                session_id,
                role: role.clone(),
                total_records: records.len(),
                public_key: public_key_hex.clone(),
                root_hash,
                leaf_hash,
                key_pinned: false,
                error: Some(AuditTamperError {
                    seq: rec.seq,
                    reason: format!(
                        "sequence gap or reordering: expected {expected_seq}, got {}",
                        rec.seq
                    ),
                }),
            };
        }

        if rec.prev_hash != expected_prev_hash {
            return AuditVerificationReport {
                valid: false,
                session_id,
                role: role.clone(),
                total_records: records.len(),
                public_key: public_key_hex.clone(),
                root_hash,
                leaf_hash,
                key_pinned: false,
                error: Some(AuditTamperError {
                    seq: rec.seq,
                    reason: format!(
                        "broken hash chain at seq {}: prev_hash '{}' does not match expected '{}'",
                        rec.seq, rec.prev_hash, expected_prev_hash
                    ),
                }),
            };
        }

        // Recompute content hash
        let computed_hash = rec.compute_hash();

        if computed_hash != rec.hash {
            return AuditVerificationReport {
                valid: false,
                session_id,
                role: role.clone(),
                total_records: records.len(),
                public_key: public_key_hex.clone(),
                root_hash,
                leaf_hash,
                key_pinned: false,
                error: Some(AuditTamperError {
                    seq: rec.seq,
                    reason: format!(
                        "tampered content hash at seq {}: computed '{computed_hash}', recorded '{}'",
                        rec.seq, rec.hash
                    ),
                }),
            };
        }

        // Verify Ed25519 signature
        let sig_bytes = match hex::decode(&rec.sig) {
            Ok(b) => b,
            Err(_) => {
                return AuditVerificationReport {
                    valid: false,
                    session_id,
                    role: role.clone(),
                    total_records: records.len(),
                    public_key: public_key_hex.clone(),
                    root_hash,
                    leaf_hash,
                    key_pinned: false,
                    error: Some(AuditTamperError {
                        seq: rec.seq,
                        reason: format!("invalid hex encoding for signature at seq {}", rec.seq),
                    }),
                };
            }
        };

        let sig_array: [u8; 64] = match sig_bytes.try_into() {
            Ok(arr) => arr,
            Err(_) => {
                return AuditVerificationReport {
                    valid: false,
                    session_id,
                    role: role.clone(),
                    total_records: records.len(),
                    public_key: public_key_hex.clone(),
                    root_hash,
                    leaf_hash,
                    key_pinned: false,
                    error: Some(AuditTamperError {
                        seq: rec.seq,
                        reason: format!("signature at seq {} must be 64 bytes", rec.seq),
                    }),
                };
            }
        };

        let signature = Signature::from_bytes(&sig_array);
        if let Err(e) = verifying_key.verify(rec.hash.as_bytes(), &signature) {
            return AuditVerificationReport {
                valid: false,
                session_id,
                role: role.clone(),
                total_records: records.len(),
                public_key: public_key_hex.clone(),
                root_hash,
                leaf_hash,
                key_pinned: false,
                error: Some(AuditTamperError {
                    seq: rec.seq,
                    reason: format!(
                        "cryptographic signature verification failed at seq {}: {e}",
                        rec.seq
                    ),
                }),
            };
        }

        expected_prev_hash = rec.hash.clone();
        leaf_hash = rec.hash.clone();
    }

    AuditVerificationReport {
        valid: true,
        session_id,
        role,
        total_records: records.len(),
        public_key: public_key_hex.clone(),
        root_hash,
        leaf_hash,
        key_pinned: false,
        error: None,
    }
}

/// Verify a raw JSONL text stream.
pub fn verify_audit_log(jsonl: &str) -> Result<AuditVerificationReport, String> {
    let records = parse_audit_records(jsonl)?;
    Ok(verify_audit_records(&records))
}

/// Verify a file on disk.
pub fn verify_audit_file(path: &Path) -> Result<AuditVerificationReport, String> {
    verify_audit_file_pinned(path, None)
}

/// Verify a file on disk, optionally against a pinned signer.
pub fn verify_audit_file_pinned(
    path: &Path,
    pinned: Option<&VerifyingKey>,
) -> Result<AuditVerificationReport, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let records = parse_audit_records(&text)?;
    Ok(verify_audit_records_pinned(&records, pinned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sink_captures_records() {
        let sink = AuditSink::memory();
        sink.write(&AuditRecord::pre("s1", "ping"));
        let mut post = AuditRecord::post("s1", "ping");
        post.ok = Some(true);
        sink.write(&post);
        let recs = sink.memory_records();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["phase"], "pre");
        assert_eq!(recs[1]["phase"], "post");
        assert_eq!(recs[1]["ok"], true);
    }

    #[test]
    fn file_sink_appends_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let sink = AuditSink::file(dir.path().to_path_buf(), "sess").unwrap();
        sink.write(&AuditRecord::pre("sess", "a"));
        sink.write(&AuditRecord::pre("sess", "b"));
        let raw = fs::read_to_string(sink.path().unwrap()).unwrap();
        let recs = parse_audit_records(&raw).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].tool, "a");
        assert_eq!(recs[1].tool, "b");
    }

    #[test]
    fn cryptographic_chain_verifies_untouched_ledger() {
        let sink = AuditSink::memory();
        sink.write(&AuditRecord::pre("s1", "login"));
        let mut post1 = AuditRecord::post("s1", "login");
        post1.ok = Some(true);
        sink.write(&post1);

        let mut pre2 = AuditRecord::pre("s1", "read_patient");
        pre2.tier = Some("read".into());
        pre2.args_redacted = Some(serde_json::json!({"patient_id": "<PATIENT_1>"}));
        sink.write(&pre2);

        let jsonl = sink
            .memory_records()
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        let report = verify_audit_log(&jsonl).expect("verification runs");
        assert!(report.valid);
        assert_eq!(report.total_records, 3);
        assert_eq!(report.session_id, "s1");
        assert!(report.error.is_none());
        assert!(!report.root_hash.is_empty());
        assert!(!report.leaf_hash.is_empty());
    }

    #[test]
    fn detects_tampered_payload() {
        let sink = AuditSink::memory();
        sink.write(&AuditRecord::pre("s1", "cmd1"));
        sink.write(&AuditRecord::pre("s1", "cmd2"));

        let mut raw_records = sink.memory_records();
        // Tamper with the tool name in record 1
        raw_records[1]["tool"] = serde_json::json!("malicious_command");

        let tampered_jsonl = raw_records
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        let report = verify_audit_log(&tampered_jsonl).unwrap();
        assert!(!report.valid);
        let err = report.error.expect("error reported");
        assert_eq!(err.seq, 1);
        assert!(err.reason.contains("tampered content hash"));
    }

    #[test]
    fn detects_deleted_record() {
        let sink = AuditSink::memory();
        sink.write(&AuditRecord::pre("s1", "cmd1"));
        sink.write(&AuditRecord::pre("s1", "cmd2"));
        sink.write(&AuditRecord::pre("s1", "cmd3"));

        let mut raw_records = sink.memory_records();
        // Delete record 1 (middle record)
        raw_records.remove(1);

        let tampered_jsonl = raw_records
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        let report = verify_audit_log(&tampered_jsonl).unwrap();
        assert!(!report.valid);
        let err = report.error.expect("error reported");
        assert_eq!(err.seq, 2);
        assert!(err.reason.contains("sequence gap") || err.reason.contains("broken hash chain"));
    }

    #[test]
    fn detects_tampered_role() {
        let sink = AuditSink::memory().with_role("qa");
        let mut pre = AuditRecord::pre("s1", "browser_navigate");
        pre.role = Some("qa".to_string());
        sink.write(&pre);

        let mut raw_records = sink.memory_records();
        assert_eq!(raw_records[0]["role"], "qa");

        // Untampered verifies fine
        let jsonl = raw_records[0].to_string();
        let report = verify_audit_log(&jsonl).unwrap();
        assert!(report.valid);
        assert_eq!(report.role.as_deref(), Some("qa"));

        // Tamper with the role from "qa" to "admin"
        raw_records[0]["role"] = serde_json::json!("admin");
        let tampered_jsonl = raw_records[0].to_string();
        let tampered_report = verify_audit_log(&tampered_jsonl).unwrap();
        assert!(!tampered_report.valid);
        let err = tampered_report.error.expect("error reported");
        assert_eq!(err.seq, 0);
        assert!(err.reason.contains("tampered content hash"));
    }
}
