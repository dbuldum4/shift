use gpui::{
    Animation, AnimationExt, App, Application, Bounds, Context, ExternalPaths, KeyBinding, Menu,
    MenuItem, PathBuilder, PathPromptOptions, PathStyle, Render, Rgba, StrokeOptions,
    SystemMenuType, TitlebarOptions, Window, WindowBounds, WindowOptions, actions, canvas, div,
    ease_out_quint, point, prelude::*, px, rgb, size,
};
use std::path::PathBuf;
use std::time::Duration;

const APP_NAME: &str = "Shift";
const DROP_ZONE_COLOR: u32 = 0x171a1f;
const DROP_ZONE_HOVER_COLOR: u32 = 0x1c2026;

fn interpolate_color(from: Rgba, to: Rgba, progress: f32) -> Rgba {
    Rgba {
        r: from.r + (to.r - from.r) * progress,
        g: from.g + (to.g - from.g) * progress,
        b: from.b + (to.b - from.b) * progress,
        a: from.a + (to.a - from.a) * progress,
    }
}

fn rounded_dashed_border() -> impl IntoElement {
    canvas(
        |_, _, _| {},
        |bounds, _, window, _| {
            // Leave 4px of visible clearance beyond the 4px stroke.
            let inset = px(6.0);
            // `rounded_xl` is a 12px outer radius, so the inset path needs a
            // matching 6px centerline radius to remain concentric.
            let radius = px(6.0);
            let left = bounds.left() + inset;
            let right = bounds.right() - inset;
            let top = bounds.top() + inset;
            let bottom = bounds.bottom() - inset;

            let options = StrokeOptions::default()
                .with_line_width(4.0)
                .with_line_cap(lyon::path::LineCap::Round);
            let mut path = PathBuilder::stroke(px(4.0))
                .with_style(PathStyle::Stroke(options))
                .dash_array(&[px(8.0), px(8.0)]);

            path.move_to(point(left + radius, top));
            path.line_to(point(right - radius, top));
            path.arc_to(
                point(radius, radius),
                px(0.0),
                false,
                true,
                point(right, top + radius),
            );
            path.line_to(point(right, bottom - radius));
            path.arc_to(
                point(radius, radius),
                px(0.0),
                false,
                true,
                point(right - radius, bottom),
            );
            path.line_to(point(left + radius, bottom));
            path.arc_to(
                point(radius, radius),
                px(0.0),
                false,
                true,
                point(left, bottom - radius),
            );
            path.line_to(point(left, top + radius));
            path.arc_to(
                point(radius, radius),
                px(0.0),
                false,
                true,
                point(left + radius, top),
            );
            path.close();

            if let Ok(path) = path.build() {
                window.paint_path(path, rgb(0x343941));
            }
        },
    )
    .absolute()
    .size_full()
}

actions!(shift, [Quit]);

struct Shift {
    selected_file: Option<PathBuf>,
    drop_zone_hovered: bool,
}

impl Shift {
    fn choose_file(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose file".into()),
        });

        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| {
                    this.selected_file = Some(path);
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

impl Render for Shift {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_name = self.selected_file.as_ref().and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        });

        div()
            .flex()
            .size_full()
            .bg(rgb(0x101216))
            .text_color(rgb(0xf5f7fa))
            .child(div().flex_1().h_full().p_8().child({
                let drop_zone_hovered = self.drop_zone_hovered;

                div()
                    .id("file-drop-zone")
                    .flex()
                    .flex_col()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .rounded_xl()
                    .bg(rgb(DROP_ZONE_COLOR))
                    .cursor_pointer()
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if this.drop_zone_hovered != *hovered {
                            this.drop_zone_hovered = *hovered;
                            cx.notify();
                        }
                    }))
                    .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(rgb(0x1c2722)))
                    .child(rounded_dashed_border())
                    .child(div().text_3xl().text_color(rgb(0x8faa98)).child("\u{2191}"))
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("Drop a file here"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x9299a6))
                            .child("or click to browse"),
                    )
                    .when_some(selected_name, |element, name| {
                        element.child(
                            div()
                                .mt_3()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(rgb(0x23272e))
                                .text_sm()
                                .child(name),
                        )
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx)))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                        if let Some(path) = paths.paths().first() {
                            this.selected_file = Some(path.clone());
                            cx.notify();
                        }
                    }))
                    .with_animation(
                        if drop_zone_hovered {
                            "file-drop-zone-hover-enter"
                        } else {
                            "file-drop-zone-hover-leave"
                        },
                        Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
                        move |element, progress| {
                            let (from, to) = if drop_zone_hovered {
                                (rgb(DROP_ZONE_COLOR), rgb(DROP_ZONE_HOVER_COLOR))
                            } else {
                                (rgb(DROP_ZONE_HOVER_COLOR), rgb(DROP_ZONE_COLOR))
                            };
                            element.bg(interpolate_color(from, to, progress))
                        },
                    )
            }))
            .child(
                div()
                    .h_full()
                    .py_8()
                    .child(div().w(px(1.0)).h_full().bg(rgb(0x292d34))),
            )
            .child(div().flex_1().h_full().bg(rgb(0x101216)))
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.set_menus(vec![Menu {
            name: APP_NAME.into(),
            items: vec![
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action(format!("Quit {APP_NAME}"), Quit),
            ],
        }]);

        let bounds = Bounds::centered(None, size(px(900.0), px(600.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    ..Default::default()
                }),
                app_id: Some(APP_NAME.into()),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|_| Shift {
                    selected_file: None,
                    drop_zone_hovered: false,
                })
            },
        )
        .expect("failed to open the main window");

        cx.activate(true);
    });
}
