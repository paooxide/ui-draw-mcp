//! Tunables for screen capture.
//!
//! These were compiled-in constants. They are operator-facing numbers — they
//! trade image cost against legibility, and the right value depends on the
//! display, the model, and what the agent is being asked to read — so they
//! belong in `config.toml` rather than in a rebuild.
//!
//! Every value has a defensible default; an operator who sets none of them gets
//! exactly the previous behaviour.

use crate::backend::Detail;

/// Capture tunables, resolved once at startup.
#[derive(Debug, Clone, Copy)]
pub struct VisionConfig {
    /// Longest delivered edge for `detail: "low"` — layout and state checks.
    pub detail_low_px: u32,
    /// Longest delivered edge for `detail: "balanced"` — most interactions.
    pub detail_balanced_px: u32,
    /// Longest delivered edge for `detail: "full"` — small text stays legible.
    pub detail_full_px: u32,
    /// Detail used when a call names neither `detail` nor `max_edge`.
    pub default_detail: Detail,
    /// Mean-absolute-difference (0–255) below which two frames count as
    /// visually identical and the second is not re-sent.
    pub unchanged_mad: f64,
    /// Pixels per image token. Image cost tracks *pixel area*, not file size,
    /// so this is the divisor in the cost estimate.
    pub pixels_per_token: u32,
    /// Soft advisory cap; oversized captures are still returned but flagged.
    pub max_image_bytes: usize,
}

impl Default for VisionConfig {
    fn default() -> Self {
        VisionConfig {
            detail_low_px: 768,
            detail_balanced_px: 1024,
            detail_full_px: 1568,
            default_detail: Detail::Full,
            // Measured on this hardware: two *idle* captures 1.2s apart differ
            // by ~0.002, so this sits ~500x above the noise floor while still
            // catching any change large enough for an agent to act on.
            unchanged_mad: 1.0,
            // Anthropic-style estimate.
            pixels_per_token: 750,
            max_image_bytes: 8_000_000,
        }
    }
}

impl VisionConfig {
    /// Longest delivered edge for a detail tier.
    pub fn max_edge(&self, detail: Detail) -> u32 {
        match detail {
            Detail::Low => self.detail_low_px,
            Detail::Balanced => self.detail_balanced_px,
            Detail::Full => self.detail_full_px,
        }
    }

    /// Estimated image tokens for a `w * h` frame.
    pub fn image_tokens(&self, w: u32, h: u32) -> u64 {
        let divisor = self.pixels_per_token.max(1) as u64;
        (w as u64 * h as u64) / divisor
    }

    /// Reject a configuration that would make capture useless rather than
    /// letting it fail confusingly at the first call.
    pub fn validate(&self) -> Result<(), String> {
        for (name, px) in [
            ("detail_low_px", self.detail_low_px),
            ("detail_balanced_px", self.detail_balanced_px),
            ("detail_full_px", self.detail_full_px),
        ] {
            if !(160..=4096).contains(&px) {
                return Err(format!(
                    "vision.{name} must be between 160 and 4096, got {px}"
                ));
            }
        }
        if self.pixels_per_token == 0 {
            return Err("vision.pixels_per_token must be greater than zero".into());
        }
        if !self.unchanged_mad.is_finite() || self.unchanged_mad < 0.0 {
            return Err(format!(
                "vision.unchanged_mad must be a non-negative number, got {}",
                self.unchanged_mad
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults must reproduce the previously compiled-in behaviour, or
    /// moving them into config would be a silent behaviour change.
    #[test]
    fn defaults_match_the_former_constants() {
        let c = VisionConfig::default();
        assert_eq!(c.max_edge(Detail::Low), 768);
        assert_eq!(c.max_edge(Detail::Balanced), 1024);
        assert_eq!(c.max_edge(Detail::Full), 1568);
        assert_eq!(c.default_detail, Detail::Full);
        assert_eq!(c.unchanged_mad, 1.0);
        assert_eq!(c.image_tokens(3024, 1964), 7918);
        assert_eq!(c.image_tokens(1568, 1018), 2128);
        assert_eq!(c.image_tokens(768, 498), 509);
    }

    #[test]
    fn cost_scales_with_area_not_edge() {
        let c = VisionConfig::default();
        assert_eq!(c.image_tokens(1000, 1000) / 4, c.image_tokens(500, 500));
    }

    #[test]
    fn nonsense_configurations_are_rejected() {
        let bad = [
            VisionConfig {
                detail_full_px: 10,
                ..Default::default()
            },
            VisionConfig {
                detail_low_px: 99_999,
                ..Default::default()
            },
            VisionConfig {
                pixels_per_token: 0,
                ..Default::default()
            },
            VisionConfig {
                unchanged_mad: -1.0,
                ..Default::default()
            },
            VisionConfig {
                unchanged_mad: f64::NAN,
                ..Default::default()
            },
        ];
        for c in bad {
            assert!(c.validate().is_err(), "should reject: {c:?}");
        }
        assert!(VisionConfig::default().validate().is_ok());
    }

    /// A zero divisor would panic on divide; `image_tokens` clamps instead so a
    /// config that slipped past validation cannot take the process down.
    #[test]
    fn zero_divisor_does_not_panic() {
        let c = VisionConfig {
            pixels_per_token: 0,
            ..Default::default()
        };
        assert_eq!(c.image_tokens(10, 10), 100);
    }
}
