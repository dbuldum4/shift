mod file_picker;
mod text_input;

use gpui::{
    Animation, AnimationExt, App, Application, Bounds, ClipboardItem, Context, ElementId, Entity,
    ExternalPaths, FocusHandle, FontWeight, KeyBinding, Menu, MenuItem, PathBuilder, PathStyle,
    Pixels, Point, Render, SharedString, StrokeOptions, SystemMenuType, TitlebarOptions, Window,
    WindowBounds, WindowOptions, actions, canvas, div, ease_out_quint, hsla, point, prelude::*, px,
    rgb, size,
};
use shift_core::conversion::{
    BatchEnqueueOptions, BatchEvent, BatchFormatSelection, BatchItem, BatchItemId, BatchItemState,
    BatchQueue, BatchSource, ConversionArtifact, ConversionOptions, ConversionProgress,
    ConversionRegistry, DefuddleOptions, DiagnosticsReport, DoclingImageExportMode, DoclingOptions,
    DoclingTableMode, FfmpegEncodeMode, FfmpegOptions, FfmpegQuality, MAX_EXPAND_FILES,
    MarkItDownOptions, OutputFormat, PandocOptions, PdfInputOptions, Readiness,
    available_ready_outputs, available_ready_url_outputs, expand_input_paths, is_audio_output,
    is_ffmpeg_output, is_image_output, is_subtitle_output, is_video_output, looks_like_url,
    paths_refer_to_same_file, pdf_engine_candidates, run_batch, suggested_output_for_path,
    suggested_output_for_url,
};
use shift_core::history::{
    LoadedHistory, MAX_HISTORY_ARTIFACT_BYTES, MAX_HISTORY_ENTRIES, StoredHistoryEntry,
    StoredOutcome, StoredSource, clear_history_store, intern_module_id, load_history, save_history,
};
use shift_core::preferences::{load_module_priority, save_module_priority};
use shift_core::{
    cache_artifact_bytes, load_default_session_settings, save_default_session_settings,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
const STATUS_READY_FILL: u32 = 0x1a1a1a;
const STATUS_READY_TEXT: u32 = 0xe8e8e8;
const STATUS_READY_BORDER: u32 = 0x555555;
const STATUS_MISSING_FILL: u32 = 0x111111;
const STATUS_MISSING_TEXT: u32 = 0x888888;
const STATUS_MISSING_BORDER: u32 = 0x333333;
const HISTORY_SIDEBAR_WIDTH: f32 = 220.0;
const SETTINGS_SIDEBAR_WIDTH: f32 = 220.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SettingsSection {
    Converters,
    General,
    Options,
    Paths,
    Diagnostics,
    About,
}

impl SettingsSection {
    fn label(self) -> &'static str {
        match self {
            Self::Converters => "Converters",
            Self::General => "General",
            Self::Options => "Options",
            Self::Paths => "Paths",
            Self::Diagnostics => "Diagnostics",
            Self::About => "About",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Converters => "Choose which engine runs first when several support a conversion.",
            Self::General => "Session output format and retained conversion history.",
            Self::Options => {
                "Session conversion knobs for FFmpeg, Docling, Defuddle, Pandoc, and MarkItDown."
            }
            Self::Paths => "Where Shift looks for tools, preferences, and history.",
            Self::Diagnostics => "Installed engines, versions, and install guidance.",
            Self::About => "Version, modules, and project info.",
        }
    }
}

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
    /// Large media/document not retained in RAM; restore re-runs conversion.
    ReadyLarge {
        module_id: SharedString,
        byte_len: usize,
    },
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
                .child("Drop files here"),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(TEXT_SECONDARY))
                .child("or click to browse (multi-select)"),
        )
}

fn batch_item_status_label(item: &BatchItem) -> SharedString {
    match &item.state {
        BatchItemState::Queued => "queued".into(),
        BatchItemState::Running => "running…".into(),
        BatchItemState::Succeeded { written_path, .. } => {
            format!("✓ {}", written_path.display()).into()
        }
        BatchItemState::Failed { error } => format!("✗ {error}").into(),
        BatchItemState::Cancelled => "cancelled".into(),
    }
}

fn batch_queue_panel(
    items: &[BatchItem],
    output_dir: Option<&Path>,
    running: bool,
    force: bool,
    status: Option<SharedString>,
    item_progress: &HashMap<u64, (Option<f32>, SharedString)>,
    cx: &mut Context<Shift>,
) -> impl IntoElement {
    let progress_queued = items
        .iter()
        .filter(|item| matches!(item.state, BatchItemState::Queued))
        .count();
    let progress_failed = items
        .iter()
        .filter(|item| matches!(item.state, BatchItemState::Failed { .. }))
        .count();
    let can_start = progress_queued > 0 && !running;
    let can_retry = progress_failed > 0 && !running;
    let folder_label: SharedString = output_dir
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "Beside each source".into())
        .into();
    let force_label: SharedString = if force {
        "Overwrite: on".into()
    } else {
        "Overwrite: off".into()
    };

    div()
        .id("batch-queue-panel")
        .flex()
        .flex_col()
        .gap_3()
        .h_full()
        .p_6()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("Queue · {} item(s)", items.len())),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            div()
                                .id("batch-action-folder")
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
                                .hover(|style| {
                                    style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS))
                                })
                                .child("Folder")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.choose_output_folder(cx);
                                })),
                        )
                        .child(
                            div()
                                .id("batch-action-force")
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .bg(if force {
                                    rgb(BG_ACTIVE)
                                } else {
                                    rgb(BG_ELEVATED)
                                })
                                .border_1()
                                .border_color(if force {
                                    rgb(BORDER_FOCUS)
                                } else {
                                    rgb(BORDER_STRONG)
                                })
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT_PRIMARY))
                                .cursor_pointer()
                                .hover(|style| {
                                    style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS))
                                })
                                .child(force_label)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.toggle_batch_force(cx);
                                })),
                        )
                        .when(can_start, |row| {
                            row.child(
                                div()
                                    .id("batch-action-start")
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
                                    .hover(|style| {
                                        style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS))
                                    })
                                    .child("Start")
                                    .on_click(cx.listener(|this, _, _, cx| this.start_batch(cx))),
                            )
                        })
                        .when(running, |row| {
                            row.child(
                                div()
                                    .id("batch-action-cancel")
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
                                    .hover(|style| {
                                        style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS))
                                    })
                                    .child("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_batch(cx))),
                            )
                        })
                        .when(can_retry, |row| {
                            row.child(
                                div()
                                    .id("batch-action-retry")
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
                                    .hover(|style| {
                                        style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS))
                                    })
                                    .child("Retry failed")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.retry_failed_batch(cx)),
                                    ),
                            )
                        })
                        .child(
                            div()
                                .id("batch-action-clear")
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
                                .hover(|style| {
                                    style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS))
                                })
                                .child("Clear")
                                .on_click(cx.listener(|this, _, _, cx| this.clear_batch_queue(cx))),
                        ),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(TEXT_MUTED))
                .child(format!("Output: {folder_label}")),
        )
        .when_some(status, |panel, status| {
            panel.child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_SECONDARY))
                    .child(status),
            )
        })
        .child(
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .flex()
                .flex_col()
                .gap_1()
                .children(items.iter().map(|item| {
                    let id = item.id;
                    let name: SharedString = item.source.display_name().into();
                    let mut detail = batch_item_status_label(item).to_string();
                    if matches!(item.state, BatchItemState::Running) {
                        if let Some((fraction, label)) = item_progress.get(&id.0) {
                            detail = match fraction {
                                Some(value) => {
                                    format!("{label} ({:.0}%)", value.clamp(0.0, 1.0) * 100.0)
                                }
                                None => label.to_string(),
                            };
                        }
                    }
                    let format_label: SharedString = match item.format_selection {
                        BatchFormatSelection::Inherit => {
                            format!("{} · inherit", item.output_format.label()).into()
                        }
                        BatchFormatSelection::Override(format) => {
                            format!("{} · override", format.label()).into()
                        }
                    };
                    let retryable = item.state.is_retryable() && !running;
                    let can_override = matches!(item.state, BatchItemState::Queued) && !running;
                    let success_path = match &item.state {
                        BatchItemState::Succeeded { written_path, .. } => {
                            Some(written_path.clone())
                        }
                        _ => None,
                    };
                    div()
                        .id(ElementId::Name(format!("batch-item-{}", id.0).into()))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(BG_RAISED))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .min_w_0()
                                .flex_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(rgb(TEXT_PRIMARY))
                                        .truncate()
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(TEXT_MUTED))
                                        .truncate()
                                        .child(detail),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(TEXT_DIM))
                                        .child(format_label),
                                ),
                        )
                        .when(can_override, |row| {
                            let is_override =
                                matches!(item.format_selection, BatchFormatSelection::Override(_));
                            row.child(
                                div()
                                    .id(ElementId::Name(format!("batch-format-{}", id.0).into()))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(rgb(TEXT_SECONDARY))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(BG_HOVER)).text_color(rgb(TEXT)))
                                    .child(if is_override { "Inherit" } else { "Override" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_batch_item_format(id, cx);
                                        cx.stop_propagation();
                                    })),
                            )
                        })
                        .when_some(success_path, |row, path| {
                            row.child(
                                div()
                                    .id(ElementId::Name(format!("batch-reveal-{}", id.0).into()))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(rgb(TEXT_SECONDARY))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(BG_HOVER)).text_color(rgb(TEXT)))
                                    .child("Reveal")
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        file_picker::reveal_in_finder(&path);
                                        cx.stop_propagation();
                                    })),
                            )
                        })
                        .when(retryable, |row| {
                            row.child(
                                div()
                                    .id(ElementId::Name(format!("batch-retry-{}", id.0).into()))
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(rgb(TEXT_SECONDARY))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(rgb(BG_HOVER)).text_color(rgb(TEXT)))
                                    .child("Retry")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.retry_batch_item(id, cx);
                                        cx.stop_propagation();
                                    })),
                            )
                        })
                })),
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
                                    .child("Completed work is kept across launches."),
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
                .child("Click to add files  ·  Drop more for batch"),
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

struct ConversionPanelView {
    active_modules: Vec<&'static str>,
    output_format: OutputFormat,
    quality: FfmpegQuality,
    encode_mode: FfmpegEncodeMode,
    mono: bool,
    mute: bool,
    normalize_audio: bool,
    burn_subtitles: bool,
    sample_rate_hz: Option<u32>,
    scale_width: Option<u32>,
    start_input: Entity<TextInput>,
    duration_input: Entity<TextInput>,
    frame_input: Entity<TextInput>,
    fps_input: Entity<TextInput>,
    frame_interval_input: Entity<TextInput>,
    audio_stream_input: Entity<TextInput>,
    subtitle_stream_input: Entity<TextInput>,
    docling_images: DoclingImageExportMode,
    docling_ocr: bool,
    docling_tables: bool,
    docling_table_mode: DoclingTableMode,
    docling_ocr_lang_input: Entity<TextInput>,
    defuddle_frontmatter: bool,
    defuddle_lang_input: Entity<TextInput>,
    pandoc_standalone: bool,
    pandoc_toc: bool,
    pandoc_pdf_engine: Option<String>,
    pandoc_reference_doc: Option<PathBuf>,
    pdf_page_from_input: Entity<TextInput>,
    pdf_page_to_input: Entity<TextInput>,
    pdf_password_input: Entity<TextInput>,
    markitdown_keep_data_uris: bool,
}

fn conversion_options_panel(
    view: ConversionPanelView,
    cx: &mut Context<Shift>,
) -> impl IntoElement {
    let ConversionPanelView {
        active_modules,
        output_format,
        quality,
        encode_mode,
        mono,
        mute,
        normalize_audio,
        burn_subtitles,
        sample_rate_hz,
        scale_width,
        start_input,
        duration_input,
        frame_input,
        fps_input,
        frame_interval_input,
        audio_stream_input,
        subtitle_stream_input,
        docling_images,
        docling_ocr,
        docling_tables,
        docling_table_mode,
        docling_ocr_lang_input,
        defuddle_frontmatter,
        defuddle_lang_input,
        pandoc_standalone,
        pandoc_toc,
        pandoc_pdf_engine,
        pandoc_reference_doc,
        pdf_page_from_input,
        pdf_page_to_input,
        pdf_password_input,
        markitdown_keep_data_uris,
    } = view;
    let show_ffmpeg = active_modules.contains(&"ffmpeg");
    let show_docling = active_modules.contains(&"docling");
    let show_defuddle = active_modules.contains(&"defuddle");
    let show_pandoc = active_modules.contains(&"pandoc");
    let show_markitdown = active_modules.contains(&"markitdown");
    let show_pdf_pages = show_docling || show_markitdown;
    let show_audio = is_audio_output(output_format) || is_video_output(output_format);
    let show_video = is_video_output(output_format) || is_image_output(output_format);
    let show_frame = is_image_output(output_format);
    let show_sequence = output_format == OutputFormat::PNG_SEQUENCE_ZIP;
    let show_subtitle = is_subtitle_output(output_format);
    let show_trim = !is_image_output(output_format) && !is_subtitle_output(output_format);
    let show_pdf_engine = output_format == OutputFormat::PDF;
    let ref_doc_label: SharedString = pandoc_reference_doc
        .as_ref()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "Pick reference…".into())
        .into();

    div()
        .id("conversion-options")
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
                        .child("Conversion options"),
                )
                .child(
                    div()
                        .id("apply-conversion-options")
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
                            this.apply_conversion_options(cx);
                            cx.stop_propagation();
                        })),
                ),
        )
        .when(show_ffmpeg, |panel| {
            panel.child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(TEXT_MUTED))
                    .child("FFmpeg"),
            )
        })
        .when(show_ffmpeg, |panel| {
            let mut section = panel
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
                );
            if show_trim {
                section = section.child(
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
                                        .child(start_input.clone()),
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
                                        .child(duration_input.clone()),
                                ),
                        ),
                );
            }
            if show_frame {
                section = section.child(
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
                                .child(frame_input.clone()),
                        ),
                );
            }
            if show_audio {
                section = section
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
                                    this.persist_session_settings(cx);
                                    this.start_conversion(cx);
                                },
                            ))
                            .child(chip(
                                "media-mute",
                                if mute { "Mute ✓" } else { "Mute" },
                                mute,
                                cx,
                                |this, cx| {
                                    this.ffmpeg_mute = !this.ffmpeg_mute;
                                    this.persist_session_settings(cx);
                                    this.start_conversion(cx);
                                },
                            ))
                            .child(chip(
                                "media-normalize",
                                if normalize_audio {
                                    "Normalize ✓"
                                } else {
                                    "Normalize"
                                },
                                normalize_audio,
                                cx,
                                |this, cx| {
                                    this.ffmpeg_normalize = !this.ffmpeg_normalize;
                                    this.persist_session_settings(cx);
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
                                    this.persist_session_settings(cx);
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
                                    .child(audio_stream_input.clone()),
                            ),
                    );
            }
            if show_video {
                section = section.child(
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
                                this.persist_session_settings(cx);
                                this.start_conversion(cx);
                            },
                        )),
                );
                section = section
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child("FPS"))
                            .child(
                                div()
                                    .h(px(32.0))
                                    .px_2()
                                    .rounded_md()
                                    .bg(rgb(BG_SURFACE))
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .child(fps_input.clone()),
                            ),
                    )
                    .child(div().flex().flex_wrap().gap_2().items_center().child(chip(
                        "media-burn-subs",
                        if burn_subtitles {
                            "Burn subs ✓"
                        } else {
                            "Burn subs"
                        },
                        burn_subtitles,
                        cx,
                        |this, cx| {
                            this.ffmpeg_burn_subs = !this.ffmpeg_burn_subs;
                            this.persist_session_settings(cx);
                            this.start_conversion(cx);
                        },
                    )));
            }
            if show_sequence {
                section = section.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_MUTED))
                                .child("Frame interval (sec)"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(rgb(BG_SURFACE))
                                .border_1()
                                .border_color(rgb(BORDER))
                                .child(frame_interval_input.clone()),
                        ),
                );
            }
            if show_subtitle {
                section = section.child(
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
                                .child(subtitle_stream_input.clone()),
                        ),
                );
            }
            section
        })
        .when(show_docling, |panel| {
            panel
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_MUTED))
                        .child("Docling"),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child("Images"))
                        .child(chip(
                            "docling-images-placeholder",
                            DoclingImageExportMode::Placeholder.label(),
                            docling_images == DoclingImageExportMode::Placeholder,
                            cx,
                            |this, cx| {
                                this.docling_images = DoclingImageExportMode::Placeholder;
                                this.start_conversion(cx);
                            },
                        ))
                        .child(chip(
                            "docling-images-embedded",
                            DoclingImageExportMode::Embedded.label(),
                            docling_images == DoclingImageExportMode::Embedded,
                            cx,
                            |this, cx| {
                                this.docling_images = DoclingImageExportMode::Embedded;
                                this.start_conversion(cx);
                            },
                        ))
                        .child(chip(
                            "docling-images-referenced",
                            DoclingImageExportMode::Referenced.label(),
                            docling_images == DoclingImageExportMode::Referenced,
                            cx,
                            |this, cx| {
                                this.docling_images = DoclingImageExportMode::Referenced;
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
                        .child(chip(
                            "docling-ocr",
                            if docling_ocr { "OCR ✓" } else { "OCR" },
                            docling_ocr,
                            cx,
                            |this, cx| {
                                this.docling_ocr = !this.docling_ocr;
                                this.start_conversion(cx);
                            },
                        ))
                        .child(chip(
                            "docling-tables",
                            if docling_tables {
                                "Tables ✓"
                            } else {
                                "Tables"
                            },
                            docling_tables,
                            cx,
                            |this, cx| {
                                this.docling_tables = !this.docling_tables;
                                this.start_conversion(cx);
                            },
                        ))
                        .child(chip(
                            "docling-table-fast",
                            DoclingTableMode::Fast.label(),
                            docling_table_mode == DoclingTableMode::Fast,
                            cx,
                            |this, cx| {
                                this.docling_table_mode = DoclingTableMode::Fast;
                                this.start_conversion(cx);
                            },
                        ))
                        .child(chip(
                            "docling-table-accurate",
                            DoclingTableMode::Accurate.label(),
                            docling_table_mode == DoclingTableMode::Accurate,
                            cx,
                            |this, cx| {
                                this.docling_table_mode = DoclingTableMode::Accurate;
                                this.persist_session_settings(cx);
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
                                .child("OCR language (e.g. eng)"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(rgb(BG_SURFACE))
                                .border_1()
                                .border_color(rgb(BORDER))
                                .child(docling_ocr_lang_input),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child("Embedded images can produce large artifacts."),
                )
        })
        .when(show_pdf_pages, |panel| {
            panel
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_MUTED))
                        .child("PDF pages"),
                )
                .child(
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
                                        .child("From page"),
                                )
                                .child(
                                    div()
                                        .h(px(32.0))
                                        .px_2()
                                        .rounded_md()
                                        .bg(rgb(BG_SURFACE))
                                        .border_1()
                                        .border_color(rgb(BORDER))
                                        .child(pdf_page_from_input),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .flex_1()
                                .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child("To page"))
                                .child(
                                    div()
                                        .h(px(32.0))
                                        .px_2()
                                        .rounded_md()
                                        .bg(rgb(BG_SURFACE))
                                        .border_1()
                                        .border_color(rgb(BORDER))
                                        .child(pdf_page_to_input),
                                ),
                        ),
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
                                .child("PDF password (session only)"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(rgb(BG_SURFACE))
                                .border_1()
                                .border_color(rgb(BORDER))
                                .child(pdf_password_input),
                        ),
                )
        })
        .when(show_defuddle, |panel| {
            panel
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_MUTED))
                        .child("Defuddle"),
                )
                .child(div().flex().flex_wrap().gap_2().items_center().child(chip(
                    "defuddle-frontmatter",
                    if defuddle_frontmatter {
                        "Frontmatter ✓"
                    } else {
                        "Frontmatter"
                    },
                    defuddle_frontmatter,
                    cx,
                    |this, cx| {
                        this.defuddle_frontmatter = !this.defuddle_frontmatter;
                        this.start_conversion(cx);
                    },
                )))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_MUTED))
                                .child("Language (BCP 47)"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(rgb(BG_SURFACE))
                                .border_1()
                                .border_color(rgb(BORDER))
                                .child(defuddle_lang_input),
                        ),
                )
        })
        .when(show_pandoc, |panel| {
            let mut section = panel
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_MUTED))
                        .child("Pandoc"),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(chip(
                            "pandoc-standalone",
                            if pandoc_standalone {
                                "Standalone ✓"
                            } else {
                                "Standalone"
                            },
                            pandoc_standalone,
                            cx,
                            |this, cx| {
                                this.pandoc_standalone = !this.pandoc_standalone;
                                this.start_conversion(cx);
                            },
                        ))
                        .child(chip(
                            "pandoc-toc",
                            if pandoc_toc { "TOC ✓" } else { "TOC" },
                            pandoc_toc,
                            cx,
                            |this, cx| {
                                this.pandoc_toc = !this.pandoc_toc;
                                this.start_conversion(cx);
                            },
                        )),
                );
            if show_pdf_engine {
                let mut engines = div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .child("PDF engine"),
                    )
                    .child(chip(
                        "pandoc-pdf-auto",
                        "Auto",
                        pandoc_pdf_engine.is_none(),
                        cx,
                        |this, cx| {
                            this.pandoc_pdf_engine = None;
                            this.start_conversion(cx);
                        },
                    ));
                for (index, name) in pdf_engine_candidates().iter().take(5).enumerate() {
                    let selected = pandoc_pdf_engine.as_deref() == Some(*name);
                    let label = (*name).to_owned();
                    engines = engines.child(chip(
                        ("pandoc-pdf-engine", index),
                        *name,
                        selected,
                        cx,
                        move |this, cx| {
                            this.pandoc_pdf_engine = Some(label.clone());
                            this.persist_session_settings(cx);
                            this.start_conversion(cx);
                        },
                    ));
                }
                section = section.child(engines);
            }
            section = section.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .child("Reference doc"),
                    )
                    .child(
                        div()
                            .id("pandoc-reference-doc")
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
                            .child(ref_doc_label)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pick_reference_doc(cx);
                                cx.stop_propagation();
                            })),
                    )
                    .when(pandoc_reference_doc.is_some(), |row| {
                        row.child(chip(
                            "pandoc-reference-clear",
                            "Clear",
                            false,
                            cx,
                            |this, cx| {
                                this.pandoc_reference_doc = None;
                                this.persist_session_settings(cx);
                                this.start_conversion(cx);
                            },
                        ))
                    }),
            );
            section
        })
        .when(show_markitdown, |panel| {
            panel
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_MUTED))
                        .child("MarkItDown"),
                )
                .child(div().flex().flex_wrap().gap_2().items_center().child(chip(
                    "markitdown-keep-data-uris",
                    if markitdown_keep_data_uris {
                        "Keep data URIs ✓"
                    } else {
                        "Keep data URIs"
                    },
                    markitdown_keep_data_uris,
                    cx,
                    |this, cx| {
                        this.markitdown_keep_data_uris = !this.markitdown_keep_data_uris;
                        this.start_conversion(cx);
                    },
                )))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_DIM))
                        .child("Keeping data URIs can produce large Markdown files."),
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
    format_filter_input: Entity<TextInput>,
    format_filter: String,
    available_outputs: Vec<OutputFormat>,
    /// Formats whose engines are installed and ready (subset of available when diagnostics known).
    ready_outputs: Option<Vec<OutputFormat>>,
    show_conversion_options: bool,
    conversion_options: ConversionPanelView,
    conversion_progress: Option<(Option<f32>, SharedString)>,
    show_command_inspect: bool,
    install_hints: Vec<(SharedString, SharedString)>,
}

fn output_panel(view: OutputPanelView, cx: &mut Context<Shift>) -> impl IntoElement {
    let OutputPanelView {
        state,
        save_status,
        output_format,
        output_menu_open,
        format_filter_input,
        format_filter,
        available_outputs,
        ready_outputs,
        show_conversion_options,
        conversion_options,
        conversion_progress,
        show_command_inspect,
        install_hints,
    } = view;
    let filter_lower = format_filter.to_ascii_lowercase();
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
        ConversionState::Converting => {
            let progress_label = conversion_progress
                .as_ref()
                .map(|(_, label)| label.clone())
                .unwrap_or_else(|| format!("Converting to {}…", output_format.label()).into());
            let fraction = conversion_progress
                .as_ref()
                .and_then(|(fraction, _)| *fraction);
            div()
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
                        .child(progress_label),
                )
                .when_some(fraction, |panel, value| {
                    let clamped = value.clamp(0.0, 1.0);
                    panel
                        .child(
                            div()
                                .w(px(220.0))
                                .h(px(6.0))
                                .rounded_full()
                                .bg(rgb(BG_ELEVATED))
                                .border_1()
                                .border_color(rgb(BORDER))
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .bg(rgb(TEXT_SECONDARY))
                                        .w(px(220.0 * clamped)),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_MUTED))
                                .child(format!("{:.0}%", clamped * 100.0)),
                        )
                })
                .child(
                    div()
                        .id("cancel-conversion")
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
                        .hover(|style| style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS)))
                        .child("Cancel")
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_conversion(cx))),
                )
        }
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
            )
            .children(install_hints.into_iter().enumerate().map(|(index, (label, hint))| {
                let hint_for_copy = hint.clone();
                div()
                    .id(("install-hint", index as u64))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .bg(rgb(BG_RAISED))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT_SECONDARY))
                            .child(label),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .child(hint.clone()),
                    )
                    .child(
                        div()
                            .id(("copy-install", index as u64))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(BG_ELEVATED))
                            .border_1()
                            .border_color(rgb(BORDER_STRONG))
                            .text_xs()
                            .text_color(rgb(TEXT_PRIMARY))
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(BG_ACTIVE)))
                            .child("Copy install command")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    hint_for_copy.to_string(),
                                ));
                                this.save_status = Some("Install command copied.".into());
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    )
            })),
        ConversionState::Ready(artifact) => {
            let file_name: SharedString = artifact.file_name.clone().into();
            let size = format_file_size(artifact.bytes.len() as u64);
            let excerpt = artifact_preview(artifact.as_ref());
            let is_text = artifact.format.is_text_previewable();
            let pipeline_badge: SharedString = if artifact.pipeline.is_empty() {
                module_label(artifact.module_id).into()
            } else {
                artifact
                    .pipeline
                    .iter()
                    .map(|id| module_label(id))
                    .collect::<Vec<_>>()
                    .join(" → ")
                    .into()
            };
            let conversion_detail = format!(
                "{}  ·  {size}  ·  via {pipeline_badge}",
                artifact.format.label()
            );
            let commands: Vec<SharedString> = artifact
                .invocations
                .iter()
                .map(|inv| format!("{}: {}", inv.module_id, inv.argv_display).into())
                .collect();
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
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .min_w_0()
                                .flex_1()
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
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .gap_1()
                                        .child(
                                            div()
                                                .px_2()
                                                .py_1()
                                                .rounded_md()
                                                .bg(rgb(BADGE_FILL))
                                                .text_xs()
                                                .text_color(rgb(BADGE_TEXT))
                                                .child(pipeline_badge),
                                        ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .child(action_chip("save-conversion", "Download", cx, |this, cx| {
                                    this.save_output(cx);
                                }))
                                .child(action_chip("copy-conversion", "Copy", cx, |this, cx| {
                                    this.copy_output(cx);
                                }))
                                .child(action_chip("reveal-conversion", "Reveal", cx, |this, cx| {
                                    this.reveal_output(cx);
                                }))
                                .child(action_chip("open-conversion", "Open", cx, |this, cx| {
                                    this.open_output(cx);
                                }))
                                .when(!commands.is_empty(), |row| {
                                    row.child(action_chip(
                                        "show-command",
                                        if show_command_inspect {
                                            "Hide cmd"
                                        } else {
                                            "Show cmd"
                                        },
                                        cx,
                                        |this, cx| {
                                            this.show_command_inspect = !this.show_command_inspect;
                                            cx.notify();
                                        },
                                    ))
                                }),
                        ),
                )
                .when(!is_text, |panel| {
                    panel.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_4()
                            .rounded_xl()
                            .bg(rgb(BG_ELEVATED))
                            .border_1()
                            .border_color(rgb(BORDER_STRONG))
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Binary artifact"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_MUTED))
                                    .child(format!(
                                        "{} · {size} · not shown inline",
                                        artifact.format.label()
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(action_chip(
                                        "binary-copy-path",
                                        "Copy path",
                                        cx,
                                        |this, cx| this.copy_output(cx),
                                    ))
                                    .child(action_chip(
                                        "binary-reveal",
                                        "Reveal",
                                        cx,
                                        |this, cx| this.reveal_output(cx),
                                    ))
                                    .child(action_chip(
                                        "binary-open",
                                        "Open",
                                        cx,
                                        |this, cx| this.open_output(cx),
                                    )),
                            ),
                    )
                })
                .when(is_text, |panel| {
                    panel.child(
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
                })
                .when(show_command_inspect && !commands.is_empty(), |panel| {
                    panel.child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(BG_RAISED))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT_SECONDARY))
                                    .child("Command"),
                            )
                            .children(commands.into_iter().map(|line| {
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_MUTED))
                                    .child(line)
                            })),
                    )
                })
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
                    .w(px(220.0))
                    .max_h(px(420.0))
                    .overflow_y_scroll()
                    .p_1()
                    .rounded_lg()
                    .bg(rgb(BG_ELEVATED))
                    .border_1()
                    .border_color(rgb(BORDER_STRONG))
                    .shadow_lg()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .h(px(32.0))
                            .px_2()
                            .mb_1()
                            .rounded_md()
                            .bg(rgb(BG_SURFACE))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(format_filter_input),
                    )
                    .children(
                        OutputFormat::ALL
                            .iter()
                            .copied()
                            .enumerate()
                            .filter(|(_, format)| {
                                if filter_lower.is_empty() {
                                    return true;
                                }
                                format.label().to_ascii_lowercase().contains(&filter_lower)
                                    || format.id().to_ascii_lowercase().contains(&filter_lower)
                            })
                            .map(|(index, format)| {
                                let enabled = available_outputs.contains(&format);
                                let engine_ready = ready_outputs
                                    .as_ref()
                                    .map(|ready| ready.contains(&format))
                                    .unwrap_or(true);
                                let label_color = if !enabled {
                                    rgb(TEXT_DIM)
                                } else if !engine_ready {
                                    rgb(TEXT_MUTED)
                                } else {
                                    rgb(TEXT_PRIMARY)
                                };
                                div()
                                    .id(("output-format", index))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .px_3()
                                    .py_2()
                                    .rounded_md()
                                    .text_sm()
                                    .text_color(label_color)
                                    .when(enabled, |row| {
                                        row.cursor_pointer()
                                            .hover(|style| style.bg(rgb(BG_HOVER)))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.set_output_format(format, cx);
                                                cx.stop_propagation();
                                            }))
                                    })
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(format.label())
                                            .when(enabled && !engine_ready, |row| {
                                                row.child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(TEXT_DIM))
                                                        .child("missing"),
                                                )
                                            }),
                                    )
                                    .when(format == output_format, |row| {
                                        row.child(div().text_color(rgb(TEXT)).child("✓"))
                                    })
                            }),
                    ),
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
        .when(show_conversion_options, |panel| {
            panel.child(conversion_options_panel(conversion_options, cx))
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

fn action_chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    cx: &mut Context<Shift>,
    on_click: impl Fn(&mut Shift, &mut Context<Shift>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
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
        .hover(|style| style.bg(rgb(BG_ACTIVE)).border_color(rgb(BORDER_FOCUS)))
        .child(label.into())
        .on_click(cx.listener(move |this, _, _, cx| {
            on_click(this, cx);
            cx.stop_propagation();
        }))
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

fn module_description(id: &str) -> &str {
    match id {
        "markitdown" => "Broad document, image, audio, and archive → Markdown.",
        "pandoc" => "Publishing formats (DOCX, PDF, HTML, wiki, and more).",
        "defuddle" => "Clean article extraction from URLs and local HTML.",
        "docling" => "Layout-aware PDF and office documents → Markdown/HTML/text.",
        "ffmpeg" => "Audio, video, stills, and subtitle conversion.",
        _ => "Conversion module.",
    }
}

fn to_stored_entry(entry: &ConversionHistoryEntry) -> StoredHistoryEntry {
    let source = match &entry.source {
        HistorySource::File(path) => StoredSource::File(path.clone()),
        HistorySource::Url(url) => StoredSource::Url(url.clone()),
    };
    let outcome = match &entry.outcome {
        HistoryOutcome::Ready(artifact) => StoredOutcome::Ready {
            module_id: artifact.module_id.to_owned(),
            file_name: artifact.file_name.clone(),
            format: artifact.format.id().to_owned(),
            bytes: artifact.bytes.clone(),
        },
        HistoryOutcome::ReadyLarge {
            module_id,
            byte_len,
        } => StoredOutcome::ReadyLarge {
            module_id: module_id.to_string(),
            byte_len: *byte_len,
        },
        HistoryOutcome::Failed(message) => StoredOutcome::Failed(message.to_string()),
    };
    StoredHistoryEntry {
        id: entry.id,
        source,
        name: entry.name.to_string(),
        detail: entry.detail.to_string(),
        extension_label: entry.extension_label.to_string(),
        badge_color: entry.badge_color,
        badge_text_color: entry.badge_text_color,
        output_format: entry.output_format.id().to_owned(),
        outcome,
    }
}

fn from_stored_entry(entry: StoredHistoryEntry) -> Option<ConversionHistoryEntry> {
    let output_format = entry.output_format.parse().ok()?;
    let source = match entry.source {
        StoredSource::File(path) => HistorySource::File(path),
        StoredSource::Url(url) => HistorySource::Url(url),
    };
    let outcome = match entry.outcome {
        StoredOutcome::Ready {
            module_id,
            file_name,
            format,
            bytes,
        } => {
            let format: OutputFormat = format.parse().ok().unwrap_or(output_format);
            HistoryOutcome::Ready(Arc::new(ConversionArtifact {
                file_name,
                media_type: format.media_type(),
                bytes,
                format,
                module_id: intern_module_id(&module_id),
                pipeline: Vec::new(),
                invocations: Vec::new(),
            }))
        }
        StoredOutcome::ReadyLarge {
            module_id,
            byte_len,
        } => HistoryOutcome::ReadyLarge {
            module_id: module_id.into(),
            byte_len,
        },
        StoredOutcome::Failed(message) => HistoryOutcome::Failed(message.into()),
    };
    Some(ConversionHistoryEntry {
        id: entry.id,
        source,
        name: entry.name.into(),
        detail: entry.detail.into(),
        extension_label: entry.extension_label.into(),
        badge_color: entry.badge_color,
        badge_text_color: entry.badge_text_color,
        output_format,
        outcome,
    })
}

fn history_from_store(loaded: LoadedHistory) -> (Vec<ConversionHistoryEntry>, u64) {
    let mut max_id = 0u64;
    let entries: Vec<_> = loaded
        .entries
        .into_iter()
        .filter_map(|entry| {
            max_id = max_id.max(entry.id);
            from_stored_entry(entry)
        })
        .collect();
    let next_id = loaded.next_id.max(max_id.saturating_add(1)).max(1);
    (entries, next_id)
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

fn settings_nav_item(
    section: SettingsSection,
    active: SettingsSection,
    index: usize,
    cx: &mut Context<Shift>,
) -> impl IntoElement + use<> {
    let selected = section == active;
    div()
        .id(("settings-nav", index))
        .flex()
        .items_center()
        .w_full()
        .px_3()
        .py_2()
        .rounded_lg()
        .cursor_pointer()
        .bg(if selected { rgb(BG_ELEVATED) } else { rgb(BG) })
        .border_1()
        .border_color(if selected { rgb(BORDER) } else { rgb(BG) })
        .hover(|style| {
            if selected {
                style
            } else {
                style.bg(rgb(BG_SURFACE))
            }
        })
        .child(
            div()
                .text_sm()
                .font_weight(if selected {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if selected {
                    rgb(TEXT_PRIMARY)
                } else {
                    rgb(TEXT_SECONDARY)
                })
                .child(section.label()),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.settings_section = section;
            cx.notify();
            cx.stop_propagation();
        }))
}

fn settings_section_header(
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .min_w_0()
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(title.into()),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_sm()
                .text_color(rgb(TEXT_SECONDARY))
                .child(subtitle.into()),
        )
}

fn settings_card(title: impl Into<SharedString>, body: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .w_full()
        .min_w_0()
        .p_4()
        .rounded_xl()
        .bg(rgb(BG_SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT_SECONDARY))
                .child(title.into()),
        )
        .child(body)
}

fn readiness_badge(readiness: Readiness) -> impl IntoElement {
    let (fill, text, border, label) = match readiness {
        Readiness::Ready => (
            STATUS_READY_FILL,
            STATUS_READY_TEXT,
            STATUS_READY_BORDER,
            "READY",
        ),
        Readiness::Missing => (
            STATUS_MISSING_FILL,
            STATUS_MISSING_TEXT,
            STATUS_MISSING_BORDER,
            "MISSING",
        ),
    };
    div()
        .flex_shrink_0()
        .px_2()
        .py_0p5()
        .rounded_md()
        .bg(rgb(fill))
        .border_1()
        .border_color(rgb(border))
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(text))
        .child(label)
}

fn settings_converters_panel(
    priority: &[String],
    preference_error: Option<SharedString>,
    diagnostics: Option<Arc<DiagnosticsReport>>,
    cx: &mut Context<Shift>,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .gap_5()
        .w_full()
        .min_w_0()
        .child(settings_section_header(
            "Converters",
            "Drag modules to choose which compatible engine runs first.",
        ))
        .child(div().flex().flex_col().gap_2().w_full().min_w_0().children(
            priority.iter().enumerate().map(|(index, id)| {
                let label = module_label(id).to_owned();
                let description = module_description(id).to_owned();
                let readiness = diagnostics
                    .as_ref()
                    .and_then(|report| report.engine(id))
                    .map(|engine| engine.readiness);
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
                    .child(div().flex_shrink_0().text_color(rgb(TEXT_MUTED)).child("⠿"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(TEXT_MUTED))
                                    .truncate()
                                    .child(description),
                            ),
                    )
                    .when_some(readiness, |row, status| row.child(readiness_badge(status)))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(rgb(TEXT_MUTED))
                            .child(if index == 0 { "First" } else { "Fallback" }),
                    )
            }),
        ))
        .when_some(preference_error, |panel, error| {
            panel.child(
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
                    "Priority only applies when multiple modules support the selected conversion. Status badges show whether each engine is installed on this Mac (see Diagnostics).",
                ),
        )
}

fn settings_general_panel(
    output_format: OutputFormat,
    history_count: usize,
    cx: &mut Context<Shift>,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .gap_5()
        .w_full()
        .min_w_0()
        .child(settings_section_header(
            "General",
            "Output format applies to this session. History is retained on this Mac across launches.",
        ))
        .child(settings_card(
            "Current output format",
            div()
                .flex()
                .flex_col()
                .gap_3()
                .w_full()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .w_full()
                        .child(chip(
                            "default-output-md",
                            OutputFormat::MARKDOWN.label(),
                            output_format == OutputFormat::MARKDOWN,
                            cx,
                            |this, cx| this.set_output_format(OutputFormat::MARKDOWN, cx),
                        ))
                        .child(chip(
                            "default-output-html",
                            OutputFormat::HTML.label(),
                            output_format == OutputFormat::HTML,
                            cx,
                            |this, cx| this.set_output_format(OutputFormat::HTML, cx),
                        ))
                        .child(chip(
                            "default-output-pdf",
                            OutputFormat::PDF.label(),
                            output_format == OutputFormat::PDF,
                            cx,
                            |this, cx| this.set_output_format(OutputFormat::PDF, cx),
                        ))
                        .child(chip(
                            "default-output-docx",
                            OutputFormat::DOCX.label(),
                            output_format == OutputFormat::DOCX,
                            cx,
                            |this, cx| this.set_output_format(OutputFormat::DOCX, cx),
                        ))
                        .child(chip(
                            "default-output-pptx",
                            OutputFormat::PPTX.label(),
                            output_format == OutputFormat::PPTX,
                            cx,
                            |this, cx| this.set_output_format(OutputFormat::PPTX, cx),
                        )),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child(
                            "Same control as the main output menu. Choosing a format here updates the session and reconverts the current source when one is selected.",
                        ),
                ),
        ))
        .child(settings_card(
            "History",
            div()
                .flex()
                .flex_col()
                .gap_3()
                .w_full()
                .child(div().text_sm().text_color(rgb(TEXT_PRIMARY)).child(format!(
                    "{history_count} entr{} retained (max {MAX_HISTORY_ENTRIES}).",
                    if history_count == 1 { "y" } else { "ies" }
                )))
                .child(
                    div()
                        .id("settings-clear-history")
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(36.0))
                        .px_4()
                        .rounded_lg()
                        .bg(rgb(BG_ELEVATED))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .text_sm()
                        .text_color(rgb(TEXT_SECONDARY))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(BG_HOVER)).text_color(rgb(TEXT_PRIMARY)))
                        .child("Clear history")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.clear_history(cx);
                            cx.stop_propagation();
                        })),
                )
                .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child(
                    "History is saved under Application Support and restored when you reopen Shift. Clear removes it from this Mac.",
                )),
        ))
}

#[allow(clippy::too_many_arguments)]
fn settings_options_panel(
    quality: FfmpegQuality,
    encode_mode: FfmpegEncodeMode,
    mono: bool,
    docling_images: DoclingImageExportMode,
    docling_ocr: bool,
    docling_tables: bool,
    docling_table_mode: DoclingTableMode,
    defuddle_frontmatter: bool,
    pandoc_standalone: bool,
    pandoc_toc: bool,
    markitdown_keep_data_uris: bool,
    cx: &mut Context<Shift>,
) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .gap_5()
        .w_full()
        .min_w_0()
        .child(settings_section_header(
            "Options",
            "Session conversion knobs. Changes apply immediately when a matching conversion is active. Saved across launches (passwords never are).",
        ))
        .child(settings_card(
            "FFmpeg quality",
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(chip(
                            "settings-quality-balanced",
                            FfmpegQuality::Balanced.label(),
                            quality == FfmpegQuality::Balanced,
                            cx,
                            |this, cx| {
                                this.ffmpeg_quality = FfmpegQuality::Balanced;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-quality-high",
                            FfmpegQuality::High.label(),
                            quality == FfmpegQuality::High,
                            cx,
                            |this, cx| {
                                this.ffmpeg_quality = FfmpegQuality::High;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-quality-small",
                            FfmpegQuality::Small.label(),
                            quality == FfmpegQuality::Small,
                            cx,
                            |this, cx| {
                                this.ffmpeg_quality = FfmpegQuality::Small;
                                this.apply_session_option_change(cx);
                            },
                        )),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child("Tradeoff when re-encoding media. Ignored during stream copy."),
                ),
        ))
        .child(settings_card(
            "FFmpeg encode mode",
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(chip(
                            "settings-encode-auto",
                            FfmpegEncodeMode::Auto.label(),
                            encode_mode == FfmpegEncodeMode::Auto,
                            cx,
                            |this, cx| {
                                this.ffmpeg_encode_mode = FfmpegEncodeMode::Auto;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-encode-copy",
                            FfmpegEncodeMode::PreferCopy.label(),
                            encode_mode == FfmpegEncodeMode::PreferCopy,
                            cx,
                            |this, cx| {
                                this.ffmpeg_encode_mode = FfmpegEncodeMode::PreferCopy;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-encode-reencode",
                            FfmpegEncodeMode::Reencode.label(),
                            encode_mode == FfmpegEncodeMode::Reencode,
                            cx,
                            |this, cx| {
                                this.ffmpeg_encode_mode = FfmpegEncodeMode::Reencode;
                                this.apply_session_option_change(cx);
                            },
                        )),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child(
                            "Auto re-encodes with quality presets. Stream copy remuxes without re-encoding. Re-encode always applies quality, mono, sample rate, and scale.",
                        ),
                ),
        ))
        .child(settings_card(
            "FFmpeg audio",
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div().flex().flex_wrap().gap_2().child(chip(
                        "settings-mono",
                        if mono { "Mono on" } else { "Mono off" },
                        mono,
                        cx,
                        |this, cx| {
                            this.ffmpeg_mono = !this.ffmpeg_mono;
                            this.apply_session_option_change(cx);
                        },
                    )),
                ),
        ))
        .child(settings_card(
            "Docling",
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(chip(
                            "settings-docling-placeholder",
                            DoclingImageExportMode::Placeholder.label(),
                            docling_images == DoclingImageExportMode::Placeholder,
                            cx,
                            |this, cx| {
                                this.docling_images = DoclingImageExportMode::Placeholder;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-docling-embedded",
                            DoclingImageExportMode::Embedded.label(),
                            docling_images == DoclingImageExportMode::Embedded,
                            cx,
                            |this, cx| {
                                this.docling_images = DoclingImageExportMode::Embedded;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-docling-referenced",
                            DoclingImageExportMode::Referenced.label(),
                            docling_images == DoclingImageExportMode::Referenced,
                            cx,
                            |this, cx| {
                                this.docling_images = DoclingImageExportMode::Referenced;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-docling-ocr",
                            if docling_ocr { "OCR ✓" } else { "OCR" },
                            docling_ocr,
                            cx,
                            |this, cx| {
                                this.docling_ocr = !this.docling_ocr;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-docling-tables",
                            if docling_tables {
                                "Tables ✓"
                            } else {
                                "Tables"
                            },
                            docling_tables,
                            cx,
                            |this, cx| {
                                this.docling_tables = !this.docling_tables;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-docling-table-fast",
                            DoclingTableMode::Fast.label(),
                            docling_table_mode == DoclingTableMode::Fast,
                            cx,
                            |this, cx| {
                                this.docling_table_mode = DoclingTableMode::Fast;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-docling-table-accurate",
                            DoclingTableMode::Accurate.label(),
                            docling_table_mode == DoclingTableMode::Accurate,
                            cx,
                            |this, cx| {
                                this.docling_table_mode = DoclingTableMode::Accurate;
                                this.apply_session_option_change(cx);
                            },
                        )),
                ),
        ))
        .child(settings_card(
            "Defuddle / Pandoc / MarkItDown",
            div()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(chip(
                            "settings-defuddle-frontmatter",
                            if defuddle_frontmatter {
                                "Frontmatter ✓"
                            } else {
                                "Frontmatter"
                            },
                            defuddle_frontmatter,
                            cx,
                            |this, cx| {
                                this.defuddle_frontmatter = !this.defuddle_frontmatter;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-pandoc-standalone",
                            if pandoc_standalone {
                                "Standalone ✓"
                            } else {
                                "Standalone"
                            },
                            pandoc_standalone,
                            cx,
                            |this, cx| {
                                this.pandoc_standalone = !this.pandoc_standalone;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-pandoc-toc",
                            if pandoc_toc { "TOC ✓" } else { "TOC" },
                            pandoc_toc,
                            cx,
                            |this, cx| {
                                this.pandoc_toc = !this.pandoc_toc;
                                this.apply_session_option_change(cx);
                            },
                        ))
                        .child(chip(
                            "settings-markitdown-uris",
                            if markitdown_keep_data_uris {
                                "Keep data URIs ✓"
                            } else {
                                "Keep data URIs"
                            },
                            markitdown_keep_data_uris,
                            cx,
                            |this, cx| {
                                this.markitdown_keep_data_uris = !this.markitdown_keep_data_uris;
                                this.apply_session_option_change(cx);
                            },
                        )),
                ),
        ))
}

fn settings_paths_panel() -> impl IntoElement + use<> {
    let home = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join("Library/Application Support/Shift"))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/Library/Application Support/Shift".into());

    let env_rows = [
        (
            "SHIFT_MODULE_PRIORITY",
            "Comma-separated converter order override",
        ),
        ("SHIFT_MARKITDOWN_BIN", "Path to the markitdown executable"),
        ("SHIFT_PANDOC_BIN", "Path to the pandoc executable"),
        ("SHIFT_DEFUDDLE_BIN", "Path to the defuddle executable"),
        ("SHIFT_DOCLING_BIN", "Path to the docling executable"),
        ("SHIFT_FFMPEG_BIN", "Path to the ffmpeg executable"),
        (
            "SHIFT_PDF_ENGINE",
            "PDF engine for Pandoc (typst, xelatex, …)",
        ),
    ];

    div()
        .flex()
        .flex_col()
        .gap_5()
        .w_full()
        .min_w_0()
        .child(settings_section_header(
            "Paths",
            "Where preferences live and how external tools are discovered.",
        ))
        .child(settings_card(
            "Preferences directory",
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(div().text_sm().text_color(rgb(TEXT_PRIMARY)).child(home))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child(
                            "Module priority is stored in module-priority; conversion history is stored in history.",
                        ),
                ),
        ))
        .child(settings_card(
            "Environment overrides",
            div()
                .flex()
                .flex_col()
                .gap_3()
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .w_full()
                        .min_w_0()
                        .children(env_rows.into_iter().map(|(name, hint)| {
                            div()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .w_full()
                                .min_w_0()
                                .px_3()
                                .py_2()
                                .rounded_lg()
                                .bg(rgb(BG_ELEVATED))
                                .border_1()
                                .border_color(rgb(BORDER))
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(rgb(TEXT_PRIMARY))
                                        .child(name),
                                )
                                .child(div().text_xs().text_color(rgb(TEXT_MUTED)).child(hint))
                        })),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child(
                            "Optional shell variables that override Shift’s automatic tool discovery. Set them in your terminal profile or launch environment; restart Shift after changing them. Leave unset to use PATH and project-local installs.",
                        ),
                ),
        ))
}

fn settings_diagnostics_panel(
    diagnostics: Option<Arc<DiagnosticsReport>>,
    loading: bool,
    cx: &mut Context<Shift>,
) -> impl IntoElement + use<> {
    let summary = diagnostics.as_ref().map(|report| {
        format!(
            "{}/{} engines ready · PDF {}",
            report.ready_engine_count(),
            report.engines.len(),
            if report.any_pdf_engine_ready() {
                "ready"
            } else {
                "missing"
            }
        )
    });

    div()
        .flex()
        .flex_col()
        .gap_5()
        .w_full()
        .min_w_0()
        .child(settings_section_header(
            "Diagnostics",
            "Installed engines on this Mac versus formats Shift knows how to convert.",
        ))
        .child(settings_card(
            "Supported vs available",
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(TEXT_PRIMARY))
                        .child(
                            "Format supported means a module registers the conversion pair. Conversion currently available means the required external engine is installed and ready.",
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child(
                            "Use `shift-cli formats` for registered capability and `shift-cli doctor` for readiness (exit 0 = at least one engine ready; check complete= in --script for a full install).",
                        ),
                )
                .when_some(summary, |card, text| {
                    card.child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(TEXT_SECONDARY))
                            .child(text),
                    )
                }),
        ))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .w_full()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_SECONDARY))
                        .child("Conversion engines"),
                )
                .child(
                    div()
                        .id("settings-diagnostics-refresh")
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(32.0))
                        .px_3()
                        .rounded_lg()
                        .bg(rgb(BG_ELEVATED))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .text_xs()
                        .text_color(rgb(TEXT_SECONDARY))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(BG_HOVER)).text_color(rgb(TEXT_PRIMARY)))
                        .child(if loading { "Checking…" } else { "Refresh" })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.refresh_diagnostics(cx);
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
                .children(
                    diagnostics
                        .as_ref()
                        .map(|report| {
                            report
                                .engines
                                .iter()
                                .map(|engine| {
                                    let version = engine
                                        .version
                                        .clone()
                                        .unwrap_or_else(|| {
                                            if engine.readiness.is_ready() {
                                                "version unknown".into()
                                            } else {
                                                "not installed".into()
                                            }
                                        });
                                    let path = engine
                                        .resolved_path
                                        .as_ref()
                                        .map(|p| p.display().to_string())
                                        .unwrap_or_else(|| "—".into());
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .w_full()
                                        .min_w_0()
                                        .px_4()
                                        .py_3()
                                        .rounded_lg()
                                        .bg(rgb(BG_ELEVATED))
                                        .border_1()
                                        .border_color(rgb(BORDER))
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .justify_between()
                                                .gap_3()
                                                .w_full()
                                                .min_w_0()
                                                .child(
                                                    div()
                                                        .flex()
                                                        .flex_col()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .gap_0p5()
                                                        .child(
                                                            div()
                                                                .text_sm()
                                                                .font_weight(FontWeight::SEMIBOLD)
                                                                .text_color(rgb(TEXT_PRIMARY))
                                                                .truncate()
                                                                .child(engine.label),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(rgb(TEXT_MUTED))
                                                                .truncate()
                                                                .child(format!(
                                                                    "{version} · {path}"
                                                                )),
                                                        ),
                                                )
                                                .child(readiness_badge(engine.readiness)),
                                        )
                                        .when(!engine.readiness.is_ready(), |card| {
                                            card.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(TEXT_SECONDARY))
                                                    .child(format!(
                                                        "Install: {} · or set {}",
                                                        engine.install_hint, engine.env_override
                                                    )),
                                            )
                                        })
                                        .when_some(engine.notes.clone(), |card, notes| {
                                            card.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(TEXT_MUTED))
                                                    .child(notes),
                                            )
                                        })
                                        .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_else(|| {
                            vec![div()
                                .text_sm()
                                .text_color(rgb(TEXT_MUTED))
                                .child(if loading {
                                    "Probing engines…"
                                } else {
                                    "No diagnostics yet. Click Refresh."
                                })
                                .into_any_element()]
                        }),
                ),
        )
        .child(settings_card(
            "PDF engines (Pandoc)",
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(TEXT_MUTED))
                        .child(
                            "Pandoc shells out to an external PDF engine. Typst is recommended for new installs (`brew install typst`). Override with SHIFT_PDF_ENGINE.",
                        ),
                )
                .children(
                    diagnostics
                        .as_ref()
                        .map(|report| {
                            report
                                .pdf_engines
                                .iter()
                                .map(|engine| {
                                    let version = engine
                                        .version
                                        .clone()
                                        .unwrap_or_else(|| "—".into());
                                    let selected = if engine.selected { " · selected" } else { "" };
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .gap_3()
                                        .px_3()
                                        .py_2()
                                        .rounded_lg()
                                        .bg(rgb(BG_SURFACE))
                                        .border_1()
                                        .border_color(rgb(BORDER))
                                        .child(
                                            div()
                                                .flex()
                                                .flex_col()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_0p5()
                                                .child(
                                                    div()
                                                        .text_sm()
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(rgb(TEXT_PRIMARY))
                                                        .child(format!("{}{selected}", engine.name)),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(TEXT_MUTED))
                                                        .truncate()
                                                        .child(version),
                                                ),
                                        )
                                        .child(readiness_badge(engine.readiness))
                                        .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                )
                .when(
                    diagnostics
                        .as_ref()
                        .is_some_and(|report| !report.any_pdf_engine_ready()),
                    |card| {
                        card.child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_SECONDARY))
                                .child(
                                    "Install: brew install typst  ·  or brew install --cask basictex  ·  or set SHIFT_PDF_ENGINE",
                                ),
                        )
                    },
                ),
        ))
}

fn settings_about_panel(priority: &[String]) -> impl IntoElement + use<> {
    div()
        .flex()
        .flex_col()
        .gap_5()
        .w_full()
        .min_w_0()
        .child(settings_section_header(
            "About",
            "Shift converts local files and URLs into downloadable artifacts.",
        ))
        .child(settings_card(
            "Application",
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_PRIMARY))
                        .child(format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"))),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(TEXT_SECONDARY))
                        .child(
                            "Native macOS app and shift-cli share the same conversion modules and dispatch rules.",
                        ),
                ),
        ))
        .child(settings_card(
            "Loaded modules",
            div()
                .flex()
                .flex_col()
                .gap_2()
                .children(priority.iter().map(|id| {
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .bg(rgb(BG_ELEVATED))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT_PRIMARY))
                                .child(module_label(id).to_owned()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_MUTED))
                                .child(id.clone()),
                        )
                })),
        ))
        .child(
            div()
                .text_xs()
                .text_color(rgb(TEXT_MUTED))
                .child(
                    "MarkItDown · Pandoc · Defuddle · Docling · FFmpeg",
                ),
        )
}

struct SettingsView {
    section: SettingsSection,
    priority: Vec<String>,
    preference_error: Option<SharedString>,
    output_format: OutputFormat,
    history_count: usize,
    quality: FfmpegQuality,
    encode_mode: FfmpegEncodeMode,
    mono: bool,
    docling_images: DoclingImageExportMode,
    docling_ocr: bool,
    docling_tables: bool,
    docling_table_mode: DoclingTableMode,
    defuddle_frontmatter: bool,
    pandoc_standalone: bool,
    pandoc_toc: bool,
    markitdown_keep_data_uris: bool,
    diagnostics: Option<Arc<DiagnosticsReport>>,
    diagnostics_loading: bool,
}

fn settings_content(view: &SettingsView, cx: &mut Context<Shift>) -> impl IntoElement + use<> {
    let SettingsView {
        section,
        priority,
        preference_error,
        output_format,
        history_count,
        quality,
        encode_mode,
        mono,
        docling_images,
        docling_ocr,
        docling_tables,
        docling_table_mode,
        defuddle_frontmatter,
        pandoc_standalone,
        pandoc_toc,
        markitdown_keep_data_uris,
        diagnostics,
        diagnostics_loading,
    } = view;

    div()
        .id("settings-content")
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_hidden()
        .bg(rgb(BG))
        .child(
            div()
                .id("settings-content-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .p_8()
                .child(match *section {
                    SettingsSection::Converters => settings_converters_panel(
                        priority,
                        preference_error.clone(),
                        diagnostics.clone(),
                        cx,
                    )
                    .into_any_element(),
                    SettingsSection::General => {
                        settings_general_panel(*output_format, *history_count, cx)
                            .into_any_element()
                    }
                    SettingsSection::Options => settings_options_panel(
                        *quality,
                        *encode_mode,
                        *mono,
                        *docling_images,
                        *docling_ocr,
                        *docling_tables,
                        *docling_table_mode,
                        *defuddle_frontmatter,
                        *pandoc_standalone,
                        *pandoc_toc,
                        *markitdown_keep_data_uris,
                        cx,
                    )
                    .into_any_element(),
                    SettingsSection::Paths => settings_paths_panel().into_any_element(),
                    SettingsSection::Diagnostics => {
                        settings_diagnostics_panel(diagnostics.clone(), *diagnostics_loading, cx)
                            .into_any_element()
                    }
                    SettingsSection::About => settings_about_panel(priority).into_any_element(),
                }),
        )
}

fn settings_screen(view: SettingsView, cx: &mut Context<Shift>) -> impl IntoElement + use<> {
    let section = view.section;

    div()
        .id("settings-screen")
        .absolute()
        .top_0()
        .right_0()
        .bottom_0()
        .left_0()
        .flex()
        .flex_col()
        .size_full()
        .bg(rgb(BG))
        .cursor_default()
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(
            // Top bar with back control. Extra top padding clears the traffic lights.
            div()
                .id("settings-topbar")
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_3()
                .w_full()
                .px_4()
                .pt(px(40.0))
                .pb_3()
                .border_b_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .id("settings-back")
                        .flex()
                        .items_center()
                        .gap_2()
                        .h(px(36.0))
                        .px_3()
                        .rounded_lg()
                        .bg(rgb(BG_SURFACE))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(TEXT_PRIMARY))
                        .cursor_pointer()
                        .hover(|style| style.bg(rgb(BG_HOVER)))
                        .child(div().text_color(rgb(TEXT_SECONDARY)).child("←"))
                        .child("Back")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.settings_open = false;
                            cx.notify();
                            cx.stop_propagation();
                        })),
                )
                .child(
                    // Non-clickable breadcrumb: Settings / <current section>
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(div().text_color(rgb(TEXT_SECONDARY)).child("Settings"))
                        .child(div().text_color(rgb(TEXT_MUTED)).child("/"))
                        .child(div().text_color(rgb(TEXT_PRIMARY)).child(section.label())),
                )
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(rgb(TEXT_MUTED))
                        .child(section.description()),
                ),
        )
        .child(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    // Left sidebar of settings sections.
                    div()
                        .id("settings-sidebar")
                        .flex()
                        .flex_col()
                        .flex_shrink_0()
                        .w(px(SETTINGS_SIDEBAR_WIDTH))
                        .h_full()
                        .bg(rgb(BG))
                        .border_r_1()
                        .border_color(rgb(BORDER))
                        .px_3()
                        .py_4()
                        .gap_1()
                        .child(settings_nav_item(
                            SettingsSection::Converters,
                            section,
                            0,
                            cx,
                        ))
                        .child(settings_nav_item(SettingsSection::General, section, 1, cx))
                        .child(settings_nav_item(SettingsSection::Options, section, 2, cx))
                        .child(settings_nav_item(SettingsSection::Paths, section, 3, cx))
                        .child(settings_nav_item(
                            SettingsSection::Diagnostics,
                            section,
                            4,
                            cx,
                        ))
                        .child(settings_nav_item(SettingsSection::About, section, 5, cx)),
                )
                .child(settings_content(&view, cx)),
        )
}

actions!(
    shift,
    [
        Quit,
        SaveOutput,
        CopyOutput,
        RevealOutput,
        ToggleFormatMenu,
        OpenSettings,
        ShowShortcuts,
        CancelWork,
    ]
);

/// Pending folder expansion confirmation before batch enqueue.
#[derive(Clone)]
struct FolderExpandConfirm {
    expanded: Vec<PathBuf>,
}

struct Shift {
    focus_handle: FocusHandle,
    selected_file: Option<PathBuf>,
    selected_url: Option<String>,
    file_preview: Option<FilePreview>,
    selection_generation: u64,
    conversion_generation: u64,
    conversion: ConversionState,
    save_status: Option<SharedString>,
    preference_error: Option<SharedString>,
    output_format: OutputFormat,
    /// When false, selecting a new source may apply a suggested format.
    user_chose_format: bool,
    output_menu_open: bool,
    format_filter_input: Entity<TextInput>,
    settings_open: bool,
    settings_section: SettingsSection,
    shortcuts_help_open: bool,
    show_command_inspect: bool,
    module_priority: Vec<String>,
    diagnostics: Option<Arc<DiagnosticsReport>>,
    diagnostics_loading: bool,
    url_input: Entity<TextInput>,
    history: Vec<ConversionHistoryEntry>,
    next_history_id: u64,
    active_history_id: Option<u64>,
    // Shared batch queue (same runner as shift-cli).
    batch_queue: BatchQueue,
    batch_output_dir: Option<PathBuf>,
    batch_running: bool,
    batch_generation: u64,
    batch_cancel: Arc<AtomicBool>,
    batch_status: Option<SharedString>,
    /// When true, batch writes overwrite existing outputs (CLI `--force` parity).
    batch_force: bool,
    /// Per-item progress labels/fractions from the batch runner.
    batch_item_progress: HashMap<u64, (Option<f32>, SharedString)>,
    /// Pending recursive folder expansion (confirm before enqueue).
    folder_confirm: Option<FolderExpandConfirm>,
    /// Cooperative cancel for the active single-file / single-URL conversion.
    conversion_cancel: Arc<AtomicBool>,
    /// Live conversion progress (fraction when known + label).
    conversion_progress: Option<(Option<f32>, SharedString)>,
    /// Cached path for the ready artifact (binary copy / reveal / open).
    cached_ready_path: Option<PathBuf>,
    // Session conversion options (shown for engines on the active route).
    ffmpeg_quality: FfmpegQuality,
    ffmpeg_encode_mode: FfmpegEncodeMode,
    ffmpeg_mono: bool,
    ffmpeg_mute: bool,
    ffmpeg_normalize: bool,
    ffmpeg_burn_subs: bool,
    ffmpeg_sample_rate_hz: Option<u32>,
    ffmpeg_scale_width: Option<u32>,
    ffmpeg_start_input: Entity<TextInput>,
    ffmpeg_duration_input: Entity<TextInput>,
    ffmpeg_frame_input: Entity<TextInput>,
    ffmpeg_fps_input: Entity<TextInput>,
    ffmpeg_frame_interval_input: Entity<TextInput>,
    ffmpeg_audio_stream_input: Entity<TextInput>,
    ffmpeg_subtitle_stream_input: Entity<TextInput>,
    docling_images: DoclingImageExportMode,
    docling_ocr: bool,
    docling_tables: bool,
    docling_table_mode: DoclingTableMode,
    docling_ocr_lang_input: Entity<TextInput>,
    defuddle_frontmatter: bool,
    defuddle_lang_input: Entity<TextInput>,
    pandoc_standalone: bool,
    pandoc_toc: bool,
    pandoc_pdf_engine: Option<String>,
    pandoc_reference_doc: Option<PathBuf>,
    pdf_page_from_input: Entity<TextInput>,
    pdf_page_to_input: Entity<TextInput>,
    pdf_password_input: Entity<TextInput>,
    markitdown_keep_data_uris: bool,
}

impl Shift {
    fn refresh_diagnostics(&mut self, cx: &mut Context<Self>) {
        if self.diagnostics_loading {
            return;
        }
        self.diagnostics_loading = true;
        cx.notify();

        let task = cx
            .background_executor()
            .spawn(async move { DiagnosticsReport::collect() });
        cx.spawn(async move |this, cx| {
            let report = task.await;
            let _ = this.update(cx, |this, cx| {
                this.diagnostics = Some(Arc::new(report));
                this.diagnostics_loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    fn ensure_diagnostics(&mut self, cx: &mut Context<Self>) {
        if self.diagnostics.is_none() && !self.diagnostics_loading {
            self.refresh_diagnostics(cx);
        }
    }

    fn choose_file(&mut self, cx: &mut Context<Self>) {
        // Ignore clicks while a dialog is already open (prevents multi-panel
        // races that can hang the open/save panel service).
        if file_picker::is_busy() {
            return;
        }

        let start_dir = self
            .selected_file
            .as_ref()
            .and_then(|path| path.parent().map(|p| p.to_path_buf()))
            .or_else(|| self.batch_output_dir.clone());

        // Multi-select open panel: one file keeps interactive preview; many
        // files enter the shared batch queue.
        let receiver = file_picker::pick_files(start_dir);

        cx.spawn(async move |this, cx| {
            let paths = receiver.await.unwrap_or_default();
            let _ = this.update(cx, |this, cx| {
                if paths.is_empty() {
                    cx.notify();
                } else {
                    this.ingest_paths(paths, cx);
                }
            });
        })
        .detach();
    }

    fn choose_output_folder(&mut self, cx: &mut Context<Self>) {
        if file_picker::is_busy() {
            return;
        }
        let start_dir = self.batch_output_dir.clone().or_else(|| {
            self.selected_file
                .as_ref()
                .and_then(|path| path.parent().map(|p| p.to_path_buf()))
        });
        let receiver = file_picker::pick_directory(start_dir);
        cx.spawn(async move |this, cx| {
            let path = receiver.await.ok().flatten();
            let _ = this.update(cx, |this, cx| {
                if let Some(path) = path {
                    this.set_batch_output_dir(path, cx);
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn set_batch_output_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.batch_running {
            self.batch_status =
                Some("Cannot change output folder while a batch is running.".into());
            cx.notify();
            return;
        }
        file_picker::remember_directory(&path);
        self.batch_output_dir = Some(path.clone());
        self.batch_queue.set_output_dir(Some(path.as_path()));
        self.batch_status = Some(format!("Output folder: {}", path.display()).into());
        self.persist_session_settings(cx);
        cx.notify();
    }

    fn ingest_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let has_dir = paths.iter().any(|path| path.is_dir());
        if has_dir {
            match expand_input_paths(&paths, true) {
                Ok(expanded) => {
                    if expanded.is_empty() {
                        self.batch_status =
                            Some("No convertible files found in the selected folder(s).".into());
                        self.folder_confirm = None;
                        cx.notify();
                        return;
                    }
                    self.folder_confirm = Some(FolderExpandConfirm {
                        expanded: expanded.clone(),
                    });
                    self.batch_status = Some(
                        format!(
                            "Expand folders? {} file(s) (cap {}). Confirm to queue or dismiss.",
                            expanded.len(),
                            MAX_EXPAND_FILES
                        )
                        .into(),
                    );
                    cx.notify();
                }
                Err(error) => {
                    self.folder_confirm = None;
                    self.batch_status = Some(error.to_string().into());
                    cx.notify();
                }
            }
            return;
        }
        if paths.len() == 1 && self.batch_queue.is_empty() {
            self.set_selected_file(paths[0].clone(), cx);
            return;
        }
        // Queue only; user picks Folder (optional) then Start.
        self.enqueue_paths(paths, false, cx);
    }

    fn confirm_folder_expand(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.folder_confirm.take() else {
            return;
        };
        self.enqueue_paths(confirm.expanded, false, cx);
    }

    fn dismiss_folder_confirm(&mut self, cx: &mut Context<Self>) {
        self.folder_confirm = None;
        self.batch_status = Some("Folder expansion cancelled.".into());
        cx.notify();
    }

    fn toggle_batch_item_format(&mut self, id: BatchItemId, cx: &mut Context<Self>) {
        if self.batch_running {
            return;
        }
        let Some(item) = self.batch_queue.get(id) else {
            return;
        };
        let selection = match item.format_selection {
            BatchFormatSelection::Inherit => BatchFormatSelection::Override(self.output_format),
            BatchFormatSelection::Override(_) => BatchFormatSelection::Inherit,
        };
        if self.batch_queue.set_item_format_selection(
            id,
            selection,
            self.output_format,
            self.batch_output_dir.as_deref(),
        ) {
            self.batch_status = Some(match selection {
                BatchFormatSelection::Inherit => "Item format: inherit global.".into(),
                BatchFormatSelection::Override(format) => {
                    format!("Item format pinned to {}.", format.label()).into()
                }
            });
            cx.notify();
        }
    }

    fn enqueue_paths(&mut self, paths: Vec<PathBuf>, auto_start: bool, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        if self.batch_running {
            self.batch_status =
                Some("Cannot add files while a batch is running. Wait or Cancel first.".into());
            cx.notify();
            return;
        }
        for path in &paths {
            file_picker::remember_directory(path);
        }
        let options = match self.build_conversion_options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.batch_status = Some(error.into());
                cx.notify();
                return;
            }
        };
        let enqueue = BatchEnqueueOptions {
            output_format: self.output_format,
            conversion: options,
            output_dir: self.batch_output_dir.clone(),
            force: self.batch_force,
        };
        let sources = paths
            .into_iter()
            .map(BatchSource::from_path_or_url)
            .collect::<Vec<_>>();
        let count = sources.len();
        self.batch_queue.enqueue_many(sources, &enqueue);
        // Focus first file for the drop-zone card when entering batch mode.
        if let Some(item) = self.batch_queue.items().first() {
            if let Some(path) = item.source.as_file() {
                self.selected_file = Some(path.to_path_buf());
                self.selected_url = None;
                self.file_preview = Some(build_file_preview(path));
            }
        }
        self.batch_status =
            Some(format!("Queued {count} file(s). Choose Folder if needed, then Start.").into());
        self.conversion = ConversionState::Empty;
        cx.notify();
        if auto_start {
            self.start_batch(cx);
        }
    }

    fn start_batch(&mut self, cx: &mut Context<Self>) {
        if self.batch_running {
            return;
        }
        if self.batch_queue.progress().queued == 0 {
            self.batch_status = Some("Nothing queued to convert.".into());
            cx.notify();
            return;
        }

        // Refresh destinations and options for remaining queued items.
        // Override items keep their pinned format; inherit items follow global.
        let options = match self.build_conversion_options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.batch_status = Some(error.into());
                cx.notify();
                return;
            }
        };
        self.batch_queue
            .refresh_inherited_formats(self.output_format, self.batch_output_dir.as_deref());
        for item in self.batch_queue.items_mut() {
            if matches!(item.state, BatchItemState::Queued) {
                item.options = options.clone();
                item.force = self.batch_force;
            }
        }
        self.batch_item_progress.clear();

        // Fresh cancel flag per run so a prior Clear/Cancel cannot be undone by
        // a later start, and so an abandoned worker keeps its own flag.
        self.batch_cancel = Arc::new(AtomicBool::new(false));
        self.batch_running = true;
        self.batch_generation = self.batch_generation.wrapping_add(1);
        let generation = self.batch_generation;
        let priority = self.module_priority.clone();
        let cancel = Arc::clone(&self.batch_cancel);
        let mut queue = self.batch_queue.clone();
        self.batch_status = Some("Batch running…".into());
        cx.notify();

        let (event_tx, event_rx) = std::sync::mpsc::channel::<BatchEvent>();
        let (done_tx, done_rx) =
            std::sync::mpsc::channel::<(BatchQueue, shift_core::conversion::BatchSummary)>();

        // Blocking convert/write work on GPUI's background executor (not a raw thread).
        cx.background_executor()
            .spawn(async move {
                let registry = ConversionRegistry::default().with_priority(&priority);
                let summary = run_batch(&mut queue, &registry, &cancel, |event| {
                    let _ = event_tx.send(event);
                });
                let _ = done_tx.send((queue, summary));
            })
            .detach();

        cx.spawn(async move |this, cx| {
            loop {
                // Drain progress events without blocking the UI thread forever.
                let mut drained = 0;
                while let Ok(event) = event_rx.try_recv() {
                    drained += 1;
                    let _ = this.update(cx, |this, cx| {
                        if this.batch_generation != generation {
                            return;
                        }
                        this.apply_batch_event(event);
                        cx.notify();
                    });
                }
                if let Ok((queue, summary)) = done_rx.try_recv() {
                    // Apply any trailing events first.
                    while let Ok(event) = event_rx.try_recv() {
                        let _ = this.update(cx, |this, cx| {
                            if this.batch_generation == generation {
                                this.apply_batch_event(event);
                                cx.notify();
                            }
                        });
                    }
                    let _ = this.update(cx, |this, cx| {
                        if this.batch_generation != generation {
                            // Abandoned by Clear: release the running lock so a new
                            // batch can start. Do not restore the worker's queue onto
                            // the UI (user already cleared it).
                            this.batch_running = false;
                            if this
                                .batch_status
                                .as_ref()
                                .is_some_and(|s| s.as_ref().starts_with("Clearing"))
                            {
                                this.batch_status = Some("Queue cleared.".into());
                            }
                            cx.notify();
                            return;
                        }
                        this.batch_queue = queue;
                        this.batch_running = false;
                        this.batch_status = Some(
                            format!(
                                "Batch complete: {} succeeded, {} failed, {} cancelled",
                                summary.succeeded, summary.failed, summary.cancelled
                            )
                            .into(),
                        );
                        cx.notify();
                    });
                    break;
                }
                if drained == 0 {
                    cx.background_executor()
                        .timer(Duration::from_millis(40))
                        .await;
                }
            }
        })
        .detach();
    }

    fn toggle_batch_force(&mut self, cx: &mut Context<Self>) {
        if self.batch_running {
            self.batch_status = Some("Cannot change Overwrite while a batch is running.".into());
            cx.notify();
            return;
        }
        self.batch_force = !self.batch_force;
        // Keep already-queued items in sync with the toggle.
        for item in self.batch_queue.items_mut() {
            if matches!(item.state, BatchItemState::Queued) {
                item.force = self.batch_force;
            }
        }
        self.batch_status = Some(
            if self.batch_force {
                "Overwrite existing outputs: on (CLI --force)."
            } else {
                "Overwrite existing outputs: off."
            }
            .into(),
        );
        self.persist_session_settings(cx);
        cx.notify();
    }

    fn apply_batch_event(&mut self, event: BatchEvent) {
        match event {
            BatchEvent::ItemStarted { id, .. } => {
                if let Some(item) = self.batch_queue.get_mut(id) {
                    // attempts is owned by the worker; only mirror Running here.
                    item.state = BatchItemState::Running;
                }
                self.batch_status = Some("Converting…".into());
            }
            BatchEvent::ItemSucceeded {
                id,
                path,
                module_id,
                byte_len,
                source_name,
            } => {
                if let Some(item) = self.batch_queue.get_mut(id) {
                    item.state = BatchItemState::Succeeded {
                        written_path: path.clone(),
                        module_id: module_id.clone(),
                        byte_len,
                    };
                    item.destination = path.clone();
                }
                self.batch_status =
                    Some(format!("Saved {source_name} → {}", path.display()).into());
            }
            BatchEvent::ItemFailed {
                id,
                error,
                source_name,
            } => {
                if let Some(item) = self.batch_queue.get_mut(id) {
                    item.state = BatchItemState::Failed {
                        error: error.clone(),
                    };
                }
                self.batch_status = Some(format!("Failed {source_name}: {error}").into());
            }
            BatchEvent::ItemCancelled { id, source_name } => {
                if let Some(item) = self.batch_queue.get_mut(id) {
                    item.state = BatchItemState::Cancelled;
                }
                self.batch_status = Some(format!("Cancelled {source_name}").into());
            }
            BatchEvent::ItemProgress {
                id,
                fraction,
                label,
            } => {
                self.batch_item_progress
                    .insert(id.0, (fraction, label.clone().into()));
                self.batch_status = Some(match fraction {
                    Some(value) => format!("{label} ({:.0}%)", value * 100.0).into(),
                    None => label.into(),
                });
            }
            BatchEvent::Progress(progress) => {
                self.batch_status = Some(
                    format!(
                        "{}/{} · {} ok · {} failed · {} cancelled",
                        progress.completed(),
                        progress.total,
                        progress.succeeded,
                        progress.failed,
                        progress.cancelled
                    )
                    .into(),
                );
            }
        }
    }

    fn cancel_batch(&mut self, cx: &mut Context<Self>) {
        self.batch_cancel.store(true, Ordering::SeqCst);
        if !self.batch_running {
            let n = self.batch_queue.cancel_queued();
            self.batch_status = Some(format!("Cancelled {n} queued item(s)").into());
        } else {
            self.batch_status = Some("Cancelling batch…".into());
        }
        cx.notify();
    }

    fn retry_batch_item(&mut self, id: BatchItemId, cx: &mut Context<Self>) {
        if self.batch_queue.retry(id) {
            self.batch_status = Some("Item re-queued.".into());
            cx.notify();
            if !self.batch_running {
                self.start_batch(cx);
            }
        }
    }

    fn retry_failed_batch(&mut self, cx: &mut Context<Self>) {
        let n = self.batch_queue.retry_failed();
        self.batch_status = Some(format!("Re-queued {n} item(s)").into());
        cx.notify();
        if n > 0 && !self.batch_running {
            self.start_batch(cx);
        }
    }

    fn clear_batch_queue(&mut self, cx: &mut Context<Self>) {
        if self.batch_running {
            self.batch_cancel.store(true, Ordering::SeqCst);
            // Discard the worker result so Clear is durable when the run finishes.
            // Keep batch_running true until that worker exits so start_batch cannot
            // spawn a second concurrent run_batch against overlapping destinations.
            self.batch_generation = self.batch_generation.wrapping_add(1);
            self.batch_status = Some("Clearing queue…".into());
        } else {
            self.batch_status = None;
        }
        self.batch_queue.clear();
        cx.notify();
    }

    fn set_selected_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        file_picker::remember_directory(&path);
        self.cancel_active_conversion();
        self.ensure_diagnostics(cx);
        self.selection_generation = self.selection_generation.wrapping_add(1);
        let generation = self.selection_generation;

        self.selected_url = None;
        self.url_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.file_preview = Some(build_file_preview_with_size(&path, "…".into()));
        self.selected_file = Some(path.clone());
        let available_outputs = ConversionRegistry::default().available_outputs(&path);
        if !self.user_chose_format {
            let suggested = suggested_output_for_path(&path);
            if available_outputs.contains(&suggested) {
                self.output_format = suggested;
            } else if !available_outputs.contains(&self.output_format) {
                self.output_format = available_outputs
                    .first()
                    .copied()
                    .unwrap_or(OutputFormat::MARKDOWN);
            }
        } else if !available_outputs.contains(&self.output_format) {
            self.output_format = available_outputs
                .first()
                .copied()
                .unwrap_or(OutputFormat::MARKDOWN);
        }
        self.conversion = ConversionState::Converting;
        self.conversion_progress = None;
        self.cached_ready_path = None;
        self.show_command_inspect = false;
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
            self.cancel_active_conversion();
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

        self.cancel_active_conversion();
        self.ensure_diagnostics(cx);
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.selected_file = None;
        self.selected_url = Some(url.clone());
        self.file_preview = Some(build_url_preview(&url));
        self.url_input
            .update(cx, |input, cx| input.set_content(url.clone(), cx));

        let available_outputs = ConversionRegistry::default().available_url_outputs();
        if !self.user_chose_format {
            let suggested = suggested_output_for_url();
            if available_outputs.contains(&suggested) {
                self.output_format = suggested;
            } else if !available_outputs.contains(&self.output_format) {
                self.output_format = available_outputs
                    .first()
                    .copied()
                    .unwrap_or(OutputFormat::MARKDOWN);
            }
        } else if !available_outputs.contains(&self.output_format) {
            self.output_format = available_outputs
                .first()
                .copied()
                .unwrap_or(OutputFormat::MARKDOWN);
        }
        self.conversion = ConversionState::Converting;
        self.conversion_progress = None;
        self.cached_ready_path = None;
        self.show_command_inspect = false;
        self.save_status = None;
        self.output_menu_open = false;
        self.active_history_id = None;
        cx.notify();
        self.start_conversion(cx);
    }

    fn start_conversion(&mut self, cx: &mut Context<Self>) {
        // Multi-file work goes through start_batch / run_batch only.
        if !self.batch_queue.is_empty() {
            return;
        }
        // Keep session knobs durable when options change reconverts.
        self.persist_session_settings(cx);
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

    fn active_option_modules(&self) -> Vec<&'static str> {
        let registry = ConversionRegistry::default().with_priority(&self.module_priority);
        if self.selected_url.is_some() {
            return registry
                .url_route_module_ids(self.output_format)
                .unwrap_or_default();
        }
        if let Some(path) = self.selected_file.as_ref() {
            return registry
                .route_module_ids(path, self.output_format)
                .unwrap_or_default();
        }
        // No source yet: still surface FFmpeg knobs when the chosen output is media.
        if is_ffmpeg_output(self.output_format) {
            vec!["ffmpeg"]
        } else {
            Vec::new()
        }
    }

    fn conversion_options_visible(&self) -> bool {
        !self.active_option_modules().is_empty()
    }

    fn build_conversion_options(&self, cx: &App) -> Result<ConversionOptions, String> {
        let start_secs = parse_optional_secs(self.ffmpeg_start_input.read(cx).content())?;
        let duration_secs = parse_optional_secs(self.ffmpeg_duration_input.read(cx).content())?;
        let frame_secs = parse_optional_secs(self.ffmpeg_frame_input.read(cx).content())?;
        let frame_interval_secs =
            parse_optional_secs(self.ffmpeg_frame_interval_input.read(cx).content())?;
        let fps = parse_optional_secs(self.ffmpeg_fps_input.read(cx).content())?;
        let audio_stream = parse_optional_u32(self.ffmpeg_audio_stream_input.read(cx).content())?;
        let subtitle_stream =
            parse_optional_u32(self.ffmpeg_subtitle_stream_input.read(cx).content())?;
        let page_from = parse_optional_u32(self.pdf_page_from_input.read(cx).content())?;
        let page_to = parse_optional_u32(self.pdf_page_to_input.read(cx).content())?;
        let password = {
            let value = self.pdf_password_input.read(cx).content().trim().to_owned();
            if value.is_empty() { None } else { Some(value) }
        };
        let lang = self
            .defuddle_lang_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let defuddle_lang = if lang.is_empty() { None } else { Some(lang) };
        let ocr_lang = {
            let value = self
                .docling_ocr_lang_input
                .read(cx)
                .content()
                .trim()
                .to_owned();
            if value.is_empty() { None } else { Some(value) }
        };
        Ok(ConversionOptions {
            ffmpeg: FfmpegOptions {
                start_secs,
                duration_secs,
                frame_secs,
                frame_interval_secs,
                audio_stream,
                subtitle_stream,
                encode_mode: self.ffmpeg_encode_mode,
                quality: self.ffmpeg_quality,
                mono: self.ffmpeg_mono,
                sample_rate_hz: self.ffmpeg_sample_rate_hz,
                scale_width: self.ffmpeg_scale_width,
                fps,
                mute: self.ffmpeg_mute,
                normalize_audio: self.ffmpeg_normalize,
                burn_subtitles: self.ffmpeg_burn_subs,
            },
            markitdown: MarkItDownOptions {
                keep_data_uris: self.markitdown_keep_data_uris,
            },
            pandoc: PandocOptions {
                pdf_engine: self.pandoc_pdf_engine.clone(),
                standalone: self.pandoc_standalone,
                toc: self.pandoc_toc,
                reference_doc: self.pandoc_reference_doc.clone(),
            },
            defuddle: DefuddleOptions {
                frontmatter: self.defuddle_frontmatter,
                lang: defuddle_lang,
            },
            docling: DoclingOptions {
                image_export_mode: self.docling_images,
                ocr: self.docling_ocr,
                ocr_lang,
                tables: self.docling_tables,
                table_mode: self.docling_table_mode,
            },
            pdf: PdfInputOptions {
                password,
                page_from,
                page_to,
            },
            cancel: None,
            progress: None,
        })
    }

    fn persist_session_settings(&self, cx: &App) {
        let mut settings = load_default_session_settings();
        settings.set_output_format(self.output_format);
        settings.batch_output_dir = self.batch_output_dir.clone();
        settings.batch_force = self.batch_force;
        if let Ok(options) = self.build_conversion_options(cx) {
            settings.apply_conversion_options(&options);
        }
        let _ = save_default_session_settings(&settings);
    }

    fn install_hints_for_failure(&self) -> Vec<(SharedString, SharedString)> {
        let Some(report) = self.diagnostics.as_ref() else {
            return Vec::new();
        };
        let modules = self.active_option_modules();
        report
            .engines
            .iter()
            .filter(|engine| !engine.readiness.is_ready())
            .filter(|engine| modules.is_empty() || modules.contains(&engine.id))
            .map(|engine| {
                (
                    format!("Install {}", engine.label).into(),
                    engine.install_hint.clone().into(),
                )
            })
            .collect()
    }

    fn ensure_cached_ready_path(&mut self) -> Option<PathBuf> {
        if let Some(path) = self.cached_ready_path.clone() {
            if path.is_file() {
                return Some(path);
            }
        }
        let ConversionState::Ready(artifact) = &self.conversion else {
            return None;
        };
        match cache_artifact_bytes(&artifact.file_name, &artifact.bytes) {
            Ok(path) => {
                self.cached_ready_path = Some(path.clone());
                Some(path)
            }
            Err(error) => {
                self.save_status = Some(format!("Could not cache artifact: {error}").into());
                None
            }
        }
    }

    fn copy_output(&mut self, cx: &mut Context<Self>) {
        let ConversionState::Ready(artifact) = &self.conversion else {
            return;
        };
        if artifact.format.is_text_previewable() {
            if let Some(text) = artifact.text() {
                cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
                self.save_status = Some("Copied text to clipboard.".into());
                cx.notify();
                return;
            }
        }
        if let Some(path) = self.ensure_cached_ready_path() {
            cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
            self.save_status = Some("Copied artifact path to clipboard.".into());
            cx.notify();
        } else {
            cx.notify();
        }
    }

    fn reveal_output(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.ensure_cached_ready_path() {
            file_picker::reveal_in_finder(&path);
            self.save_status = Some("Revealed in Finder.".into());
        }
        cx.notify();
    }

    fn open_output(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.ensure_cached_ready_path() {
            file_picker::open_path(&path);
            self.save_status = Some("Opened with default app.".into());
        }
        cx.notify();
    }

    fn pick_reference_doc(&mut self, cx: &mut Context<Self>) {
        if file_picker::is_busy() {
            return;
        }
        let start_dir = self
            .pandoc_reference_doc
            .as_ref()
            .and_then(|path| path.parent().map(|p| p.to_path_buf()))
            .or_else(|| {
                self.selected_file
                    .as_ref()
                    .and_then(|path| path.parent().map(|p| p.to_path_buf()))
            });
        let receiver = file_picker::pick_file(start_dir);
        cx.spawn(async move |this, cx| {
            let path = receiver.await.ok().flatten();
            let _ = this.update(cx, |this, cx| {
                if let Some(path) = path {
                    this.pandoc_reference_doc = Some(path);
                    this.persist_session_settings(cx);
                    this.start_conversion(cx);
                } else {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Signal any in-flight single conversion to abort its external process.
    fn cancel_active_conversion(&mut self) {
        self.conversion_cancel.store(true, Ordering::SeqCst);
    }

    /// User-facing cancel for the active single-file / single-URL conversion.
    fn cancel_conversion(&mut self, cx: &mut Context<Self>) {
        if matches!(self.conversion, ConversionState::Converting) {
            self.cancel_active_conversion();
            self.conversion_generation = self.conversion_generation.wrapping_add(1);
            self.conversion = ConversionState::Failed("Conversion cancelled.".into());
            self.conversion_progress = None;
            self.save_status = None;
            cx.notify();
            return;
        }
        if self.batch_running || !self.batch_queue.is_empty() {
            self.cancel_batch(cx);
        }
    }

    fn apply_conversion_options(&mut self, cx: &mut Context<Self>) {
        self.persist_session_settings(cx);
        self.start_conversion(cx);
    }

    /// Apply a session option change from Settings (reconvert only when relevant).
    fn apply_session_option_change(&mut self, cx: &mut Context<Self>) {
        self.persist_session_settings(cx);
        if self.conversion_options_visible() {
            self.start_conversion(cx);
        } else {
            cx.notify();
        }
    }

    fn start_file_conversion(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // Kill any previous single convert before starting a new one.
        self.cancel_active_conversion();
        self.conversion_cancel = Arc::new(AtomicBool::new(false));
        self.conversion_generation = self.conversion_generation.wrapping_add(1);
        let conversion_generation = self.conversion_generation;
        let generation = self.selection_generation;
        let output_format = self.output_format;
        let priority = self.module_priority.clone();
        let mut options = match self.build_conversion_options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.conversion = ConversionState::Failed(error.into());
                cx.notify();
                return;
            }
        };
        options.cancel = Some(Arc::clone(&self.conversion_cancel));
        let (progress_tx, progress_rx) = std::sync::mpsc::channel::<ConversionProgress>();
        options.progress = Some(Arc::new(move |progress| {
            let _ = progress_tx.send(progress);
        }));
        self.conversion = ConversionState::Converting;
        self.conversion_progress = Some((
            None,
            format!("Converting to {}…", output_format.label()).into(),
        ));
        self.cached_ready_path = None;
        self.save_status = None;
        self.active_history_id = None;
        cx.notify();

        let conversion_path = path.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        cx.background_executor()
            .spawn(async move {
                let result = ConversionRegistry::default()
                    .with_priority(&priority)
                    .convert_to_with_options(&conversion_path, output_format, &options);
                let _ = done_tx.send(result);
            })
            .detach();

        cx.spawn(async move |this, cx| {
            loop {
                while let Ok(progress) = progress_rx.try_recv() {
                    let _ = this.update(cx, |this, cx| {
                        if this.selection_generation != generation
                            || this.conversion_generation != conversion_generation
                        {
                            return;
                        }
                        this.conversion_progress = Some(match progress {
                            ConversionProgress::Phase(label) => (None, label.into()),
                            ConversionProgress::Fraction { fraction, label } => {
                                (Some(fraction), label.into())
                            }
                        });
                        cx.notify();
                    });
                }
                if let Ok(result) = done_rx.try_recv() {
                    let _ = this.update(cx, |this, cx| {
                        if this.selection_generation == generation
                            && this.conversion_generation == conversion_generation
                            && this.selected_file.as_ref() == Some(&path)
                        {
                            this.conversion_progress = None;
                            this.conversion = match result {
                                Ok(artifact) => {
                                    let artifact = Arc::new(artifact);
                                    this.record_history(HistoryOutcome::Ready(Arc::clone(
                                        &artifact,
                                    )));
                                    ConversionState::Ready(artifact)
                                }
                                Err(error) if error.is_cancelled() => {
                                    ConversionState::Failed("Conversion cancelled.".into())
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
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
            }
        })
        .detach();
    }

    fn start_url_conversion(&mut self, url: String, cx: &mut Context<Self>) {
        self.cancel_active_conversion();
        self.conversion_cancel = Arc::new(AtomicBool::new(false));
        self.conversion_generation = self.conversion_generation.wrapping_add(1);
        let conversion_generation = self.conversion_generation;
        let generation = self.selection_generation;
        let output_format = self.output_format;
        let priority = self.module_priority.clone();
        let mut options = match self.build_conversion_options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.conversion = ConversionState::Failed(error.into());
                cx.notify();
                return;
            }
        };
        options.cancel = Some(Arc::clone(&self.conversion_cancel));
        let (progress_tx, progress_rx) = std::sync::mpsc::channel::<ConversionProgress>();
        options.progress = Some(Arc::new(move |progress| {
            let _ = progress_tx.send(progress);
        }));
        self.conversion = ConversionState::Converting;
        self.conversion_progress = Some((
            None,
            format!("Converting to {}…", output_format.label()).into(),
        ));
        self.cached_ready_path = None;
        self.save_status = None;
        self.active_history_id = None;
        cx.notify();

        let conversion_url = url.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        cx.background_executor()
            .spawn(async move {
                let result = ConversionRegistry::default()
                    .with_priority(&priority)
                    .convert_url_with_options(&conversion_url, output_format, &options);
                let _ = done_tx.send(result);
            })
            .detach();

        cx.spawn(async move |this, cx| {
            loop {
                while let Ok(progress) = progress_rx.try_recv() {
                    let _ = this.update(cx, |this, cx| {
                        if this.selection_generation != generation
                            || this.conversion_generation != conversion_generation
                        {
                            return;
                        }
                        this.conversion_progress = Some(match progress {
                            ConversionProgress::Phase(label) => (None, label.into()),
                            ConversionProgress::Fraction { fraction, label } => {
                                (Some(fraction), label.into())
                            }
                        });
                        cx.notify();
                    });
                }
                if let Ok(result) = done_rx.try_recv() {
                    let _ = this.update(cx, |this, cx| {
                        if this.selection_generation == generation
                            && this.conversion_generation == conversion_generation
                            && this.selected_url.as_ref() == Some(&url)
                        {
                            this.conversion_progress = None;
                            this.conversion = match result {
                                Ok(artifact) => {
                                    let artifact = Arc::new(artifact);
                                    this.record_history(HistoryOutcome::Ready(Arc::clone(
                                        &artifact,
                                    )));
                                    ConversionState::Ready(artifact)
                                }
                                Err(error) if error.is_cancelled() => {
                                    ConversionState::Failed("Conversion cancelled.".into())
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
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
            }
        })
        .detach();
    }

    fn set_output_format(&mut self, format: OutputFormat, cx: &mut Context<Self>) {
        self.output_menu_open = false;
        self.user_chose_format = true;
        if self.output_format == format {
            cx.notify();
            return;
        }
        if self.batch_running {
            self.batch_status =
                Some("Cannot change output format while a batch is running.".into());
            cx.notify();
            return;
        }
        self.output_format = format;
        self.persist_session_settings(cx);
        if !self.batch_queue.is_empty() {
            self.batch_queue
                .set_output_format_for_queued(format, self.batch_output_dir.as_deref());
            self.batch_status = Some(format!("Queued items updated to {}.", format.label()).into());
            cx.notify();
            // Multi-file mode uses Start / run_batch, not single-file conversion.
            return;
        }
        self.start_conversion(cx);
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
        self.cancel_active_conversion();
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.selected_file = None;
        self.selected_url = None;
        self.url_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.file_preview = None;
        self.conversion = ConversionState::Empty;
        self.conversion_progress = None;
        self.cached_ready_path = None;
        self.show_command_inspect = false;
        self.save_status = None;
        self.output_menu_open = false;
        self.active_history_id = None;
        cx.notify();
    }

    fn action_save_output(&mut self, _: &SaveOutput, _window: &mut Window, cx: &mut Context<Self>) {
        self.save_output(cx);
    }

    fn action_copy_output(&mut self, _: &CopyOutput, _window: &mut Window, cx: &mut Context<Self>) {
        self.copy_output(cx);
    }

    fn action_reveal_output(
        &mut self,
        _: &RevealOutput,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reveal_output(cx);
    }

    fn action_toggle_format(
        &mut self,
        _: &ToggleFormatMenu,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.output_menu_open = !self.output_menu_open;
        cx.notify();
    }

    fn action_open_settings(
        &mut self,
        _: &OpenSettings,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.output_menu_open = false;
        self.settings_open = !self.settings_open;
        if self.settings_open {
            self.ensure_diagnostics(cx);
        }
        cx.notify();
    }

    fn action_show_shortcuts(
        &mut self,
        _: &ShowShortcuts,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shortcuts_help_open = !self.shortcuts_help_open;
        cx.notify();
    }

    fn action_cancel_work(&mut self, _: &CancelWork, _window: &mut Window, cx: &mut Context<Self>) {
        if self.shortcuts_help_open {
            self.shortcuts_help_open = false;
            cx.notify();
            return;
        }
        if self.settings_open {
            self.settings_open = false;
            cx.notify();
            return;
        }
        if self.folder_confirm.is_some() {
            self.dismiss_folder_confirm(cx);
            return;
        }
        if self.output_menu_open {
            self.output_menu_open = false;
            cx.notify();
            return;
        }
        self.cancel_conversion(cx);
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

        // Cap retained payload so media conversions cannot pin gigabytes in history.
        let outcome = match outcome {
            HistoryOutcome::Ready(artifact)
                if artifact.bytes.len() > MAX_HISTORY_ARTIFACT_BYTES =>
            {
                HistoryOutcome::ReadyLarge {
                    module_id: artifact.module_id.into(),
                    byte_len: artifact.bytes.len(),
                }
            }
            other => other,
        };

        let detail = match &outcome {
            HistoryOutcome::Ready(artifact) => format!(
                "{}  ·  via {}",
                artifact.format.label(),
                module_label(artifact.module_id)
            ),
            HistoryOutcome::ReadyLarge {
                module_id,
                byte_len,
                ..
            } => format!(
                "{}  ·  {}  ·  via {} (re-convert to restore)",
                self.output_format.label(),
                format_file_size(*byte_len as u64),
                module_label(module_id)
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
        self.persist_history();
    }

    fn persist_history(&self) {
        let stored: Vec<StoredHistoryEntry> = self.history.iter().map(to_stored_entry).collect();
        // Best-effort: keep the in-memory list if the disk write fails.
        let _ = save_history(&stored, self.next_history_id);
    }

    fn restore_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(entry) = self.history.iter().find(|entry| entry.id == id).cloned() else {
            return;
        };

        // Invalidate any in-flight work so a late conversion cannot overwrite
        // the restored snapshot.
        self.cancel_active_conversion();
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.conversion_generation = self.conversion_generation.wrapping_add(1);
        self.output_menu_open = false;
        self.save_status = None;
        self.output_format = entry.output_format;
        self.user_chose_format = true;
        self.cached_ready_path = None;
        self.conversion_progress = None;
        self.show_command_inspect = false;
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

        match entry.outcome {
            HistoryOutcome::Ready(artifact) => {
                self.conversion = ConversionState::Ready(artifact);
                cx.notify();
            }
            HistoryOutcome::ReadyLarge { .. } => {
                // Full bytes were not retained; re-run conversion for this source.
                self.conversion = ConversionState::Converting;
                cx.notify();
                self.start_conversion(cx);
            }
            HistoryOutcome::Failed(message) => {
                self.conversion = ConversionState::Failed(message);
                cx.notify();
            }
        }
    }

    fn clear_history(&mut self, cx: &mut Context<Self>) {
        self.history.clear();
        self.active_history_id = None;
        let _ = clear_history_store();
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
        let source_path = self.selected_file.clone();
        let directory = source_path
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

            // Mirror CLI / batch: never overwrite the selected source file.
            if let Some(source) = source_path.as_ref() {
                if paths_refer_to_same_file(source, &path) {
                    let _ = this.update(cx, |this, cx| {
                        if this.selection_generation != selection_generation
                            || this.conversion_generation != conversion_generation
                        {
                            return;
                        }
                        this.save_status = Some(
                            format!(
                                "Refusing to overwrite source file {} — choose a different path.",
                                source.display()
                            )
                            .into(),
                        );
                        cx.notify();
                    });
                    return;
                }
            }

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
        let registry = ConversionRegistry::default();
        let available_outputs = if self.selected_url.is_some() {
            registry.available_url_outputs()
        } else if let Some(path) = self.selected_file.as_ref() {
            registry.available_outputs(path)
        } else {
            OutputFormat::ALL.to_vec()
        };
        // When diagnostics are loaded for a concrete source, badge formats whose
        // engines are missing so users see install hints before converting.
        let ready_outputs = self.diagnostics.as_ref().and_then(|report| {
            if self.selected_url.is_some() {
                Some(available_ready_url_outputs(&registry, report))
            } else {
                self.selected_file
                    .as_ref()
                    .map(|path| available_ready_outputs(&registry, report, path))
            }
        });
        let output_format = self.output_format;
        let output_menu_open = self.output_menu_open;
        let format_filter_input = self.format_filter_input.clone();
        let format_filter = self.format_filter_input.read(cx).content().to_owned();
        let settings_open = self.settings_open;
        let shortcuts_help_open = self.shortcuts_help_open;
        let show_command_inspect = self.show_command_inspect;
        let conversion_progress = self.conversion_progress.clone();
        let install_hints = if matches!(self.conversion, ConversionState::Failed(_)) {
            self.install_hints_for_failure()
        } else {
            Vec::new()
        };
        let folder_confirm = self.folder_confirm.clone();
        let settings_section = self.settings_section;
        let module_priority = self.module_priority.clone();
        let preference_error = self.preference_error.clone();
        let url_input = self.url_input.clone();
        let history = self.history.clone();
        let history_count = history.len();
        let active_history_id = self.active_history_id;
        let active_option_modules = self.active_option_modules();
        let show_conversion_options = !active_option_modules.is_empty();
        let ffmpeg_quality = self.ffmpeg_quality;
        let ffmpeg_encode_mode = self.ffmpeg_encode_mode;
        let ffmpeg_mono = self.ffmpeg_mono;
        let ffmpeg_mute = self.ffmpeg_mute;
        let ffmpeg_normalize = self.ffmpeg_normalize;
        let ffmpeg_burn_subs = self.ffmpeg_burn_subs;
        let ffmpeg_sample_rate_hz = self.ffmpeg_sample_rate_hz;
        let ffmpeg_scale_width = self.ffmpeg_scale_width;
        let ffmpeg_start_input = self.ffmpeg_start_input.clone();
        let ffmpeg_duration_input = self.ffmpeg_duration_input.clone();
        let ffmpeg_frame_input = self.ffmpeg_frame_input.clone();
        let ffmpeg_fps_input = self.ffmpeg_fps_input.clone();
        let ffmpeg_frame_interval_input = self.ffmpeg_frame_interval_input.clone();
        let ffmpeg_audio_stream_input = self.ffmpeg_audio_stream_input.clone();
        let ffmpeg_subtitle_stream_input = self.ffmpeg_subtitle_stream_input.clone();
        let docling_images = self.docling_images;
        let docling_ocr = self.docling_ocr;
        let docling_tables = self.docling_tables;
        let docling_table_mode = self.docling_table_mode;
        let docling_ocr_lang_input = self.docling_ocr_lang_input.clone();
        let defuddle_frontmatter = self.defuddle_frontmatter;
        let defuddle_lang_input = self.defuddle_lang_input.clone();
        let pandoc_standalone = self.pandoc_standalone;
        let pandoc_toc = self.pandoc_toc;
        let pandoc_pdf_engine = self.pandoc_pdf_engine.clone();
        let pandoc_reference_doc = self.pandoc_reference_doc.clone();
        let pdf_page_from_input = self.pdf_page_from_input.clone();
        let pdf_page_to_input = self.pdf_page_to_input.clone();
        let pdf_password_input = self.pdf_password_input.clone();
        let markitdown_keep_data_uris = self.markitdown_keep_data_uris;
        let diagnostics = self.diagnostics.clone();
        let diagnostics_loading = self.diagnostics_loading;
        let batch_items = self.batch_queue.items().to_vec();
        let show_batch = !batch_items.is_empty();
        let batch_output_dir = self.batch_output_dir.clone();
        let batch_running = self.batch_running;
        let batch_force = self.batch_force;
        let batch_status = self.batch_status.clone();
        let batch_item_progress = self.batch_item_progress.clone();
        let focus_handle = self.focus_handle.clone();

        div()
            .id("shift-root")
            .key_context("Shift")
            .track_focus(&focus_handle)
            .on_action(cx.listener(Self::action_save_output))
            .on_action(cx.listener(Self::action_copy_output))
            .on_action(cx.listener(Self::action_reveal_output))
            .on_action(cx.listener(Self::action_toggle_format))
            .on_action(cx.listener(Self::action_open_settings))
            .on_action(cx.listener(Self::action_show_shortcuts))
            .on_action(cx.listener(Self::action_cancel_work))
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
                            .child(rounded_dashed_border(has_selection || show_batch))
                            .when_some(preview, |zone, preview| {
                                zone.child(file_preview_card(preview, cx))
                            })
                            .when(!has_selection && !show_batch, |zone| {
                                zone.child(empty_drop_prompt())
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx)))
                            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                                let paths = paths.paths().to_vec();
                                this.ingest_paths(paths, cx);
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
                    .when(show_batch, |panel| {
                        panel.child(batch_queue_panel(
                            &batch_items,
                            batch_output_dir.as_deref(),
                            batch_running,
                            batch_force,
                            batch_status,
                            &batch_item_progress,
                            cx,
                        ))
                    })
                    .when(!show_batch, |panel| {
                        panel.child(output_panel(
                            OutputPanelView {
                                state: conversion,
                                save_status,
                                output_format,
                                output_menu_open,
                                format_filter_input,
                                format_filter,
                                available_outputs,
                                ready_outputs,
                                show_conversion_options,
                                conversion_options: ConversionPanelView {
                                    active_modules: active_option_modules,
                                    output_format,
                                    quality: ffmpeg_quality,
                                    encode_mode: ffmpeg_encode_mode,
                                    mono: ffmpeg_mono,
                                    mute: ffmpeg_mute,
                                    normalize_audio: ffmpeg_normalize,
                                    burn_subtitles: ffmpeg_burn_subs,
                                    sample_rate_hz: ffmpeg_sample_rate_hz,
                                    scale_width: ffmpeg_scale_width,
                                    start_input: ffmpeg_start_input,
                                    duration_input: ffmpeg_duration_input,
                                    frame_input: ffmpeg_frame_input,
                                    fps_input: ffmpeg_fps_input,
                                    frame_interval_input: ffmpeg_frame_interval_input,
                                    audio_stream_input: ffmpeg_audio_stream_input,
                                    subtitle_stream_input: ffmpeg_subtitle_stream_input,
                                    docling_images,
                                    docling_ocr,
                                    docling_tables,
                                    docling_table_mode,
                                    docling_ocr_lang_input,
                                    defuddle_frontmatter,
                                    defuddle_lang_input,
                                    pandoc_standalone,
                                    pandoc_toc,
                                    pandoc_pdf_engine,
                                    pandoc_reference_doc,
                                    pdf_page_from_input,
                                    pdf_page_to_input,
                                    pdf_password_input,
                                    markitdown_keep_data_uris,
                                },
                                conversion_progress,
                                show_command_inspect,
                                install_hints,
                            },
                            cx,
                        ))
                    }),
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
                        this.ensure_diagnostics(cx);
                        cx.notify();
                    })),
            )
            .when_some(folder_confirm, |root, confirm| {
                let count = confirm.expanded.len();
                root.child(
                    div()
                        .id("folder-confirm-overlay")
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(hsla(0.0, 0.0, 0.0, 0.72))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.dismiss_folder_confirm(cx);
                            cx.stop_propagation();
                        }))
                        .child(
                            div()
                                .id("folder-confirm-dialog")
                                .w(px(420.0))
                                .p_6()
                                .rounded_xl()
                                .bg(rgb(BG_ELEVATED))
                                .border_1()
                                .border_color(rgb(BORDER_STRONG))
                                .flex()
                                .flex_col()
                                .gap_4()
                                .on_click(|_, _, cx| cx.stop_propagation())
                                .child(
                                    div()
                                        .text_lg()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Expand folders?"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(TEXT_SECONDARY))
                                        .child(format!(
                                            "Queue {count} convertible file(s) (cap {MAX_EXPAND_FILES})."
                                        )),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .gap_2()
                                        .justify_end()
                                        .child(action_chip(
                                            "folder-confirm-cancel",
                                            "Cancel",
                                            cx,
                                            |this, cx| this.dismiss_folder_confirm(cx),
                                        ))
                                        .child(action_chip(
                                            "folder-confirm-ok",
                                            "Queue files",
                                            cx,
                                            |this, cx| this.confirm_folder_expand(cx),
                                        )),
                                ),
                        ),
                )
            })
            .when(shortcuts_help_open, |root| {
                root.child(
                    div()
                        .id("shortcuts-help-overlay")
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(hsla(0.0, 0.0, 0.0, 0.72))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.shortcuts_help_open = false;
                            cx.notify();
                            cx.stop_propagation();
                        }))
                        .child(
                            div()
                                .id("shortcuts-help-dialog")
                                .w(px(420.0))
                                .p_6()
                                .rounded_xl()
                                .bg(rgb(BG_ELEVATED))
                                .border_1()
                                .border_color(rgb(BORDER_STRONG))
                                .flex()
                                .flex_col()
                                .gap_3()
                                .on_click(|_, _, cx| cx.stop_propagation())
                                .child(
                                    div()
                                        .text_lg()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("Keyboard shortcuts"),
                                )
                                .children(
                                    [
                                        ("⌘S", "Download / save output"),
                                        ("⌘C", "Copy output (text or path)"),
                                        ("⌘R", "Reveal output in Finder"),
                                        ("⌘⇧F", "Toggle output format menu"),
                                        ("⌘,", "Open settings"),
                                        ("⌘/", "Show this help"),
                                        ("Esc", "Cancel / close overlays"),
                                    ]
                                    .into_iter()
                                    .map(|(key, desc)| {
                                        div()
                                            .flex()
                                            .justify_between()
                                            .gap_4()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(rgb(TEXT_PRIMARY))
                                                    .child(key),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(rgb(TEXT_SECONDARY))
                                                    .child(desc),
                                            )
                                    }),
                                ),
                        ),
                )
            })
            .when(settings_open, |root| {
                root.child(settings_screen(
                    SettingsView {
                        section: settings_section,
                        priority: module_priority,
                        preference_error,
                        output_format,
                        history_count,
                        quality: ffmpeg_quality,
                        encode_mode: ffmpeg_encode_mode,
                        mono: ffmpeg_mono,
                        docling_images,
                        docling_ocr,
                        docling_tables,
                        docling_table_mode,
                        defuddle_frontmatter,
                        pandoc_standalone,
                        pandoc_toc,
                        markitdown_keep_data_uris,
                        diagnostics,
                        diagnostics_loading,
                    },
                    cx,
                ))
            })
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-s", SaveOutput, Some("Shift")),
            KeyBinding::new("cmd-c", CopyOutput, Some("Shift")),
            KeyBinding::new("cmd-r", RevealOutput, Some("Shift")),
            KeyBinding::new("cmd-shift-f", ToggleFormatMenu, Some("Shift")),
            KeyBinding::new("cmd-,", OpenSettings, Some("Shift")),
            KeyBinding::new("cmd-/", ShowShortcuts, Some("Shift")),
            KeyBinding::new("escape", CancelWork, Some("Shift")),
        ]);
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
            |window, cx| {
                let shift_entity = cx.new(|cx| {
                    let session = load_default_session_settings();
                    let options = session.to_conversion_options();
                    let url_input = cx.new(|cx| TextInput::new(cx, "Paste a URL to extract…", ""));
                    let format_filter_input =
                        cx.new(|cx| TextInput::new(cx, "Filter formats…", ""));
                    let ffmpeg_start_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "0",
                            options
                                .ffmpeg
                                .start_secs
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        )
                    });
                    let ffmpeg_duration_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "optional",
                            options
                                .ffmpeg
                                .duration_secs
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        )
                    });
                    let ffmpeg_frame_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "0",
                            options
                                .ffmpeg
                                .frame_secs
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        )
                    });
                    let ffmpeg_fps_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "optional",
                            options
                                .ffmpeg
                                .fps
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        )
                    });
                    let ffmpeg_frame_interval_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "1.0",
                            options
                                .ffmpeg
                                .frame_interval_secs
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        )
                    });
                    let ffmpeg_audio_stream_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "0",
                            options
                                .ffmpeg
                                .audio_stream
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        )
                    });
                    let ffmpeg_subtitle_stream_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "0",
                            options
                                .ffmpeg
                                .subtitle_stream
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        )
                    });
                    let docling_ocr_lang_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "e.g. eng",
                            options.docling.ocr_lang.clone().unwrap_or_default(),
                        )
                    });
                    let defuddle_lang_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "optional, e.g. en",
                            options.defuddle.lang.clone().unwrap_or_default(),
                        )
                    });
                    let pdf_page_from_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "from",
                            options
                                .pdf
                                .page_from
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        )
                    });
                    let pdf_page_to_input = cx.new(|cx| {
                        TextInput::new(
                            cx,
                            "to",
                            options
                                .pdf
                                .page_to
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        )
                    });
                    let pdf_password_input =
                        cx.new(|cx| TextInput::new(cx, "password (not saved)", ""));
                    let (history, next_history_id) = history_from_store(load_history());
                    let mut batch_queue = BatchQueue::new();
                    if let Some(dir) = session.batch_output_dir.as_ref() {
                        batch_queue.set_output_dir(Some(dir.as_path()));
                    }
                    let focus_handle = cx.focus_handle();
                    Shift {
                        focus_handle,
                        selected_file: None,
                        selected_url: None,
                        file_preview: None,
                        selection_generation: 0,
                        conversion_generation: 0,
                        conversion: ConversionState::Empty,
                        save_status: None,
                        preference_error: None,
                        output_format: session.output_format(),
                        user_chose_format: false,
                        output_menu_open: false,
                        format_filter_input,
                        settings_open: false,
                        settings_section: SettingsSection::Converters,
                        shortcuts_help_open: false,
                        show_command_inspect: false,
                        module_priority: load_module_priority(),
                        diagnostics: None,
                        diagnostics_loading: false,
                        url_input,
                        history,
                        next_history_id,
                        active_history_id: None,
                        batch_queue,
                        batch_output_dir: session.batch_output_dir.clone(),
                        batch_running: false,
                        batch_generation: 0,
                        batch_cancel: Arc::new(AtomicBool::new(false)),
                        batch_status: None,
                        batch_force: session.batch_force,
                        batch_item_progress: HashMap::new(),
                        folder_confirm: None,
                        conversion_cancel: Arc::new(AtomicBool::new(false)),
                        conversion_progress: None,
                        cached_ready_path: None,
                        ffmpeg_quality: options.ffmpeg.quality,
                        ffmpeg_encode_mode: options.ffmpeg.encode_mode,
                        ffmpeg_mono: options.ffmpeg.mono,
                        ffmpeg_mute: options.ffmpeg.mute,
                        ffmpeg_normalize: options.ffmpeg.normalize_audio,
                        ffmpeg_burn_subs: options.ffmpeg.burn_subtitles,
                        ffmpeg_sample_rate_hz: options.ffmpeg.sample_rate_hz,
                        ffmpeg_scale_width: options.ffmpeg.scale_width,
                        ffmpeg_start_input,
                        ffmpeg_duration_input,
                        ffmpeg_frame_input,
                        ffmpeg_fps_input,
                        ffmpeg_frame_interval_input,
                        ffmpeg_audio_stream_input,
                        ffmpeg_subtitle_stream_input,
                        docling_images: options.docling.image_export_mode,
                        docling_ocr: options.docling.ocr,
                        docling_tables: options.docling.tables,
                        docling_table_mode: options.docling.table_mode,
                        docling_ocr_lang_input,
                        defuddle_frontmatter: options.defuddle.frontmatter,
                        defuddle_lang_input,
                        pandoc_standalone: options.pandoc.standalone,
                        pandoc_toc: options.pandoc.toc,
                        pandoc_pdf_engine: options.pandoc.pdf_engine.clone(),
                        pandoc_reference_doc: options.pandoc.reference_doc.clone(),
                        pdf_page_from_input,
                        pdf_page_to_input,
                        pdf_password_input,
                        markitdown_keep_data_uris: options.markitdown.keep_data_uris,
                    }
                });

                window.focus(&shift_entity.read(cx).focus_handle);

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
        let _ = shift_core::purge_artifact_cache_defaults();

        cx.activate(true);
    });
}
