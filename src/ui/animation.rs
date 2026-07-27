use std::time::Duration;

/// Durations for the app's small motion budget. UI animations stay under 300ms;
/// the spinner breathes slower so it does not feel frantic.
pub const ENTER_DURATION: Duration = Duration::from_millis(200);
pub const PANEL_DURATION: Duration = Duration::from_millis(180);
pub const DIALOG_DURATION: Duration = Duration::from_millis(220);
pub const SPINNER_PERIOD: Duration = Duration::from_millis(1600);

// Onboarding is rare / first-time (Emil frequency table → delight is allowed).
// Keep under the modal budget; stagger must never feel slow.
/// Full card + scrim entrance.
pub const ONBOARDING_ENTER: Duration = Duration::from_millis(260);
/// Content swap between steps (forward / back).
pub const ONBOARDING_STEP: Duration = Duration::from_millis(200);
/// Per-item fade after stagger delay.
pub const ONBOARDING_ITEM_MS: u64 = 200;
/// Delay between staggered children (30–80ms range).
pub const ONBOARDING_STAGGER_MS: u64 = 45;
/// Subtle vertical travel for scale-adjacent physicality (GPUI has no div transform).
pub const ONBOARDING_SLIDE_PX: f32 = 10.0;

/// Fully transparent → opaque. Use only for staggered *content* that sits on a
/// already-visible shell — never for the modal scrim or card itself.
#[inline]
pub fn fade_opacity(progress: f32) -> f32 {
    progress.clamp(0.0, 1.0)
}

/// App-wide modal enter curve: never paint a dialog at opacity 0.
///
/// Matches shortcuts / folder-confirm / settings panels (`0.12 + 0.88 * t`).
/// Starting at 0 makes a oneshot animation look “missing” if the first frames
/// are dropped or the element state restarts mid-flight.
#[inline]
pub fn enter_opacity(progress: f32) -> f32 {
    0.12 + 0.88 * progress.clamp(0.0, 1.0)
}

/// Hold at 0 until `delay_ms / total_ms`, then ease-out-quint for the remainder.
///
/// Used to fake per-item stagger without a separate delay API: each child gets a
/// longer total duration and this easing so earlier time is spent invisible.
pub fn delayed_ease_out_quint(delay_ms: u64, total_ms: u64) -> impl Fn(f32) -> f32 {
    let delay_frac = if total_ms == 0 {
        0.0
    } else {
        (delay_ms as f32 / total_ms as f32).clamp(0.0, 0.95)
    };
    move |t| {
        let t = t.clamp(0.0, 1.0);
        if t <= delay_frac {
            0.0
        } else {
            let local = (t - delay_frac) / (1.0 - delay_frac);
            // ease-out-quint — strong curve, starts fast (Emil: never ease-in on UI)
            1.0 - (1.0 - local).powi(5)
        }
    }
}

/// Total duration for a staggered child at `index` (0-based).
pub fn onboarding_stagger_duration(index: usize) -> Duration {
    Duration::from_millis(ONBOARDING_ITEM_MS + index as u64 * ONBOARDING_STAGGER_MS)
}

/// Easing for a staggered child at `index` (0-based).
pub fn onboarding_stagger_easing(index: usize) -> impl Fn(f32) -> f32 {
    let delay = index as u64 * ONBOARDING_STAGGER_MS;
    let total = ONBOARDING_ITEM_MS + delay;
    delayed_ease_out_quint(delay, total)
}

/// Direction of onboarding step navigation for direction-aware content motion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OnboardingNavDirection {
    /// First paint or no prior step.
    #[default]
    Enter,
    Forward,
    Back,
}

impl OnboardingNavDirection {
    /// Signed slide offset at progress 0 (px). Forward rises from below; back from above.
    pub fn slide_start_px(self) -> f32 {
        match self {
            Self::Enter => ONBOARDING_SLIDE_PX,
            Self::Forward => ONBOARDING_SLIDE_PX,
            Self::Back => -ONBOARDING_SLIDE_PX,
        }
    }

    /// Short tag for ElementIds so forward/back restarts animation state.
    pub fn id_tag(self) -> &'static str {
        match self {
            Self::Enter => "e",
            Self::Forward => "f",
            Self::Back => "b",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delayed_ease_holds_then_ease_out() {
        let ease = delayed_ease_out_quint(50, 250);
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(0.2), 0.0); // still in delay (50/250)
        let mid = ease(0.6);
        assert!(mid > 0.0 && mid < 1.0, "mid={mid}");
        assert!((ease(1.0) - 1.0).abs() < 1e-5);
        // ease-out: first half of the active range moves more than the second half
        let a = ease(0.4); // just after delay
        let b = ease(0.7);
        let c = ease(1.0);
        assert!((b - a) > (c - b) * 0.5, "ease-out should front-load motion");
    }

    #[test]
    fn stagger_duration_grows_with_index() {
        assert_eq!(
            onboarding_stagger_duration(0),
            Duration::from_millis(ONBOARDING_ITEM_MS)
        );
        assert_eq!(
            onboarding_stagger_duration(2),
            Duration::from_millis(ONBOARDING_ITEM_MS + 2 * ONBOARDING_STAGGER_MS)
        );
    }

    #[test]
    fn nav_direction_slide_signs() {
        assert!(OnboardingNavDirection::Forward.slide_start_px() > 0.0);
        assert!(OnboardingNavDirection::Back.slide_start_px() < 0.0);
        assert_eq!(
            OnboardingNavDirection::Enter.slide_start_px(),
            OnboardingNavDirection::Forward.slide_start_px()
        );
    }

    #[test]
    fn fade_opacity_clamps() {
        assert_eq!(fade_opacity(-0.5), 0.0);
        assert_eq!(fade_opacity(0.5), 0.5);
        assert_eq!(fade_opacity(1.5), 1.0);
    }

    #[test]
    fn enter_opacity_never_fully_transparent() {
        assert!((enter_opacity(0.0) - 0.12).abs() < 1e-5);
        assert!((enter_opacity(1.0) - 1.0).abs() < 1e-5);
        assert!(enter_opacity(-1.0) >= 0.12);
        assert!(enter_opacity(2.0) <= 1.0);
    }
}
