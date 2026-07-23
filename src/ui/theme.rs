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
    drop_target: Hsla::from(rgb(0x0a0a0a)),
    drop_target_hover: Hsla::from(rgb(0x111111)),
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
