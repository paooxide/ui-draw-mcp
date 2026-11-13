//! Desktop record and replay: a person demonstrates, agentctl keeps *what they
//! did to what*, and a later run does it again by finding the same thing.
//!
//! The browser's recorder keeps selectors, not coordinates, so a flow survives
//! the page being re-laid-out. The desktop version keeps the accessibility
//! equivalent: for each click, the role, title and ancestry of the element that
//! was under the pointer; for each run of typing, the field that had the focus
//! and the text; for each chord, the chord. A coordinate is stored only as a
//! hint for when the element cannot be found again.
//!
//! This module is the part that needs no operating system: reducing what was
//! observed to steps, coalescing keystrokes into typed text, scrubbing secrets,
//! scoring a locator against the candidates of a live tree, and the flow file.
//! Listening for the events and reading the accessibility facts at the
//! pointer is the platform backend's ([`RecordBackend`]); running a flow goes
//! through the server's gate in `tools.rs`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Longest a session may last, whatever was asked.
pub const MAX_RECORD_MS: u64 = 600_000;
/// Duration used when the caller names none.
pub const DEFAULT_RECORD_MS: u64 = 120_000;
/// Most steps a recording may hold. A person demonstrating a task does not
/// need more, and a flow this long is not one to replay unattended.
pub const MAX_RECORD_STEPS: usize = 200;
/// Most raw events kept before the session stops listening, so a held key or a
/// stuck button cannot grow memory without bound.
pub const MAX_RAW_EVENTS: usize = 4_000;
/// Version written into every flow file.
pub const FORMAT_VERSION: u32 = 1;

/// One link of the chain from an element up to its window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crumb {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// A screen point, kept only as a fallback hint.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// What to look for when a step is replayed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Locator {
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    /// Normalised role (`button`, `textarea`); empty when the pointer was over
    /// something the tree does not describe.
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Nearest parent first, up to and including the window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestry: Vec<Crumb>,
    /// Where the element was. Only used when nothing matches the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<Point>,
}

/// What a step does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Click,
    Type,
    Shortcut,
    /// Replace a field's value. Written by a person or agent editing a flow;
    /// the recorder cannot see a value being set, only keys.
    Set,
}

/// One step of a flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub op: Op,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<Locator>,
    /// `type` / `set`: the text. Never present for a secret field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `shortcut`: the chord, in `keyboard_shortcut` syntax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combo: Option<String>,
    /// `type` / `set` into a secure field: the name the value is supplied
    /// under when the flow runs, as the browser recorder does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    /// `click`: left, right or middle (left when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    /// `click`: 2 for a double click.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clicks: Option<u32>,
    /// `click` on a menu item: the titles from the menu bar down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub menu: Option<Vec<String>>,
    /// Milliseconds since the recording began.
    #[serde(default)]
    pub at_ms: u64,
}

impl Step {
    fn bare(op: Op, at_ms: u64) -> Step {
        Step {
            op,
            locator: None,
            text: None,
            combo: None,
            secret_ref: None,
            button: None,
            clicks: None,
            menu: None,
            at_ms,
        }
    }
}

/// A named, saved sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesktopFlow {
    pub name: String,
    pub version: u32,
    pub steps: Vec<Step>,
    pub updated_ms: u128,
}

// ---- what a backend observed ---------------------------------------------------

/// The accessibility facts about the element an event touched.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub app: String,
    pub window: Option<String>,
    pub role: String,
    pub title: Option<String>,
    pub ancestry: Vec<Crumb>,
    pub secure: bool,
}

/// A raw input event.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Click {
        /// 0 left, 1 right, 2 other.
        button: u8,
        clicks: u32,
        x: f64,
        y: f64,
    },
    Key {
        /// Canonical key name (`s`, `return`, `left`...), `keyboard_shortcut`
        /// syntax, when the backend knows it.
        name: Option<String>,
        /// The characters the key produced.
        text: String,
        cmd: bool,
        ctrl: bool,
        alt: bool,
        shift: bool,
    },
}

/// An event with the facts read when it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct Captured {
    pub at_ms: u64,
    pub input: Input,
    /// `None` when nothing could be read: the pointer or focus was somewhere
    /// the accessibility API does not describe, or its app is unknown.
    pub facts: Option<Facts>,
}

/// Remove what must never be kept. Done as soon as an event is read, by the
/// backend and again by the session, so the text typed into a secure field
/// exists for as short a time as the code allows and never reaches a step.
pub fn scrub(c: &mut Captured) {
    let secure = c.facts.as_ref().is_some_and(|f| f.secure);
    if let (true, Input::Key { text, .. }) = (secure, &mut c.input) {
        text.clear();
    }
}

/// What the session does with an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    Keep,
    /// Its app is not in `allowed_apps`.
    OtherApp,
    /// No facts, so the app is unknown.
    Unknown,
}

/// Whether an event may be kept. Events for any other application are counted
/// by the caller and not stored, nor looked at again.
pub fn admit(c: &Captured, allowed: &[String]) -> Admit {
    match &c.facts {
        None => Admit::Unknown,
        Some(f) if !mcp_types::apps::app_allowed(allowed, &f.app) => Admit::OtherApp,
        Some(_) => Admit::Keep,
    }
}

/// A secret's name, as the browser recorder derives one: lowercase letters and
/// digits, runs of anything else collapsed to `_`, at most 40 characters. Falls
/// back to `secret`.
pub fn secret_name(title: Option<&str>, role: &str) -> String {
    let raw = title.filter(|t| !t.trim().is_empty()).unwrap_or(role);
    let mut out = String::new();
    let mut sep = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            if sep && !out.is_empty() {
                out.push('_');
            }
            sep = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            sep = true;
        }
    }
    out.truncate(40);
    let out = out.trim_end_matches('_').to_string();
    if out.is_empty() {
        "secret".to_string()
    } else {
        out
    }
}

fn locator_of(f: &Facts, hint: Option<Point>) -> Locator {
    Locator {
        app: f.app.clone(),
        window: f.window.clone(),
        role: mcp_a11y::normalize_role(&f.role),
        title: f.title.clone().filter(|t| !t.trim().is_empty()),
        ancestry: f
            .ancestry
            .iter()
            .map(|c| Crumb {
                role: mcp_a11y::normalize_role(&c.role),
                title: c.title.clone().filter(|t| !t.trim().is_empty()),
            })
            .collect(),
        hint,
    }
}

/// The chord for a key event with a command, control or option modifier.
fn chord(name: &str, cmd: bool, ctrl: bool, alt: bool, shift: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if cmd {
        parts.push("cmd");
    }
    if ctrl {
        parts.push("ctrl");
    }
    if alt {
        parts.push("opt");
    }
    if shift {
        parts.push("shift");
    }
    parts.push(name);
    parts.join("+")
}

/// Whether text is something a person typed, as opposed to a control
/// character a key like Return or Escape produces.
fn printable(text: &str) -> bool {
    // The private-use block is where macOS puts the characters of arrow, page
    // and function keys; they are keys, not text.
    !text.is_empty()
        && text
            .chars()
            .all(|c| !c.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&c))
}

/// The titles of a menu chain: the clicked item and its menu parents, outermost
/// first. `None` when the element is not a menu item.
fn menu_path(f: &Facts) -> Option<Vec<String>> {
    if mcp_a11y::normalize_role(&f.role) != "menuitem" {
        return None;
    }
    let mut path: Vec<String> = f
        .ancestry
        .iter()
        .filter(|c| {
            matches!(
                mcp_a11y::normalize_role(&c.role).as_str(),
                "menuitem" | "menubaritem" | "menu"
            )
        })
        .filter_map(|c| c.title.clone().filter(|t| !t.is_empty()))
        .collect();
    path.reverse();
    path.dedup();
    path.push(f.title.clone().filter(|t| !t.is_empty())?);
    Some(path)
}

struct Typing {
    locator: Locator,
    secure: bool,
    name: String,
    text: String,
    at_ms: u64,
    keys: usize,
}

fn flush(typing: &mut Option<Typing>, out: &mut Vec<Step>) {
    let Some(t) = typing.take() else { return };
    // A run edited down to nothing leaves nothing worth replaying.
    if t.keys == 0 || (!t.secure && t.text.is_empty()) {
        return;
    }
    let mut s = Step::bare(Op::Type, t.at_ms);
    s.locator = Some(t.locator);
    if t.secure {
        s.secret_ref = Some(t.name);
    } else {
        s.text = Some(t.text);
    }
    out.push(s);
}

/// Turn what was observed into steps.
///
/// Clicks become `click` steps (a second click on the same element within the
/// same gesture raises `clicks` instead of adding a step). Printable keys with
/// no command, control or option modifier are coalesced into one `type` step
/// per field, with Backspace editing the run, and a run ends at a click, at a
/// non-printing key, or when the focused element changes. Keys with a command,
/// control or option modifier, and the non-printing keys, become `shortcut`
/// steps. Typing in a secure field becomes a `type` step with a `secret_ref`
/// and no text. Events with no facts are not steps: they cannot be located.
pub fn reduce(events: &[Captured]) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::new();
    let mut typing: Option<Typing> = None;
    for ev in events {
        let Some(f) = &ev.facts else { continue };
        match &ev.input {
            Input::Click {
                button,
                clicks,
                x,
                y,
            } => {
                flush(&mut typing, &mut out);
                let loc = locator_of(f, Some(Point { x: *x, y: *y }));
                // The second and third clicks of one gesture arrive as their
                // own events with a higher count.
                if *clicks > 1 {
                    if let Some(prev) = out.last_mut() {
                        if prev.op == Op::Click
                            && prev.locator.as_ref().map(|l| (&l.app, &l.role, &l.title))
                                == Some((&loc.app, &loc.role, &loc.title))
                        {
                            prev.clicks = Some(*clicks);
                            continue;
                        }
                    }
                }
                let mut s = Step::bare(Op::Click, ev.at_ms);
                s.menu = menu_path(f);
                s.locator = Some(loc);
                s.button = match button {
                    1 => Some("right".into()),
                    2 => Some("middle".into()),
                    _ => None,
                };
                if *clicks > 1 {
                    s.clicks = Some(*clicks);
                }
                out.push(s);
            }
            Input::Key {
                name,
                text,
                cmd,
                ctrl,
                alt,
                shift,
            } => {
                let modified = *cmd || *ctrl || *alt;
                let text_key = !modified && (f.secure || printable(text));
                if text_key {
                    // A different field ends the run.
                    let same = typing.as_ref().is_some_and(|t| {
                        t.locator.app == f.app
                            && t.locator.role == mcp_a11y::normalize_role(&f.role)
                            && t.locator.title == f.title.clone().filter(|x| !x.trim().is_empty())
                            && t.secure == f.secure
                    });
                    if !same {
                        flush(&mut typing, &mut out);
                        typing = Some(Typing {
                            locator: locator_of(f, None),
                            secure: f.secure,
                            name: secret_name(f.title.as_deref(), &f.role),
                            text: String::new(),
                            at_ms: ev.at_ms,
                            keys: 0,
                        });
                    }
                    if let Some(t) = typing.as_mut() {
                        t.keys += 1;
                        if !t.secure {
                            t.text.push_str(text);
                        }
                    }
                    continue;
                }
                // Backspace edits the run in progress instead of becoming a step.
                if name.as_deref() == Some("delete") && !modified && !*shift {
                    if let Some(t) = typing.as_mut() {
                        if t.secure {
                            t.keys = t.keys.saturating_sub(1);
                        } else {
                            t.text.pop();
                        }
                        continue;
                    }
                }
                flush(&mut typing, &mut out);
                let Some(key) = name else { continue };
                let mut s = Step::bare(Op::Shortcut, ev.at_ms);
                s.combo = Some(chord(key, *cmd, *ctrl, *alt, *shift));
                s.locator = Some(locator_of(f, None));
                out.push(s);
            }
        }
    }
    flush(&mut typing, &mut out);
    out
}

/// Cut a parent chain (nearest first) where a locator should stop: after the
/// window that holds the element, or before the application when there is no
/// window (a menu bar item), and at most [`MAX_ANCESTRY`] links. Both the
/// backend reading the element under the pointer and the walk over a snapshot
/// use it, so the two chains are cut at the same place.
pub fn trim_ancestry(chain: Vec<Crumb>) -> Vec<Crumb> {
    let mut out = Vec::new();
    for c in chain {
        let role = mcp_a11y::normalize_role(&c.role);
        if role == "application" {
            break;
        }
        let window = role == "window";
        out.push(c);
        if window || out.len() >= MAX_ANCESTRY {
            break;
        }
    }
    out
}

/// Most parents a locator keeps.
pub const MAX_ANCESTRY: usize = 8;

/// The candidates of a snapshot: every node that has a ref, with its parents
/// (nearest first, cut by [`trim_ancestry`]) and the centre of its bounds.
/// `refs` maps a backend node id to the ref the flattener gave it.
pub fn candidates_of(
    root: &mcp_a11y::UiNode,
    refs: &std::collections::HashMap<u64, String>,
) -> Vec<Candidate> {
    fn walk(
        n: &mcp_a11y::UiNode,
        refs: &std::collections::HashMap<u64, String>,
        parents: &mut Vec<Crumb>,
        out: &mut Vec<Candidate>,
    ) {
        if let Some(r) = n.node_id.and_then(|id| refs.get(&id)) {
            out.push(Candidate {
                reff: r.clone(),
                role: mcp_a11y::normalize_role(&n.role),
                title: n.name.clone().filter(|t| !t.trim().is_empty()),
                ancestry: trim_ancestry(parents.iter().rev().cloned().collect()),
                center: n.bounds.map(|b| Point {
                    x: b.x + b.w / 2.0,
                    y: b.y + b.h / 2.0,
                }),
            });
        }
        parents.push(Crumb {
            role: mcp_a11y::normalize_role(&n.role),
            title: n.name.clone().filter(|t| !t.trim().is_empty()),
        });
        for c in &n.children {
            walk(c, refs, parents, out);
        }
        parents.pop();
    }
    let mut out = Vec::new();
    walk(root, refs, &mut Vec::new(), &mut out);
    out
}

/// The `ui_action` a recorded click stands for.
///
/// A click on a place where text goes is a request for the keyboard focus, and
/// an accessibility press on such an element is not offered (a text area has
/// no AXPress), so it is replayed as `focus`; the double and right click keep
/// their own actions, which the element either offers or refuses by name.
pub fn click_action(role: &str, button: Option<&str>, clicks: Option<u32>) -> &'static str {
    match (button, clicks) {
        (Some("right"), _) => "right_click",
        (_, Some(n)) if n >= 2 => "double_click",
        _ if matches!(role, "textarea" | "textfield" | "combobox" | "searchfield") => "focus",
        _ => "click",
    }
}

// ---- finding the element again --------------------------------------------------

/// A live element a locator may match.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub reff: String,
    pub role: String,
    pub title: Option<String>,
    /// Nearest parent first.
    pub ancestry: Vec<Crumb>,
    pub center: Option<Point>,
}

/// How well a locator matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchKind {
    /// Role, title and the whole ancestry agree.
    Exact,
    /// Role and title agree but the surroundings moved, or the title only
    /// contains or is contained by the recorded one.
    Fuzzy,
    /// Nothing matched; the recorded coordinate is used.
    Coordinate,
}

/// What looking for a locator found.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Found {
        index: usize,
        kind: MatchKind,
    },
    /// Several fit equally well and nothing separates them; acting on one
    /// would be a guess.
    Ambiguous(Vec<usize>),
    NotFound,
}

fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn same_title(a: &Option<String>, b: &Option<String>) -> Option<bool> {
    match (a, b) {
        (None, None) => Some(true),
        (Some(a), Some(b)) => {
            let (a, b) = (norm(a), norm(b));
            if a == b {
                Some(true)
            } else if !a.is_empty() && !b.is_empty() && (a.contains(&b) || b.contains(&a)) {
                Some(false)
            } else {
                None
            }
        }
        // A titled control against an untitled one is a different control; the
        // reverse (the recording had no title) is tolerated as fuzzy.
        (None, Some(_)) => Some(false),
        (Some(_), None) => None,
    }
}

/// Crumbs of the recording that the candidate's ancestry agrees with, in
/// order from the nearest parent, and whether all of them did.
fn ancestry_agreement(want: &[Crumb], have: &[Crumb]) -> (usize, bool) {
    let mut agreed = 0;
    for (w, h) in want.iter().zip(have) {
        let title_ok = match (&w.title, &h.title) {
            (Some(a), Some(b)) => norm(a) == norm(b),
            (None, _) => true,
            (Some(_), None) => false,
        };
        if w.role == h.role && title_ok {
            agreed += 1;
        } else {
            break;
        }
    }
    (agreed, agreed == want.len())
}

fn distance(a: Point, b: Point) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// Find the candidate a locator describes.
///
/// A candidate must have the recorded role. Its title must agree (equal, or one
/// containing the other, which is fuzzy; a different title is no match, so a
/// control that was renamed is not clicked for its old name). Among those, the
/// ones whose ancestry agrees most win; ties are broken by the recorded
/// coordinate when there is one and by nothing else: if two candidates remain
/// equally good the answer is [`Resolution::Ambiguous`], never a guess. With no
/// role-and-title match at all, the recorded coordinate is the fallback.
pub fn resolve(loc: &Locator, cands: &[Candidate]) -> Resolution {
    struct Scored {
        i: usize,
        exact_title: bool,
        agreed: usize,
        full: bool,
    }
    let mut scored: Vec<Scored> = Vec::new();
    if !loc.role.is_empty() {
        for (i, c) in cands.iter().enumerate() {
            if c.role != loc.role {
                continue;
            }
            let Some(exact_title) = same_title(&loc.title, &c.title) else {
                continue;
            };
            let (agreed, full) = ancestry_agreement(&loc.ancestry, &c.ancestry);
            scored.push(Scored {
                i,
                exact_title,
                agreed,
                full,
            });
        }
    }
    if scored.is_empty() {
        return if loc.hint.is_some() {
            Resolution::Found {
                index: usize::MAX,
                kind: MatchKind::Coordinate,
            }
        } else {
            Resolution::NotFound
        };
    }
    let best = scored
        .iter()
        .map(|s| (s.exact_title, s.agreed))
        .max()
        .unwrap_or((false, 0));
    let mut top: Vec<&Scored> = scored
        .iter()
        .filter(|s| (s.exact_title, s.agreed) == best)
        .collect();
    if top.len() > 1 {
        if let Some(h) = loc.hint {
            let d = |s: &&Scored| {
                cands[s.i]
                    .center
                    .map(|c| distance(c, h))
                    .unwrap_or(f64::INFINITY)
            };
            let nearest = top.iter().map(d).fold(f64::INFINITY, f64::min);
            let near: Vec<&Scored> = top.iter().copied().filter(|s| d(s) == nearest).collect();
            // Only a clear winner by position counts: a candidate with no
            // position at all must not win a tie.
            if nearest.is_finite() && near.len() == 1 {
                top = near;
            }
        }
    }
    if top.len() > 1 {
        return Resolution::Ambiguous(top.iter().map(|s| s.i).collect());
    }
    let s = top[0];
    Resolution::Found {
        index: s.i,
        kind: if s.exact_title && s.full {
            MatchKind::Exact
        } else {
            MatchKind::Fuzzy
        },
    }
}

// ---- secrets in a flow ------------------------------------------------------------

/// Refuse a flow that would persist a secret: a step with a `secret_ref` must
/// hold no text. Same rule as the browser's `validate_flow_secrets`.
pub fn validate_secrets(steps: &[Step]) -> Result<(), String> {
    for (i, s) in steps.iter().enumerate() {
        if let Some(r) = &s.secret_ref {
            if r.trim().is_empty() {
                return Err(format!("step {i}: 'secret_ref' must be a non-empty string"));
            }
            if s.text.is_some() {
                return Err(format!(
                    "step {i} has a secret_ref and also holds 'text', which would be stored in the flow file. \
                     Remove 'text'; the value is passed as {{\"secrets\": {{\"{r}\": \"...\"}}}} when the flow runs"
                ));
            }
        }
    }
    Ok(())
}

/// Every distinct `secret_ref`, in first-use order.
pub fn secret_refs(steps: &[Step]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in steps {
        if let Some(r) = &s.secret_ref {
            if !out.contains(r) {
                out.push(r.clone());
            }
        }
    }
    out
}

/// The refs a flow needs that `supplied` lacks.
pub fn missing_secrets(steps: &[Step], supplied: &BTreeMap<String, String>) -> Vec<String> {
    secret_refs(steps)
        .into_iter()
        .filter(|r| !supplied.contains_key(r))
        .collect()
}

// ---- the flow file ------------------------------------------------------------------

/// Why a flow could not be stored or read.
#[derive(Debug)]
pub enum FlowError {
    Invalid(String),
    Io(String),
}

/// File-backed flows, keyed by name; one JSON file in the agentctl state
/// directory, like the browser's.
pub struct FlowStore {
    path: PathBuf,
    max_flows: usize,
    max_steps: usize,
}

impl FlowStore {
    pub fn new(path: PathBuf, max_flows: usize, max_steps: usize) -> Self {
        FlowStore {
            path,
            max_flows,
            max_steps,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn load(&self) -> Result<BTreeMap<String, DesktopFlow>, FlowError> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => {
                parse_flows(&t).map_err(|e| FlowError::Io(format!("{}: {e}", self.path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(FlowError::Io(format!("{}: {e}", self.path.display()))),
        }
    }

    fn persist(&self, map: &BTreeMap<String, DesktopFlow>) -> Result<(), FlowError> {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let json = serde_json::to_string_pretty(map)
            .map_err(|e| FlowError::Io(format!("serialize: {e}")))?;
        // Written beside and renamed, so a crash leaves the old file whole.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| FlowError::Io(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| FlowError::Io(format!("{}: {e}", self.path.display())))
    }

    pub fn save(
        &self,
        name: &str,
        steps: Vec<Step>,
        now_ms: u128,
    ) -> Result<DesktopFlow, FlowError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(FlowError::Invalid("flow name must not be empty".into()));
        }
        if steps.is_empty() {
            return Err(FlowError::Invalid("a flow needs at least one step".into()));
        }
        validate_secrets(&steps).map_err(FlowError::Invalid)?;
        if steps.len() > self.max_steps {
            return Err(FlowError::Invalid(format!(
                "a flow may have at most {} steps",
                self.max_steps
            )));
        }
        let mut map = self.load()?;
        if !map.contains_key(name) && map.len() >= self.max_flows {
            return Err(FlowError::Invalid(format!(
                "at most {} flows may be stored",
                self.max_flows
            )));
        }
        let flow = DesktopFlow {
            name: name.to_string(),
            version: FORMAT_VERSION,
            steps,
            updated_ms: now_ms,
        };
        map.insert(name.to_string(), flow.clone());
        self.persist(&map)?;
        Ok(flow)
    }

    pub fn get(&self, name: &str) -> Result<Option<DesktopFlow>, FlowError> {
        Ok(self.load()?.remove(name.trim()))
    }

    pub fn list(&self) -> Result<Vec<DesktopFlow>, FlowError> {
        Ok(self.load()?.into_values().collect())
    }

    pub fn delete(&self, name: &str) -> Result<bool, FlowError> {
        let mut map = self.load()?;
        let removed = map.remove(name.trim()).is_some();
        if removed {
            self.persist(&map)?;
        }
        Ok(removed)
    }
}

/// Read a flow file. A file from a newer format is refused by name rather than
/// half-understood.
pub fn parse_flows(text: &str) -> Result<BTreeMap<String, DesktopFlow>, String> {
    let map: BTreeMap<String, DesktopFlow> =
        serde_json::from_str(text).map_err(|e| e.to_string())?;
    if let Some(f) = map.values().find(|f| f.version > FORMAT_VERSION) {
        return Err(format!(
            "flow '{}' is format version {}, newer than this agentctl understands ({FORMAT_VERSION})",
            f.name, f.version
        ));
    }
    Ok(map)
}

// ---- the platform side ------------------------------------------------------------------

/// Why recording could not start.
#[derive(Debug, Clone)]
pub enum RecordError {
    /// A permission is missing; the message names which and where to grant it.
    PermissionDenied(String),
    /// This platform has no way to do it; the message says why.
    Unsupported(String),
    Failed(String),
}

/// A running listener. Dropping it stops listening.
pub trait RecordSession: Send {
    /// Events seen since the last call, in order, with their facts read.
    fn drain(&mut self) -> Vec<Captured>;
    /// Events the backend had to discard because it could not keep up.
    fn lost(&self) -> usize;
}

/// A platform's way to watch a person use the machine, without taking part:
/// a listen-only tap on macOS.
pub trait RecordBackend: Send + Sync {
    /// Start listening, or say what is missing.
    fn start_recording(&self) -> Result<Box<dyn RecordSession>, RecordError>;
}

/// What is written to the state directory while a recording is running, so
/// `agentctl doctor` can say that one is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveMarker {
    pub pid: u32,
    pub started_ms: u128,
    pub expires_ms: u128,
    pub apps: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crumb(role: &str, title: Option<&str>) -> Crumb {
        Crumb {
            role: role.into(),
            title: title.map(str::to_string),
        }
    }

    fn facts(app: &str, role: &str, title: Option<&str>) -> Facts {
        Facts {
            app: app.into(),
            window: Some("Untitled".into()),
            role: role.into(),
            title: title.map(str::to_string),
            ancestry: vec![
                crumb("AXScrollArea", None),
                crumb("AXWindow", Some("Untitled")),
            ],
            secure: false,
        }
    }

    fn click(at: u64, f: Facts) -> Captured {
        Captured {
            at_ms: at,
            input: Input::Click {
                button: 0,
                clicks: 1,
                x: 100.0,
                y: 200.0,
            },
            facts: Some(f),
        }
    }

    fn key(
        at: u64,
        name: Option<&str>,
        text: &str,
        mods: (bool, bool, bool, bool),
        f: Facts,
    ) -> Captured {
        Captured {
            at_ms: at,
            input: Input::Key {
                name: name.map(str::to_string),
                text: text.into(),
                cmd: mods.0,
                ctrl: mods.1,
                alt: mods.2,
                shift: mods.3,
            },
            facts: Some(f),
        }
    }

    const NONE: (bool, bool, bool, bool) = (false, false, false, false);

    fn typed(at: u64, ch: &str, f: Facts) -> Captured {
        key(at, Some(ch), ch, NONE, f)
    }

    #[test]
    fn a_click_keeps_what_was_under_the_pointer_and_the_coordinate_only_as_a_hint() {
        let steps = reduce(&[click(5, facts("TextEdit", "AXTextArea", None))]);
        assert_eq!(steps.len(), 1);
        let s = &steps[0];
        assert_eq!(s.op, Op::Click);
        let l = s.locator.as_ref().unwrap();
        assert_eq!(l.app, "TextEdit");
        assert_eq!(l.role, "textarea");
        assert_eq!(l.title, None);
        assert_eq!(l.ancestry[1], crumb("window", Some("Untitled")));
        assert_eq!(l.hint, Some(Point { x: 100.0, y: 200.0 }));
        assert_eq!(s.at_ms, 5);
    }

    #[test]
    fn keystrokes_in_one_field_become_one_typed_text() {
        let f = facts("TextEdit", "AXTextArea", None);
        let ev: Vec<Captured> = "Hello"
            .chars()
            .enumerate()
            .map(|(i, c)| typed(i as u64, &c.to_string(), f.clone()))
            .collect();
        let steps = reduce(&ev);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].op, Op::Type);
        assert_eq!(steps[0].text.as_deref(), Some("Hello"));
        assert_eq!(steps[0].locator.as_ref().unwrap().role, "textarea");
        // A typed step carries no coordinate: the field is what matters.
        assert_eq!(steps[0].locator.as_ref().unwrap().hint, None);
    }

    #[test]
    fn backspace_edits_the_run_instead_of_becoming_a_step() {
        let f = facts("TextEdit", "AXTextArea", None);
        let ev = vec![
            typed(1, "c", f.clone()),
            typed(2, "a", f.clone()),
            typed(3, "x", f.clone()),
            key(4, Some("delete"), "\u{7f}", NONE, f.clone()),
            typed(5, "t", f),
        ];
        let steps = reduce(&ev);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].text.as_deref(), Some("cat"));
    }

    #[test]
    fn a_click_between_two_runs_of_typing_keeps_them_apart() {
        let a = facts("TextEdit", "AXTextArea", None);
        let steps = reduce(&[
            typed(1, "a", a.clone()),
            click(2, a.clone()),
            typed(3, "b", a),
        ]);
        let ops: Vec<(Op, Option<&str>)> =
            steps.iter().map(|s| (s.op, s.text.as_deref())).collect();
        assert_eq!(
            ops,
            vec![
                (Op::Type, Some("a")),
                (Op::Click, None),
                (Op::Type, Some("b"))
            ]
        );
    }

    #[test]
    fn typing_in_another_field_ends_the_run() {
        let steps = reduce(&[
            typed(1, "a", facts("Mail", "AXTextField", Some("To"))),
            typed(2, "b", facts("Mail", "AXTextField", Some("Subject"))),
        ]);
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps[1].locator.as_ref().unwrap().title.as_deref(),
            Some("Subject")
        );
    }

    #[test]
    fn command_chords_and_non_printing_keys_become_shortcuts() {
        let f = facts("TextEdit", "AXTextArea", None);
        let steps = reduce(&[
            key(1, Some("s"), "s", (true, false, false, false), f.clone()),
            key(
                2,
                Some("left"),
                "\u{f702}",
                (false, false, false, true),
                f.clone(),
            ),
            key(3, Some("return"), "\r", NONE, f),
        ]);
        let combos: Vec<&str> = steps.iter().map(|s| s.combo.as_deref().unwrap()).collect();
        assert_eq!(combos, vec!["cmd+s", "shift+left", "return"]);
        assert!(steps.iter().all(|s| s.op == Op::Shortcut));
        // Every spelling the recorder writes is one the replay tool parses.
        for c in combos {
            assert!(crate::parse_combo(c, crate::Os::Mac).is_ok(), "{c}");
        }
    }

    #[test]
    fn a_chord_ends_the_typing_before_it() {
        let f = facts("TextEdit", "AXTextArea", None);
        let steps = reduce(&[
            typed(1, "h", f.clone()),
            key(2, Some("a"), "a", (true, false, false, false), f.clone()),
            typed(3, "i", f),
        ]);
        let ops: Vec<Op> = steps.iter().map(|s| s.op).collect();
        assert_eq!(ops, vec![Op::Type, Op::Shortcut, Op::Type]);
        assert_eq!(steps[1].combo.as_deref(), Some("cmd+a"));
    }

    #[test]
    fn a_double_click_is_one_step_with_a_count() {
        let f = facts("Finder", "AXRow", Some("Documents"));
        let mut second = click(2, f.clone());
        if let Input::Click { clicks, .. } = &mut second.input {
            *clicks = 2;
        }
        let steps = reduce(&[click(1, f), second]);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].clicks, Some(2));
    }

    #[test]
    fn a_right_click_says_which_button() {
        let mut c = click(1, facts("Finder", "AXRow", Some("x")));
        if let Input::Click { button, .. } = &mut c.input {
            *button = 1;
        }
        assert_eq!(reduce(&[c])[0].button.as_deref(), Some("right"));
    }

    #[test]
    fn a_menu_pick_keeps_the_menu_path_from_the_menu_bar_down() {
        let f = Facts {
            app: "TextEdit".into(),
            window: None,
            role: "AXMenuItem".into(),
            title: Some("Save\u{2026}".into()),
            ancestry: vec![
                crumb("AXMenu", Some("File")),
                crumb("AXMenuBarItem", Some("File")),
                crumb("AXMenuBar", None),
            ],
            secure: false,
        };
        let steps = reduce(&[click(1, f)]);
        assert_eq!(
            steps[0].menu,
            Some(vec!["File".to_string(), "Save\u{2026}".to_string()])
        );
    }

    #[test]
    fn text_typed_into_a_secure_field_is_never_a_step_and_names_a_secret() {
        let mut f = facts("Mail", "AXTextField", Some("Password"));
        f.secure = true;
        let mut ev: Vec<Captured> = "hunter2"
            .chars()
            .enumerate()
            .map(|(i, c)| typed(i as u64, &c.to_string(), f.clone()))
            .collect();
        for e in &mut ev {
            scrub(e);
        }
        let steps = reduce(&ev);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].secret_ref.as_deref(), Some("password"));
        assert_eq!(steps[0].text, None);
        let json = serde_json::to_string(&steps).unwrap();
        assert!(!json.contains("hunter"), "{json}");
        assert!(validate_secrets(&steps).is_ok());
    }

    #[test]
    fn scrubbing_clears_secure_key_text_and_nothing_else() {
        let mut secure = facts("Mail", "AXTextField", Some("Password"));
        secure.secure = true;
        let mut a = typed(1, "x", secure);
        scrub(&mut a);
        assert!(matches!(&a.input, Input::Key { text, .. } if text.is_empty()));
        let mut b = typed(1, "x", facts("Mail", "AXTextField", Some("To")));
        scrub(&mut b);
        assert!(matches!(&b.input, Input::Key { text, .. } if text == "x"));
        // A click on a secure field has no text to scrub and keeps its facts.
        let mut c = click(1, facts("Mail", "AXTextField", Some("Password")));
        scrub(&mut c);
        assert!(c.facts.is_some());
    }

    #[test]
    fn a_secret_flow_that_holds_text_is_refused() {
        let mut s = Step::bare(Op::Type, 0);
        s.secret_ref = Some("password".into());
        s.text = Some("hunter2".into());
        let e = validate_secrets(&[s]).unwrap_err();
        assert!(e.contains("step 0") && !e.contains("hunter2"), "{e}");
    }

    #[test]
    fn secret_names_follow_the_browser_rule() {
        assert_eq!(secret_name(Some("Password"), "AXTextField"), "password");
        assert_eq!(
            secret_name(Some("  Card number (CVV) "), "x"),
            "card_number_cvv"
        );
        assert_eq!(secret_name(None, "AXTextField"), "axtextfield");
        assert_eq!(secret_name(Some("***"), ""), "secret");
        assert!(secret_name(Some(&"a".repeat(100)), "x").len() <= 40);
    }

    #[test]
    fn events_with_no_facts_are_not_steps() {
        let mut c = click(1, facts("A", "AXButton", Some("x")));
        c.facts = None;
        assert!(reduce(&[c]).is_empty());
    }

    #[test]
    fn only_events_in_allowed_apps_are_admitted() {
        let allowed = vec!["TextEdit".to_string()];
        let ok = click(1, facts("TextEdit", "AXTextArea", None));
        let other = click(1, facts("Mail", "AXButton", Some("Send")));
        let mut unknown = click(1, facts("TextEdit", "AXTextArea", None));
        unknown.facts = None;
        assert_eq!(admit(&ok, &allowed), Admit::Keep);
        assert_eq!(admit(&other, &allowed), Admit::OtherApp);
        assert_eq!(admit(&unknown, &allowed), Admit::Unknown);
        assert_eq!(admit(&ok, &[]), Admit::OtherApp);
    }

    fn cand(r: &str, role: &str, title: Option<&str>, at: (f64, f64)) -> Candidate {
        Candidate {
            reff: r.into(),
            role: role.into(),
            title: title.map(str::to_string),
            ancestry: vec![crumb("scrollarea", None), crumb("window", Some("Untitled"))],
            center: Some(Point { x: at.0, y: at.1 }),
        }
    }

    fn locator(role: &str, title: Option<&str>, hint: Option<(f64, f64)>) -> Locator {
        Locator {
            app: "TextEdit".into(),
            window: None,
            role: role.into(),
            title: title.map(str::to_string),
            ancestry: vec![crumb("scrollarea", None), crumb("window", Some("Untitled"))],
            hint: hint.map(|(x, y)| Point { x, y }),
        }
    }

    #[test]
    fn ancestry_stops_after_the_window_and_before_the_application() {
        let chain = vec![
            crumb("AXScrollArea", None),
            crumb("AXWindow", Some("Untitled")),
            crumb("AXApplication", Some("TextEdit")),
        ];
        let t = trim_ancestry(chain);
        assert_eq!(t.len(), 2);
        assert_eq!(t[1].role, "AXWindow");
        // A menu bar item has no window: the application is not a parent.
        let menu = trim_ancestry(vec![
            crumb("AXMenu", Some("File")),
            crumb("AXMenuBar", None),
            crumb("AXApplication", None),
        ]);
        assert_eq!(menu.len(), 2);
        let long: Vec<Crumb> = (0..20)
            .map(|i| crumb("AXGroup", Some(&i.to_string())))
            .collect();
        assert_eq!(trim_ancestry(long).len(), MAX_ANCESTRY);
    }

    fn node(
        role: &str,
        name: Option<&str>,
        id: Option<u64>,
        kids: Vec<mcp_a11y::UiNode>,
    ) -> mcp_a11y::UiNode {
        mcp_a11y::UiNode {
            role: role.into(),
            name: name.map(str::to_string),
            node_id: id,
            bounds: id.map(|_| mcp_a11y::Bounds {
                x: 10.0,
                y: 20.0,
                w: 100.0,
                h: 40.0,
            }),
            children: kids,
            ..Default::default()
        }
    }

    #[test]
    fn a_snapshot_yields_candidates_whose_parents_match_what_the_recorder_reads() {
        let tree = node(
            "AXApplication",
            Some("TextEdit"),
            None,
            vec![node(
                "AXWindow",
                Some("Untitled"),
                Some(1),
                vec![node(
                    "AXScrollArea",
                    None,
                    None,
                    vec![node("AXTextArea", None, Some(2), vec![])],
                )],
            )],
        );
        let refs: std::collections::HashMap<u64, String> =
            [(1, "@e1".to_string()), (2, "@e2".to_string())].into();
        let cands = candidates_of(&tree, &refs);
        assert_eq!(cands.len(), 2);
        let area = &cands[1];
        assert_eq!(area.reff, "@e2");
        assert_eq!(area.role, "textarea");
        assert_eq!(area.ancestry[0].role, "scrollarea");
        assert_eq!(area.ancestry[1].role, "window");
        assert_eq!(area.ancestry.len(), 2);
        assert_eq!(area.center, Some(Point { x: 60.0, y: 40.0 }));
        // What the recorder would have stored for a click there finds it.
        let facts = Facts {
            app: "TextEdit".into(),
            window: Some("Untitled".into()),
            role: "AXTextArea".into(),
            title: None,
            ancestry: trim_ancestry(vec![
                crumb("AXScrollArea", None),
                crumb("AXWindow", Some("Untitled")),
                crumb("AXApplication", Some("TextEdit")),
            ]),
            secure: false,
        };
        let loc = locator_of(&facts, None);
        assert_eq!(
            resolve(&loc, &cands),
            Resolution::Found {
                index: 1,
                kind: MatchKind::Exact
            }
        );
    }

    #[test]
    fn a_click_where_text_goes_replays_as_focus() {
        assert_eq!(click_action("textarea", None, None), "focus");
        assert_eq!(click_action("textfield", Some("left"), Some(1)), "focus");
        assert_eq!(click_action("button", None, None), "click");
        assert_eq!(click_action("textarea", None, Some(2)), "double_click");
        assert_eq!(click_action("button", Some("right"), None), "right_click");
    }

    #[test]
    fn a_unique_role_title_and_ancestry_match_is_exact() {
        let c = vec![
            cand("@e1", "button", Some("Save"), (0.0, 0.0)),
            cand("@e2", "textarea", None, (50.0, 50.0)),
        ];
        assert_eq!(
            resolve(&locator("textarea", None, None), &c),
            Resolution::Found {
                index: 1,
                kind: MatchKind::Exact
            }
        );
    }

    #[test]
    fn a_title_that_contains_the_recorded_one_is_fuzzy_and_a_different_one_is_not_a_match() {
        let c = vec![cand("@e1", "button", Some("Save As\u{2026}"), (0.0, 0.0))];
        assert_eq!(
            resolve(&locator("button", Some("Save As"), None), &c),
            Resolution::Found {
                index: 0,
                kind: MatchKind::Fuzzy
            }
        );
        // A renamed control is not clicked for its old name.
        assert_eq!(
            resolve(&locator("button", Some("Delete"), None), &c),
            Resolution::NotFound
        );
    }

    #[test]
    fn a_changed_surrounding_downgrades_an_exact_match_to_fuzzy() {
        let mut c = cand("@e1", "textarea", None, (0.0, 0.0));
        c.ancestry = vec![crumb("group", None), crumb("window", Some("Other"))];
        assert_eq!(
            resolve(&locator("textarea", None, None), &[c]),
            Resolution::Found {
                index: 0,
                kind: MatchKind::Fuzzy
            }
        );
    }

    #[test]
    fn a_better_ancestry_beats_a_worse_one_with_the_same_title() {
        let good = cand("@e1", "button", Some("OK"), (0.0, 0.0));
        let mut worse = cand("@e2", "button", Some("OK"), (0.0, 0.0));
        worse.ancestry = vec![crumb("sheet", None)];
        assert_eq!(
            resolve(&locator("button", Some("OK"), None), &[worse, good]),
            Resolution::Found {
                index: 1,
                kind: MatchKind::Exact
            }
        );
    }

    #[test]
    fn two_equal_candidates_are_ambiguous_unless_the_position_separates_them() {
        let c = vec![
            cand("@e1", "button", Some("OK"), (10.0, 10.0)),
            cand("@e2", "button", Some("OK"), (300.0, 300.0)),
        ];
        assert_eq!(
            resolve(&locator("button", Some("OK"), None), &c),
            Resolution::Ambiguous(vec![0, 1])
        );
        assert_eq!(
            resolve(&locator("button", Some("OK"), Some((290.0, 305.0))), &c),
            Resolution::Found {
                index: 1,
                kind: MatchKind::Exact
            }
        );
        // Candidates with no position cannot win a tie on position.
        let mut blind = c.clone();
        blind[0].center = None;
        blind[1].center = None;
        assert!(matches!(
            resolve(&locator("button", Some("OK"), Some((1.0, 1.0))), &blind),
            Resolution::Ambiguous(_)
        ));
    }

    #[test]
    fn nothing_matching_falls_back_to_the_coordinate_only_when_there_is_one() {
        let c = vec![cand("@e1", "button", Some("Save"), (0.0, 0.0))];
        assert_eq!(
            resolve(&locator("slider", None, Some((5.0, 5.0))), &c),
            Resolution::Found {
                index: usize::MAX,
                kind: MatchKind::Coordinate
            }
        );
        assert_eq!(
            resolve(&locator("slider", None, None), &c),
            Resolution::NotFound
        );
        // A locator with no role at all (nothing was described) is coordinate-only.
        assert_eq!(
            resolve(&locator("", None, Some((5.0, 5.0))), &c),
            Resolution::Found {
                index: usize::MAX,
                kind: MatchKind::Coordinate
            }
        );
    }

    fn store(tag: &str) -> FlowStore {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "agentctl-desktop-flows-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        FlowStore::new(p, 3, 5)
    }

    #[test]
    fn a_flow_round_trips_through_the_file_with_every_field() {
        let s = store("rt");
        let mut click = Step::bare(Op::Click, 10);
        click.locator = Some(locator("button", Some("OK"), Some((1.0, 2.0))));
        click.button = Some("right".into());
        click.clicks = Some(2);
        click.menu = Some(vec!["File".into(), "Open".into()]);
        let mut secret = Step::bare(Op::Type, 20);
        secret.secret_ref = Some("password".into());
        let mut chord = Step::bare(Op::Shortcut, 30);
        chord.combo = Some("cmd+s".into());
        let steps = vec![click, secret, chord];
        s.save("login", steps.clone(), 7).unwrap();
        let back = s.get("login").unwrap().unwrap();
        assert_eq!(back.steps, steps);
        assert_eq!(back.version, FORMAT_VERSION);
        assert_eq!(back.updated_ms, 7);
        assert_eq!(s.list().unwrap().len(), 1);
        assert!(s.delete("login").unwrap());
        assert!(!s.delete("login").unwrap());
        let _ = std::fs::remove_file(s.path());
    }

    #[test]
    fn the_store_enforces_its_bounds_and_the_secret_rule_and_writes_nothing_when_refused() {
        let s = store("bounds");
        let step = Step::bare(Op::Shortcut, 0);
        assert!(matches!(
            s.save(" ", vec![step.clone()], 1),
            Err(FlowError::Invalid(_))
        ));
        assert!(matches!(s.save("a", vec![], 1), Err(FlowError::Invalid(_))));
        assert!(matches!(
            s.save("a", vec![step.clone(); 6], 1),
            Err(FlowError::Invalid(_))
        ));
        let mut leaky = Step::bare(Op::Type, 0);
        leaky.secret_ref = Some("p".into());
        leaky.text = Some("hunter2".into());
        assert!(matches!(
            s.save("a", vec![leaky], 1),
            Err(FlowError::Invalid(_))
        ));
        assert!(
            !s.path().exists(),
            "a refused save must not create the file"
        );
        for n in ["a", "b", "c"] {
            s.save(n, vec![step.clone()], 1).unwrap();
        }
        assert!(matches!(
            s.save("d", vec![step.clone()], 1),
            Err(FlowError::Invalid(_))
        ));
        // Replacing an existing name is not a new flow.
        s.save("a", vec![step], 2).unwrap();
        let _ = std::fs::remove_file(s.path());
    }

    #[test]
    fn a_file_from_a_newer_format_is_refused_by_name() {
        let e = parse_flows(r#"{"x":{"name":"x","version":99,"steps":[],"updated_ms":0}}"#)
            .unwrap_err();
        assert!(e.contains("'x'") && e.contains("99"), "{e}");
        assert!(parse_flows("not json").is_err());
    }

    #[test]
    fn the_serialised_step_is_compact() {
        let mut s = Step::bare(Op::Shortcut, 3);
        s.combo = Some("cmd+s".into());
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"op":"shortcut","combo":"cmd+s","at_ms":3}"#
        );
    }

    #[test]
    fn missing_secrets_are_named() {
        let mut a = Step::bare(Op::Type, 0);
        a.secret_ref = Some("pw".into());
        let mut b = Step::bare(Op::Type, 0);
        b.secret_ref = Some("otp".into());
        let supplied: BTreeMap<String, String> = [("pw".to_string(), "x".to_string())].into();
        assert_eq!(missing_secrets(&[a, b], &supplied), vec!["otp".to_string()]);
    }
}
