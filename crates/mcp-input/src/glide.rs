//! Smooth Bezier mouse gliding and movement interpolation for demos,
//! screencasts, and visual presentation flair ("vibes").
//!
//! Rather than instantaneous coordinate teleportation, smooth gliding interpolates
//! natural, organic cubic Bezier curves with ease-in-out velocity profiles.
//! Every waypoint is recorded through the backend's standard move dispatch,
//! maintaining full synchronization with the human takeover detection queue.

use std::time::Duration;
use tokio::time::sleep;

use mcp_types::CancelToken;

use crate::backend::{InputBackend, InputError, MouseKind};

/// Speed and pacing preset for mouse gliding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlidePreset {
    /// Cinematic showcase: ~350ms duration, smooth sweeping curve (ideal for video recording).
    Cinematic,
    /// Interactive demo: ~200ms duration, responsive arc (ideal for live presentations).
    Demo,
    /// Snappy: ~100ms duration, tight arc (subtle visual continuity).
    Snappy,
    /// Instant teleport: 0ms (default headless / CI execution).
    Instant,
}

impl GlidePreset {
    /// The one place a speed name becomes a preset: `cinematic`, `demo`,
    /// `snappy`, and `instant` (or `off`), case-insensitive. `None` for
    /// anything else, so each caller decides whether that is an error; config
    /// loading (`mcp-policy`, which cannot depend on this crate) keeps its own
    /// copy of the list and a test there pins the two together.
    pub fn from_speed(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cinematic" => Some(Self::Cinematic),
            "demo" => Some(Self::Demo),
            "snappy" => Some(Self::Snappy),
            "instant" | "off" => Some(Self::Instant),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cinematic => "cinematic",
            Self::Demo => "demo",
            Self::Snappy => "snappy",
            Self::Instant => "instant",
        }
    }

    pub fn default_duration_ms(&self) -> u64 {
        match self {
            Self::Cinematic => 350,
            Self::Demo => 200,
            Self::Snappy => 100,
            Self::Instant => 0,
        }
    }

    pub fn default_steps(&self) -> u32 {
        match self {
            Self::Cinematic => 24,
            Self::Demo => 16,
            Self::Snappy => 10,
            Self::Instant => 1,
        }
    }

    pub fn default_curvature(&self) -> f64 {
        match self {
            Self::Cinematic => 0.08,
            Self::Demo => 0.05,
            Self::Snappy => 0.03,
            Self::Instant => 0.0,
        }
    }
}

/// The longest a single glide may be asked to take. A pointer that takes
/// longer than this to arrive is not a demo, it is a hang, and the per-waypoint
/// pacing could not honour a larger figure anyway (60 waypoints at 40 ms).
pub const MAX_GLIDE_MS: u64 = 2000;

/// Reject a caller-supplied glide duration above [`MAX_GLIDE_MS`].
pub fn check_duration_ms(ms: u64) -> Result<u64, String> {
    if ms > MAX_GLIDE_MS {
        Err(format!(
            "duration_ms {ms} is above the {MAX_GLIDE_MS} ms maximum for a glide"
        ))
    } else {
        Ok(ms)
    }
}

/// Configuration for smooth cursor gliding.
#[derive(Debug, Clone, PartialEq)]
pub struct GlideConfig {
    pub enabled: bool,
    pub preset: GlidePreset,
    pub duration_ms: Option<u64>,
    pub steps: Option<u32>,
    pub curvature: Option<f64>,
}

impl Default for GlideConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            preset: GlidePreset::Instant,
            duration_ms: None,
            steps: None,
            curvature: None,
        }
    }
}

impl GlideConfig {
    pub fn instant() -> Self {
        Self::default()
    }

    pub fn cinematic() -> Self {
        Self {
            enabled: true,
            preset: GlidePreset::Cinematic,
            duration_ms: None,
            steps: None,
            curvature: None,
        }
    }

    pub fn demo() -> Self {
        Self {
            enabled: true,
            preset: GlidePreset::Demo,
            duration_ms: None,
            steps: None,
            curvature: None,
        }
    }

    pub fn snappy() -> Self {
        Self {
            enabled: true,
            preset: GlidePreset::Snappy,
            duration_ms: None,
            steps: None,
            curvature: None,
        }
    }

    pub fn duration(&self) -> u64 {
        self.duration_ms
            .unwrap_or_else(|| self.preset.default_duration_ms())
            .min(MAX_GLIDE_MS)
    }

    pub fn steps(&self) -> u32 {
        self.steps
            .unwrap_or_else(|| self.preset.default_steps())
            .clamp(2, 60)
    }

    pub fn curvature(&self) -> f64 {
        self.curvature
            .unwrap_or_else(|| self.preset.default_curvature())
            .clamp(-0.25, 0.25)
    }
}

impl From<GlidePreset> for GlideConfig {
    fn from(preset: GlidePreset) -> Self {
        match preset {
            GlidePreset::Cinematic => Self::cinematic(),
            GlidePreset::Demo => Self::demo(),
            GlidePreset::Snappy => Self::snappy(),
            GlidePreset::Instant => Self::instant(),
        }
    }
}

/// Calculate a human-like cubic Bezier trajectory from `from` to `to`.
///
/// Uses an S-curve ease-in-out parameter distribution to mimic natural human
/// wrist and arm motor acceleration and deceleration.
pub fn bezier_interpolate(
    from: (f64, f64),
    to: (f64, f64),
    steps: u32,
    curvature: f64,
) -> Vec<(f64, f64)> {
    let steps = steps.max(1);
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let dist = (dx * dx + dy * dy).sqrt();

    // If already at target or distance negligible, just return the target point.
    if dist < 2.0 || steps == 1 {
        return vec![to];
    }

    // Normal vector perpendicular to the travel direction.
    let (nx, ny) = (-dy / dist, dx / dist);

    // Deflection arc proportional to distance, capped to avoid erratic off-screen loops.
    let arc = (dist * curvature).clamp(-60.0, 60.0);

    // Cubic Bezier control points:
    // P1: starts along vector with deflection
    // P2: approaches target with diminishing deflection
    let p0 = from;
    let p1 = (from.0 + 0.28 * dx + nx * arc, from.1 + 0.28 * dy + ny * arc);
    let p2 = (
        from.0 + 0.72 * dx + nx * arc * 0.65,
        from.1 + 0.72 * dy + ny * arc * 0.65,
    );
    let p3 = to;

    let mut points = Vec::with_capacity(steps as usize);

    for i in 1..=steps {
        let u = i as f64 / steps as f64;
        // Cubic ease-in-out: smooth acceleration from rest and gentle deceleration to target
        let t = if u < 0.5 {
            4.0 * u * u * u
        } else {
            1.0 - (-2.0 * u + 2.0).powi(3) / 2.0
        };

        let omt = 1.0 - t;
        let omt2 = omt * omt;
        let omt3 = omt2 * omt;
        let t2 = t * t;
        let t3 = t2 * t;

        let x = omt3 * p0.0 + 3.0 * omt2 * t * p1.0 + 3.0 * omt * t2 * p2.0 + t3 * p3.0;
        let y = omt3 * p0.1 + 3.0 * omt2 * t * p1.1 + 3.0 * omt * t2 * p2.1 + t3 * p3.1;

        // Round to 1 decimal place
        points.push(((x * 10.0).round() / 10.0, (y * 10.0).round() / 10.0));
    }

    // Ensure the final point matches `to` exactly
    if let Some(last) = points.last_mut() {
        *last = to;
    }

    points
}

/// Stop a glide that a human or the caller has interrupted. `since` is the
/// takeover generation read when the call started.
fn check_interrupted(
    backend: &dyn InputBackend,
    cancel: &CancelToken,
    since: u64,
) -> Result<(), InputError> {
    if cancel.is_cancelled() {
        return Err(InputError::Failed(
            "glide aborted: the call was cancelled".into(),
        ));
    }
    if backend.takeover_generation() != since {
        return Err(InputError::Failed(
            "glide aborted: a human took over the pointer".into(),
        ));
    }
    Ok(())
}

/// Execute smooth cursor gliding to `to` if glide is enabled.
///
/// Returns how many waypoints were actually emitted: `0` when gliding is off,
/// the pointer position is unknown, or the move is too small to need one. The
/// caller reports "glided" from this, never from the configuration.
///
/// The glide is the one place an input call spends tens to hundreds of
/// milliseconds issuing pointer moves, which is exactly when a person grabs the
/// mouse. Both the call's cancel token and the backend's takeover generation
/// are therefore checked before every waypoint, and again after the last one.
///
/// `since` is `backend.takeover_generation()` read when the call *started*,
/// not here: a takeover between admission and the first waypoint must still
/// stop the glide, so nothing in this function clears or re-reads it.
pub async fn execute_glide(
    backend: &dyn InputBackend,
    to: (f64, f64),
    config: &GlideConfig,
    cancel: &CancelToken,
    since: u64,
) -> Result<u32, InputError> {
    if !config.enabled || matches!(config.preset, GlidePreset::Instant) {
        return Ok(0);
    }

    let Some(current) = backend.pointer_position().await? else {
        return Ok(0);
    };

    let dx = to.0 - current.0;
    let dy = to.1 - current.1;
    let dist = (dx * dx + dy * dy).sqrt();

    // No intermediate glide needed for micro-movements (< 3px).
    if dist < 3.0 {
        return Ok(0);
    }

    let steps = config.steps();
    let path = bezier_interpolate(current, to, steps, config.curvature());
    let total_duration = config.duration();
    let per_step_ms = (total_duration / path.len() as u64).clamp(4, 40);

    let mut emitted = 0u32;
    for pt in path {
        check_interrupted(backend, cancel, since)?;
        backend
            .mouse(MouseKind::Move, pt.0, pt.1, None, &[])
            .await?;
        emitted += 1;
        sleep(Duration::from_millis(per_step_ms)).await;
    }
    // Last look before the caller's real action (a click) lands.
    check_interrupted(backend, cancel, since)?;

    Ok(emitted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glide_preset_parsing() {
        assert_eq!(
            GlidePreset::from_speed("cinematic"),
            Some(GlidePreset::Cinematic)
        );
        assert_eq!(GlidePreset::from_speed("demo"), Some(GlidePreset::Demo));
        // Aliases once accepted here were never in a schema or in config
        // validation, so the same word meant different things in different
        // places. One vocabulary now.
        assert_eq!(GlidePreset::from_speed("presentation"), None);
        assert_eq!(
            GlidePreset::from_speed(" Cinematic "),
            Some(GlidePreset::Cinematic)
        );
        assert_eq!(GlidePreset::from_speed("snappy"), Some(GlidePreset::Snappy));
        assert_eq!(
            GlidePreset::from_speed("instant"),
            Some(GlidePreset::Instant)
        );
        assert_eq!(GlidePreset::from_speed("off"), Some(GlidePreset::Instant));
        assert_eq!(GlidePreset::from_speed("unknown"), None);
    }

    #[test]
    fn test_bezier_interpolation_endpoints() {
        let from = (100.0, 200.0);
        let to = (500.0, 600.0);
        let points = bezier_interpolate(from, to, 20, 0.08);

        assert_eq!(points.len(), 20);
        assert_eq!(points.last().copied(), Some(to));

        // First point should be close to starting point
        let first = points[0];
        assert!(first.0 > 100.0 && first.0 < 200.0);
        assert!(first.1 > 200.0 && first.1 < 300.0);
    }

    #[test]
    fn test_bezier_interpolation_short_distance() {
        let from = (100.0, 100.0);
        let to = (100.5, 100.5);
        let points = bezier_interpolate(from, to, 20, 0.08);
        assert_eq!(points, vec![to]);
    }

    #[test]
    fn test_glide_config_presets() {
        let cine = GlideConfig::cinematic();
        assert!(cine.enabled);
        assert_eq!(cine.duration(), 350);
        assert_eq!(cine.steps(), 24);

        let demo = GlideConfig::demo();
        assert!(demo.enabled);
        assert_eq!(demo.duration(), 200);
        assert_eq!(demo.steps(), 16);

        let instant = GlideConfig::instant();
        assert!(!instant.enabled);
        assert_eq!(instant.duration(), 0);
    }

    use crate::backend::{ClipData, ClipFormat, ScrollDir, SemanticAction};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    /// Records every pointer move and can raise a takeover flag or trip a
    /// cancel token after N of them. This exercises glide *pacing*; it makes no
    /// claim about what an OS does with the moves.
    struct Recorder {
        moves: Mutex<Vec<(f64, f64)>>,
        position: Option<(f64, f64)>,
        takeover_after: Option<usize>,
        cancel_after: Option<(usize, CancelToken)>,
        generation: AtomicU64,
    }

    impl Recorder {
        fn at(position: Option<(f64, f64)>) -> Self {
            Recorder {
                moves: Mutex::new(Vec::new()),
                position,
                takeover_after: None,
                cancel_after: None,
                generation: AtomicU64::new(0),
            }
        }
        fn count(&self) -> usize {
            self.moves.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl InputBackend for Recorder {
        async fn perform(
            &self,
            _: u64,
            _: SemanticAction,
            _: Option<&str>,
        ) -> Result<(), InputError> {
            Ok(())
        }
        async fn set_value(&self, _: u64, _: &str) -> Result<(), InputError> {
            Ok(())
        }
        async fn type_text(&self, _: &str) -> Result<(), InputError> {
            Ok(())
        }
        async fn key_combo(&self, _: &str) -> Result<(), InputError> {
            Ok(())
        }
        async fn mouse(
            &self,
            _: MouseKind,
            x: f64,
            y: f64,
            _: Option<&str>,
            _: &[String],
        ) -> Result<(), InputError> {
            let n = {
                let mut m = self.moves.lock().unwrap();
                m.push((x, y));
                m.len()
            };
            if self.takeover_after == Some(n) {
                self.cancel_pending();
            }
            if let Some((at, tok)) = &self.cancel_after {
                if *at == n {
                    tok.cancel();
                }
            }
            Ok(())
        }
        async fn scroll_at(
            &self,
            _: f64,
            _: f64,
            _: ScrollDir,
            _: i32,
            _: &[String],
        ) -> Result<(), InputError> {
            Ok(())
        }
        async fn hover(&self, _: f64, _: f64) -> Result<(), InputError> {
            Ok(())
        }
        async fn drag(
            &self,
            _: (f64, f64),
            _: (f64, f64),
            _: &[String],
            _: u32,
            _: u64,
            _: u64,
        ) -> Result<(), InputError> {
            Ok(())
        }
        async fn clipboard_read(&self, format: ClipFormat) -> Result<ClipData, InputError> {
            Ok(ClipData { format, data: None })
        }
        fn input_target(&self) -> Option<String> {
            None
        }
        async fn clipboard_write(&self, _: ClipFormat, _: &str) -> Result<(), InputError> {
            Ok(())
        }
        fn platform(&self) -> &'static str {
            "recorder"
        }
        async fn pointer_position(&self) -> Result<Option<(f64, f64)>, InputError> {
            Ok(self.position)
        }
        fn cancel_pending(&self) {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        fn takeover_generation(&self) -> u64 {
            self.generation.load(Ordering::SeqCst)
        }
    }

    const FAR: (f64, f64) = (400.0, 300.0);

    #[tokio::test]
    async fn instant_emits_no_glide_waypoints() {
        let b = Recorder::at(Some((0.0, 0.0)));
        let n = execute_glide(&b, FAR, &GlideConfig::instant(), &CancelToken::new(), 0)
            .await
            .unwrap();
        // The caller's own move is then the single move of an instant action.
        assert_eq!((n, b.count()), (0, 0));
    }

    #[tokio::test]
    async fn demo_emits_sixteen_waypoints_ending_on_target() {
        let b = Recorder::at(Some((0.0, 0.0)));
        let n = execute_glide(&b, FAR, &GlideConfig::demo(), &CancelToken::new(), 0)
            .await
            .unwrap();
        assert_eq!(n, 16);
        let moves = b.moves.lock().unwrap();
        assert_eq!(moves.len(), 16);
        assert_eq!(moves.last().copied(), Some(FAR));
    }

    #[tokio::test]
    async fn unknown_position_or_tiny_move_emits_nothing() {
        let none = Recorder::at(None);
        let n = execute_glide(&none, FAR, &GlideConfig::demo(), &CancelToken::new(), 0)
            .await
            .unwrap();
        assert_eq!((n, none.count()), (0, 0));

        let near = Recorder::at(Some((399.0, 300.0)));
        let n = execute_glide(&near, FAR, &GlideConfig::demo(), &CancelToken::new(), 0)
            .await
            .unwrap();
        assert_eq!((n, near.count()), (0, 0));
    }

    #[tokio::test]
    async fn takeover_mid_glide_stops_early() {
        let mut b = Recorder::at(Some((0.0, 0.0)));
        b.takeover_after = Some(5);
        let err = execute_glide(&b, FAR, &GlideConfig::demo(), &CancelToken::new(), 0)
            .await
            .unwrap_err();
        assert!(matches!(&err, InputError::Failed(m) if m.contains("human took over")));
        assert_eq!(b.count(), 5, "no waypoint may follow the takeover");
    }

    #[tokio::test]
    async fn cancel_token_mid_glide_stops_early() {
        let token = CancelToken::new();
        let mut b = Recorder::at(Some((0.0, 0.0)));
        b.cancel_after = Some((3, token.clone()));
        let err = execute_glide(&b, FAR, &GlideConfig::demo(), &token, 0)
            .await
            .unwrap_err();
        assert!(matches!(&err, InputError::Failed(m) if m.contains("cancelled")));
        assert_eq!(b.count(), 3);
    }

    #[tokio::test]
    async fn a_takeover_before_the_call_started_does_not_abort_it() {
        let b = Recorder::at(Some((0.0, 0.0)));
        b.cancel_pending();
        // The call reads the generation as it starts, after the old takeover.
        let since = b.takeover_generation();
        let n = execute_glide(&b, FAR, &GlideConfig::demo(), &CancelToken::new(), since)
            .await
            .unwrap();
        assert_eq!(n, 16);
    }

    #[tokio::test]
    async fn a_takeover_after_admission_but_before_the_first_waypoint_is_not_lost() {
        let b = Recorder::at(Some((0.0, 0.0)));
        // The call starts and reads the generation...
        let since = b.takeover_generation();
        // ...then the human grabs the mouse before the glide emits anything.
        b.cancel_pending();
        let err = execute_glide(&b, FAR, &GlideConfig::demo(), &CancelToken::new(), since)
            .await
            .unwrap_err();
        assert!(matches!(&err, InputError::Failed(m) if m.contains("human took over")));
        assert_eq!(b.count(), 0, "not one waypoint may follow the takeover");
    }
}
