mod file_picker;
mod text_input;

use gpui::{
    Animation, AnimationExt, App, Application, Bounds, Context, Entity, ExternalPaths, FontWeight,
    KeyBinding, Menu, MenuItem, PathBuilder, PathStyle, Pixels, Point, Render, SharedString,
    StrokeOptions, SystemMenuType, TitlebarOptions, Window, WindowBounds, WindowOptions, actions,
    canvas, div, ease_out_quint, hsla, point, prelude::*, px, rgb, size,
};
use shift_core::conversion::{
    ConversionArtifact, ConversionRegistry, OutputFormat, looks_like_url,
};
use shift_core::preferences::{load_module_priority, save_module_priority};
use std::path::{Path, PathBuf};
use std::time::Duration;
use text_input::TextInput;

const APP_NAME: &str = "Shift";
const DROP_ZONE_COLOR: u32 = 0x171a1f;
const DROP_ZONE_HOVER_COLOR: u32 = 0x191d23;

#[derive(Clone)]
struct FilePreview {
    name: SharedString,
    subtitle: SharedString,
    extension_label: SharedString,
    badge_color: u32,
    badge_text_color: u32,
}

#[derive(Clone)]
enum ConversionState {
    Empty,
    Converting,
    Ready(ConversionArtifact),
    Failed(SharedString),
}

#[derive(Clone)]
struct ModuleDrag {
    index: usize,
    label: SharedString,
    position: Point<Pixels>,
}

impl ModuleDrag {
    fn new(index: usize, label: impl Into<SharedString>) -> Self {
        Self {
            index,
            label: label.into(),
            position: Point::default(),
        }
    }

    fn position(mut self, position: Point<Pixels>) -> Self {
        self.position = position;
        self
    }
}

impl Render for ModuleDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .left(self.position.x - px(90.0))
            .top(self.position.y - px(22.0))
            .w(px(180.0))
            .px_4()
            .py_3()
            .rounded_lg()
            .bg(rgb(0x2a3038))
            .border_1()
            .border_color(rgb(0x46515e))
            .shadow_lg()
            .text_sm()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(0xf0f3f7))
            .child(self.label.clone())
    }
}

fn format_file_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let size = bytes as f64;
    if size < KB {
        format!("{bytes} B")
    } else if size < MB {
        format!("{:.1} KB", size / KB)
    } else if size < GB {
        format!("{:.1} MB", size / MB)
    } else {
        format!("{:.2} GB", size / GB)
    }
}

fn extension_badge(path: &Path) -> (String, u32, u32) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_uppercase())
        .unwrap_or_default();

    // Soft badge fills + readable accent text, keyed by common kinds.
    let (label, fill, text): (&str, u32, u32) = match ext.as_str() {
        "PNG" | "JPG" | "JPEG" | "GIF" | "WEBP" | "HEIC" | "SVG" | "BMP" | "TIFF" => {
            ("IMG", 0x24352c, 0x8fd9a8)
        }
        "MP4" | "MOV" | "MKV" | "AVI" | "WEBM" => ("VID", 0x35242f, 0xe0a0c0),
        "MP3" | "WAV" | "AAC" | "FLAC" | "M4A" | "OGG" => ("AUD", 0x2a2840, 0xb8b0f0),
        "PDF" => ("PDF", 0x3a2424, 0xf0a0a0),
        "ZIP" | "TAR" | "GZ" | "TGZ" | "7Z" | "RAR" => ("ZIP", 0x3a3020, 0xe8c888),
        "RS" | "TS" | "TSX" | "JS" | "JSX" | "PY" | "GO" | "SWIFT" | "KT" | "JAVA" | "C"
        | "CPP" | "H" | "CS" | "RB" | "PHP" => (ext.as_str(), 0x1e2e3a, 0x88c4e8),
        "MD" | "TXT" | "RTF" | "DOC" | "DOCX" | "PAGES" => (ext.as_str(), 0x243038, 0x9ec4d8),
        "JSON" | "YAML" | "YML" | "TOML" | "XML" | "CSV" => (ext.as_str(), 0x302a20, 0xe0c090),
        "" => ("FILE", 0x2a3038, 0xa8b4c4),
        other if other.len() <= 4 => (other, 0x2a3038, 0xa8b4c4),
        _ => ("FILE", 0x2a3038, 0xa8b4c4),
    };

    (label.to_string(), fill, text)
}

fn build_file_preview_with_size(path: &Path, size_label: String) -> FilePreview {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());

    let location = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Disk".into());

    let (extension_label, badge_color, badge_text_color) = extension_badge(path);

    FilePreview {
        name: name.into(),
        subtitle: format!("{size_label}  ·  {location}").into(),
        extension_label: extension_label.into(),
        badge_color,
        badge_text_color,
    }
}

fn build_file_preview(path: &Path) -> FilePreview {
    let size_label = std::fs::metadata(path)
        .map(|m| format_file_size(m.len()))
        .unwrap_or_else(|_| "—".into());

    build_file_preview_with_size(path, size_label)
}

fn rounded_dashed_border(accent: bool) -> impl IntoElement {
    canvas(
        move |_, _, _| {},
        move |bounds, _, window, _| {
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
                let color = if accent { rgb(0x3d5566) } else { rgb(0x343941) };
                window.paint_path(path, color);
            }
        },
    )
    .absolute()
    .size_full()
}

fn empty_drop_prompt() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .child(div().text_3xl().text_color(rgb(0x8fa3b8)).child("\u{2191}"))
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Drop a file here"),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(0x9299a6))
                .child("or click to browse"),
        )
}

fn build_url_preview(url: &str) -> FilePreview {
    let host = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("HTTPS://")
        .trim_start_matches("HTTP://")
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("Web")
        .to_owned();

    FilePreview {
        name: url.trim().to_owned().into(),
        subtitle: format!("URL  ·  {host}").into(),
        extension_label: "WEB".into(),
        badge_color: 0x243038,
        badge_text_color: 0x9ec4d8,
    }
}

fn file_preview_card(preview: FilePreview, cx: &mut Context<Shift>) -> impl IntoElement {
    let badge_color = preview.badge_color;
    let badge_text_color = preview.badge_text_color;

    div()
        .id("file-preview")
        .flex()
        .flex_col()
        .items_center()
        .gap_4()
        .w_full()
        .max_w(px(320.0))
        .child(
            div()
                .relative()
                .flex()
                .w_full()
                .items_center()
                .gap_3()
                .px_4()
                .py_3()
                .rounded_xl()
                .bg(rgb(0x1c2028))
                .border_1()
                .border_color(rgb(0x2e343e))
                .shadow(vec![gpui::BoxShadow {
                    color: hsla(220.0 / 360.0, 0.35, 0.04, 0.55),
                    blur_radius: px(24.0),
                    spread_radius: px(0.0),
                    offset: point(px(0.0), px(8.0)),
                }])
                // File-type badge
                .child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .size(px(48.0))
                        .rounded_lg()
                        .bg(rgb(badge_color))
                        .border_1()
                        .border_color(hsla(0.0, 0.0, 1.0, 0.06))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(badge_text_color))
                                .child(preview.extension_label),
                        ),
                )
                // Name + meta
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0xf0f3f7))
                                .truncate()
                                .child(preview.name),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x8b929e))
                                .truncate()
                                .child(preview.subtitle),
                        ),
                )
                // Clear (unpick) — does not delete the file on disk.
                .child(
                    div()
                        .id("clear-selected-file")
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .size(px(28.0))
                        .rounded_full()
                        .bg(rgb(0x272c34))
                        .border_1()
                        .border_color(rgb(0x353b45))
                        .text_color(rgb(0x9aa1ad))
                        .cursor_pointer()
                        .hover(|style| {
                            style
                                .bg(rgb(0x3a2428))
                                .border_color(rgb(0x6a3a42))
                                .text_color(rgb(0xf0b0b8))
                        })
                        .active(|style| style.opacity(0.85))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child("\u{2715}"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.clear_selected_file(cx);
                            cx.stop_propagation();
                        })),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(0x6e7582))
                .child("Click to replace  ·  Drop another file"),
        )
        .with_animation(
            "file-preview-in",
            Animation::new(Duration::from_millis(220)).with_easing(ease_out_quint()),
            |element, progress| element.opacity(0.35 + 0.65 * progress),
        )
}

fn markdown_excerpt(artifact: &ConversionArtifact) -> SharedString {
    let text = artifact
        .text()
        .unwrap_or("Markdown output is not valid UTF-8.");
    let mut excerpt: String = text.chars().take(1_200).collect();
    if text.chars().count() > 1_200 {
        excerpt.push_str("\n\n…");
    }
    if excerpt.trim().is_empty() {
        excerpt.push_str("The conversion completed with an empty document.");
    }
    excerpt.into()
}

fn output_panel(
    state: ConversionState,
    save_status: Option<SharedString>,
    output_format: OutputFormat,
    output_menu_open: bool,
    available_outputs: Vec<OutputFormat>,
    cx: &mut Context<Shift>,
) -> impl IntoElement {
    let content = match state {
        ConversionState::Empty => div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .h_full()
            .text_center()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(0xc6ccd5))
                    .child("Markdown appears here"),
            )
            .child(
                div()
                    .max_w(px(260.0))
                    .text_sm()
                    .text_color(rgb(0x767e8b))
                    .child(
                        "Choose a supported file or paste a URL — Shift converts it automatically.",
                    ),
            ),
        ConversionState::Converting => div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .h_full()
            .child(
                div()
                    .text_2xl()
                    .text_color(rgb(0x8fb3cc))
                    .child("↻")
                    .with_animation(
                        "conversion-pulse",
                        Animation::new(Duration::from_millis(900)).repeat(),
                        |element, progress| element.opacity(0.35 + progress * 0.65),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(0xb8c0cb))
                    .child(format!("Converting to {}…", output_format.label())),
            ),
        ConversionState::Failed(message) => div()
            .flex()
            .flex_col()
            .justify_center()
            .gap_3()
            .h_full()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(0xf0b0b8))
                    .child("Conversion failed"),
            )
            .child(
                div()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(0x251a1d))
                    .border_1()
                    .border_color(rgb(0x4a2930))
                    .text_sm()
                    .text_color(rgb(0xd4a0a7))
                    .child(message),
            ),
        ConversionState::Ready(artifact) => {
            let file_name: SharedString = artifact.file_name.clone().into();
            let size = format_file_size(artifact.bytes.len() as u64);
            let excerpt = markdown_excerpt(&artifact);
            let conversion_detail = format!(
                "{}  ·  {size}  ·  via {}",
                artifact.format.label(),
                module_label(artifact.module_id)
            );
            div()
                .flex()
                .flex_col()
                .gap_4()
                .h_full()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_lg()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(file_name),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x858d99))
                                        .child(conversion_detail),
                                ),
                        )
                        .child(
                            div()
                                .id("save-conversion")
                                .px_4()
                                .py_2()
                                .rounded_lg()
                                .bg(rgb(0x2a3038))
                                .border_1()
                                .border_color(rgb(0x46515e))
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0xe8edf3))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(0x343b45)).border_color(rgb(0x5a6674)))
                                .active(|style| style.opacity(0.82))
                                .child("Download")
                                .on_click(cx.listener(|this, _, _, cx| this.save_output(cx))),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .p_5()
                        .rounded_xl()
                        .bg(rgb(0x15181d))
                        .border_1()
                        .border_color(rgb(0x292e36))
                        .overflow_hidden()
                        .text_sm()
                        .text_color(rgb(0xb9c0ca))
                        .child(excerpt),
                )
                .when_some(save_status, |panel, status| {
                    panel.child(div().text_xs().text_color(rgb(0x8fc9a5)).child(status))
                })
        }
    };

    let selector = div()
        .id("output-format-selector")
        .relative()
        .child(
            div()
                .id("output-format-trigger")
                .flex()
                .items_center()
                .gap_2()
                .h(px(40.0))
                .px_3()
                .rounded_lg()
                .bg(rgb(0x1b1f25))
                .border_1()
                .border_color(rgb(0x303640))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x22272e)))
                .child(div().text_xs().text_color(rgb(0x7e8794)).child("Output"))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(output_format.label()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(18.0))
                        .text_xs()
                        .text_color(rgb(0x8e98a5))
                        .child("▾"),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.output_menu_open = !this.output_menu_open;
                    cx.notify();
                    cx.stop_propagation();
                })),
        )
        .when(output_menu_open, |selector| {
            selector.child(
                div()
                    .id("output-format-menu")
                    .absolute()
                    .top(px(42.0))
                    .right_0()
                    .w(px(190.0))
                    .max_h(px(420.0))
                    .overflow_y_scroll()
                    .p_1()
                    .rounded_lg()
                    .bg(rgb(0x20242b))
                    .border_1()
                    .border_color(rgb(0x353c46))
                    .shadow_lg()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .children(OutputFormat::ALL.iter().copied().enumerate().map(
                        |(index, format)| {
                            let enabled = available_outputs.contains(&format);
                            div()
                                .id(("output-format", index))
                                .flex()
                                .items_center()
                                .justify_between()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .text_sm()
                                .text_color(if enabled {
                                    rgb(0xe4e8ed)
                                } else {
                                    rgb(0x626a75)
                                })
                                .when(enabled, |row| {
                                    row.cursor_pointer()
                                        .hover(|style| style.bg(rgb(0x2b3139)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.set_output_format(format, cx);
                                            cx.stop_propagation();
                                        }))
                                })
                                .child(format.label())
                                .when(format == output_format, |row| {
                                    row.child(div().text_color(rgb(0x91c1df)).child("✓"))
                                })
                        },
                    )),
            )
        });

    div()
        .relative()
        .size_full()
        .p_8()
        .pt(px(78.0))
        .child(content)
        .child(
            div()
                .absolute()
                .top(px(28.0))
                .right(px(72.0))
                .child(selector),
        )
}

fn module_label(id: &str) -> &str {
    match id {
        "markitdown" => "MarkItDown",
        "pandoc" => "Pandoc",
        "defuddle" => "Defuddle",
        "docling" => "Docling",
        other => other,
    }
}

fn url_input_bar(url_input: Entity<TextInput>, cx: &mut Context<Shift>) -> impl IntoElement {
    div()
        .id("url-input-bar")
        .flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .rounded_lg()
                .bg(rgb(0x1b1f25))
                .border_1()
                .border_color(rgb(0x303640))
                .text_sm()
                .text_color(rgb(0xe8edf3))
                .overflow_hidden()
                .child(url_input),
        )
        .child(
            div()
                .id("convert-url")
                .flex()
                .items_center()
                .justify_center()
                .h(px(40.0))
                .px_4()
                .rounded_lg()
                .bg(rgb(0x2a3038))
                .border_1()
                .border_color(rgb(0x46515e))
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(0xe8edf3))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x343b45)).border_color(rgb(0x5a6674)))
                .active(|style| style.opacity(0.82))
                .child("Convert")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.submit_url_from_input(cx);
                    cx.stop_propagation();
                })),
        )
}

fn settings_modal(priority: &[String], cx: &mut Context<Shift>) -> impl IntoElement {
    div()
        .id("settings-backdrop")
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .left_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(hsla(220.0 / 360.0, 0.25, 0.04, 0.78))
        .cursor_default()
        .on_click(cx.listener(|this, _, _, cx| {
            this.settings_open = false;
            cx.notify();
            cx.stop_propagation();
        }))
        .child(
            div()
                .id("settings-modal")
                .relative()
                .flex()
                .flex_col()
                .gap_5()
                .w(px(440.0))
                .p_6()
                .rounded_xl()
                .bg(rgb(0x181c22))
                .border_1()
                .border_color(rgb(0x343b45))
                .shadow_lg()
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .items_start()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_lg()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Module priority"),
                                )
                                .child(div().text_sm().text_color(rgb(0x8d96a3)).child(
                                    "Drag modules to choose which compatible engine runs first.",
                                )),
                        )
                        .child(
                            div()
                                .id("close-settings")
                                .absolute()
                                .top(px(10.0))
                                .right(px(10.0))
                                .size(px(24.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0xf0f3f7))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(0x292f37)))
                                .child("×")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.settings_open = false;
                                    cx.notify();
                                    cx.stop_propagation();
                                })),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children(priority.iter().enumerate().map(|(index, id)| {
                            let label = module_label(id).to_owned();
                            let drag = ModuleDrag::new(index, label.clone());
                            div()
                                .id(("module-priority", index))
                                .flex()
                                .items_center()
                                .gap_3()
                                .px_4()
                                .py_3()
                                .rounded_lg()
                                .bg(rgb(0x20252c))
                                .border_1()
                                .border_color(rgb(0x303741))
                                .text_color(rgb(0xe4e8ed))
                                .cursor_move()
                                .drag_over::<ModuleDrag>(|style, _, _, _| {
                                    style.bg(rgb(0x29323a)).border_color(rgb(0x4c6475))
                                })
                                .on_drag(drag, |info: &ModuleDrag, position, _, cx| {
                                    cx.new(|_| info.clone().position(position))
                                })
                                .on_drop(cx.listener(move |this, info: &ModuleDrag, _, cx| {
                                    this.move_module(info.index, index, cx);
                                }))
                                .child(div().text_color(rgb(0x798390)).child("⠿"))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(0x707986))
                                        .child(if index == 0 { "First" } else { "Fallback" }),
                                )
                        })),
                )
                .child(div().text_xs().text_color(rgb(0x69727e)).child(
                    "Priority only applies when multiple modules support the selected conversion.",
                )),
        )
}

actions!(shift, [Quit]);

struct Shift {
    selected_file: Option<PathBuf>,
    selected_url: Option<String>,
    file_preview: Option<FilePreview>,
    selection_generation: u64,
    conversion_generation: u64,
    conversion: ConversionState,
    save_status: Option<SharedString>,
    output_format: OutputFormat,
    output_menu_open: bool,
    settings_open: bool,
    module_priority: Vec<String>,
    url_input: Entity<TextInput>,
}

impl Shift {
    fn choose_file(&mut self, cx: &mut Context<Self>) {
        // Ignore clicks while a dialog is already open (prevents multi-panel
        // races that can hang the open/save panel service).
        if file_picker::is_busy() {
            return;
        }

        let start_dir = self
            .selected_file
            .as_ref()
            .and_then(|path| path.parent().map(|p| p.to_path_buf()));

        let receiver = file_picker::pick_file(start_dir);

        cx.spawn(async move |this, cx| {
            let path = receiver.await.ok().flatten();
            let _ = this.update(cx, |this, cx| {
                if let Some(path) = path {
                    this.set_selected_file(path, cx);
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn set_selected_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        file_picker::remember_directory(&path);
        self.selection_generation = self.selection_generation.wrapping_add(1);
        let generation = self.selection_generation;

        self.selected_url = None;
        self.url_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.file_preview = Some(build_file_preview_with_size(&path, "…".into()));
        self.selected_file = Some(path.clone());
        let available_outputs = ConversionRegistry::default().available_outputs(&path);
        if !available_outputs.contains(&self.output_format) {
            self.output_format = available_outputs
                .first()
                .copied()
                .unwrap_or(OutputFormat::MARKDOWN);
        }
        self.conversion = ConversionState::Converting;
        self.save_status = None;
        self.output_menu_open = false;
        cx.notify();

        let preview_path = path.clone();
        let selected_preview_path = path.clone();
        let preview_task = cx
            .background_executor()
            .spawn(async move { build_file_preview(&preview_path) });

        cx.spawn(async move |this, cx| {
            let preview = preview_task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation == generation
                    && this.selected_file.as_ref() == Some(&selected_preview_path)
                {
                    this.file_preview = Some(preview);
                    cx.notify();
                }
            });
        })
        .detach();

        self.start_conversion(cx);
    }

    fn submit_url_from_input(&mut self, cx: &mut Context<Self>) {
        let url = self.url_input.read(cx).content().trim().to_owned();
        self.set_selected_url(url, cx);
    }

    fn set_selected_url(&mut self, url: String, cx: &mut Context<Self>) {
        let url = url.trim().to_owned();
        if url.is_empty() {
            return;
        }
        if !looks_like_url(&url) {
            self.conversion = ConversionState::Failed(
                "Enter a full http:// or https:// URL to extract with Defuddle.".into(),
            );
            self.save_status = None;
            self.output_menu_open = false;
            cx.notify();
            return;
        }

        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.selected_file = None;
        self.selected_url = Some(url.clone());
        self.file_preview = Some(build_url_preview(&url));
        self.url_input
            .update(cx, |input, cx| input.set_content(url.clone(), cx));

        let available_outputs = ConversionRegistry::default().available_url_outputs();
        if !available_outputs.contains(&self.output_format) {
            self.output_format = available_outputs
                .first()
                .copied()
                .unwrap_or(OutputFormat::MARKDOWN);
        }
        self.conversion = ConversionState::Converting;
        self.save_status = None;
        self.output_menu_open = false;
        cx.notify();
        self.start_conversion(cx);
    }

    fn start_conversion(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.selected_file.clone() {
            self.start_file_conversion(path, cx);
            return;
        }
        if let Some(url) = self.selected_url.clone() {
            self.start_url_conversion(url, cx);
            return;
        }
        self.conversion = ConversionState::Empty;
        cx.notify();
    }

    fn start_file_conversion(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.conversion_generation = self.conversion_generation.wrapping_add(1);
        let conversion_generation = self.conversion_generation;
        let generation = self.selection_generation;
        let output_format = self.output_format;
        let priority = self.module_priority.clone();
        self.conversion = ConversionState::Converting;
        self.save_status = None;
        cx.notify();

        let conversion_path = path.clone();
        let conversion_task = cx.background_executor().spawn(async move {
            ConversionRegistry::default()
                .with_priority(&priority)
                .convert_to(&conversion_path, output_format)
        });
        cx.spawn(async move |this, cx| {
            let result = conversion_task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation == generation
                    && this.conversion_generation == conversion_generation
                    && this.selected_file.as_ref() == Some(&path)
                {
                    this.conversion = match result {
                        Ok(artifact) => ConversionState::Ready(artifact),
                        Err(error) => ConversionState::Failed(error.to_string().into()),
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn start_url_conversion(&mut self, url: String, cx: &mut Context<Self>) {
        self.conversion_generation = self.conversion_generation.wrapping_add(1);
        let conversion_generation = self.conversion_generation;
        let generation = self.selection_generation;
        let output_format = self.output_format;
        let priority = self.module_priority.clone();
        self.conversion = ConversionState::Converting;
        self.save_status = None;
        cx.notify();

        let conversion_url = url.clone();
        let conversion_task = cx.background_executor().spawn(async move {
            ConversionRegistry::default()
                .with_priority(&priority)
                .convert_url(&conversion_url, output_format)
        });
        cx.spawn(async move |this, cx| {
            let result = conversion_task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation == generation
                    && this.conversion_generation == conversion_generation
                    && this.selected_url.as_ref() == Some(&url)
                {
                    this.conversion = match result {
                        Ok(artifact) => ConversionState::Ready(artifact),
                        Err(error) => ConversionState::Failed(error.to_string().into()),
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn set_output_format(&mut self, format: OutputFormat, cx: &mut Context<Self>) {
        self.output_menu_open = false;
        if self.output_format != format {
            self.output_format = format;
            self.start_conversion(cx);
        } else {
            cx.notify();
        }
    }

    fn move_module(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from >= self.module_priority.len() || to >= self.module_priority.len() || from == to {
            return;
        }
        let module = self.module_priority.remove(from);
        self.module_priority.insert(to, module);
        let _ = save_module_priority(&self.module_priority);
        if self.selected_file.is_some() || self.selected_url.is_some() {
            self.start_conversion(cx);
        } else {
            cx.notify();
        }
    }

    fn clear_selected_file(&mut self, cx: &mut Context<Self>) {
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.selected_file = None;
        self.selected_url = None;
        self.url_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.file_preview = None;
        self.conversion = ConversionState::Empty;
        self.save_status = None;
        self.output_menu_open = false;
        cx.notify();
    }

    fn save_output(&mut self, cx: &mut Context<Self>) {
        if file_picker::is_busy() {
            return;
        }
        let ConversionState::Ready(artifact) = self.conversion.clone() else {
            return;
        };
        let directory = self
            .selected_file
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let receiver = file_picker::pick_save_file(&artifact.file_name, directory);

        cx.spawn(async move |this, cx| {
            let Some(path) = receiver.await.ok().flatten() else {
                return;
            };
            let file_name = path
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            let result = cx
                .background_executor()
                .spawn(async move { artifact.write_to(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.save_status = Some(match result {
                    Ok(()) => format!("Saved {file_name}").into(),
                    Err(error) => format!("Could not save: {error}").into(),
                });
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for Shift {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preview = self.file_preview.clone();
        let has_selection = self.selected_file.is_some() || self.selected_url.is_some();
        let conversion = self.conversion.clone();
        let save_status = self.save_status.clone();
        let available_outputs = if self.selected_url.is_some() {
            ConversionRegistry::default().available_url_outputs()
        } else if let Some(path) = self.selected_file.as_ref() {
            ConversionRegistry::default().available_outputs(path)
        } else {
            OutputFormat::ALL.to_vec()
        };
        let output_format = self.output_format;
        let output_menu_open = self.output_menu_open;
        let settings_open = self.settings_open;
        let module_priority = self.module_priority.clone();
        let url_input = self.url_input.clone();

        div()
            .id("shift-root")
            .relative()
            .flex()
            .size_full()
            .bg(rgb(0x101216))
            .text_color(rgb(0xf5f7fa))
            .on_click(cx.listener(|this, _, _, cx| {
                if this.output_menu_open {
                    this.output_menu_open = false;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .p_8()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(url_input_bar(url_input, cx))
                    .child({
                        // Hover styling uses hitbox hover (stays true while pressed).
                        // Do NOT drive ElementId / with_animation from on_hover: that
                        // API reports false on mouse-down, remounts this node, and
                        // clears pending_mouse_down — so the first click is eaten.
                        div()
                            .id("file-drop-zone")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .items_center()
                            .justify_center()
                            .rounded_xl()
                            .bg(rgb(DROP_ZONE_COLOR))
                            .hover(|style| style.bg(rgb(DROP_ZONE_HOVER_COLOR)))
                            .cursor_pointer()
                            .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(rgb(0x1c242a)))
                            .child(rounded_dashed_border(has_selection))
                            .when_some(preview, |zone, preview| {
                                zone.child(file_preview_card(preview, cx))
                            })
                            .when(!has_selection, |zone| zone.child(empty_drop_prompt()))
                            .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx)))
                            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                                if let Some(path) = paths.paths().first() {
                                    this.set_selected_file(path.clone(), cx);
                                }
                            }))
                    }),
            )
            .child(
                div()
                    .h_full()
                    .py_8()
                    .child(div().w(px(1.0)).h_full().bg(rgb(0x292d34))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(rgb(0x101216))
                    .child(output_panel(
                        conversion,
                        save_status,
                        output_format,
                        output_menu_open,
                        available_outputs,
                        cx,
                    )),
            )
            .child(
                div()
                    .id("open-settings")
                    .absolute()
                    .top(px(28.0))
                    .right(px(24.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(40.0))
                    .rounded_lg()
                    .bg(rgb(0x1b1f25))
                    .border_1()
                    .border_color(rgb(0x303640))
                    .text_color(rgb(0xa7b0bc))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0x292f37)).text_color(rgb(0xe3e7ec)))
                    .child("⚙")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.output_menu_open = false;
                        this.settings_open = true;
                        cx.notify();
                    })),
            )
            .when(settings_open, |root| {
                root.child(settings_modal(&module_priority, cx))
            })
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        text_input::bind_keys(cx);
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
                window_min_size: Some(size(px(600.0), px(400.0))),
                ..Default::default()
            },
            |_, cx| {
                let shift_entity = cx.new(|cx| {
                    let url_input = cx.new(|cx| TextInput::new(cx, "Paste a URL to extract…", ""));
                    Shift {
                        selected_file: None,
                        selected_url: None,
                        file_preview: None,
                        selection_generation: 0,
                        conversion_generation: 0,
                        conversion: ConversionState::Empty,
                        save_status: None,
                        output_format: OutputFormat::MARKDOWN,
                        output_menu_open: false,
                        settings_open: false,
                        module_priority: load_module_priority(),
                        url_input,
                    }
                });

                // Route Enter in the URL field back to the app entity.
                let parent = shift_entity.downgrade();
                let url_input = shift_entity.read(cx).url_input.clone();
                url_input.update(cx, |input, _cx| {
                    input.set_on_submit(move |url, cx| {
                        let parent = parent.clone();
                        let url = url.to_owned();
                        cx.defer(move |cx| {
                            let _ = parent.update(cx, |this, cx| this.set_selected_url(url, cx));
                        });
                    });
                });

                shift_entity
            },
        )
        .expect("failed to open the main window");

        // Warm the open/save panel service so the first click is fast.
        file_picker::prewarm();

        cx.activate(true);
    });
}
