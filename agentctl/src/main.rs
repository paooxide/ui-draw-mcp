//! `agentctl` — the composition root. Loads config, wires enabled engines into
//! the registry, and serves MCP over stdio. Logs go to **stderr only**; stdout
//! is the protocol channel.

use std::sync::Arc;

use mcp_core::{HttpConfig, HttpTransport, Registry, Server, PROTOCOL_VERSION};
use mcp_policy::{AuditSink, Mode, Policy, PolicyConfig, Redactor};
use mcp_types::{Category, ToolModule};

mod tools_system;
use tools_system::SystemModule;

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

/// The out-of-band human-approval channel.
///
/// On macOS this is a real native dialog (default button **Deny**, and the
/// timeout denies). Elsewhere there is no channel yet, so consent requests are
/// refused rather than silently allowed.
fn consent_provider() -> Arc<dyn mcp_policy::ConsentProvider> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(mcp_policy::DialogConsent::default())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(mcp_policy::NoConsent)
    }
}

fn new_session_id() -> String {
    let pid = std::process::id();
    let ms = mcp_policy::now_ms();
    format!("sess-{pid}-{ms}")
}

async fn serve(force_http: bool) -> std::io::Result<()> {
    let cfg = config_or_exit();
    let session_id = new_session_id();
    let audit = AuditSink::file(cfg.audit_dir.clone(), &session_id)?;
    let autonomous = matches!(cfg.mode, Mode::Autonomous);
    let allowed_apps = cfg.allowed_apps.clone();
    let terminal_apps = cfg.terminal_apps.clone();
    let engines = EngineConfig::from(&cfg);
    let http = (force_http || cfg.http_enabled).then(|| http_config(&cfg));
    let policy =
        Arc::new(Policy::new(cfg, audit, Redactor::empty()).with_consent(consent_provider()));
    let modules = build_modules(autonomous, allowed_apps, terminal_apps, engines);
    let registry = Registry::build(modules)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let server = Server::new(registry, policy, session_id.clone());

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

/// Assemble the enabled engines. On macOS the real AXUIElement backend
/// (perception plus semantic input) is wired in; elsewhere only the diagnostic
/// tools are present. The a11y and input engines share one snapshot arena so
/// input can act on refs from `get_ui_tree`.
/// The slice of config the OS-independent engines need.
struct EngineConfig {
    allowed_origins: Vec<String>,
    fs_roots: Vec<std::path::PathBuf>,
    allowed_commands: Vec<String>,
    allow_shell: bool,
    allowed_hosts: Vec<String>,
    allow_private_network: bool,
    allowed_services: Vec<String>,
    allowed_shells: Vec<String>,
    max_pty_sessions: usize,
    max_pty_buffer: usize,
    allowed_sources: Vec<String>,
    allow_arbitrary_source: bool,
    package_allowlist: Vec<String>,
    package_denylist: Vec<String>,
    memory_store: std::path::PathBuf,
    max_recipes: usize,
    autonomous: bool,
    // The vision engine only exists where there is a capture backend.
    #[cfg(target_os = "macos")]
    vision: mcp_vision::VisionConfig,
}

impl From<&PolicyConfig> for EngineConfig {
    fn from(c: &PolicyConfig) -> Self {
        EngineConfig {
            allowed_origins: c.allowed_origins.clone(),
            fs_roots: c.fs_roots.clone(),
            allowed_commands: c.allowed_commands.clone(),
            allow_shell: c.allow_shell,
            allowed_hosts: c.allowed_hosts.clone(),
            allow_private_network: c.allow_private_network,
            allowed_services: c.allowed_services.clone(),
            allowed_shells: c.allowed_shells.clone(),
            max_pty_sessions: c.max_pty_sessions,
            max_pty_buffer: c.max_pty_buffer,
            allowed_sources: c.allowed_sources.clone(),
            allow_arbitrary_source: c.allow_arbitrary_source,
            package_allowlist: c.package_allowlist.clone(),
            package_denylist: c.package_denylist.clone(),
            memory_store: c.memory_store.clone(),
            max_recipes: c.max_recipes,
            autonomous: matches!(c.mode, Mode::Autonomous),
            #[cfg(target_os = "macos")]
            vision: vision_config(c),
        }
    }
}

/// Map the operator's `[vision]` settings onto the capture engine's own config.
///
///
/// `mcp-policy` carries these as plain numbers so it need not depend on an
/// engine; the translation — including turning `default_detail` from a string
/// into a `Detail` — happens here, at the composition root.
#[cfg(target_os = "macos")]
fn vision_config(c: &PolicyConfig) -> mcp_vision::VisionConfig {
    mcp_vision::VisionConfig {
        detail_low_px: c.vision_detail_low_px,
        detail_balanced_px: c.vision_detail_balanced_px,
        detail_full_px: c.vision_detail_full_px,
        // The string was validated at load; fall back rather than panic if a
        // future spelling slips through.
        default_detail: mcp_vision::Detail::parse(&c.vision_default_detail)
            .unwrap_or(mcp_vision::Detail::Full),
        unchanged_mad: c.vision_unchanged_mad,
        pixels_per_token: c.vision_pixels_per_token,
        max_image_bytes: c.vision_max_image_bytes,
    }
}

#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn build_modules(
    autonomous: bool,
    allowed_apps: Vec<String>,
    terminal_apps: Vec<String>,
    engines: EngineConfig,
) -> Vec<Arc<dyn ToolModule>> {
    let mut modules: Vec<Arc<dyn ToolModule>> = vec![Arc::new(SystemModule)];
    #[cfg(target_os = "macos")]
    let audio_roots = engines.fs_roots.clone();

    // OS-independent engines — wired on every platform. Each one is closed by
    // default: with no roots/commands/hosts/services configured it refuses
    // everything rather than falling open.
    {
        use mcp_browser::{BrowserModule, CdpBackend};
        use mcp_fs::{default_denied, FsModule, Jail};
        use mcp_memory::{MemoryModule, Store as MemoryStore};
        use mcp_net::{NetModule, NetPolicy};
        use mcp_pkg::{PkgModule, PkgPolicy};
        use mcp_proc::{ExecPolicy, ProcModule};
        use mcp_pty::{PtyModule, PtyPolicy};
        use mcp_sec::SecModule;
        use mcp_sys::SysModule;

        modules.push(Arc::new(BrowserModule::new(Arc::new(CdpBackend::new(
            engines.allowed_origins,
        )))));

        let jail = Jail::new(engines.fs_roots.clone(), default_denied());
        modules.push(Arc::new(FsModule::new(jail, 1_000_000, 500)));

        let exec_policy = ExecPolicy {
            allowed: engines.allowed_commands,
            allow_shell: engines.allow_shell,
            ..ExecPolicy::default()
        };
        modules.push(Arc::new(ProcModule::new(
            exec_policy,
            engines.fs_roots.clone(),
        )));

        modules.push(Arc::new(NetModule::new(
            NetPolicy {
                allowed_hosts: engines.allowed_hosts,
                allow_private: engines.allow_private_network,
            },
            20,
            200_000,
        )));

        modules.push(Arc::new(SysModule::default()));
        modules.push(Arc::new(SecModule::new(engines.allowed_services)));

        // Interactive shells. `allow_shell` gates this for the same reason it
        // gates `exec --shell`: a PTY *is* a shell, and gating one but not the
        // other would be theatre.
        modules.push(Arc::new(PtyModule::new(PtyPolicy {
            allowed_shells: engines.allowed_shells,
            allow_shell: engines.allow_shell,
            roots: engines.fs_roots.clone(),
            max_sessions: engines.max_pty_sessions,
            max_buffer: engines.max_pty_buffer,
            autonomous: engines.autonomous,
            ..PtyPolicy::default()
        })));

        modules.push(Arc::new(PkgModule::new(PkgPolicy {
            allowed_sources: engines.allowed_sources,
            allow_arbitrary_source: engines.allow_arbitrary_source,
            allowlist: engines.package_allowlist,
            denylist: engines.package_denylist,
            ..PkgPolicy::default()
        })));

        modules.push(Arc::new(MemoryModule::new(MemoryStore::new(
            engines.memory_store,
            engines.max_recipes,
            200,
        ))));
    }

    #[cfg(target_os = "macos")]
    {
        use mcp_a11y::A11yModule;
        use mcp_desktop::{DesktopModule, MacosDesktop};
        use mcp_input::{InputModule, InputPolicy};
        use mcp_macos::MacosBackend;
        use mcp_vision::VisionModule;
        use mcp_window::WindowModule;

        let backend = Arc::new(MacosBackend::new());
        let a11y = A11yModule::new(backend.clone(), 12_000);
        let arena = a11y.arena();
        let input_policy = InputPolicy {
            autonomous,
            terminal_apps,
            ..InputPolicy::default()
        };
        let input = InputModule::new(backend.clone(), arena, input_policy);
        let vision = VisionModule::new(backend.clone(), engines.vision);
        let window = WindowModule::new(backend.clone(), backend, allowed_apps);
        modules.push(Arc::new(a11y));
        modules.push(Arc::new(input));
        modules.push(Arc::new(vision));
        modules.push(Arc::new(window));
        // `play_audio` is bounded by the same roots as the filesystem engine:
        // an agent able to name any path could use the speakers to read out a
        // file it was never allowed to open.
        modules.push(Arc::new(DesktopModule::new(
            Arc::new(MacosDesktop::new()),
            audio_roots,
        )));
    }
    modules
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
        match cfg.mode {
            Mode::Interactive => "interactive",
            Mode::Autonomous => "autonomous",
        },
        cfg.max_consent_prompts
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
    println!(
        "mode = {}",
        match cfg.mode {
            Mode::Interactive => "interactive",
            Mode::Autonomous => "autonomous",
        }
    );
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
         \x20   help             Show this help\n",
        env!("CARGO_PKG_VERSION")
    );
}
