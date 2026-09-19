//! `agentctl` — the composition root. Loads config, wires enabled engines into
//! the registry, and serves MCP over stdio. Logs go to **stderr only**; stdout
//! is the protocol channel.

use std::sync::Arc;

use agentctl::{build_modules, consent_provider, new_session_id, tools_doc};
use mcp_core::{HttpConfig, HttpTransport, PROTOCOL_VERSION};
use mcp_policy::{AuditSink, PolicyConfig};
use mcp_types::Category;
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    init_tracing();

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("serve");

    let force_http = args.iter().any(|a| a == "--http");

    match cmd {
        "serve" => serve(force_http).await,
        "doctor" => {
            doctor();
            Ok(())
        }
        "tools" => {
            tools(&args);
            Ok(())
        }
        "bridge" => bridge(&args).await,
        "test" => test_cmd(&args).await,
        "transcript" => transcript_cmd(&args),
        "config" if args.get(2).map(String::as_str) == Some("print") => {
            config_print();
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("agentctl: unknown command '{other}'\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    // INFO and above, as the fmt default has always been, with one carve-out:
    // ashpd's zbus proxy logs a WARN every time it fails to pre-populate a
    // D-Bus property cache, which it does routinely on portals that expose no
    // such properties. It is noise, not a fault, so zbus is held to ERROR.
    // Everything else is untouched, and a real zbus error still prints.
    let filter = tracing_subscriber::filter::filter_fn(|meta| {
        let level = *meta.level();
        if level > tracing::Level::INFO {
            return false;
        }
        if meta.target().starts_with("zbus") && level > tracing::Level::ERROR {
            return false;
        }
        true
    });
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false);
    tracing_subscriber::registry()
        .with(fmt_layer.with_filter(filter))
        .try_init()
        .ok();
}

/// Build the policy config: secure defaults, layered with `config.toml` when
/// present (`$AGENTCTL_CONFIG`, else `~/.agentctl/config.toml`).
///
/// A config file that exists but does not parse is a hard error — falling back
/// to defaults would silently *widen* a stricter operator policy.
fn build_config() -> Result<PolicyConfig, String> {
    let user_supplied = mcp_policy::config_path().exists();
    let mut cfg = PolicyConfig::load()?;
    if !user_supplied {
        // Skeleton defaults: diagnostics plus the browser engine. Its
        // eval/cookies/network tools stay Dangerous-tier and consent-gated.
        cfg.categories.push(Category::System);
        cfg.categories.push(Category::Browser);
    }
    Ok(cfg)
}

/// Load config or exit with a clear message (fail-closed).
fn config_or_exit() -> PolicyConfig {
    match build_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agentctl: refusing to start — bad config: {e}");
            std::process::exit(2);
        }
    }
}

async fn serve(force_http: bool) -> std::io::Result<()> {
    let cfg = config_or_exit();
    let session_id = new_session_id();
    let audit = AuditSink::file(cfg.audit_dir.clone(), &session_id)?;
    let http = (force_http || cfg.http_enabled).then(|| http_config(&cfg));
    let override_cfg = mcp_input::OverrideConfig {
        enabled: cfg.human_override,
        threshold_px: cfg.human_override_px as f64,
        grace_ms: cfg.human_override_grace_ms,
        ..mcp_input::OverrideConfig::default()
    };
    let (server, policy, wiring) =
        agentctl::build_server_with(cfg, audit, consent_provider(), session_id.clone())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Reaching for the mouse is how people already interrupt something; this
    // makes that gesture stop the agent.
    if let (Some(input), Some(activity)) = (wiring.input.clone(), wiring.activity.clone()) {
        agentctl::override_watch::spawn(
            input,
            activity,
            override_cfg,
            policy.clone(),
            wiring.desktop.clone(),
            session_id.clone(),
        );
    }

    let Some(http) = http else {
        tracing::info!(session = %session_id, protocol = PROTOCOL_VERSION, "agentctl serving on stdio");
        let r = run_until_signal(server.serve_stdio()).await;
        server.shutdown();
        return r;
    };

    // One transport at a time. Serving both would mean an unauthenticated
    // stdio peer and an authenticated network peer sharing one session's
    // consent budget and audit stream, which makes the log ambiguous about who
    // asked for what.
    let generated = http.token.is_empty();
    let http = HttpConfig {
        token: if generated {
            mcp_core::generate_token()?
        } else {
            http.token
        },
        ..http
    };
    // Print before binding so a failed bind still leaves the operator with the
    // token, and to stderr because stdout is the protocol channel.
    if generated {
        eprintln!(
            "agentctl: generated bearer token for this session:\n  {}\n\
             Pass it as `Authorization: Bearer <token>`. Set http.token in \
             config.toml to keep one across restarts.",
            http.token
        );
    }
    let transport = HttpTransport::bind(http).await?;
    let addr = transport.local_addr()?;
    tracing::info!(
        session = %session_id,
        protocol = PROTOCOL_VERSION,
        %addr,
        "agentctl serving on http (loopback, bearer auth)"
    );
    let server = Arc::new(server);
    let r = run_until_signal(transport.serve(server.clone())).await;
    server.shutdown();
    r
}

/// Serve until the transport ends or the process is asked to stop.
///
/// Without this, Ctrl-C and SIGTERM kill the process outright: no destructor
/// runs, so a browser this session launched keeps running with its temporary
/// profile, and a PTY keeps its child process group alive. Both signals are
/// treated as a clean end of service so the shutdown hooks get to run.
async fn run_until_signal<F>(serving: F) -> std::io::Result<()>
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        tokio::pin!(serving);
        tokio::select! {
            r = &mut serving => r,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupted; shutting engines down");
                Ok(())
            }
            _ = term.recv() => {
                tracing::info!("terminated; shutting engines down");
                Ok(())
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::pin!(serving);
        tokio::select! {
            r = &mut serving => r,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupted; shutting engines down");
                Ok(())
            }
        }
    }
}

/// Translate the operator's `[http]` settings into the transport's own config.
fn http_config(c: &PolicyConfig) -> HttpConfig {
    let bind = c.http_bind.parse().unwrap_or_else(|e| {
        eprintln!(
            "agentctl: refusing to start — http.bind '{}' is not an address: {e}",
            c.http_bind
        );
        std::process::exit(2);
    });
    HttpConfig {
        bind,
        token: c.http_token.clone(),
        allowed_origins: c.http_allowed_origins.clone(),
        ..HttpConfig::default()
    }
}

/// Print the tool catalog. `--all` ignores `policy.categories` so the document
/// covers every tool the build contains, which is what a reference is for;
/// without it the output is what an agent would actually be offered.
fn tools(args: &[String]) {
    let all = args.iter().any(|a| a == "--all");
    let as_json = args.iter().any(|a| a == "--json");
    let cfg = if all {
        // Never the operator's config: the committed reference must not depend
        // on whose machine generated it.
        PolicyConfig::default()
    } else {
        config_or_exit()
    };
    let mut descriptors: Vec<_> = build_modules(&cfg)
        .iter()
        .flat_map(|m| m.descriptors())
        .collect();
    if !all {
        descriptors.retain(|d| cfg.categories.contains(&d.category));
    }
    if as_json {
        println!("{}", tools_doc::render_json(&descriptors));
    } else {
        print!("{}", tools_doc::render_markdown(&descriptors));
    }
}

/// The value of `--name value`, if present.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// Where per-user state lives, derived from the configured kill-switch path so
/// a custom state directory is honoured.
fn state_dir(cfg: &PolicyConfig) -> std::path::PathBuf {
    cfg.kill_switch_file
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
}

fn fail(msg: String) -> std::io::Error {
    std::io::Error::other(msg)
}

/// Drive this server with Gemini, end to end.
async fn bridge(args: &[String]) -> std::io::Result<()> {
    let cfg = config_or_exit();
    let dir = state_dir(&cfg);
    if has(args, "--list-models") {
        return agentctl::bridge::list_models(&dir).await.map_err(fail);
    }
    let Some(task) = flag(args, "--task") else {
        eprintln!(
            "agentctl bridge: --task is required.\n\n\
             \x20   agentctl bridge --task \"open TextEdit and type hello\"\n\
             \x20   agentctl bridge --list-models"
        );
        std::process::exit(2);
    };
    let defaults = agentctl::bridge::BridgeOpts::default();
    let opts = agentctl::bridge::BridgeOpts {
        task: task.to_string(),
        model: flag(args, "--model")
            .map(str::to_string)
            .or_else(|| std::env::var("GEMINI_MODEL").ok())
            .unwrap_or(defaults.model),
        max_turns: flag(args, "--max-turns")
            .and_then(|v| v.parse().ok())
            .unwrap_or(defaults.max_turns),
        mode: flag(args, "--mode").unwrap_or("AUTO").to_string(),
        thinking_level: flag(args, "--thinking-level").map(str::to_string),
        record: flag(args, "--record").map(std::path::PathBuf::from),
        config: flag(args, "--config").map(std::path::PathBuf::from),
        system: flag(args, "--system").map(str::to_string),
        prune: has(args, "--prune"),
    };
    let transcript = agentctl::bridge::run(opts, &dir).await.map_err(fail)?;
    let (ok, total) = transcript.tally();
    println!("{ok}/{total} tool calls succeeded");
    Ok(())
}

/// `agentctl test [names...]` replays saved `browser_flow` UI tests against an
/// attached Chromium and reports pass/fail plus the issues each flow hit
/// (failing step, console errors, failed requests). Exit 1 on any failure, or
/// on any issue with `--strict`. A green run never invokes a model.
async fn test_cmd(args: &[String]) -> std::io::Result<()> {
    use mcp_browser::{BrowserModule, CdpBackend, FlowStore, NavPolicy};
    use mcp_types::{CallCtx, CancelToken, ToolModule};

    let cfg = config_or_exit();
    let dir = state_dir(&cfg);
    let port: u16 = flag(args, "--attach")
        .and_then(|s| s.parse().ok())
        .unwrap_or(9222);
    let strict = has(args, "--strict");
    let json_out = flag(args, "--json");
    let names = positional_flows(args);

    let backend = Arc::new(CdpBackend::new(NavPolicy::new(
        &cfg.allowed_origins,
        cfg.browser_allow_private,
    )));
    let module = BrowserModule::new(backend)
        .with_flow_store(FlowStore::new(dir.join("browser_flows.json"), 200, 200));
    let ctx = CallCtx::new(new_session_id(), CancelToken::new());

    let conn = module
        .call("browser_connect", json!({ "attach": { "port": port } }), &ctx)
        .await;
    if !conn.ok {
        eprintln!(
            "agentctl test: could not attach to Chromium on 127.0.0.1:{port}.\n  \
             Start it with --remote-debugging-port={port}, or pass --attach <port>."
        );
        if let Some(e) = conn.error {
            eprintln!("  {}", e.message);
        }
        std::process::exit(2);
    }
    let browser_id = conn
        .data
        .as_ref()
        .and_then(|d| d.get("browser_id"))
        .and_then(Value::as_u64)
        .unwrap_or(1);

    let flows: Vec<String> = if names.is_empty() {
        let list = module
            .call("browser_flow", json!({ "action": "list" }), &ctx)
            .await;
        list.data
            .as_ref()
            .and_then(|d| d.get("flows"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.get("name").and_then(Value::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        names
    };
    if flows.is_empty() {
        eprintln!("agentctl test: no saved flows to run. Record one with browser_flow first.");
        std::process::exit(2);
    }

    eprintln!(
        "agentctl test: {} flow(s) against 127.0.0.1:{port}",
        flows.len()
    );
    let mut reports: Vec<Value> = Vec::new();
    for name in &flows {
        // A fresh tab per flow, so page state does not leak between tests.
        let tab = module
            .call(
                "browser_tabs",
                json!({ "browser_id": browser_id, "action": "open", "url": "about:blank" }),
                &ctx,
            )
            .await;
        let target = tab
            .data
            .as_ref()
            .and_then(|d| d.get("target_id"))
            .and_then(Value::as_str)
            .map(String::from);
        let Some(target) = target else {
            reports.push(json!({ "name": name, "passed": false, "error": "could not open a tab",
                "console_errors": [], "failed_requests": [] }));
            continue;
        };
        // Arm capture so even a passing flow reports console errors / failed
        // requests it happened to trigger.
        let _ = module
            .call("browser_capture", json!({ "target_id": target, "action": "start" }), &ctx)
            .await;
        let run = module
            .call(
                "browser_flow",
                json!({ "action": "run", "name": name, "target_id": target }),
                &ctx,
            )
            .await;
        let cap = module
            .call("browser_capture", json!({ "target_id": target, "action": "read" }), &ctx)
            .await;
        let _ = module
            .call(
                "browser_tabs",
                json!({ "browser_id": browser_id, "action": "close", "target_id": target }),
                &ctx,
            )
            .await;
        reports.push(build_flow_report(name, &run, &cap));
    }

    let (passed, failed, issue_flows) = print_report(&reports, strict);
    if let Some(p) = json_out {
        let doc = json!({ "passed": passed, "failed": failed, "flows": reports });
        let text = serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n";
        if let Err(e) = std::fs::write(p, text) {
            eprintln!("agentctl test: could not write {p}: {e}");
        } else {
            eprintln!("wrote report to {p}");
        }
    }
    let bad = failed > 0 || (strict && issue_flows > 0);
    std::process::exit(if bad { 1 } else { 0 });
}

/// Flow names passed positionally, skipping flags and the values of the flags
/// that take one.
fn positional_flows(args: &[String]) -> Vec<String> {
    let value_flags = ["--attach", "--json", "--config"];
    let mut out = Vec::new();
    let mut i = 2; // args[0]=bin, args[1]="test"
    while i < args.len() {
        let a = &args[i];
        if a.starts_with("--") {
            i += if value_flags.contains(&a.as_str()) { 2 } else { 1 };
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

/// Fold a flow's replay envelope and its capture into one report row.
fn build_flow_report(name: &str, run: &mcp_types::Envelope, cap: &mcp_types::Envelope) -> Value {
    let rd = run.data.clone().unwrap_or_else(|| json!({}));
    let steps = rd.get("steps").cloned().unwrap_or_else(|| json!([]));
    let failing = steps
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|s| s.get("ok").and_then(Value::as_bool) == Some(false))
                .cloned()
        })
        .unwrap_or(Value::Null);
    let cd = cap.data.clone().unwrap_or_else(|| json!({}));
    let is_err_level = |c: &&Value| {
        matches!(
            c.get("level").and_then(Value::as_str),
            Some("error") | Some("uncaught") | Some("unhandledrejection")
        )
    };
    let console_errors: Vec<Value> = cd
        .get("console")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(is_err_level).cloned().collect())
        .unwrap_or_default();
    let failed_requests: Vec<Value> = cd
        .get("network")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|n| n.get("ok").and_then(Value::as_bool) == Some(false))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    json!({
        "name": name,
        "passed": run.ok,
        "ran": rd.get("ran").cloned().unwrap_or(json!(0)),
        "failing_step": failing,
        "console_errors": console_errors,
        "failed_requests": failed_requests,
        "steps": steps,
    })
}

/// Print the human report; return (passed, failed, flows-with-issues).
fn print_report(reports: &[Value], strict: bool) -> (usize, usize, usize) {
    let arr_len = |v: &Value, k: &str| v.get(k).and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
    let (mut passed, mut failed, mut issue_flows) = (0usize, 0usize, 0usize);
    println!();
    for r in reports {
        let name = r.get("name").and_then(Value::as_str).unwrap_or("?");
        let ok = r.get("passed").and_then(Value::as_bool).unwrap_or(false);
        let ran = r.get("ran").and_then(Value::as_u64).unwrap_or(0);
        let ce = arr_len(r, "console_errors");
        let fr = arr_len(r, "failed_requests");
        if ce > 0 || fr > 0 {
            issue_flows += 1;
        }
        println!("  [{}] {name}  ({ran} step(s) ran)", if ok { "PASS" } else { "FAIL" });
        if ok {
            passed += 1;
        } else {
            failed += 1;
            if let Some(err) = r.get("error").and_then(Value::as_str) {
                println!("       error: {err}");
            }
            let fs = &r["failing_step"];
            if !fs.is_null() {
                let op = fs.get("op").and_then(Value::as_str).unwrap_or("?");
                let detail = serde_json::to_string(&fs["detail"]).unwrap_or_default();
                let detail: String = detail.chars().take(200).collect();
                println!("       failing step: {op} -> {detail}");
            }
        }
        if ce > 0 {
            println!("       console errors: {ce}");
            for c in r["console_errors"].as_array().into_iter().flatten().take(5) {
                println!("         - {}", c.get("text").and_then(Value::as_str).unwrap_or(""));
            }
        }
        if fr > 0 {
            println!("       failed requests: {fr}");
            for n in r["failed_requests"].as_array().into_iter().flatten().take(5) {
                let m = n.get("method").and_then(Value::as_str).unwrap_or("?");
                let st = n.get("status").and_then(Value::as_u64).unwrap_or(0);
                let u = n.get("url").and_then(Value::as_str).unwrap_or("");
                println!("         - {m} {st} {u}");
            }
        }
    }
    let extra = if strict && issue_flows > 0 {
        format!(", {issue_flows} with issues (strict)")
    } else {
        String::new()
    };
    println!("\n{passed} passed, {failed} failed{extra}");
    (passed, failed, issue_flows)
}

/// Rebuild a transcript from an audit log — how a session driven by a client
/// we do not control (Claude Code, Cursor) gets recorded.
fn transcript_cmd(args: &[String]) -> std::io::Result<()> {
    let Some(path) = flag(args, "--from-audit") else {
        eprintln!(
            "agentctl transcript: --from-audit <session.jsonl> is required.\n\n\
             \x20   agentctl transcript --from-audit ~/.agentctl/audit/sess-123.jsonl \\\n\
             \x20       --task \"write a note in TextEdit\" --out docs/fixtures/claude-code.json"
        );
        std::process::exit(2);
    };
    let jsonl = std::fs::read_to_string(path)?;
    let t = agentctl::transcript::from_audit(
        &jsonl,
        flag(args, "--client").unwrap_or("claude-code"),
        flag(args, "--task").unwrap_or("(task not recorded)"),
    )
    .map_err(fail)?;
    let json = serde_json::to_string_pretty(&t.to_json()).map_err(|e| fail(e.to_string()))?;
    match flag(args, "--out") {
        Some(out) => {
            std::fs::write(out, json + "\n")?;
            let (ok, total) = t.tally();
            eprintln!("wrote {out} — {ok}/{total} tool calls succeeded");
        }
        None => println!("{json}"),
    }
    Ok(())
}

fn doctor() {
    let cfg = config_or_exit();
    println!("agentctl doctor");
    println!("  os:              {}", std::env::consts::OS);
    println!("  protocol:        {PROTOCOL_VERSION}");
    println!(
        "  kill switch:     {} (present: {})",
        cfg.kill_switch_file.display(),
        cfg.kill_switch_file.exists()
    );
    // If it is engaged, why matters more than that it is: a STOP file with no
    // explanation looks like a bug rather than a decision.
    if let Ok(first) = std::fs::read_to_string(&cfg.kill_switch_file) {
        if let Some(line) = first.lines().next().filter(|l| !l.trim().is_empty()) {
            println!("                   ENGAGED — {line}");
        }
    }
    println!("  audit dir:       {}", cfg.audit_dir.display());
    println!(
        "  config file:     {} (present: {})",
        mcp_policy::config_path().display(),
        mcp_policy::config_path().exists()
    );
    println!("  allowed apps:    {}", cfg.allowed_apps.join(", "));
    println!("  terminal apps:   {} entries", cfg.terminal_apps.len());
    println!(
        "  consent channel: {} ({} mode, max {} prompts)",
        consent_provider().kind(),
        cfg.mode.as_str(),
        cfg.max_consent_prompts
    );
    println!(
        "  human override:  {} ({}px, grace {}ms)",
        if cfg.human_override { "on" } else { "off" },
        cfg.human_override_px,
        cfg.human_override_grace_ms
    );
    match cfg.access {
        Some(a) => println!(
            "  access profile:  {} (all categories on; {})",
            a.as_str(),
            match a {
                mcp_policy::Access::Ask => "dangerous actions ask you",
                mcp_policy::Access::Auto => "no prompts; destructive actions refused",
                mcp_policy::Access::Bypass => "no prompts; destructive gate OFF",
            }
        ),
        None => println!("  access profile:  none (granular categories/enable)"),
    }
    println!("  enabled cats:    {}", slugs(&cfg));
    print_judge(&cfg);
    println!(
        "  transport:       {}",
        if cfg.http_enabled {
            format!(
                "http on {} (loopback, bearer auth, {} allowed origin(s))",
                cfg.http_bind,
                cfg.http_allowed_origins.len()
            )
        } else {
            "stdio".to_string()
        }
    );
    if cfg.http_enabled && cfg.http_token.is_empty() {
        println!("    -> no http.token set; one is generated per run and printed to stderr");
    }
    print_permissions();
    #[cfg(target_os = "macos")]
    print_ocr_helper(&cfg);
    #[cfg(target_os = "linux")]
    print_linux_session(&cfg);
}

/// Report what the Linux desktop engines depend on: the accessibility bus,
/// the portals, the consent dialog, the OCR models. Preflight only: nothing
/// here opens a portal session, so no approval dialog is raised.
#[cfg(target_os = "linux")]
fn print_linux_session(cfg: &PolicyConfig) {
    let dir = cfg
        .kill_switch_file
        .parent()
        .map(|p| p.join("bin"))
        .unwrap_or_else(std::env::temp_dir);
    // `doctor` runs outside the server's runtime; a scoped thread with its own
    // runtime works whether or not one is already active.
    let report = std::thread::scope(|s| {
        s.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map(|rt| rt.block_on(mcp_linux::doctor(&dir)))
        })
        .join()
    });
    match report {
        Ok(Ok(d)) => {
            for line in d.lines() {
                println!("{line}");
            }
        }
        Ok(Err(e)) => println!("  linux session:   could not probe ({e})"),
        Err(_) => println!("  linux session:   probe panicked"),
    }
}

/// Report the TCC grants the desktop engines depend on.
///
/// Both fail *silently* when missing — AX returns nothing, capture returns the
/// wallpaper — so an unchecked permission looks like an empty desktop rather
/// than a setup problem. Preflight only: a diagnostic must not raise a system
/// permission dialog as a side effect.
#[cfg(target_os = "macos")]
fn print_ocr_helper(cfg: &PolicyConfig) {
    let dir = cfg
        .kill_switch_file
        .parent()
        .map(|p| p.join("bin"))
        .unwrap_or_else(std::env::temp_dir);
    let tools_ok = std::process::Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let state = if !tools_ok {
        "needs the Xcode Command Line Tools (xcode-select --install)".to_string()
    } else if dir.exists()
        && std::fs::read_dir(&dir)
            .map(|mut d| {
                d.any(|e| {
                    e.map(|e| e.file_name().to_string_lossy().starts_with("agentctl-ocr-"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    {
        format!("compiled ({})", dir.display())
    } else {
        "not compiled yet (the first ocr_region call builds it)".to_string()
    };
    println!("  ocr helper:      {state}");
}

/// The judge: whether it is on, whether a key was found, and where it sends.
/// Never the key.
fn print_judge(cfg: &PolicyConfig) {
    if !cfg.judge.enabled {
        println!("  judge:           off ([judge] enabled = \"false\")");
        return;
    }
    let dir = cfg
        .kill_switch_file
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    match mcp_policy::mcp_judge::api_key(&dir) {
        Ok(k) => println!(
            "  judge:           on, {} at {} (key present, {} chars)",
            cfg.judge.model,
            cfg.judge.base_url,
            k.len()
        ),
        Err(e) => {
            println!("  judge:           on, but NO KEY: every judgment is skipped");
            println!("    -> {e}");
        }
    }
}

fn print_permissions() {
    #[cfg(target_os = "macos")]
    {
        let p = mcp_macos::permissions();
        let mark = |ok: bool| if ok { "granted" } else { "MISSING" };
        println!("  accessibility:   {}", mark(p.accessibility));
        println!("  screen record:   {}", mark(p.screen_recording));
        if !p.accessibility {
            println!("    -> System Settings > Privacy & Security > Accessibility");
            println!(
                "       Without it get_ui_tree, ui_action and keyboard input all return nothing."
            );
        }
        if !p.screen_recording {
            println!("    -> System Settings > Privacy & Security > Screen Recording");
            println!("       Without it capture_screen returns the wallpaper, not the windows.");
        }
    }
    #[cfg(target_os = "linux")]
    println!("  permissions:     none needed; the portals ask per grant (see below)");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    println!("  permissions:     n/a on this platform");
}

fn config_print() {
    let cfg = config_or_exit();
    println!("[policy]");
    println!("categories = [{}]", slugs(&cfg));
    println!("enable = {:?}", cfg.enable);
    println!("mode = {}", cfg.mode.as_str());
    println!("allowed_apps = {:?}", cfg.allowed_apps);
    println!("max_denials = {}", cfg.max_denials);
    println!("max_consent_prompts = {}", cfg.max_consent_prompts);
    println!("kill_switch_file = {:?}", cfg.kill_switch_file);
    println!("audit_dir = {:?}", cfg.audit_dir);
}

fn slugs(cfg: &PolicyConfig) -> String {
    cfg.categories
        .iter()
        .map(|c| c.slug())
        .collect::<Vec<_>>()
        .join(", ")
}

fn print_help() {
    println!(
        "agentctl {} — MCP server for GUI/desktop control\n\
         \n\
         USAGE:\n\
         \x20   agentctl [COMMAND]\n\
         \n\
         COMMANDS:\n\
         \x20   serve            Serve MCP over stdio (default)\n\
         \x20   serve --http     Serve MCP over loopback HTTP with bearer auth\n\
         \x20   doctor           Print environment & permission status\n\
         \x20   config print     Print the effective configuration\n\
         \x20   tools            Print the tool reference (--all, --json)\n\
         \x20   bridge           Drive this server with Gemini (--task, --list-models, --prune)\n\
         \x20   test             Replay saved browser_flow UI tests (--attach, --json, --strict)\n\
         \x20   transcript       Rebuild a session record from an audit log\n\
         \x20   help             Show this help\n",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod test_cmd_tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn positional_flows_skips_flags_and_their_values() {
        let a = s(&["agentctl", "test", "login", "--attach", "9333", "checkout", "--strict", "--json", "/tmp/r.json"]);
        assert_eq!(positional_flows(&a), vec!["login", "checkout"]);
        // No names, only flags: empty (means "all flows").
        assert!(positional_flows(&s(&["agentctl", "test", "--strict"])).is_empty());
    }

    fn env_ok(data: Value) -> mcp_types::Envelope {
        mcp_types::Envelope { ok: true, tool: "t".into(), data: Some(data), error: None, image: None }
    }
    fn env_fail(data: Value) -> mcp_types::Envelope {
        mcp_types::Envelope {
            ok: false,
            tool: "t".into(),
            data: Some(data),
            error: Some(mcp_types::ToolError { code: mcp_types::ErrorCode::ActionFailed, message: "x".into(), suggestion: None }),
            image: None,
        }
    }

    #[test]
    fn a_failing_flow_report_carries_the_failing_step_and_captured_issues() {
        let run = env_fail(json!({
            "name": "f", "passed": false, "ran": 2,
            "steps": [
                {"i":0,"op":"navigate","ok":true,"detail":{}},
                {"i":1,"op":"assert","ok":false,"detail":{"passed":false}}
            ]
        }));
        let cap = env_ok(json!({
            "console": [
                {"level":"error","text":"TypeError: boom"},
                {"level":"warn","text":"ignored"}
            ],
            "network": [
                {"method":"GET","url":"https://x/ok","status":200,"ok":true},
                {"method":"POST","url":"https://x/orders","status":500,"ok":false}
            ]
        }));
        let r = build_flow_report("f", &run, &cap);
        assert_eq!(r["passed"], false);
        assert_eq!(r["failing_step"]["op"], "assert");
        // Only error-level console entries and non-2xx requests are issues.
        assert_eq!(r["console_errors"].as_array().unwrap().len(), 1);
        assert_eq!(r["failed_requests"].as_array().unwrap().len(), 1);
        assert_eq!(r["failed_requests"][0]["status"], 500);
    }

    #[test]
    fn print_report_counts_pass_fail_and_issue_flows() {
        let clean_pass = build_flow_report("a", &env_ok(json!({"passed":true,"ran":3,"steps":[]})), &env_ok(json!({"console":[],"network":[]})));
        let pass_with_issue = build_flow_report("b", &env_ok(json!({"passed":true,"ran":1,"steps":[]})), &env_ok(json!({"console":[{"level":"error","text":"e"}],"network":[]})));
        let fail = build_flow_report("c", &env_fail(json!({"passed":false,"ran":1,"steps":[{"i":0,"op":"assert","ok":false,"detail":{}}]})), &env_ok(json!({"console":[],"network":[]})));
        let (passed, failed, issues) = print_report(&[clean_pass, pass_with_issue, fail], false);
        assert_eq!((passed, failed, issues), (2, 1, 1));
    }
}
