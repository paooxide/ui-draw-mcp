//! `foreground = "never"` for the window engine: which calls are refused, what
//! the refusal says, and the backend wrapper that makes the refusal hold.
//!
//! The same two layers as the input engine (see `mcp_input::ForegroundInput`):
//! the tool layer refuses first and names the alternative; the wrapper is the
//! boundary no later tool can go around. Anything not known to leave the
//! foreground alone is refused, and the refusal says so when the doubt is
//! the reason.

use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::Refusal;
use serde_json::Value;

use crate::backend::{
    DialogInfo, DialogScope, MenuItemInfo, Rect, TargetInfo, WindowAction, WindowBackend,
    WindowError, WindowInfo,
};

/// How a tool stands under `foreground = "never"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stance {
    /// Reads, or acts without taking the foreground.
    Background,
    /// Always brings something forward or opens a menu in another app.
    Refused,
    /// Depends on the arguments: see [`by_args`].
    ByArgs,
    /// Has a background form the module uses instead (`launch`).
    Substituted,
}

/// Every tool of the window engine and where it stands. A test requires this
/// to list exactly the advertised tools; until a new one is listed it is
/// refused.
pub(crate) const TOOLS: &[(&str, Stance)] = &[
    ("list_windows", Stance::Background),
    ("list_apps", Stance::Background),
    ("launch", Stance::Substituted),
    // Ends a process by name. Nothing is raised; the OS decides what is
    // frontmost afterwards, as it does when the user quits an app.
    ("close_app", Stance::Background),
    ("control_window", Stance::ByArgs),
    ("menu_open", Stance::Refused),
    ("menu_invoke", Stance::Refused),
    ("handle_dialogs", Stance::Background),
    ("menu_list", Stance::Background),
    ("wait_for", Stance::Background),
    ("focus_app", Stance::ByArgs),
];

const FRONT: &str = "bring an app or window to the front";
const UNVERIFIED_RESTORE: &str = "un-minimise a window, which brings it to the front (not \
                                  verified to be background-safe, so it is refused)";
const UNVERIFIED_MENU: &str = "open a menu in another app's menu bar, which may bring that app \
                               forward (not verified to be background-safe, so it is refused)";
const NOT_NEEDED: &str = "focus_app with `activate: false` pins the app without raising it, \
                          get_ui_tree with `app` reads it the same way, and ui_action, \
                          set_value and select_text act on its refs while it stays behind";
const MENU_INSTEAD: &str = "menu_list reads a menu's items and shortcuts without opening \
                            anything; ui_action click on the control that does the same thing";

/// The refusal for `tool` called with `args`, or `None` when it may run.
pub fn tool_refusal(backend: &dyn WindowBackend, tool: &str, args: &Value) -> Option<Refusal> {
    match TOOLS.iter().find(|(n, _)| *n == tool).map(|(_, s)| *s) {
        Some(Stance::Background | Stance::Substituted) => None,
        Some(Stance::Refused) => Some(Refusal::new(tool, UNVERIFIED_MENU, Some(MENU_INSTEAD))),
        Some(Stance::ByArgs) => by_args(backend, tool, args),
        None => Some(Refusal::new(
            tool,
            "do something this server has not classified as background-safe",
            None,
        )),
    }
}

fn by_args(backend: &dyn WindowBackend, tool: &str, args: &Value) -> Option<Refusal> {
    match tool {
        "control_window" => {
            let name = args.get("action").and_then(Value::as_str)?;
            // An invalid action is INVALID_ARGS before it does anything.
            let action = WindowAction::parse(name)?;
            action_refusal(backend, action)
        }
        // `app` pins and, unless `activate` is the boolean false, raises; no
        // `app` only releases the pin. Anything but a plain `false` counts as
        // the raising form, so a value the module will reject is refused here
        // first rather than guessed to be the quiet one.
        "focus_app" => (args.get("app").is_some_and(|a| !a.is_null())
            && args.get("activate") != Some(&Value::Bool(false)))
        .then(|| Refusal::new(tool, FRONT, Some(NOT_NEEDED))),
        _ => None,
    }
}

fn action_refusal(backend: &dyn WindowBackend, action: WindowAction) -> Option<Refusal> {
    let what = |a: &str| format!("control_window {a}");
    match action {
        WindowAction::Focus => Some(Refusal::new(what("focus"), FRONT, Some(NOT_NEEDED))),
        WindowAction::Restore => Some(Refusal::new(what("restore"), UNVERIFIED_RESTORE, None)),
        a if backend.control_takes_foreground(a) => Some(Refusal::new(
            what(&format!("{a:?}").to_lowercase()),
            "bring the window to the front first: on this platform that window action cannot \
             run in the background",
            None,
        )),
        _ => None,
    }
}

/// A [`WindowBackend`] that brings nothing forward, whatever it is asked: the
/// backend boundary of `foreground = "never"`. Built by the composition root
/// only when the operator set `never`.
///
/// `launch` is refused here and `launch_in_background` forwarded, so the only
/// way to start an app through this wrapper is the form that asks not to
/// activate it.
pub struct ForegroundWindow {
    inner: Arc<dyn WindowBackend>,
}

impl ForegroundWindow {
    pub fn new(inner: Arc<dyn WindowBackend>) -> Self {
        ForegroundWindow { inner }
    }
}

fn refused(what: &str, effect: &'static str, instead: Option<&str>) -> WindowError {
    WindowError::Refused(Refusal::new(what, effect, instead).one_line())
}

#[async_trait]
impl WindowBackend for ForegroundWindow {
    async fn list_windows(&self, app: Option<&str>) -> Result<Vec<WindowInfo>, WindowError> {
        self.inner.list_windows(app).await
    }
    async fn list_apps(&self) -> Result<Vec<String>, WindowError> {
        self.inner.list_apps().await
    }
    async fn launch(&self, _app: &str) -> Result<(), WindowError> {
        Err(refused(
            "launch",
            "start an app and bring it to the front",
            Some("launch with foreground = \"never\" starts it without activating it"),
        ))
    }
    async fn launch_in_background(&self, app: &str) -> Result<(), WindowError> {
        self.inner.launch_in_background(app).await
    }
    async fn close_app(&self, target: &str) -> Result<(), WindowError> {
        self.inner.close_app(target).await
    }
    async fn control_window(
        &self,
        app: Option<&str>,
        title: Option<&str>,
        action: WindowAction,
        bounds: Option<Rect>,
    ) -> Result<(), WindowError> {
        if let Some(r) = action_refusal(&*self.inner, action) {
            return Err(WindowError::Refused(r.one_line()));
        }
        self.inner.control_window(app, title, action, bounds).await
    }
    fn control_takes_foreground(&self, action: WindowAction) -> bool {
        self.inner.control_takes_foreground(action)
    }
    async fn menu_open(&self, _app: Option<&str>, _path: &[String]) -> Result<(), WindowError> {
        Err(refused("menu_open", UNVERIFIED_MENU, Some(MENU_INSTEAD)))
    }
    async fn menu_invoke(&self, _app: Option<&str>, _path: &[String]) -> Result<(), WindowError> {
        Err(refused("menu_invoke", UNVERIFIED_MENU, Some(MENU_INSTEAD)))
    }
    async fn list_dialogs(
        &self,
        app: Option<&str>,
        scope: DialogScope,
    ) -> Result<Vec<DialogInfo>, WindowError> {
        self.inner.list_dialogs(app, scope).await
    }
    async fn menu_list(
        &self,
        app: Option<&str>,
        path: &[String],
        depth: u32,
    ) -> Result<Vec<MenuItemInfo>, WindowError> {
        self.inner.menu_list(app, path, depth).await
    }
    async fn focus_app(&self, app: Option<&str>) -> Result<Option<String>, WindowError> {
        match app {
            Some(_) => Err(refused("focus_app", FRONT, Some(NOT_NEEDED))),
            None => self.inner.focus_app(None).await,
        }
    }
    async fn pin_app(&self, app: &str) -> Result<TargetInfo, WindowError> {
        self.inner.pin_app(app).await
    }
    async fn describe_target(&self, app: Option<&str>) -> Result<TargetInfo, WindowError> {
        self.inner.describe_target(app).await
    }
    async fn pinned(&self) -> Option<TargetInfo> {
        self.inner.pinned().await
    }
    fn platform(&self) -> &'static str {
        self.inner.platform()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use mcp_a11y::{A11yBackend, BackendError, RawSnapshot, SnapshotRequest};
    use mcp_types::{CallCtx, CancelToken, ErrorCode, Foreground, ToolModule};
    use serde_json::json;

    use super::*;
    use crate::WindowModule;

    /// Records the name of every backend method it is asked to run, and
    /// answers each with a harmless success. It stands in for no OS: it exists
    /// to show what did and did not reach the backend.
    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<String>>,
        /// What `control_takes_foreground` answers, as a platform would.
        takes_foreground: bool,
    }

    impl Recorder {
        fn note(&self, what: &str) {
            self.calls.lock().unwrap().push(what.to_string());
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl WindowBackend for Recorder {
        async fn list_windows(&self, _: Option<&str>) -> Result<Vec<WindowInfo>, WindowError> {
            self.note("list_windows");
            Ok(Vec::new())
        }
        async fn list_apps(&self) -> Result<Vec<String>, WindowError> {
            self.note("list_apps");
            Ok(Vec::new())
        }
        async fn launch(&self, _: &str) -> Result<(), WindowError> {
            self.note("launch");
            Ok(())
        }
        async fn launch_in_background(&self, _: &str) -> Result<(), WindowError> {
            self.note("launch_in_background");
            Ok(())
        }
        async fn close_app(&self, _: &str) -> Result<(), WindowError> {
            self.note("close_app");
            Ok(())
        }
        async fn control_window(
            &self,
            _: Option<&str>,
            _: Option<&str>,
            action: WindowAction,
            _: Option<Rect>,
        ) -> Result<(), WindowError> {
            self.note(&format!("control_window {action:?}"));
            Ok(())
        }
        fn control_takes_foreground(&self, action: WindowAction) -> bool {
            matches!(action, WindowAction::Focus | WindowAction::Restore) || self.takes_foreground
        }
        async fn menu_open(&self, _: Option<&str>, _: &[String]) -> Result<(), WindowError> {
            self.note("menu_open");
            Ok(())
        }
        async fn menu_invoke(&self, _: Option<&str>, _: &[String]) -> Result<(), WindowError> {
            self.note("menu_invoke");
            Ok(())
        }
        async fn list_dialogs(
            &self,
            _: Option<&str>,
            _: DialogScope,
        ) -> Result<Vec<DialogInfo>, WindowError> {
            self.note("list_dialogs");
            Ok(Vec::new())
        }
        async fn menu_list(
            &self,
            _: Option<&str>,
            _: &[String],
            _: u32,
        ) -> Result<Vec<MenuItemInfo>, WindowError> {
            self.note("menu_list");
            Ok(Vec::new())
        }
        async fn focus_app(&self, app: Option<&str>) -> Result<Option<String>, WindowError> {
            self.note(&format!("focus_app {}", app.unwrap_or("-")));
            Ok(app.map(str::to_string))
        }
        async fn pin_app(&self, app: &str) -> Result<TargetInfo, WindowError> {
            self.note(&format!("pin_app {app}"));
            Ok(TargetInfo {
                app: app.to_string(),
                pid: Some(1),
                from: crate::backend::TargetSource::Named,
            })
        }
        // Resolution is a read the module makes before every call; it is not
        // recorded, so a test of what reached the backend sees only the work.
        async fn describe_target(&self, app: Option<&str>) -> Result<TargetInfo, WindowError> {
            Ok(TargetInfo {
                app: app.unwrap_or("Frontmost").to_string(),
                pid: Some(1),
                from: if app.is_some() {
                    crate::backend::TargetSource::Named
                } else {
                    crate::backend::TargetSource::Frontmost
                },
            })
        }
        async fn pinned(&self) -> Option<TargetInfo> {
            None
        }
        fn platform(&self) -> &'static str {
            "recorder"
        }
    }

    struct NoTree;

    #[async_trait]
    impl A11yBackend for NoTree {
        async fn snapshot(&self, _: &SnapshotRequest) -> Result<RawSnapshot, BackendError> {
            Err(BackendError::Unsupported("test".into()))
        }
        fn platform(&self) -> &'static str {
            "recorder"
        }
    }

    fn module(rec: &Arc<Recorder>, foreground: Foreground) -> WindowModule {
        WindowModule::new(rec.clone(), Arc::new(NoTree), vec!["TextEdit".into()])
            .with_foreground(foreground)
    }

    async fn call(m: &WindowModule, tool: &str, args: Value) -> mcp_types::Envelope {
        m.call(tool, args, &CallCtx::new("t", CancelToken::new()))
            .await
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Want {
        Refused,
        Runs,
    }

    /// One row per way each window tool can be called; a tool with no row
    /// fails `every_window_tool_has_a_foreground_decision`.
    fn cases() -> Vec<(&'static str, Value, Want)> {
        use Want::{Refused, Runs};
        let app = json!({"app":"TextEdit"});
        vec![
            ("list_windows", json!({}), Runs),
            ("list_apps", json!({}), Runs),
            ("launch", app.clone(), Runs),
            ("close_app", app.clone(), Runs),
            ("handle_dialogs", json!({}), Runs),
            ("menu_list", json!({}), Runs),
            ("focus_app", json!({}), Runs),
            ("focus_app", json!({"app": null}), Runs),
            ("focus_app", app.clone(), Refused),
            (
                "focus_app",
                json!({"app":"TextEdit","activate":true}),
                Refused,
            ),
            // Anything that is not a plain `false` is the raising form.
            (
                "focus_app",
                json!({"app":"TextEdit","activate":"false"}),
                Refused,
            ),
            (
                "focus_app",
                json!({"app":"TextEdit","activate":null}),
                Refused,
            ),
            (
                "focus_app",
                json!({"app":"TextEdit","activate":false}),
                Runs,
            ),
            ("focus_app", json!({"activate":false}), Runs),
            ("control_window", json!({"action":"focus"}), Refused),
            ("control_window", json!({"action":"restore"}), Refused),
            ("control_window", json!({"action":"minimize"}), Runs),
            ("control_window", json!({"action":"maximize"}), Runs),
            ("control_window", json!({"action":"close"}), Runs),
            (
                "control_window",
                json!({"action":"move","bounds":{"x":0,"y":0,"w":1,"h":1}}),
                Runs,
            ),
            ("control_window", json!({"action":"bogus"}), Runs),
            ("menu_open", json!({"path":["File"]}), Refused),
            ("menu_invoke", json!({"path":["File","Save"]}), Refused),
        ]
    }

    #[test]
    fn every_window_tool_has_a_foreground_decision() {
        let rec = Arc::new(Recorder::default());
        let m = module(&rec, Foreground::Never);
        let advertised: Vec<String> = m.descriptors().into_iter().map(|d| d.name).collect();
        let classified: Vec<&str> = TOOLS.iter().map(|(n, _)| *n).collect();
        for name in &advertised {
            assert!(
                classified.contains(&name.as_str()),
                "{name} is advertised but has no entry in foreground::TOOLS: decide whether it \
                 can run with foreground = \"never\" (it is refused until somebody does)"
            );
            assert!(
                name == "wait_for" || cases().iter().any(|(t, _, _)| t == name),
                "{name} has no case in cases()"
            );
        }
        for name in &classified {
            assert!(advertised.iter().any(|a| a == name), "{name} is not a tool");
        }
        for (tool, stance) in TOOLS {
            if *stance == Stance::Refused {
                assert!(cases()
                    .iter()
                    .filter(|(t, _, _)| t == tool)
                    .all(|(_, _, w)| *w == Want::Refused));
            }
        }
    }

    #[tokio::test]
    async fn under_never_what_takes_the_foreground_is_refused_before_the_backend() {
        for (tool, args, want) in cases() {
            let rec = Arc::new(Recorder::default());
            let m = module(&rec, Foreground::Never);
            let env = call(&m, tool, args.clone()).await;
            let denied = env
                .error
                .as_ref()
                .is_some_and(|e| e.code == ErrorCode::PolicyDenied);
            match want {
                Want::Refused => {
                    assert!(!env.ok && denied, "{tool} {args}: {env:?}");
                    let err = env.error.unwrap();
                    assert!(err.message.contains("foreground = \"never\""), "{tool}");
                    assert!(err.suggestion.is_some(), "{tool}");
                    assert!(rec.calls().is_empty(), "{tool} {args} reached the backend");
                }
                Want::Runs => assert!(!denied, "{tool} {args}: {env:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_default_refuses_nothing_and_launches_as_it_always_did() {
        for (tool, args, _) in cases() {
            let rec = Arc::new(Recorder::default());
            let m = module(&rec, Foreground::Allow);
            let env = call(&m, tool, args.clone()).await;
            assert!(
                env.error
                    .as_ref()
                    .is_none_or(|e| e.code != ErrorCode::PolicyDenied),
                "{tool} {args} refused under allow: {env:?}"
            );
        }
        let rec = Arc::new(Recorder::default());
        let m = module(&rec, Foreground::Allow);
        assert!(call(&m, "launch", json!({"app":"TextEdit"})).await.ok);
        assert_eq!(rec.calls(), ["launch"]);
    }

    #[tokio::test]
    async fn launch_under_never_asks_not_to_activate_and_still_honours_allowed_apps() {
        let rec = Arc::new(Recorder::default());
        let m = module(&rec, Foreground::Never);
        assert!(call(&m, "launch", json!({"app":"TextEdit"})).await.ok);
        assert_eq!(rec.calls(), ["launch_in_background"]);
        let env = call(&m, "launch", json!({"app":"Safari"})).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        assert_eq!(rec.calls(), ["launch_in_background"]);
    }

    #[tokio::test]
    async fn a_platform_that_cannot_launch_quietly_refuses_the_launch() {
        // The trait's default: no background launch is a refusal, never a
        // fallback to the activating one.
        struct Plain(Recorder);
        #[async_trait]
        impl WindowBackend for Plain {
            async fn list_windows(&self, a: Option<&str>) -> Result<Vec<WindowInfo>, WindowError> {
                self.0.list_windows(a).await
            }
            async fn list_apps(&self) -> Result<Vec<String>, WindowError> {
                self.0.list_apps().await
            }
            async fn launch(&self, a: &str) -> Result<(), WindowError> {
                self.0.launch(a).await
            }
            async fn close_app(&self, a: &str) -> Result<(), WindowError> {
                self.0.close_app(a).await
            }
            async fn control_window(
                &self,
                a: Option<&str>,
                t: Option<&str>,
                w: WindowAction,
                b: Option<Rect>,
            ) -> Result<(), WindowError> {
                self.0.control_window(a, t, w, b).await
            }
            fn control_takes_foreground(&self, _: WindowAction) -> bool {
                true
            }
            async fn menu_open(&self, a: Option<&str>, p: &[String]) -> Result<(), WindowError> {
                self.0.menu_open(a, p).await
            }
            async fn menu_invoke(&self, a: Option<&str>, p: &[String]) -> Result<(), WindowError> {
                self.0.menu_invoke(a, p).await
            }
            async fn list_dialogs(
                &self,
                a: Option<&str>,
                s: DialogScope,
            ) -> Result<Vec<DialogInfo>, WindowError> {
                self.0.list_dialogs(a, s).await
            }
            async fn menu_list(
                &self,
                a: Option<&str>,
                p: &[String],
                d: u32,
            ) -> Result<Vec<MenuItemInfo>, WindowError> {
                self.0.menu_list(a, p, d).await
            }
            async fn focus_app(&self, a: Option<&str>) -> Result<Option<String>, WindowError> {
                self.0.focus_app(a).await
            }
            async fn pin_app(&self, a: &str) -> Result<TargetInfo, WindowError> {
                self.0.pin_app(a).await
            }
            async fn describe_target(&self, a: Option<&str>) -> Result<TargetInfo, WindowError> {
                self.0.describe_target(a).await
            }
            async fn pinned(&self) -> Option<TargetInfo> {
                self.0.pinned().await
            }
            fn platform(&self) -> &'static str {
                "plain"
            }
        }
        let plain = Arc::new(Plain(Recorder::default()));
        let m = WindowModule::new(plain.clone(), Arc::new(NoTree), vec!["TextEdit".into()])
            .with_foreground(Foreground::Never);
        let env = call(&m, "launch", json!({"app":"TextEdit"})).await;
        assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied);
        assert!(plain.0.calls().is_empty());
    }

    #[tokio::test]
    async fn on_a_platform_where_every_window_action_raises_only_move_and_resize_run() {
        let rec = Arc::new(Recorder {
            takes_foreground: true,
            ..Recorder::default()
        });
        let m = module(&rec, Foreground::Never);
        for a in ["minimize", "maximize", "close", "restore", "focus"] {
            let env = call(&m, "control_window", json!({"action": a})).await;
            assert_eq!(env.error.unwrap().code, ErrorCode::PolicyDenied, "{a}");
        }
        assert!(rec.calls().is_empty());
    }

    #[tokio::test]
    async fn the_wrapper_holds_even_when_the_tool_layer_is_skipped() {
        let rec = Arc::new(Recorder::default());
        let g = Arc::new(ForegroundWindow::new(rec.clone()));
        // `allow` at the tool layer: nothing is refused there.
        let m = WindowModule::new(g, Arc::new(NoTree), vec!["TextEdit".into()]);
        for (tool, args, want) in cases() {
            if want == Want::Refused {
                let env = call(&m, tool, args.clone()).await;
                assert!(!env.ok, "{tool} {args} got through the wrapper: {env:?}");
            }
        }
        // The plain `launch` is refused by the wrapper too.
        assert!(!call(&m, "launch", json!({"app":"TextEdit"})).await.ok);
        assert!(rec.calls().is_empty(), "{:?}", rec.calls());
        // Reads and the quiet forms go through.
        for (tool, args) in [
            ("list_windows", json!({})),
            ("close_app", json!({"app":"TextEdit"})),
            ("control_window", json!({"action":"minimize"})),
            ("focus_app", json!({"app": null})),
        ] {
            assert!(call(&m, tool, args).await.ok, "{tool}");
        }
        assert_eq!(
            rec.calls(),
            [
                "list_windows",
                "close_app",
                "control_window Minimize",
                "focus_app -"
            ]
        );
    }

    /// Under `never` the only way to pin an app is the form that does not
    /// raise it, and it works through both layers.
    #[tokio::test]
    async fn the_quiet_pin_runs_under_never_and_through_the_wrapper() {
        let rec = Arc::new(Recorder::default());
        let m = module(&rec, Foreground::Never);
        let env = call(&m, "focus_app", json!({"app":"TextEdit","activate":false})).await;
        assert!(env.ok, "{env:?}");
        assert_eq!(rec.calls(), ["pin_app TextEdit"]);

        let rec = Arc::new(Recorder::default());
        let g = Arc::new(ForegroundWindow::new(rec.clone()));
        let m = WindowModule::new(g, Arc::new(NoTree), vec!["TextEdit".into()]);
        assert!(
            call(&m, "focus_app", json!({"app":"TextEdit","activate":false}))
                .await
                .ok
        );
        assert_eq!(rec.calls(), ["pin_app TextEdit"]);
        // The raising form never reaches the backend through the wrapper.
        assert!(!call(&m, "focus_app", json!({"app":"TextEdit"})).await.ok);
        assert_eq!(rec.calls(), ["pin_app TextEdit"]);
    }

    #[test]
    fn the_refusals_name_the_alternative() {
        let rec = Recorder::default();
        let r = tool_refusal(&rec, "focus_app", &json!({"app":"TextEdit"})).unwrap();
        assert!(r.suggestion().contains("get_ui_tree") && r.suggestion().contains("set_value"));
        assert!(
            r.suggestion().contains("activate: false"),
            "the refusal must name the form that is allowed: {}",
            r.suggestion()
        );
        let r = tool_refusal(&rec, "menu_open", &json!({"path":["File"]})).unwrap();
        assert!(r.suggestion().contains("menu_list"));
        assert!(tool_refusal(&rec, "no_such_tool", &json!({})).is_some());
    }
}
