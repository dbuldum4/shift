mod app;
mod file_picker;
mod text_input;
mod ui;

#[cfg(test)]
mod ui_tests;

use crate::app::{ConversionHistoryEntry, ConversionState, HistoryOutcome, HistorySource, Shift};
use crate::ui::animation;
use crate::ui::theme::{THEME, card_shadow};
use gpui::{
    Action, Animation, AnimationExt, App, Application, Bounds, ClipboardEntry, ClipboardItem,
    Context, CursorStyle, ElementId, Entity, ExternalPaths, FocusHandle, Focusable, FontWeight,
    ImageFormat, KeyBinding, Menu, MenuItem, MouseButton, MouseDownEvent, MouseMoveEvent,
    PathBuilder, PathStyle, Pixels, Point, Render, SharedString, StrokeOptions, SystemMenuType,
    TitlebarOptions, WeakEntity, Window, WindowBounds, WindowOptions, actions, canvas, div,
    ease_out_quint, point, prelude::*, pulsating_between, px, rgb, size,
};
use shift_core::conversion::{
    BatchEnqueueOptions, BatchEvent, BatchFormatSelection, BatchItem, BatchItemId, BatchItemState,
    BatchQueue, BatchSource, ConversionArtifact, ConversionOptions, ConversionProgress,
    ConversionRegistry, DefuddleOptions, DiagnosticsReport, DoclingImageExportMode, DoclingOptions,
    DoclingTableMode, FfmpegEncodeMode, FfmpegOptions, FfmpegQuality, MAX_EXPAND_FILES, MagicPaste,
    MarkItDownOptions, OutputFormat, PandocOptions, PasteToken, PdfInputOptions, Readiness,
    available_ready_outputs, available_ready_url_outputs, expand_input_paths, is_audio_output,
    is_ffmpeg_output, is_image_output, is_subtitle_output, is_video_output, looks_like_url,
    materialize_magic_paste, parse_magic_paste, paths_refer_to_same_file, pdf_engine_candidates,
    run_batch, stage_pasted_image, suggested_output_for_path, suggested_output_for_url,
    url_display_host,
};
use shift_core::history::{
    LoadedHistory, MAX_HISTORY_ARTIFACT_BYTES, MAX_HISTORY_LIMIT, MIN_HISTORY_LIMIT,
    StoredHistoryEntry, StoredOutcome, StoredSource, history_db_path, intern_module_id,
    load_history, save_history_delta_to,
};
use shift_core::preferences::{load_module_priority, save_module_priority};
use shift_core::{
    cache_artifact_bytes, export_matches_bytes, load_default_session_settings,
    save_default_session_settings, stage_export_file,
};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use text_input::TextInput;

pub(crate) const APP_NAME: &str = "Shift";
/// Default UI font for the app chrome; persisted settings fall back to this value.
pub(crate) const DEFAULT_UI_FONT: &str = shift_core::session_settings::DEFAULT_UI_FONT_FAMILY;
/// Monospace accent font, used for code-like labels (file drag ghosts, etc.).
pub(crate) const FONT_MONO: &str = "Geist Mono";
/// Curated font families for Theme settings (label, family name).
/// Family names must match what Core Text / GPUI resolve on macOS.
/// Bundled and system UI font choices. Bundled Geist fonts are registered in `main`.
pub(crate) const UI_FONT_CHOICES: &[(&str, &str)] = &[
    ("Geist", "Geist"),
    ("Geist Mono", "Geist Mono"),
    ("System", ".SystemUIFont"),
    ("Menlo", "Menlo"),
    ("SF Mono", "SF Mono"),
    ("Monaco", "Monaco"),
    ("Courier New", "Courier New"),
    ("Andale Mono", "Andale Mono"),
    ("Helvetica Neue", "Helvetica Neue"),
];
/// Monochrome file-type badge colors, retained as `u32` because they are stored
/// in the SQLite history schema. UI render sites should use `THEME.badge_fill` /
/// `THEME.badge_text` so the chrome can vary independently of the persisted value.
pub(crate) const BADGE_FILL: u32 = 0x1a1a1a;
pub(crate) const BADGE_TEXT: u32 = 0xcccccc;
// Keep mins near the defaults so a drag can't crush history chips or the output pane.
// Sum of mins + handles must fit the window minimum (900px).
pub(crate) const HISTORY_SIDEBAR_MIN: f32 = 220.0;
pub(crate) const HISTORY_SIDEBAR_MAX: f32 = 360.0;
pub(crate) const OUTPUT_PANEL_MIN: f32 = 340.0;
pub(crate) const OUTPUT_PANEL_MAX: f32 = 600.0;
pub(crate) const CENTER_PANEL_MIN: f32 = 300.0;
/// Hit target width for each vertical resize handle (visual line is 1px inside).
pub(crate) const PANEL_RESIZE_HANDLE_WIDTH: f32 = 5.0;
pub(crate) const SETTINGS_SIDEBAR_WIDTH: f32 = 220.0;

/// Precomputed (format, lowercased label, lowercased id) tuples for the output
/// format menu filter so the menu does not re-allocate lowercased strings per render.
static OUTPUT_FORMAT_FILTER_CHOICES: std::sync::OnceLock<Vec<(OutputFormat, String, String)>> =
    std::sync::OnceLock::new();

fn output_format_filter_choices() -> &'static [(OutputFormat, String, String)] {
    OUTPUT_FORMAT_FILTER_CHOICES.get_or_init(|| {
        OutputFormat::ALL
            .iter()
            .map(|&format| {
                (
                    format,
                    format.label().to_ascii_lowercase(),
                    format.id().to_ascii_lowercase(),
                )
            })
            .collect()
    })
}

// Text overflow: prefer `.overflow_hidden().text_ellipsis().line_clamp(1)` over
// `.truncate()`. The latter sets `whitespace_nowrap`, and GPUI then caches the
// first text measure forever (`wrap_width` stays `None`) — labels freeze as bare
// "…" (first pass too narrow) or hard-clip without an ellipsis (first pass wide).

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PanelResizeTarget {
    History,
    Output,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PanelResizeDrag {
    target: PanelResizeTarget,
    start_x: f32,
    start_width: f32,
}

/// Clamp history sidebar width so the center panel keeps a usable minimum.
fn clamp_history_sidebar_width(width: f32, window_width: f32, output_panel_width: f32) -> f32 {
    let reserved = output_panel_width + CENTER_PANEL_MIN + PANEL_RESIZE_HANDLE_WIDTH * 2.0;
    let max_by_window = (window_width - reserved).max(HISTORY_SIDEBAR_MIN);
    width.clamp(HISTORY_SIDEBAR_MIN, HISTORY_SIDEBAR_MAX.min(max_by_window))
}

/// Clamp output panel width so the center panel keeps a usable minimum.
fn clamp_output_panel_width(width: f32, window_width: f32, history_sidebar_width: f32) -> f32 {
    let reserved = history_sidebar_width + CENTER_PANEL_MIN + PANEL_RESIZE_HANDLE_WIDTH * 2.0;
    let max_by_window = (window_width - reserved).max(OUTPUT_PANEL_MIN);
    width.clamp(OUTPUT_PANEL_MIN, OUTPUT_PANEL_MAX.min(max_by_window))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SettingsSection {
    Converters,
    General,
    Theme,
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
            Self::Theme => "Theme",
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
            Self::Theme => "UI appearance — font family for the native app chrome.",
            Self::Options => {
                "Session conversion knobs for FFmpeg, Docling, Defuddle, Pandoc, and MarkItDown."
            }
            Self::Paths => "Where Shift looks for tools, preferences, and history.",
            Self::Diagnostics => "Installed engines, versions, and install guidance.",
            Self::About => "Version, modules, and project info.",
        }
    }
}

fn ui_font_choice_label(family: &str) -> SharedString {
    UI_FONT_CHOICES
        .iter()
        .find(|(_, name)| *name == family)
        .map(|(label, _)| SharedString::from(*label))
        .unwrap_or_else(|| family.to_owned().into())
}

#[derive(Clone)]
pub(crate) struct FilePreview {
    name: SharedString,
    subtitle: SharedString,
    extension_label: SharedString,
    badge_color: u32,
    badge_text_color: u32,
}

#[derive(Clone)]
pub(crate) struct ModuleDrag {
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
            .bg(THEME.elevated)
            .border_1()
            .border_color(THEME.border_strong)
            .shadow_lg()
            .text_sm()
            .font_family(FONT_MONO)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(THEME.text)
            .child(self.label.clone())
    }
}

/// Ghost shown briefly while a native Finder file drag is initiated from the output panel.
#[derive(Clone)]
pub(crate) struct OutputFileDrag {
    label: SharedString,
    position: Point<Pixels>,
}

impl OutputFileDrag {
    fn new(label: impl Into<SharedString>, position: Point<Pixels>) -> Self {
        Self {
            label: label.into(),
            position,
        }
    }
}

impl Render for OutputFileDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .left(self.position.x - px(100.0))
            .top(self.position.y - px(22.0))
            .max_w(px(240.0))
            .px_4()
            .py_3()
            .rounded_lg()
            .bg(THEME.elevated)
            .border_1()
            .border_color(THEME.border_strong)
            .shadow_lg()
            .text_sm()
            .font_family(FONT_MONO)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(THEME.text)
            .child(self.label.clone())
    }
}

/// Start a native Finder file drag using a pre-staged path (no large rewrite on the UI thread).
///
/// `begin_file_drag` still blocks for the AppKit drag session; staging must already be done.
fn begin_output_file_drag(
    payload: &OutputDragPayload,
    app: WeakEntity<Shift>,
    position: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<OutputFileDrag> {
    match payload.staged_path.as_ref() {
        Some(path) if path.is_file() => {
            if !file_picker::begin_file_drag(path) {
                report_drag_failure(
                    app,
                    "Could not start Finder drag (no active mouse event or window).",
                    cx,
                );
            }
        }
        Some(_) => {
            report_drag_failure(
                app,
                "Could not start drag: staged file is missing. Try Reveal or Download.",
                cx,
            );
        }
        None => {
            report_drag_failure(
                app,
                "Could not start drag: artifact is not staged yet. Try Reveal first.",
                cx,
            );
        }
    }
    // Native drag owns the gesture; drop GPUI's internal drag after the session ends.
    window.defer(cx, |window, cx| {
        cx.stop_active_drag(window);
    });
    cx.new(|_| OutputFileDrag::new(payload.file_name.clone(), position))
}

fn report_drag_failure(app: WeakEntity<Shift>, message: &str, cx: &mut App) {
    let message = message.to_owned();
    let _ = app.update(cx, |this, cx| {
        this.save_status = Some(message.into());
        cx.notify();
    });
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

/// Unicode-aware single-line ellipsis. Prefer this over GPUI `text_ellipsis` /
/// `truncate` for labels: those measure an unusably narrow width in several
/// flex/scroll layouts and collapse strings to ~3 characters (e.g. "PLAN.md" → "PLA").
fn ellipsize_chars(s: &str, max_chars: usize) -> SharedString {
    if max_chars == 0 {
        return SharedString::from("…");
    }
    let mut iter = s.chars();
    let mut out = String::new();
    for _ in 0..max_chars {
        match iter.next() {
            Some(c) => out.push(c),
            None => return out.into(),
        }
    }
    if iter.next().is_none() {
        return out.into();
    }
    // Replace the last kept char with an ellipsis so we stay within max_chars.
    out.pop();
    out.push('…');
    out.into()
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
                    THEME.border_focused
                } else {
                    THEME.border
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
                .text_color(THEME.text_secondary)
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
                .text_color(THEME.text_secondary)
                .child("or click to browse (multi-select)"),
        )
}

fn batch_item_status_label(item: &BatchItem) -> SharedString {
    match &item.state {
        BatchItemState::Queued => "queued".into(),
        BatchItemState::Running => "running…".into(),
        BatchItemState::Succeeded { written_path, .. } => {
            // Full path — trust requirement: never hide where the file landed.
            format!("✓ saved · {}", written_path.display()).into()
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
                                .bg(THEME.elevated)
                                .border_1()
                                .border_color(THEME.border_strong)
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(THEME.text_primary)
                                .cursor_pointer()
                                .hover(|style| {
                                    style.bg(THEME.active).border_color(THEME.border_focused)
                                })
                                .active(|style| style.opacity(THEME.active_opacity))
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
                                .bg(if force { THEME.active } else { THEME.elevated })
                                .border_1()
                                .border_color(if force {
                                    THEME.border_focused
                                } else {
                                    THEME.border_strong
                                })
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(THEME.text_primary)
                                .cursor_pointer()
                                .hover(|style| {
                                    style.bg(THEME.active).border_color(THEME.border_focused)
                                })
                                .active(|style| style.opacity(THEME.active_opacity))
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
                                    .bg(THEME.elevated)
                                    .border_1()
                                    .border_color(THEME.border_strong)
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(THEME.text_primary)
                                    .cursor_pointer()
                                    .hover(|style| {
                                        style.bg(THEME.active).border_color(THEME.border_focused)
                                    })
                                    .active(|style| style.opacity(THEME.active_opacity))
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
                                    .bg(THEME.elevated)
                                    .border_1()
                                    .border_color(THEME.border_strong)
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(THEME.text_primary)
                                    .cursor_pointer()
                                    .hover(|style| {
                                        style.bg(THEME.active).border_color(THEME.border_focused)
                                    })
                                    .active(|style| style.opacity(THEME.active_opacity))
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
                                    .bg(THEME.elevated)
                                    .border_1()
                                    .border_color(THEME.border_strong)
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(THEME.text_primary)
                                    .cursor_pointer()
                                    .hover(|style| {
                                        style.bg(THEME.active).border_color(THEME.border_focused)
                                    })
                                    .active(|style| style.opacity(THEME.active_opacity))
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
                                .bg(THEME.elevated)
                                .border_1()
                                .border_color(THEME.border_strong)
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(THEME.text_primary)
                                .cursor_pointer()
                                .hover(|style| {
                                    style.bg(THEME.active).border_color(THEME.border_focused)
                                })
                                .active(|style| style.opacity(THEME.active_opacity))
                                .child("Clear")
                                .on_click(cx.listener(|this, _, _, cx| this.clear_batch_queue(cx))),
                        ),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(THEME.text_muted)
                .child(format!("Output: {folder_label}")),
        )
        .when_some(status, |panel, status| {
            panel.child(
                div()
                    .text_xs()
                    .text_color(THEME.text_secondary)
                    .child(status),
            )
        })
        .child(
            div()
                .id("batch-queue-list")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
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
                        .bg(THEME.raised)
                        .border_1()
                        .border_color(THEME.border)
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
                                        .text_color(THEME.text_primary)
                                        .child(ellipsize_chars(name.as_ref(), 48)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(THEME.text_muted)
                                        .child(ellipsize_chars(detail.as_ref(), 56)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(THEME.text_dim)
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
                                    .text_color(THEME.text_secondary)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(THEME.hover).text_color(THEME.text))
                                    .active(|style| style.opacity(THEME.active_opacity))
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
                                    .text_color(THEME.text_secondary)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(THEME.hover).text_color(THEME.text))
                                    .active(|style| style.opacity(THEME.active_opacity))
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
                                    .text_color(THEME.text_secondary)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(THEME.hover).text_color(THEME.text))
                                    .active(|style| style.opacity(THEME.active_opacity))
                                    .child("Retry")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.retry_batch_item(id, cx);
                                        cx.stop_propagation();
                                    })),
                            )
                        })
                })),
        )
        .with_animation(
            "batch-queue-in",
            Animation::new(animation::ENTER_DURATION).with_easing(ease_out_quint()),
            |element, progress| element.opacity(0.12 + 0.88 * progress),
        )
}

fn history_sidebar(
    visible: &[ConversionHistoryEntry],
    history_total: usize,
    history_search: Entity<TextInput>,
    active_history_id: Option<u64>,
    width: f32,
    cx: &mut Context<Shift>,
) -> impl IntoElement {
    let is_empty = visible.is_empty();
    let has_any = history_total > 0 && !visible.is_empty();

    div()
        .id("history-sidebar")
        .flex()
        .flex_col()
        .flex_shrink_0()
        .w(px(width))
        .h_full()
        .bg(THEME.background)
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .px_4()
                .pt(px(28.0))
                .pb_3()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(THEME.text_secondary)
                                .child("History"),
                        )
                        .when(has_any, |header| {
                            header.child(
                                div()
                                    .id("clear-history")
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .text_xs()
                                    .text_color(THEME.text_muted)
                                    .cursor_pointer()
                                    .hover(|style| {
                                        style.bg(THEME.elevated).text_color(THEME.text_secondary)
                                    })
                                    .active(|style| style.opacity(THEME.active_opacity))
                                    .child("Clear")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.clear_history(cx);
                                        cx.stop_propagation();
                                    })),
                            )
                        }),
                )
                .child(
                    div().flex().items_center().gap_2().child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .rounded_lg()
                            .bg(THEME.surface)
                            .border_1()
                            .border_color(THEME.border)
                            .text_sm()
                            .text_color(THEME.text_primary)
                            .overflow_hidden()
                            .child(history_search),
                    ),
                ),
        )
        .child(
            div()
                .id("history-list")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .w_full()
                .min_w_0()
                .px_2()
                .pb_3()
                .gap_1()
                .overflow_x_hidden()
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
                                    .text_color(THEME.text_muted)
                                    .child("No conversions yet"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(THEME.text_dim)
                                    .child("Completed work is kept across launches."),
                            ),
                    )
                })
                .children(visible.iter().map(|entry| {
                    let id = entry.id;
                    let active = active_history_id == Some(id);
                    let failed = matches!(entry.outcome, HistoryOutcome::Failed(_));
                    let output_format = history_output_format(entry);
                    let output_badge_label: SharedString =
                        output_format_badge_label(output_format).into();
                    let detail = history_entry_detail(entry);
                    let archive_label = if entry.archived {
                        "Unarchive"
                    } else {
                        "Archive"
                    };

                    div()
                        .id(("history-entry", id))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .w_full()
                        .px_2()
                        .py_2()
                        .rounded_lg()
                        .cursor_pointer()
                        .active(|style| style.opacity(THEME.active_opacity))
                        .when(active, |row| {
                            row.bg(THEME.elevated)
                                .border_1()
                                .border_color(THEME.border_strong)
                        })
                        .when(!active, |row| {
                            row.border_1()
                                .border_color(THEME.background)
                                .hover(|style| style.bg(THEME.surface))
                        })
                        .child(
                            div()
                                .w_full()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(if failed {
                                    THEME.text_secondary
                                } else {
                                    THEME.text_primary
                                })
                                .child(ellipsize_chars(entry.name.as_ref(), 48)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .w_full()
                                .child(history_conversion_chip(
                                    entry.extension_label.clone(),
                                    output_badge_label,
                                ))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_xs()
                                        .text_color(THEME.text_muted)
                                        .child(ellipsize_chars(detail.as_ref(), 56)),
                                ),
                        )
                        .when(active, |row| {
                            row.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_3()
                                    .px_1()
                                    .child(
                                        div()
                                            .id(("history-archive", id))
                                            .text_xs()
                                            .text_color(THEME.text_muted)
                                            .cursor_pointer()
                                            .hover(|style| style.text_color(THEME.text_primary))
                                            .active(|style| style.opacity(THEME.active_opacity))
                                            .child(archive_label)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.archive_history_entry(id, cx);
                                                cx.stop_propagation();
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id(("history-delete", id))
                                            .text_xs()
                                            .text_color(THEME.text_muted)
                                            .cursor_pointer()
                                            .hover(|style| style.text_color(THEME.text_primary))
                                            .active(|style| style.opacity(THEME.active_opacity))
                                            .child("Delete")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.delete_history_entry(id, cx);
                                                cx.stop_propagation();
                                            })),
                                    ),
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.restore_history_entry(id, cx);
                            cx.stop_propagation();
                        }))
                })),
        )
}

fn history_matches_search(entry: &ConversionHistoryEntry, query: &str) -> bool {
    entry.name.to_lowercase().contains(query)
        || entry.detail.to_lowercase().contains(query)
        || entry.extension_label.to_lowercase().contains(query)
        || history_output_format(entry)
            .label()
            .to_lowercase()
            .contains(query)
        || match &entry.source {
            HistorySource::File(path) => path.to_string_lossy().to_lowercase().contains(query),
            HistorySource::Url(url) => url.to_lowercase().contains(query),
        }
}

/// Vertical drag handle between main columns (history | source | output).
fn vertical_resize_handle(
    id: impl Into<ElementId>,
    target: PanelResizeTarget,
    active: bool,
    padded: bool,
    cx: &mut Context<Shift>,
) -> impl IntoElement {
    let line = div()
        .w(px(1.0))
        .h_full()
        .mx_auto()
        .bg(if active {
            THEME.border_focused
        } else {
            THEME.border
        })
        .group_hover("panel-resize", |style| style.bg(THEME.border_strong));

    div()
        .id(id)
        .group("panel-resize")
        .h_full()
        .w(px(PANEL_RESIZE_HANDLE_WIDTH))
        .flex_shrink_0()
        .cursor(CursorStyle::ResizeColumn)
        .when(padded, |handle| handle.py_8())
        .child(line)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                this.begin_panel_resize(target, f32::from(event.position.x), cx);
                cx.stop_propagation();
            }),
        )
        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
            if this.panel_resize.is_some() {
                this.handle_panel_resize_move(event, window, cx);
                cx.stop_propagation();
            }
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, _, cx| {
                this.end_panel_resize(cx);
            }),
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

fn file_preview_card(
    preview: FilePreview,
    can_convert: bool,
    cx: &mut Context<Shift>,
) -> impl IntoElement {
    let badge_color = preview.badge_color;
    let badge_text_color = preview.badge_text_color;

    div()
        .id("file-preview")
        .flex()
        .flex_col()
        .items_center()
        .gap_4()
        // Fixed width so the inner flex row cannot shrink-wrap the text column
        // down to a few characters (same bug history rows hit).
        .w(px(320.0))
        .child(
            div()
                .relative()
                .flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_3()
                .px_4()
                .py_3()
                .rounded_xl()
                .bg(THEME.surface)
                .border_1()
                .border_color(THEME.border)
                .shadow(card_shadow())
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
                        .border_color(THEME.border_light)
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .text_color(rgb(badge_text_color))
                                .child(preview.extension_label),
                        ),
                )
                // Name + meta. Avoid text_ellipsis — same GPUI Truncate bug as history.
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
                                .text_color(THEME.text)
                                .child(ellipsize_chars(preview.name.as_ref(), 40)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(THEME.text_secondary)
                                .child(ellipsize_chars(preview.subtitle.as_ref(), 56)),
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
                        .bg(THEME.hover)
                        .border_1()
                        .border_color(THEME.border_strong)
                        .text_color(THEME.text_secondary)
                        .cursor_pointer()
                        .hover(|style| {
                            style
                                .bg(THEME.hover)
                                .border_color(THEME.border_focused)
                                .text_color(THEME.text)
                        })
                        .active(|style| style.opacity(THEME.active_opacity))
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
        .when(can_convert, |this| {
            this.child(
                div()
                    .id("convert-selected")
                    .flex()
                    .items_center()
                    .justify_center()
                    .w_full()
                    .h(px(40.0))
                    .px_4()
                    .rounded_lg()
                    .bg(THEME.elevated)
                    .border_1()
                    .border_color(THEME.border_strong)
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(THEME.text_primary)
                    .cursor_pointer()
                    .hover(|style| style.bg(THEME.active).border_color(THEME.border_focused))
                    .active(|style| style.opacity(THEME.active_opacity))
                    .child("Convert")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_conversion(cx);
                        cx.stop_propagation();
                    })),
            )
        })
        .child(
            div()
                .text_xs()
                .text_color(THEME.text_muted)
                .child("Click to add files  ·  Drop more for batch"),
        )
        .with_animation(
            "file-preview-in",
            Animation::new(animation::DIALOG_DURATION).with_easing(ease_out_quint()),
            |element, progress| element.opacity(0.12 + 0.88 * progress),
        )
}

fn artifact_preview(artifact: &ConversionArtifact) -> SharedString {
    artifact.preview_summary().into()
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
        .bg(if selected { THEME.text } else { THEME.surface })
        .text_color(if selected {
            THEME.text_inverse
        } else {
            THEME.text_secondary
        })
        .border_1()
        .border_color(if selected { THEME.text } else { THEME.border })
        .hover(|style| {
            if selected {
                style
            } else {
                style.bg(THEME.hover)
            }
        })
        .active(|style| style.opacity(THEME.active_opacity))
        .child(label.into())
        .on_click(cx.listener(move |this, _, _, cx| {
            on_click(this, cx);
            this.persist_session_settings(cx);
            cx.notify();
            cx.stop_propagation();
        }))
}

pub(crate) struct ConversionPanelView {
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
    pandoc_citations: bool,
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
        pandoc_citations,
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
        .bg(THEME.raised)
        .border_1()
        .border_color(THEME.border)
        .shadow(card_shadow())
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(THEME.text_secondary)
                        .child("Conversion options"),
                )
                .child(
                    div()
                        .id("apply-conversion-options")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(THEME.elevated)
                        .border_1()
                        .border_color(THEME.border_strong)
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(THEME.text_primary)
                        .cursor_pointer()
                        .hover(|style| style.bg(THEME.active))
                        .active(|style| style.opacity(THEME.active_opacity))
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
                    .text_color(THEME.text_muted)
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
                        .child(
                            div()
                                .text_xs()
                                .text_color(THEME.text_muted)
                                .child("Quality"),
                        )
                        .child(chip(
                            "media-quality-balanced",
                            FfmpegQuality::Balanced.label(),
                            quality == FfmpegQuality::Balanced,
                            cx,
                            |this, _cx| {
                                this.ffmpeg_quality = FfmpegQuality::Balanced;
                            },
                        ))
                        .child(chip(
                            "media-quality-high",
                            FfmpegQuality::High.label(),
                            quality == FfmpegQuality::High,
                            cx,
                            |this, _cx| {
                                this.ffmpeg_quality = FfmpegQuality::High;
                            },
                        ))
                        .child(chip(
                            "media-quality-small",
                            FfmpegQuality::Small.label(),
                            quality == FfmpegQuality::Small,
                            cx,
                            |this, _cx| {
                                this.ffmpeg_quality = FfmpegQuality::Small;
                            },
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(div().text_xs().text_color(THEME.text_muted).child("Encode"))
                        .child(chip(
                            "media-encode-auto",
                            FfmpegEncodeMode::Auto.label(),
                            encode_mode == FfmpegEncodeMode::Auto,
                            cx,
                            |this, _cx| {
                                this.ffmpeg_encode_mode = FfmpegEncodeMode::Auto;
                            },
                        ))
                        .child(chip(
                            "media-encode-copy",
                            FfmpegEncodeMode::PreferCopy.label(),
                            encode_mode == FfmpegEncodeMode::PreferCopy,
                            cx,
                            |this, _cx| {
                                this.ffmpeg_encode_mode = FfmpegEncodeMode::PreferCopy;
                            },
                        ))
                        .child(chip(
                            "media-encode-reencode",
                            FfmpegEncodeMode::Reencode.label(),
                            encode_mode == FfmpegEncodeMode::Reencode,
                            cx,
                            |this, _cx| {
                                this.ffmpeg_encode_mode = FfmpegEncodeMode::Reencode;
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
                                        .text_color(THEME.text_muted)
                                        .child("Start (sec)"),
                                )
                                .child(
                                    div()
                                        .h(px(32.0))
                                        .px_2()
                                        .rounded_md()
                                        .bg(THEME.surface)
                                        .border_1()
                                        .border_color(THEME.border)
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
                                        .text_color(THEME.text_muted)
                                        .child("Duration (sec)"),
                                )
                                .child(
                                    div()
                                        .h(px(32.0))
                                        .px_2()
                                        .rounded_md()
                                        .bg(THEME.surface)
                                        .border_1()
                                        .border_color(THEME.border)
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
                                .text_color(THEME.text_muted)
                                .child("Frame at (sec)"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(THEME.surface)
                                .border_1()
                                .border_color(THEME.border)
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
                            .child(div().text_xs().text_color(THEME.text_muted).child("Audio"))
                            .child(chip(
                                "media-mono",
                                if mono { "Mono ✓" } else { "Mono" },
                                mono,
                                cx,
                                |this, cx| {
                                    this.ffmpeg_mono = !this.ffmpeg_mono;
                                    this.persist_session_settings(cx);
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
                                },
                            ))
                            .child(chip(
                                "media-rate-44100",
                                "44.1 kHz",
                                sample_rate_hz == Some(44_100),
                                cx,
                                |this, _cx| {
                                    this.ffmpeg_sample_rate_hz = Some(44_100);
                                },
                            ))
                            .child(chip(
                                "media-rate-48000",
                                "48 kHz",
                                sample_rate_hz == Some(48_000),
                                cx,
                                |this, _cx| {
                                    this.ffmpeg_sample_rate_hz = Some(48_000);
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
                                    .text_color(THEME.text_muted)
                                    .child("Audio stream index"),
                            )
                            .child(
                                div()
                                    .h(px(32.0))
                                    .px_2()
                                    .rounded_md()
                                    .bg(THEME.surface)
                                    .border_1()
                                    .border_color(THEME.border)
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
                        .child(div().text_xs().text_color(THEME.text_muted).child("Width"))
                        .child(chip(
                            "media-scale-auto",
                            "Auto",
                            scale_width.is_none(),
                            cx,
                            |this, _cx| {
                                this.ffmpeg_scale_width = None;
                            },
                        ))
                        .child(chip(
                            "media-scale-720",
                            "720",
                            scale_width == Some(720),
                            cx,
                            |this, _cx| {
                                this.ffmpeg_scale_width = Some(720);
                            },
                        ))
                        .child(chip(
                            "media-scale-1280",
                            "1280",
                            scale_width == Some(1280),
                            cx,
                            |this, _cx| {
                                this.ffmpeg_scale_width = Some(1280);
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
                            },
                        )),
                );
                section = section
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_xs().text_color(THEME.text_muted).child("FPS"))
                            .child(
                                div()
                                    .h(px(32.0))
                                    .px_2()
                                    .rounded_md()
                                    .bg(THEME.surface)
                                    .border_1()
                                    .border_color(THEME.border)
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
                                .text_color(THEME.text_muted)
                                .child("Frame interval (sec)"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(THEME.surface)
                                .border_1()
                                .border_color(THEME.border)
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
                                .text_color(THEME.text_muted)
                                .child("Subtitle stream index"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(THEME.surface)
                                .border_1()
                                .border_color(THEME.border)
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
                        .text_color(THEME.text_muted)
                        .child("Docling"),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(div().text_xs().text_color(THEME.text_muted).child("Images"))
                        .child(chip(
                            "docling-images-placeholder",
                            DoclingImageExportMode::Placeholder.label(),
                            docling_images == DoclingImageExportMode::Placeholder,
                            cx,
                            |this, _cx| {
                                this.docling_images = DoclingImageExportMode::Placeholder;
                            },
                        ))
                        .child(chip(
                            "docling-images-embedded",
                            DoclingImageExportMode::Embedded.label(),
                            docling_images == DoclingImageExportMode::Embedded,
                            cx,
                            |this, _cx| {
                                this.docling_images = DoclingImageExportMode::Embedded;
                            },
                        ))
                        .child(chip(
                            "docling-images-referenced",
                            DoclingImageExportMode::Referenced.label(),
                            docling_images == DoclingImageExportMode::Referenced,
                            cx,
                            |this, _cx| {
                                this.docling_images = DoclingImageExportMode::Referenced;
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
                            |this, _cx| {
                                this.docling_ocr = !this.docling_ocr;
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
                            |this, _cx| {
                                this.docling_tables = !this.docling_tables;
                            },
                        ))
                        .child(chip(
                            "docling-table-fast",
                            DoclingTableMode::Fast.label(),
                            docling_table_mode == DoclingTableMode::Fast,
                            cx,
                            |this, _cx| {
                                this.docling_table_mode = DoclingTableMode::Fast;
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
                                .text_color(THEME.text_muted)
                                .child("OCR language (e.g. eng)"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(THEME.surface)
                                .border_1()
                                .border_color(THEME.border)
                                .child(docling_ocr_lang_input),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(THEME.text_dim)
                        .child("Embedded images can produce large artifacts."),
                )
        })
        .when(show_pdf_pages, |panel| {
            panel
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(THEME.text_muted)
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
                                        .text_color(THEME.text_muted)
                                        .child("From page"),
                                )
                                .child(
                                    div()
                                        .h(px(32.0))
                                        .px_2()
                                        .rounded_md()
                                        .bg(THEME.surface)
                                        .border_1()
                                        .border_color(THEME.border)
                                        .child(pdf_page_from_input),
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
                                        .text_color(THEME.text_muted)
                                        .child("To page"),
                                )
                                .child(
                                    div()
                                        .h(px(32.0))
                                        .px_2()
                                        .rounded_md()
                                        .bg(THEME.surface)
                                        .border_1()
                                        .border_color(THEME.border)
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
                                .text_color(THEME.text_muted)
                                .child("PDF password (session only)"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(THEME.surface)
                                .border_1()
                                .border_color(THEME.border)
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
                        .text_color(THEME.text_muted)
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
                    |this, _cx| {
                        this.defuddle_frontmatter = !this.defuddle_frontmatter;
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
                                .text_color(THEME.text_muted)
                                .child("Language (BCP 47)"),
                        )
                        .child(
                            div()
                                .h(px(32.0))
                                .px_2()
                                .rounded_md()
                                .bg(THEME.surface)
                                .border_1()
                                .border_color(THEME.border)
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
                        .text_color(THEME.text_muted)
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
                            |this, _cx| {
                                this.pandoc_standalone = !this.pandoc_standalone;
                            },
                        ))
                        .child(chip(
                            "pandoc-toc",
                            if pandoc_toc { "TOC ✓" } else { "TOC" },
                            pandoc_toc,
                            cx,
                            |this, _cx| {
                                this.pandoc_toc = !this.pandoc_toc;
                            },
                        ))
                        .child(chip(
                            "pandoc-citations",
                            if pandoc_citations {
                                "Citations ✓"
                            } else {
                                "Citations"
                            },
                            pandoc_citations,
                            cx,
                            |this, cx| {
                                this.pandoc_citations = !this.pandoc_citations;
                                this.persist_session_settings(cx);
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
                            .text_color(THEME.text_muted)
                            .child("PDF engine"),
                    )
                    .child(chip(
                        "pandoc-pdf-auto",
                        "Auto",
                        pandoc_pdf_engine.is_none(),
                        cx,
                        |this, _cx| {
                            this.pandoc_pdf_engine = None;
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
                            .text_color(THEME.text_muted)
                            .child("Reference doc"),
                    )
                    .child(
                        div()
                            .id("pandoc-reference-doc")
                            .max_w(px(220.0))
                            .min_w_0()
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(THEME.elevated)
                            .border_1()
                            .border_color(THEME.border_strong)
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(THEME.text_primary)
                            .overflow_hidden()
                            .text_ellipsis()
                            .line_clamp(1)
                            .cursor_pointer()
                            .hover(|style| style.bg(THEME.active))
                            .active(|style| style.opacity(THEME.active_opacity))
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
                        .text_color(THEME.text_muted)
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
                    |this, _cx| {
                        this.markitdown_keep_data_uris = !this.markitdown_keep_data_uris;
                    },
                )))
                .child(
                    div()
                        .text_xs()
                        .text_color(THEME.text_dim)
                        .child("Keeping data URIs can produce large Markdown files."),
                )
        })
        .child(
            div()
                .text_xs()
                .text_color(THEME.text_dim)
                .child("Edit fields, then Apply. Chips reconvert immediately."),
        )
        .with_animation(
            "conversion-options-in",
            Animation::new(animation::PANEL_DURATION).with_easing(ease_out_quint()),
            |element, progress| element.opacity(0.12 + 0.88 * progress),
        )
}

pub(crate) struct OutputPanelView {
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
    /// Pre-staged export path for the ready artifact (avoids rewriting large media on drag).
    cached_ready_path: Option<PathBuf>,
}

/// Payload for native Finder drag from the output panel.
#[derive(Clone)]
pub(crate) struct OutputDragPayload {
    file_name: String,
    /// Pre-staged path when available; drag will not rewrite bytes when this is set.
    staged_path: Option<PathBuf>,
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
        cached_ready_path,
    } = view;
    let app_entity = cx.weak_entity();
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
                    .text_color(THEME.text_secondary)
                    .child("Output appears here"),
            )
            .child(
                div()
                    .max_w(px(280.0))
                    .text_sm()
                    .text_color(THEME.text_muted)
                    .child(
                        "Choose a document, media file, or paste a URL, path, or image — Shift converts it automatically.",
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
                        .text_color(THEME.text_secondary)
                        .child("↻")
                        .with_animation(
                            "conversion-pulse",
                            Animation::new(animation::SPINNER_PERIOD).with_easing(pulsating_between(0.35, 1.0)).repeat(),
                            |element, progress| element.opacity(progress),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(THEME.text_secondary)
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
                                .bg(THEME.elevated)
                                .border_1()
                                .border_color(THEME.border)
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .bg(THEME.text_secondary)
                                        .w(px(220.0 * clamped)),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(THEME.text_muted)
                                .child(format!("{:.0}%", clamped * 100.0)),
                        )
                })
                .child(
                    div()
                        .id("cancel-conversion")
                        .px_3()
                        .py_1()
                        .rounded_md()
                        .bg(THEME.elevated)
                        .border_1()
                        .border_color(THEME.border_strong)
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(THEME.text_primary)
                        .cursor_pointer()
                        .hover(|style| style.bg(THEME.active).border_color(THEME.border_focused))
                        .active(|style| style.opacity(THEME.active_opacity))
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
                    .text_color(THEME.text)
                    .child("Conversion failed"),
            )
            .child(
                div()
                    .p_4()
                    .rounded_lg()
                    .bg(THEME.elevated)
                    .border_1()
                    .border_color(THEME.border_strong)
                    .text_sm()
                    .text_color(THEME.text_secondary)
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
                    .bg(THEME.raised)
                    .border_1()
                    .border_color(THEME.border)
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(THEME.text_secondary)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(THEME.text_muted)
                            .child(hint.clone()),
                    )
                    .child(
                        div()
                            .id(("copy-install", index as u64))
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(THEME.elevated)
                            .border_1()
                            .border_color(THEME.border_strong)
                            .text_xs()
                            .text_color(THEME.text_primary)
                            .cursor_pointer()
                            .hover(|style| style.bg(THEME.active))
                            .active(|style| style.opacity(THEME.active_opacity))
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
            // Full name kept for drag/save; display may be ellipsized in software.
            let file_name_full = artifact.file_name.clone();
            let file_name_display = ellipsize_chars(&artifact.file_name, 42);
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
            let conversion_detail = ellipsize_chars(
                &format!(
                    "{}  ·  {size}  ·  via {pipeline_badge}",
                    artifact.format.label()
                ),
                56,
            );
            let commands: Vec<SharedString> = artifact
                .invocations
                .iter()
                .map(|inv| format!("{}: {}", inv.module_id, inv.argv_display).into())
                .collect();
            let drag_payload = OutputDragPayload {
                file_name: file_name_full,
                staged_path: cached_ready_path.clone(),
            };
            let drag_app = app_entity.clone();
            // One result card: name + metadata + actions (and text preview when available).
            // Binary outputs keep the same surface — no second card with duplicate controls.
            div()
                .flex()
                .flex_col()
                .gap_3()
                .h_full()
                .child(
                    div()
                        .id("output-result-card")
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_4()
                        .rounded_xl()
                        .bg(THEME.elevated)
                        .border_1()
                        .border_color(THEME.border_strong)
                        .shadow(card_shadow())
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_start()
                                .justify_between()
                                .gap_2()
                                .child(
                                    div()
                                        .id("output-drag-source")
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .min_w_0()
                                        .flex_1()
                                        .min_w(px(160.0))
                                        .px_1()
                                        .py_0p5()
                                        .rounded_md()
                                        .cursor_move()
                                        .hover(|style| style.bg(THEME.hover))
                                        .on_drag(
                                            drag_payload,
                                            move |payload, position, window, cx| {
                                                begin_output_file_drag(
                                                    payload,
                                                    drag_app.clone(),
                                                    position,
                                                    window,
                                                    cx,
                                                )
                                            },
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_2()
                                                .w_full()
                                                .child(
                                                    div()
                                                        .flex_shrink_0()
                                                        .text_color(THEME.text_muted)
                                                        .child("⠿"),
                                                )
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .text_lg()
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .overflow_hidden()
                                                        .text_ellipsis()
                                                        .line_clamp(1)
                                                        .child(file_name_display),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(THEME.text_secondary)
                                                .child(conversion_detail),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_wrap()
                                                .items_center()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .px_2()
                                                        .py_1()
                                                        .rounded_md()
                                                        .bg(THEME.badge_fill)
                                                        .text_xs()
                                                        .text_color(THEME.badge_text)
                                                        .child(pipeline_badge),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(THEME.text_muted)
                                                        .child("Drag to Downloads or Documents"),
                                                ),
                                        ),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_shrink_0()
                                        .flex_wrap()
                                        .gap_2()
                                        .child(action_chip(
                                            "save-conversion",
                                            "Download",
                                            cx,
                                            |this, cx| {
                                                this.save_output(cx);
                                            },
                                        ))
                                        .child(action_chip(
                                            "copy-conversion",
                                            if is_text { "Copy" } else { "Copy path" },
                                            cx,
                                            |this, cx| {
                                                this.copy_output(cx);
                                            },
                                        ))
                                        .child(action_chip(
                                            "reveal-conversion",
                                            "Reveal",
                                            cx,
                                            |this, cx| {
                                                this.reveal_output(cx);
                                            },
                                        ))
                                        .child(action_chip(
                                            "open-conversion",
                                            "Open",
                                            cx,
                                            |this, cx| {
                                                this.open_output(cx);
                                            },
                                        ))
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
                                                    this.show_command_inspect =
                                                        !this.show_command_inspect;
                                                    cx.notify();
                                                },
                                            ))
                                        }),
                                ),
                        )
                        .when(!is_text, |card| {
                            card.child(
                                div()
                                    .text_xs()
                                    .text_color(THEME.text_muted)
                                    .child(
                                        "Not shown inline — Download, drag the file above, or Open with your default app.",
                                    ),
                            )
                        })
                        .when(is_text, |card| {
                            card.child(
                                div()
                                    .p_4()
                                    .rounded_lg()
                                    .bg(THEME.raised)
                                    .border_1()
                                    .border_color(THEME.border)
                                    .text_sm()
                                    .text_color(THEME.text_secondary)
                                    .child(excerpt),
                            )
                        })
                        .with_animation(
                            "output-result-card-in",
                            Animation::new(animation::DIALOG_DURATION).with_easing(ease_out_quint()),
                            |element, progress| element.opacity(0.12 + 0.88 * progress),
                        ),
                )
                .when_some(cached_ready_path.clone(), |panel, staged| {
                    // Preview is not a permanent save; still show the staged path
                    // so Reveal/Open have a concrete location.
                    panel.child(
                        div()
                            .text_xs()
                            .text_color(THEME.text_dim)
                            .child(format!("On disk (staged) · {}", staged.display())),
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
                            .bg(THEME.raised)
                            .border_1()
                            .border_color(THEME.border)
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(THEME.text_secondary)
                                    .child("Command"),
                            )
                            .children(commands.into_iter().map(|line| {
                                div()
                                    .text_xs()
                                    .text_color(THEME.text_muted)
                                    .child(line)
                            })),
                    )
                })
                .when_some(save_status, |panel, status| {
                    panel.child(div().text_xs().text_color(THEME.text_secondary).child(status))
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
                .bg(THEME.surface)
                .border_1()
                .border_color(THEME.border)
                .cursor_pointer()
                .hover(|style| style.bg(THEME.hover))
                .active(|style| style.opacity(THEME.active_opacity))
                .child(div().text_xs().text_color(THEME.text_muted).child("Output"))
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
                        .text_color(THEME.text_secondary)
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
                    .bg(THEME.elevated)
                    .border_1()
                    .border_color(THEME.border_strong)
                    .shadow_lg()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .h(px(32.0))
                            .px_2()
                            .mb_1()
                            .rounded_md()
                            .bg(THEME.surface)
                            .border_1()
                            .border_color(THEME.border)
                            .child(format_filter_input),
                    )
                    .children(
                        output_format_filter_choices()
                            .iter()
                            .enumerate()
                            .filter(|(_, (_, label, id))| {
                                if filter_lower.is_empty() {
                                    return true;
                                }
                                label.contains(&filter_lower) || id.contains(&filter_lower)
                            })
                            .map(|(index, (format, _, _))| {
                                let format = *format;
                                let enabled = available_outputs.contains(&format);
                                let engine_ready = ready_outputs
                                    .as_ref()
                                    .map(|ready| ready.contains(&format))
                                    .unwrap_or(true);
                                let label_color = if !enabled {
                                    THEME.text_dim
                                } else if !engine_ready {
                                    THEME.text_muted
                                } else {
                                    THEME.text_primary
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
                                            .hover(|style| style.bg(THEME.hover))
                                            .active(|style| style.opacity(THEME.active_opacity))
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
                                                        .text_color(THEME.text_dim)
                                                        .child("engine not installed"),
                                                )
                                            })
                                            .when(!enabled, |row| {
                                                row.child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(THEME.text_dim)
                                                        .child("not for this source"),
                                                )
                                            }),
                                    )
                                    .when(format == output_format, |row| {
                                        row.child(div().text_color(THEME.text).child("✓"))
                                    })
                            }),
                    ),
            )
        });

    // Outer clips to the panel; scroll child carries options + result so tall
    // Pandoc/FFmpeg knobs + artifact cards remain reachable.
    div()
        .relative()
        .size_full()
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(
            div()
                .id("output-panel-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .p_8()
                .pt(px(78.0))
                .gap_3()
                .when(show_conversion_options, |panel| {
                    panel.child(conversion_options_panel(conversion_options, cx))
                })
                // flex_1 grows when short (empty/converting centering); omit
                // min_h_0 so tall Ready content expands the scroll range.
                .child(div().flex_1().child(content)),
        )
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
        .bg(THEME.elevated)
        .border_1()
        .border_color(THEME.border_strong)
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(THEME.text_primary)
        .cursor_pointer()
        .hover(|style| style.bg(THEME.active).border_color(THEME.border_focused))
        .active(|style| style.opacity(THEME.active_opacity))
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

/// Short uppercase badge for an output format (mirrors input extension badges).
fn output_format_badge_label(format: OutputFormat) -> String {
    let ext = format.extension().to_ascii_uppercase();
    if ext.is_empty() {
        "OUT".into()
    } else if ext.len() <= 4 {
        ext
    } else {
        ext.chars().take(4).collect()
    }
}

/// Resolved output format for a history row (prefers the artifact when present).
fn history_output_format(entry: &ConversionHistoryEntry) -> OutputFormat {
    match &entry.outcome {
        HistoryOutcome::Ready(artifact) => artifact.format,
        HistoryOutcome::ReadyLarge { .. } | HistoryOutcome::Failed(_) => entry.output_format,
    }
}

/// Secondary line under the history title: engine / size / failure — input and
/// output formats are shown as badges, so they are not repeated here.
fn history_entry_detail(entry: &ConversionHistoryEntry) -> SharedString {
    match &entry.outcome {
        HistoryOutcome::Ready(artifact) => {
            format!("via {}", module_label(artifact.module_id)).into()
        }
        HistoryOutcome::ReadyLarge {
            module_id,
            byte_len,
        } => format!(
            "{}  ·  via {} (re-convert to restore)",
            format_file_size(*byte_len as u64),
            module_label(module_id)
        )
        .into(),
        HistoryOutcome::Failed(_) => "failed".into(),
    }
}

/// Persisted detail string: keeps input → output explicit for stored history.
fn history_entry_stored_detail(entry: &ConversionHistoryEntry) -> String {
    let input = entry.extension_label.as_ref();
    let output = history_output_format(entry).label();
    match &entry.outcome {
        HistoryOutcome::Ready(artifact) => {
            format!(
                "{input} → {output}  ·  via {}",
                module_label(artifact.module_id)
            )
        }
        HistoryOutcome::ReadyLarge {
            module_id,
            byte_len,
        } => format!(
            "{input} → {output}  ·  {}  ·  via {} (re-convert to restore)",
            format_file_size(*byte_len as u64),
            module_label(module_id)
        ),
        HistoryOutcome::Failed(_) => format!("{input} → {output}  ·  failed"),
    }
}

/// Compact chip showing input → output so history rows read as a conversion.
fn history_conversion_chip(
    input_label: SharedString,
    output_label: SharedString,
) -> impl IntoElement {
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .gap_1()
        .h(px(32.0))
        .px_2()
        .rounded_md()
        .bg(THEME.badge_fill)
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(THEME.badge_text)
                .child(input_label),
        )
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(THEME.text_muted)
                .child("→"),
        )
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(THEME.badge_text)
                .child(output_label),
        )
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

fn url_input_bar(
    url_input: Entity<TextInput>,
    window: &mut Window,
    cx: &mut Context<Shift>,
) -> impl IntoElement {
    let url_focused = url_input.read(cx).focus_handle(cx).is_focused(window);
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
                .bg(THEME.surface)
                .border_1()
                .border_color(if url_focused {
                    THEME.border_focused
                } else {
                    THEME.border
                })
                .text_sm()
                .text_color(THEME.text_primary)
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
                .bg(THEME.elevated)
                .border_1()
                .border_color(THEME.border_strong)
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(THEME.text_primary)
                .cursor_pointer()
                .hover(|style| style.bg(THEME.active).border_color(THEME.border_focused))
                .active(|style| style.opacity(THEME.active_opacity))
                .child("Convert")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.submit_magic_paste_from_input(cx);
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
        .bg(if selected {
            THEME.elevated
        } else {
            THEME.background
        })
        .border_1()
        .border_color(if selected {
            THEME.border
        } else {
            THEME.background
        })
        .hover(|style| {
            if selected {
                style
            } else {
                style.bg(THEME.surface)
            }
        })
        .active(|style| style.opacity(THEME.active_opacity))
        .child(
            div()
                .text_sm()
                .font_weight(if selected {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                })
                .text_color(if selected {
                    THEME.text_primary
                } else {
                    THEME.text_secondary
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
                .text_color(THEME.text)
                .child(title.into()),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_sm()
                .text_color(THEME.text_secondary)
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
        .bg(THEME.surface)
        .border_1()
        .border_color(THEME.border)
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(THEME.text_secondary)
                .child(title.into()),
        )
        .child(body)
}

fn readiness_badge(readiness: Readiness) -> impl IntoElement {
    let (fill, text, border, label) = match readiness {
        Readiness::Ready => (
            THEME.status_ready_fill,
            THEME.status_ready_text,
            THEME.status_ready_border,
            "READY",
        ),
        Readiness::Missing => (
            THEME.status_missing_fill,
            THEME.status_missing_text,
            THEME.status_missing_border,
            "MISSING",
        ),
    };
    div()
        .flex_shrink_0()
        .px_2()
        .py_0p5()
        .rounded_md()
        .bg(fill)
        .border_1()
        .border_color(border)
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(text)
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
                    .bg(THEME.elevated)
                    .border_1()
                    .border_color(THEME.border)
                    .text_color(THEME.text_primary)
                    .cursor_move()
                    .drag_over::<ModuleDrag>(|style, _, _, _| {
                        style.bg(THEME.active).border_color(THEME.border_focused)
                    })
                    .on_drag(drag, |info: &ModuleDrag, position, _, cx| {
                        cx.new(|_| info.clone().position(position))
                    })
                    .on_drop(cx.listener(move |this, info: &ModuleDrag, _, cx| {
                        this.move_module(info.index, index, cx);
                    }))
                    .child(div().flex_shrink_0().text_color(THEME.text_muted).child("⠿"))
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
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(THEME.text_muted)
                                    .child(description),
                            ),
                    )
                    .when_some(readiness, |row, status| row.child(readiness_badge(status)))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(THEME.text_muted)
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
                    .bg(THEME.elevated)
                    .border_1()
                    .border_color(THEME.border_strong)
                    .text_xs()
                    .text_color(THEME.text_secondary)
                    .child(error),
            )
        })
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_xs()
                .text_color(THEME.text_muted)
                .child(
                    "Priority only applies when multiple modules support the selected conversion. Status badges show whether each engine is installed on this Mac (see Diagnostics).",
                ),
        )
}

fn settings_general_panel(
    output_format: OutputFormat,
    history_count: usize,
    history_limit: usize,
    history_limit_input: Entity<TextInput>,
    show_archived: bool,
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
                        .text_color(THEME.text_muted)
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
                .child(div().text_sm().text_color(THEME.text_primary).child(format!(
                    "{history_count} entr{} retained (limit {history_limit}).",
                    if history_count == 1 { "y" } else { "ies" }
                )))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(THEME.text_secondary)
                                .child("Keep up to:"),
                        )
                        .child(
                            div()
                                .w(px(80.0))
                                .min_w_0()
                                .rounded_lg()
                                .bg(THEME.surface)
                                .border_1()
                                .border_color(THEME.border)
                                .text_sm()
                                .text_color(THEME.text_primary)
                                .overflow_hidden()
                                .child(history_limit_input),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(THEME.text_secondary)
                                .child("entries"),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(chip(
                            "show-archived",
                            "Show archived",
                            show_archived,
                            cx,
                            |this, cx| {
                                this.show_archived = !this.show_archived;
                                this.mark_history_cache_dirty();
                                this.persist_session_settings(cx);
                                cx.notify();
                            },
                        )),
                )
                .child(
                    div()
                        .id("settings-clear-history")
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(36.0))
                        .px_4()
                        .rounded_lg()
                        .bg(THEME.elevated)
                        .border_1()
                        .border_color(THEME.border)
                        .text_sm()
                        .text_color(THEME.text_secondary)
                        .cursor_pointer()
                        .hover(|style| style.bg(THEME.hover).text_color(THEME.text_primary))
                        .active(|style| style.opacity(THEME.active_opacity))
                        .child("Clear history")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.clear_history(cx);
                            cx.stop_propagation();
                        })),
                )
                .child(div().text_xs().text_color(THEME.text_muted).child(
                    "History is saved under Application Support and restored when you reopen Shift. Clear removes it from this Mac.",
                )),
        ))
}

fn settings_theme_panel(ui_font_family: &str, cx: &mut Context<Shift>) -> impl IntoElement + use<> {
    let selected = ui_font_family.to_owned();
    let preview_family: SharedString = selected.clone().into();
    let preview_label = ui_font_choice_label(&selected);

    div()
        .flex()
        .flex_col()
        .gap_5()
        .w_full()
        .min_w_0()
        .child(settings_section_header(
            "Theme",
            "Choose the typeface used for Shift’s interface. Changes apply immediately and are kept across launches.",
        ))
        .child(settings_card(
            "Font family",
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
                        .children(UI_FONT_CHOICES.iter().enumerate().map(|(index, (label, family))| {
                            let family_owned = (*family).to_owned();
                            let is_selected = selected == *family;
                            div()
                                .id(("ui-font", index as u64))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_xs()
                                .font_family(*family)
                                .font_weight(FontWeight::MEDIUM)
                                .cursor_pointer()
                                .bg(if is_selected {
                                    THEME.text
                                } else {
                                    THEME.surface
                                })
                                .text_color(if is_selected {
                                    THEME.text_inverse
                                } else {
                                    THEME.text_secondary
                                })
                                .border_1()
                                .border_color(if is_selected {
                                    THEME.text
                                } else {
                                    THEME.border
                                })
                                .hover(|style| {
                                    if is_selected {
                                        style
                                    } else {
                                        style.bg(THEME.hover)
                                    }
                                })
                                .active(|style| style.opacity(THEME.active_opacity))
                                .child(*label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_ui_font_family(family_owned.clone(), cx);
                                    cx.stop_propagation();
                                }))
                        })),
                )
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .p_3()
                        .rounded_lg()
                        .bg(THEME.raised)
                        .border_1()
                        .border_color(THEME.border)
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(THEME.text_muted)
                                .child(format!("Preview · {preview_label}")),
                        )
                        .child(
                            div()
                                .font_family(preview_family)
                                .text_sm()
                                .text_color(THEME.text_primary)
                                .child("The quick brown fox — MD → PDF 0123456789"),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(THEME.text_muted)
                        .child(
                            "Uses fonts installed on this Mac. If a face is missing, the system falls back to a similar default.",
                        ),
                ),
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
    pandoc_citations: bool,
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
                        .text_color(THEME.text_muted)
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
                        .text_color(THEME.text_muted)
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
                            "settings-pandoc-citations",
                            if pandoc_citations {
                                "Citations ✓"
                            } else {
                                "Citations"
                            },
                            pandoc_citations,
                            cx,
                            |this, cx| {
                                this.pandoc_citations = !this.pandoc_citations;
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
                .child(div().text_sm().text_color(THEME.text_primary).child(home))
                .child(
                    div()
                        .text_xs()
                        .text_color(THEME.text_muted)
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
                                .bg(THEME.elevated)
                                .border_1()
                                .border_color(THEME.border)
                                .child(
                                    div()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(THEME.text_primary)
                                        .child(name),
                                )
                                .child(div().text_xs().text_color(THEME.text_muted).child(hint))
                        })),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(THEME.text_muted)
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
                        .text_color(THEME.text_primary)
                        .child(
                            "Format supported means a module registers the conversion pair. Conversion currently available means the required external engine is installed and ready.",
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(THEME.text_muted)
                        .child(
                            "Use `shift-cli formats` for registered capability and `shift-cli doctor` for readiness (exit 0 = at least one engine ready; check complete= in --script for a full install).",
                        ),
                )
                .when_some(summary, |card, text| {
                    card.child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(THEME.text_secondary)
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
                        .text_color(THEME.text_secondary)
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
                        .bg(THEME.elevated)
                        .border_1()
                        .border_color(THEME.border)
                        .text_xs()
                        .text_color(THEME.text_secondary)
                        .cursor_pointer()
                        .hover(|style| style.bg(THEME.hover).text_color(THEME.text_primary))
                        .active(|style| style.opacity(THEME.active_opacity))
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
                                        .bg(THEME.elevated)
                                        .border_1()
                                        .border_color(THEME.border)
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
                                                                .text_color(THEME.text_primary)
                                                                .overflow_hidden()
                                        .text_ellipsis()
                                        .line_clamp(1)
                                                                .child(engine.label),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(THEME.text_muted)
                                                                .overflow_hidden()
                                        .text_ellipsis()
                                        .line_clamp(1)
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
                                                    .text_color(THEME.text_secondary)
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
                                                    .text_color(THEME.text_muted)
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
                                .text_color(THEME.text_muted)
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
                        .text_color(THEME.text_muted)
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
                                        .bg(THEME.surface)
                                        .border_1()
                                        .border_color(THEME.border)
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
                                                        .text_color(THEME.text_primary)
                                                        .child(format!("{}{selected}", engine.name)),
                                                )
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(THEME.text_muted)
                                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .line_clamp(1)
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
                                .text_color(THEME.text_secondary)
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
                        .text_color(THEME.text_primary)
                        .child(format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"))),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(THEME.text_secondary)
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
                        .bg(THEME.elevated)
                        .border_1()
                        .border_color(THEME.border)
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(THEME.text_primary)
                                .child(module_label(id).to_owned()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(THEME.text_muted)
                                .child(id.clone()),
                        )
                })),
        ))
        .child(
            div()
                .text_xs()
                .text_color(THEME.text_muted)
                .child(
                    "MarkItDown · Pandoc · Defuddle · Docling · FFmpeg",
                ),
        )
}

pub(crate) struct SettingsView {
    section: SettingsSection,
    priority: Vec<String>,
    preference_error: Option<SharedString>,
    output_format: OutputFormat,
    history_count: usize,
    history_limit: usize,
    history_limit_input: Entity<TextInput>,
    show_archived: bool,
    ui_font_family: String,
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
    pandoc_citations: bool,
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
        history_limit,
        history_limit_input,
        show_archived,
        ui_font_family,
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
        pandoc_citations,
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
        .bg(THEME.background)
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
                    SettingsSection::General => settings_general_panel(
                        *output_format,
                        *history_count,
                        *history_limit,
                        history_limit_input.clone(),
                        *show_archived,
                        cx,
                    )
                    .into_any_element(),
                    SettingsSection::Theme => {
                        settings_theme_panel(ui_font_family, cx).into_any_element()
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
                        *pandoc_citations,
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
        .bg(THEME.background)
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
                .border_color(THEME.border)
                .child(
                    div()
                        .id("settings-back")
                        .flex()
                        .items_center()
                        .gap_2()
                        .h(px(36.0))
                        .px_3()
                        .rounded_lg()
                        .bg(THEME.surface)
                        .border_1()
                        .border_color(THEME.border)
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(THEME.text_primary)
                        .cursor_pointer()
                        .hover(|style| style.bg(THEME.hover))
                        .active(|style| style.opacity(THEME.active_opacity))
                        .child(div().text_color(THEME.text_secondary).child("←"))
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
                        .child(div().text_color(THEME.text_secondary).child("Settings"))
                        .child(div().text_color(THEME.text_muted).child("/"))
                        .child(div().text_color(THEME.text_primary).child(section.label())),
                )
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(THEME.text_muted)
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
                        .bg(THEME.background)
                        .border_r_1()
                        .border_color(THEME.border)
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
                        .child(settings_nav_item(SettingsSection::Theme, section, 2, cx))
                        .child(settings_nav_item(SettingsSection::Options, section, 3, cx))
                        .child(settings_nav_item(SettingsSection::Paths, section, 4, cx))
                        .child(settings_nav_item(
                            SettingsSection::Diagnostics,
                            section,
                            5,
                            cx,
                        ))
                        .child(settings_nav_item(SettingsSection::About, section, 6, cx)),
                )
                .child(settings_content(&view, cx)),
        )
        .with_animation(
            "settings-screen-in",
            Animation::new(animation::ENTER_DURATION).with_easing(ease_out_quint()),
            |element, progress| element.opacity(0.12 + 0.88 * progress),
        )
}

fn main() {
    crate::app::main();
}

#[cfg(test)]
mod ui_perf {
    //! Performance budgets for pure UI helpers used on the main render path.
    //!
    //! These are not full GPUI frame tests (no window/GPU). They guard the cheap
    //! pure work that still runs every selection change, history restore, batch
    //! update, and options parse — and would freeze the UI if they regress.

    use super::*;
    use crate::app::*;
    use shift_core::history::MAX_HISTORY_ENTRIES;
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    /// Soft wall-clock budget. Loose for unoptimized debug builds; still fails
    /// if a pure helper shells out or turns quadratic.
    fn assert_within(budget: Duration, label: &str, work: impl FnOnce()) {
        let start = Instant::now();
        work();
        let elapsed = start.elapsed();
        assert!(
            elapsed <= budget,
            "{label} took {elapsed:?}, budget {budget:?}"
        );
    }

    fn sample_artifact(id: u64, bytes: Vec<u8>) -> ConversionArtifact {
        ConversionArtifact {
            file_name: format!("file{id}.md"),
            media_type: "text/markdown",
            bytes,
            format: OutputFormat::MARKDOWN,
            module_id: "pandoc",
            pipeline: vec!["pandoc"],
            invocations: Vec::new(),
        }
    }

    fn sample_history_entry(id: u64, outcome: HistoryOutcome) -> ConversionHistoryEntry {
        ConversionHistoryEntry {
            id,
            source: HistorySource::File(PathBuf::from(format!(
                "/Users/me/Documents/report{id}.docx"
            ))),
            name: format!("report{id}.docx").into(),
            detail: "DOCX → Markdown  ·  via Pandoc".into(),
            extension_label: "DOCX".into(),
            badge_color: BADGE_FILL,
            badge_text_color: BADGE_TEXT,
            output_format: OutputFormat::MARKDOWN,
            outcome,
            archived: false,
        }
    }

    fn sample_batch_item(id: u64, state: BatchItemState) -> BatchItem {
        BatchItem {
            id: BatchItemId(id),
            source: BatchSource::File(PathBuf::from(format!("/tmp/input{id}.pdf"))),
            output_format: OutputFormat::MARKDOWN,
            format_selection: BatchFormatSelection::Inherit,
            options: ConversionOptions::default(),
            destination: PathBuf::from(format!("/tmp/out{id}.md")),
            force: false,
            state,
            attempts: 0,
        }
    }

    #[test]
    fn history_detail_makes_input_and_output_explicit() {
        let ready = sample_history_entry(
            1,
            HistoryOutcome::Ready(Arc::new(sample_artifact(1, b"# hi".to_vec()))),
        );
        assert_eq!(
            history_entry_stored_detail(&ready),
            "DOCX → Markdown  ·  via Pandoc"
        );
        assert_eq!(history_entry_detail(&ready).as_ref(), "via Pandoc");
        assert_eq!(
            output_format_badge_label(history_output_format(&ready)),
            "MD"
        );

        let failed = sample_history_entry(2, HistoryOutcome::Failed("boom".into()));
        assert_eq!(
            history_entry_stored_detail(&failed),
            "DOCX → Markdown  ·  failed"
        );
        assert_eq!(history_entry_detail(&failed).as_ref(), "failed");

        let large = sample_history_entry(
            3,
            HistoryOutcome::ReadyLarge {
                module_id: "pandoc".into(),
                byte_len: 2 * 1024 * 1024,
            },
        );
        let stored = history_entry_stored_detail(&large);
        assert!(stored.starts_with("DOCX → Markdown  ·  "));
        assert!(stored.contains("via Pandoc"));
        assert!(stored.contains("re-convert to restore"));
        assert_eq!(output_format_badge_label(OutputFormat::PDF), "PDF");
        assert_eq!(output_format_badge_label(OutputFormat::DOCX), "DOCX");
    }

    #[test]
    fn ellipsize_chars_keeps_short_strings_and_caps_long_ones() {
        assert_eq!(ellipsize_chars("PLAN.md", 42).as_ref(), "PLAN.md");
        assert_eq!(ellipsize_chars("failed", 42).as_ref(), "failed");
        assert_eq!(ellipsize_chars("via Pandoc", 42).as_ref(), "via Pandoc");
        let long = "personal strength week 5 workout.docx";
        let clipped = ellipsize_chars(long, 12);
        assert_eq!(clipped.chars().count(), 12);
        assert!(clipped.as_ref().ends_with('…'));
        assert!(clipped.as_ref().starts_with("personal st"));
        // Must not collapse to a few bare characters the way GPUI Truncate did.
        assert_ne!(ellipsize_chars(long, 40).as_ref(), "per");
        assert_ne!(ellipsize_chars("PLAN.md", 40).as_ref(), "PLA");
    }

    #[test]
    fn format_file_size_stays_fast_across_scales() {
        let sizes = [
            0u64,
            1,
            512,
            1023,
            1024,
            12_345,
            1024 * 1024 - 1,
            1024 * 1024,
            50 * 1024 * 1024,
            1024 * 1024 * 1024,
            5 * 1024 * 1024 * 1024,
            u64::MAX / 2,
        ];
        assert_within(Duration::from_secs(1), "format_file_size×60k", || {
            for _ in 0..5_000 {
                for &size in &sizes {
                    black_box(format_file_size(size));
                }
            }
        });
        assert_eq!(format_file_size(0), "0 B");
        assert_eq!(format_file_size(1024), "1.0 KB");
        assert!(format_file_size(1024 * 1024).contains("MB"));
    }

    #[test]
    fn extension_badge_classifies_common_types_quickly() {
        let paths = [
            "photo.PNG",
            "clip.mp4",
            "track.flac",
            "scan.PDF",
            "archive.zip",
            "main.rs",
            "notes.md",
            "data.json",
            "README",
            "long.extensionname",
            "a.heic",
            "b.webp",
            "c.srt",
            "d.docx",
            "e.pptx",
            "f.toml",
            "g.yaml",
            "h.csv",
            "i.mov",
            "j.mkv",
        ];
        assert_within(Duration::from_secs(1), "extension_badge×20k", || {
            for _ in 0..1_000 {
                for name in paths {
                    black_box(extension_badge(Path::new(name)));
                }
            }
        });
        let (img, _, _) = extension_badge(Path::new("x.jpg"));
        assert_eq!(img, "IMG");
        let (vid, _, _) = extension_badge(Path::new("x.mp4"));
        assert_eq!(vid, "VID");
        let (file, _, _) = extension_badge(Path::new("noext"));
        assert_eq!(file, "FILE");
    }

    #[test]
    fn build_file_preview_with_size_is_cheap_for_large_batches() {
        assert_within(
            Duration::from_secs(1),
            "build_file_preview_with_size×2k",
            || {
                for i in 0..2_000 {
                    let path = PathBuf::from(format!(
                        "/Users/me/Projects/shift/assets/sample_{i:05}.docx"
                    ));
                    black_box(build_file_preview_with_size(
                        &path,
                        format_file_size(12_345 + i as u64),
                    ));
                }
            },
        );
        let preview =
            build_file_preview_with_size(Path::new("/tmp/folder/notes.md"), "1.2 KB".into());
        assert_eq!(preview.name.as_ref(), "notes.md");
        assert!(preview.subtitle.as_ref().contains("folder"));
        assert_eq!(preview.extension_label.as_ref(), "MD");
    }

    #[test]
    fn build_url_preview_handles_many_hosts() {
        assert_within(Duration::from_secs(1), "build_url_preview×5k", || {
            for i in 0..5_000 {
                let host = i % 97;
                let url = format!("https://news.example{host}.com/articles/{i}?q=1#top");
                black_box(build_url_preview(&url));
            }
        });
        let preview = build_url_preview("  HTTPS://Example.COM/path  ");
        assert!(preview.subtitle.as_ref().contains("Example.COM"));
        assert_eq!(preview.extension_label.as_ref(), "WEB");
    }

    #[test]
    fn batch_item_status_labels_scale_with_queue_size() {
        let states = [
            BatchItemState::Queued,
            BatchItemState::Running,
            BatchItemState::Succeeded {
                written_path: PathBuf::from("/Volumes/Exports/out.md"),
                module_id: "pandoc".into(),
                byte_len: 4096,
            },
            BatchItemState::Failed {
                error: "engine missing".into(),
            },
            BatchItemState::Cancelled,
        ];
        let items: Vec<_> = (0..2_000)
            .map(|i| sample_batch_item(i, states[i as usize % states.len()].clone()))
            .collect();

        assert_within(
            Duration::from_secs(1),
            "batch_item_status_label×10 passes",
            || {
                for _ in 0..10 {
                    for item in &items {
                        black_box(batch_item_status_label(item));
                    }
                }
            },
        );

        let ok = batch_item_status_label(&sample_batch_item(
            1,
            BatchItemState::Succeeded {
                written_path: PathBuf::from("/tmp/a.md"),
                module_id: "pandoc".into(),
                byte_len: 1,
            },
        ));
        assert!(ok.as_ref().contains("/tmp/a.md"));
    }

    #[test]
    fn artifact_preview_summary_path_stays_responsive() {
        let text = sample_artifact(1, "# Heading\n\n".repeat(800).into_bytes());
        let binary = ConversionArtifact {
            file_name: "clip.mp4".into(),
            media_type: "video/mp4",
            bytes: vec![0u8; 64 * 1024],
            format: OutputFormat::MP4,
            module_id: "ffmpeg",
            pipeline: vec!["ffmpeg"],
            invocations: Vec::new(),
        };

        assert_within(Duration::from_secs(1), "artifact_preview×400", || {
            for _ in 0..200 {
                black_box(artifact_preview(&text));
                black_box(artifact_preview(&binary));
            }
        });
        assert!(!artifact_preview(&text).is_empty());
        assert!(
            artifact_preview(&binary)
                .as_ref()
                .contains("Not shown inline")
                || artifact_preview(&binary).as_ref().contains("Video")
                || artifact_preview(&binary).as_ref().contains("mp4")
                || !artifact_preview(&binary).is_empty()
        );
    }

    #[test]
    fn option_field_parsers_handle_busy_settings_edits() {
        let secs = ["", "0", "1.5", "  12  ", "abc", "-1", "1e9", "nan"];
        let ints = ["", "0", "30", "  4 ", "x", "-3", "999999"];
        assert_within(Duration::from_secs(1), "parse_optional×15k", || {
            for _ in 0..1_000 {
                for s in secs {
                    let _ = black_box(parse_optional_secs(s));
                }
                for s in ints {
                    let _ = black_box(parse_optional_u32(s));
                }
            }
        });
        assert_eq!(parse_optional_secs("").unwrap(), None);
        assert_eq!(parse_optional_secs("2.5").unwrap(), Some(2.5));
        assert!(parse_optional_secs("-1").is_err());
        assert_eq!(parse_optional_u32("12").unwrap(), Some(12));
        assert!(parse_optional_u32("nope").is_err());
    }

    #[test]
    fn module_chrome_lookups_are_constant_time_style() {
        let ids = [
            "markitdown",
            "pandoc",
            "defuddle",
            "docling",
            "ffmpeg",
            "unknown-module",
            "",
        ];
        assert_within(Duration::from_secs(1), "module_label×70k", || {
            for _ in 0..10_000 {
                for id in ids {
                    black_box(module_label(id));
                    black_box(module_description(id));
                }
            }
        });
        assert_eq!(module_label("pandoc"), "Pandoc");
        assert!(module_description("ffmpeg").contains("Audio"));
    }

    #[test]
    fn settings_section_metadata_is_instant() {
        let sections = [
            SettingsSection::Converters,
            SettingsSection::General,
            SettingsSection::Theme,
            SettingsSection::Options,
            SettingsSection::Paths,
            SettingsSection::Diagnostics,
            SettingsSection::About,
        ];
        assert_within(Duration::from_secs(1), "settings_section×30k", || {
            for _ in 0..5_000 {
                for section in sections {
                    black_box(section.label());
                    black_box(section.description());
                }
            }
        });
        assert_eq!(SettingsSection::Theme.label(), "Theme");
        assert_eq!(SettingsSection::About.label(), "About");
        assert_eq!(ui_font_choice_label("Menlo").as_ref(), "Menlo");
        assert_eq!(ui_font_choice_label(".SystemUIFont").as_ref(), "System");
        assert_eq!(ui_font_choice_label("CustomFace").as_ref(), "CustomFace");
        assert!(!UI_FONT_CHOICES.is_empty());
    }

    #[test]
    fn history_store_round_trip_stays_within_budget_for_full_sidebar() {
        let entries: Vec<_> = (1..=MAX_HISTORY_ENTRIES as u64)
            .map(|id| {
                let outcome = if id % 3 == 0 {
                    HistoryOutcome::Failed("engine missing".into())
                } else if id % 3 == 1 {
                    HistoryOutcome::ReadyLarge {
                        module_id: "ffmpeg".into(),
                        byte_len: 8_000_000,
                    }
                } else {
                    HistoryOutcome::Ready(Arc::new(sample_artifact(
                        id,
                        format!("# body {id}\n").repeat(64).into_bytes(),
                    )))
                };
                sample_history_entry(id, outcome)
            })
            .collect();

        assert_within(
            Duration::from_secs(2),
            "history to/from store ×100",
            || {
                for _ in 0..100 {
                    let stored: Vec<_> = entries.iter().map(to_stored_entry).collect();
                    let loaded = LoadedHistory {
                        entries: stored,
                        next_id: MAX_HISTORY_ENTRIES as u64 + 1,
                    };
                    let (restored, next_id) = history_from_store(loaded);
                    black_box((restored.len(), next_id));
                }
            },
        );

        let stored = to_stored_entry(&entries[0]);
        let back = from_stored_entry(stored).expect("round trip");
        assert_eq!(back.id, entries[0].id);
        assert_eq!(back.name.as_ref(), entries[0].name.as_ref());
    }

    #[test]
    fn history_from_store_skips_bad_formats_without_quadratic_cost() {
        let mut entries = Vec::new();
        for id in 1..=200 {
            entries.push(StoredHistoryEntry {
                id,
                source: StoredSource::File(PathBuf::from(format!("/tmp/{id}.bin"))),
                name: format!("{id}.bin"),
                detail: "x".into(),
                extension_label: "BIN".into(),
                badge_color: 1,
                badge_text_color: 2,
                // Mix valid and invalid so filtering runs.
                output_format: if id % 5 == 0 {
                    "not-a-real-format".into()
                } else {
                    "markdown".into()
                },
                outcome: StoredOutcome::Failed("nope".into()),
                archived: false,
            });
        }
        assert_within(Duration::from_secs(1), "history_from_store filter", || {
            for _ in 0..100 {
                let loaded = LoadedHistory {
                    entries: entries.clone(),
                    next_id: 201,
                };
                let (restored, next_id) = history_from_store(loaded);
                assert_eq!(next_id, 201);
                black_box(restored.len());
            }
        });
    }

    #[test]
    fn ready_state_arc_clone_stays_cheap_for_large_artifacts() {
        // Render clones ConversionState::Ready; Arc keeps that O(1).
        let large = Arc::new(ConversionArtifact {
            file_name: "huge.md".into(),
            media_type: "text/markdown",
            bytes: vec![b'x'; 2 * 1024 * 1024],
            format: OutputFormat::MARKDOWN,
            module_id: "markitdown",
            pipeline: vec!["markitdown"],
            invocations: Vec::new(),
        });
        let state = ConversionState::Ready(large);

        assert_within(Duration::from_secs(1), "Ready Arc clone×50k", || {
            for _ in 0..50_000 {
                black_box(state.clone());
            }
        });

        // Cloning must not deep-copy the payload (still one strong count owner + clones).
        if let ConversionState::Ready(a) = &state {
            assert_eq!(Arc::strong_count(a), 1);
            let cloned = state.clone();
            if let ConversionState::Ready(b) = cloned {
                assert!(Arc::ptr_eq(a, &b));
                assert_eq!(Arc::strong_count(a), 2);
            }
        }
    }

    #[test]
    fn history_outcome_ready_clone_shares_artifact_bytes() {
        let artifact = Arc::new(sample_artifact(9, vec![b'#'; 256 * 1024]));
        let entry = sample_history_entry(9, HistoryOutcome::Ready(artifact.clone()));
        assert_within(Duration::from_secs(1), "history entry clone×20k", || {
            for _ in 0..20_000 {
                black_box(entry.clone());
            }
        });
        if let HistoryOutcome::Ready(a) = &entry.outcome {
            assert_eq!(Arc::strong_count(a), 2); // entry + local
        }
    }

    #[test]
    fn shared_string_preview_fields_construct_quickly() {
        assert_within(Duration::from_secs(1), "SharedString previews×5k", || {
            for i in 0..5_000 {
                let preview = FilePreview {
                    name: format!("document-{i}.pdf").into(),
                    subtitle: format!("{:.1} MB  ·  Downloads", (i % 90) as f64 + 0.1).into(),
                    extension_label: "PDF".into(),
                    badge_color: BADGE_FILL,
                    badge_text_color: BADGE_TEXT,
                };
                black_box(preview.name.as_ref().len() + preview.subtitle.as_ref().len());
            }
        });
    }

    #[test]
    fn output_format_chip_labels_cover_full_catalog_fast() {
        // Format chips iterate OutputFormat::ALL on every selection change.
        assert_within(
            Duration::from_secs(1),
            "format labels×500 catalogs",
            || {
                for _ in 0..500 {
                    for format in OutputFormat::ALL {
                        black_box(format.id());
                        black_box(format.label());
                        black_box(format.extension());
                        black_box(format.is_text_previewable());
                    }
                }
            },
        );
        assert!(!OutputFormat::ALL.is_empty());
    }

    #[test]
    fn conversion_state_variants_construct_without_surprise_alloc_spikes() {
        assert_within(Duration::from_secs(1), "ConversionState×10k", || {
            for i in 0..10_000 {
                let state = match i % 4 {
                    0 => ConversionState::Empty,
                    1 => ConversionState::Converting,
                    2 => {
                        ConversionState::Ready(Arc::new(sample_artifact(i as u64, b"ok".to_vec())))
                    }
                    _ => ConversionState::Failed("boom".into()),
                };
                black_box(matches!(state, ConversionState::Empty));
            }
        });
    }

    #[test]
    fn module_drag_and_output_drag_labels_are_cheap() {
        assert_within(Duration::from_secs(1), "drag structs×10k", || {
            for i in 0..10_000 {
                let drag = ModuleDrag::new(i % 5, format!("Module {i}"))
                    .position(point(px((i % 800) as f32), px((i % 600) as f32)));
                let out = OutputFileDrag::new(format!("out-{i}.md"), point(px(10.0), px(20.0)));
                black_box((drag.index, out.label.as_ref().len()));
            }
        });
    }

    #[test]
    fn panel_width_clamps_respect_min_max_and_center_room() {
        let window = 1180.0;
        // Within range: unchanged.
        assert_eq!(clamp_history_sidebar_width(240.0, window, 470.0), 240.0);
        assert_eq!(clamp_output_panel_width(470.0, window, 240.0), 470.0);

        // Floor / ceiling (absolute max only when the window still has center room).
        assert_eq!(
            clamp_history_sidebar_width(10.0, window, 470.0),
            HISTORY_SIDEBAR_MIN
        );
        assert_eq!(
            clamp_history_sidebar_width(9999.0, 2000.0, OUTPUT_PANEL_MIN),
            HISTORY_SIDEBAR_MAX
        );
        // Absolute max when the window is wide enough; otherwise window room wins.
        let room_limited = window - 470.0 - CENTER_PANEL_MIN - PANEL_RESIZE_HANDLE_WIDTH * 2.0;
        assert_eq!(
            clamp_history_sidebar_width(9999.0, window, 470.0),
            HISTORY_SIDEBAR_MAX.min(room_limited)
        );
        assert_eq!(
            clamp_output_panel_width(10.0, window, 240.0),
            OUTPUT_PANEL_MIN
        );
        assert_eq!(
            clamp_output_panel_width(9999.0, 2000.0, HISTORY_SIDEBAR_MIN),
            OUTPUT_PANEL_MAX
        );

        // Narrow window: leave room for the center column.
        let narrow = 900.0;
        let peer_output = OUTPUT_PANEL_MIN;
        let history = clamp_history_sidebar_width(HISTORY_SIDEBAR_MAX, narrow, peer_output);
        let center_left = narrow - history - peer_output - PANEL_RESIZE_HANDLE_WIDTH * 2.0;
        assert!(
            center_left + 0.5 >= CENTER_PANEL_MIN,
            "history clamp left only {center_left} for center"
        );
        let peer_history = HISTORY_SIDEBAR_MIN;
        let output = clamp_output_panel_width(OUTPUT_PANEL_MAX, narrow, peer_history);
        let center_right = narrow - peer_history - output - PANEL_RESIZE_HANDLE_WIDTH * 2.0;
        assert!(
            center_right + 0.5 >= CENTER_PANEL_MIN,
            "output clamp left only {center_right} for center"
        );
    }
}
