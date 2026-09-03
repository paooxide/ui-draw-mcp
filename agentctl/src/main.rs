//! `agentctl` — the composition root. Loads config, wires enabled engines into
//! the registry, and serves MCP over stdio. Logs go to **stderr only**; stdout
//! is the protocol channel.

use std::sync::Arc;

use agentctl::{build_modules, consent_provider, new_session_id, tools_doc};
use mcp_core::{HttpConfig, HttpTransport, PROTOCOL_VERSION};
use mcp_policy::{AuditSink, PolicyConfig};
use mcp_types::Category;

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
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
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
    println!("  enabled cats:    {}", slugs(&cfg));
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
}

/// Report the TCC grants the desktop engines depend on.
///
/// Both fail *silently* when missing — AX returns nothing, capture returns the
/// wallpaper — so an unchecked permission looks like an empty desktop rather
/// than a setup problem. Preflight only: a diagnostic must not raise a system
/// permission dialog as a side effect.
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
    #[cfg(not(target_os = "macos"))]
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
         \x20   help             Show this help\n",
        env!("CARGO_PKG_VERSION")
    );
}
