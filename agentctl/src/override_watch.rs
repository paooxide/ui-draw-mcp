//! The human-override watcher: stop when somebody reaches for the mouse.
//!
//! Lives at the composition root because it is the only place allowed to hold
//! both an engine's backend and the policy. An engine must never be able to
//! trip the kill switch itself — that is the whole point of the gate — so the
//! engine exposes a sensor, the policy exposes a brake, and this joins them.

use std::sync::Arc;

use mcp_input::{Activity, Detector, InputBackend, OverrideConfig, Verdict};
use mcp_policy::Policy;

/// Start watching. Returns immediately; the loop runs until the pointer cannot
/// be read, or the switch trips.
pub fn spawn(
    input: Arc<dyn InputBackend>,
    activity: Arc<Activity>,
    cfg: OverrideConfig,
    policy: Arc<Policy>,
    desktop: Option<Arc<dyn mcp_desktop::DesktopBackend>>,
    session_id: String,
) {
    if !cfg.enabled {
        tracing::debug!("human override is disabled");
        return;
    }
    // Known up front on some backends (Wayland): say so now, at warn level,
    // rather than at the first call. A person relying on moving the mouse to
    // stop the agent must be told that it will not work.
    if let Some(why) = input.pointer_unavailable_reason() {
        tracing::warn!(
            reason = %why,
            "human takeover detection is OFF: moving the mouse will not stop the agent; \
             use the STOP file instead"
        );
        return;
    }
    tokio::spawn(async move {
        let mut detector = Detector::new();
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(cfg.poll_ms.max(10)));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut probed = false;

        loop {
            interval.tick().await;
            let now = mcp_policy::now_ms() as u64;

            // Only sample while the server is actually driving. A human moving
            // their own mouse with nothing running is not an event, and polling
            // the pointer fifty times a second for no reason is rude.
            if !activity.driving(now, cfg.grace_ms) {
                detector.observe(now, false, &[], None, &cfg);
                continue;
            }

            let observed = match input.pointer_position().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(error = ?e, "pointer read failed");
                    None
                }
            };
            if !probed {
                probed = true;
                if observed.is_none() {
                    tracing::warn!(
                        "human takeover detection is OFF: this backend could not read the \
                         pointer, so there is nothing to compare against; moving the mouse \
                         will not stop the agent"
                    );
                    return;
                }
            }

            let recent = input.recent_pointer_sets();
            if let Verdict::Tripped {
                observed,
                nearest,
                distance,
            } = detector.observe(now, true, &recent, observed, &cfg)
            {
                let reason = format!(
                    "human-override: the pointer moved to ({:.0}, {:.0}), {distance:.0}px from \
                     anywhere agentctl put it{}",
                    observed.0,
                    observed.1,
                    match nearest {
                        Some((x, y)) => format!(" (nearest was ({x:.0}, {y:.0}))"),
                        None => String::new(),
                    }
                );
                // Abandon in-flight work first: a drag must release the button
                // before the person starts moving the mouse in earnest.
                input.cancel_pending();
                policy.trip_kill_switch(&session_id, &reason);

                // Tell the human what just happened and how to undo it. Being
                // stopped with no explanation is indistinguishable from being
                // broken.
                if let Some(d) = &desktop {
                    let _ = d
                        .notify(
                            "agentctl stopped",
                            &format!(
                                "You moved the mouse while the agent was driving. \
                                 Delete {} to resume.",
                                policy.config().kill_switch_file.display()
                            ),
                            None,
                            true,
                        )
                        .await;
                }
                return;
            }
        }
    });
}
