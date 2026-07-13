mod file_picker;
mod text_input;

use gpui::{
    Animation, AnimationExt, App, Application, Bounds, Context, ElementId, Entity, ExternalPaths,
    FontWeight, KeyBinding, Menu, MenuItem, PathBuilder, PathStyle, Pixels, Point, Render,
    SharedString, StrokeOptions, SystemMenuType, TitlebarOptions, Window, WindowBounds,
    WindowOptions, actions, canvas, div, ease_out_quint, hsla, point, prelude::*, px, rgb, size,
};
use shift_core::conversion::{
    ConversionArtifact, ConversionOptions, ConversionRegistry, FfmpegEncodeMode, FfmpegOptions,
    FfmpegQuality, OutputFormat, input_looks_like_media, is_audio_output, is_ffmpeg_output,
    is_image_output, is_subtitle_output, is_video_output, looks_like_url,
};
use shift_core::preferences::{load_module_priority, save_module_priority};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use text_input::TextInput;

const APP_NAME: &str = "Shift";
/// Black-and-white monospaced developer theme (gpui.rs-inspired).
const FONT_MONO: &str = "Menlo";
const BG: u32 = 0x000000;
const BG_RAISED: u32 = 0x0a0a0a;
const BG_SURFACE: u32 = 0x111111;
const BG_ELEVATED: u32 = 0x1a1a1a;
const BG_HOVER: u32 = 0x222222;
const BG_ACTIVE: u32 = 0x2a2a2a;
const DROP_ZONE_COLOR: u32 = 0x0a0a0a;
const DROP_ZONE_HOVER_COLOR: u32 = 0x111111;
const BORDER: u32 = 0x222222;
const BORDER_STRONG: u32 = 0x333333;
const BORDER_FOCUS: u32 = 0x555555;
const TEXT: u32 = 0xffffff;
const TEXT_PRIMARY: u32 = 0xe8e8e8;
const TEXT_SECONDARY: u32 = 0x888888;
const TEXT_MUTED: u32 = 0x666666;
const TEXT_DIM: u32 = 0x444444;
const TEXT_INVERSE: u32 = 0x000000;
const BADGE_FILL: u32 = 0x1a1a1a;
const BADGE_TEXT: u32 = 0xcccccc;
/// Cap session history so large conversion artifacts cannot grow without bound.
const MAX_HISTORY_ENTRIES: usize = 30;
const HISTORY_SIDEBAR_WIDTH: f32 = 220.0;

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
    /// Shared so render clones stay cheap for large artifacts.
    Ready(Arc<ConversionArtifact>),
    Failed(SharedString),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HistorySource {
    File(PathBuf),
    Url(String),
}

#[derive(Clone)]
enum HistoryOutcome {
    Ready(Arc<ConversionArtifact>),
    Failed(SharedString),
}

#[derive(Clone)]
struct ConversionHistoryEntry {
    id: u64,
    source: HistorySource,
    name: SharedString,
    detail: SharedString,
    extension_label: SharedString,
    badge_color: u32,
    badge_text_color: u32,
    output_format: OutputFormat,
    outcome: HistoryOutcome,
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
            .bg(rgb(BG_ELEVATED))
            .border_1()
            .border_color(rgb(BORDER_STRONG))
            .shadow_lg()
            .text_sm()
            .font_family(FONT_MONO)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(TEXT))
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

    // Monochrome type labels — same fill/text, distinguish by abbreviation only.
    let label: &str = match ext.as_str() {
        "PNG" | "JPG" | "JPEG" | "GIF" | "WEBP" | "HEIC" | "SVG" | "BMP" | "TIFF" => "IMG",
        "MP4" | "MOV" | "MKV" | "AVI" | "WEBM" => "VID",
        "MP3" | "WAV" | "AAC" | "FLAC" | "M4A" | "OGG" => "AUD",
        "PDF" => "PDF",
        "ZIP" | "TAR" | "GZ" | "TGZ" | "7Z" | "RAR" => "ZIP",
        "RS" | "TS" | "TSX" | "JS" | "JSX" | "PY" | "GO" | "SWIFT" | "KT" | "JAVA" | "C"
        | "CPP" | "H" | "CS" | "RB" | "PHP" | "MD" | "TXT" | "RTF" | "DOC" | "DOCX" | "PAGES"
        | "JSON" | "YAML" | "YML" | "TOML" | "XML" | "CSV" => ext.as_str(),
        "" => "FILE",
        other if other.len() <= 4 => other,
        _ => "FILE",
    };

    (label.to_string(), BADGE_FILL, BADGE_TEXT)
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
                let color = if accent {
                    rgb(BORDER_FOCUS)
                } else {
                    rgb(BORDER)
                };
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
        .child(
            div()
                .text_3xl()
                .text_color(rgb(TEXT_SECONDARY))
                .child("\u{2191}"),
        )
        .child(
            div()
                .text_lg()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Drop a file here"),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(TEXT_SECONDARY))
                .child("or click to browse"),
        )
}

fn history_sidebar(
    history: &[ConversionHistoryEntry],
    active_history_id: Option<u64>,
    cx: &mut Context<Shift>,
) -> impl IntoElement {
    let is_empty = history.is_empty();

    div()
        .id("history-sidebar")
        .flex()
        .flex_col()
        .flex_shrink_0()
        .w(px(HISTORY_SIDEBAR_WIDTH))
        .h_full()
        .bg(rgb(BG))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_4()
                .pt(px(28.0))
                .pb_3()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_SECONDARY))
                        .child("History"),
                )
                .when(!is_empty, |header| {
                    header.child(
                        div()
                            .id("clear-history")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .cursor_pointer()
                            .hover(|style| {
                                style.bg(rgb(BG_ELEVATED)).text_color(rgb(TEXT_SECONDARY))
                            })
                            .child("Clear")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clear_history(cx);
                                cx.stop_propagation();
                            })),
                    )
                }),
        )
        .child(
            div()
                .id("history-list")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .px_2()
                .pb_3()
                .gap_1()
                .overflow_y_scroll()
                .when(is_empty, |list| {
                    list.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px_3()
                            .py_4()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(TEXT_MUTED))
                                    .child("No conversions yet"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_DIM))
                                    .child("Completed work shows up here."),
                            ),
                    )
                })
                .children(history.iter().cloned().map(|entry| {
                    let id = entry.id;
                    let active = active_history_id == Some(id);
                    let failed = matches!(entry.outcome, HistoryOutcome::Failed(_));
                    let badge_color = entry.badge_color;
                    let badge_text_color = entry.badge_text_color;

                    div()
                        .id(("history-entry", id))
                        .flex()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .px_2()
                        .py_2()
                        .rounded_lg()
                        .cursor_pointer()
                        .when(active, |row| {
                            row.bg(rgb(BG_ELEVATED))
                                .border_1()
                                .border_color(rgb(BORDER_STRONG))
                        })
                        .when(!active, |row| {
                            row.border_1()
                                .border_color(rgb(BG))
                                .hover(|style| style.bg(rgb(BG_SURFACE)))
                        })
                        .child(
                            div()
                                .flex()
                                .flex_shrink_0()
                                .items_center()
                                .justify_center()
                                .size(px(32.0))
                                .rounded_md()
                                .bg(rgb(badge_color))
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(rgb(badge_text_color))
                                        .child(entry.extension_label),
                                ),
                        )
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
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(if failed {
                                            rgb(TEXT_SECONDARY)
                                        } else {
                                            rgb(TEXT_PRIMARY)
                                        })
                                        .truncate()
                                        .child(entry.name),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(TEXT_MUTED))
                                        .truncate()
                                        .child(entry.detail),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.restore_history_entry(id, cx);
                            cx.stop_propagation();
                        }))
                })),
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
        badge_color: BADGE_FILL,
        badge_text_color: BADGE_TEXT,
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
                .bg(rgb(BG_SURFACE))
                .border_1()
                .border_color(rgb(BORDER))
                .shadow(vec![gpui::BoxShadow {
                    color: hsla(0.0, 0.0, 0.0, 0.65),
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
                                .text_color(rgb(TEXT))
                                .truncate()
                                .child(preview.name),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_SECONDARY))
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
                        .bg(rgb(BG_HOVER))
                        .border_1()
                        .border_color(rgb(BORDER_STRONG))
                        .text_color(rgb(TEXT_SECONDARY))
                        .cursor_pointer()
                        .hover(|style| {
                            style
                                .bg(rgb(BG_HOVER))
                                .border_color(rgb(BORDER_FOCUS))
                                .text_color(rgb(TEXT))
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
                .text_color(rgb(TEXT_MUTED))
                .child("Click to replace  ·  Drop another file"),
        )
        .with_animation(
            "file-preview-in",
            Animation::new(Duration::from_millis(220)).with_easing(ease_out_quint()),
            |element, progress| element.opacity(0.35 + 0.65 * progress),
        )
}

fn markdown_excerpt(artifact: &ConversionArtifact) -> SharedString {
    let text = artifact.text().unwrap_or("Text output is not valid UTF-8.");
    let mut excerpt: String = text.chars().take(1_200).collect();
    if text.chars().count() > 1_200 {
        excerpt.push_str("\n\n…");
    }
    if excerpt.trim().is_empty() {
        excerpt.push_str("The conversion completed with an empty document.");
    }
    excerpt.into()
}

fn artifact_preview(artifact: &ConversionArtifact) -> SharedString {
    if artifact.format.is_text_previewable() {
        return markdown_excerpt(artifact);
    }
    let size = format_file_size(artifact.bytes.len() as u64);
    format!(
        "{} ready to download.\n\n{} · {size} · via {}\n\nBinary media is not shown inline — use Download to save the file.",
        artifact.format.label(),
        artifact.file_name,
        module_label(artifact.module_id)
    )
    .into()
}

fn parse_optional_secs(value: &str) -> Result<Option<f64>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<f64>()
        .map_err(|_| format!("expected seconds, got “{value}”"))
        .and_then(|secs| {
            if secs.is_finite() && secs >= 0.0 {
                Ok(Some(secs))
            } else {
                Err("seconds must be a non-negative number".into())
            }
        })
}

fn parse_optional_u32(value: &str) -> Result<Option<u32>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<u32>()
        .map(Some)
        .map_err(|_| format!("expected a whole number, got “{value}”"))
}

fn chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    cx: &mut Context<Shift>,
    on_click: impl Fn(&mut Shift, &mut Context<Shift>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .bg(if selected { rgb(TEXT) } else { rgb(BG_SURFACE) })
        .text_color(if selected {
            rgb(TEXT_INVERSE)
        } else {
            rgb(TEXT_SECONDARY)
        })
        .border_1()
        .border_color(if selected { rgb(TEXT) } else { rgb(BORDER) })
        .hover(|style| {
            if selected {
                style
            } else {
                style.bg(rgb(BG_HOVER))
            }
        })
        .child(label.into())
        .on_click(cx.listener(move |this, _, _, cx| {
            on_click(this, cx);
            cx.stop_propagation();
        }))
}

struct MediaPanelView {
    output_format: OutputFormat,
    quality: FfmpegQuality,
    encode_mode: FfmpegEncodeMode,
    mono: bool,
    sample_rate_hz: Option<u32>,
    scale_width: Option<u32>,
    start_input: Entity<TextInput>,
    duration_input: Entity<TextInput>,
    frame_input: Entity<TextInput>,
    audio_stream_input: Entity<TextInput>,
    subtitle_stream_input: Entity<TextInput>,
}

fn media_options_panel(view: MediaPanelView, cx: &mut Context<Shift>) -> impl IntoElement {
    let MediaPanelView {
        output_format,
        quality,
        encode_mode,
        mono,
        sample_rate_hz,
        scale_width,
        start_input,
        duration_input,
        frame_input,
        audio_stream_input,
        subtitle_stream_input,
    } = view;
    let show_audio = is_audio_output(output_format) || is_video_output(output_format);
    let show_video = is_video_output(output_format) || is_image_output(output_format);
    let show_frame = is_image_output(output_format);
    let show_subtitle = is_subtitle_output(output_format);
    let show_trim = !is_image_output(output_format) && !is_subtitle_output(output_format);

    div()
        .id("media-options")
        .flex()
        .flex_col()
        .gap_3()
        .w_full()
        .p_3()
        .rounded_xl()
        .bg(rgb(BG_RAISED))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_SECONDARY))
                        .child("Media options (FFmpeg)"),
                )
                .child(
                    div()
                        .id("apply-media-options")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(BG_ELEVATED))
                        .border_1()
                        .border_color(rgb(BORDER_STRONG))
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_PRIMARY))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(BG_ACTIVE)))
                        .child("Apply")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.apply_media_options(cx);
                            cx.stop_propagation();
                        })),
                ),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .items_center()
                .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child("Quality"))
                .child(chip(
                    "media-quality-balanced",
                    FfmpegQuality::Balanced.label(),
                    quality == FfmpegQuality::Balanced,
                    cx,
                    |this, cx| {
                        this.ffmpeg_quality = FfmpegQuality::Balanced;
                        this.start_conversion(cx);
                    },
                ))
                .child(chip(
                    "media-quality-high",
                    FfmpegQuality::High.label(),
                    quality == FfmpegQuality::High,
                    cx,
                    |this, cx| {
                        this.ffmpeg_quality = FfmpegQuality::High;
                        this.start_conversion(cx);
                    },
                ))
                .child(chip(
                    "media-quality-small",
                    FfmpegQuality::Small.label(),
                    quality == FfmpegQuality::Small,
                    cx,
                    |this, cx| {
                        this.ffmpeg_quality = FfmpegQuality::Small;
                        this.start_conversion(cx);
                    },
                )),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .items_center()
                .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child("Encode"))
                .child(chip(
                    "media-encode-auto",
                    FfmpegEncodeMode::Auto.label(),
                    encode_mode == FfmpegEncodeMode::Auto,
                    cx,
                    |this, cx| {
                        this.ffmpeg_encode_mode = FfmpegEncodeMode::Auto;
                        this.start_conversion(cx);
                    },
                ))
                .child(chip(
                    "media-encode-copy",
                    FfmpegEncodeMode::PreferCopy.label(),
                    encode_mode == FfmpegEncodeMode::PreferCopy,
                    cx,
                    |this, cx| {
                        this.ffmpeg_encode_mode = FfmpegEncodeMode::PreferCopy;
                        this.start_conversion(cx);
                    },
                ))
                .child(chip(
                    "media-encode-reencode",
                    FfmpegEncodeMode::Reencode.label(),
                    encode_mode == FfmpegEncodeMode::Reencode,
                    cx,
                    |this, cx| {
                        this.ffmpeg_encode_mode = FfmpegEncodeMode::Reencode;
                        this.start_conversion(cx);
                    },
                )),
        )
        .when(show_trim, |panel| {
            panel.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_MUTED))
                                    .child("Start (sec)"),
                            )
                            .child(
                                div()
                                    .h(px(32.0))
                                    .px_2()
                                    .rounded_md()
                                    .bg(rgb(BG_SURFACE))
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .child(start_input),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .flex_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_MUTED))
                                    .child("Duration (sec)"),
                            )
                            .child(
                                div()
                                    .h(px(32.0))
                                    .px_2()
                                    .rounded_md()
                                    .bg(rgb(BG_SURFACE))
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .child(duration_input),
                            ),
                    ),
            )
        })
        .when(show_frame, |panel| {
            panel.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .child("Frame at (sec)"),
                    )
                    .child(
                        div()
                            .h(px(32.0))
                            .px_2()
                            .rounded_md()
                            .bg(rgb(BG_SURFACE))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(frame_input),
                    ),
            )
        })
        .when(show_audio, |panel| {
            panel
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child("Audio"))
                        .child(chip(
                            "media-mono",
                            if mono { "Mono ✓" } else { "Mono" },
                            mono,
                            cx,
                            |this, cx| {
                                this.ffmpeg_mono = !this.ffmpeg_mono;
                                this.start_conversion(cx);
                            },
                        ))
                        .child(chip(
                            "media-rate-auto",
                            "Rate auto",
                            sample_rate_hz.is_none(),
                            cx,
                            |this, cx| {
                                this.ffmpeg_sample_rate_hz = None;
                                this.start_conversion(cx);
                            },
                        ))
                        .child(chip(
                            "media-rate-44100",
                            "44.1 kHz",
                            sample_rate_hz == Some(44_100),
                            cx,
                            |this, cx| {
                                this.ffmpeg_sample_rate_hz = Some(44_100);
                                this.start_conversion(cx);
                            },
                        ))
                        .child(chip(
                            "media-rate-48000",
                            "48 kHz",
                            sample_rate_hz == Some(48_000),
                            cx,
                            |this, cx| {
                                this.ffmpeg_sample_rate_hz = Some(48_000);
                                this.start_conversion(cx);
                            },
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_MUTED))
                                .child("Audio stream index"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(rgb(BG_SURFACE))
                                .border_1()
                                .border_color(rgb(BORDER))
                                .child(audio_stream_input),
                        ),
                )
        })
        .when(show_video, |panel| {
            panel.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child("Width"))
                    .child(chip(
                        "media-scale-auto",
                        "Auto",
                        scale_width.is_none(),
                        cx,
                        |this, cx| {
                            this.ffmpeg_scale_width = None;
                            this.start_conversion(cx);
                        },
                    ))
                    .child(chip(
                        "media-scale-720",
                        "720",
                        scale_width == Some(720),
                        cx,
                        |this, cx| {
                            this.ffmpeg_scale_width = Some(720);
                            this.start_conversion(cx);
                        },
                    ))
                    .child(chip(
                        "media-scale-1280",
                        "1280",
                        scale_width == Some(1280),
                        cx,
                        |this, cx| {
                            this.ffmpeg_scale_width = Some(1280);
                            this.start_conversion(cx);
                        },
                    ))
                    .child(chip(
                        "media-scale-1920",
                        "1920",
                        scale_width == Some(1920),
                        cx,
                        |this, cx| {
                            this.ffmpeg_scale_width = Some(1920);
                            this.start_conversion(cx);
                        },
                    )),
            )
        })
        .when(show_subtitle, |panel| {
            panel.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .child("Subtitle stream index"),
                    )
                    .child(
                        div()
                            .h(px(32.0))
                            .px_2()
                            .rounded_md()
                            .bg(rgb(BG_SURFACE))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(subtitle_stream_input),
                    ),
            )
        })
        .child(
            div()
                .text_xs()
                .text_color(rgb(TEXT_DIM))
                .child("Edit fields, then Apply. Chips reconvert immediately."),
        )
}

struct OutputPanelView {
    state: ConversionState,
    save_status: Option<SharedString>,
    output_format: OutputFormat,
    output_menu_open: bool,
    available_outputs: Vec<OutputFormat>,
    show_media_options: bool,
    media: MediaPanelView,
}

fn output_panel(view: OutputPanelView, cx: &mut Context<Shift>) -> impl IntoElement {
    let OutputPanelView {
        state,
        save_status,
        output_format,
        output_menu_open,
        available_outputs,
        show_media_options,
        media,
    } = view;
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
                    .text_color(rgb(TEXT_SECONDARY))
                    .child("Output appears here"),
            )
            .child(
                div()
                    .max_w(px(280.0))
                    .text_sm()
                    .text_color(rgb(TEXT_MUTED))
                    .child(
                        "Choose a document, media file, or paste a URL — Shift converts it automatically.",
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
                    .text_color(rgb(TEXT_SECONDARY))
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
                    .text_color(rgb(TEXT_SECONDARY))
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
                    .text_color(rgb(TEXT))
                    .child("Conversion failed"),
            )
            .child(
                div()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(BG_ELEVATED))
                    .border_1()
                    .border_color(rgb(BORDER_STRONG))
                    .text_sm()
                    .text_color(rgb(TEXT_SECONDARY))
                    .child(message),
            ),
        ConversionState::Ready(artifact) => {
            let file_name: SharedString = artifact.file_name.clone().into();
            let size = format_file_size(artifact.bytes.len() as u64);
            let excerpt = artifact_preview(artifact.as_ref());
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
                                        .text_color(rgb(TEXT_SECONDARY))
                                        .child(conversion_detail),
                                ),
                        )
                        .child(
                            div()
                                .id("save-conversion")
                                .px_4()
                                .py_2()
                                .rounded_lg()
                                .bg(rgb(BG_ELEVATED))
                                .border_1()
                                .border_color(rgb(BORDER_STRONG))
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT_PRIMARY))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS)))
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
                        .bg(rgb(BG_RAISED))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .overflow_hidden()
                        .text_sm()
                        .text_color(rgb(TEXT_SECONDARY))
                        .child(excerpt),
                )
                .when_some(save_status, |panel, status| {
                    panel.child(div().text_xs().text_color(rgb(TEXT_SECONDARY)).child(status))
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
                .bg(rgb(BG_SURFACE))
                .border_1()
                .border_color(rgb(BORDER))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(BG_HOVER)))
                .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child("Output"))
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
                        .text_color(rgb(TEXT_SECONDARY))
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
                    .bg(rgb(BG_ELEVATED))
                    .border_1()
                    .border_color(rgb(BORDER_STRONG))
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
                                    rgb(TEXT_PRIMARY)
                                } else {
                                    rgb(TEXT_DIM)
                                })
                                .when(enabled, |row| {
                                    row.cursor_pointer()
                                        .hover(|style| style.bg(rgb(BG_HOVER)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.set_output_format(format, cx);
                                            cx.stop_propagation();
                                        }))
                                })
                                .child(format.label())
                                .when(format == output_format, |row| {
                                    row.child(div().text_color(rgb(TEXT)).child("✓"))
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
        .flex()
        .flex_col()
        .gap_3()
        .when(show_media_options, |panel| {
            panel.child(media_options_panel(media, cx))
        })
        .child(div().flex_1().min_h_0().child(content))
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
        "ffmpeg" => "FFmpeg",
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
                .bg(rgb(BG_SURFACE))
                .border_1()
                .border_color(rgb(BORDER))
                .text_sm()
                .text_color(rgb(TEXT_PRIMARY))
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
                .bg(rgb(BG_ELEVATED))
                .border_1()
                .border_color(rgb(BORDER_STRONG))
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT_PRIMARY))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS)))
                .active(|style| style.opacity(0.82))
                .child("Convert")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.submit_url_from_input(cx);
                    cx.stop_propagation();
                })),
        )
}

fn settings_modal(
    priority: &[String],
    preference_error: Option<SharedString>,
    cx: &mut Context<Shift>,
) -> impl IntoElement {
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
        .bg(hsla(0.0, 0.0, 0.0, 0.82))
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
                // Clip any child that still tries to paint past the fixed width so
                // monospaced copy cannot spill outside the rounded card.
                .overflow_hidden()
                .p_6()
                .rounded_xl()
                .bg(rgb(BG_SURFACE))
                .border_1()
                .border_color(rgb(BG_ACTIVE))
                .shadow_lg()
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    // Keep the close control in normal flex flow (not absolute) so the
                    // title/subtitle column always receives a definite remaining width
                    // and long monospaced copy wraps instead of overflowing.
                    div()
                        .flex()
                        .items_start()
                        .gap_3()
                        .w_full()
                        .min_w_0()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .text_lg()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Module priority"),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .min_w_0()
                                        .text_sm()
                                        .text_color(rgb(TEXT_SECONDARY))
                                        .child(
                                            "Drag modules to choose which compatible engine runs first.",
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .id("close-settings")
                                .flex_shrink_0()
                                .size(px(24.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(BG_HOVER)))
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
                        .w_full()
                        .min_w_0()
                        .children(priority.iter().enumerate().map(|(index, id)| {
                            let label = module_label(id).to_owned();
                            let drag = ModuleDrag::new(index, label.clone());
                            div()
                                .id(("module-priority", index))
                                .flex()
                                .items_center()
                                .gap_3()
                                .w_full()
                                .min_w_0()
                                .px_4()
                                .py_3()
                                .rounded_lg()
                                .bg(rgb(BG_ELEVATED))
                                .border_1()
                                .border_color(rgb(BORDER))
                                .text_color(rgb(TEXT_PRIMARY))
                                .cursor_move()
                                .drag_over::<ModuleDrag>(|style, _, _, _| {
                                    style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS))
                                })
                                .on_drag(drag, |info: &ModuleDrag, position, _, cx| {
                                    cx.new(|_| info.clone().position(position))
                                })
                                .on_drop(cx.listener(move |this, info: &ModuleDrag, _, cx| {
                                    this.move_module(info.index, index, cx);
                                }))
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_color(rgb(TEXT_MUTED))
                                        .child("⠿"),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .truncate()
                                        .child(label),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_xs()
                                        .text_color(rgb(TEXT_MUTED))
                                        .child(if index == 0 { "First" } else { "Fallback" }),
                                )
                        })),
                )
                .when_some(preference_error, |modal, error| {
                    modal.child(
                        div()
                            .w_full()
                            .min_w_0()
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(BG_ELEVATED))
                            .border_1()
                            .border_color(rgb(BORDER_STRONG))
                            .text_xs()
                            .text_color(rgb(TEXT_SECONDARY))
                            .child(error),
                    )
                })
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child(
                            "Priority only applies when multiple modules support the selected conversion.",
                        ),
                ),
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
    preference_error: Option<SharedString>,
    output_format: OutputFormat,
    output_menu_open: bool,
    settings_open: bool,
    module_priority: Vec<String>,
    url_input: Entity<TextInput>,
    history: Vec<ConversionHistoryEntry>,
    next_history_id: u64,
    active_history_id: Option<u64>,
    // FFmpeg media options (shown for media inputs / media outputs).
    ffmpeg_quality: FfmpegQuality,
    ffmpeg_encode_mode: FfmpegEncodeMode,
    ffmpeg_mono: bool,
    ffmpeg_sample_rate_hz: Option<u32>,
    ffmpeg_scale_width: Option<u32>,
    ffmpeg_start_input: Entity<TextInput>,
    ffmpeg_duration_input: Entity<TextInput>,
    ffmpeg_frame_input: Entity<TextInput>,
    ffmpeg_audio_stream_input: Entity<TextInput>,
    ffmpeg_subtitle_stream_input: Entity<TextInput>,
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
        self.active_history_id = None;
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
            // Invalidate any in-flight conversion so a late success cannot
            // replace this validation error with an unrelated Ready state.
            self.selection_generation = self.selection_generation.wrapping_add(1);
            self.conversion_generation = self.conversion_generation.wrapping_add(1);
            self.selected_file = None;
            self.selected_url = None;
            self.file_preview = None;
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
        self.active_history_id = None;
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

    fn media_options_visible(&self) -> bool {
        let media_input = self
            .selected_file
            .as_ref()
            .is_some_and(|path| input_looks_like_media(path));
        media_input || is_ffmpeg_output(self.output_format)
    }

    fn build_conversion_options(&self, cx: &App) -> Result<ConversionOptions, String> {
        let start_secs = parse_optional_secs(self.ffmpeg_start_input.read(cx).content())?;
        let duration_secs = parse_optional_secs(self.ffmpeg_duration_input.read(cx).content())?;
        let frame_secs = parse_optional_secs(self.ffmpeg_frame_input.read(cx).content())?;
        let audio_stream = parse_optional_u32(self.ffmpeg_audio_stream_input.read(cx).content())?;
        let subtitle_stream =
            parse_optional_u32(self.ffmpeg_subtitle_stream_input.read(cx).content())?;
        Ok(ConversionOptions {
            ffmpeg: FfmpegOptions {
                start_secs,
                duration_secs,
                frame_secs,
                audio_stream,
                subtitle_stream,
                encode_mode: self.ffmpeg_encode_mode,
                quality: self.ffmpeg_quality,
                mono: self.ffmpeg_mono,
                sample_rate_hz: self.ffmpeg_sample_rate_hz,
                scale_width: self.ffmpeg_scale_width,
            },
        })
    }

    fn apply_media_options(&mut self, cx: &mut Context<Self>) {
        self.start_conversion(cx);
    }

    fn start_file_conversion(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.conversion_generation = self.conversion_generation.wrapping_add(1);
        let conversion_generation = self.conversion_generation;
        let generation = self.selection_generation;
        let output_format = self.output_format;
        let priority = self.module_priority.clone();
        let options = match self.build_conversion_options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.conversion = ConversionState::Failed(error.into());
                cx.notify();
                return;
            }
        };
        self.conversion = ConversionState::Converting;
        self.save_status = None;
        self.active_history_id = None;
        cx.notify();

        let conversion_path = path.clone();
        let conversion_task = cx.background_executor().spawn(async move {
            ConversionRegistry::default()
                .with_priority(&priority)
                .convert_to_with_options(&conversion_path, output_format, &options)
        });
        cx.spawn(async move |this, cx| {
            let result = conversion_task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation == generation
                    && this.conversion_generation == conversion_generation
                    && this.selected_file.as_ref() == Some(&path)
                {
                    this.conversion = match result {
                        Ok(artifact) => {
                            let artifact = Arc::new(artifact);
                            this.record_history(HistoryOutcome::Ready(Arc::clone(&artifact)));
                            ConversionState::Ready(artifact)
                        }
                        Err(error) => {
                            let message: SharedString = error.to_string().into();
                            this.record_history(HistoryOutcome::Failed(message.clone()));
                            ConversionState::Failed(message)
                        }
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
        let options = match self.build_conversion_options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.conversion = ConversionState::Failed(error.into());
                cx.notify();
                return;
            }
        };
        self.conversion = ConversionState::Converting;
        self.save_status = None;
        self.active_history_id = None;
        cx.notify();

        let conversion_url = url.clone();
        let conversion_task = cx.background_executor().spawn(async move {
            ConversionRegistry::default()
                .with_priority(&priority)
                .convert_url_with_options(&conversion_url, output_format, &options)
        });
        cx.spawn(async move |this, cx| {
            let result = conversion_task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation == generation
                    && this.conversion_generation == conversion_generation
                    && this.selected_url.as_ref() == Some(&url)
                {
                    this.conversion = match result {
                        Ok(artifact) => {
                            let artifact = Arc::new(artifact);
                            this.record_history(HistoryOutcome::Ready(Arc::clone(&artifact)));
                            ConversionState::Ready(artifact)
                        }
                        Err(error) => {
                            let message: SharedString = error.to_string().into();
                            this.record_history(HistoryOutcome::Failed(message.clone()));
                            ConversionState::Failed(message)
                        }
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
        // Apply the new order for this session even if persistence fails, but
        // surface the write error so the next launch is not silently different.
        match save_module_priority(&self.module_priority) {
            Ok(()) => self.preference_error = None,
            Err(error) => {
                self.preference_error =
                    Some(format!("Could not save module priority: {error}").into());
            }
        }
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
        self.active_history_id = None;
        cx.notify();
    }

    fn record_history(&mut self, outcome: HistoryOutcome) {
        let (source, preview) = if let Some(path) = self.selected_file.clone() {
            let preview = self
                .file_preview
                .clone()
                .unwrap_or_else(|| build_file_preview(&path));
            (HistorySource::File(path), preview)
        } else if let Some(url) = self.selected_url.clone() {
            let preview = self
                .file_preview
                .clone()
                .unwrap_or_else(|| build_url_preview(&url));
            (HistorySource::Url(url), preview)
        } else {
            return;
        };

        let detail = match &outcome {
            HistoryOutcome::Ready(artifact) => format!(
                "{}  ·  via {}",
                artifact.format.label(),
                module_label(artifact.module_id)
            ),
            HistoryOutcome::Failed(_) => format!("{}  ·  failed", self.output_format.label()),
        };

        let id = self.next_history_id;
        self.next_history_id = self.next_history_id.wrapping_add(1);
        self.history.insert(
            0,
            ConversionHistoryEntry {
                id,
                source,
                name: preview.name,
                detail: detail.into(),
                extension_label: preview.extension_label,
                badge_color: preview.badge_color,
                badge_text_color: preview.badge_text_color,
                output_format: self.output_format,
                outcome,
            },
        );
        self.history.truncate(MAX_HISTORY_ENTRIES);
        self.active_history_id = Some(id);
    }

    fn restore_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(entry) = self.history.iter().find(|entry| entry.id == id).cloned() else {
            return;
        };

        // Invalidate any in-flight work so a late conversion cannot overwrite
        // the restored snapshot.
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.conversion_generation = self.conversion_generation.wrapping_add(1);
        self.output_menu_open = false;
        self.save_status = None;
        self.output_format = entry.output_format;
        self.active_history_id = Some(entry.id);

        match &entry.source {
            HistorySource::File(path) => {
                file_picker::remember_directory(path);
                self.selected_file = Some(path.clone());
                self.selected_url = None;
                self.url_input
                    .update(cx, |input, cx| input.set_content("", cx));
                // Prefer a live preview when the source is still on disk; fall
                // back to the snapshot captured at conversion time.
                self.file_preview = Some(if path.exists() {
                    build_file_preview(path)
                } else {
                    FilePreview {
                        name: entry.name.clone(),
                        subtitle: entry.detail.clone(),
                        extension_label: entry.extension_label.clone(),
                        badge_color: entry.badge_color,
                        badge_text_color: entry.badge_text_color,
                    }
                });
            }
            HistorySource::Url(url) => {
                self.selected_file = None;
                self.selected_url = Some(url.clone());
                self.url_input
                    .update(cx, |input, cx| input.set_content(url.clone(), cx));
                self.file_preview = Some(build_url_preview(url));
            }
        }

        self.conversion = match entry.outcome {
            HistoryOutcome::Ready(artifact) => ConversionState::Ready(artifact),
            HistoryOutcome::Failed(message) => ConversionState::Failed(message),
        };
        cx.notify();
    }

    fn clear_history(&mut self, cx: &mut Context<Self>) {
        self.history.clear();
        self.active_history_id = None;
        cx.notify();
    }

    fn save_output(&mut self, cx: &mut Context<Self>) {
        if file_picker::is_busy() {
            return;
        }
        let ConversionState::Ready(artifact) = &self.conversion else {
            return;
        };
        let artifact = Arc::clone(artifact);
        let selection_generation = self.selection_generation;
        let conversion_generation = self.conversion_generation;
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
                // Only attribute save status to the selection that started it.
                if this.selection_generation != selection_generation
                    || this.conversion_generation != conversion_generation
                {
                    return;
                }
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
        let preference_error = self.preference_error.clone();
        let url_input = self.url_input.clone();
        let history = self.history.clone();
        let active_history_id = self.active_history_id;
        let show_media_options = self.media_options_visible();
        let ffmpeg_quality = self.ffmpeg_quality;
        let ffmpeg_encode_mode = self.ffmpeg_encode_mode;
        let ffmpeg_mono = self.ffmpeg_mono;
        let ffmpeg_sample_rate_hz = self.ffmpeg_sample_rate_hz;
        let ffmpeg_scale_width = self.ffmpeg_scale_width;
        let ffmpeg_start_input = self.ffmpeg_start_input.clone();
        let ffmpeg_duration_input = self.ffmpeg_duration_input.clone();
        let ffmpeg_frame_input = self.ffmpeg_frame_input.clone();
        let ffmpeg_audio_stream_input = self.ffmpeg_audio_stream_input.clone();
        let ffmpeg_subtitle_stream_input = self.ffmpeg_subtitle_stream_input.clone();

        div()
            .id("shift-root")
            .relative()
            .flex()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family(FONT_MONO)
            .on_click(cx.listener(|this, _, _, cx| {
                if this.output_menu_open {
                    this.output_menu_open = false;
                    cx.notify();
                }
            }))
            .child(history_sidebar(&history, active_history_id, cx))
            .child(
                div()
                    .h_full()
                    .child(div().w(px(1.0)).h_full().bg(rgb(BORDER))),
            )
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
                            .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(rgb(BG_ELEVATED)))
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
                    .child(div().w(px(1.0)).h_full().bg(rgb(BORDER))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(rgb(BG))
                    .child(output_panel(
                        OutputPanelView {
                            state: conversion,
                            save_status,
                            output_format,
                            output_menu_open,
                            available_outputs,
                            show_media_options,
                            media: MediaPanelView {
                                output_format,
                                quality: ffmpeg_quality,
                                encode_mode: ffmpeg_encode_mode,
                                mono: ffmpeg_mono,
                                sample_rate_hz: ffmpeg_sample_rate_hz,
                                scale_width: ffmpeg_scale_width,
                                start_input: ffmpeg_start_input,
                                duration_input: ffmpeg_duration_input,
                                frame_input: ffmpeg_frame_input,
                                audio_stream_input: ffmpeg_audio_stream_input,
                                subtitle_stream_input: ffmpeg_subtitle_stream_input,
                            },
                        },
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
                    .bg(rgb(BG_SURFACE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .text_color(rgb(TEXT_SECONDARY))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(BG_HOVER)).text_color(rgb(TEXT_PRIMARY)))
                    .child("⚙")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.output_menu_open = false;
                        this.settings_open = true;
                        cx.notify();
                    })),
            )
            .when(settings_open, |root| {
                root.child(settings_modal(&module_priority, preference_error, cx))
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

        let bounds = Bounds::centered(None, size(px(1180.0), px(720.0)), cx);

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    ..Default::default()
                }),
                app_id: Some(APP_NAME.into()),
                window_min_size: Some(size(px(900.0), px(520.0))),
                ..Default::default()
            },
            |_, cx| {
                let shift_entity = cx.new(|cx| {
                    let url_input = cx.new(|cx| TextInput::new(cx, "Paste a URL to extract…", ""));
                    let ffmpeg_start_input = cx.new(|cx| TextInput::new(cx, "0", ""));
                    let ffmpeg_duration_input = cx.new(|cx| TextInput::new(cx, "optional", ""));
                    let ffmpeg_frame_input = cx.new(|cx| TextInput::new(cx, "0", ""));
                    let ffmpeg_audio_stream_input = cx.new(|cx| TextInput::new(cx, "0", ""));
                    let ffmpeg_subtitle_stream_input = cx.new(|cx| TextInput::new(cx, "0", ""));
                    Shift {
                        selected_file: None,
                        selected_url: None,
                        file_preview: None,
                        selection_generation: 0,
                        conversion_generation: 0,
                        conversion: ConversionState::Empty,
                        save_status: None,
                        preference_error: None,
                        output_format: OutputFormat::MARKDOWN,
                        output_menu_open: false,
                        settings_open: false,
                        module_priority: load_module_priority(),
                        url_input,
                        history: Vec::new(),
                        next_history_id: 1,
                        active_history_id: None,
                        ffmpeg_quality: FfmpegQuality::default(),
                        ffmpeg_encode_mode: FfmpegEncodeMode::default(),
                        ffmpeg_mono: false,
                        ffmpeg_sample_rate_hz: None,
                        ffmpeg_scale_width: None,
                        ffmpeg_start_input,
                        ffmpeg_duration_input,
                        ffmpeg_frame_input,
                        ffmpeg_audio_stream_input,
                        ffmpeg_subtitle_stream_input,
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
