use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

/// One audit line. Two are written per call: a `pre` record (tool, redacted
/// args, tier, decision) and a `post` record (ok, error code, latency).
#[derive(Debug, Clone, Serialize)]
pub struct AuditRecord {
    pub phase: &'static str, // "pre" | "post"
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
}

impl AuditRecord {
    pub fn pre(session_id: &str, tool: &str) -> Self {
        AuditRecord {
            phase: "pre",
            ts_ms: now_ms(),
            session_id: session_id.to_string(),
            tool: tool.to_string(),
            tier: None,
            decision: None,
            args_redacted: None,
            ok: None,
            error_code: None,
            latency_ms: None,
        }
    }

    pub fn post(session_id: &str, tool: &str) -> Self {
        AuditRecord {
            phase: "post",
            ..AuditRecord::pre(session_id, tool)
        }
    }
}

/// Append-only audit sink. Production writes JSONL to a per-session file behind a
/// single lock (serialized writes → no interleaving). Tests use an in-memory
/// sink to assert records without touching disk.
pub struct AuditSink {
    inner: Mutex<Inner>,
}

enum Inner {
    File(PathBuf),
    Memory(Vec<Value>),
}

impl AuditSink {
    /// Create a file-backed sink at `<dir>/<session_id>.jsonl`.
    pub fn file(dir: PathBuf, session_id: &str) -> std::io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{session_id}.jsonl"));
        Ok(AuditSink {
            inner: Mutex::new(Inner::File(path)),
        })
    }

    /// Create an in-memory sink (tests).
    pub fn memory() -> Self {
        AuditSink {
            inner: Mutex::new(Inner::Memory(Vec::new())),
        }
    }

    /// Write one record. Best-effort: an audit write failure is logged but never
    /// crashes a call.
    pub fn write(&self, record: &AuditRecord) {
        let value = match serde_json::to_value(record) {
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

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Read and parse a JSONL audit file (test/inspection helper).
#[cfg(test)]
fn read_jsonl(path: &std::path::Path) -> std::io::Result<Vec<Value>> {
    let text = fs::read_to_string(path)?;
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
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
        let recs = read_jsonl(&sink.path().unwrap()).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["tool"], "a");
        assert_eq!(recs[1]["tool"], "b");
    }
}
