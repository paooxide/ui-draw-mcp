use crate::audit::{AuditRecord, AuditVerificationReport};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Formats epoch milliseconds as an ISO 8601 UTC timestamp string (`YYYY-MM-DDTHH:MM:SS.mmmZ`).
pub fn format_utc_timestamp(ts_ms: u128) -> String {
    let secs = (ts_ms / 1000) as i64;
    let millis = (ts_ms % 1000) as u32;

    let mut days = secs / 86400;
    let mut rem_secs = secs % 86400;
    if rem_secs < 0 {
        rem_secs += 86400;
        days -= 1;
    }

    let hours = rem_secs / 3600;
    let mins = (rem_secs % 3600) / 60;
    let s = rem_secs % 60;

    let mut year = 1970;
    loop {
        let leap = is_leap_year(year);
        let days_in_year = if leap { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let leap = is_leap_year(year);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for &d in &month_days {
        if days < d {
            break;
        }
        days -= d;
        month += 1;
    }
    let day = days + 1;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hours, mins, s, millis
    )
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

/// A structured HIPAA Access Event record (HIPAA Security Rule § 164.312(b)).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HipaaAccessEvent {
    pub seq: u64,
    pub timestamp_utc: String,
    pub timestamp_ms: u128,
    pub session_id: String,
    pub role: Option<String>,
    pub phase: String,
    pub tool: String,
    pub tier: String,
    pub decision: String,
    pub ephi_accessed: bool,
    pub ephi_tokens: Vec<String>,
    pub args_sanitized: Value,
    pub integrity_hash: String,
    pub signature: String,
}

/// Extracts ePHI/synthetic tokens from a JSON value.
pub fn extract_ephi_tokens(val: &Value) -> Vec<String> {
    let mut tokens = Vec::new();
    collect_tokens_recursive(val, &mut tokens);
    tokens.sort();
    tokens.dedup();
    tokens
}

fn collect_tokens_recursive(val: &Value, tokens: &mut Vec<String>) {
    match val {
        Value::String(s) => {
            // Find all <TAG_n> tokens
            let mut start = 0;
            while let Some(open) = s[start..].find('<') {
                let actual_open = start + open;
                if let Some(close) = s[actual_open..].find('>') {
                    let actual_close = actual_open + close;
                    let candidate = &s[actual_open..=actual_close];
                    if is_ephi_token(candidate) {
                        tokens.push(candidate.to_string());
                    }
                    start = actual_close + 1;
                } else {
                    break;
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_tokens_recursive(item, tokens);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                collect_tokens_recursive(v, tokens);
            }
        }
        _ => {}
    }
}

/// Whether `val` holds an identifier in plaintext. A session run with the
/// tokenizer off logs raw values, not tokens; judging access by tokens alone
/// would report no ePHI for exactly the sessions that exposed the most.
fn contains_raw_identifiers(val: &Value) -> bool {
    let mut probe = val.clone();
    crate::SessionAnonymizer::new().anonymize_value(&mut probe);
    probe != *val
}

fn is_ephi_token(token: &str) -> bool {
    let inner = &token[1..token.len() - 1];
    let prefixes = [
        "PATIENT_",
        "PERSON_",
        "SSN_",
        "CREDIT_CARD_",
        "EMAIL_",
        "PHONE_",
        "MRN_",
        "IPV4_",
        "SECRET_KEY_",
        "PUBLIC_KEY_",
    ];
    prefixes.iter().any(|p| inner.starts_with(p))
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Compliance bundle exporter for HIPAA and SOC2 Type II.
pub struct ComplianceExporter;

impl ComplianceExporter {
    /// Convert an audit record into a HIPAA Access Event.
    pub fn to_hipaa_event(rec: &AuditRecord) -> HipaaAccessEvent {
        let empty_val = json!({});
        let args = rec.args_redacted.as_ref().unwrap_or(&empty_val);
        let tokens = extract_ephi_tokens(args);
        let ephi_accessed = !tokens.is_empty() || contains_raw_identifiers(args);

        HipaaAccessEvent {
            seq: rec.seq,
            timestamp_utc: format_utc_timestamp(rec.ts_ms),
            timestamp_ms: rec.ts_ms,
            session_id: rec.session_id.clone(),
            role: rec.role.clone(),
            phase: rec.phase.clone(),
            tool: rec.tool.clone(),
            tier: rec.tier.clone().unwrap_or_else(|| "unspecified".into()),
            decision: rec.decision.clone().unwrap_or_else(|| "none".into()),
            ephi_accessed,
            ephi_tokens: tokens,
            args_sanitized: args.clone(),
            integrity_hash: rec.hash.clone(),
            signature: rec.sig.clone(),
        }
    }

    /// Export audit records to RFC 4180 compliant CSV (HIPAA § 164.312(b)).
    pub fn to_hipaa_csv(records: &[AuditRecord]) -> String {
        let headers = "Seq,Timestamp_UTC,Timestamp_Ms,Session_ID,Role,Phase,Tool,Tier,Decision,ePHI_Accessed,ePHI_Tokens,Integrity_Hash,Signature";
        let mut lines = Vec::with_capacity(records.len() + 1);
        lines.push(headers.to_string());

        for rec in records {
            let ev = Self::to_hipaa_event(rec);
            let tokens_str = ev.ephi_tokens.join(";");
            let row = format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{}",
                ev.seq,
                csv_escape(&ev.timestamp_utc),
                ev.timestamp_ms,
                csv_escape(&ev.session_id),
                csv_escape(ev.role.as_deref().unwrap_or("none")),
                csv_escape(&ev.phase),
                csv_escape(&ev.tool),
                csv_escape(&ev.tier),
                csv_escape(&ev.decision),
                if ev.ephi_accessed { "true" } else { "false" },
                csv_escape(&tokens_str),
                csv_escape(&ev.integrity_hash),
                csv_escape(&ev.signature),
            );
            lines.push(row);
        }

        lines.join("\n")
    }

    /// Export audit records to structured HIPAA JSON event array.
    pub fn to_hipaa_json(records: &[AuditRecord]) -> Value {
        let events: Vec<HipaaAccessEvent> = records.iter().map(Self::to_hipaa_event).collect();
        json!(events)
    }

    /// Export a verified audit session to a comprehensive SOC2 Operational Audit Report (Markdown).
    pub fn to_soc2_report(
        verification: &AuditVerificationReport,
        records: &[AuditRecord],
    ) -> String {
        let first_ts = records.first().map(|r| r.ts_ms).unwrap_or(0);
        let last_ts = records.last().map(|r| r.ts_ms).unwrap_or(0);

        let total_ops = records.len();
        let pre_ops = records.iter().filter(|r| r.phase == "pre").count();
        let post_ops = records.iter().filter(|r| r.phase == "post").count();
        let dangerous_ops = records
            .iter()
            .filter(|r| r.tier.as_deref() == Some("dangerous"))
            .count();
        let denials = records
            .iter()
            .filter(|r| r.decision.as_deref().is_some_and(|d| d.contains("deny")))
            .count();
        let consents = records
            .iter()
            .filter(|r| r.decision.as_deref().is_some_and(|d| d.contains("consent")))
            .count();

        let ephi_count = records
            .iter()
            .filter(|r| Self::to_hipaa_event(r).ephi_accessed)
            .count();

        let integrity_badge = match (verification.valid, verification.key_pinned) {
            (true, true) => "**VERIFIED (intact, signed by the pinned operator key)**",
            (true, false) => {
                "**SELF-CONSISTENT ONLY (signer not pinned: a rewritten and re-signed log \
                 would also pass; verify with the operator's public key)**"
            }
            (false, _) => "**TAMPERING DETECTED (Ledger integrity compromised)**",
        };

        let mut md = String::new();
        md.push_str("# SOC2 & HIPAA Operational Audit Report\n\n");
        md.push_str(&format!(
            "- **Session ID:** `{}`\n",
            if verification.session_id.is_empty() {
                "unknown"
            } else {
                &verification.session_id
            }
        ));
        md.push_str(&format!(
            "- **Active Role / Profile:** `{}`\n",
            verification.role.as_deref().unwrap_or("unspecified")
        ));
        md.push_str(&format!(
            "- **Generated:** {}\n",
            format_utc_timestamp(crate::audit::now_ms())
        ));
        md.push_str(&format!(
            "- **Audit Time Range:** {} to {}\n",
            format_utc_timestamp(first_ts),
            format_utc_timestamp(last_ts)
        ));
        md.push_str(&format!(
            "- **Cryptographic Status:** {integrity_badge}\n\n"
        ));

        md.push_str("## 1. Cryptographic Ledger Certificate\n\n");
        md.push_str("| Metric | Value |\n| :--- | :--- |\n");
        md.push_str(&format!(
            "| **Ledger Valid** | `{}` |\n",
            verification.valid
        ));
        md.push_str(&format!(
            "| **Total Records** | `{}` |\n",
            verification.total_records
        ));
        md.push_str(&format!(
            "| **Ed25519 Public Key** | `{}` |\n",
            verification.public_key
        ));
        md.push_str(&format!(
            "| **Signer Pinned** | `{}` |\n",
            verification.key_pinned
        ));
        md.push_str(&format!(
            "| **Genesis Root Hash** | `{}` |\n",
            verification.root_hash
        ));
        md.push_str(&format!(
            "| **Ledger Leaf Hash** | `{}` |\n",
            verification.leaf_hash
        ));
        md.push_str("| **Hashing Algorithm** | `SHA-256 (Canonical Payload Chained)` |\n");
        md.push_str("| **Signature Standard** | `Ed25519 (Strict RFC 8032)` |\n\n");

        if let Some(err) = &verification.error {
            md.push_str("### ⚠️ Tamper Detection Details\n\n");
            md.push_str(&format!("- **Failed at Sequence:** `{}`\n", err.seq));
            md.push_str(&format!("- **Reason:** {}\n\n", err.reason));
        }

        md.push_str("## 2. Operational & Security Metrics\n\n");
        md.push_str("| Control Metric | Count |\n| :--- | :--- |\n");
        md.push_str(&format!("| Total Audit Events | `{total_ops}` |\n"));
        md.push_str(&format!("| Inbound Intent (Pre-Calls) | `{pre_ops}` |\n"));
        md.push_str(&format!(
            "| Execution Outcomes (Post-Calls) | `{post_ops}` |\n"
        ));
        md.push_str(&format!(
            "| Dangerous-Tier Operations | `{dangerous_ops}` |\n"
        ));
        md.push_str(&format!("| Human Consent Interventions | `{consents}` |\n"));
        md.push_str(&format!("| Policy Denials Recorded | `{denials}` |\n"));
        md.push_str(&format!("| ePHI Access Events | `{ephi_count}` |\n\n"));

        md.push_str("## 3. Chronological Audit Trail\n\n");
        md.push_str("| Seq | Timestamp (UTC) | Phase | Tool | Tier | Decision | ePHI | Hash |\n");
        md.push_str("| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |\n");

        for rec in records {
            let ev = Self::to_hipaa_event(rec);
            let short_hash = if ev.integrity_hash.len() >= 12 {
                format!("{}...", &ev.integrity_hash[..12])
            } else {
                ev.integrity_hash
            };
            md.push_str(&format!(
                "| `{}` | {} | `{}` | `{}` | `{}` | `{}` | {} | `{}` |\n",
                ev.seq,
                ev.timestamp_utc,
                ev.phase,
                ev.tool,
                ev.tier,
                ev.decision,
                if ev.ephi_accessed { "⚠️ Yes" } else { "No" },
                short_hash
            ));
        }

        md
    }

    /// Export a verified audit session to a structured SOC2 JSON object.
    pub fn to_soc2_json(verification: &AuditVerificationReport, records: &[AuditRecord]) -> Value {
        let first_ts = records.first().map(|r| r.ts_ms).unwrap_or(0);
        let last_ts = records.last().map(|r| r.ts_ms).unwrap_or(0);

        let pre_ops = records.iter().filter(|r| r.phase == "pre").count();
        let post_ops = records.iter().filter(|r| r.phase == "post").count();
        let dangerous_ops = records
            .iter()
            .filter(|r| r.tier.as_deref() == Some("dangerous"))
            .count();
        let denials = records
            .iter()
            .filter(|r| r.decision.as_deref().is_some_and(|d| d.contains("deny")))
            .count();
        let consents = records
            .iter()
            .filter(|r| r.decision.as_deref().is_some_and(|d| d.contains("consent")))
            .count();

        let mut ephi_count = 0;
        let mut events = Vec::new();
        for r in records {
            let ev = Self::to_hipaa_event(r);
            if ev.ephi_accessed {
                ephi_count += 1;
            }
            events.push(ev);
        }

        json!({
            "compliance_type": "SOC2_Type_II",
            "report_generated_utc": format_utc_timestamp(crate::audit::now_ms()),
            "session_id": verification.session_id,
            "role": verification.role.clone(),
            "period": {
                "start_utc": format_utc_timestamp(first_ts),
                "end_utc": format_utc_timestamp(last_ts),
                "start_ms": first_ts,
                "end_ms": last_ts
            },
            "verification": verification,
            "metrics": {
                "total_events": records.len(),
                "pre_calls": pre_ops,
                "post_calls": post_ops,
                "dangerous_tier_calls": dangerous_ops,
                "human_consent_prompts": consents,
                "policy_denials": denials,
                "ephi_access_events": ephi_count
            },
            "ledger": events
        })
    }
}
