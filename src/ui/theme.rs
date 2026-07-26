use gpui::{BoxShadow, Hsla, hsla, point, px, rgb};
use std::sync::LazyLock;

/// A Zed-style semantic color and elevation token set for Shift.
///
/// Colors are stored as [`Hsla`] so they work with GPUI's `text_color`,
/// `border_color`, and `bg` builders without extra conversion at call sites.
#[derive(Clone, Debug)]
pub struct Theme {
    pub background: Hsla,
    pub raised: Hsla,
    pub surface: Hsla,
    pub elevated: Hsla,
    pub hover: Hsla,
    pub active: Hsla,
    pub drop_target: Hsla,
    pub drop_target_hover: Hsla,
    pub border: Hsla,
    pub border_strong: Hsla,
    pub border_focused: Hsla,
    pub border_light: Hsla,
    pub text: Hsla,
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_muted: Hsla,
    pub text_dim: Hsla,
    pub text_inverse: Hsla,
    pub scrim: Hsla,
    pub shadow: Hsla,
    pub shadow_key: Hsla,
    pub badge_fill: Hsla,
    pub badge_text: Hsla,
    pub status_ready_fill: Hsla,
    pub status_ready_text: Hsla,
    pub status_ready_border: Hsla,
    pub status_missing_fill: Hsla,
    pub status_missing_text: Hsla,
    pub status_missing_border: Hsla,
    pub active_opacity: f32,
}

pub static THEME: LazyLock<Theme> = LazyLock::new(|| Theme {
    background: Hsla::from(rgb(0x0a0a0a)),
    raised: Hsla::from(rgb(0x0a0a0a)),
    surface: Hsla::from(rgb(0x111111)),
    elevated: Hsla::from(rgb(0x1a1a1a)),
    hover: Hsla::from(rgb(0x222222)),
    active: Hsla::from(rgb(0x2a2a2a)),
    drop_target: Hsla::from(rgb(0x111111)),
    drop_target_hover: Hsla::from(rgb(0x161616)),
    border: Hsla::from(rgb(0x222222)),
    border_strong: Hsla::from(rgb(0x333333)),
    border_focused: Hsla::from(rgb(0x555555)),
    border_light: hsla(0.0, 0.0, 1.0, 0.06),
    text: Hsla::from(rgb(0xffffff)),
    text_primary: Hsla::from(rgb(0xe8e8e8)),
    text_secondary: Hsla::from(rgb(0x888888)),
    text_muted: Hsla::from(rgb(0x666666)),
    text_dim: Hsla::from(rgb(0x444444)),
    text_inverse: Hsla::from(rgb(0x000000)),
    scrim: hsla(0.0, 0.0, 0.0, 0.72),
    shadow: hsla(0.0, 0.0, 0.0, 0.65),
    shadow_key: hsla(0.0, 0.0, 0.0, 0.25),
    badge_fill: Hsla::from(rgb(0x1a1a1a)),
    badge_text: Hsla::from(rgb(0xcccccc)),
    status_ready_fill: Hsla::from(rgb(0x1a1a1a)),
    status_ready_text: Hsla::from(rgb(0xe8e8e8)),
    status_ready_border: Hsla::from(rgb(0x555555)),
    status_missing_fill: Hsla::from(rgb(0x111111)),
    status_missing_text: Hsla::from(rgb(0x888888)),
    status_missing_border: Hsla::from(rgb(0x333333)),
    active_opacity: 0.88,
});

/// A layered shadow for raised cards and dialogs: a tight key shadow plus a soft
/// ambient wash. Matches Zed's elevation stack and Apple's material depth.
pub fn card_shadow() -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: THEME.shadow_key,
            blur_radius: px(4.0),
            spread_radius: px(0.0),
            offset: point(px(0.0), px(1.0)),
        },
        BoxShadow {
            color: THEME.shadow,
            blur_radius: px(24.0),
            spread_radius: px(0.0),
            offset: point(px(0.0), px(8.0)),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme_colors(theme: &Theme) -> Vec<(&'static str, Hsla)> {
        vec![
            ("background", theme.background),
            ("raised", theme.raised),
            ("surface", theme.surface),
            ("elevated", theme.elevated),
            ("hover", theme.hover),
            ("active", theme.active),
            ("drop_target", theme.drop_target),
            ("drop_target_hover", theme.drop_target_hover),
            ("border", theme.border),
            ("border_strong", theme.border_strong),
            ("border_focused", theme.border_focused),
            ("border_light", theme.border_light),
            ("text", theme.text),
            ("text_primary", theme.text_primary),
            ("text_secondary", theme.text_secondary),
            ("text_muted", theme.text_muted),
            ("text_dim", theme.text_dim),
            ("text_inverse", theme.text_inverse),
            ("scrim", theme.scrim),
            ("shadow", theme.shadow),
            ("shadow_key", theme.shadow_key),
            ("badge_fill", theme.badge_fill),
            ("badge_text", theme.badge_text),
            ("status_ready_fill", theme.status_ready_fill),
            ("status_ready_text", theme.status_ready_text),
            ("status_ready_border", theme.status_ready_border),
            ("status_missing_fill", theme.status_missing_fill),
            ("status_missing_text", theme.status_missing_text),
            ("status_missing_border", theme.status_missing_border),
        ]
    }

    fn assert_finite_hsla(name: &str, color: Hsla) {
        assert!(
            color.h.is_finite()
                && color.s.is_finite()
                && color.l.is_finite()
                && color.a.is_finite(),
            "{name} must have finite HSLA components, got {color:?}"
        );
        assert!(
            (0.0..=1.0).contains(&color.h)
                && (0.0..=1.0).contains(&color.s)
                && (0.0..=1.0).contains(&color.l)
                && (0.0..=1.0).contains(&color.a),
            "{name} components must be in [0,1], got h={} s={} l={} a={}",
            color.h,
            color.s,
            color.l,
            color.a
        );
    }

    #[test]
    fn theme_tokens_are_finite_hsla() {
        for (name, color) in theme_colors(&THEME) {
            assert_finite_hsla(name, color);
        }
    }

    #[test]
    fn active_opacity_is_in_open_unit_interval() {
        let opacity = THEME.active_opacity;
        assert!(
            opacity.is_finite() && opacity > 0.0 && opacity <= 1.0,
            "active_opacity must be in (0,1], got {opacity}"
        );
        assert!(
            (opacity - 0.88).abs() < f32::EPSILON,
            "expected historical active_opacity 0.88, got {opacity}"
        );
    }

    #[test]
    fn card_shadow_has_key_and_ambient_layers() {
        let shadows = card_shadow();
        assert_eq!(shadows.len(), 2, "card_shadow should layer key + ambient");

        let key = &shadows[0];
        assert_eq!(key.color, THEME.shadow_key);
        assert_eq!(key.blur_radius, px(4.0));
        assert_eq!(key.spread_radius, px(0.0));
        assert_eq!(key.offset, point(px(0.0), px(1.0)));

        let ambient = &shadows[1];
        assert_eq!(ambient.color, THEME.shadow);
        assert_eq!(ambient.blur_radius, px(24.0));
        assert_eq!(ambient.spread_radius, px(0.0));
        assert_eq!(ambient.offset, point(px(0.0), px(8.0)));
    }

    #[test]
    fn card_shadow_is_deterministic() {
        let a = card_shadow();
        let b = card_shadow();
        assert_eq!(a.len(), b.len());
        for (left, right) in a.iter().zip(b.iter()) {
            assert_eq!(left.color, right.color);
            assert_eq!(left.blur_radius, right.blur_radius);
            assert_eq!(left.spread_radius, right.spread_radius);
            assert_eq!(left.offset, right.offset);
        }
    }

    #[test]
    fn elevation_stack_increases_lightness() {
        // Dark UI: raised surfaces step up in lightness as elevation increases.
        assert!(THEME.background.l <= THEME.surface.l);
        assert!(THEME.surface.l <= THEME.elevated.l);
        assert!(THEME.elevated.l <= THEME.hover.l);
        assert!(THEME.hover.l <= THEME.active.l);
    }

    #[test]
    fn border_tokens_step_up_in_strength() {
        assert!(THEME.border.l < THEME.border_strong.l);
        assert!(THEME.border_strong.l < THEME.border_focused.l);
        // border_light is a translucent white wash, not a gray step.
        assert!(THEME.border_light.a < 1.0);
        assert!(THEME.border_light.l > 0.5);
    }

    #[test]
    fn text_hierarchy_steps_down_in_lightness() {
        assert!(THEME.text.l >= THEME.text_primary.l);
        assert!(THEME.text_primary.l > THEME.text_secondary.l);
        assert!(THEME.text_secondary.l > THEME.text_muted.l);
        assert!(THEME.text_muted.l > THEME.text_dim.l);
        assert!(THEME.text_inverse.l < THEME.text_dim.l);
    }

    #[test]
    fn scrim_and_shadows_are_dark_translucent() {
        for (name, color) in [
            ("scrim", THEME.scrim),
            ("shadow", THEME.shadow),
            ("shadow_key", THEME.shadow_key),
        ] {
            assert!(color.l < 0.1, "{name} should be near-black, l={}", color.l);
            assert!(
                color.a > 0.0 && color.a < 1.0,
                "{name} should be translucent, a={}",
                color.a
            );
        }
        // Ambient shadow is stronger than the key shadow.
        assert!(THEME.shadow.a > THEME.shadow_key.a);
    }

    #[test]
    fn status_ready_is_brighter_than_missing() {
        assert!(THEME.status_ready_text.l > THEME.status_missing_text.l);
        assert!(THEME.status_ready_border.l > THEME.status_missing_border.l);
        assert!(THEME.status_ready_fill.l >= THEME.status_missing_fill.l);
    }

    #[test]
    fn intentional_aliases_and_distinct_roles() {
        // Historical: background and raised share the same base canvas color.
        assert_eq!(THEME.background, THEME.raised);
        assert_eq!(THEME.surface, THEME.drop_target);
        assert_eq!(THEME.hover, THEME.border);
        assert_eq!(THEME.elevated, THEME.badge_fill);
        assert_eq!(THEME.elevated, THEME.status_ready_fill);
        assert_eq!(THEME.text_primary, THEME.status_ready_text);
        assert_eq!(THEME.text_secondary, THEME.status_missing_text);
        assert_eq!(THEME.border_strong, THEME.status_missing_border);
        assert_eq!(THEME.border_focused, THEME.status_ready_border);

        // Roles that must stay distinct for contrast / hierarchy.
        assert_ne!(THEME.background, THEME.surface);
        assert_ne!(THEME.surface, THEME.elevated);
        assert_ne!(THEME.text, THEME.text_inverse);
        assert_ne!(THEME.text_primary, THEME.text_muted);
        assert_ne!(THEME.badge_fill, THEME.badge_text);
        assert_ne!(THEME.drop_target, THEME.drop_target_hover);
        assert_ne!(THEME.status_ready_fill, THEME.status_missing_fill);
    }

    #[test]
    fn theme_clone_and_debug_are_usable() {
        let cloned = THEME.clone();
        assert_eq!(cloned.background, THEME.background);
        assert_eq!(cloned.active_opacity, THEME.active_opacity);
        assert_eq!(cloned.text, THEME.text);
        assert_eq!(cloned.surface, THEME.surface);
        assert_eq!(cloned.scrim, THEME.scrim);

        // Debug THEME via LazyLock may wrap as LazyLock(...); clone is bare Theme.
        let debug_clone = format!("{cloned:?}");
        assert!(
            debug_clone.contains("Theme") && debug_clone.contains("background"),
            "Debug output should mention Theme fields, got: {debug_clone}"
        );
        let debug_static = format!("{:?}", *THEME);
        assert_eq!(debug_static, debug_clone);
    }

    #[test]
    fn theme_static_is_lazy_singleton() {
        let a: *const Theme = &*THEME;
        let b: *const Theme = &*THEME;
        assert_eq!(a, b, "THEME should resolve to a single LazyLock instance");
    }

    #[test]
    fn default_hsla_is_not_used_for_primary_surfaces() {
        let default = Hsla::default();
        // Primary surfaces should be intentional dark grays, not Default::default.
        for (name, color) in [
            ("background", THEME.background),
            ("surface", THEME.surface),
            ("elevated", THEME.elevated),
            ("text", THEME.text),
            ("border", THEME.border),
        ] {
            assert_ne!(
                color, default,
                "{name} should not be Hsla::default() ({default:?})"
            );
        }
    }

    #[test]
    fn opaque_surfaces_have_full_alpha() {
        for (name, color) in [
            ("background", THEME.background),
            ("raised", THEME.raised),
            ("surface", THEME.surface),
            ("elevated", THEME.elevated),
            ("hover", THEME.hover),
            ("active", THEME.active),
            ("border", THEME.border),
            ("border_strong", THEME.border_strong),
            ("border_focused", THEME.border_focused),
            ("text", THEME.text),
            ("text_primary", THEME.text_primary),
            ("text_secondary", THEME.text_secondary),
            ("text_muted", THEME.text_muted),
            ("text_dim", THEME.text_dim),
            ("text_inverse", THEME.text_inverse),
            ("badge_fill", THEME.badge_fill),
            ("badge_text", THEME.badge_text),
        ] {
            assert!(
                (color.a - 1.0).abs() < f32::EPSILON,
                "{name} should be opaque (a=1), got a={}",
                color.a
            );
        }
    }

    #[test]
    fn theme_color_count_matches_struct_fields() {
        // Guard against adding a Theme color field without updating theme_colors().
        let colors = theme_colors(&THEME);
        assert_eq!(
            colors.len(),
            29,
            "update theme_colors() when Theme gains/loses color fields"
        );
        let mut names: Vec<&str> = colors.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 29, "theme color names must be unique");
    }
}
