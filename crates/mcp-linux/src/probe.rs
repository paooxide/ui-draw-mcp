//! The facts behind `effects`, read from AT-SPI.
//!
//! AT-SPI has no cheap equivalent of "the focused element" (finding it means
//! walking for the `Focused` state), so that fact is left out here and an
//! action's `effects` on Linux never carries `focus`. What it does carry is
//! read from one level of the application's children: the windows, and which
//! of them are dialogs. Dialogs are windows of their own on this platform, so
//! they are reported as overlays and not also as windows.

use async_trait::async_trait;
use atspi::State;
use mcp_input::{Overlay, UiFacts, UiProbe};

use crate::a11y::{self, AppInfo};
use crate::backend::LinuxBackend;
use crate::roles;

/// Children of the application examined, as `windows_of` does.
const MAX_CHILDREN: usize = 400;

#[async_trait]
impl UiProbe for LinuxBackend {
    async fn probe(&self, _describe_new: Option<&[Overlay]>) -> UiFacts {
        let mut facts = UiFacts::default();
        let Ok(conn) = self.conn().await else {
            return facts;
        };
        let active = a11y::active_app(&conn).await.ok().flatten();
        facts.frontmost = active.as_ref().map(|a| a.name.clone());
        // The app the session drives, else the one in front. Reading it does
        // not make it the target.
        let pinned = self.lock().target.as_ref().map(|t| AppInfo {
            obj: t.obj.clone(),
            name: t.name.clone(),
        });
        let Some(app) = pinned.or(active) else {
            return facts;
        };
        let Ok(proxy) = a11y::proxy_for(&conn, &app.obj).await else {
            return facts;
        };
        let Ok(children) = proxy.get_children().await else {
            return facts;
        };
        let (mut windows, mut overlays) = (Vec::new(), Vec::new());
        for obj in children.into_iter().take(MAX_CHILDREN) {
            let Ok(p) = a11y::proxy_for(&conn, &obj).await else {
                continue;
            };
            let (Ok(role), Ok(states)) = (p.get_role().await, p.get_state().await) else {
                continue;
            };
            if !roles::is_window(role) {
                continue;
            }
            let title = p.name().await.unwrap_or_default();
            if roles::is_dialog(role, states) {
                // A dialog that is not showing is a window kept for later.
                if states.contains(State::Showing) {
                    overlays.push(Overlay {
                        kind: roles::dialog_kind(role).to_string(),
                        title: Some(title).filter(|t| !t.is_empty()),
                        text: None,
                        buttons: Vec::new(),
                    });
                }
            } else {
                windows.push(title);
            }
        }
        facts.windows = Some(windows);
        facts.overlays = Some(overlays);
        facts
    }
}
