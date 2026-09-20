//! Engine wiring: which capability engines exist, and how the operator's
//! config maps onto each one's own policy type.
//!
//! Split out of `main.rs` so tests, the tool-reference generator and the
//! reference-client bridge can build the *same* module set the server serves,
//! rather than a lookalike that drifts.

use std::sync::Arc;

use mcp_policy::{Mode, PolicyConfig};
use mcp_types::ToolModule;

use crate::tools_system::SystemModule;

/// Assemble the enabled engines. On macOS the real AXUIElement backend
/// (perception plus semantic input) is wired in, on Linux the AT-SPI and
/// portal backend; elsewhere only the diagnostic tools are present. The a11y and input engines share one snapshot arena so
/// input can act on refs from `get_ui_tree`.
/// The slice of config the OS-independent engines need.
pub struct EngineConfig {
    pub allowed_origins: Vec<String>,
    pub browser_allow_private: bool,
    pub fs_roots: Vec<std::path::PathBuf>,
    pub allowed_commands: Vec<String>,
    pub allow_shell: bool,
    pub allowed_hosts: Vec<String>,
    pub allow_private_network: bool,
    pub allowed_services: Vec<String>,
    pub allowed_shells: Vec<String>,
    pub max_pty_sessions: usize,
    pub max_pty_buffer: usize,
    pub allowed_sources: Vec<String>,
    pub allow_arbitrary_source: bool,
    pub package_allowlist: Vec<String>,
    pub package_denylist: Vec<String>,
    pub memory_store: std::path::PathBuf,
    pub max_recipes: usize,
    pub autonomous: bool,
    pub bypass: bool,
    // The vision engine only exists where there is a capture backend.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub vision: mcp_vision::VisionConfig,
}

impl From<&PolicyConfig> for EngineConfig {
    fn from(c: &PolicyConfig) -> Self {
        EngineConfig {
            allowed_origins: c.allowed_origins.clone(),
            browser_allow_private: c.browser_allow_private,
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
            bypass: c.access == Some(mcp_policy::Access::Bypass),
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            vision: vision_config(c),
        }
    }
}

/// Map the operator's `[vision]` settings onto the capture engine's own config.
///
///
/// `mcp-policy` carries these as plain numbers so it need not depend on an
/// engine; the translation (including turning `default_detail` from a string
/// into a `Detail`) happens here, at the composition root.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn vision_config(c: &PolicyConfig) -> mcp_vision::VisionConfig {
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

/// Wire every engine the config enables.
///
/// Takes the whole `PolicyConfig` rather than a handful of extracted fields so
/// there is exactly one place a new setting has to be threaded through, and so
/// callers cannot build a *nearly* correct server by forgetting an argument.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(unused_variables)
)]
pub fn build_modules(cfg: &PolicyConfig) -> Vec<Arc<dyn ToolModule>> {
    build_stack(cfg).0
}

/// What the composition root needs beyond the module list: the handles the
/// human-override watcher joins together. An engine must never be able to trip
/// the kill switch itself, so the engine exposes a sensor and the root wires it
/// to the brake.
#[derive(Default)]
pub struct Wiring {
    pub input: Option<Arc<dyn mcp_input::InputBackend>>,
    pub activity: Option<Arc<mcp_input::Activity>>,
    pub desktop: Option<Arc<dyn mcp_desktop::DesktopBackend>>,
    /// The judge, built once here so the policy kernel and the engines that
    /// consult it share one set of counters.
    pub judge: Option<Arc<mcp_policy::mcp_judge::Judge>>,
}

/// Where agentctl keeps state beside the config: the kill switch's directory.
fn state_dir(cfg: &PolicyConfig) -> std::path::PathBuf {
    cfg.kill_switch_file
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
}

/// Wire every engine, and hand back the extra handles.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(unused_variables)
)]
pub fn build_stack(cfg: &PolicyConfig) -> (Vec<Arc<dyn ToolModule>>, Wiring) {
    let engines = EngineConfig::from(cfg);
    let autonomous = matches!(cfg.mode, Mode::Autonomous);
    let bypass = cfg.access == Some(mcp_policy::Access::Bypass);
    let allowed_apps = cfg.allowed_apps.clone();
    let terminal_apps = cfg.terminal_apps.clone();
    let mut modules: Vec<Arc<dyn ToolModule>> = vec![Arc::new(SystemModule)];
    // Only a platform with a desktop backend fills this in; elsewhere the
    // Wiring stays empty and the `mut` is genuinely unused.
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(unused_mut))]
    let mut wiring = Wiring::default();
    // Built even when disabled: a disabled judge answers every question with
    // `Disabled`, which every caller treats as "no opinion".
    let judge = mcp_policy::mcp_judge::Judge::from_config(cfg.judge.clone(), &state_dir(cfg));
    wiring.judge = Some(judge.clone());
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let audio_roots = engines.fs_roots.clone();

    // OS-independent engines, wired on every platform. Each one is closed by
    // default: with no roots/commands/hosts/services configured it refuses
    // everything rather than falling open.
    {
        use mcp_browser::{BrowserModule, CdpBackend, FlowStore, NavPolicy, VisualStore};
        use mcp_fs::{default_denied, FsModule, Jail};
        use mcp_memory::{MemoryModule, Store as MemoryStore};
        use mcp_net::{NetModule, NetPolicy};
        use mcp_pkg::{PkgModule, PkgPolicy};
        use mcp_proc::{ExecPolicy, ProcModule};
        use mcp_pty::{PtyModule, PtyPolicy};
        use mcp_sec::SecModule;
        use mcp_sys::SysModule;

        modules.push(Arc::new(
            BrowserModule::new(Arc::new(CdpBackend::new(NavPolicy::new(
                &engines.allowed_origins,
                engines.browser_allow_private,
            ))))
            .with_flow_store(FlowStore::new(
                state_dir(cfg).join("browser_flows.json"),
                200,
                200,
            ))
            .with_visual_store(VisualStore::new(
                state_dir(cfg).join("browser_baselines.json"),
                500,
            ))
            .with_judge(judge.clone()),
        ));

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
            bypass: engines.bypass,
            judge: Some(judge.clone()),
            ..PtyPolicy::default()
        })));

        modules.push(Arc::new(PkgModule::new(PkgPolicy {
            allowed_sources: engines.allowed_sources,
            allow_arbitrary_source: engines.allow_arbitrary_source,
            allowlist: engines.package_allowlist,
            denylist: engines.package_denylist,
            ..PkgPolicy::default()
        })));

        modules.push(Arc::new(
            MemoryModule::new(MemoryStore::new(
                engines.memory_store,
                engines.max_recipes,
                200,
            ))
            .with_judge(judge.clone()),
        ));
    }

    #[cfg(target_os = "macos")]
    {
        use mcp_a11y::A11yModule;
        use mcp_desktop::{DesktopModule, MacosDesktop};
        use mcp_input::{InputModule, InputPolicy};
        use mcp_macos::MacosBackend;
        use mcp_vision::VisionModule;
        use mcp_window::WindowModule;

        // The OCR helper is cached beside the rest of the agentctl state, next to
        // the config and the audit log, rather than in a temp directory that a
        // reboot would clear.
        let helper_dir = cfg
            .kill_switch_file
            .parent()
            .map(|p| p.join("bin"))
            .unwrap_or_else(std::env::temp_dir);
        let backend = Arc::new(MacosBackend::new().with_helper_dir(helper_dir));
        let a11y = A11yModule::new(backend.clone(), 12_000).with_judge(judge.clone());
        let arena = a11y.arena();
        let input_policy = InputPolicy {
            autonomous,
            bypass,
            terminal_apps,
            judge: Some(judge.clone()),
            ..InputPolicy::default()
        };
        // The postcondition verifier shares the wait evaluator with wait_for,
        // so `expect` and an explicit wait cannot disagree about when the UI
        // has settled.
        let evaluator = mcp_window::WaitEvaluator::new(backend.clone(), backend.clone())
            .with_judge(judge.clone());
        let verifier = Arc::new(mcp_input::Verifier::new(
            evaluator,
            backend.clone(),
            arena.clone(),
        ));
        let activity = mcp_input::Activity::new();
        let input = InputModule::new(backend.clone(), arena, input_policy)
            .with_verifier(verifier)
            .with_activity(activity.clone());
        let vision = VisionModule::new(backend.clone(), engines.vision);
        let window = WindowModule::new(backend.clone(), backend.clone(), allowed_apps)
            .with_judge(judge.clone());
        modules.push(Arc::new(a11y));
        modules.push(Arc::new(input));
        modules.push(Arc::new(vision));
        modules.push(Arc::new(window));
        // `play_audio` is bounded by the same roots as the filesystem engine:
        // an agent able to name any path could use the speakers to read out a
        // file it was never allowed to open.
        let desktop_backend = Arc::new(MacosDesktop::new());
        modules.push(Arc::new(DesktopModule::new(
            desktop_backend.clone(),
            audio_roots,
        )));
        wiring.input = Some(backend.clone());
        wiring.activity = Some(activity);
        wiring.desktop = Some(desktop_backend);
    }
    #[cfg(target_os = "linux")]
    {
        use mcp_a11y::A11yModule;
        use mcp_desktop::DesktopModule;
        use mcp_input::{InputModule, InputPolicy};
        use mcp_linux::{LinuxBackend, LinuxDesktop};
        use mcp_vision::VisionModule;
        use mcp_window::WindowModule;

        // OCR models and the portal grant are cached beside the rest of the
        // agentctl state, next to the config and the audit log.
        let helper_dir = cfg
            .kill_switch_file
            .parent()
            .map(|p| p.join("bin"))
            .unwrap_or_else(std::env::temp_dir);
        let backend = Arc::new(LinuxBackend::new().with_helper_dir(helper_dir));
        let a11y = A11yModule::new(backend.clone(), 12_000).with_judge(judge.clone());
        let arena = a11y.arena();
        let input_policy = InputPolicy {
            autonomous,
            bypass,
            terminal_apps,
            judge: Some(judge.clone()),
            ..InputPolicy::default()
        };
        let evaluator = mcp_window::WaitEvaluator::new(backend.clone(), backend.clone())
            .with_judge(judge.clone());
        let verifier = Arc::new(mcp_input::Verifier::new(
            evaluator,
            backend.clone(),
            arena.clone(),
        ));
        let activity = mcp_input::Activity::new();
        let input = InputModule::new(backend.clone(), arena, input_policy)
            .with_verifier(verifier)
            .with_activity(activity.clone());
        let vision = VisionModule::new(backend.clone(), engines.vision);
        let window = WindowModule::new(backend.clone(), backend.clone(), allowed_apps)
            .with_judge(judge.clone());
        modules.push(Arc::new(a11y));
        modules.push(Arc::new(input));
        modules.push(Arc::new(vision));
        modules.push(Arc::new(window));
        let desktop_backend = Arc::new(LinuxDesktop::new());
        modules.push(Arc::new(DesktopModule::new(
            desktop_backend.clone(),
            audio_roots,
        )));
        wiring.input = Some(backend.clone());
        wiring.activity = Some(activity);
        wiring.desktop = Some(desktop_backend);
    }
    (modules, wiring)
}
