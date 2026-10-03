use mcp_policy::{
    extract_ephi_tokens, format_utc_timestamp, parse_audit_records, verify_audit_file,
    verify_audit_log, verify_audit_records, AuditRecord, AuditSink, ComplianceExporter,
};
use serde_json::json;

#[test]
fn test_untouched_cryptographic_ledger_verifies() {
    let sink = AuditSink::memory();
    sink.write(&AuditRecord::pre("sess_100", "ping"));
    let mut post1 = AuditRecord::post("sess_100", "ping");
    post1.ok = Some(true);
    sink.write(&post1);

    let mut pre2 = AuditRecord::pre("sess_100", "browser_navigate");
    pre2.tier = Some("dangerous".into());
    pre2.args_redacted = Some(json!({"url": "https://records.example.com"}));
    sink.write(&pre2);

    let mut post2 = AuditRecord::post("sess_100", "browser_navigate");
    post2.ok = Some(true);
    post2.latency_ms = Some(145);
    sink.write(&post2);

    let jsonl = sink
        .memory_records()
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let records = parse_audit_records(&jsonl).expect("valid jsonl");
    assert_eq!(records.len(), 4);

    let report = verify_audit_records(&records);
    assert!(report.valid, "untouched ledger must verify successfully");
    assert_eq!(report.session_id, "sess_100");
    assert_eq!(report.total_records, 4);
    assert!(!report.public_key.is_empty());
    assert!(!report.root_hash.is_empty());
    assert!(!report.leaf_hash.is_empty());
    assert!(report.error.is_none());
}

#[test]
fn test_tamper_detection_modified_payload() {
    let sink = AuditSink::memory();
    sink.write(&AuditRecord::pre("sess_tamper", "sys_logs"));
    let mut post = AuditRecord::post("sess_tamper", "sys_logs");
    post.ok = Some(true);
    sink.write(&post);

    let mut raw_records = sink.memory_records();
    // Attacker modifies tool name from 'sys_logs' to 'format_disk'
    raw_records[0]["tool"] = json!("format_disk");

    let tampered_jsonl = raw_records
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let report = verify_audit_log(&tampered_jsonl).expect("parses jsonl");
    assert!(!report.valid, "tampered payload must fail verification");
    let err = report.error.expect("tamper error captured");
    assert_eq!(err.seq, 0);
    assert!(err.reason.contains("tampered content hash"));
}

#[test]
fn test_tamper_detection_modified_timestamp() {
    let sink = AuditSink::memory();
    sink.write(&AuditRecord::pre("sess_ts", "find_elements"));
    let mut post = AuditRecord::post("sess_ts", "find_elements");
    post.ok = Some(true);
    sink.write(&post);

    let mut raw_records = sink.memory_records();
    // Tamper with timestamp
    raw_records[1]["ts_ms"] = json!(9999999999999u64);

    let tampered_jsonl = raw_records
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let report = verify_audit_log(&tampered_jsonl).expect("parses jsonl");
    assert!(!report.valid);
    let err = report.error.expect("tamper error");
    assert_eq!(err.seq, 1);
    assert!(err.reason.contains("tampered content hash"));
}

#[test]
fn test_tamper_detection_reordered_records() {
    let sink = AuditSink::memory();
    sink.write(&AuditRecord::pre("sess_reorder", "op1"));
    sink.write(&AuditRecord::pre("sess_reorder", "op2"));
    sink.write(&AuditRecord::pre("sess_reorder", "op3"));

    let mut raw_records = sink.memory_records();
    // Swap records 1 and 2
    raw_records.swap(1, 2);

    let tampered_jsonl = raw_records
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let report = verify_audit_log(&tampered_jsonl).expect("parses");
    assert!(!report.valid);
    let err = report.error.expect("tamper error");
    assert_eq!(err.seq, 2);
    assert!(err.reason.contains("sequence gap") || err.reason.contains("broken hash chain"));
}

#[test]
fn test_tamper_detection_deleted_middle_record() {
    let sink = AuditSink::memory();
    sink.write(&AuditRecord::pre("sess_del", "step1"));
    sink.write(&AuditRecord::pre("sess_del", "step2_sensitive"));
    sink.write(&AuditRecord::pre("sess_del", "step3"));

    let mut raw_records = sink.memory_records();
    // Delete step 2
    raw_records.remove(1);

    let tampered_jsonl = raw_records
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let report = verify_audit_log(&tampered_jsonl).expect("parses");
    assert!(!report.valid);
    let err = report.error.expect("tamper error");
    assert_eq!(err.seq, 2);
    assert!(err.reason.contains("sequence gap") || err.reason.contains("broken hash chain"));
}

#[test]
fn test_tamper_detection_corrupted_signature() {
    let sink = AuditSink::memory();
    sink.write(&AuditRecord::pre("sess_sig", "cmd"));

    let mut raw_records = sink.memory_records();
    // Invert the last character of the hex signature
    let orig_sig = raw_records[0]["sig"].as_str().unwrap();
    let mut corrupted_sig = orig_sig.to_string();
    corrupted_sig.pop();
    corrupted_sig.push(if orig_sig.ends_with('0') { '1' } else { '0' });
    raw_records[0]["sig"] = json!(corrupted_sig);

    let tampered_jsonl = raw_records
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    let report = verify_audit_log(&tampered_jsonl).expect("parses");
    assert!(!report.valid);
    let err = report.error.expect("signature failure");
    assert_eq!(err.seq, 0);
    assert!(err.reason.contains("signature verification failed"));
}

#[test]
fn test_file_sink_verification_and_export_bundle() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let sink = AuditSink::file(tmp.path().to_path_buf(), "sess_e2e").expect("file sink");

    let mut pre1 = AuditRecord::pre("sess_e2e", "keyboard_type");
    pre1.tier = Some("dangerous".into());
    pre1.decision = Some("allow (consent dialog approved)".into());
    pre1.args_redacted = Some(json!({
        "patient": "<PATIENT_1>",
        "ssn": "<SSN_1>",
        "key": "<SECRET_KEY_1>"
    }));
    sink.write(&pre1);

    let mut post1 = AuditRecord::post("sess_e2e", "keyboard_type");
    post1.ok = Some(true);
    post1.latency_ms = Some(42);
    sink.write(&post1);

    let mut pre2 = AuditRecord::pre("sess_e2e", "rm_rf");
    pre2.tier = Some("dangerous".into());
    pre2.decision = Some("deny (destructive shell command)".into());
    sink.write(&pre2);

    let audit_file = tmp.path().join("sess_e2e.jsonl");
    assert!(audit_file.exists());

    // 1. Verify file on disk
    let report = verify_audit_file(&audit_file).expect("file verification runs");
    assert!(report.valid);
    assert_eq!(report.total_records, 3);
    assert_eq!(report.session_id, "sess_e2e");

    let raw = std::fs::read_to_string(&audit_file).expect("read");
    let records = parse_audit_records(&raw).expect("parsed");

    // 2. Test HIPAA CSV export (RFC 4180)
    let csv = ComplianceExporter::to_hipaa_csv(&records);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 4); // 1 header + 3 records
    assert!(lines[0].starts_with("Seq,Timestamp_UTC,Timestamp_Ms"));
    // First record has ePHI
    assert!(lines[1].contains("true"));
    assert!(lines[1].contains("<PATIENT_1>;<SECRET_KEY_1>;<SSN_1>"));
    // Last record (rm_rf) has no ePHI
    assert!(lines[3].contains("false"));

    // 3. Test HIPAA JSON export
    let hipaa_json = ComplianceExporter::to_hipaa_json(&records);
    let arr = hipaa_json.as_array().expect("json array");
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["ephi_accessed"], true);
    let tokens = arr[0]["ephi_tokens"].as_array().expect("token array");
    assert_eq!(tokens.len(), 3);

    // 4. Test SOC2 Markdown Report
    let md = ComplianceExporter::to_soc2_report(&report, &records);
    assert!(md.contains("# SOC2 & HIPAA Operational Audit Report"));
    // The signer was not pinned, so the report claims self-consistency only.
    assert!(md.contains("SELF-CONSISTENT ONLY"));
    assert!(md.contains("| **Ledger Valid** | `true` |"));
    assert!(md.contains("| Total Audit Events | `3` |"));
    assert!(md.contains("| Dangerous-Tier Operations | `2` |"));
    assert!(md.contains("| Policy Denials Recorded | `1` |"));
    assert!(md.contains("| ePHI Access Events | `1` |"));

    // 5. Test SOC2 Structured JSON bundle
    let soc2_json = ComplianceExporter::to_soc2_json(&report, &records);
    assert_eq!(soc2_json["compliance_type"], "SOC2_Type_II");
    assert_eq!(soc2_json["verification"]["valid"], true);
    assert_eq!(soc2_json["metrics"]["total_events"], 3);
    assert_eq!(soc2_json["metrics"]["policy_denials"], 1);
    assert_eq!(soc2_json["metrics"]["ephi_access_events"], 1);
}

#[test]
fn test_timestamp_and_ephi_token_extractors() {
    let ts_str = format_utc_timestamp(1774886400000); // 2026-03-30T16:00:00.000Z
    assert!(ts_str.starts_with("2026-03-30T16:00:00"));
    assert!(ts_str.ends_with(".000Z"));

    let val = json!({
        "payload": {
            "msg": "Found patient <PATIENT_1> with ssn <SSN_2> and phone <PHONE_1>",
            "other": ["benign text", "<MRN_99>", "<UNKNOWN_TAG>"]
        }
    });
    let tokens = extract_ephi_tokens(&val);
    assert_eq!(
        tokens,
        vec!["<MRN_99>", "<PATIENT_1>", "<PHONE_1>", "<SSN_2>"]
    );
}

// ---- signer pinning ---------------------------------------------------------

fn write_log(sink: &AuditSink) {
    sink.write(&AuditRecord::pre("s", "fs_delete"));
    let mut post = AuditRecord::post("s", "fs_delete");
    post.ok = Some(true);
    sink.write(&post);
}

fn read_log(path: &std::path::Path) -> Vec<AuditRecord> {
    parse_audit_records(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The attack pinning exists for: rewrite the log, re-sign every record with a
/// fresh key, embed that key in record 0. Unpinned, it verifies.
#[test]
fn rewritten_and_resigned_log_passes_unpinned_but_fails_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("audit.key");
    let pub_hex = mcp_policy::generate_signing_key_file(&key_path).unwrap();
    let pinned = mcp_policy::verifying_key_from_hex(&pub_hex).unwrap();
    let key = mcp_policy::load_signing_key(&key_path).unwrap();

    let real = AuditSink::file_with_key(dir.path().join("real"), "s", Some(key)).unwrap();
    write_log(&real);
    let records = read_log(&real.path().unwrap());
    let ok = mcp_policy::verify_audit_records_pinned(&records, Some(&pinned));
    assert!(ok.valid && ok.key_pinned, "{:?}", ok.error);

    // The forger replays the same events, now hiding the deletion, and signs
    // with a key of their own.
    let forged = AuditSink::memory();
    forged.write(&AuditRecord::pre("s", "fs_list"));
    let forged: Vec<AuditRecord> = forged
        .memory_records()
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
    let unpinned = verify_audit_records(&forged);
    assert!(unpinned.valid && !unpinned.key_pinned);
    let pinned_report = mcp_policy::verify_audit_records_pinned(&forged, Some(&pinned));
    assert!(!pinned_report.valid);
    assert!(pinned_report.error.unwrap().reason.contains("pinned key"));
}

#[test]
fn signing_key_file_is_private_and_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("audit.key");
    mcp_policy::generate_signing_key_file(&key_path).unwrap();
    assert!(mcp_policy::generate_signing_key_file(&key_path).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = mcp_policy::load_signing_key(&key_path).unwrap_err();
        assert!(err.contains("chmod 600"), "{err}");
    }
}

#[test]
fn malformed_pinned_key_is_an_error() {
    assert!(mcp_policy::verifying_key_from_hex("zz").is_err());
    assert!(mcp_policy::verifying_key_from_hex("abcd").is_err());
}

/// An unpinned report must not claim more than self-consistency.
#[test]
fn soc2_badge_distinguishes_pinned_from_self_consistent() {
    let sink = AuditSink::memory();
    write_log(&sink);
    let records: Vec<AuditRecord> = sink
        .memory_records()
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect();
    let md = ComplianceExporter::to_soc2_report(&verify_audit_records(&records), &records);
    assert!(md.contains("SELF-CONSISTENT ONLY"));
    assert!(!md.contains("VERIFIED"));
}

/// With the tokenizer off the log holds raw values; they are still ePHI.
#[test]
fn raw_identifiers_count_as_ephi_access() {
    let mut rec = AuditRecord::pre("s", "keyboard_type");
    rec.args_redacted = Some(json!({ "text": "SSN 123-45-6789" }));
    assert!(ComplianceExporter::to_hipaa_event(&rec).ephi_accessed);

    let mut clean = AuditRecord::pre("s", "keyboard_type");
    clean.args_redacted = Some(json!({ "text": "hello" }));
    assert!(!ComplianceExporter::to_hipaa_event(&clean).ephi_accessed);
}
