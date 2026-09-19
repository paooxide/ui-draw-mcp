//! Typed judgments from a System One model (TypeSafe's `jev`).
//!
//! The server already does the deterministic part of every decision: the
//! allowlists, the tiers, the destructive patterns, the element index. What
//! is left is the semantic residue, "which of these forty buttons is the one
//! that saves", "is this page text talking to the model", "would this line
//! wipe a disk if the pattern list missed it". A System One model answers a
//! question like that in about a hundred milliseconds with a probability,
//! not prose, which is something code can consume.
//!
//! Two rules make it safe to have in a security kernel:
//!
//! 1. **A judgment may only tighten.** It can add a denial, escalate to
//!    consent, or rank candidates for the agent. Nothing here is consulted on
//!    an allow path, so a wrong, absent or manipulated answer can only make
//!    the server more careful, never less. The model's own documentation
//!    says adversarial content can move its answers; this is why that does
//!    not matter.
//! 2. **It degrades, it never blocks.** No key, no network, a 5xx, a timeout:
//!    the caller gets an error naming the cause, counts it, and proceeds on
//!    the deterministic answer alone. The one thing that fails fast is a
//!    config that cannot be right (a bad URL, a threshold outside 0..1),
//!    because that can never start working.
//!
//! Off by default. Everything sent leaves the machine: element names, page
//! text, typed commands. The caller sends state after redaction and never a
//! secure field's value.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
pub const DEFAULT_MODEL: &str = "jev-latest";
/// The API's documented ceiling on state; we stay well under it.
pub const DEFAULT_MAX_STATE_BYTES: usize = 48_000;
/// Choice questions take at most this many options.
pub const MAX_CHOICE_OPTIONS: usize = 255;
pub const ENV_KEY: &str = "TYPESAFE_API_KEY";
pub const KEY_FILE: &str = "typesafe.key";

/// Operator settings, from `[judge]` in `config.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub timeout_ms: u64,
    /// A Noul at or above this counts as "yes" wherever a yes tightens. It is
    /// the fallback for every use below that has no bar of its own.
    pub threshold: f64,
    /// The bar for the destructive-command second opinion. A yes here escalates
    /// a command to consent or a denial, so a lower value is *more* cautious.
    /// `None` falls back to [`threshold`].
    pub destructive_threshold: Option<f64>,
    /// The bar for judging that an untrusted result is addressed to a model
    /// (prompt injection). A yes flags the content. `None` falls back to
    /// [`threshold`].
    pub injection_threshold: Option<f64>,
    /// The bar a ranking's `any_fits` must clear for a semantic pick (an
    /// element, a dialog button, a recalled recipe) to count as a real match
    /// rather than a forced choice among poor options. `None` falls back to
    /// [`threshold`].
    pub match_threshold: Option<f64>,
    pub max_state_bytes: usize,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        JudgeConfig {
            enabled: false,
            base_url: DEFAULT_BASE_URL.into(),
            model: DEFAULT_MODEL.into(),
            timeout_ms: 4_000,
            threshold: 0.7,
            destructive_threshold: None,
            injection_threshold: None,
            match_threshold: None,
            max_state_bytes: DEFAULT_MAX_STATE_BYTES,
        }
    }
}

impl JudgeConfig {
    /// A config that can never work is rejected up front, at load time.
    pub fn validate(&self) -> Result<(), String> {
        if !self.base_url.starts_with("https://") {
            return Err(format!(
                "judge.base_url must start with https:// (got '{}')",
                self.base_url
            ));
        }
        if self.model.trim().is_empty() {
            return Err("judge.model must not be empty".into());
        }
        check_threshold("judge.threshold", Some(self.threshold))?;
        check_threshold("judge.destructive_threshold", self.destructive_threshold)?;
        check_threshold("judge.injection_threshold", self.injection_threshold)?;
        check_threshold("judge.match_threshold", self.match_threshold)?;
        if self.timeout_ms == 0 {
            return Err("judge.timeout_ms must be positive".into());
        }
        if self.max_state_bytes < 1_000 {
            return Err("judge.max_state_bytes must be at least 1000".into());
        }
        Ok(())
    }

    /// The bar for the destructive second opinion, falling back to the general
    /// threshold when it has no override.
    pub fn destructive_threshold(&self) -> f64 {
        self.destructive_threshold.unwrap_or(self.threshold)
    }

    /// The bar for the injection second opinion, falling back to the general
    /// threshold.
    pub fn injection_threshold(&self) -> f64 {
        self.injection_threshold.unwrap_or(self.threshold)
    }

    /// The bar a ranking's `any_fits` must clear, falling back to the general
    /// threshold.
    pub fn match_threshold(&self) -> f64 {
        self.match_threshold.unwrap_or(self.threshold)
    }
}

/// A threshold, when set, must be a real number within 0 and 1: outside that
/// it can only silently disable or always-fire the comparison it controls.
fn check_threshold(name: &str, value: Option<f64>) -> Result<(), String> {
    if let Some(t) = value {
        if !(0.0..=1.0).contains(&t) || t.is_nan() {
            return Err(format!("{name} must be within 0 and 1 (got {t})"));
        }
    }
    Ok(())
}

/// One question. Ids are for code; the meaning lives in the instructions.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

/// One answer, as the API returns it.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Answers {
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: Usage,
}

/// Why no judgment came back. Every variant is a reason to proceed on the
/// deterministic answer, and none is a reason to allow anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeError {
    /// `[judge] enabled = false`, or the judge was never wired.
    Disabled,
    /// Enabled, but no key was found where keys are looked for.
    NoKey(String),
    /// The request was refused before it was sent (empty candidates, etc.).
    BadRequest(String),
    /// Could not reach the service, or it did not answer in time.
    Unreachable(String),
    /// The service answered with a non-success status.
    Status(u16, String),
    /// The body did not parse as the documented shape.
    Malformed(String),
}

impl JudgeError {
    pub fn message(&self) -> String {
        match self {
            JudgeError::Disabled => "the judge is disabled ([judge] enabled = false)".into(),
            JudgeError::NoKey(m) => format!("the judge is enabled but has no API key: {m}"),
            JudgeError::BadRequest(m) => format!("judge request refused: {m}"),
            JudgeError::Unreachable(m) => format!("judge unreachable: {m}"),
            JudgeError::Status(401, _) => "judge: the API key was rejected (401)".into(),
            JudgeError::Status(422, body) => {
                format!("judge: request rejected (422): {}", short(body))
            }
            JudgeError::Status(429, _) => "judge: rate limited (429)".into(),
            JudgeError::Status(529, _) => "judge: service overloaded (529)".into(),
            JudgeError::Status(code, body) => format!("judge: HTTP {code}: {}", short(body)),
            JudgeError::Malformed(m) => format!("judge: unexpected response: {m}"),
        }
    }
}

fn short(s: &str) -> String {
    s.chars().take(200).collect()
}

/// A raw HTTP exchange with the service. The real one is `curl`; a test
/// hands in a recorded one.
#[async_trait]
pub trait Transport: Send + Sync {
    /// POST `body` as JSON to `url` with a bearer `key`. Returns the status
    /// and the body text, or why the exchange did not complete.
    async fn post(
        &self,
        url: &str,
        key: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<(u16, String), String>;
}

/// `curl` on a config file read from stdin, so the key is never in argv.
pub struct CurlTransport;

fn curl_bin() -> &'static str {
    if cfg!(unix) && Path::new("/usr/bin/curl").exists() {
        "/usr/bin/curl"
    } else {
        "curl"
    }
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Render the config curl reads from stdin. The status code is appended on
/// its own line so an error body is kept.
pub fn curl_config(url: &str, key: &str, body_path: &Path, timeout: Duration) -> String {
    [
        "silent".to_string(),
        "show-error".to_string(),
        format!("max-time = {}", timeout.as_secs().max(1)),
        format!("url = {}", quote(url)),
        format!(
            "header = {}",
            quote(&format!("Authorization: Bearer {key}"))
        ),
        "header = \"Content-Type: application/json\"".to_string(),
        "write-out = \"\\n%{http_code}\"".to_string(),
        format!(
            "data = {}",
            quote(&format!("@{}", body_path.to_string_lossy()))
        ),
    ]
    .join("\n")
        + "\n"
}

/// Split curl's output into the body and the status the template appended.
pub fn split_status(out: &str) -> (u16, &str) {
    match out.rfind('\n') {
        Some(i) => (out[i + 1..].trim().parse().unwrap_or(0), &out[..i]),
        None => (0, out),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    f.write_all(bytes)
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

struct BodyFile(PathBuf);
impl Drop for BodyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[async_trait]
impl Transport for CurlTransport {
    async fn post(
        &self,
        url: &str,
        key: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<(u16, String), String> {
        use tokio::io::AsyncWriteExt;
        let seq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("agentctl-judge-{}-{seq}.json", std::process::id()));
        write_private(
            &path,
            serde_json::to_string(body).unwrap_or_default().as_bytes(),
        )?;
        let body_file = BodyFile(path);
        let config = curl_config(url, key, &body_file.0, timeout);
        let mut child = tokio::process::Command::new(curl_bin())
            .args(["--config", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start curl: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(config.as_bytes())
                .await
                .map_err(|e| format!("curl stdin: {e}"))?;
        }
        let out = tokio::time::timeout(timeout + Duration::from_secs(2), child.wait_with_output())
            .await
            .map_err(|_| "curl did not finish in time".to_string())?
            .map_err(|e| format!("curl: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let (status, body) = split_status(&text);
        if status == 0 {
            return Err(format!(
                "no HTTP status (curl exit {}): {}",
                out.status.code().unwrap_or(-1),
                short(&String::from_utf8_lossy(&out.stderr))
            ));
        }
        Ok((status, body.to_string()))
    }
}

/// `KEY=value` lines, quotes stripped, `export` tolerated.
pub fn parse_dotenv(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value)
            .trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(unix)]
fn check_private(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("could not stat {}: {e}", path.display()))?
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is readable by other users (mode {:o}); run chmod 600 {}",
            path.display(),
            mode & 0o777,
            path.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private(_path: &Path) -> Result<(), String> {
    Ok(())
}

/// Where the key comes from, in order: the environment, `.env` (or
/// `$AGENTCTL_ENV`), then `<state>/typesafe.key`. Never an argument.
pub fn api_key(state_dir: &Path) -> Result<String, String> {
    if let Ok(k) = std::env::var(ENV_KEY) {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return Ok(k);
        }
    }
    let dotenv = std::env::var("AGENTCTL_ENV")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".env"));
    if dotenv.exists() {
        check_private(&dotenv)?;
        let text = std::fs::read_to_string(&dotenv)
            .map_err(|e| format!("could not read {}: {e}", dotenv.display()))?;
        if let Some(k) = parse_dotenv(&text, ENV_KEY) {
            return Ok(k);
        }
    }
    let key_file = state_dir.join(KEY_FILE);
    if key_file.exists() {
        check_private(&key_file)?;
        let k = std::fs::read_to_string(&key_file)
            .map_err(|e| format!("could not read {}: {e}", key_file.display()))?
            .trim()
            .to_string();
        if !k.is_empty() {
            return Ok(k);
        }
    }
    Err(format!(
        "set {ENV_KEY}, or put it in ./.env, or in {} (chmod 600)",
        key_file.display()
    ))
}

/// Counters for the degraded modes, so an operator can see the judge is
/// silently absent rather than silently agreeing.
#[derive(Debug, Default)]
pub struct Counters {
    pub asked: AtomicU64,
    pub answered: AtomicU64,
    pub failed: AtomicU64,
}

/// The judge. Cheap to clone through an `Arc`; safe to share.
pub struct Judge {
    cfg: JudgeConfig,
    key: Result<String, String>,
    transport: Box<dyn Transport>,
    pub counters: Counters,
}

impl std::fmt::Debug for Judge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("Judge")
            .field("enabled", &self.cfg.enabled)
            .field("model", &self.cfg.model)
            .field("has_key", &self.key.is_ok())
            .finish()
    }
}

impl Judge {
    /// Build from config, looking the key up under `state_dir`. A missing
    /// key is loud (one ERROR line) and non-fatal.
    pub fn from_config(cfg: JudgeConfig, state_dir: &Path) -> Arc<Judge> {
        let key = if cfg.enabled {
            let k = api_key(state_dir);
            if let Err(e) = &k {
                tracing::error!(
                    variable = ENV_KEY,
                    "judge is enabled but has no API key; every judgment will be skipped: {e}"
                );
            }
            k
        } else {
            Err("disabled".into())
        };
        Arc::new(Judge {
            cfg,
            key,
            transport: Box::new(CurlTransport),
            counters: Counters::default(),
        })
    }

    /// Build with an explicit key and transport (tests, and embedding).
    pub fn with_transport(
        cfg: JudgeConfig,
        key: Option<String>,
        transport: Box<dyn Transport>,
    ) -> Judge {
        Judge {
            cfg,
            key: key.ok_or_else(|| "no key".to_string()),
            transport,
            counters: Counters::default(),
        }
    }

    pub fn config(&self) -> &JudgeConfig {
        &self.cfg
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn has_key(&self) -> bool {
        self.key.is_ok()
    }

    /// Whether a judgment can be attempted at all right now.
    pub fn available(&self) -> bool {
        self.cfg.enabled && self.key.is_ok()
    }

    pub fn threshold(&self) -> f64 {
        self.cfg.threshold
    }

    /// The bar for the destructive second opinion (see [`JudgeConfig`]).
    pub fn destructive_threshold(&self) -> f64 {
        self.cfg.destructive_threshold()
    }

    /// The bar for the injection second opinion.
    pub fn injection_threshold(&self) -> f64 {
        self.cfg.injection_threshold()
    }

    /// The bar a ranking's `any_fits` must clear to count as a real match.
    pub fn match_threshold(&self) -> f64 {
        self.cfg.match_threshold()
    }

    /// Cut a text down to the state budget, keeping the head, and say so.
    pub fn fit(&self, text: &str) -> String {
        truncate_utf8(text, self.cfg.max_state_bytes)
    }

    /// Ask a batch of questions over one state. Independent questions go
    /// together: they run in parallel on the service and cost one round trip.
    pub async fn ask(
        &self,
        state: Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Answers, JudgeError> {
        if !self.cfg.enabled {
            return Err(JudgeError::Disabled);
        }
        let key = match &self.key {
            Ok(k) => k.clone(),
            Err(e) => return Err(JudgeError::NoKey(e.clone())),
        };
        if questions.is_empty() {
            return Err(JudgeError::BadRequest("no questions".into()));
        }
        for (id, q) in &questions {
            if let Question::Choice { criteria, .. } = q {
                if criteria.is_empty() {
                    return Err(JudgeError::BadRequest(format!(
                        "choice '{id}' has no options"
                    )));
                }
                if criteria.len() > MAX_CHOICE_OPTIONS {
                    return Err(JudgeError::BadRequest(format!(
                        "choice '{id}' has {} options; the limit is {MAX_CHOICE_OPTIONS}",
                        criteria.len()
                    )));
                }
            }
        }
        let body = json!({
            "state": state,
            "model": self.cfg.model,
            "questions": questions,
        });
        let url = format!("{}/v1/systemone", self.cfg.base_url.trim_end_matches('/'));
        let timeout = Duration::from_millis(self.cfg.timeout_ms);
        self.counters.asked.fetch_add(1, Ordering::Relaxed);
        // One retry on the two statuses the docs say to back off on.
        let mut attempt = 0;
        let result = loop {
            attempt += 1;
            let r = self.transport.post(&url, &key, &body, timeout).await;
            match r {
                Ok((429 | 529, body)) if attempt == 1 => {
                    tracing::debug!("judge asked to back off; retrying once");
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    let _ = body;
                    continue;
                }
                other => break other,
            }
        };
        let outcome = match result {
            Err(e) => Err(JudgeError::Unreachable(e)),
            Ok((200, body)) => serde_json::from_str::<Answers>(&body)
                .map_err(|e| JudgeError::Malformed(format!("{e}: {}", short(&body)))),
            Ok((code, body)) => Err(JudgeError::Status(code, body)),
        };
        match &outcome {
            Ok(_) => {
                self.counters.answered.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                self.counters.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %e.message(), "judgment skipped");
            }
        }
        outcome
    }

    /// A well-formed reply that does not answer the question asked is still
    /// a judgment not delivered, and is counted as one.
    fn malformed(&self, what: String) -> JudgeError {
        self.counters.failed.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(error = %what, "judgment skipped");
        JudgeError::Malformed(what)
    }

    /// One yes/no question. The probability of yes.
    pub async fn noul(
        &self,
        state: Value,
        instructions: &str,
        yes: &str,
        no: &str,
    ) -> Result<f64, JudgeError> {
        let mut qs = BTreeMap::new();
        qs.insert(
            "q".to_string(),
            Question::Noul {
                instructions: instructions.into(),
                criteria: Some(NoulCriteria {
                    yes: yes.into(),
                    no: no.into(),
                }),
            },
        );
        let a = self.ask(state, qs).await?;
        match a.answers.get("q") {
            Some(Answer::Noul { noul }) => Ok(noul.clamp(0.0, 1.0)),
            other => Err(self.malformed(format!("expected a noul answer, got {other:?}"))),
        }
    }

    /// Rank candidates against a request: a Choice over them, plus a Noul
    /// asking whether any of them fits at all, so "none of these" is an
    /// answer rather than a forced pick.
    pub async fn rank(
        &self,
        state: Value,
        instructions: &str,
        candidates: &BTreeMap<String, String>,
    ) -> Result<Ranking, JudgeError> {
        if candidates.is_empty() {
            return Err(JudgeError::BadRequest("no candidates to rank".into()));
        }
        let mut qs = BTreeMap::new();
        qs.insert(
            "pick".to_string(),
            Question::Choice {
                instructions: instructions.into(),
                criteria: candidates.clone(),
            },
        );
        qs.insert(
            "any".to_string(),
            Question::Noul {
                instructions: "Does at least one of the candidate elements listed in `candidates` match what `request` asks for?".into(),
                criteria: Some(NoulCriteria {
                    yes: "One of the candidates is the element the request describes".into(),
                    no: "None of the candidates is what the request describes".into(),
                }),
            },
        );
        let a = self.ask(state, qs).await?;
        let (choice, probabilities, confidence) = match a.answers.get("pick") {
            Some(Answer::Choice {
                choice,
                probabilities,
                confidence,
            }) => (choice.clone(), probabilities.clone(), *confidence),
            other => return Err(self.malformed(format!("expected a choice answer, got {other:?}"))),
        };
        let any = match a.answers.get("any") {
            Some(Answer::Noul { noul }) => noul.clamp(0.0, 1.0),
            _ => 1.0,
        };
        Ok(Ranking {
            choice,
            probabilities,
            confidence: confidence.clamp(0.0, 1.0),
            any_fits: any,
        })
    }
}

/// A ranked pick.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranking {
    pub choice: String,
    pub probabilities: BTreeMap<String, f64>,
    pub confidence: f64,
    /// Probability that any candidate fits at all.
    pub any_fits: f64,
}

/// Cut on a character boundary, appending a marker when something was lost.
pub fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let marker = "\n[truncated]";
    let budget = max_bytes.saturating_sub(marker.len());
    let mut cut = budget.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{marker}", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A recorded exchange: what was sent, and a scripted reply.
    struct Scripted {
        replies: Mutex<Vec<Result<(u16, String), String>>>,
        sent: Mutex<Vec<(String, String, Value)>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<(u16, String), String>>) -> Arc<Self> {
            Arc::new(Scripted {
                replies: Mutex::new(replies),
                sent: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Transport for Arc<Scripted> {
        async fn post(
            &self,
            url: &str,
            key: &str,
            body: &Value,
            _t: Duration,
        ) -> Result<(u16, String), String> {
            self.sent
                .lock()
                .unwrap()
                .push((url.into(), key.into(), body.clone()));
            let mut r = self.replies.lock().unwrap();
            if r.is_empty() {
                return Err("script exhausted".into());
            }
            r.remove(0)
        }
    }

    fn judge(replies: Vec<Result<(u16, String), String>>) -> (Judge, Arc<Scripted>) {
        let s = Scripted::new(replies);
        let cfg = JudgeConfig {
            enabled: true,
            ..JudgeConfig::default()
        };
        (
            Judge::with_transport(cfg, Some("k-test".into()), Box::new(s.clone())),
            s,
        )
    }

    const CHOICE_REPLY: &str = r#"{"model":"jev-latest","answers":{"pick":{"type":"choice","choice":"@e3","probabilities":{"@e3":0.85,"@e7":0.15},"confidence":0.82},"any":{"type":"noul","noul":0.9}},"usage":{"input_tokens":312,"output_tokens":48}}"#;

    // ---- happy path -----------------------------------------------------

    #[tokio::test]
    async fn a_noul_round_trips_with_the_documented_body_shape() {
        let (j, s) = judge(vec![Ok((200, r#"{"model":"jev-latest","answers":{"q":{"type":"noul","noul":0.92}},"usage":{"input_tokens":1,"output_tokens":1}}"#.into()))]);
        let p = j
            .noul(
                json!({"text": "rm -rf /"}),
                "Would this destroy data?",
                "yes",
                "no",
            )
            .await
            .unwrap();
        assert!((p - 0.92).abs() < 1e-9);
        let sent = s.sent.lock().unwrap();
        let (url, key, body) = &sent[0];
        assert_eq!(url, "https://api.typesafe.ai/v1/systemone");
        assert_eq!(key, "k-test");
        assert_eq!(body["model"], json!("jev-latest"));
        assert_eq!(body["state"], json!({"text": "rm -rf /"}));
        assert_eq!(body["questions"]["q"]["type"], json!("noul"));
        assert_eq!(body["questions"]["q"]["criteria"]["true"], json!("yes"));
        assert_eq!(body["questions"]["q"]["criteria"]["false"], json!("no"));
        assert_eq!(j.counters.answered.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_ranking_carries_the_distribution_and_the_any_fits_gate() {
        let (j, s) = judge(vec![Ok((200, CHOICE_REPLY.into()))]);
        let mut c = BTreeMap::new();
        c.insert("@e3".to_string(), "button 'Save'".to_string());
        c.insert("@e7".to_string(), "button 'Cancel'".to_string());
        let r = j
            .rank(
                json!({"request": "save the document"}),
                "Which element?",
                &c,
            )
            .await
            .unwrap();
        assert_eq!(r.choice, "@e3");
        assert_eq!(r.probabilities["@e7"], 0.15);
        assert_eq!(r.confidence, 0.82);
        assert_eq!(r.any_fits, 0.9);
        let sent = s.sent.lock().unwrap();
        let body = &sent[0].2;
        assert_eq!(body["questions"]["pick"]["type"], json!("choice"));
        assert_eq!(
            body["questions"]["pick"]["criteria"]["@e3"],
            json!("button 'Save'")
        );
        assert_eq!(body["questions"]["any"]["type"], json!("noul"));
    }

    #[tokio::test]
    async fn score_answers_parse() {
        let a: Answers = serde_json::from_str(r#"{"model":"m","answers":{"s":{"type":"score","score":1.6,"legend":{"0":"Calm"},"probabilities":{"0":0.05,"1":0.3,"2":0.65},"confidence":0.78}}}"#).unwrap();
        match &a.answers["s"] {
            Answer::Score {
                score, confidence, ..
            } => {
                assert_eq!(*score, 1.6);
                assert_eq!(*confidence, 0.78);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(a.usage.input_tokens, 0, "usage is optional");
    }

    // ---- boundaries -----------------------------------------------------

    #[tokio::test]
    async fn empty_and_oversized_choices_are_refused_before_any_call() {
        let (j, s) = judge(vec![]);
        let r = j.rank(json!({}), "x", &BTreeMap::new()).await;
        assert!(matches!(r, Err(JudgeError::BadRequest(_))));
        let big: BTreeMap<String, String> =
            (0..256).map(|i| (format!("@e{i}"), "x".into())).collect();
        let r = j.rank(json!({}), "x", &big).await;
        assert!(matches!(r, Err(JudgeError::BadRequest(m)) if m.contains("255")));
        let r = j.ask(json!({}), BTreeMap::new()).await;
        assert!(matches!(r, Err(JudgeError::BadRequest(_))));
        assert!(s.sent.lock().unwrap().is_empty(), "nothing must be sent");
        assert_eq!(j.counters.asked.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn truncation_respects_char_boundaries_and_marks_the_cut() {
        assert_eq!(truncate_utf8("abc", 10), "abc");
        let t = truncate_utf8(&"é".repeat(100), 30);
        assert!(t.ends_with("[truncated]"));
        assert!(t.len() <= 30);
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
        assert_eq!(truncate_utf8("", 5), "");
        // A budget smaller than the marker still yields a valid string.
        let t = truncate_utf8("hello world", 4);
        assert!(t.ends_with("[truncated]"));
    }

    #[test]
    fn config_that_can_never_work_is_rejected() {
        let ok = JudgeConfig::default();
        assert!(ok.validate().is_ok());
        for bad in [
            JudgeConfig {
                base_url: "http://api.typesafe.ai".into(),
                ..ok.clone()
            },
            JudgeConfig {
                model: " ".into(),
                ..ok.clone()
            },
            JudgeConfig {
                threshold: 1.5,
                ..ok.clone()
            },
            JudgeConfig {
                threshold: -0.1,
                ..ok.clone()
            },
            JudgeConfig {
                threshold: f64::NAN,
                ..ok.clone()
            },
            JudgeConfig {
                timeout_ms: 0,
                ..ok.clone()
            },
            JudgeConfig {
                max_state_bytes: 10,
                ..ok.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert!(JudgeConfig {
            threshold: 0.0,
            ..ok.clone()
        }
        .validate()
        .is_ok());
        assert!(JudgeConfig {
            threshold: 1.0,
            ..ok
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn dotenv_parsing_and_key_lookup_order() {
        assert_eq!(
            parse_dotenv("TYPESAFE_API_KEY=abc\n", "TYPESAFE_API_KEY").as_deref(),
            Some("abc")
        );
        assert_eq!(
            parse_dotenv("export TYPESAFE_API_KEY=\"q q\"", "TYPESAFE_API_KEY").as_deref(),
            Some("q q")
        );
        assert_eq!(
            parse_dotenv("# TYPESAFE_API_KEY=abc\nOTHER=1", "TYPESAFE_API_KEY"),
            None
        );
        assert_eq!(parse_dotenv("TYPESAFE_API_KEY=", "TYPESAFE_API_KEY"), None);
        let dir = std::env::temp_dir().join(format!("agentctl-judge-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // With nothing anywhere: an error that names every place.
        std::env::remove_var(ENV_KEY);
        std::env::set_var("AGENTCTL_ENV", dir.join("no-such.env"));
        let e = api_key(&dir).unwrap_err();
        assert!(e.contains(ENV_KEY) && e.contains("typesafe.key"), "{e}");
        // A key file that is world-readable is refused, not read.
        let kf = dir.join(KEY_FILE);
        std::fs::write(&kf, "secret\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&kf, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(api_key(&dir).unwrap_err().contains("chmod 600"));
            std::fs::set_permissions(&kf, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(api_key(&dir).unwrap(), "secret");
        std::env::remove_var("AGENTCTL_ENV");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- faults ---------------------------------------------------------

    #[tokio::test]
    async fn every_failure_is_an_error_with_a_cause_and_is_counted() {
        for (reply, expect) in [
            (Ok((401, "{}".into())), "401"),
            (Ok((422, r#"{"error":"bad"}"#.into())), "422"),
            (Ok((500, "".into())), "HTTP 500"),
            (Ok((200, "not json".into())), "unexpected response"),
            (Ok((200, r#"{"answers":{"q":{"type":"choice","choice":"x","probabilities":{},"confidence":1}}}"#.into())), "expected a noul"),
            (Err("connection reset".into()), "unreachable"),
        ] {
            let (j, _) = judge(vec![reply]);
            let e = j.noul(json!({}), "q", "y", "n").await.unwrap_err();
            assert!(e.message().contains(expect), "{expect}: {}", e.message());
            assert_eq!(j.counters.failed.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn back_off_statuses_are_retried_once_then_reported() {
        let (j, s) = judge(vec![
            Ok((429, "".into())),
            Ok((
                200,
                r#"{"answers":{"q":{"type":"noul","noul":0.5}}}"#.into(),
            )),
        ]);
        assert_eq!(j.noul(json!({}), "q", "y", "n").await.unwrap(), 0.5);
        assert_eq!(s.sent.lock().unwrap().len(), 2);
        let (j, s) = judge(vec![Ok((529, "".into())), Ok((529, "".into()))]);
        assert!(matches!(
            j.noul(json!({}), "q", "y", "n").await,
            Err(JudgeError::Status(529, _))
        ));
        assert_eq!(s.sent.lock().unwrap().len(), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn disabled_and_keyless_judges_never_touch_the_network() {
        let s = Scripted::new(vec![Ok((200, CHOICE_REPLY.into()))]);
        let off = Judge::with_transport(
            JudgeConfig::default(),
            Some("k".into()),
            Box::new(s.clone()),
        );
        assert!(!off.available());
        assert_eq!(
            off.noul(json!({}), "q", "y", "n").await,
            Err(JudgeError::Disabled)
        );
        let on = JudgeConfig {
            enabled: true,
            ..JudgeConfig::default()
        };
        let keyless = Judge::with_transport(on, None, Box::new(s.clone()));
        assert!(!keyless.available() && keyless.enabled());
        assert!(matches!(
            keyless.noul(json!({}), "q", "y", "n").await,
            Err(JudgeError::NoKey(_))
        ));
        assert!(s.sent.lock().unwrap().is_empty());
        assert!(
            !format!("{keyless:?}").contains("k-test"),
            "debug output never shows a key"
        );
    }

    #[test]
    fn curl_config_keeps_the_key_out_of_argv_and_records_status() {
        let c = curl_config(
            "https://x/v1/systemone",
            "k\"ey",
            Path::new("/tmp/b.json"),
            Duration::from_millis(4000),
        );
        assert!(c.contains("header = \"Authorization: Bearer k\\\"ey\""));
        assert!(c.contains("max-time = 4"));
        assert!(c.contains("data = \"@/tmp/b.json\""));
        assert_eq!(split_status("{\"a\":1}\n200"), (200, "{\"a\":1}"));
        assert_eq!(split_status("junk"), (0, "junk"));
        assert_eq!(split_status("body\nnotanumber"), (0, "body"));
    }

    // ---- concurrency ----------------------------------------------------

    /// The judge is shared by every tool call; counters and the transport
    /// must stay consistent under parallel use.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_asks_count_correctly() {
        let replies: Vec<_> = (0..40)
            .map(|i| {
                if i % 4 == 0 {
                    Ok((500, "".into()))
                } else {
                    Ok((
                        200,
                        r#"{"answers":{"q":{"type":"noul","noul":0.1}}}"#.into(),
                    ))
                }
            })
            .collect();
        let (j, _) = judge(replies);
        let j = Arc::new(j);
        let mut tasks = Vec::new();
        for _ in 0..40 {
            let j = j.clone();
            tasks.push(tokio::spawn(async move {
                j.noul(json!({}), "q", "y", "n").await.is_ok()
            }));
        }
        let mut ok = 0;
        for t in tasks {
            if t.await.unwrap() {
                ok += 1;
            }
        }
        assert_eq!(ok, 30);
        assert_eq!(j.counters.asked.load(Ordering::Relaxed), 40);
        assert_eq!(j.counters.answered.load(Ordering::Relaxed), 30);
        assert_eq!(j.counters.failed.load(Ordering::Relaxed), 10);
    }

    // ---- per-use thresholds --------------------------------------------

    #[test]
    fn an_unset_per_use_threshold_falls_back_to_the_general_one() {
        let cfg = JudgeConfig {
            threshold: 0.62,
            ..JudgeConfig::default()
        };
        assert_eq!(cfg.destructive_threshold(), 0.62);
        assert_eq!(cfg.injection_threshold(), 0.62);
        assert_eq!(cfg.match_threshold(), 0.62);
    }

    #[test]
    fn a_set_per_use_threshold_overrides_the_general_one() {
        let cfg = JudgeConfig {
            threshold: 0.7,
            destructive_threshold: Some(0.4),
            match_threshold: Some(0.55),
            ..JudgeConfig::default()
        };
        assert_eq!(cfg.destructive_threshold(), 0.4);
        // injection was left unset, so it still inherits the general bar.
        assert_eq!(cfg.injection_threshold(), 0.7);
        assert_eq!(cfg.match_threshold(), 0.55);
    }

    #[test]
    fn the_resolvers_agree_between_config_and_judge() {
        let cfg = JudgeConfig {
            threshold: 0.7,
            injection_threshold: Some(0.9),
            ..JudgeConfig::default()
        };
        let j = Judge::with_transport(cfg, Some("k".into()), Box::new(NoTransport));
        assert_eq!(j.injection_threshold(), 0.9);
        assert_eq!(j.destructive_threshold(), 0.7);
        assert_eq!(j.match_threshold(), 0.7);
    }

    #[test]
    fn a_valid_config_with_overrides_passes_and_boundaries_hold() {
        let cfg = JudgeConfig {
            destructive_threshold: Some(0.0),
            injection_threshold: Some(1.0),
            match_threshold: Some(0.5),
            ..JudgeConfig::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn an_out_of_range_or_nan_override_is_rejected_at_validate() {
        for bad in [-0.1_f64, 1.1, f64::NAN, f64::INFINITY] {
            let cfg = JudgeConfig {
                destructive_threshold: Some(bad),
                ..JudgeConfig::default()
            };
            assert!(
                cfg.validate().is_err(),
                "destructive_threshold {bad} must be rejected"
            );
            let cfg = JudgeConfig {
                match_threshold: Some(bad),
                ..JudgeConfig::default()
            };
            assert!(
                cfg.validate().is_err(),
                "match_threshold {bad} must be rejected"
            );
        }
    }

    /// A transport that must never be called: these tests only read config.
    struct NoTransport;
    #[async_trait]
    impl Transport for NoTransport {
        async fn post(
            &self,
            _u: &str,
            _k: &str,
            _b: &Value,
            _t: Duration,
        ) -> Result<(u16, String), String> {
            panic!("NoTransport must not be asked to post")
        }
    }
}
