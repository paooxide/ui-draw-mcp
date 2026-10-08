use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::{
    CallCtx, Category, Envelope, ErrorCode, ImageContent, Tier, ToolDescriptor, ToolError,
    ToolModule,
};
use serde_json::{json, Value};

use crate::backend::{BrowserBackend, BrowserError};
use crate::cdp::DialogPolicy;

/// The `browser` CDP engine: DOM-level control of a
/// Chromium browser attached over the Chrome DevTools Protocol.
pub struct BrowserModule {
    backend: Arc<dyn BrowserBackend>,
    flows: Option<crate::flow::FlowStore>,
    baselines: Option<crate::visual::VisualStore>,
    profiles: Option<crate::profile::ProfileStore>,
    judge: Option<Arc<mcp_judge::Judge>>,
    showcase: std::sync::Mutex<crate::showcase::ShowcaseConfig>,
    /// Where saved screenshots and screencasts are written; never caller-chosen.
    media_dir: Option<std::path::PathBuf>,
    /// Jails a path `browser_upload` was asked to attach; `None` refuses uploads.
    upload_resolver: Option<UploadResolver>,
}

/// Resolves a caller-supplied path to the real, contained one, or says why not.
pub type UploadResolver = Arc<dyn Fn(&str) -> Result<std::path::PathBuf, String> + Send + Sync>;

impl BrowserModule {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        BrowserModule {
            backend,
            flows: None,
            baselines: None,
            profiles: None,
            judge: None,
            showcase: std::sync::Mutex::new(crate::showcase::ShowcaseConfig::default()),
            media_dir: None,
            upload_resolver: None,
        }
    }

    /// Enable `browser_upload`. Every path goes through `resolve` (the
    /// filesystem jail: `fs.roots` and the credential deny-list), and only the
    /// path it returns is ever handed to the browser.
    pub fn with_upload_resolver(mut self, resolve: UploadResolver) -> Self {
        self.upload_resolver = Some(resolve);
        self
    }

    /// Enable `browser_screenshot save` and `browser_screencast`: files are
    /// written under this agentctl-owned directory, with generated names.
    pub fn with_media_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.media_dir = Some(dir);
        self
    }

    /// Enable `browser_flow` (save/replay UI tests) backed by a JSON file.
    pub fn with_flow_store(mut self, store: crate::flow::FlowStore) -> Self {
        self.flows = Some(store);
        self
    }

    /// Enable the `visual` assert clause (baseline screenshot + pixel diff).
    pub fn with_visual_store(mut self, store: crate::visual::VisualStore) -> Self {
        self.baselines = Some(store);
        self
    }

    /// Enable `browser_profile` (save/restore session states) backed by a JSON file.
    pub fn with_profile_store(mut self, store: crate::profile::ProfileStore) -> Self {
        self.profiles = Some(store);
        self
    }

    /// Enable the `ux` assert clause (judge-scored heuristic review; advisory).
    pub fn with_judge(mut self, judge: Arc<mcp_judge::Judge>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Enable showcase mode (animated SVG pointer, gliding, ripples, typing HUD).
    pub fn with_showcase(self, config: crate::showcase::ShowcaseConfig) -> Self {
        *self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
        self
    }
}

/// Assert clauses evaluated in the page by the backend. `visual` and `ux` are
/// handled at the module layer instead (they need the baseline store / judge).
const JS_ASSERT_KEYS: &[&str] = &[
    "text",
    "not_text",
    "url",
    "selector",
    "no_console_errors",
    "no_failed_requests",
    "a11y",
    "style",
    "component",
];

/// A per-dimension UX instruction for the judge. Known dimensions get a focused
/// prompt; an unknown one gets a sensible generic prompt so callers can add
/// their own without a code change.
fn ux_instruction(dim: &str) -> String {
    match dim {
        "clarity" => {
            "The screen's purpose and its primary action are immediately clear to a first-time user."
        }
        "hierarchy" => {
            "The visual hierarchy guides the eye: the most important element stands out and the \
             grouping of related content is logical."
        }
        "affordance" => {
            "Interactive elements clearly look interactive and their labels say what they will do."
        }
        "consistency" => {
            "Labels, terminology and controls are consistent with each other and with common \
             platform conventions."
        }
        other => return format!("From a UX standpoint, the screen exhibits good {other}."),
    }
    .to_string()
}

/// Gather the page facts the UX judge reasons over: title, url, headings,
/// action labels, field names and a bounded slice of visible text.
const UX_FACTS_JS: &str = r#"(function(){
  function txt(el){ return ((el&&el.innerText)||'').trim().replace(/\s+/g,' '); }
  var main=document.querySelector('main')||document.body;
  var h=[].slice.call(main.querySelectorAll('h1,h2,h3')).map(txt).filter(Boolean).slice(0,20);
  var a=[].slice.call(main.querySelectorAll('button,a[href],[role=button]')).map(function(b){return (b.getAttribute('aria-label')||txt(b));}).filter(Boolean).slice(0,30);
  var f=[].slice.call(main.querySelectorAll('input,select,textarea')).map(function(i){return i.getAttribute('placeholder')||i.getAttribute('name')||i.getAttribute('type')||'field';}).slice(0,30);
  return { title: document.title, url: location.href, headings: h, actions: a, fields: f, text: txt(main).slice(0,2000) };
})()"#;

/// Decode two base64 PNGs, compare them pixel-for-pixel on a canvas, and return
/// the changed-pixel ratio plus a bounding box. `__BASE__`/`__CUR__` are
/// replaced with base64 (the base64 alphabet has no quotes, so single-quoting
/// is safe). A small per-pixel threshold ignores antialiasing noise.
const VISUAL_DIFF_JS: &str = r#"(async function(){
  var A='__BASE__', B='__CUR__';
  function load(src){ return new Promise(function(res,rej){ var im=new Image(); im.onload=function(){res(im);}; im.onerror=function(){rej(new Error('decode failed'));}; im.src='data:image/png;base64,'+src; }); }
  var ia, ib;
  try{ ia=await load(A); ib=await load(B); }catch(e){ return {error:String(e&&e.message||e)}; }
  if(ia.width!==ib.width||ia.height!==ib.height){ return {dims_match:false, base:ia.width+'x'+ia.height, cur:ib.width+'x'+ib.height, diff_ratio:1}; }
  var w=ia.width, h=ia.height;
  function ctx(){ try{ var c=new OffscreenCanvas(w,h); return c.getContext('2d',{willReadFrequently:true}); }catch(e){ var cv=document.createElement('canvas'); cv.width=w; cv.height=h; return cv.getContext('2d',{willReadFrequently:true}); } }
  var xa=ctx(), xb=ctx();
  xa.drawImage(ia,0,0); xb.drawImage(ib,0,0);
  var da=xa.getImageData(0,0,w,h).data, db=xb.getImageData(0,0,w,h).data;
  var thr=16, changed=0, minx=w, miny=h, maxx=-1, maxy=-1;
  for(var i=0;i<da.length;i+=4){ var d=Math.abs(da[i]-db[i])+Math.abs(da[i+1]-db[i+1])+Math.abs(da[i+2]-db[i+2])+Math.abs(da[i+3]-db[i+3]); if(d>thr){ changed++; var p=i/4, px=p%w, py=(p/w)|0; if(px<minx)minx=px; if(px>maxx)maxx=px; if(py<miny)miny=py; if(py>maxy)maxy=py; } }
  var total=w*h;
  return { dims_match:true, w:w, h:h, changed:changed, total:total, diff_ratio: total? changed/total : 0, bbox: (maxx>=0)? {x:minx,y:miny,w:maxx-minx+1,h:maxy-miny+1} : null };
})()"#;

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

// Envelope is intentionally large (carries an optional image); it is the Err
// type here only as a control-flow shortcut for missing args.
#[allow(clippy::result_large_err)]
fn require<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, Envelope> {
    str_arg(args, key)
        .ok_or_else(|| Envelope::fail(tool, ErrorCode::InvalidArgs, format!("missing '{key}'")))
}

fn browser_err(tool: &str, e: BrowserError) -> Envelope {
    let (code, msg) = match e {
        BrowserError::PermissionDenied(m) => (ErrorCode::PermDenied, m),
        BrowserError::NotFound(m) => (ErrorCode::NotFound, m),
        BrowserError::Unsupported(m) => (ErrorCode::UnsupportedOs, m),
        BrowserError::Timeout(m) => (ErrorCode::Timeout, m),
        BrowserError::Failed(m) => (ErrorCode::ActionFailed, m),
    };
    Envelope::fail(tool, code, msg)
}

fn result(tool: &str, r: Result<Value, BrowserError>) -> Envelope {
    match r {
        Ok(v) => Envelope::ok(tool, v),
        Err(e) => browser_err(tool, e),
    }
}

/// A connected browser and the ids of its page tabs, active one first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BrowserTabs {
    pub id: u32,
    pub tabs: Vec<String>,
}

/// What an omitted or browser-id `target_id` resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetPick {
    /// The caller named a tab; use it as given.
    Given(String),
    /// A default: the active tab of this browser.
    Active { target: String, browser_id: u32 },
}

/// Why no tab could be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TargetError {
    NoBrowser,
    Ambiguous(Vec<u32>),
    NoTabs(u32),
    Unknown(String, Vec<u32>),
}

impl TargetError {
    fn into_envelope(self, tool: &str) -> Envelope {
        let ids = |v: &[u32]| v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ");
        match self {
            TargetError::NoBrowser => Envelope::fail_with(
                tool,
                ErrorCode::NotFound,
                "no browser is connected, so there is no tab to use",
                "call browser_connect first; its result lists the tabs",
            ),
            TargetError::Ambiguous(v) => Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!(
                    "missing 'target_id', and several browsers are connected (browser_id {})",
                    ids(&v)
                ),
                "pass target_id: a tab id from browser_tabs, or a browser_id to use that browser's active tab",
            ),
            TargetError::NoTabs(b) => Envelope::fail_with(
                tool,
                ErrorCode::NotFound,
                format!("browser {b} has no open page tab"),
                "open one with browser_tabs action 'open'",
            ),
            TargetError::Unknown(g, v) => Envelope::fail_with(
                tool,
                ErrorCode::NotFound,
                format!(
                    "target '{g}' is neither a tab id nor a connected browser_id (connected: {})",
                    ids(&v)
                ),
                "pass a target_id from browser_connect or browser_tabs, or omit it to use the active tab",
            ),
        }
    }
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The `target_id` argument as text. A model that has only seen
/// `browser_id: 1` sends `1`, as a string or as a number; empty means omitted.
fn target_arg(args: &Value) -> Option<String> {
    match args.get("target_id")? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Decide which tab a tab-scoped call acts on. A tab id is used as given (the
/// backend says if it is wrong). All digits is a `browser_id` unless it really
/// is a tab id: that browser's active tab. Omitted is the active tab of the
/// only connected browser; with none or several there is nothing to guess.
pub(crate) fn pick_target(
    given: Option<&str>,
    browsers: &[BrowserTabs],
) -> Result<TargetPick, TargetError> {
    let active = |b: &BrowserTabs| {
        b.tabs
            .first()
            .map(|t| TargetPick::Active {
                target: t.clone(),
                browser_id: b.id,
            })
            .ok_or(TargetError::NoTabs(b.id))
    };
    let ids = || browsers.iter().map(|b| b.id).collect::<Vec<_>>();
    match given {
        Some(g) if !all_digits(g) => Ok(TargetPick::Given(g.to_string())),
        Some(g) => {
            if browsers.iter().any(|b| b.tabs.iter().any(|t| t == g)) {
                return Ok(TargetPick::Given(g.to_string()));
            }
            match browsers.iter().find(|b| g.parse::<u32>() == Ok(b.id)) {
                Some(b) => active(b),
                None => Err(TargetError::Unknown(g.to_string(), ids())),
            }
        }
        None => match browsers {
            [] => Err(TargetError::NoBrowser),
            [only] => active(only),
            _ => Err(TargetError::Ambiguous(ids())),
        },
    }
}

/// Whether this call is one whose `target_id` may be left out. Tools that
/// fork tabs (`browser_branch`), list across tabs, or stop by recording id
/// keep their own rules.
fn defaults_target(name: &str, args: &Value) -> bool {
    let action = str_arg(args, "action");
    match name {
        "browser_navigate" | "browser_snapshot" | "browser_query" | "browser_act"
        | "browser_upload" | "browser_fill_form" | "browser_extract" | "browser_wait"
        | "browser_challenge" | "browser_record" | "browser_screenshot" | "browser_viewport"
        | "browser_eval" | "browser_dialog" | "browser_network" | "browser_cookies"
        | "browser_capture" | "browser_assert" | "browser_showcase" => true,
        "browser_profile" => matches!(action, Some("save" | "restore")),
        "browser_checkpoint" => matches!(action.unwrap_or("save"), "save" | "rollback" | "delete"),
        "browser_flow" => action == Some("run"),
        "browser_screencast" => action == Some("start"),
        _ => false,
    }
}

/// Drop the backslash an over-escaping caller left before a quote. `\"` and
/// `\'` are never valid XPath (CSS does accept them, so selectors are not
/// passed through this).
pub(crate) fn unescape_xpath_quotes(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains("\\\"") && !s.contains("\\'") {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some('"' | '\'')) {
            continue;
        }
        out.push(c);
    }
    std::borrow::Cow::Owned(out)
}

/// A selector that starts like an XPath, the test `within` uses in the page.
fn looks_like_xpath(s: &str) -> bool {
    matches!(s.chars().next(), Some('/' | '('))
}

fn unescape_field(obj: &mut Value, key: &str) {
    if let Some(Value::String(s)) = obj.get(key) {
        if let std::borrow::Cow::Owned(fixed) = unescape_xpath_quotes(s) {
            obj[key] = Value::String(fixed);
        }
    }
}

/// Apply [`unescape_xpath_quotes`] to every argument that is an XPath: a
/// `ref`, a `query` with `by: "xpath"`, an XPath-looking `within`, and the
/// same in `fill_form`'s fields and submit.
fn fix_xpath_args(args: &mut Value) {
    fn one(o: &mut Value, selector_key: &str) {
        unescape_field(o, "ref");
        let xpath = match (str_arg(o, "by"), str_arg(o, selector_key)) {
            (Some(by), _) => by == "xpath",
            (None, Some(s)) => selector_key == "selector" && looks_like_xpath(s),
            _ => false,
        };
        if xpath {
            unescape_field(o, selector_key);
        }
    }
    if !args.is_object() {
        return;
    }
    one(args, "query");
    if str_arg(args, "within").is_some_and(looks_like_xpath) {
        unescape_field(args, "within");
    }
    if let Some(fields) = args.get_mut("fields").and_then(Value::as_array_mut) {
        for f in fields.iter_mut().filter(|f| f.is_object()) {
            one(f, "selector");
        }
    }
    if let Some(submit) = args.get_mut("submit").filter(|s| s.is_object()) {
        one(submit, "selector");
    }
}

/// A locator spelling that means something other than the field it was put in,
/// rewritten to the `by` / `query` (and `text` filter) that mean what it meant.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Respelled {
    pub by: &'static str,
    pub query: String,
    /// The text a trailing `:has-text("X")` and friends asked for.
    pub text: Option<String>,
}

/// `s` without one pair of quotes around it.
fn unquote(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// Whether `s` is wholly one quoted string.
fn is_quoted(s: &str) -> bool {
    s.len() > 1 && unquote(s).len() + 2 == s.len()
}

/// Pseudo-classes that make `text:foo` a CSS selector for an SVG `<text>`.
const REAL_PSEUDOS: [&str; 24] = [
    "hover",
    "focus",
    "active",
    "visited",
    "link",
    "checked",
    "disabled",
    "enabled",
    "empty",
    "root",
    "target",
    "required",
    "optional",
    "first-child",
    "last-child",
    "only-child",
    "first-of-type",
    "last-of-type",
    "only-of-type",
    "nth-child(",
    "nth-of-type(",
    "not(",
    "is(",
    "has(",
];

/// The text in `text=X`, `text:X`, `text("X")` and `text "X"`.
fn text_spelling(q: &str) -> Option<String> {
    let x = if let Some(r) = q.strip_prefix("text=") {
        unquote(r)
    } else if let Some(r) = q.strip_prefix("text:") {
        let lower = r.trim_start().to_ascii_lowercase();
        if REAL_PSEUDOS.iter().any(|p| lower.starts_with(p)) {
            return None;
        }
        unquote(r)
    } else if let Some(r) = q.strip_prefix("text(").and_then(|r| r.strip_suffix(')')) {
        is_quoted(r.trim()).then(|| unquote(r))?
    } else {
        let r = q
            .strip_prefix("text")
            .filter(|r| r.starts_with(char::is_whitespace))?;
        is_quoted(r.trim()).then(|| unquote(r))?
    };
    (!x.is_empty()).then(|| x.to_string())
}

/// `div[1]/div[2]/div[4]`: a snapshot ref (`/html/body/div[1]/...`) that lost
/// its front, as the XPath it was cut from.
fn ref_fragment(q: &str) -> Option<String> {
    let step_ok = |s: &str| {
        let (tag, idx) = match s.split_once('[') {
            Some((t, i)) => (t, Some(i.strip_suffix(']').unwrap_or("x"))),
            None => (s, None),
        };
        tag.starts_with(|c: char| c.is_ascii_alphabetic())
            && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && idx.map_or(true, |i| {
                !i.is_empty() && i.chars().all(|c| c.is_ascii_digit())
            })
    };
    let steps: Vec<&str> = q.split('/').collect();
    if steps.len() < 2 || !steps.iter().all(|s| step_ok(s)) {
        return None;
    }
    Some(match steps[0] {
        "html" => format!("/{q}"),
        "body" => format!("/html/{q}"),
        _ => format!("/html/body/{q}"),
    })
}

/// A CSS selector ending in `:has-text("X")`, `:contains("X")`, `:text("X")` or
/// `:text-is("X")`, split into the CSS before it and X. `None` when the shape
/// cannot be rewritten without changing what it selects: the pseudo is not
/// last, there is a selector list, or the base ends in a combinator.
fn trailing_text_pseudo(q: &str) -> Option<(&str, String)> {
    const PSEUDOS: [&str; 4] = [":has-text(", ":contains(", ":text-is(", ":text("];
    let body = q.strip_suffix(')')?;
    let (at, p) = PSEUDOS
        .iter()
        .filter_map(|p| body.rfind(p).map(|i| (i, *p)))
        .max_by_key(|(i, _)| *i)?;
    let arg = body[at + p.len()..].trim();
    let x = unquote(arg);
    let bad_arg = if is_quoted(arg) {
        x.contains(arg.chars().next()?)
    } else {
        x.contains(['(', ')', '"', '\''])
    };
    if x.is_empty() || bad_arg {
        return None;
    }
    let raw_base = &body[..at];
    // `div :contains(..)` filters div's descendants; the base alone would be div.
    if raw_base.ends_with(char::is_whitespace) {
        return None;
    }
    let (mut brackets, mut parens, mut quote) = (0i32, 0i32, None);
    for c in raw_base.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[') => brackets += 1,
            (None, ']') => brackets -= 1,
            (None, '(') => parens += 1,
            (None, ')') => parens -= 1,
            (None, ',') if brackets == 0 && parens == 0 => return None,
            _ => {}
        }
    }
    let open = quote.is_some() || brackets != 0 || parens != 0;
    if open || raw_base.ends_with(['>', '+', '~']) || jquery_pseudo(raw_base).is_some() {
        return None;
    }
    Some((raw_base, x.to_string()))
}

/// The other spellings of a locator that a model reaches for, as the `by` and
/// `query` they mean (see [`Respelled`]); `None` when the query stands as it
/// is. Only an unset `by` or `css` is respelled: `text` and `xpath` are taken
/// at their word.
pub(crate) fn respell_locator(by: Option<&str>, query: &str) -> Option<Respelled> {
    if !matches!(by, None | Some("css")) {
        return None;
    }
    let q = query.trim();
    let plain = |by: &'static str, query: &str| {
        Some(Respelled {
            by,
            query: query.to_string(),
            text: None,
        })
    };
    let css = |q: &str| match trailing_text_pseudo(q) {
        Some(("", x)) => plain("text", &x),
        Some((base, x)) => Some(Respelled {
            by: "css",
            query: base.to_string(),
            text: Some(x),
        }),
        None => None,
    };
    if looks_like_xpath(q) {
        return plain("xpath", q);
    }
    if let Some(r) = q.strip_prefix("xpath=") {
        return plain("xpath", r.trim());
    }
    if let Some(r) = q.strip_prefix("css=") {
        return css(r.trim()).or_else(|| plain("css", r.trim()));
    }
    if let Some(x) = text_spelling(q) {
        return plain("text", &x);
    }
    if let Some(xp) = ref_fragment(q) {
        return plain("xpath", &xp);
    }
    css(q)
}

/// [`respell_locator`] for a scope (`within`, a snapshot's `root_selector`),
/// which is CSS or XPath and has no text filter.
pub(crate) fn respell_scope(s: &str) -> Option<String> {
    let r = respell_locator(None, s)?;
    (r.text.is_none() && matches!(r.by, "css" | "xpath")).then_some(r.query)
}

/// Respell the locators in a call's arguments (see [`respell_locator`]).
fn respell_args(name: &str, args: &mut Value) {
    fn one(o: &mut Value, key: &str) {
        let Some(q) = str_arg(o, key) else { return };
        let Some(r) = respell_locator(str_arg(o, "by"), q) else {
            return;
        };
        // A filter the caller already gave stays theirs; the pseudo is left for
        // the selector hint to explain.
        if r.text.is_some() && str_arg(o, "text").is_some() {
            return;
        }
        o["by"] = json!(r.by);
        o[key] = json!(r.query);
        if let Some(t) = r.text {
            o["text"] = json!(t);
        }
    }
    if !args.is_object() {
        return;
    }
    match name {
        "browser_act" | "browser_query" | "browser_upload" => {
            one(args, "query");
            if let Some(w) = str_arg(args, "within").and_then(respell_scope) {
                args["within"] = json!(w);
            }
        }
        "browser_snapshot" => {
            if let Some(r) = str_arg(args, "root_selector").and_then(respell_scope) {
                args["root_selector"] = json!(r);
            }
        }
        "browser_fill_form" => {
            if let Some(fields) = args.get_mut("fields").and_then(Value::as_array_mut) {
                for f in fields.iter_mut().filter(|f| f.is_object()) {
                    one(f, "selector");
                }
            }
            if let Some(submit) = args.get_mut("submit").filter(|s| s.is_object()) {
                one(submit, "selector");
            }
        }
        _ => {}
    }
}

/// The jQuery / Playwright pseudo-class a CSS selector uses, if any: none of
/// these exist in CSS. `:first` and `:last` are not `:first-child` and friends.
pub(crate) fn jquery_pseudo(selector: &str) -> Option<&'static str> {
    const WITH_ARGS: [&str; 4] = [":contains(", ":has-text(", ":text(", ":eq("];
    const BARE: [&str; 3] = [":visible", ":first", ":last"];
    let word = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    if let Some(p) = WITH_ARGS.into_iter().find(|p| selector.contains(p)) {
        return Some(p);
    }
    BARE.into_iter().find(|p| {
        selector
            .match_indices(p)
            .any(|(i, _)| !selector[i + p.len()..].chars().next().is_some_and(word))
    })
}

/// A browser's complaint that a CSS selector does not parse.
fn is_selector_syntax_error(msg: &str) -> bool {
    msg.contains("valid selector") || msg.contains("SyntaxError")
}

/// [`result`], except that a CSS selector the browser could not parse, and
/// that uses a jQuery or Playwright pseudo-class, says what to use instead.
/// The browser's own text stays in the message.
fn result_with_selector_hint(tool: &str, r: Result<Value, BrowserError>, css: &[&str]) -> Envelope {
    if let Err(e) = &r {
        let msg = browser_err_msg(e);
        let pseudo = css.iter().find_map(|s| jquery_pseudo(s));
        // With no `by` a selector the browser cannot parse is not an error but a
        // text search, so the pseudo-class shows up as a miss instead.
        let unparsed = is_selector_syntax_error(&msg) || msg.starts_with("no element matches");
        if let (true, Some(p)) = (unparsed, pseudo) {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                msg,
                format!(
                    "'{p}' is a jQuery/Playwright pseudo-class, not CSS; to find an element by its text use by: \"text\" with the text as the query (browser_act also has a 'text' filter to narrow a CSS match)"
                ),
            );
        }
    }
    result(tool, r)
}

/// The CSS selectors among `fill_form`'s fields and submit.
fn fill_css_selectors(args: &Value) -> Vec<&str> {
    fn css(f: &Value) -> Option<&str> {
        let s = f.get("selector")?.as_str()?;
        matches!(str_arg(f, "by"), None | Some("css")).then_some(s)
    }
    let mut out: Vec<&str> = args
        .get("fields")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(css).collect())
        .unwrap_or_default();
    out.extend(args.get("submit").and_then(css));
    out
}

impl BrowserModule {
    /// Resolve a tab-scoped call's `target_id` (see [`pick_target`]). `None`
    /// when the caller's tab id stands as it is; otherwise the tab to use and
    /// whether it was a default.
    async fn resolve_target(&self, args: &Value) -> Result<Option<(String, bool)>, TargetError> {
        let given = target_arg(args);
        if given.as_deref().is_some_and(|g| !all_digits(g)) {
            return Ok(None);
        }
        let mut browsers = Vec::new();
        for id in self.backend.browser_ids().await {
            let tabs = match self.backend.tabs(id, "list", None, None).await {
                Ok(v) => v
                    .get("tabs")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.get("target_id").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            browsers.push(BrowserTabs { id, tabs });
        }
        match pick_target(given.as_deref(), &browsers)? {
            TargetPick::Given(t) => Ok(Some((t, false))),
            TargetPick::Active { target, .. } => Ok(Some((target, true))),
        }
    }

    async fn connect(&self, args: &Value) -> Envelope {
        let attach_port = args
            .get("attach")
            .and_then(|a| a.get("port"))
            .and_then(Value::as_u64)
            .map(|p| p as u16);
        let launch = args.get("launch").cloned();
        let profile_name = launch
            .as_ref()
            .and_then(|l| l.get("profile"))
            .and_then(Value::as_str)
            .or_else(|| str_arg(args, "profile"))
            .map(|s| s.to_string());

        // Resolve the profile *before* connecting: an unknown name must fail
        // without launching (and leaking) a browser, and it must fail loudly
        // rather than connect without the session the caller asked for.
        let profile = match profile_name {
            None => None,
            Some(ref pname) => {
                let Some(ref store) = self.profiles else {
                    return Envelope::fail(
                        "browser_connect",
                        ErrorCode::InvalidArgs,
                        format!(
                            "profile '{pname}' requested but browser profiles are not enabled \
                             (no profile store configured)"
                        ),
                    );
                };
                match store.get(pname) {
                    Ok(Some(p)) => Some(p),
                    Ok(None) => {
                        return Envelope::fail(
                            "browser_connect",
                            ErrorCode::NotFound,
                            format!("profile '{pname}' not found"),
                        )
                    }
                    Err(crate::profile::ProfileError::Io(m)) => {
                        return Envelope::fail("browser_connect", ErrorCode::ActionFailed, m)
                    }
                    Err(crate::profile::ProfileError::Invalid(m)) => {
                        return Envelope::fail("browser_connect", ErrorCode::InvalidArgs, m)
                    }
                }
            }
        };

        let res = self.backend.connect(attach_port, launch).await;
        match res {
            Ok(mut val) => {
                if let Some(prof) = profile {
                    let browser_id =
                        val.get("browser_id").and_then(Value::as_u64).unwrap_or(1) as u32;
                    let outcome = self.restore_profile_onto_first_tab(browser_id, &prof).await;
                    if let Some(map) = val.as_object_mut() {
                        match outcome {
                            Ok(()) => {
                                map.insert("profile_restored".into(), json!(prof.name));
                            }
                            Err(msg) => {
                                // The browser is connected, so this is not a
                                // failed call; but it must not claim success.
                                map.insert("profile_restored".into(), json!(false));
                                map.insert("profile_restore_error".into(), json!(msg));
                            }
                        }
                    }
                }
                Envelope::ok("browser_connect", val)
            }
            Err(e) => browser_err("browser_connect", e),
        }
    }

    /// Put a saved profile onto the first tab of a freshly connected browser.
    ///
    /// Order matters: cookies and web storage are scoped to an origin, and a
    /// new tab is `about:blank`, where storage writes throw. So go to the
    /// profile's saved URL first, wait for that document to finish loading,
    /// and only then restore. Returns the reason on any failure.
    async fn restore_profile_onto_first_tab(
        &self,
        browser_id: u32,
        prof: &crate::profile::Profile,
    ) -> Result<(), String> {
        let tabs = self
            .backend
            .tabs(browser_id, "list", None, None)
            .await
            .map_err(|e| format!("could not list tabs: {}", browser_err_msg(&e)))?;
        let first = tabs
            .get("tabs")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .ok_or_else(|| "browser has no tab to restore the profile onto".to_string())?;
        let target_id = first
            .get("target_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "first tab has no target_id".to_string())?;
        let before = first.get("url").and_then(Value::as_str).unwrap_or("");

        let dest = prof
            .url
            .as_deref()
            .filter(|u| !u.is_empty() && *u != "about:blank");
        if let Some(dest) = dest {
            self.backend
                .navigate(target_id, "goto", Some(dest))
                .await
                .map_err(|e| format!("could not open {dest}: {}", browser_err_msg(&e)))?;
            self.wait_page_loaded(target_id, before, dest).await?;
        }

        let state = json!({
            "cookies": prof.cookies,
            "localStorage": prof.local_storage,
            "sessionStorage": prof.session_storage,
        });
        let res = self
            .backend
            .profile_restore(target_id, &state)
            .await
            .map_err(|e| format!("restore failed: {}", browser_err_msg(&e)))?;
        if res.get("ok").and_then(Value::as_bool) == Some(false) {
            let why = res
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("storage restore failed");
            let hint = if dest.is_none() {
                " (the profile has no saved URL, so web storage has no page to live on)"
            } else {
                ""
            };
            return Err(format!("restore failed: {why}{hint}"));
        }
        Ok(())
    }

    /// Block until `target` shows a finished document other than `before`
    /// (the pre-navigation URL), or is already at `dest`. Polls the page: the
    /// old document still reports `complete` for a moment after navigating.
    async fn wait_page_loaded(&self, target: &str, before: &str, dest: &str) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Ok(v) = self
                .backend
                .eval(target, "location.href + '|' + document.readyState")
                .await
            {
                let probe = v.get("result").and_then(Value::as_str).unwrap_or("");
                if let Some((href, state)) = probe.rsplit_once('|') {
                    let moved = href != before
                        || href == dest
                        || before.trim_end_matches('/') == dest.trim_end_matches('/');
                    if state == "complete" && moved && href != "about:blank" {
                        return Ok(());
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("{dest} did not finish loading within 15s"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    async fn disconnect(&self, args: &Value) -> Envelope {
        let Some(browser_id) = args.get("browser_id").and_then(Value::as_u64) else {
            return Envelope::fail(
                "browser_disconnect",
                ErrorCode::InvalidArgs,
                "missing 'browser_id'",
            );
        };
        let kill = args.get("kill").and_then(Value::as_bool).unwrap_or(false);
        result(
            "browser_disconnect",
            self.backend.disconnect(browser_id as u32, kill).await,
        )
    }

    async fn tabs(&self, args: &Value) -> Envelope {
        let Some(browser_id) = args.get("browser_id").and_then(Value::as_u64) else {
            return Envelope::fail(
                "browser_tabs",
                ErrorCode::InvalidArgs,
                "missing 'browser_id'",
            );
        };
        let action = str_arg(args, "action").unwrap_or("list");
        let target = str_arg(args, "target_id");
        let url = str_arg(args, "url");
        result(
            "browser_tabs",
            self.backend
                .tabs(browser_id as u32, action, target, url)
                .await,
        )
    }

    async fn navigate(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_navigate") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("goto");
        result(
            "browser_navigate",
            self.backend
                .navigate(target, action, str_arg(args, "url"))
                .await,
        )
    }

    async fn snapshot(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_snapshot") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let mode = str_arg(args, "mode").unwrap_or("dom");
        result(
            "browser_snapshot",
            self.backend
                .snapshot(target, mode, str_arg(args, "root_selector"))
                .await,
        )
    }

    async fn query(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_query") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let by = str_arg(args, "by").unwrap_or("auto");
        let q = match require(args, "query", "browser_query") {
            Ok(q) => q,
            Err(e) => return e,
        };
        let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
        let css: Vec<&str> = css_candidate(args, "query").into_iter().collect();
        result_with_selector_hint(
            "browser_query",
            self.backend
                .query(target, by, q, all, str_arg(args, "text"))
                .await,
            &css,
        )
    }

    async fn act(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_act") {
            Ok(t) => t,
            Err(e) => return e,
        };
        // Either a ref from a prior snapshot/query, or a selector resolved in
        // the same call (one round trip instead of query-then-act).
        let action =
            match crate::input::canonical_action(str_arg(args, "action").unwrap_or("click")) {
                Ok(a) => a,
                Err(m) => return Envelope::fail("browser_act", ErrorCode::InvalidArgs, m),
            };
        let pointer = match pointer_args(args) {
            Ok(p) => p,
            Err(m) => return Envelope::fail("browser_act", ErrorCode::InvalidArgs, m),
        };
        // Real pointer input: the multi-click, scroll and drag actions, and a
        // click or hover given coordinates.
        let is_pointer = matches!(
            action,
            "double_click" | "triple_click" | "right_click" | "scroll" | "drag"
        ) || (matches!(action, "click" | "hover")
            && (pointer.x.is_some() || pointer.y.is_some()));
        // Typing and key presses may go to whatever has focus.
        let found = parse_locator(args);
        let locator = match found {
            Some(l) => l,
            None if matches!(action, "type" | "press") => crate::backend::Locator::Focused,
            None if is_pointer => crate::backend::Locator::Focused,
            None => {
                return Envelope::fail_with(
                    "browser_act",
                    ErrorCode::InvalidArgs,
                    "need 'ref' (from browser_query/snapshot) or 'query' (with optional 'by', 'within', 'text', 'index')",
                    "pass ref, or query plus by=css|xpath|text",
                )
            }
        };
        let secret = args.get("secret").and_then(Value::as_bool) == Some(true);
        let opts = match parse_act_opts(args) {
            Ok(o) => o,
            Err(m) => return Envelope::fail("browser_act", ErrorCode::InvalidArgs, m),
        };
        let mut css: Vec<&str> = css_candidate(args, "query").into_iter().collect();
        css.extend(str_arg(args, "within").filter(|w| !looks_like_xpath(w)));
        if is_pointer {
            return result_with_selector_hint(
                "browser_act",
                self.backend
                    .act_pointer(target, found, action, pointer, opts)
                    .await,
                &css,
            );
        }
        result_with_selector_hint(
            "browser_act",
            self.backend
                .act_opts(
                    target,
                    locator,
                    action,
                    str_arg(args, "value"),
                    secret,
                    opts,
                )
                .await,
            &css,
        )
    }

    async fn upload(&self, args: &Value) -> Envelope {
        let tool = "browser_upload";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let Some(locator) = parse_locator(args) else {
            return Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                "need 'ref' (from browser_query/snapshot) or 'query' (with optional 'by', 'within', 'text', 'index')",
                "pass ref, or query plus by=css|xpath|text",
            );
        };
        let paths = match upload_paths(args) {
            Ok(p) => p,
            Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        };
        let Some(resolve) = self.upload_resolver.as_ref() else {
            return Envelope::fail_with(
                tool,
                ErrorCode::PermDenied,
                "browser_upload reads local files, and no file roots are configured (fs.roots)",
                "the operator must set fs.roots in config.toml to the directories whose files may be uploaded",
            );
        };
        // Every path is resolved through the jail first, and only the
        // resolved path is used from here on; the caller's string is not.
        let mut files = Vec::with_capacity(paths.len());
        let mut listed = Vec::with_capacity(paths.len());
        for p in paths {
            let resolved = match resolve(p) {
                Ok(r) => r,
                Err(m) => return Envelope::fail(tool, ErrorCode::PermDenied, m),
            };
            let meta = match std::fs::metadata(&resolved) {
                Ok(m) => m,
                Err(e) => {
                    return Envelope::fail(
                        tool,
                        ErrorCode::NotFound,
                        format!("cannot read '{p}': {e}"),
                    )
                }
            };
            if let Err(m) = check_upload_file(p, meta.is_file(), meta.len()) {
                return Envelope::fail(tool, ErrorCode::InvalidArgs, m);
            }
            let name = resolved
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            listed.push(json!({ "name": name, "bytes": meta.len() }));
            files.push(resolved.to_string_lossy().into_owned());
        }
        let r = self.backend.upload(target, locator, &files).await;
        match r {
            Ok(mut v) => {
                if let Some(m) = v.as_object_mut() {
                    m.insert("count".into(), json!(listed.len()));
                    m.insert("files".into(), Value::Array(listed));
                }
                Envelope::ok(tool, v)
            }
            Err(e) => browser_err(tool, e),
        }
    }

    async fn fill_form(&self, args: &Value) -> Envelope {
        let tool = "browser_fill_form";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let Some(fields) = args.get("fields").and_then(Value::as_array) else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'fields' array");
        };
        if fields.is_empty() {
            return Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                "'fields' array must not be empty",
            );
        }
        let submit = args.get("submit");
        result_with_selector_hint(
            tool,
            self.backend
                .fill_form(target, &Value::Array(fields.clone()), submit)
                .await,
            &fill_css_selectors(args),
        )
    }

    async fn extract(&self, args: &Value) -> Envelope {
        let tool = "browser_extract";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let Some(schema) = args.get("schema") else {
            return Envelope::fail(tool, ErrorCode::InvalidArgs, "missing 'schema' object");
        };
        let within = str_arg(args, "within");
        result(tool, self.backend.extract(target, schema, within).await)
    }

    async fn profile(&self, args: &Value) -> Envelope {
        let tool = "browser_profile";
        let Some(store) = self.profiles.as_ref() else {
            return Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "browser_profile is not enabled (no profile store configured)",
                "run agentctl with a state dir so profiles can be saved",
            );
        };
        let action = str_arg(args, "action").unwrap_or("list");
        match action {
            "save" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "save needs 'name'");
                };
                let state = match self.backend.profile_state(target).await {
                    Ok(s) => s,
                    Err(e) => return browser_err(tool, e),
                };
                let cookies = state
                    .get("cookies")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let ls = state.get("localStorage").cloned().unwrap_or(json!({}));
                let ss = state.get("sessionStorage").cloned().unwrap_or(json!({}));
                let url = state
                    .get("url")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                match store.save(name, cookies, ls, ss, url, now_ms()) {
                    Ok(p) => Envelope::ok(
                        tool,
                        json!({
                            "saved": true,
                            "name": p.name,
                            "cookies_count": p.cookies.len(),
                            "local_storage_keys": p.local_storage.as_object().map(|o| o.len()).unwrap_or(0),
                            "session_storage_keys": p.session_storage.as_object().map(|o| o.len()).unwrap_or(0),
                            "url": p.url,
                        }),
                    ),
                    Err(crate::profile::ProfileError::Invalid(m)) => {
                        Envelope::fail(tool, ErrorCode::InvalidArgs, m)
                    }
                    Err(crate::profile::ProfileError::Io(m)) => {
                        Envelope::fail(tool, ErrorCode::ActionFailed, m)
                    }
                }
            }
            "restore" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "restore needs 'name'");
                };
                let profile = match store.get(name) {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        return Envelope::fail(
                            tool,
                            ErrorCode::NotFound,
                            format!("profile '{name}' not found"),
                        )
                    }
                    Err(crate::profile::ProfileError::Io(m)) => {
                        return Envelope::fail(tool, ErrorCode::ActionFailed, m)
                    }
                    Err(crate::profile::ProfileError::Invalid(m)) => {
                        return Envelope::fail(tool, ErrorCode::InvalidArgs, m)
                    }
                };
                let state = json!({
                    "cookies": profile.cookies,
                    "localStorage": profile.local_storage,
                    "sessionStorage": profile.session_storage,
                });
                match self.backend.profile_restore(target, &state).await {
                    Ok(res) if res.get("ok").and_then(Value::as_bool) == Some(false) => {
                        // The backend reports a failed storage write inside an
                        // otherwise successful reply; do not call that restored.
                        Envelope::fail(
                            tool,
                            ErrorCode::ActionFailed,
                            format!(
                                "restore of '{name}' failed: {}",
                                res.get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("storage restore failed")
                            ),
                        )
                    }
                    Ok(mut res) => {
                        if let Some(map) = res.as_object_mut() {
                            map.insert("restored".into(), json!(true));
                            map.insert("name".into(), json!(name));
                            Envelope::ok(tool, res)
                        } else {
                            Envelope::ok(
                                tool,
                                json!({ "ok": true, "name": name, "restored": true, "detail": res }),
                            )
                        }
                    }
                    Err(e) => browser_err(tool, e),
                }
            }
            "list" => match store.list() {
                Ok(ps) => {
                    let rows: Vec<Value> = ps
                        .iter()
                        .map(|p| {
                            json!({
                                "name": p.name,
                                "cookies_count": p.cookies_count,
                                "local_storage_count": p.local_storage_count,
                                "url": p.url,
                                "updated_ms": p.updated_ms,
                            })
                        })
                        .collect();
                    Envelope::ok(tool, json!({ "profiles": rows, "count": rows.len() }))
                }
                Err(crate::profile::ProfileError::Io(m)) => {
                    Envelope::fail(tool, ErrorCode::ActionFailed, m)
                }
                Err(crate::profile::ProfileError::Invalid(m)) => {
                    Envelope::fail(tool, ErrorCode::InvalidArgs, m)
                }
            },
            "delete" => {
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "delete needs 'name'");
                };
                match store.delete(name) {
                    Ok(true) => Envelope::ok(tool, json!({ "deleted": true, "name": name })),
                    Ok(false) => Envelope::fail(
                        tool,
                        ErrorCode::NotFound,
                        format!("profile '{name}' not found"),
                    ),
                    Err(crate::profile::ProfileError::Io(m)) => {
                        Envelope::fail(tool, ErrorCode::ActionFailed, m)
                    }
                    Err(crate::profile::ProfileError::Invalid(m)) => {
                        Envelope::fail(tool, ErrorCode::InvalidArgs, m)
                    }
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown action '{other}' (use save|restore|list|delete)"),
            ),
        }
    }

    async fn branch(&self, args: &Value) -> Envelope {
        let tool = "browser_branch";
        let action = str_arg(args, "action").unwrap_or("list");
        match action {
            "create" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let branch_id = match require(args, "branch_id", tool) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                result(tool, self.backend.branch_create(target, branch_id).await)
            }
            "commit" => {
                let branch_id = match require(args, "branch_id", tool) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                result(tool, self.backend.branch_commit(branch_id).await)
            }
            "discard" => {
                let branch_id = match require(args, "branch_id", tool) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                result(tool, self.backend.branch_discard(branch_id).await)
            }
            "switch" => {
                let branch_id = match require(args, "branch_id", tool) {
                    Ok(b) => b,
                    Err(e) => return e,
                };
                result(tool, self.backend.branch_switch(branch_id).await)
            }
            "list" => {
                let target = str_arg(args, "target_id");
                result(tool, self.backend.branch_list(target).await)
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!(
                    "unknown branch action '{other}'; use create, commit, discard, switch, or list"
                ),
            ),
        }
    }

    async fn checkpoint(&self, args: &Value) -> Envelope {
        let tool = "browser_checkpoint";
        let action = str_arg(args, "action").unwrap_or("save");
        match action {
            "save" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let tag = str_arg(args, "tag");
                result(tool, self.backend.checkpoint_save(target, tag).await)
            }
            "rollback" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let tag = str_arg(args, "tag");
                result(tool, self.backend.checkpoint_rollback(target, tag).await)
            }
            "list" => {
                let target = str_arg(args, "target_id");
                result(tool, self.backend.checkpoint_list(target).await)
            }
            "delete" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let tag = str_arg(args, "tag");
                result(tool, self.backend.checkpoint_delete(target, tag).await)
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown checkpoint action '{other}'; use save, rollback, list, or delete"),
            ),
        }
    }

    async fn wait(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_wait") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(10_000);
        // A duration and no condition is a plain sleep.
        if let Some(ms) = sleep_only_ms(args) {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
            return Envelope::ok("browser_wait", json!({ "waited_ms": ms }));
        }
        let (cond, arg) = match parse_wait_condition(args) {
            Ok(c) => c,
            Err(m) => return Envelope::fail("browser_wait", ErrorCode::InvalidArgs, m),
        };
        let nav_window = match args.get("navigation_timeout_ms") {
            None | Some(Value::Null) => None,
            Some(v) => match nav_window_arg(v) {
                Some(n) => Some(n),
                None => {
                    return Envelope::fail(
                        "browser_wait",
                        ErrorCode::InvalidArgs,
                        format!(
                            "navigation_timeout_ms must be an integer from 0 to {}",
                            crate::backend::NAV_EXPECT_MAX_MS
                        ),
                    )
                }
            },
        };
        if nav_window.is_some() && cond != "navigation" {
            return Envelope::fail(
                "browser_wait",
                ErrorCode::InvalidArgs,
                "navigation_timeout_ms only applies to the 'navigation' condition",
            );
        }
        result(
            "browser_wait",
            self.backend
                .wait_window(target, cond, arg, timeout_ms, nav_window)
                .await,
        )
    }

    async fn challenge(&self, args: &Value) -> Envelope {
        let tool = "browser_challenge";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("detect");
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000);

        match action {
            "detect" | "status" => {
                match crate::challenge::ChallengeManager::detect(self.backend.as_ref(), target)
                    .await
                {
                    Ok(st) => Envelope::ok(tool, json!(st)),
                    Err(e) => browser_err(tool, e),
                }
            }
            "wait" | "clear" => {
                match crate::challenge::ChallengeManager::wait_for_clearance(
                    self.backend.as_ref(),
                    target,
                    timeout_ms,
                )
                .await
                {
                    Ok(v) => Envelope::ok(tool, v),
                    Err(e) => browser_err(tool, e),
                }
            }
            "hud_show" => {
                let kind_str = str_arg(args, "kind").unwrap_or("unknown");
                let kind = crate::challenge::ChallengeKind::from_name(kind_str);
                match crate::challenge::ChallengeManager::inject_hud(
                    self.backend.as_ref(),
                    target,
                    &kind,
                )
                .await
                {
                    Ok(()) => Envelope::ok(tool, json!({ "hud": "visible", "kind": kind_str })),
                    Err(e) => browser_err(tool, e),
                }
            }
            "hud_hide" => {
                match crate::challenge::ChallengeManager::remove_hud(self.backend.as_ref(), target)
                    .await
                {
                    Ok(()) => Envelope::ok(tool, json!({ "hud": "removed" })),
                    Err(e) => browser_err(tool, e),
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!(
                    "unknown challenge action '{other}'; use detect, wait, hud_show, or hud_hide"
                ),
            ),
        }
    }

    async fn record(&self, args: &Value) -> Envelope {
        let tool = "browser_record";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("status");
        match action {
            "start" => {
                let dialogs = match str_arg(args, "dialogs") {
                    None => None,
                    Some(s) => match crate::cdp::RecordDialogs::parse(s) {
                        Some(d) => Some(d),
                        None => {
                            return Envelope::fail_with(
                                tool,
                                ErrorCode::InvalidArgs,
                                format!("unknown dialogs setting '{s}'"),
                                "use 'human' (the person answers), 'accept' or 'dismiss'",
                            )
                        }
                    },
                };
                match crate::record::RecordManager::start_with(
                    self.backend.as_ref(),
                    target,
                    dialogs,
                )
                .await
                {
                    Ok(v) => Envelope::ok(tool, v),
                    Err(e) => browser_err(tool, e),
                }
            }
            "status" => {
                match crate::record::RecordManager::status(self.backend.as_ref(), target).await {
                    Ok(v) => Envelope::ok(tool, v),
                    Err(e) => browser_err(tool, e),
                }
            }
            "stop" => {
                if let Some(name) = str_arg(args, "name") {
                    let Some(store) = self.flows.as_ref() else {
                        return Envelope::fail_with(
                            tool,
                            ErrorCode::UnsupportedOs,
                            "browser_record auto-save is not enabled (no flow store configured)",
                            "run agentctl with a state dir so flows can be saved",
                        );
                    };
                    match crate::record::RecordManager::stop_and_save(
                        self.backend.as_ref(),
                        store,
                        target,
                        name,
                    )
                    .await
                    {
                        Ok(f) => Envelope::ok(
                            tool,
                            json!({ "saved": true, "name": f.name, "steps": f.steps.len(), "flow": f }),
                        ),
                        Err(e) => flow_err(tool, e),
                    }
                } else {
                    match crate::record::RecordManager::stop(self.backend.as_ref(), target).await {
                        Ok(steps) => Envelope::ok(
                            tool,
                            json!({ "recording": false, "steps": steps, "count": steps.len() }),
                        ),
                        Err(e) => browser_err(tool, e),
                    }
                }
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown record action '{other}'; use start, stop, or status"),
            ),
        }
    }

    async fn showcase(&self, args: &Value) -> Envelope {
        let tool = "browser_showcase";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let mut cfg = self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(en) = args.get("enabled").and_then(Value::as_bool) {
            cfg.enabled = en;
            if en && matches!(cfg.speed, crate::showcase::ShowcaseSpeed::Off) {
                cfg.speed = crate::showcase::ShowcaseSpeed::Demo;
            }
        }
        if let Some(spd) = str_arg(args, "speed") {
            if let Some(s) = crate::showcase::ShowcaseSpeed::parse(spd) {
                cfg.speed = s;
                cfg.enabled = !matches!(s, crate::showcase::ShowcaseSpeed::Off);
            } else {
                return Envelope::fail(
                    tool,
                    ErrorCode::InvalidArgs,
                    "speed must be cinematic, demo, snappy, or off",
                );
            }
        }
        if let Some(r) = args.get("click_ripple").and_then(Value::as_bool) {
            cfg.click_ripple = r;
        }
        if let Some(h) = args.get("typing_hud").and_then(Value::as_bool) {
            cfg.typing_hud = h;
        }
        cfg.apply_visual_args(args);
        *self
            .showcase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cfg.clone();
        result(tool, self.backend.showcase(target, Some(cfg)).await)
    }

    async fn screenshot(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_screenshot") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let save = args.get("save").and_then(Value::as_bool).unwrap_or(false);
        let media = if save {
            match self.media_dir_for("browser_screenshot") {
                Ok(d) => Some(d),
                Err(e) => return e,
            }
        } else {
            None
        };
        match self.backend.screenshot(target, str_arg(args, "ref")).await {
            Ok(shot) if media.is_some() => result(
                "browser_screenshot",
                crate::screencast::save_screenshot(
                    media.as_deref().unwrap_or(std::path::Path::new("")),
                    &shot.base64,
                    shot.width,
                    shot.height,
                ),
            ),
            Ok(shot) => {
                // A whole-page capture does not measure itself; a 0x0 next to
                // the image reads as a blank page, so take the PNG's own size.
                let (width, height) = match (shot.width, shot.height) {
                    (0, _) | (_, 0) => {
                        crate::screencast::png_b64_size(&shot.base64).unwrap_or((0, 0))
                    }
                    wh => wh,
                };
                Envelope::ok_image(
                    "browser_screenshot",
                    json!({ "width": width, "height": height }),
                    ImageContent {
                        mime_type: "image/png".into(),
                        base64: shot.base64,
                    },
                )
            }
            Err(e) => browser_err("browser_screenshot", e),
        }
    }

    #[allow(clippy::result_large_err)]
    fn media_dir_for(&self, tool: &str) -> Result<std::path::PathBuf, Envelope> {
        self.media_dir.clone().ok_or_else(|| {
            browser_err(
                tool,
                BrowserError::Unsupported(
                    "no media directory is configured, so nothing can be saved to disk".into(),
                ),
            )
        })
    }

    async fn screencast(&self, args: &Value) -> Envelope {
        let tool = "browser_screencast";
        let int = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
        };
        match str_arg(args, "action").unwrap_or("status") {
            "start" => {
                let target = match require(args, "target_id", tool) {
                    Ok(t) => t,
                    Err(e) => return e,
                };
                let media = match self.media_dir_for(tool) {
                    Ok(d) => d,
                    Err(e) => return e,
                };
                let opts = crate::screencast::ScreencastOpts::from_args(
                    int("fps"),
                    int("quality"),
                    int("max_seconds"),
                );
                result(
                    tool,
                    self.backend.screencast_start(target, &media, opts).await,
                )
            }
            "stop" => {
                let (target, id) = (str_arg(args, "target_id"), str_arg(args, "recording_id"));
                if target.is_none() && id.is_none() {
                    return Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        "stop needs 'target_id' or 'recording_id'",
                    );
                }
                let keep = args
                    .get("keep_frames")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                result(tool, self.backend.screencast_stop(target, id, keep).await)
            }
            "status" => result(tool, self.backend.screencast_status().await),
            other => Envelope::fail_with(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown action '{other}'"),
                "use 'start', 'stop' or 'status'",
            ),
        }
    }

    async fn eval(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_eval") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let expr = match require(args, "expression", "browser_eval") {
            Ok(e) => e,
            Err(e) => return e,
        };
        let opts = crate::backend::EvalOptions {
            timeout_ms: args.get("timeout_ms").and_then(Value::as_u64),
            detached: args
                .get("detached")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        result(
            "browser_eval",
            self.backend.eval_with(target, expr, &opts).await,
        )
    }

    async fn dialog(&self, args: &Value) -> Envelope {
        let tool = "browser_dialog";
        let target = match require(args, "target_id", tool) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let policy = match str_arg(args, "policy") {
            None => None,
            Some("dismiss") => Some(DialogPolicy::Dismiss),
            Some("accept") => Some(DialogPolicy::Accept(
                str_arg(args, "prompt_text").map(str::to_string),
            )),
            Some(other) => {
                return Envelope::fail_with(
                    tool,
                    ErrorCode::InvalidArgs,
                    format!("unknown policy '{other}'"),
                    "use 'dismiss' (cancel the dialog) or 'accept' (confirm it)",
                )
            }
        };
        result(tool, self.backend.dialog(target, policy).await)
    }

    async fn network(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_network") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("log");
        result(
            "browser_network",
            self.backend
                .network(
                    target,
                    action,
                    str_arg(args, "filter"),
                    args.get("headers").cloned(),
                    args.get("duration_ms").and_then(Value::as_u64),
                )
                .await,
        )
    }

    async fn cookies(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_cookies") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("get");
        result(
            "browser_cookies",
            self.backend
                .cookies(target, action, args.get("cookie").cloned())
                .await,
        )
    }

    async fn capture(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_capture") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let action = str_arg(args, "action").unwrap_or("read");
        result(
            "browser_capture",
            self.backend.capture(target, action, args).await,
        )
    }

    async fn assert(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_assert") {
            Ok(t) => t,
            Err(e) => return e,
        };
        match self.assert_full(target, args).await {
            Ok(v) => {
                let passed = v.get("passed").and_then(Value::as_bool) == Some(true);
                if passed {
                    Envelope::ok("browser_assert", v)
                } else {
                    // A failed assertion is an error the harness must see, but
                    // the per-check detail rides along in `data`.
                    Envelope {
                        ok: false,
                        tool: "browser_assert".into(),
                        data: Some(v),
                        error: Some(ToolError {
                            code: ErrorCode::ActionFailed,
                            message: "assertion failed".into(),
                            suggestion: Some("see data.checks for which clause failed".into()),
                        }),
                        image: None,
                    }
                }
            }
            Err(e) => browser_err("browser_assert", e),
        }
    }

    async fn viewport(&self, args: &Value) -> Envelope {
        let target = match require(args, "target_id", "browser_viewport") {
            Ok(t) => t,
            Err(e) => return e,
        };
        let width = args.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
        let height = args.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
        let mobile = args.get("mobile").and_then(Value::as_bool).unwrap_or(false);
        let scale = args.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
        match self
            .backend
            .set_viewport(target, width, height, mobile, scale)
            .await
        {
            Ok(v) => Envelope::ok("browser_viewport", v),
            Err(e) => browser_err("browser_viewport", e),
        }
    }

    /// The whole assertion: the in-page JS clauses (via the backend), plus the
    /// `visual` diff and `ux` review, which need this layer's baseline store and
    /// judge. Returns the same `{passed, checks}` shape as the backend, so the
    /// flow engine and `agentctl test` report treat every clause alike.
    async fn assert_full(&self, target: &str, spec: &Value) -> Result<Value, BrowserError> {
        let mut checks: Vec<Value> = Vec::new();
        let wants_visual = spec.get("visual").is_some();
        let wants_ux = spec.get("ux").is_some();
        let wants_js = JS_ASSERT_KEYS.iter().any(|k| spec.get(*k).is_some());
        // Run the in-page clauses when the spec asks for one, or when it asks
        // for nothing here at all (so a plain functional assert still reaches
        // the backend); skip only when the spec is purely visual/ux.
        if wants_js || (!wants_visual && !wants_ux) {
            let r = self.backend.assert(target, spec).await?;
            if let Some(arr) = r.get("checks").and_then(Value::as_array) {
                checks.extend(arr.iter().cloned());
            }
        }
        if spec.get("visual").is_some() {
            checks.push(self.visual_check(target, spec).await);
        }
        if spec.get("ux").is_some() {
            checks.push(self.ux_check(target, spec).await);
        }
        if checks.is_empty() {
            return Err(BrowserError::Failed(
                "no assertions given (text/not_text/url/selector/no_console_errors/\
                 no_failed_requests/a11y/style/component/visual/ux)"
                    .into(),
            ));
        }
        let passed = checks
            .iter()
            .all(|c| c.get("ok").and_then(Value::as_bool) == Some(true));
        Ok(json!({ "passed": passed, "checks": checks }))
    }

    /// Visual regression: screenshot now, compare to a stored baseline. The
    /// first run for a name saves the baseline and passes; later runs pass when
    /// the changed-pixel ratio is within tolerance and the dimensions match.
    /// Any internal failure becomes a failing check rather than aborting the
    /// whole assertion, so it reports cleanly.
    async fn visual_check(&self, target: &str, spec: &Value) -> Value {
        let fail = |d: String| json!({ "name": "visual", "ok": false, "detail": d });
        let v = &spec["visual"];
        let (name, tolerance, node_ref) = match v {
            Value::String(s) => (s.clone(), 0.01_f64, None),
            _ => (
                v.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                v.get("tolerance").and_then(Value::as_f64).unwrap_or(0.01),
                v.get("ref").and_then(Value::as_str).map(String::from),
            ),
        };
        if name.trim().is_empty() {
            return fail("visual needs a baseline name".into());
        }
        let Some(store) = self.baselines.as_ref() else {
            return fail("visual store not configured (run with a state dir)".into());
        };
        let shot = match self.backend.screenshot(target, node_ref.as_deref()).await {
            Ok(s) => s,
            Err(e) => return fail(format!("screenshot failed: {}", browser_err_msg(&e))),
        };
        let base = match store.get(&name) {
            Ok(b) => b,
            Err(e) => return fail(format!("baseline load failed: {e:?}")),
        };
        let Some(base) = base else {
            return match store.save(&name, shot.base64, shot.width, shot.height, now_ms()) {
                Ok(_) => json!({ "name": "visual", "ok": true, "created": true,
                    "detail": format!("baseline created: {name}") }),
                Err(e) => fail(format!("baseline save failed: {e:?}")),
            };
        };
        let diff = match self
            .visual_diff(target, &base.png_base64, &shot.base64)
            .await
        {
            Ok(d) => d,
            Err(e) => return fail(format!("diff failed: {}", browser_err_msg(&e))),
        };
        if let Some(err) = diff.get("error").and_then(Value::as_str) {
            return fail(format!("diff error: {err}"));
        }
        if diff.get("dims_match").and_then(Value::as_bool) != Some(true) {
            let b = diff.get("base").and_then(Value::as_str).unwrap_or("?");
            let c = diff.get("cur").and_then(Value::as_str).unwrap_or("?");
            return json!({ "name": "visual", "ok": false,
                "detail": format!("dimensions changed {b} -> {c}"), "diff": diff });
        }
        let ratio = diff
            .get("diff_ratio")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        json!({
            "name": "visual",
            "ok": ratio <= tolerance,
            "detail": format!("{:.2}% changed (tol {:.2}%)", ratio * 100.0, tolerance * 100.0),
            "diff": diff,
        })
    }

    /// Diff two base64 PNGs in the page: decode both, compare pixels on a canvas,
    /// return the changed ratio and a bounding box. Done in-page to avoid an
    /// image-decoding dependency and to keep the whole comparison in one place.
    async fn visual_diff(
        &self,
        target: &str,
        base_b64: &str,
        cur_b64: &str,
    ) -> Result<Value, BrowserError> {
        let expr = VISUAL_DIFF_JS
            .replace("__BASE__", base_b64)
            .replace("__CUR__", cur_b64);
        let out = self.backend.eval(target, &expr).await?;
        Ok(out.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Judge-scored UX review: gather page facts, ask the judge one Noul per
    /// dimension (clarity/hierarchy/affordance/consistency by default), and
    /// report the scores. Advisory by default (never fails the run); set
    /// `ux.gate=true` (+ optional `ux.min`) to fail when a dimension is low.
    /// Degrades to a skipped, passing check when no judge is configured or the
    /// judge is unreachable, so it never blocks a run on its own absence.
    async fn ux_check(&self, target: &str, spec: &Value) -> Value {
        let uo = &spec["ux"];
        let dims: Vec<String> = uo
            .get("dims")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                ["clarity", "hierarchy", "affordance", "consistency"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
        let gate = uo.get("gate").and_then(Value::as_bool).unwrap_or(false);
        let min = uo.get("min").and_then(Value::as_f64).unwrap_or(0.5);
        let skipped =
            |d: String| json!({ "name": "ux", "ok": true, "advisory": true, "detail": d });
        let Some(judge) = self.judge.as_ref() else {
            return skipped("judge not configured; skipped".into());
        };
        let facts = match self.backend.eval(target, UX_FACTS_JS).await {
            Ok(v) => v.get("result").cloned().unwrap_or_else(|| json!({})),
            Err(e) => {
                return skipped(format!(
                    "could not read page facts: {}",
                    browser_err_msg(&e)
                ))
            }
        };
        let mut qs = std::collections::BTreeMap::new();
        for d in &dims {
            qs.insert(
                d.clone(),
                mcp_judge::Question::Noul {
                    instructions: ux_instruction(d),
                    criteria: None,
                },
            );
        }
        match judge.ask(facts, qs).await {
            Ok(ans) => {
                let mut scores = serde_json::Map::new();
                let mut low = Vec::new();
                for d in &dims {
                    if let Some(mcp_judge::Answer::Noul { noul }) = ans.answers.get(d) {
                        let p = noul.clamp(0.0, 1.0);
                        scores.insert(d.clone(), json!((p * 100.0).round() / 100.0));
                        if p < min {
                            low.push(d.clone());
                        }
                    }
                }
                let ok = !gate || low.is_empty();
                json!({
                    "name": "ux",
                    "ok": ok,
                    "advisory": !gate,
                    "detail": format!("{} scored, {} below {:.2}", scores.len(), low.len(), min),
                    "scores": scores,
                    "low": low,
                })
            }
            Err(e) => skipped(format!("judge unavailable: {}", e.message())),
        }
    }

    async fn flow(&self, args: &Value) -> Envelope {
        let tool = "browser_flow";
        let Some(store) = self.flows.as_ref() else {
            return Envelope::fail_with(
                tool,
                ErrorCode::UnsupportedOs,
                "browser_flow is not enabled (no flow store configured)",
                "run agentctl with a state dir so flows can be saved",
            );
        };
        let action = str_arg(args, "action").unwrap_or("list");
        match action {
            "save" => {
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "save needs 'name'");
                };
                let Some(steps) = args.get("steps").and_then(Value::as_array) else {
                    return Envelope::fail(
                        tool,
                        ErrorCode::InvalidArgs,
                        "save needs 'steps' array",
                    );
                };
                match store.save(name, steps.clone(), now_ms()) {
                    Ok(f) => Envelope::ok(tool, json!({ "name": f.name, "steps": f.steps.len() })),
                    Err(e) => flow_err(tool, e),
                }
            }
            "list" => match store.list() {
                Ok(fs) => {
                    let rows: Vec<Value> = fs
                        .iter()
                        .map(|f| json!({ "name": f.name, "steps": f.steps.len() }))
                        .collect();
                    Envelope::ok(tool, json!({ "flows": rows, "count": rows.len() }))
                }
                Err(e) => flow_err(tool, e),
            },
            "get" => {
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "get needs 'name'");
                };
                match store.get(name) {
                    Ok(Some(f)) => Envelope::ok(tool, json!({ "name": f.name, "steps": f.steps })),
                    Ok(None) => {
                        Envelope::fail(tool, ErrorCode::NotFound, format!("no flow '{name}'"))
                    }
                    Err(e) => flow_err(tool, e),
                }
            }
            "delete" => {
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "delete needs 'name'");
                };
                match store.delete(name) {
                    Ok(removed) => Envelope::ok(tool, json!({ "deleted": removed })),
                    Err(e) => flow_err(tool, e),
                }
            }
            "run" => {
                let Some(name) = str_arg(args, "name") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "run needs 'name'");
                };
                let Some(target) = str_arg(args, "target_id") else {
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, "run needs 'target_id'");
                };
                let flow = match store.get(name) {
                    Ok(Some(f)) => f,
                    Ok(None) => {
                        return Envelope::fail(
                            tool,
                            ErrorCode::NotFound,
                            format!("no flow '{name}'"),
                        )
                    }
                    Err(e) => return flow_err(tool, e),
                };
                let cont = args
                    .get("continue_on_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let secrets = match parse_secrets(args.get("secrets")) {
                    Ok(s) => s,
                    Err(m) => return Envelope::fail(tool, ErrorCode::InvalidArgs, m),
                };
                // Fail before any step runs: a login that types the username
                // and then stops for want of a password has already changed
                // the page.
                let missing = crate::flow::missing_secret_refs(&flow.steps, &secrets);
                if !missing.is_empty() {
                    let first = crate::flow::missing_msg(&missing[0]);
                    let msg = if missing.len() > 1 {
                        format!("{first}; also missing: {}", missing[1..].join(", "))
                    } else {
                        first
                    };
                    return Envelope::fail(tool, ErrorCode::InvalidArgs, msg);
                }
                self.replay(tool, target, &flow, cont, &secrets).await
            }
            other => Envelope::fail(
                tool,
                ErrorCode::InvalidArgs,
                format!("unknown action '{other}'"),
            ),
        }
    }

    /// Replay a flow's steps against `target`, stopping at the first failure
    /// unless `cont`. A green run never invokes a model.
    async fn replay(
        &self,
        tool: &str,
        target: &str,
        flow: &crate::flow::Flow,
        cont: bool,
        secrets: &BTreeMap<String, String>,
    ) -> Envelope {
        let mut results = Vec::new();
        let mut passed = true;
        for (i, step) in flow.steps.iter().enumerate() {
            let (ok, detail) = match crate::flow::resolve_step_secrets(step, secrets) {
                // The resolved copy lives for this step only.
                Ok(resolved) => self.run_step(target, &resolved).await,
                Err(m) => (false, json!(m)),
            };
            results.push(json!({ "i": i, "op": step.get("op"), "ok": ok, "detail": detail }));
            if !ok {
                passed = false;
                if !cont {
                    break;
                }
            }
        }
        let data = json!({
            "name": flow.name, "passed": passed,
            "ran": results.len(), "steps": results,
        });
        if passed {
            Envelope::ok(tool, data)
        } else {
            Envelope {
                ok: false,
                tool: tool.into(),
                data: Some(data),
                error: Some(ToolError {
                    code: ErrorCode::ActionFailed,
                    message: format!("flow '{}' failed", flow.name),
                    suggestion: Some("see data.steps for the failing step".into()),
                }),
                image: None,
            }
        }
    }

    /// Execute one replay step. Returns (ok, detail).
    async fn run_step(&self, target: &str, step: &Value) -> (bool, Value) {
        let op = step.get("op").and_then(Value::as_str).unwrap_or("");
        let r: Result<Value, BrowserError> = match op {
            "navigate" => {
                self.backend
                    .navigate(
                        target,
                        str_arg(step, "action").unwrap_or("goto"),
                        str_arg(step, "url"),
                    )
                    .await
            }
            "act" => {
                // An older recording holds a placeholder in place of the
                // value: refuse loudly rather than type it.
                if str_arg(step, "value") == Some(crate::flow::SECRET_PLACEHOLDER) {
                    return (
                        false,
                        json!("act step holds a secret placeholder from an older recording: its value was never stored. Re-record it, or replace the step's value with \"secret_ref\": \"<name>\" and pass secrets:{\"<name>\": \"...\"} when running the flow"),
                    );
                }
                let locator = if let Some(r) = str_arg(step, "ref") {
                    crate::backend::Locator::Ref(r)
                } else if let Some(q) = str_arg(step, "query") {
                    crate::backend::Locator::Selector {
                        by: str_arg(step, "by").unwrap_or("css"),
                        query: q,
                        within: str_arg(step, "within"),
                        text: str_arg(step, "text"),
                        index: step
                            .get("index")
                            .and_then(Value::as_u64)
                            .map(|n| n as usize),
                    }
                } else {
                    return (false, json!("act step needs 'ref' or 'query' (with optional 'by', 'within', 'text', 'index')"));
                };
                // A secret value (substituted from `secrets` for this step) is
                // masked in anything that would display it, like `browser_act`.
                let secret = step.get("secret").and_then(Value::as_bool) == Some(true);
                self.backend
                    .act_masked(
                        target,
                        locator,
                        str_arg(step, "action").unwrap_or("click"),
                        str_arg(step, "value"),
                        secret,
                    )
                    .await
            }
            "dialog" => {
                // How the tab answers its JavaScript dialogs from here on (the
                // same standing policy as `browser_dialog`). A recording puts
                // one before the action that raised the dialog.
                let policy = match str_arg(step, "policy") {
                    Some("accept") => {
                        DialogPolicy::Accept(str_arg(step, "prompt_text").map(str::to_string))
                    }
                    Some("dismiss") => DialogPolicy::Dismiss,
                    _ => {
                        return (
                            false,
                            json!("dialog step needs 'policy': 'accept' or 'dismiss'"),
                        )
                    }
                };
                self.backend.dialog(target, Some(policy)).await
            }
            "viewport" => {
                let width = step.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
                let height = step.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
                let mobile = step.get("mobile").and_then(Value::as_bool).unwrap_or(false);
                let scale = step.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
                self.backend
                    .set_viewport(target, width, height, mobile, scale)
                    .await
            }
            "fill_form" => {
                let Some(fields) = step.get("fields") else {
                    return (false, json!("fill_form step needs 'fields'"));
                };
                self.backend
                    .fill_form(target, fields, step.get("submit"))
                    .await
            }
            "extract" => {
                let Some(schema) = step.get("schema") else {
                    return (false, json!("extract step needs 'schema'"));
                };
                self.backend
                    .extract(target, schema, str_arg(step, "within"))
                    .await
            }
            "wait" => {
                let (cond, arg) = if let Some(sel) = str_arg(step, "selector") {
                    ("selector", Some(sel))
                } else if step.get("dom_settled").is_some() {
                    ("dom_settled", None)
                } else if step.get("htmx_settled").is_some()
                    || str_arg(step, "condition") == Some("htmx_settled")
                {
                    ("htmx_settled", None)
                } else if step.get("navigation").is_some() {
                    ("navigation", None)
                } else {
                    ("network_idle", None)
                };
                let t = step
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(10_000);
                let window = match step.get("navigation_timeout_ms") {
                    None | Some(Value::Null) => None,
                    Some(v) => match nav_window_arg(v) {
                        Some(n) => Some(n),
                        None => {
                            return (
                                false,
                                json!("navigation_timeout_ms must be an integer from 0 to 30000"),
                            )
                        }
                    },
                };
                self.backend.wait_window(target, cond, arg, t, window).await
            }
            "capture" => {
                self.backend
                    .capture(target, str_arg(step, "action").unwrap_or("start"), step)
                    .await
            }
            "assert" => match self.assert_full(target, step).await {
                // An assert's own pass/fail is the step's ok.
                Ok(v) => {
                    let passed = v.get("passed").and_then(Value::as_bool) == Some(true);
                    return (passed, v);
                }
                Err(e) => Err(e),
            },
            other => return (false, json!(format!("unknown step op '{other}'"))),
        };
        match r {
            Ok(v) => (true, v),
            Err(e) => (false, json!(browser_err_msg(&e))),
        }
    }
}

/// The conditions `browser_wait` knows, as the `condition` enum spells them.
const WAIT_CONDITIONS: [&str; 6] = [
    "selector",
    "dom_settled",
    "htmx_settled",
    "navigation",
    "network_idle",
    "challenge_cleared",
];

/// Which condition a `browser_wait` call asks for. `condition` is the
/// preferred form; `selector` and the boolean flags are aliases for it. A
/// flag selects its condition only when `true` (`navigation:false` selects
/// nothing), and asking for more than one condition is an error, not a silent
/// pick. Naming the same condition twice (`condition:"navigation"` with
/// `navigation:true`) is one condition.
fn parse_wait_condition(args: &Value) -> Result<(&'static str, Option<&str>), String> {
    let mut found: Vec<&'static str> = Vec::new();
    let mut arg = None;
    if let Some(sel) = str_arg(args, "selector") {
        found.push("selector");
        arg = Some(sel);
    }
    for flag in &WAIT_CONDITIONS[1..] {
        match args.get(*flag) {
            None | Some(Value::Null) => {}
            Some(Value::Bool(true)) => found.push(flag),
            Some(Value::Bool(false)) => {}
            Some(_) => return Err(format!("'{flag}' must be a boolean")),
        }
    }
    if let Some(cond) = str_arg(args, "condition") {
        let cond = if cond == "challenge" {
            "challenge_cleared"
        } else {
            cond
        };
        let Some(known) = WAIT_CONDITIONS.iter().find(|k| **k == cond) else {
            return Err(format!(
                "unknown condition '{cond}'; use one of {}",
                WAIT_CONDITIONS.join(", ")
            ));
        };
        if *known == "selector" && arg.is_none() {
            return Err(
                "condition 'selector' needs the 'selector' argument (the CSS selector to wait for)"
                    .into(),
            );
        }
        if !found.contains(known) {
            found.push(known);
        }
    }
    match found.as_slice() {
        [] => Err(format!(
            "provide one wait condition: 'condition' (one of {}), or the alias 'selector' or a boolean flag set to true",
            WAIT_CONDITIONS.join(", ")
        )),
        [one] => Ok((one, arg.filter(|_| *one == "selector"))),
        many => Err(format!(
            "give one wait condition, got {}: {}",
            many.len(),
            many.join(", ")
        )),
    }
}

/// The longest plain `browser_wait` sleep.
const WAIT_SLEEP_MAX_MS: u64 = 30_000;

/// The sleep a `browser_wait` with a duration (`timeout_ms`, `ms` or
/// `duration_ms`) and no condition asks for, in ms, capped. `None` when any
/// condition is given: the duration is then that condition's timeout.
fn sleep_only_ms(args: &Value) -> Option<u64> {
    let flagged = WAIT_CONDITIONS[1..]
        .iter()
        .any(|f| args.get(*f).and_then(Value::as_bool) == Some(true));
    if flagged || str_arg(args, "selector").is_some() || str_arg(args, "condition").is_some() {
        return None;
    }
    ["timeout_ms", "ms", "duration_ms"]
        .iter()
        .find_map(|k| args.get(*k).and_then(crate::input::coord))
        .map(|ms| (ms.max(0.0) as u64).min(WAIT_SLEEP_MAX_MS))
}

/// The pointer arguments of `browser_act`: coordinates may be numbers or
/// numeric strings, and the drag destination is an element or a point.
fn pointer_args(args: &Value) -> Result<crate::backend::PointerArgs<'_>, String> {
    let num = |k: &str| -> Result<Option<f64>, String> {
        match args.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => crate::input::coord(v)
                .map(Some)
                .ok_or_else(|| format!("'{k}' must be a number of CSS px, got {v}")),
        }
    };
    let to = if let Some(r) = str_arg(args, "to_ref") {
        Some(crate::backend::Locator::Ref(r))
    } else {
        str_arg(args, "to_query").map(|q| crate::backend::Locator::Selector {
            by: if looks_like_xpath(q) { "xpath" } else { "auto" },
            query: q,
            within: None,
            text: None,
            index: None,
        })
    };
    Ok(crate::backend::PointerArgs {
        x: num("x")?,
        y: num("y")?,
        to,
        to_x: num("to_x")?,
        to_y: num("to_y")?,
        dx: num("dx")?,
        dy: num("dy")?,
        value: str_arg(args, "value"),
    })
}

/// How the element is located: a `ref`, else a `query` (the `browser_act`
/// arguments); `None` when neither is given.
fn parse_locator(args: &Value) -> Option<crate::backend::Locator<'_>> {
    if let Some(r) = str_arg(args, "ref") {
        Some(crate::backend::Locator::Ref(r))
    } else {
        str_arg(args, "query").map(|q| crate::backend::Locator::Selector {
            // No `by` is "auto": CSS, else visible text (see `__find_all`).
            by: str_arg(args, "by").unwrap_or("auto"),
            query: q,
            within: str_arg(args, "within"),
            text: str_arg(args, "text"),
            index: args
                .get("index")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
        })
    }
}

/// The CSS among a call's `by` / `query` pairs, for the selector hint: with no
/// `by` the query may be CSS (and then reads like it), so it counts too.
fn css_candidate<'a>(o: &'a Value, key: &str) -> Option<&'a str> {
    str_arg(o, key).filter(|_| matches!(str_arg(o, "by"), None | Some("css")))
}

/// Most files one `browser_upload` may attach.
const UPLOAD_MAX_FILES: usize = 10;
/// Largest single file `browser_upload` will attach.
const UPLOAD_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// The `paths` argument of `browser_upload`: 1 to [`UPLOAD_MAX_FILES`] non-empty strings.
fn upload_paths(args: &Value) -> Result<Vec<&str>, String> {
    let Some(list) = args.get("paths").and_then(Value::as_array) else {
        return Err("missing 'paths': an array of 1 to 10 file paths".into());
    };
    if list.is_empty() || list.len() > UPLOAD_MAX_FILES {
        return Err(format!(
            "'paths' must hold 1 to {UPLOAD_MAX_FILES} files, got {}",
            list.len()
        ));
    }
    list.iter()
        .map(|v| {
            v.as_str()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| "every entry of 'paths' must be a non-empty string".to_string())
        })
        .collect()
}

/// Whether a resolved path may be attached: a regular file (not a directory,
/// socket or device) of at most [`UPLOAD_MAX_BYTES`].
fn check_upload_file(path: &str, is_file: bool, bytes: u64) -> Result<(), String> {
    if !is_file {
        return Err(format!("'{path}' is not a regular file"));
    }
    if bytes > UPLOAD_MAX_BYTES {
        return Err(format!(
            "'{path}' is {bytes} bytes; the limit is {UPLOAD_MAX_BYTES} (50 MiB)"
        ));
    }
    Ok(())
}

/// The `scroll`, `wait_after` and `timeout_ms` arguments of `browser_act`.
fn parse_act_opts(args: &Value) -> Result<crate::backend::ActOpts, String> {
    use crate::backend::{ActOpts, ScrollMode};
    let mut opts = ActOpts::default();
    match str_arg(args, "scroll") {
        None => {}
        Some("none") => opts.scroll = ScrollMode::None,
        Some("nearest") => opts.scroll = ScrollMode::Nearest,
        Some("center") => opts.scroll = ScrollMode::Center,
        Some(other) => {
            return Err(format!(
                "unknown scroll '{other}'; use none, nearest (default) or center"
            ))
        }
    }
    match str_arg(args, "wait_after") {
        None | Some("none") => {}
        Some("settle") => opts.settle = true,
        Some(other) => {
            return Err(format!(
                "unknown wait_after '{other}'; use none (default) or settle"
            ))
        }
    }
    match args.get("timeout_ms") {
        None | Some(Value::Null) => {}
        Some(v) => match v.as_u64() {
            Some(n) => opts.timeout_ms = n,
            None => return Err("timeout_ms must be a non-negative integer".into()),
        },
    }
    Ok(opts)
}

/// A `navigation_timeout_ms` value: an integer within the allowed window.
fn nav_window_arg(v: &Value) -> Option<u64> {
    v.as_u64()
        .filter(|n| *n <= crate::backend::NAV_EXPECT_MAX_MS)
}

/// A flow step that makes the tab answer its dialogs "yes".
fn is_accepting_dialog_step(step: &Value) -> bool {
    step.get("op").and_then(Value::as_str) == Some("dialog")
        && step.get("policy").and_then(Value::as_str) == Some("accept")
}

/// The `secrets` argument of a flow run: an object of string values. A wrong
/// shape is refused without echoing any value.
fn parse_secrets(v: Option<&Value>) -> Result<BTreeMap<String, String>, String> {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return Ok(BTreeMap::new());
    };
    let Some(obj) = v.as_object() else {
        return Err("'secrets' must be an object of name: value strings".into());
    };
    let mut out = BTreeMap::new();
    for (k, val) in obj {
        match val.as_str() {
            Some(s) => {
                out.insert(k.clone(), s.to_string());
            }
            None => return Err(format!("secret '{k}' must be a string")),
        }
    }
    Ok(out)
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn flow_err(tool: &str, e: crate::flow::FlowError) -> Envelope {
    match e {
        crate::flow::FlowError::Invalid(m) => Envelope::fail(tool, ErrorCode::InvalidArgs, m),
        crate::flow::FlowError::Io(m) => Envelope::fail(tool, ErrorCode::ActionFailed, m),
    }
}

fn browser_err_msg(e: &BrowserError) -> String {
    match e {
        BrowserError::PermissionDenied(m)
        | BrowserError::NotFound(m)
        | BrowserError::Unsupported(m)
        | BrowserError::Timeout(m)
        | BrowserError::Failed(m) => m.clone(),
    }
}

#[async_trait]
impl ToolModule for BrowserModule {
    fn descriptors(&self) -> Vec<ToolDescriptor> {
        let obj = |props: Value, required: Value| json!({ "type": "object", "properties": props, "required": required });
        vec![
            ToolDescriptor::new(
                "browser_connect",
                Category::Browser,
                Tier::Standard,
                "Attach to a Chromium started with --remote-debugging-port, or launch one. Returns the open tabs (active first) and the active target_id.",
                obj(
                    json!({
                        "attach": { "type": "object", "properties": { "port": { "type": "integer" } } },
                        "launch": { "type": "object", "properties": {
                            "browser": { "type": "string", "enum": ["chromium", "safari"], "description": "default chromium; safari is experimental (macOS)" },
                            "url": { "type": "string", "description": "Safari only: first page to open" },
                            "port": { "type": "integer", "description": "omit or 0 to pick a free port" },
                            "headless": { "type": "boolean" },
                            "args": { "type": "array", "items": { "type": "string" }, "description": "Chromium only: extra flags as --name or --name=value; an allowlist, anything else is refused with the list" },
                            "background_throttling": { "type": "boolean", "description": "Chromium only: true keeps Chrome's throttling when the window is covered (default false)" },
                            "user_data_dir": { "type": "string" },
                            "profile": { "type": "string", "description": "saved profile to restore" }
                        } },
                        "profile": { "type": "string", "description": "saved profile to restore" }
                    }),
                    json!([]),
                ),
            ).details(
                "Optionally auto-restores a saved profile. The browser's active tab is brought to the front on connect \
                 (result 'foregrounded'). `launch.browser='safari'` drives Safari through safaridriver (macOS only, \
                 experimental: needs `safaridriver --enable` once, and a Safari that was already open when automation was \
                 enabled must be quit first; opens a visible window). On Safari, browser_network, browser_dialog, \
                 browser_viewport, browser_record, browser_branch, browser_checkpoint and browser_act 'press' return \
                 Unsupported.\n\n\
                 `launch.url` is Safari only (on Chromium use browser_navigate) and is checked against the navigation \
                 policy. `launch.port` omitted or 0 lets Chrome pick a free one; the connect result reports it. \
                 `launch.args` are Chromium only, each one entry written --name or --name=value (no spaces; at most 32). \
                 Only these are accepted: --window-size, --window-position, --start-maximized, --start-fullscreen, \
                 --force-device-scale-factor, --hide-scrollbars, --force-dark-mode, --lang, --accept-lang, --user-agent, \
                 --mute-audio, --autoplay-policy, --disable-gpu, --disable-extensions, --disable-notifications, \
                 --disable-default-apps, --disable-sync, --disable-search-engine-choice-screen, \
                 --use-fake-device-for-media-stream, --auto-open-devtools-for-tabs, --incognito, the three \
                 --disable-*background* flags, and --disable-features naming CalculateNativeWinOcclusion, Translate, \
                 MediaRouter, OptimizationHints, AutofillServerCommunication or PaintHolding (merged with agentctl's own).\n\n\
                 `launch.background_throttling`: a visible (headless=false) browser is started with flags that stop Chrome \
                 throttling timers, rendering and screen recording when its window is behind another or covered. Set true \
                 to leave Chrome's normal throttling on. Default false. `profile` (top level or under `launch`) is the \
                 saved profile name to auto-restore upon connecting.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_disconnect",
                Category::Browser,
                Tier::Standard,
                "Disconnect from a browser. kill=true also stops one agentctl launched (an attached browser is never killed).",
                obj(
                    json!({
                        "browser_id": { "type": "integer" },
                        "kill": { "type": "boolean" }
                    }),
                    json!(["browser_id"]),
                ),
            ).details(
                "With kill=true, also stop a browser this session launched and delete the temporary profile it created \
                 (attached browsers are never killed). `kill` is only valid for a browser agentctl launched.",
            ),
            ToolDescriptor::new(
                "browser_tabs",
                Category::Browser,
                Tier::Standard,
                "List, open (url), activate or close (target_id) tabs of a connected browser.",
                obj(
                    json!({
                        "browser_id": { "type": "integer" },
                        "action": { "type": "string", "enum": ["list", "open", "activate", "close"] },
                        "target_id": { "type": "string" },
                        "url": { "type": "string" }
                    }),
                    json!(["browser_id", "action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_navigate",
                Category::Browser,
                Tier::Standard,
                "Navigate a tab: goto a url, or go back, forward or reload.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "action": { "type": "string", "enum": ["goto", "back", "forward", "reload"] },
                        "url": { "type": "string" }
                    }),
                    json!(["action"]),
                ),
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_snapshot",
                Category::Browser,
                Tier::Read,
                "Flatten a page into interactable node refs (dom or accessibility mode) or raw text. Pass the refs to browser_act.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "mode": { "type": "string", "enum": ["dom", "accessibility", "text"] },
                        "root_selector": { "type": "string", "description": "limit the snapshot to this subtree" }
                    }),
                    json!([]),
                ),
            ).details(
                "The web equivalent of get_ui_tree. A <canvas> gets child nodes (tag canvas-child) only if the page itself \
                 publishes its interactive regions via canvas.__agentctl_regions or a data-canvas-regions JSON attribute; \
                 any other canvas is an opaque node. Besides controls and roles, an element a page makes clickable with \
                 a pointer cursor is listed (the outermost one, not each span inside it); one with no text is named by \
                 its aria-label, title or alt, the file name of the icon it draws, or its id or class words, and that name \
                 works as a by=text query. 'root_selector' is a CSS selector or an XPath (a ref from an earlier snapshot).",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_query",
                Category::Browser,
                Tier::Read,
                "Resolve node refs by css selector, xpath or text (case-insensitive; exact matches first, clickable elements preferred). all=true returns every match.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "by": { "type": "string", "enum": ["css", "xpath", "text"] },
                        "query": { "type": "string" },
                        "all": { "type": "boolean" }
                    }),
                    json!(["query"]),
                ),
            ).details(
                "Resolve node refs. With no 'by' the query is tried as CSS and, if it does not parse or matches nothing, as \
                 visible text; the result then says matched_by (css or text). Common spellings are read as what they mean: a \
                 query starting with / or ( is an XPath; xpath=..., css=..., text=..., text:..., text(\"..\") and text \"..\" \
                 name their kind; a trailing :has-text(\"X\"), :contains(\"X\"), :text(\"X\") or :text-is(\"X\") becomes a \
                 text filter on the CSS before it (or a text search when nothing precedes it); and a truncated snapshot ref \
                 such as div[1]/div[2] is the XPath /html/body/div[1]/div[2]. An explicit 'by' of text or xpath is taken \
                 as given.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_act",
                Category::Browser,
                Tier::Standard,
                "Act on a DOM node: click, double_click, triple_click, right_click, hover, drag, scroll, type, select, focus, scroll_into_view, submit or press. Target it with 'ref' (from browser_query/snapshot) or with 'query' plus optional by, within, text, index. x and y click or hover at a point (offsets inside the target, or viewport px without one). type replaces the field's content and reports value_after. press takes a key or combo such as ctrl+a. A click returns at once, before its request or navigation has begun: use wait_after='settle' or browser_wait.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "ref": { "type": "string", "description": "a ref from browser_query/snapshot" },
                        "by": { "type": "string", "enum": ["css", "xpath", "text"], "description": "how to read 'query' (default: CSS, else visible text)" },
                        "query": { "type": "string", "description": "selector to resolve and act on in one call, instead of 'ref'; type and press with neither act on the focused element, scroll on the page" },
                        "within": { "type": "string", "description": "root selector scoping the query" },
                        "text": { "type": "string", "description": "substring filter on the matches" },
                        "index": { "type": "integer", "description": "0-based match index (default 0)" },
                        "action": { "type": "string", "enum": ["click", "double_click", "triple_click", "right_click", "hover", "drag", "scroll", "type", "select", "focus", "scroll_into_view", "submit", "press"] },
                        "value": { "type": "string", "description": "text for type, option text or value for select (on the <select> or an option), key or combo for press (Enter, Escape, ArrowDown, a, ctrl+a, Shift+Tab), direction for scroll (up, down, left, right, top, bottom)" },
                        "x": { "type": "number", "description": "pointer x in CSS px: an offset from the target's top-left corner, or a viewport point without a target" },
                        "y": { "type": "number", "description": "pointer y, as x" },
                        "to_ref": { "type": "string", "description": "drag destination element (a ref)" },
                        "to_query": { "type": "string", "description": "drag destination element (a selector)" },
                        "to_x": { "type": "number", "description": "drag destination x: an offset inside to_ref/to_query, else a viewport point" },
                        "to_y": { "type": "number", "description": "drag destination y, as to_x" },
                        "dx": { "type": "number", "description": "drag: x distance from the start; scroll: horizontal distance in CSS px" },
                        "dy": { "type": "number", "description": "drag: y distance from the start; scroll: vertical distance in CSS px (default one viewport down)" },
                        "secret": { "type": "boolean", "description": "value is a secret: kept out of the audit log and the showcase HUD" },
                        "scroll": { "type": "string", "enum": ["none", "nearest", "center"], "description": "bring the element into view first (default nearest)" },
                        "wait_after": { "type": "string", "enum": ["none", "settle"], "description": "settle waits for a started navigation, htmx and quiet network, and adds navigated, requests_started and settled to the result (Chrome; about 2s when nothing starts). Default none" },
                        "timeout_ms": { "type": "integer", "description": "settle bound (default 10000); on expiry the action still succeeded and the result has settled:false" }
                    }),
                    json!(["action"]),
                ),
            ).details(
                "`press` takes a key or a combo in `value`: a named key (Enter, Escape, Tab, Arrow keys, Home, End, PageUp, \
                 PageDown, Backspace, Delete, Insert, Space, F1-F12), a character, or modifiers (ctrl, alt, shift, meta/cmd) \
                 joined by +, as in ctrl+a or Shift+ArrowDown. It is a real key event to the target, or to the focused node \
                 or page with no target; Chrome only. ctrl or cmd with a, c, x, v, z or y edits as a person's shortcut \
                 does, also on macOS.\n\n\
                 Pointer actions are real input (Chrome only): double_click, triple_click (selects a line), right_click \
                 (contextmenu) and hover, and click too, accept `x` and `y`: with a target they are offsets from its top-left \
                 corner (for a canvas, its pixel coordinates), without one viewport CSS px; the result's click_at is where \
                 it landed, and a point outside the viewport is an error. `scroll` turns the mouse wheel over the target (or \
                 the page) by dx and dy, by default one viewport down; `value` may be up, down, left, right, top or bottom. \
                 `drag` presses on the target (or at x and y), moves in steps and releases at the destination: the \
                 to_ref or to_query element (its centre, or to_x and to_y inside it), else to_x and to_y as viewport \
                 points, else dx and dy from the start; the destination must be on screen. Mouse-event drags (sliders, \
                 sortable lists, selecting text) and HTML5 draggable elements both work; the result says html5_drag. \
                 Spellings such as key, dblclick, triple-click, drag_and_drop are accepted.\n\n\
                 On Chrome a click is real pointer input (mousedown, mouseup, click, as a person's) and type is a real \
                 insertion that replaces the field's content, so React-style controlled fields and menus that open on \
                 mousedown work; the result reports input 'cdp', or 'synthetic' with input_reason when the element is \
                 covered, off screen, in a frame, a select/option or a file input. type reports value_after (value_length \
                 for a password or secret field). A native <select> is set with select (on the list or one of its \
                 options) or a click on an <option>; both report selected and changed, and an option that is missing, \
                 disabled or undone by the page is an error. A page-published canvas region (a canvas-child ref from browser_snapshot) \
                 supports only click and hover, sent as real mouse input at the region centre; other actions on it return \
                 Unsupported.\n\n\
                 Query targeting: 'by' is css, xpath or text (used when no 'ref'); with none, the query is tried as CSS and, \
                 if it does not parse or matches nothing, as visible text (the result says matched_by), so a plain word \
                 such as \"Submit\" works. The spellings browser_query lists (/ or ( for XPath, text=..., :has-text(..) \
                 and so on) are read as what they mean here too, and in 'within', browser_upload and browser_fill_form. \
                 Text is case-insensitive, exact matches first, clickable elements preferred. When nothing matches the \
                 error says what was tried and lists up to three elements whose text holds a word of the query. The check \
                 is made once: act does not wait for an element to appear (use browser_wait). 'within' is an optional CSS/XPath root selector scoping \
                 the search, 'text' an optional substring filter to narrow matches, 'index' an optional 0-based match index \
                 when the query matches several elements (default 0).\n\n\
                 `secret`: the value is a secret, so it stays out of the audit log and is never shown in the showcase \
                 typing HUD (password and one-time-code fields are masked automatically).\n\n\
                 `scroll` brings the element into view first: nearest (default) moves the page only as far as needed and \
                 not at all when it is visible, center centres it (can scroll a wide page sideways), none does not scroll. \
                 scroll_into_view always scrolls.\n\n\
                 `wait_after`: none (default) returns as soon as the action ran, when a click's request or navigation has \
                 usually not begun yet. settle then waits for a navigation it started to load, for htmx_settled if the page \
                 has htmx, and for the network to go quiet, and adds navigated, requests_started (fetch/XHR/htmx begun on \
                 the page since the action) and settled to the result. A click that starts no request and no navigation \
                 costs about 2s here; Chrome only. `timeout_ms` (wait_after settle only) bounds the whole settle wait \
                 (default 10000); when it runs out the action still succeeded and the result has settled:false and \
                 settle_error.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_upload",
                Category::Browser,
                Tier::Dangerous,
                "Attach local files to a file input (type=file). Target it like browser_act ('ref' or 'query'); 'paths' holds 1 to 10 absolute paths inside fs.roots. Returns {files:[{name, bytes}], count}, never file contents. Chrome only.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "ref": { "type": "string", "description": "a ref from browser_query/snapshot" },
                        "by": { "type": "string", "enum": ["css", "xpath", "text"] },
                        "query": { "type": "string", "description": "selector, instead of 'ref'" },
                        "within": { "type": "string" },
                        "text": { "type": "string" },
                        "index": { "type": "integer" },
                        "paths": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "absolute paths of the files to attach (1 to 10)"
                        }
                    }),
                    json!(["paths"]),
                ),
            ).details(
                "Attach local files to a file input (type=file; a click would open the OS file chooser, which agentctl cannot \
                 drive). Target the input like browser_act: 'ref' or 'query' (plus optional 'by', 'within', 'text', 'index'; \
                 'by' is read like browser_act's and used when there is no 'ref'; 'within' is an optional CSS/XPath root selector, \
                 'text' a substring filter, 'index' a 0-based match index, default 0). A label is followed to its input, and \
                 an element holding exactly one file input uses that one; otherwise the error says what was found (target a \
                 hidden input directly). 'paths' holds 1 to 10 files, each at most 50 MiB, and more than one needs the \
                 input's multiple attribute. Only files inside the configured fs.roots can be attached (credential stores \
                 are always refused): a page can read what is attached, so this is how local files leave the machine. \
                 Chrome fires trusted input and change events. Returns {ok, files:[{name, bytes}], count, input_multiple}, \
                 never file contents. Chrome only.",
            ),
            ToolDescriptor::new(
                "browser_fill_form",
                Category::Browser,
                Tier::Standard,
                "Fill several form fields (input, select, checkbox, radio) in one call and optionally submit.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "fields": {
                            "type": "array",
                            "items": { "type": "object", "properties": {
                                "ref": { "type": "string" },
                                "selector": { "type": "string" },
                                "by": { "type": "string", "enum": ["css", "xpath", "text"], "description": "how to read 'selector' (default: CSS, else visible text; a leading / or ( means xpath)" }
                            } },
                            "description": "[{ref or selector, by, value, type, secret}]"
                        },
                        "submit": {
                            "type": "object",
                            "description": "{ref or selector} of the submit control"
                        }
                    }),
                    json!(["fields"]),
                ),
            ).details(
                "Eliminates round-trips for registration or checkout forms. `fields` is an array of \
                 [{ref or selector, by, value, type, secret}]; `submit` is an optional submit trigger: {ref or selector}.",
            ),
            ToolDescriptor::new(
                "browser_extract",
                Category::Browser,
                Tier::Read,
                "Extract structured data (text, attributes, lists, tables) from the page with a schema of CSS rules.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "schema": {
                            "type": "object",
                            "description": "field name to rule {selector, attr, regex, multiple, fields}"
                        },
                        "within": {
                            "type": "string",
                            "description": "CSS root selector to scope extraction"
                        }
                    }),
                    json!(["schema"]),
                ),
            ).details(
                "Offloads extraction parsing from the LLM. The schema maps field names to rules \
                 {selector, attr, regex, multiple, fields}.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_profile",
                Category::Browser,
                Tier::Standard,
                "Save, restore, list or delete session profiles (cookies, localStorage, sessionStorage) to swap user or auth state without logging in again.",
                obj(
                    json!({
                        "action": { "type": "string", "enum": ["save", "restore", "list", "delete"] },
                        "target_id": { "type": "string", "description": "save/restore: tab (default: active tab)" },
                        "name": { "type": "string" }
                    }),
                    json!(["action"]),
                ),
            ).details(
                "`target_id` (save/restore) is the tab to snapshot or populate; `name` (save/restore/delete) is the profile name.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_branch",
                Category::Browser,
                Tier::Standard,
                "Fork an isolated background context from a tab (create), try things without touching the visible tab, then commit the winning state, discard, switch or list branches. Chrome only.",
                obj(
                    json!({
                        "action": { "type": "string", "enum": ["create", "commit", "discard", "switch", "list"] },
                        "target_id": { "type": "string", "description": "create: parent tab to fork from" },
                        "branch_id": { "type": "string", "description": "branch identifier" }
                    }),
                    json!(["action"]),
                ),
            ).details(
                "Speculative browser context branching: fork an isolated background context from a tab ('create'), run \
                 trials without affecting the visible tab, commit winning state ('commit'), discard failed branches \
                 ('discard'), switch focus ('switch'), or list branches ('list'). Branches run in a separate browser \
                 context and fail with an error if one cannot be created (no silent fallback to the shared context). \
                 Commit and discard report an error unless the work was done and the branch tab was really closed. At \
                 most 8 branches may be active at once (AGENTCTL_MAX_BRANCHES). Chrome only: a Safari tab returns \
                 Unsupported. `branch_id` is the unique branch identifier for create/commit/discard/switch.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_checkpoint",
                Category::Browser,
                Tier::Standard,
                "Save and roll back tab state (form values, storage, cookies, URL; not the DOM). save, rollback, list or delete by tag; re-saving a tag makes it 'latest'. Chrome only.",
                obj(
                    json!({
                        "action": { "type": "string", "enum": ["save", "rollback", "list", "delete"] },
                        "target_id": { "type": "string", "description": "tab (default: active tab)" },
                        "tag": { "type": "string", "description": "checkpoint name, e.g. 'step_2' or 'latest'" }
                    }),
                    json!(["action"]),
                ),
            ).details(
                "In-memory state checkpointing and rollback (T-1) for browser tabs. 'save' captures a deep copy of form \
                 state (input values, checks, select indexes, scroll), storage, cookies and URL (not the DOM tree); \
                 'rollback' navigates back if needed (loading the page from the network with Chrome's HTTP cache \
                 bypassed, so a server that is down is an error, not a stale cached page; the result says \
                 cache_bypassed), waits for the page to load, restores that state and fails with the reason if any part \
                 could not be restored; 'list'/'delete' manage checkpoints. Re-saving a tag makes it the newest \
                 ('latest'). File inputs are skipped. Chrome only: a Safari tab returns Unsupported.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_wait",
                Category::Browser,
                Tier::Read,
                "Wait for one condition: selector, dom_settled, htmx_settled, navigation, network_idle or challenge_cleared. Use 'condition' ('selector' also needs the selector argument); the boolean arguments of the same names are aliases, and exactly one may be given. After a click or key press, navigation waits for the new document (navigated:false if none starts within navigation_timeout_ms). With only timeout_ms and no condition it sleeps that long (max 30000).",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "condition": { "type": "string", "enum": ["selector", "dom_settled", "htmx_settled", "navigation", "network_idle", "challenge_cleared"] },
                        "selector": { "type": "string", "description": "CSS selector to wait for (alias for condition 'selector')" },
                        "dom_settled": { "type": "boolean" },
                        "htmx_settled": { "type": "boolean" },
                        "navigation": { "type": "boolean" },
                        "network_idle": { "type": "boolean" },
                        "challenge_cleared": { "type": "boolean" },
                        "timeout_ms": { "type": "integer" },
                        "navigation_timeout_ms": { "type": "integer", "description": "navigation only: how long to expect a navigation a click or key press has not started (0-30000, default 2000)" }
                    }),
                    json!([]),
                ),
            ).details(
                "Wait for a settle signal: a selector to appear, dom_settled (no DOM mutation for >=150ms; animation frames \
                 are not tracked), htmx_settled (HTMX requests and DOM swaps settled; right after a browser_act it also \
                 waits up to 1.5s for an htmx request to start; errors if htmx is not present on the page), navigation to \
                 complete (after a goto, reload, click, submit or key press in this session it waits for the NEW document, \
                 not the one being left; a click that starts no navigation within navigation_timeout_ms, default 2s, settles \
                 on the loaded page with navigated:false; raise it for a handler that navigates later than that), \
                 network_idle (fetch/XHR started after a browser_act are tracked; settled when none is in flight and none \
                 began or finished for 500ms, and not before a navigation that act may have started has happened), or \
                 verification challenge clearance. Prefer 'condition'; the other arguments are aliases. Give exactly one \
                 condition: the aliases conflict with it and with each other, and a boolean alias only selects its \
                 condition when true.\n\n\
                 `navigation_timeout_ms` (navigation only): how long (ms, 0-30000, default 2000) to keep expecting a \
                 navigation that a click, submit or key press has not started yet, before settling on the loaded page with \
                 navigated:false. Does not apply after goto, reload, back or forward, which always navigate; timeout_ms \
                 still bounds the whole wait.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_challenge",
                Category::Browser,
                Tier::Standard,
                "Detect or wait for a CAPTCHA or 2FA challenge that a person must solve. Shows a HUD in the page and resumes when it clears.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "action": { "type": "string", "enum": ["detect", "wait", "hud_show", "hud_hide"], "description": "default detect" },
                        "timeout_ms": { "type": "integer", "description": "max wait for clearance (default 30000)" },
                        "kind": { "type": "string", "description": "challenge kind override for hud_show" }
                    }),
                    json!([]),
                ),
            ).details(
                "Mixed-initiative CAPTCHA / 2FA detector and handshake. Pauses execution, shows a non-intrusive HUD in the \
                 browser informing the user to solve the verification, and auto-resumes in <=50ms upon clearance.",
            ),
            ToolDescriptor::new(
                "browser_record",
                Category::Browser,
                Tier::Standard,
                "Observe a tab's interactions (a person's and the agent's) and turn them into browser_flow steps. start, stop (name saves the flow) or status. Secret fields become secret_ref steps. Chrome only.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "action": { "type": "string", "enum": ["start", "stop", "status"], "description": "default status" },
                        "name": { "type": "string", "description": "stop: flow name to save" },
                        "dialogs": { "type": "string", "enum": ["human", "accept", "dismiss"], "description": "start: who answers JavaScript dialogs while recording. human needs a visible browser (its default); else dismiss" }
                    }),
                    json!([]),
                ),
            ).details(
                "Shadow observation & macro learning mode (Ghost Mode). Observes interactions in a tab (a person's, and the \
                 agent's own browser_act and browser_fill_form actions; events the page's own script fakes, such as \
                 el.click() or dispatchEvent, are ignored), across page loads and navigations (a link or form post becomes \
                 a wait for the next page, a typed URL or reload a goto), debounces keystrokes and click bursts, strips \
                 noise, and synthesizes clean, deterministic browser_flow steps. Secret fields are never recorded: they \
                 become steps with a secret_ref, supplied as secrets when the flow runs. Chrome only.\n\n\
                 JavaScript dialogs raised while recording are answered as 'dialogs' says: by the person at a visible \
                 window by default (the recording keeps the answer as a dialog step the flow replays before the action that \
                 raised it; a prompt's typed text is not kept), by the recorder in a headless browser (dismiss unless \
                 browser_dialog says accept). `dialogs` (start) is who answers the page's JavaScript dialogs \
                 (confirm/prompt/alert/beforeunload) while recording. human: nobody does, so the person at the browser \
                 window answers and the recording keeps how they did (needs a visible browser; the default there). accept / \
                 dismiss: the recorder answers (dismiss, or the tab's browser_dialog policy, is the default for a headless \
                 browser). Every confirm/prompt/beforeunload becomes a dialog step in the flow. `name` is an optional flow \
                 name to auto-save to the flow store upon stop.",
            ),
            ToolDescriptor::new(
                "browser_screenshot",
                Category::Browser,
                Tier::Read,
                "Capture a PNG of the page, or of one element by ref. Returned inline as an image; save=true writes it to agentctl's media directory and returns {path, width, height, bytes} instead.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "ref": { "type": "string", "description": "element ref (default: whole page)" },
                        "save": { "type": "boolean" }
                    }),
                    json!([]),
                ),
            ).details(
                "With save=true the PNG is written to agentctl's media directory (screenshots/, a generated file name) and \
                 only {path, width, height, bytes} comes back, with no image payload (default false). The newest 200 saved \
                 screenshots are kept; older ones are deleted.",
            ),
            ToolDescriptor::new(
                "browser_screencast",
                Category::Browser,
                Tier::Standard,
                "Record a tab to an mp4 video (browser_record is different: it learns replayable flows). start, stop (returns the video path) or status. One recording per tab. Not on Safari.",
                obj(
                    json!({
                        "action": { "type": "string", "enum": ["start", "stop", "status"], "description": "default status" },
                        "target_id": { "type": "string", "description": "tab (default: active tab)" },
                        "recording_id": { "type": "string", "description": "stop: instead of target_id" },
                        "fps": { "type": "integer", "description": "start: 1 to 30 (default 15)" },
                        "quality": { "type": "integer", "description": "start: JPEG quality 30 to 95 (default 80)" },
                        "max_seconds": { "type": "integer", "description": "start: auto-stop after this long, 1 to 1800 (default 300)" },
                        "keep_frames": { "type": "boolean", "description": "stop: keep the JPEG frames after encoding" }
                    }),
                    json!([]),
                ),
            ).details(
                "Record a tab to an mp4 video (browser_record is something else: it learns a replayable flow of steps, not \
                 video). start begins capturing the page at fps (default 15) on a dedicated session that keeps the page \
                 rendering even if its window is hidden or unfocused; stop ends it and encodes frames.ffconcat with ffmpeg \
                 (variable frame rate, real timestamps, 30 fps H.264) when ffmpeg is on PATH, else it keeps the frames and \
                 says how to encode them. Files land in agentctl's media directory under screencasts/<recording_id>/; the \
                 result gives the path. One recording per tab, which stops by itself at max_seconds. The showcase cursor and \
                 ripples are part of the page, so they appear in the video. Not available on Safari.\n\n\
                 `target_id`: start: the tab to record (default: the active tab); stop: the tab whose recording to stop. \
                 `recording_id` (stop) names the recording to stop instead of target_id. `keep_frames` (stop) keeps the \
                 JPEG frames and frames.ffconcat after a successful encode (default false).",
            ),
            ToolDescriptor::new(
                "browser_viewport",
                Category::Browser,
                Tier::Standard,
                "Emulate a viewport for responsive testing (width, height, mobile, scale). width=0 or omitted clears the override.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "width": { "type": "integer", "description": "css px; 0 clears" },
                        "height": { "type": "integer", "description": "css px" },
                        "mobile": { "type": "boolean", "description": "emulate a mobile device" },
                        "scale": { "type": "number", "description": "device scale factor (default 1)" }
                    }),
                    json!([]),
                ),
            ).details(
                "Override the page's device metrics (width/height, optionally mobile and a device scale factor). Call with \
                 width=0 (or omitted) to clear the override and restore the real window size. `mobile` emulates a mobile \
                 device (touch, meta viewport).",
            ),
            ToolDescriptor::new(
                "browser_eval",
                Category::Browser,
                Tier::Dangerous,
                "Run JavaScript in the page; returns the last statement's value, JSON-serialized (a returned promise is awaited). Arbitrary code execution. A script that navigates returns {navigated:true, value:null}.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "expression": { "type": "string" },
                        "timeout_ms": { "type": "integer", "description": "default 10000 (100 to 60000); a timeout is an error" },
                        "detached": { "type": "boolean", "description": "return {started:true} without waiting for the result (Chrome)" }
                    }),
                    json!(["expression"]),
                ),
            ).details(
                "On Safari, a page whose CSP forbids eval gets the code run without eval: an expression works as usual, but \
                 statements need an explicit `return` to produce a result.\n\n\
                 `timeout_ms`: stop waiting after this many ms (default 10000, clamped to 100-60000). Chrome stops script \
                 that is still running; a timeout is an error that says so. Async work already scheduled (timers, pending \
                 promises) can keep running in the page. Not enforced on Safari. `detached` starts the script and returns \
                 {started:true} without waiting for a promise it returns or for its result (Chrome only); its synchronous \
                 part still runs within the call and timeout_ms. A later rejection goes to the page console.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_dialog",
                Category::Browser,
                Tier::Standard,
                "JavaScript dialogs (alert, confirm, prompt, beforeunload) are answered automatically and dismissed by default. Set policy='accept' only when confirming is what you intend. Omit policy to read the setting and the dialogs seen.",
                json!({
                    "type": "object",
                    "properties": {
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "policy": { "type": "string", "enum": ["dismiss", "accept"] },
                        "prompt_text": { "type": "string", "description": "text for prompt() when accepting" }
                    },
                    "required": []
                }),
            ).details(
                "Inspect and control how the page's JavaScript dialogs (alert/confirm/prompt/beforeunload) are answered. \
                 They are answered automatically (an unanswered dialog blocks the tab) and dismissed by default; call with \
                 policy='accept' only when confirming is what you actually intend. Omit 'policy' to read the current \
                 setting and the dialogs seen so far.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_network",
                Category::Browser,
                Tier::Dangerous,
                "Network control: log (requests for a bounded window), intercept (headers: {block: [url patterns]}), set_headers (headers).",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "action": { "type": "string", "enum": ["log", "intercept", "set_headers"] },
                        "filter": { "type": "string", "description": "log: substring filter on rows" },
                        "duration_ms": { "type": "integer", "description": "log window, 100 to 30000" },
                        "headers": {
                            "type": "object",
                            "description": "set_headers: the headers. intercept: {block: [url patterns]}"
                        }
                    }),
                    json!(["action"]),
                ),
            ).details(
                "Network control. log: record requests and responses for a bounded window (URLs, methods, statuses: header \
                 values and cookies are deliberately omitted). intercept: block URL patterns via headers.block. \
                 set_headers: extra HTTP headers.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_cookies",
                Category::Browser,
                Tier::Dangerous,
                "Cookie access: get (values redacted), set, or clear.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "action": { "type": "string", "enum": ["get", "set", "clear"] },
                        "cookie": { "type": "object" }
                    }),
                    json!(["action"]),
                ),
            ),
            ToolDescriptor::new(
                "browser_capture",
                Category::Browser,
                Tier::Dangerous,
                "Capture fetch/XHR calls (with bodies) and console errors across navigations: start, read (only_errors, filter), clear. Bodies can hold secrets.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "action": { "type": "string", "enum": ["start", "read", "clear"] },
                        "only_errors": { "type": "boolean", "description": "read: only non-2xx or failed requests" },
                        "filter": { "type": "string", "description": "read: substring filter on rows" }
                    }),
                    json!([]),
                ),
            ).details(
                "Regression-test capture. 'start' installs a page hook (persists across navigations) that records fetch/XHR \
                 calls with request+response bodies and console errors/uncaught exceptions. 'read' returns them \
                 ('only_errors' keeps failed requests; 'filter' is a substring). 'clear' empties the buffers. Bodies can \
                 contain secrets, so this is off unless enabled.",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_assert",
                Category::Browser,
                Tier::Read,
                "Check the page in one call, optionally settling first; returns {passed, checks} and errors when it fails. Clauses: text, not_text, url, selector (+min_count), no_console_errors and no_failed_requests (need browser_capture), and the UX clauses a11y, style, component, visual, ux.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab id (default: active tab)" },
                        "text": { "type": "string", "description": "text must be present" },
                        "not_text": { "type": "string", "description": "text must be absent" },
                        "url": { "type": "string", "description": "URL must contain this" },
                        "selector": { "type": "string", "description": "css selector must match" },
                        "min_count": { "type": "integer", "description": "selector match count at least this (default 1)" },
                        "no_console_errors": { "type": "boolean" },
                        "no_failed_requests": { "type": "boolean" },
                        "within": { "type": "string", "description": "css root for the a11y, style and component checks" },
                        "a11y": { "description": "true, or {ignore:[rules], contrast:false, target_size:false, contrast_sample:N}" },
                        "style": { "type": "object", "description": "allow-lists {colors:[], fonts:[], font_sizes:[], spacing:[]}; off-token values fail" },
                        "component": { "type": "object", "description": "{selector, visible, role, states:{disabled,expanded,checked,...}}" },
                        "visual": { "description": "baseline name, or {name, tolerance, ref}" },
                        "ux": { "type": "object", "description": "{dims:[clarity,hierarchy,affordance,consistency], gate:false, min:0.5}" },
                        "wait_selector": { "type": "string", "description": "settle first: wait for this selector" },
                        "wait_dom_settled": { "type": "boolean", "description": "settle first: DOM settled" },
                        "wait_network_idle": { "type": "boolean", "description": "settle first: network idle" },
                        "timeout_ms": { "type": "integer", "description": "settle timeout (default 8000)" }
                    }),
                    json!([]),
                ),
            ).details(
                "Settle (optional) then check the page in one call; returns {passed, checks} and errors when it fails. \
                 Functional clauses: text/not_text (in page text), url (substring), selector (+min_count), \
                 no_console_errors and no_failed_requests (need browser_capture started). UX clauses: a11y (built-in WCAG \
                 rules: alt text, form labels, control names, contrast, target size, positive tabindex, duplicate ids, page \
                 lang), style (design-token conformance: colors/fonts/font_sizes/spacing allow-lists), component \
                 (role/visible/states of one element), visual (screenshot vs a saved baseline: first run saves it, later \
                 runs diff within tolerance), ux (judge-scored heuristics: clarity/hierarchy/affordance/consistency; \
                 advisory unless gate=true). 'within' scopes the DOM UX clauses to a component subtree. Settle first with \
                 wait_selector or wait_network_idle.\n\n\
                 `a11y`: true, or {ignore:[rules], contrast:false, target_size:false, contrast_sample:N} to run the \
                 built-in accessibility audit. `style`: design-token conformance, {colors:[], fonts:[], font_sizes:[], \
                 spacing:[]} allow-lists; off-token values fail. `component`: {selector, visible, role, \
                 states:{disabled,expanded,checked,...}} assertions on one element. `visual`: baseline name, or {name, \
                 tolerance, ref}; first run saves the baseline, later runs diff the screenshot within tolerance (default \
                 0.01). `ux`: judge-scored review {dims:[clarity,hierarchy,affordance,consistency], gate:false, min:0.5}; \
                 advisory unless gate=true. `within` scopes a11y/style/component checks to this css root (component \
                 testing). `no_console_errors` asserts no captured console errors and `no_failed_requests` no captured \
                 non-2xx/failed requests (both need browser_capture). `wait_selector`, `wait_dom_settled` and \
                 `wait_network_idle` settle first; `timeout_ms` is the settle timeout (default 8000).",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_flow",
                Category::Browser,
                Tier::Standard,
                "Save and replay a UI test: save (name + steps), run (name, target_id), list, get, delete. A step is {op: navigate|act|wait|capture|assert|dialog, ...} with the same fields as those tools, e.g. {op:'act',by:'text',query:'Login',action:'click'}. A secret step holds no value: {op:'act',action:'type',query:'#pw',secret:true,secret_ref:'pw'}, and run gets secrets:{pw:'...'}.",
                obj(
                    json!({
                        "action": { "type": "string", "enum": ["save", "run", "list", "get", "delete"] },
                        "name": { "type": "string" },
                        "target_id": { "type": "string", "description": "run: tab (default: active tab)" },
                        "steps": { "type": "array", "items": { "type": "object" }, "description": "save: ordered steps" },
                        "continue_on_error": { "type": "boolean", "description": "run: keep going past a failed step" },
                        "secrets": { "type": "object", "description": "run: values for the steps' secret_ref names; never stored" }
                    }),
                    json!(["action"]),
                ),
            ).details(
                "'save' (name + steps) records a flow; 'run' (name + target_id) replays it deterministically, stopping at \
                 the first failing step (set continue_on_error to run all); 'list'/'get'/'delete' manage them. A step is \
                 {op: navigate|act|wait|capture|assert|dialog, ...} using the same fields as those tools (e.g. \
                 {op:'act',by:'text',query:'Login',action:'click'}, {op:'assert',text:'Welcome'}). A secret step never \
                 holds its value: use {op:'act',action:'type',query:'#pw',secret:true,secret_ref:'pw'} and pass \
                 secrets:{pw:'...'} to 'run'; 'save' refuses a secret step with a literal value. A green run never needs \
                 a model.\n\n\
                 `secrets` (run): values for the steps' secret_ref names, e.g. {pw: '...'}; used in memory for this run \
                 only, never stored, redacted from the audit log. A missing one fails the run before any step runs. \
                 `target_id` (run) is the tab to replay against (default: the active tab).",
            ).untrusted_output(),
            ToolDescriptor::new(
                "browser_showcase",
                Category::Browser,
                Tier::Standard,
                "Demo visuals drawn in the tab: animated cursor, click ripples, typing HUD. Decoration only. The result's `rendered` says whether the cursor is really on the page.",
                obj(
                    json!({
                        "target_id": { "type": "string", "description": "tab (default: active tab)" },
                        "enabled": { "type": "boolean" },
                        "speed": { "type": "string", "enum": ["cinematic", "demo", "snappy", "off"] },
                        "click_ripple": { "type": "boolean" },
                        "typing_hud": { "type": "boolean" },
                        "cursor_style": { "type": "string", "enum": ["glow_arrow", "neon_cyan", "minimal_dot"] },
                        "glide_ms": { "type": "integer", "description": "glide duration, 0 to 3000" },
                        "cursor_size": { "type": "integer", "description": "px, 16 to 96 (default 32)" }
                    }),
                    json!([]),
                ),
            ).details(
                "Configure visual flair for demos, screencasts, and presentations: animated virtual SVG cursor, smooth \
                 cubic-bezier gliding, click ripples, and floating typing HUD, drawn inside the tab on Chrome and Safari. \
                 While on, a browser_act on Chrome also moves the real pointer (trusted mousemove events) so hover styles \
                 and mouse listeners fire. The result's `rendered` says whether the cursor really is on the page, with a \
                 `warning` when it is not. Decoration only: it never changes an action's result or error, and the typing \
                 HUD masks secrets and password/one-time-code fields.\n\n\
                 `enabled` turns the visual overlays on or off; `speed` is the gliding speed preset; `click_ripple` expands \
                 glowing shockwave rings on click; `typing_hud` displays floating action/typing badges next to the cursor; \
                 `cursor_style` is the pointer style; `glide_ms` a custom glide duration in milliseconds, 0-3000 (larger \
                 values are capped); `cursor_size` the pointer size in px, 16-96 (default 32; values outside are clamped).",
            ).idempotent(true),
        ]
    }

    /// Answering a page's own confirmation on the user's behalf is a decision,
    /// not plumbing: `confirm("Delete this account?")` becomes "yes". Dismissal
    /// (the default) needs no approval because it is the null answer.
    fn consent_prompt(&self, name: &str, args: &Value) -> Option<String> {
        let accepts = match name {
            "browser_dialog" => args.get("policy").and_then(Value::as_str) == Some("accept"),
            // Recording that answers every dialog "yes" is the same decision.
            "browser_record" => {
                args.get("action").and_then(Value::as_str) == Some("start")
                    && args.get("dialogs").and_then(Value::as_str) == Some("accept")
            }
            // So is a flow step that does, whether it is being saved or run.
            "browser_flow" => {
                let steps = match args.get("action").and_then(Value::as_str) {
                    Some("save") => args.get("steps").and_then(Value::as_array).cloned(),
                    Some("run") => args
                        .get("name")
                        .and_then(Value::as_str)
                        .zip(self.flows.as_ref())
                        .and_then(|(n, s)| s.get(n).ok().flatten())
                        .map(|f| f.steps),
                    _ => None,
                };
                steps.is_some_and(|s| s.iter().any(is_accepting_dialog_step))
            }
            _ => false,
        };
        accepts.then(|| "Automatically ACCEPT JavaScript dialogs in this tab? Any confirm() the page raises will be answered 'yes' without further prompting.".to_string())
    }

    /// Stop browsers this session launched. Without this, a `serve` that ends
    /// leaves a headless Chrome and its profile directory behind for good.
    fn shutdown(&self) {
        self.backend.shutdown();
    }

    async fn call(&self, name: &str, mut args: Value, _ctx: &CallCtx) -> Envelope {
        respell_args(name, &mut args);
        fix_xpath_args(&mut args);
        // One place gives every tab-scoped tool its `target_id`, so the tools
        // read it as they always did. A default is reported back.
        let mut defaulted = None;
        if defaults_target(name, &args) {
            match self.resolve_target(&args).await {
                Ok(Some((target, was_default))) => {
                    args["target_id"] = json!(target);
                    defaulted = was_default.then_some(target);
                }
                Ok(None) => {}
                Err(e) => return e.into_envelope(name),
            }
        }
        let mut env = self.dispatch(name, &args).await;
        if let (Some(t), true, Some(Value::Object(m))) = (defaulted, env.ok, env.data.as_mut()) {
            m.entry("target_id").or_insert(json!(t));
        }
        env
    }
}

impl BrowserModule {
    async fn dispatch(&self, name: &str, args: &Value) -> Envelope {
        match name {
            "browser_connect" => self.connect(args).await,
            "browser_disconnect" => self.disconnect(args).await,
            "browser_tabs" => self.tabs(args).await,
            "browser_navigate" => self.navigate(args).await,
            "browser_snapshot" => self.snapshot(args).await,
            "browser_query" => self.query(args).await,
            "browser_act" => self.act(args).await,
            "browser_upload" => self.upload(args).await,
            "browser_fill_form" => self.fill_form(args).await,
            "browser_extract" => self.extract(args).await,
            "browser_profile" => self.profile(args).await,
            "browser_wait" => self.wait(args).await,
            "browser_challenge" => self.challenge(args).await,
            "browser_record" => self.record(args).await,
            "browser_showcase" => self.showcase(args).await,
            "browser_screenshot" => self.screenshot(args).await,
            "browser_screencast" => self.screencast(args).await,
            "browser_viewport" => self.viewport(args).await,
            "browser_eval" => self.eval(args).await,
            "browser_dialog" => self.dialog(args).await,
            "browser_network" => self.network(args).await,
            "browser_cookies" => self.cookies(args).await,
            "browser_capture" => self.capture(args).await,
            "browser_assert" => self.assert(args).await,
            "browser_flow" => self.flow(args).await,
            "browser_branch" => self.branch(args).await,
            "browser_checkpoint" => self.checkpoint(args).await,
            other => Envelope::fail(other, ErrorCode::InvalidArgs, "unknown tool"),
        }
    }
}

#[cfg(test)]
mod act_tests {
    use super::*;
    use crate::backend::{Locator, Shot};
    use std::sync::Mutex;

    /// Records how `act` was asked to locate the element; everything else is a
    /// no-op error, since these tests only exercise the tool-layer wiring.
    #[derive(Default)]
    struct Recorder {
        acts: Mutex<Vec<String>>,
        captures: Mutex<Vec<String>>,
        viewports: Mutex<Vec<String>>,
        forms: Mutex<Vec<Value>>,
        uploads: Mutex<Vec<Vec<String>>>,
        extracts: Mutex<Vec<Value>>,
        profiles: Mutex<Vec<String>>,
        /// `navigate` / `profile_restore` calls, in the order they happened.
        order: Mutex<Vec<String>>,
        connects: std::sync::atomic::AtomicUsize,
        fail_restore: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl BrowserBackend for Recorder {
        async fn connect(&self, _p: Option<u16>, _l: Option<Value>) -> Result<Value, BrowserError> {
            self.connects
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "browser_id": 1 }))
        }
        async fn browser_ids(&self) -> Vec<u32> {
            vec![1]
        }
        async fn disconnect(&self, _b: u32, _k: bool) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn tabs(
            &self,
            _b: u32,
            _a: &str,
            _t: Option<&str>,
            _u: Option<&str>,
        ) -> Result<Value, BrowserError> {
            Ok(json!({
                "tabs": [{"target_id": "T", "url": "about:blank"}]
            }))
        }
        async fn navigate(
            &self,
            _t: &str,
            a: &str,
            _u: Option<&str>,
        ) -> Result<Value, BrowserError> {
            self.order.lock().unwrap().push(format!("navigate:{a}"));
            Ok(json!({ "ok": true, "action": a }))
        }
        async fn snapshot(
            &self,
            _t: &str,
            _m: &str,
            _r: Option<&str>,
        ) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn query(
            &self,
            _t: &str,
            _by: &str,
            _q: &str,
            _a: bool,
            _text: Option<&str>,
        ) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn act(
            &self,
            _t: &str,
            locator: Locator<'_>,
            action: &str,
            _v: Option<&str>,
        ) -> Result<Value, BrowserError> {
            let desc = match locator {
                Locator::Focused => "focused".to_string(),
                Locator::Ref(r) => format!("ref:{r}"),
                Locator::Selector {
                    by,
                    query,
                    within,
                    text,
                    index,
                } => {
                    let mut s = format!("sel:{by}:{query}");
                    if let Some(w) = within {
                        s.push_str(&format!(":within={w}"));
                    }
                    if let Some(t) = text {
                        s.push_str(&format!(":text={t}"));
                    }
                    if let Some(i) = index {
                        s.push_str(&format!(":index={i}"));
                    }
                    s
                }
            };
            self.acts.lock().unwrap().push(desc);
            Ok(json!({ "ok": true, "action": action }))
        }
        async fn upload(
            &self,
            _t: &str,
            _l: Locator<'_>,
            files: &[String],
        ) -> Result<Value, BrowserError> {
            self.uploads.lock().unwrap().push(files.to_vec());
            Ok(json!({ "ok": true, "input_multiple": true }))
        }
        async fn wait(
            &self,
            _t: &str,
            c: &str,
            _a: Option<&str>,
            _ms: u64,
        ) -> Result<Value, BrowserError> {
            Ok(json!({ "settled": true, "condition": c }))
        }
        async fn screenshot(&self, _t: &str, _r: Option<&str>) -> Result<Shot, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn eval(&self, _t: &str, _e: &str) -> Result<Value, BrowserError> {
            // Only the page-load probe is ever evaluated by the tool layer.
            Ok(json!({ "result": "https://example.com/app|complete" }))
        }
        async fn network(
            &self,
            _t: &str,
            _a: &str,
            _f: Option<&str>,
            _h: Option<Value>,
            _d: Option<u64>,
        ) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn dialog(&self, _t: &str, _p: Option<DialogPolicy>) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn cookies(
            &self,
            _t: &str,
            _a: &str,
            _c: Option<Value>,
        ) -> Result<Value, BrowserError> {
            Err(BrowserError::Failed("n/a".into()))
        }
        async fn capture(&self, _t: &str, action: &str, _o: &Value) -> Result<Value, BrowserError> {
            self.captures.lock().unwrap().push(action.to_string());
            Ok(json!({ "ok": true, "action": action }))
        }
        async fn assert(&self, _t: &str, spec: &Value) -> Result<Value, BrowserError> {
            // Echo a passed/failed result driven by a test-only `_pass` flag.
            let passed = spec.get("_pass").and_then(Value::as_bool).unwrap_or(true);
            Ok(json!({ "passed": passed, "checks": [{"name":"x","ok":passed}] }))
        }
        async fn set_viewport(
            &self,
            _t: &str,
            w: u32,
            h: u32,
            m: bool,
            s: f64,
        ) -> Result<Value, BrowserError> {
            self.viewports
                .lock()
                .unwrap()
                .push(format!("{w}x{h} mobile={m} scale={s}"));
            Ok(json!({ "width": w, "height": h, "mobile": m }))
        }
        async fn fill_form(
            &self,
            _t: &str,
            fields: &Value,
            submit: Option<&Value>,
        ) -> Result<Value, BrowserError> {
            self.forms.lock().unwrap().push(fields.clone());
            let count = fields.as_array().map(|a| a.len()).unwrap_or(0);
            Ok(json!({ "filled": count, "submitted": submit.is_some() }))
        }
        async fn extract(
            &self,
            _t: &str,
            schema: &Value,
            within: Option<&str>,
        ) -> Result<Value, BrowserError> {
            self.extracts.lock().unwrap().push(schema.clone());
            Ok(json!({
                "data": { "extracted": true },
                "within": within
            }))
        }
        async fn profile_state(&self, _t: &str) -> Result<Value, BrowserError> {
            self.profiles.lock().unwrap().push("state".into());
            Ok(json!({
                "url": "https://example.com/app",
                "cookies": [{"name": "sid", "value": "xyz"}],
                "local_storage": {"token": "abc"},
                "session_storage": {}
            }))
        }
        async fn profile_restore(&self, _t: &str, state: &Value) -> Result<Value, BrowserError> {
            self.profiles.lock().unwrap().push("restore".into());
            self.order.lock().unwrap().push("restore".into());
            if self.fail_restore.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(json!({ "ok": false, "error": "SecurityError: storage denied" }));
            }
            let cookies = state
                .get("cookies")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            Ok(json!({
                "restored_cookies": cookies,
                "local_storage_keys": 1,
                "session_storage_keys": 0
            }))
        }
        async fn showcase(
            &self,
            target: &str,
            config: Option<crate::showcase::ShowcaseConfig>,
        ) -> Result<Value, BrowserError> {
            let cfg = config.unwrap_or_default();
            Ok(json!({
                "target_id": target,
                "enabled": cfg.enabled,
                "speed": cfg.speed.as_str(),
                "glide_ms": cfg.glide_ms(),
                "click_ripple": cfg.click_ripple,
                "typing_hud": cfg.typing_hud
            }))
        }
    }

    fn module() -> (BrowserModule, Arc<Recorder>) {
        let rec = Arc::new(Recorder::default());
        (BrowserModule::new(rec.clone()), rec)
    }

    fn module_with_flows(tag: &str) -> BrowserModule {
        use crate::flow::FlowStore;
        let mut p = std::env::temp_dir();
        p.push(format!(
            "agentctl-flowtool-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        BrowserModule::new(Arc::new(Recorder::default())).with_flow_store(FlowStore::new(p, 50, 50))
    }

    #[tokio::test]
    async fn answering_dialogs_yes_needs_consent_however_it_is_asked_for() {
        let m = module_with_flows("dialog-consent");
        let asks = |name: &str, args: Value| m.consent_prompt(name, &args).is_some();
        // The direct tool, and both new ways to make every confirm "yes".
        assert!(asks("browser_dialog", json!({"policy": "accept"})));
        assert!(asks(
            "browser_record",
            json!({"action": "start", "dialogs": "accept"})
        ));
        let accepting = json!([{"op": "dialog", "policy": "accept", "type": "confirm"}]);
        assert!(asks(
            "browser_flow",
            json!({"action": "save", "name": "f", "steps": accepting})
        ));
        // A stored flow that accepts asks when it is run, not only when saved.
        let saved = m
            .call(
                "browser_flow",
                json!({"action": "save", "name": "yes", "steps": accepting}),
                &CallCtx::new("test", mcp_types::CancelToken::new()),
            )
            .await;
        assert!(saved.ok, "{saved:?}");
        assert!(asks(
            "browser_flow",
            json!({"action": "run", "name": "yes"})
        ));
        // Dismissing, the default, and a flow with no accepting step do not.
        assert!(!asks("browser_dialog", json!({"policy": "dismiss"})));
        assert!(!asks("browser_record", json!({"action": "start"})));
        assert!(!asks(
            "browser_record",
            json!({"action": "start", "dialogs": "dismiss"})
        ));
        assert!(!asks(
            "browser_flow",
            json!({"action": "save", "name": "f", "steps": [{"op": "dialog", "policy": "dismiss"}]})
        ));
        assert!(!asks(
            "browser_flow",
            json!({"action": "run", "name": "nope"})
        ));
    }

    fn module_with_profiles_and_rec(tag: &str) -> (BrowserModule, Arc<Recorder>) {
        use crate::profile::ProfileStore;
        let mut p = std::env::temp_dir();
        p.push(format!(
            "agentctl-profiletool-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        let rec = Arc::new(Recorder::default());
        (
            BrowserModule::new(rec.clone()).with_profile_store(ProfileStore::new(p, 50)),
            rec,
        )
    }

    fn module_with_profiles(tag: &str) -> BrowserModule {
        module_with_profiles_and_rec(tag).0
    }

    #[tokio::test]
    async fn a_ref_locates_by_ref() {
        let (m, rec) = module();
        let e = m
            .act(&json!({"target_id":"T","ref":"/html/body[1]/button[1]","action":"click"}))
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(rec.acts.lock().unwrap()[0], "ref:/html/body[1]/button[1]");
    }

    #[tokio::test]
    async fn a_query_locates_by_selector_in_one_call() {
        let (m, rec) = module();
        let e = m
            .act(&json!({"target_id":"T","by":"text","query":"Login","action":"click"}))
            .await;
        assert!(e.ok, "{e:?}");
        // No separate browser_query was needed: the selector reached act directly.
        assert_eq!(rec.acts.lock().unwrap()[0], "sel:text:Login");
    }

    #[tokio::test]
    async fn a_query_defaults_to_auto_when_by_is_omitted() {
        let (m, rec) = module();
        let e = m
            .act(&json!({"target_id":"T","query":"#save","action":"click"}))
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(rec.acts.lock().unwrap()[0], "sel:auto:#save");
    }

    #[tokio::test]
    async fn a_query_with_scoping_forwards_within_text_and_index() {
        let (m, rec) = module();
        let e = m
            .act(&json!({
                "target_id": "T",
                "query": "button",
                "by": "css",
                "within": ".table-row",
                "text": "Submit",
                "index": 2,
                "action": "click"
            }))
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(
            rec.acts.lock().unwrap()[0],
            "sel:css:button:within=.table-row:text=Submit:index=2"
        );
    }

    #[tokio::test]
    async fn flow_replays_act_step_with_compound_scoping() {
        let (m, rec) = module();
        let (ok, _) = m
            .run_step(
                "T",
                &json!({
                    "op": "act",
                    "query": "button.emr-btn",
                    "within": "table tbody tr:first-child",
                    "text": "Dispense",
                    "index": 0,
                    "action": "click"
                }),
            )
            .await;
        assert!(ok);
        assert_eq!(
            rec.acts.lock().unwrap()[0],
            "sel:css:button.emr-btn:within=table tbody tr:first-child:text=Dispense:index=0"
        );
    }

    #[tokio::test]
    async fn neither_ref_nor_query_is_an_invalid_argument_and_never_calls_the_backend() {
        let (m, rec) = module();
        let e = m.act(&json!({"target_id":"T","action":"click"})).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::InvalidArgs);
        assert!(
            rec.acts.lock().unwrap().is_empty(),
            "backend must not be called"
        );
    }

    #[tokio::test]
    async fn capture_routes_the_action_and_defaults_to_read() {
        let (m, rec) = module();
        assert!(
            m.capture(&json!({"target_id":"T","action":"start"}))
                .await
                .ok
        );
        assert!(m.capture(&json!({"target_id":"T"})).await.ok); // default
        assert_eq!(*rec.captures.lock().unwrap(), vec!["start", "read"]);
    }

    #[tokio::test]
    async fn viewport_passes_dimensions_through_and_a_flow_step_reaches_the_backend() {
        let (m, rec) = module();
        assert!(
            m.viewport(&json!({"target_id":"T","width":390,"height":844,"mobile":true}))
                .await
                .ok
        );
        // A viewport step in a replayed flow reaches the same backend call.
        let (ok, _) = m
            .run_step("T", &json!({"op":"viewport","width":1280,"height":800}))
            .await;
        assert!(ok);
        let v = rec.viewports.lock().unwrap();
        assert_eq!(v[0], "390x844 mobile=true scale=1");
        assert_eq!(v[1], "1280x800 mobile=false scale=1");
    }

    #[tokio::test]
    async fn capture_without_a_target_is_an_invalid_argument() {
        let (m, rec) = module();
        let e = m.capture(&json!({"action":"read"})).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::InvalidArgs);
        assert!(rec.captures.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_passing_assert_is_ok_and_a_failing_one_is_an_error_carrying_the_checks() {
        let (m, _) = module();
        let ok = m.assert(&json!({"target_id":"T","_pass":true})).await;
        assert!(ok.ok);
        assert_eq!(ok.data.unwrap()["passed"], true);

        let bad = m.assert(&json!({"target_id":"T","_pass":false})).await;
        assert!(!bad.ok, "a failed assertion must surface as an error");
        assert_eq!(bad.error.as_ref().unwrap().code, ErrorCode::ActionFailed);
        // The per-check detail still rides along for the harness.
        assert_eq!(bad.data.unwrap()["passed"], false);
    }

    #[tokio::test]
    async fn a_saved_flow_replays_green_and_reports_each_step() {
        let m = module_with_flows("green");
        let save = m
            .flow(&json!({"action":"save","name":"login","steps":[
                {"op":"navigate","url":"https://x"},
                {"op":"wait","network_idle":true},
                {"op":"assert","target_id":"ignored","_pass":true}
            ]}))
            .await;
        assert!(save.ok, "{save:?}");
        let run = m
            .flow(&json!({"action":"run","name":"login","target_id":"T"}))
            .await;
        assert!(run.ok, "green flow should pass: {run:?}");
        let d = run.data.unwrap();
        assert_eq!(d["passed"], true);
        assert_eq!(d["ran"], 3);
    }

    #[tokio::test]
    async fn a_failing_step_stops_the_run_and_marks_it_failed() {
        let m = module_with_flows("red");
        m.flow(&json!({"action":"save","name":"f","steps":[
            {"op":"navigate","url":"https://x"},
            {"op":"assert","_pass":false},
            {"op":"navigate","url":"https://never-reached"}
        ]}))
        .await;
        let run = m
            .flow(&json!({"action":"run","name":"f","target_id":"T"}))
            .await;
        assert!(!run.ok, "a failing flow is an error");
        let d = run.data.unwrap();
        assert_eq!(d["passed"], false);
        assert_eq!(
            d["ran"], 2,
            "stops at the failing assert, third step not reached"
        );
    }

    #[tokio::test]
    async fn run_without_a_target_is_an_invalid_argument() {
        let m = module_with_flows("notgt");
        m.flow(&json!({"action":"save","name":"f","steps":[{"op":"navigate"}]}))
            .await;
        let e = m.flow(&json!({"action":"run","name":"f"})).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn flow_is_disabled_without_a_store() {
        let (m, _) = module();
        let e = m.flow(&json!({"action":"list"})).await;
        assert!(!e.ok, "no store configured means the tool is unavailable");
    }

    #[tokio::test]
    async fn fill_form_batches_fields_and_reports_count() {
        let (m, rec) = module();
        let e = m
            .fill_form(&json!({
                "target_id": "T",
                "fields": [
                    { "selector": "#username", "value": "alice" },
                    { "selector": "#password", "value": "secret" }
                ],
                "submit": { "selector": "#login-btn" }
            }))
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(rec.forms.lock().unwrap().len(), 1);
        let d = e.data.unwrap();
        assert_eq!(d["filled"], 2);
        assert_eq!(d["submitted"], true);
    }

    #[tokio::test]
    async fn fill_form_without_fields_fails_invalid_args() {
        let (m, _) = module();
        let e = m.fill_form(&json!({ "target_id": "T" })).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn extract_evaluates_schema() {
        let (m, rec) = module();
        let e = m
            .extract(&json!({
                "target_id": "T",
                "schema": { "title": "h1" },
                "within": ".content"
            }))
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(rec.extracts.lock().unwrap().len(), 1);
        let d = e.data.unwrap();
        assert_eq!(d["data"]["extracted"], true);
        assert_eq!(d["within"], ".content");
    }

    #[tokio::test]
    async fn wait_accepts_network_idle() {
        let (m, _) = module();
        for args in [
            json!({ "target_id": "T", "network_idle": true }),
            json!({ "target_id": "T", "condition": "network_idle" }),
        ] {
            let e = m.wait(&args).await;
            assert!(e.ok, "{e:?}");
            assert_eq!(e.data.unwrap()["condition"], "network_idle");
        }
    }

    #[tokio::test]
    async fn wait_supports_dom_settled() {
        let (m, _) = module();
        let e = m
            .wait(&json!({
                "target_id": "T",
                "dom_settled": true,
                "timeout_ms": 3000
            }))
            .await;
        assert!(e.ok, "{e:?}");
        let d = e.data.unwrap();
        assert_eq!(d["condition"], "dom_settled");
    }

    #[tokio::test]
    async fn wait_supports_htmx_settled() {
        let (m, _) = module();
        let e = m
            .wait(&json!({
                "target_id": "T",
                "htmx_settled": true,
                "timeout_ms": 3000
            }))
            .await;
        assert!(e.ok, "{e:?}");
        let d = e.data.unwrap();
        assert_eq!(d["condition"], "htmx_settled");

        let e2 = m
            .wait(&json!({
                "target_id": "T",
                "condition": "htmx_settled"
            }))
            .await;
        assert!(e2.ok, "{e2:?}");
        assert_eq!(e2.data.unwrap()["condition"], "htmx_settled");
    }

    #[tokio::test]
    async fn connect_auto_restores_named_profile() {
        let (m, rec) = module_with_profiles_and_rec("autoload");
        let save_res = m
            .profile(&json!({
                "action": "save",
                "name": "qa_saved_session",
                "target_id": "T"
            }))
            .await;
        assert!(save_res.ok, "{save_res:?}");

        let conn_res = m
            .connect(&json!({
                "profile": "qa_saved_session"
            }))
            .await;
        assert!(conn_res.ok, "{conn_res:?}");
        let data = conn_res.data.unwrap();
        assert_eq!(data["profile_restored"], "qa_saved_session");
        assert!(rec
            .profiles
            .lock()
            .unwrap()
            .contains(&"restore".to_string()));
    }

    #[tokio::test]
    async fn connect_navigates_to_the_profile_url_before_restoring() {
        let (m, rec) = module_with_profiles_and_rec("order");
        let save = m
            .profile(&json!({ "action": "save", "name": "ordered", "target_id": "T" }))
            .await;
        assert!(save.ok, "{save:?}");
        rec.order.lock().unwrap().clear();
        let conn = m.connect(&json!({ "profile": "ordered" })).await;
        assert!(conn.ok, "{conn:?}");
        assert_eq!(conn.data.unwrap()["profile_restored"], "ordered");
        // Storage writes throw on about:blank, so the page must come first.
        assert_eq!(
            *rec.order.lock().unwrap(),
            vec!["navigate:goto".to_string(), "restore".to_string()]
        );
    }

    #[tokio::test]
    async fn connect_with_unknown_profile_errors_and_does_not_connect() {
        let (m, rec) = module_with_profiles_and_rec("unknown");
        let conn = m.connect(&json!({ "profile": "no_such_profile" })).await;
        assert!(!conn.ok, "{conn:?}");
        assert_eq!(conn.error.as_ref().unwrap().code, ErrorCode::NotFound);
        assert!(conn
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("no_such_profile"));
        assert_eq!(rec.connects.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn connect_with_profile_but_no_store_errors() {
        let (m, rec) = module();
        let conn = m.connect(&json!({ "profile": "x" })).await;
        assert!(!conn.ok, "{conn:?}");
        assert_eq!(rec.connects.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn connect_reports_a_failed_restore_instead_of_claiming_it() {
        let (m, rec) = module_with_profiles_and_rec("restore_fail");
        let save = m
            .profile(&json!({ "action": "save", "name": "bad", "target_id": "T" }))
            .await;
        assert!(save.ok, "{save:?}");
        rec.fail_restore
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let conn = m.connect(&json!({ "profile": "bad" })).await;
        assert!(conn.ok, "{conn:?}");
        let data = conn.data.unwrap();
        assert_eq!(data["profile_restored"], json!(false));
        assert!(data["profile_restore_error"]
            .as_str()
            .unwrap()
            .contains("storage denied"));
        // The explicit restore action must not say "restored" either.
        let r = m
            .profile(&json!({ "action": "restore", "target_id": "T", "name": "bad" }))
            .await;
        assert!(!r.ok, "{r:?}");
    }

    #[tokio::test]
    async fn connect_auto_restores_named_profile_under_launch() {
        let (m, rec) = module_with_profiles_and_rec("autoload_launch");
        let save_res = m
            .profile(&json!({
                "action": "save",
                "name": "qa_launch_session",
                "target_id": "T"
            }))
            .await;
        assert!(save_res.ok, "{save_res:?}");

        let conn_res = m
            .connect(&json!({
                "launch": {
                    "headless": true,
                    "profile": "qa_launch_session"
                }
            }))
            .await;
        assert!(conn_res.ok, "{conn_res:?}");
        let data = conn_res.data.unwrap();
        assert_eq!(data["profile_restored"], "qa_launch_session");
        assert!(rec
            .profiles
            .lock()
            .unwrap()
            .contains(&"restore".to_string()));
    }

    #[tokio::test]
    async fn profile_save_list_restore_delete_flow() {
        let m = module_with_profiles("full_flow");
        let save = m
            .profile(&json!({
                "action": "save",
                "name": "login_session",
                "target_id": "T"
            }))
            .await;
        assert!(save.ok, "{save:?}");
        let d = save.data.unwrap();
        assert_eq!(d["saved"], true);
        assert_eq!(d["name"], "login_session");

        let list = m.profile(&json!({ "action": "list" })).await;
        assert!(list.ok, "{list:?}");
        let list_data = list.data.unwrap();
        let arr = list_data["profiles"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "login_session");

        let restore = m
            .profile(&json!({
                "action": "restore",
                "name": "login_session",
                "target_id": "T"
            }))
            .await;
        assert!(restore.ok, "{restore:?}");
        let restore_data = restore.data.unwrap();
        assert_eq!(restore_data["restored"], true);
        assert_eq!(restore_data["restored_cookies"], 1);

        let del = m
            .profile(&json!({
                "action": "delete",
                "name": "login_session"
            }))
            .await;
        assert!(del.ok, "{del:?}");
        assert_eq!(del.data.unwrap()["deleted"], true);

        let list_after = m.profile(&json!({ "action": "list" })).await;
        assert!(list_after.ok);
        assert_eq!(
            list_after.data.unwrap()["profiles"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn profile_is_disabled_without_a_store() {
        let (m, _) = module();
        let e = m.profile(&json!({ "action": "list" })).await;
        assert!(!e.ok);
        assert_eq!(e.error.unwrap().code, ErrorCode::UnsupportedOs);
    }

    #[tokio::test]
    async fn flow_replays_fill_form_and_extract_steps() {
        let m = module_with_flows("forms_and_extract");
        let save = m
            .flow(&json!({
                "action": "save",
                "name": "pipeline",
                "steps": [
                    { "op": "navigate", "url": "https://x" },
                    { "op": "fill_form", "fields": [{ "selector": "#email", "value": "a@b.com" }] },
                    { "op": "wait", "dom_settled": true },
                    { "op": "extract", "schema": { "name": ".profile-name" } },
                    { "op": "assert", "_pass": true }
                ]
            }))
            .await;
        assert!(save.ok, "{save:?}");
        let run = m
            .flow(&json!({ "action": "run", "name": "pipeline", "target_id": "T" }))
            .await;
        assert!(run.ok, "{run:?}");
        let d = run.data.unwrap();
        assert_eq!(d["passed"], true);
        assert_eq!(d["ran"], 5);
    }

    #[tokio::test]
    async fn profile_restore_nonexistent_returns_not_found() {
        let m = module_with_profiles("notfound");
        let res = m
            .profile(&json!({
                "action": "restore",
                "name": "ghost_session",
                "target_id": "T"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn profile_save_missing_name_fails() {
        let m = module_with_profiles("noname");
        let res = m
            .profile(&json!({
                "action": "save",
                "target_id": "T"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn profile_invalid_name_fails() {
        let m = module_with_profiles("badname");
        let res = m
            .profile(&json!({
                "action": "save",
                "name": "../escape",
                "target_id": "T"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn extract_without_schema_fails() {
        let (m, _) = module();
        let res = m.extract(&json!({ "target_id": "T" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn fill_form_with_empty_fields_array_fails() {
        let (m, _) = module();
        let res = m
            .fill_form(&json!({ "target_id": "T", "fields": [] }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn assert_with_wait_dom_settled_flag() {
        let (m, _) = module();
        let res = m
            .assert(&json!({
                "target_id": "T",
                "wait_dom_settled": true,
                "_pass": true
            }))
            .await;
        assert!(res.ok, "{res:?}");
        assert_eq!(res.data.unwrap()["passed"], true);
    }

    #[tokio::test]
    async fn profile_delete_nonexistent_returns_not_found() {
        let m = module_with_profiles("del_notfound");
        let res = m
            .profile(&json!({
                "action": "delete",
                "name": "ghost_profile"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn profile_delete_missing_name_fails() {
        let m = module_with_profiles("del_noname");
        let res = m
            .profile(&json!({
                "action": "delete"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn profile_restore_missing_name_fails() {
        let m = module_with_profiles("restore_noname");
        let res = m
            .profile(&json!({
                "action": "restore",
                "target_id": "T"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn profile_unknown_action_fails() {
        let m = module_with_profiles("unknown_act");
        let res = m
            .profile(&json!({
                "action": "wipe_everything"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn fill_form_with_non_array_fields_fails() {
        let (m, _) = module();
        let res = m
            .fill_form(&json!({
                "target_id": "T",
                "fields": "not-an-array"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn wait_without_condition_fails() {
        let (m, _) = module();
        let res = m.wait(&json!({ "target_id": "T" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn branch_create_missing_args_fails() {
        let (m, _) = module();
        let res = m.branch(&json!({ "action": "create" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);

        let res = m
            .branch(&json!({ "action": "create", "target_id": "T" }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn branch_unknown_action_fails() {
        let (m, _) = module();
        let res = m.branch(&json!({ "action": "warp_speed" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn checkpoint_save_missing_target_fails() {
        let (m, _) = module();
        let res = m.checkpoint(&json!({ "action": "save" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn checkpoint_unknown_action_fails() {
        let (m, _) = module();
        let res = m.checkpoint(&json!({ "action": "quantum_leap" })).await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[tokio::test]
    async fn test_browser_showcase_toggle_and_config() {
        let (m, _) = module();
        let res = m
            .showcase(&json!({
                "target_id": "T",
                "enabled": true,
                "speed": "cinematic",
                "click_ripple": true,
                "typing_hud": true,
                "cursor_style": "glow_arrow"
            }))
            .await;
        assert!(res.ok, "{res:?}");
        let data = res.data.unwrap();
        assert_eq!(data["enabled"], true);
        assert_eq!(data["speed"], "cinematic");
        assert_eq!(data["glide_ms"], 350);
        assert_eq!(data["click_ripple"], true);
        assert_eq!(data["typing_hud"], true);
    }

    #[tokio::test]
    async fn test_browser_showcase_presets() {
        let (m, _) = module();
        let res = m
            .showcase(&json!({
                "target_id": "T",
                "speed": "snappy"
            }))
            .await;
        assert!(res.ok);
        let data = res.data.unwrap();
        assert_eq!(data["speed"], "snappy");
        assert_eq!(data["glide_ms"], 120);

        // Unknown speed returns invalid args
        let res = m
            .showcase(&json!({
                "target_id": "T",
                "speed": "supersonic"
            }))
            .await;
        assert!(!res.ok);
        assert_eq!(res.error.unwrap().code, ErrorCode::InvalidArgs);
    }

    #[test]
    fn upload_paths_must_be_one_to_ten_non_empty_strings() {
        assert_eq!(
            upload_paths(&json!({ "paths": ["/a", "/b"] })).unwrap(),
            vec!["/a", "/b"]
        );
        assert!(upload_paths(&json!({})).is_err());
        assert!(upload_paths(&json!({ "paths": [] })).is_err());
        assert!(upload_paths(&json!({ "paths": ["/a", 3] })).is_err());
        assert!(upload_paths(&json!({ "paths": [""] })).is_err());
        let eleven: Vec<String> = (0..11).map(|i| format!("/f{i}")).collect();
        assert!(upload_paths(&json!({ "paths": eleven })).is_err());
        let ten: Vec<String> = (0..10).map(|i| format!("/f{i}")).collect();
        assert_eq!(upload_paths(&json!({ "paths": ten })).unwrap().len(), 10);
    }

    #[test]
    fn upload_file_must_be_a_regular_file_within_the_size_cap() {
        assert!(check_upload_file("/a", true, 0).is_ok());
        assert!(check_upload_file("/a", true, UPLOAD_MAX_BYTES).is_ok());
        assert!(check_upload_file("/a", true, UPLOAD_MAX_BYTES + 1)
            .unwrap_err()
            .contains("50 MiB"));
        assert!(check_upload_file("/dir", false, 0)
            .unwrap_err()
            .contains("regular file"));
    }

    #[tokio::test]
    async fn upload_without_a_resolver_is_refused_before_the_backend() {
        let (m, rec) = module();
        let e = m
            .upload(&json!({"target_id":"T","query":"input","paths":["/tmp/a"]}))
            .await;
        assert!(!e.ok);
        let err = e.error.unwrap();
        assert_eq!(err.code, ErrorCode::PermDenied);
        assert!(err.message.contains("fs.roots"));
        assert!(rec.uploads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn upload_passes_only_the_resolved_path_and_refuses_what_the_jail_refuses() {
        let dir = std::env::temp_dir().join(format!("agentctl-upload-unit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("cv.txt");
        std::fs::write(&file, b"hello").unwrap();
        let ssh = dir.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("id_rsa"), b"secret").unwrap();

        // A stand-in jail with the same shape as the real one: the module
        // must use the path it returns, not the caller's string.
        let root = std::fs::canonicalize(&dir).unwrap();
        let real = root.join("cv.txt");
        let resolver: UploadResolver = {
            let root = root.clone();
            Arc::new(move |p: &str| {
                if p.to_lowercase().contains("/.ssh/") {
                    return Err(format!("path '{p}' is denied by policy (fs.deny)"));
                }
                if p == "cv" {
                    return Ok(root.join("cv.txt"));
                }
                Err(format!(
                    "path '{p}' resolves outside the allowed roots (fs.roots)"
                ))
            })
        };
        let rec = Arc::new(Recorder::default());
        let m = BrowserModule::new(rec.clone()).with_upload_resolver(resolver);

        let ok = m
            .upload(&json!({"target_id":"T","ref":"/html/body[1]/input[1]","paths":["cv"]}))
            .await;
        assert!(ok.ok, "{ok:?}");
        let data = ok.data.unwrap();
        assert_eq!(data["count"], 1);
        assert_eq!(data["files"][0]["name"], "cv.txt");
        assert_eq!(data["files"][0]["bytes"], 5);
        assert_eq!(
            rec.uploads.lock().unwrap()[0],
            vec![real.to_string_lossy().into_owned()]
        );

        for bad in [
            ssh.join("id_rsa").to_string_lossy().into_owned(),
            "/etc/passwd".to_string(),
        ] {
            let e = m
                .upload(&json!({"target_id":"T","query":"input","paths":["cv", bad]}))
                .await;
            assert!(!e.ok);
            assert_eq!(e.error.unwrap().code, ErrorCode::PermDenied);
        }
        // Nothing past the first refused path reached the backend again.
        assert_eq!(rec.uploads.lock().unwrap().len(), 1);

        // A directory is not a file.
        let dir_resolver: UploadResolver = {
            let d = root.clone();
            Arc::new(move |_p: &str| Ok(d.clone()))
        };
        let m2 = BrowserModule::new(rec.clone()).with_upload_resolver(dir_resolver);
        let e = m2
            .upload(&json!({"target_id":"T","query":"input","paths":["x"]}))
            .await;
        assert!(!e.ok);
        assert!(e.error.unwrap().message.contains("regular file"));
        assert_eq!(rec.uploads.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn type_and_press_without_a_locator_go_to_the_focused_element() {
        let rec = Arc::new(Recorder::default());
        let m = BrowserModule::new(rec.clone());
        assert!(
            m.act(&json!({"target_id":"T","action":"type","value":"x"}))
                .await
                .ok
        );
        assert!(
            m.act(&json!({"target_id":"T","action":"press","value":"Enter"}))
                .await
                .ok
        );
        assert_eq!(*rec.acts.lock().unwrap(), vec!["focused", "focused"]);
    }

    #[tokio::test]
    async fn a_default_target_is_used_and_reported() {
        let rec = Arc::new(Recorder::default());
        let m = BrowserModule::new(rec.clone());
        let ctx = CallCtx::new("test", mcp_types::CancelToken::new());
        let e = m
            .call("browser_act", json!({"action":"click","query":"#a"}), &ctx)
            .await;
        assert!(e.ok, "{e:?}");
        assert_eq!(e.data.unwrap()["target_id"], "T");
        // The browser id stands for its active tab; a given tab id is not echoed.
        let given = json!({"target_id":1,"action":"click","query":"#a"});
        let e = m.call("browser_act", given, &ctx).await;
        assert_eq!(e.data.unwrap()["target_id"], "T");
        let given = json!({"target_id":"T","action":"click","query":"#a"});
        let e = m.call("browser_act", given, &ctx).await;
        assert!(e.data.unwrap().get("target_id").is_none());
        // Forking a tab keeps its target_id required.
        let e = m
            .call(
                "browser_branch",
                json!({"action":"create","branch_id":"b"}),
                &ctx,
            )
            .await;
        assert!(!e.ok);
    }

    #[tokio::test]
    async fn escaped_quotes_are_removed_from_xpath_args_before_dispatch() {
        let rec = Arc::new(Recorder::default());
        let m = BrowserModule::new(rec.clone());
        let ctx = CallCtx::new("test", mcp_types::CancelToken::new());
        let xpath = json!({"action":"click","ref":r#"//*[@id=\"tt\"]"#});
        m.call("browser_act", xpath, &ctx).await;
        let css = json!({"action":"click","by":"css","query":r#"a[title=\"x\"]"#});
        m.call("browser_act", css, &ctx).await;
        assert_eq!(
            *rec.acts.lock().unwrap(),
            vec![
                r#"ref://*[@id="tt"]"#.to_string(),
                r#"sel:css:a[title=\"x\"]"#.to_string()
            ]
        );
    }
}

#[cfg(test)]
mod arg_tests {
    use super::*;
    use crate::backend::ScrollMode;

    fn cond(args: Value) -> Result<(&'static str, Option<String>), String> {
        parse_wait_condition(&args).map(|(c, a)| (c, a.map(str::to_string)))
    }

    #[test]
    fn wait_condition_accepts_the_preferred_and_alias_forms() {
        assert_eq!(
            cond(json!({ "condition": "network_idle" })).unwrap().0,
            "network_idle"
        );
        assert_eq!(
            cond(json!({ "network_idle": true })).unwrap().0,
            "network_idle"
        );
        assert_eq!(
            cond(json!({ "condition": "challenge" })).unwrap().0,
            "challenge_cleared"
        );
        assert_eq!(
            cond(json!({ "selector": "#a" })).unwrap(),
            ("selector", Some("#a".into()))
        );
        assert_eq!(
            cond(json!({ "condition": "selector", "selector": "#a" })).unwrap(),
            ("selector", Some("#a".into()))
        );
    }

    #[test]
    fn a_false_flag_selects_nothing() {
        // `navigation:false` used to select navigation because the key was present.
        assert!(cond(json!({ "navigation": false })).is_err());
        assert_eq!(
            cond(json!({ "navigation": false, "condition": "network_idle" }))
                .unwrap()
                .0,
            "network_idle"
        );
        assert_eq!(
            cond(json!({ "navigation": false, "dom_settled": true }))
                .unwrap()
                .0,
            "dom_settled"
        );
        assert!(cond(json!({ "navigation": "yes" }))
            .unwrap_err()
            .contains("boolean"));
    }

    #[test]
    fn conflicting_wait_conditions_are_named_not_picked() {
        let e = cond(json!({ "condition": "network_idle", "navigation": true })).unwrap_err();
        assert!(
            e.contains("network_idle") && e.contains("navigation"),
            "{e}"
        );
        let e = cond(json!({ "dom_settled": true, "htmx_settled": true })).unwrap_err();
        assert!(
            e.contains("dom_settled") && e.contains("htmx_settled"),
            "{e}"
        );
        let e = cond(json!({ "selector": "#a", "condition": "navigation" })).unwrap_err();
        assert!(e.contains("selector") && e.contains("navigation"), "{e}");
        // The same condition said twice is still one.
        assert_eq!(
            cond(json!({ "condition": "navigation", "navigation": true }))
                .unwrap()
                .0,
            "navigation"
        );
    }

    #[test]
    fn wait_condition_errors_are_specific() {
        let e = cond(json!({ "condition": "selector" })).unwrap_err();
        assert!(e.contains("needs the 'selector' argument"), "{e}");
        let e = cond(json!({ "condition": "bogus" })).unwrap_err();
        assert!(e.contains("unknown condition 'bogus'"), "{e}");
        assert!(cond(json!({}))
            .unwrap_err()
            .contains("provide one wait condition"));
    }

    #[test]
    fn act_opts_default_to_nearest_scroll_and_no_wait() {
        let o = parse_act_opts(&json!({})).unwrap();
        assert_eq!(o, crate::backend::ActOpts::default());
        assert_eq!(o.scroll, ScrollMode::Nearest);
        assert!(!o.settle);
    }

    #[test]
    fn act_opts_parse_scroll_wait_after_and_timeout() {
        let o = parse_act_opts(
            &json!({ "scroll": "center", "wait_after": "settle", "timeout_ms": 2500 }),
        )
        .unwrap();
        assert_eq!(o.scroll, ScrollMode::Center);
        assert!(o.settle);
        assert_eq!(o.timeout_ms, 2500);
        assert_eq!(
            parse_act_opts(&json!({ "scroll": "none", "wait_after": "none" }))
                .unwrap()
                .scroll,
            ScrollMode::None
        );
        assert!(parse_act_opts(&json!({ "scroll": "smooth" }))
            .unwrap_err()
            .contains("scroll"));
        assert!(parse_act_opts(&json!({ "wait_after": "idle" }))
            .unwrap_err()
            .contains("wait_after"));
        assert!(parse_act_opts(&json!({ "timeout_ms": "soon" })).is_err());
    }
}

#[cfg(test)]
mod forgiving_args_tests {
    use super::*;

    #[test]
    fn a_wait_with_only_a_duration_is_a_sleep() {
        assert_eq!(sleep_only_ms(&json!({"timeout_ms": 500})), Some(500));
        assert_eq!(sleep_only_ms(&json!({"ms": "250"})), Some(250));
        assert_eq!(sleep_only_ms(&json!({"duration_ms": 90_000})), Some(30_000));
        // A condition makes the duration its timeout.
        assert_eq!(
            sleep_only_ms(&json!({"timeout_ms": 500, "condition": "navigation"})),
            None
        );
        assert_eq!(
            sleep_only_ms(&json!({"timeout_ms": 500, "selector": "#a"})),
            None
        );
        assert_eq!(
            sleep_only_ms(&json!({"timeout_ms": 500, "dom_settled": true})),
            None
        );
        // A false flag is no condition, and nothing at all is still an error.
        assert_eq!(
            sleep_only_ms(&json!({"timeout_ms": 5, "dom_settled": false})),
            Some(5)
        );
        assert_eq!(sleep_only_ms(&json!({})), None);
    }

    #[test]
    fn pointer_args_take_quoted_numbers_and_name_a_destination() {
        let a = json!({"x": "60", "y": 115, "to_query": "#zone", "to_x": "3", "dy": -4.5});
        let p = pointer_args(&a).unwrap();
        assert_eq!(
            (p.x, p.y, p.to_x, p.to_y, p.dx),
            (Some(60.0), Some(115.0), Some(3.0), None, None)
        );
        assert_eq!(p.dy, Some(-4.5));
        assert!(matches!(
            p.to,
            Some(crate::backend::Locator::Selector {
                by: "auto",
                query: "#zone",
                ..
            })
        ));
        let both = json!({"to_query": "//div[1]", "to_ref": "//*[@id=\"a\"]"});
        let p = pointer_args(&both).unwrap();
        assert!(
            matches!(p.to, Some(crate::backend::Locator::Ref(_))),
            "a ref wins"
        );
        let e = pointer_args(&json!({"x": "left"})).unwrap_err();
        assert!(e.contains("'x'") && e.contains("number"), "{e}");
        assert!(pointer_args(&json!({"x": null})).unwrap().x.is_none());
    }

    fn b(id: u32, tabs: &[&str]) -> BrowserTabs {
        BrowserTabs {
            id,
            tabs: tabs.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn active(target: &str, browser_id: u32) -> Result<TargetPick, TargetError> {
        Ok(TargetPick::Active {
            target: target.into(),
            browser_id,
        })
    }

    #[test]
    fn a_tab_id_is_used_as_given() {
        let bs = [b(1, &["AAA"]), b(2, &["BBB"])];
        assert_eq!(
            pick_target(Some("BBB"), &bs),
            Ok(TargetPick::Given("BBB".into()))
        );
        // Not checked here: the backend answers for a tab that does not exist.
        assert_eq!(
            pick_target(Some("ZZZ"), &[]),
            Ok(TargetPick::Given("ZZZ".into()))
        );
    }

    #[test]
    fn a_browser_id_means_its_active_tab() {
        let bs = [b(1, &["AAA", "A2"]), b(2, &["BBB"])];
        assert_eq!(pick_target(Some("1"), &bs), active("AAA", 1));
        assert_eq!(pick_target(Some("2"), &bs), active("BBB", 2));
        assert_eq!(
            pick_target(Some("3"), &bs),
            Err(TargetError::Unknown("3".into(), vec![1, 2]))
        );
        assert_eq!(
            pick_target(Some("2"), &[b(2, &[])]),
            Err(TargetError::NoTabs(2))
        );
        // An all-digit string that really is a tab id stays a tab id.
        assert_eq!(
            pick_target(Some("1"), &[b(2, &["1"]), b(1, &["x"])]),
            Ok(TargetPick::Given("1".into()))
        );
    }

    #[test]
    fn an_omitted_target_needs_exactly_one_browser() {
        assert_eq!(pick_target(None, &[b(4, &["T1", "T2"])]), active("T1", 4));
        assert_eq!(pick_target(None, &[]), Err(TargetError::NoBrowser));
        assert_eq!(pick_target(None, &[b(1, &[])]), Err(TargetError::NoTabs(1)));
        assert_eq!(
            pick_target(None, &[b(1, &["a"]), b(2, &["b"])]),
            Err(TargetError::Ambiguous(vec![1, 2]))
        );
    }

    #[test]
    fn target_errors_say_what_to_pass() {
        let e = TargetError::Ambiguous(vec![1, 2]).into_envelope("browser_act");
        let err = e.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("1, 2"), "{}", err.message);
        let e = TargetError::NoBrowser.into_envelope("browser_act");
        assert!(e
            .error
            .unwrap()
            .suggestion
            .unwrap()
            .contains("browser_connect"));
    }

    #[test]
    fn target_arg_accepts_a_number_and_ignores_empty() {
        assert_eq!(target_arg(&json!({"target_id": 1})).as_deref(), Some("1"));
        assert_eq!(target_arg(&json!({"target_id": "1"})).as_deref(), Some("1"));
        assert_eq!(target_arg(&json!({"target_id": ""})), None);
        assert_eq!(target_arg(&json!({})), None);
    }

    #[test]
    fn only_the_safe_calls_default_their_target() {
        assert!(defaults_target("browser_act", &json!({})));
        assert!(defaults_target(
            "browser_profile",
            &json!({"action":"save"})
        ));
        assert!(!defaults_target(
            "browser_profile",
            &json!({"action":"list"})
        ));
        assert!(defaults_target("browser_checkpoint", &json!({})));
        assert!(!defaults_target(
            "browser_checkpoint",
            &json!({"action":"list"})
        ));
        assert!(defaults_target("browser_flow", &json!({"action":"run"})));
        assert!(!defaults_target("browser_flow", &json!({"action":"list"})));
        assert!(defaults_target(
            "browser_screencast",
            &json!({"action":"start"})
        ));
        assert!(!defaults_target(
            "browser_screencast",
            &json!({"action":"stop"})
        ));
        assert!(!defaults_target(
            "browser_branch",
            &json!({"action":"create"})
        ));
        assert!(!defaults_target("browser_tabs", &json!({})));
    }

    #[test]
    fn escaped_quotes_are_dropped_from_xpath() {
        assert_eq!(
            unescape_xpath_quotes(r#"//*[@id=\"tt\"]"#),
            r#"//*[@id="tt"]"#
        );
        assert_eq!(
            unescape_xpath_quotes(r"//a[text()=\'x\']"),
            "//a[text()='x']"
        );
        // Nothing to fix: borrowed, and other backslashes stay.
        assert!(matches!(
            unescape_xpath_quotes(r"//a[contains(., 'a\b')]"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn only_xpath_arguments_are_unescaped() {
        let mut a = json!({
            "ref": r#"//*[@id=\"a\"]"#,
            "by": "xpath", "query": r#"//*[@id=\"b\"]"#,
            "within": r#"//*[@id=\"c\"]"#,
            "fields": [
                {"selector": r#"//*[@id=\"d\"]"#},
                {"selector": r#"//*[@id=\"e\"]"#, "by": "xpath"},
                {"selector": r#"a[title=\"f\"]"#},
                {"selector": r#"//*[@id=\"g\"]"#, "by": "css"}
            ],
            "submit": {"selector": r#"(//button)[@id=\"h\"]"#}
        });
        fix_xpath_args(&mut a);
        assert_eq!(a["ref"], r#"//*[@id="a"]"#);
        assert_eq!(a["query"], r#"//*[@id="b"]"#);
        assert_eq!(a["within"], r#"//*[@id="c"]"#);
        assert_eq!(a["fields"][0]["selector"], r#"//*[@id="d"]"#);
        assert_eq!(a["fields"][1]["selector"], r#"//*[@id="e"]"#);
        // CSS accepts \" as an escape, so it is left alone.
        assert_eq!(a["fields"][2]["selector"], r#"a[title=\"f\"]"#);
        assert_eq!(a["fields"][3]["selector"], r#"//*[@id=\"g\"]"#);
        assert_eq!(a["submit"]["selector"], r#"(//button)[@id="h"]"#);
        let mut css = json!({"query": r#"a[title=\"x\"]"#});
        fix_xpath_args(&mut css);
        assert_eq!(css["query"], r#"a[title=\"x\"]"#);
    }

    #[test]
    fn jquery_pseudo_classes_are_recognised_and_css_ones_are_not() {
        for (sel, want) in [
            ("button:contains('x')", ":contains("),
            (r#"a:has-text("Go")"#, ":has-text("),
            ("li:text(Go)", ":text("),
            ("li:eq(2)", ":eq("),
            ("div.row:visible", ":visible"),
            ("li:first", ":first"),
            ("li:last > a", ":last"),
        ] {
            assert_eq!(jquery_pseudo(sel), Some(want), "{sel}");
        }
        for sel in [
            "li:first-child",
            "li:last-of-type",
            "a:not(.x)",
            "p:has(b)",
            "input:focus-visible",
            "#a .b",
        ] {
            assert_eq!(jquery_pseudo(sel), None, "{sel}");
        }
    }

    #[test]
    fn a_jquery_selector_that_fails_to_parse_gets_the_text_suggestion() {
        let bad = Err(BrowserError::Failed(
            "eval: SyntaxError: Failed to execute 'querySelectorAll' on 'Document': 'button:contains('x')' is not a valid selector.".into(),
        ));
        let e = result_with_selector_hint("browser_query", bad, &["button:contains('x')"]);
        let err = e.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("not a valid selector"));
        assert!(err.suggestion.unwrap().contains(r#"by: "text""#));
        // A failure for another reason, or a selector with no pseudo, is left as it was.
        let other = Err(BrowserError::NotFound("element not found".into()));
        let e = result_with_selector_hint("browser_act", other, &["a:contains(x)"]);
        assert_eq!(e.error.unwrap().code, ErrorCode::NotFound);
        let bad = Err(BrowserError::Failed(
            "SyntaxError: not a valid selector".into(),
        ));
        let e = result_with_selector_hint("browser_act", bad, &["a["]);
        assert_eq!(e.error.unwrap().code, ErrorCode::ActionFailed);
    }

    #[test]
    fn fill_form_css_selectors_skip_xpath_and_text_fields() {
        let mut a = json!({
            "fields": [
                {"selector": "#a"},
                {"selector": "//input"},
                {"selector": "b", "by": "text"},
                {"selector": "li:eq(1)", "by": "css"},
                {"ref": "/html/body[1]"}
            ],
            "submit": {"selector": "button:visible"}
        });
        respell_args("browser_fill_form", &mut a);
        assert_eq!(
            fill_css_selectors(&a),
            vec!["#a", "li:eq(1)", "button:visible"]
        );
    }

    fn respelled(by: Option<&str>, q: &str) -> Option<(&'static str, String, Option<String>)> {
        respell_locator(by, q).map(|r| (r.by, r.query, r.text))
    }

    fn rs(
        by: &'static str,
        q: &str,
        text: Option<&str>,
    ) -> Option<(&'static str, String, Option<String>)> {
        Some((by, q.to_string(), text.map(String::from)))
    }

    #[test]
    fn xpath_spellings_become_xpath_even_when_css_was_named() {
        for by in [None, Some("css")] {
            assert_eq!(
                respelled(by, "//*[@id=\"cv\"]"),
                rs("xpath", "//*[@id=\"cv\"]", None)
            );
            assert_eq!(respelled(by, "(//li)[2]"), rs("xpath", "(//li)[2]", None));
            assert_eq!(respelled(by, "xpath=//b"), rs("xpath", "//b", None));
            assert_eq!(
                respelled(by, "/html/body/div[1]"),
                rs("xpath", "/html/body/div[1]", None)
            );
        }
    }

    #[test]
    fn engine_prefixes_name_their_kind() {
        assert_eq!(
            respelled(None, "css=.terminal"),
            rs("css", ".terminal", None)
        );
        // A prefixed CSS selector is still respelled inside.
        assert_eq!(
            respelled(None, "css=button:has-text(\"Go\")"),
            rs("css", "button", Some("Go"))
        );
    }

    #[test]
    fn text_spellings_search_the_unquoted_text() {
        for q in [
            "text=Alanna",
            "text=\"Alanna\"",
            "text='Alanna'",
            "text:Alanna",
            "text(\"Alanna\")",
            "text('Alanna')",
            "text \"Alanna\"",
            "text 'Alanna'",
        ] {
            assert_eq!(respelled(None, q), rs("text", "Alanna", None), "{q}");
        }
        assert_eq!(respelled(Some("css"), "text=a b"), rs("text", "a b", None));
        // `text` is also an SVG tag: these are CSS.
        for q in [
            "text:hover",
            "text:first-child",
            "text:not(.x)",
            "text",
            "text.big",
            "text > tspan",
        ] {
            assert_eq!(respelled(None, q), None, "{q}");
        }
    }

    #[test]
    fn a_trailing_text_pseudo_becomes_the_text_filter() {
        assert_eq!(
            respelled(None, "button:has-text(\"Generate\")"),
            rs("css", "button", Some("Generate"))
        );
        assert_eq!(
            respelled(None, "button:contains('Submit')"),
            rs("css", "button", Some("Submit"))
        );
        assert_eq!(
            respelled(None, "button:text(\"Submit\")"),
            rs("css", "button", Some("Submit"))
        );
        assert_eq!(
            respelled(None, "li:has-text(-42)"),
            rs("css", "li", Some("-42"))
        );
        assert_eq!(
            respelled(None, "li:text-is(\"-42\")"),
            rs("css", "li", Some("-42"))
        );
        assert_eq!(
            respelled(Some("css"), "ul#l > li.x:contains(\"a b\")"),
            rs("css", "ul#l > li.x", Some("a b"))
        );
        assert_eq!(
            respelled(None, "a[href*=\"x)\"]:has-text(\"Go\")"),
            rs("css", "a[href*=\"x)\"]", Some("Go"))
        );
        // Nothing before the pseudo: a text search.
        assert_eq!(
            respelled(None, ":contains(\"Dawn\")"),
            rs("text", "Dawn", None)
        );
    }

    #[test]
    fn a_text_pseudo_in_a_shape_that_cannot_be_rewritten_is_left_alone() {
        for q in [
            "div :contains(\"x\")",
            "a:contains(\"x\") b",
            "a, b:has-text(\"x\")",
            "div >:has-text(\"x\")",
            "button:has-text(\"\")",
            "button:has-text(\"a\"b\")",
            "a:visible:contains(\"x\")",
            "a[title=\"x:contains(\"y\")",
        ] {
            assert_eq!(respelled(None, q), None, "{q}");
        }
    }

    #[test]
    fn a_ref_without_its_front_is_a_body_path() {
        assert_eq!(
            respelled(None, "div[1]/div[2]/div[4]"),
            rs("xpath", "/html/body/div[1]/div[2]/div[4]", None)
        );
        assert_eq!(
            respelled(None, "body/div/span[3]"),
            rs("xpath", "/html/body/div/span[3]", None)
        );
        assert_eq!(
            respelled(None, "html/body/div[1]"),
            rs("xpath", "/html/body/div[1]", None)
        );
        assert_eq!(
            respelled(Some("css"), "ul/li[2]"),
            rs("xpath", "/html/body/ul/li[2]", None)
        );
        for q in ["div[1]", "div[a]/p", "div[]/p", "a/", "div[1]x/p"] {
            assert_eq!(respelled(None, q), None, "{q}");
        }
    }

    #[test]
    fn ordinary_selectors_and_explicit_kinds_are_not_respelled() {
        for q in [
            "Gilli",
            "Section #1",
            "5",
            "a[href*=\"text=\"]",
            "button.primary",
            "#id > span",
            "input[name='q']",
            "li:nth-child(2)",
            "div[data-x=\"a/b\"]",
            "css",
            "div.text",
        ] {
            assert_eq!(respelled(None, q), None, "{q}");
            assert_eq!(respelled(Some("css"), q), None, "{q}");
        }
        // An explicit text or xpath is taken at its word.
        assert_eq!(respelled(Some("text"), "text=Go"), None);
        assert_eq!(respelled(Some("text"), "//b"), None);
        assert_eq!(respelled(Some("xpath"), "css=.x"), None);
    }

    #[test]
    fn scopes_take_xpath_and_prefixes_but_no_text() {
        assert_eq!(respell_scope("xpath=//form").as_deref(), Some("//form"));
        assert_eq!(respell_scope("css=#a").as_deref(), Some("#a"));
        assert_eq!(
            respell_scope("div[1]/form").as_deref(),
            Some("/html/body/div[1]/form")
        );
        assert_eq!(respell_scope("//form").as_deref(), Some("//form"));
        assert_eq!(respell_scope("#a"), None);
        assert_eq!(respell_scope("text=Go"), None);
        assert_eq!(respell_scope("form:has-text(\"Go\")"), None);
    }

    #[test]
    fn call_arguments_are_respelled_per_tool() {
        let mut a = json!({"query": "button:has-text(\"Go\")", "within": "div[1]/form"});
        respell_args("browser_act", &mut a);
        assert_eq!(
            a,
            json!({"query": "button", "by": "css", "text": "Go", "within": "/html/body/div[1]/form"})
        );
        // A text filter the caller gave is theirs; the pseudo stays for the hint.
        let mut a = json!({"query": "button:has-text(\"Go\")", "text": "Stop"});
        respell_args("browser_act", &mut a);
        assert_eq!(
            a,
            json!({"query": "button:has-text(\"Go\")", "text": "Stop"})
        );
        let mut a = json!({"root_selector": "//main"});
        respell_args("browser_snapshot", &mut a);
        assert_eq!(a, json!({"root_selector": "//main"}));
        let mut a = json!({
            "fields": [{"selector": "text=Name", "value": "x"}, {"selector": "#e", "value": "y"}],
            "submit": {"selector": "button:contains(\"Save\")"}
        });
        respell_args("browser_fill_form", &mut a);
        assert_eq!(
            a,
            json!({
                "fields": [
                    {"selector": "Name", "by": "text", "value": "x"},
                    {"selector": "#e", "value": "y"}
                ],
                "submit": {"selector": "button", "by": "css", "text": "Save"}
            })
        );
        // Tools that do not locate this way are untouched.
        let mut a = json!({"query": "text=Go"});
        respell_args("browser_wait", &mut a);
        assert_eq!(a, json!({"query": "text=Go"}));
    }
}
