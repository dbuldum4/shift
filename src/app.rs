use crate::*;
use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use shift_core::conversion::PdfCompression;
use shift_core::dependencies::{
    DependencyCapability, InstallOutcome, InstallSelection, available_capabilities,
    install_selected,
};
use std::io;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

#[derive(Clone, Debug)]
pub(crate) enum ConversionState {
    Empty,
    Converting,
    /// Shared so render clones stay cheap for large artifacts.
    Ready(Arc<ConversionArtifact>),
    Failed(SharedString),
}

impl ConversionState {
    pub(crate) fn ready_artifact(&self) -> Option<Arc<ConversionArtifact>> {
        match self {
            ConversionState::Ready(artifact) => Some(Arc::clone(artifact)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum HistorySource {
    File(PathBuf),
    Url(String),
}

#[derive(Clone)]
pub(crate) enum HistoryOutcome {
    Ready(Arc<ConversionArtifact>),
    /// Large media/document not retained in RAM; restore re-runs conversion.
    ReadyLarge {
        module_id: SharedString,
        byte_len: usize,
    },
    Failed(SharedString),
}

#[derive(Clone)]
pub(crate) struct ConversionHistoryEntry {
    pub(crate) id: u64,
    pub(crate) source: HistorySource,
    pub(crate) name: SharedString,
    pub(crate) detail: SharedString,
    pub(crate) extension_label: SharedString,
    pub(crate) badge_color: u32,
    pub(crate) badge_text_color: u32,
    pub(crate) output_format: OutputFormat,
    pub(crate) outcome: HistoryOutcome,
    pub(crate) archived: bool,
    /// Metadata-only history rows must not overwrite an existing deferred blob.
    pub(crate) artifact_deferred: bool,
}

pub(crate) fn to_stored_entry(entry: &ConversionHistoryEntry) -> StoredHistoryEntry {
    let source = match &entry.source {
        HistorySource::File(path) => StoredSource::File(path.clone()),
        HistorySource::Url(url) => {
            StoredSource::Url(shift_core::conversion::redact_url_credentials(url))
        }
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
        archived: entry.archived,
        artifact_deferred: entry.artifact_deferred,
    }
}

pub(crate) fn from_stored_entry(entry: StoredHistoryEntry) -> Option<ConversionHistoryEntry> {
    let artifact_deferred = entry.artifact_deferred;
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
        archived: entry.archived,
        artifact_deferred,
    })
}

pub(crate) fn history_from_store(loaded: LoadedHistory) -> (Vec<ConversionHistoryEntry>, u64) {
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

/// Unpack a loaded store into UI state, including load-error flags that block
/// accidental empty overwrites after a corrupt read.
pub(crate) fn history_from_store_detailed(
    loaded: LoadedHistory,
) -> (Vec<ConversionHistoryEntry>, u64, Option<SharedString>, bool) {
    let load_error = loaded.load_error.clone().map(Into::into);
    let load_incomplete = loaded.load_incomplete;
    let (entries, next_id) = history_from_store(loaded);
    (entries, next_id, load_error, load_incomplete)
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
        OpenFile,
        OpenAbout,
        ClearRecent,
        Minimize,
        Zoom,
        ToggleFullScreen,
    ]
);

/// Open a recently-converted source file from the application menu.
/// Instances carry the file path because the menu is rebuilt when history changes.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = shift, no_json)]
pub(crate) struct OpenRecent {
    pub(crate) path: String,
}

/// Pending folder expansion confirmation before batch enqueue.
#[derive(Clone)]
pub(crate) struct FolderExpandConfirm {
    pub(crate) expanded: Vec<ExpandedInputPath>,
}

/// The short, first-run introduction to Shift.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnboardingStep {
    Welcome,
    HowItWorks,
    Dependencies,
    Ready,
}

pub(crate) struct Shift {
    pub(crate) focus_handle: FocusHandle,
    pub(crate) selected_file: Option<PathBuf>,
    pub(crate) selected_url: Option<String>,
    pub(crate) file_preview: Option<FilePreview>,
    pub(crate) selection_generation: u64,
    pub(crate) conversion_generation: u64,
    pub(crate) conversion: ConversionState,
    pub(crate) save_status: Option<SharedString>,
    pub(crate) preference_error: Option<SharedString>,
    pub(crate) output_format: OutputFormat,
    /// When false, selecting a new source may apply a suggested format.
    pub(crate) user_chose_format: bool,
    pub(crate) output_menu_open: bool,
    pub(crate) format_filter_input: Entity<TextInput>,
    pub(crate) settings_open: bool,
    pub(crate) settings_section: SettingsSection,
    /// Last settings navigation direction, used to make section changes feel spatial.
    pub(crate) settings_tab_direction: crate::ui::animation::SettingsTabDirection,
    /// Saved conversion setups shared with `shift-cli`.
    pub(crate) recipes: Vec<ConversionRecipe>,
    /// Applied recipe name; remains visible when subsequently modified.
    pub(crate) active_recipe: Option<String>,
    pub(crate) recipe_modified: bool,
    pub(crate) recipe_preferred_module: Option<String>,
    pub(crate) recipe_name_input: Entity<TextInput>,
    pub(crate) recipe_naming_input: Entity<TextInput>,
    pub(crate) recipe_status: Option<SharedString>,
    /// `None` once the first-run guide has been dismissed or completed.
    pub(crate) onboarding_step: Option<OnboardingStep>,
    /// Last onboarding navigation direction (for direction-aware step motion).
    pub(crate) onboarding_nav: crate::ui::animation::OnboardingNavDirection,
    /// Optional first-run managed dependency installation state.
    pub(crate) dependency_installing: bool,
    pub(crate) dependency_install_status: Option<SharedString>,
    pub(crate) dependency_install_outcome: Option<InstallOutcome>,
    pub(crate) dependency_install_cancel: Arc<AtomicBool>,
    pub(crate) dependency_capabilities: Vec<DependencyCapability>,
    pub(crate) dependency_selection: Vec<DependencyCapability>,
    /// UI font family for the app chrome (session-persisted Theme setting).
    pub(crate) ui_font_family: String,
    pub(crate) shortcuts_help_open: bool,
    pub(crate) show_command_inspect: bool,
    pub(crate) module_priority: Vec<String>,
    /// Conversion registry with the current module priority applied.
    /// Rebuilt when the priority changes so conversion routes stay consistent.
    pub(crate) registry: Arc<ConversionRegistry>,
    pub(crate) diagnostics: Option<Arc<DiagnosticsReport>>,
    pub(crate) diagnostics_loading: bool,
    /// Cached output formats for the current selection.
    pub(crate) cached_available_outputs: Vec<OutputFormat>,
    /// Formats whose engines are ready (when diagnostics are known).
    pub(crate) cached_ready_outputs: Option<Vec<OutputFormat>>,
    pub(crate) url_input: Entity<TextInput>,
    pub(crate) history: Vec<ConversionHistoryEntry>,
    pub(crate) next_history_id: u64,
    pub(crate) active_history_id: Option<u64>,
    pub(crate) history_search: Entity<TextInput>,
    pub(crate) history_limit_input: Entity<TextInput>,
    pub(crate) history_limit: usize,
    pub(crate) show_archived: bool,
    /// Cached visible history rows, rebuilt when history/search/archived changes.
    pub(crate) cached_history_visible: Vec<ConversionHistoryEntry>,
    pub(crate) cached_history_filter: (String, bool, usize),
    pub(crate) history_cache_dirty: bool,
    /// Ids that need to be persisted (upserted) in the next save, each tagged
    /// with the revision at which it was last modified.
    pub(crate) history_dirty_ids: HashMap<u64, u64>,
    /// Ids that need to be deleted in the next save, each tagged with the
    /// revision at which the deletion was requested.
    pub(crate) history_deleted_ids: HashMap<u64, u64>,
    /// Monotonically increasing revision counter: every history mutation bumps
    /// this so persistence can distinguish stale from current entries.
    pub(crate) history_persist_revision: u64,
    /// True while a background persistence task is in flight (serializes writes).
    pub(crate) history_save_in_flight: bool,
    /// Consecutive failed history save attempts (drives exponential backoff).
    pub(crate) history_save_failures: u32,
    /// When set, a history load was incomplete/corrupt; surface to the user and
    /// avoid treating an empty in-memory list as authority to wipe the store.
    pub(crate) history_load_error: Option<SharedString>,
    pub(crate) history_load_incomplete: bool,
    /// History sidebar width (logical pixels); resizable via left divider.
    pub(crate) history_sidebar_width: f32,
    /// Output panel width (logical pixels); resizable via right divider.
    pub(crate) output_panel_width: f32,
    /// Active column-resize drag, if any.
    pub(crate) panel_resize: Option<PanelResizeDrag>,
    // Shared batch queue (same runner as shift-cli).
    pub(crate) batch_queue: BatchQueue,
    pub(crate) batch_output_dir: Option<PathBuf>,
    pub(crate) batch_running: bool,
    pub(crate) batch_generation: u64,
    pub(crate) batch_cancel: Arc<AtomicBool>,
    pub(crate) batch_status: Option<SharedString>,
    /// Expanded per-item format picker in the batch queue.
    pub(crate) batch_format_menu: Option<BatchItemId>,
    /// Batch file-name template; parsed and applied in shared batch code.
    pub(crate) batch_naming_template_input: Entity<TextInput>,
    /// When true, batch writes overwrite existing outputs (CLI `--force` parity).
    pub(crate) batch_force: bool,
    /// Per-item progress labels/fractions from the batch runner.
    pub(crate) batch_item_progress: HashMap<u64, (Option<f32>, SharedString)>,
    /// Pending recursive folder expansion (confirm before enqueue).
    pub(crate) folder_confirm: Option<FolderExpandConfirm>,
    /// Generation guard for asynchronous folder expansion results.
    pub(crate) folder_expand_generation: u64,
    /// Temporary files materialized from clipboard/remote paste inputs.
    pub(crate) staged_inputs: StagedInputs,
    /// Cooperative cancel for the active single-file / single-URL conversion.
    pub(crate) conversion_cancel: Arc<AtomicBool>,
    /// Live conversion progress (fraction when known + label).
    pub(crate) conversion_progress: Option<(Option<f32>, SharedString)>,
    /// Cached path for the ready artifact (binary copy / reveal / open).
    pub(crate) cached_ready_path: Option<PathBuf>,
    /// User-facing decimal megabyte goal for modules that support fit-to-size.
    pub(crate) target_size_input: Entity<TextInput>,
    // Session conversion options (shown for engines on the active route).
    pub(crate) ffmpeg_quality: FfmpegQuality,
    pub(crate) ffmpeg_encode_mode: FfmpegEncodeMode,
    pub(crate) ffmpeg_mono: bool,
    pub(crate) ffmpeg_mute: bool,
    pub(crate) ffmpeg_normalize: bool,
    pub(crate) ffmpeg_burn_subs: bool,
    pub(crate) ffmpeg_sample_rate_hz: Option<u32>,
    pub(crate) ffmpeg_scale_width: Option<u32>,
    pub(crate) ffmpeg_start_input: Entity<TextInput>,
    pub(crate) ffmpeg_duration_input: Entity<TextInput>,
    pub(crate) ffmpeg_frame_input: Entity<TextInput>,
    pub(crate) ffmpeg_fps_input: Entity<TextInput>,
    pub(crate) ffmpeg_frame_interval_input: Entity<TextInput>,
    pub(crate) ffmpeg_audio_stream_input: Entity<TextInput>,
    pub(crate) ffmpeg_subtitle_stream_input: Entity<TextInput>,
    pub(crate) docling_images: DoclingImageExportMode,
    pub(crate) docling_ocr: bool,
    pub(crate) docling_tables: bool,
    pub(crate) docling_table_mode: DoclingTableMode,
    pub(crate) docling_ocr_lang_input: Entity<TextInput>,
    pub(crate) docling_asr_model: DoclingAsrModel,
    pub(crate) docling_video_sampling_mode: DoclingVideoSamplingMode,
    pub(crate) docling_video_frame_interval_input: Entity<TextInput>,
    pub(crate) docling_video_cuts_per_minute_input: Entity<TextInput>,
    pub(crate) docling_video_prominence_input: Entity<TextInput>,
    pub(crate) docling_video_diarization: bool,
    pub(crate) sips_quality: SipsQuality,
    pub(crate) sips_max_dimension: Option<u32>,
    pub(crate) sips_rotate_degrees: Option<u32>,
    pub(crate) sips_flip: Option<SipsFlip>,
    pub(crate) sips_strip_color_profile: bool,
    pub(crate) spreadsheet_sheet_name_input: Entity<TextInput>,
    pub(crate) spreadsheet_sheet_index_input: Entity<TextInput>,
    pub(crate) defuddle_frontmatter: bool,
    pub(crate) defuddle_lang_input: Entity<TextInput>,
    pub(crate) pandoc_standalone: bool,
    pub(crate) pandoc_toc: bool,
    pub(crate) pandoc_citations: bool,
    pub(crate) pandoc_pdf_engine: Option<String>,
    pub(crate) pandoc_reference_doc: Option<PathBuf>,
    pub(crate) pdf_page_from_input: Entity<TextInput>,
    pub(crate) pdf_page_to_input: Entity<TextInput>,
    pub(crate) pdf_password_input: Entity<TextInput>,
    pub(crate) pdf_rotate_degrees: Option<u16>,
    pub(crate) pdf_compression: PdfCompression,
    pub(crate) pdf_linearize: bool,
    pub(crate) pdf_split_pages_input: Entity<TextInput>,
    pub(crate) markitdown_keep_data_uris: bool,
}

impl Shift {
    pub(crate) fn new(cx: &mut Context<Self>, initial_window_width: f32) -> Self {
        let session = load_default_session_settings();
        let options = session.to_conversion_options();
        let url_input = cx.new(|cx| TextInput::new(cx, "Paste a URL, path, or image…", ""));
        let format_filter_input = cx.new(|cx| TextInput::new(cx, "Filter formats…", ""));
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
        let target_size_input = cx.new(|cx| {
            TextInput::new(
                cx,
                "optional MB",
                options
                    .target_size_bytes
                    .map(crate::format_target_megabytes)
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
        let docling_video_frame_interval_input = cx.new(|cx| {
            TextInput::new(
                cx,
                "10",
                options.docling.video_frame_interval_secs.to_string(),
            )
        });
        let docling_video_cuts_per_minute_input = cx.new(|cx| {
            TextInput::new(
                cx,
                "0 = auto",
                options.docling.video_cuts_per_minute.to_string(),
            )
        });
        let docling_video_prominence_input = cx
            .new(|cx| TextInput::new(cx, "0 = auto", options.docling.video_prominence.to_string()));
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
        let pdf_password_input = cx.new(|cx| {
            let mut input = TextInput::new(cx, "password (not saved)", "");
            input.set_masked(true, cx);
            debug_assert!(input.is_masked());
            input
        });
        let pdf_split_pages_input = cx.new(|cx| {
            TextInput::new(
                cx,
                "1",
                options
                    .pdf
                    .split_pages
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            )
        });
        let spreadsheet_sheet_name_input = cx.new(|cx| {
            TextInput::new(
                cx,
                "sheet name",
                options.spreadsheet.sheet_name.clone().unwrap_or_default(),
            )
        });
        let spreadsheet_sheet_index_input = cx.new(|cx| {
            TextInput::new(
                cx,
                "1",
                options
                    .spreadsheet
                    .sheet_index
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
            )
        });
        let history_search = cx.new(|cx| TextInput::new(cx, "Search history…", ""));
        let history_limit_input =
            cx.new(|cx| TextInput::new(cx, "30", session.history_limit.to_string()));
        let batch_naming_template = session
            .batch_naming_template
            .parse::<BatchNamingTemplate>()
            .unwrap_or_default()
            .to_string();
        let batch_naming_template_input =
            cx.new(|cx| TextInput::new(cx, "{stem}.{ext}", batch_naming_template));
        let recipe_name_input = cx.new(|cx| TextInput::new(cx, "Recipe name", ""));
        let recipe_naming_input = cx.new(|cx| TextInput::new(cx, "{stem}-{format}.{ext}", ""));
        let (recipes, recipe_status) = match load_default_recipe_store() {
            Ok(store) => (store.recipes, None),
            Err(error) => (
                Vec::new(),
                Some(format!("Could not load recipes: {error}").into()),
            ),
        };
        let (history, next_history_id, history_load_error, history_load_incomplete) =
            history_from_store_detailed(load_history());
        let module_priority = load_module_priority();
        let registry = Arc::new(ConversionRegistry::default().with_priority(&module_priority));
        let dependency_capabilities = available_capabilities().unwrap_or_default();
        let cached_available_outputs = OutputFormat::ALL.to_vec();
        let cached_ready_outputs = None;
        let cached_history_filter = (String::new(), session.show_archived, history.len());
        let cached_history_visible = Vec::new();
        let mut batch_queue = BatchQueue::new();
        if let Some(dir) = session.batch_output_dir.as_ref() {
            batch_queue.set_output_dir(Some(dir.as_path()));
        }
        let focus_handle = cx.focus_handle();
        let history_sidebar_width = clamp_history_sidebar_width(
            session.history_sidebar_width,
            initial_window_width,
            session.output_panel_width,
        );
        let output_panel_width = clamp_output_panel_width(
            session.output_panel_width,
            initial_window_width,
            history_sidebar_width,
        );
        Shift {
            focus_handle,
            selected_file: None,
            selected_url: None,
            file_preview: None,
            selection_generation: 0,
            conversion_generation: 0,
            conversion: ConversionState::Empty,
            save_status: history_load_error
                .clone()
                .map(|err| format!("History load issue: {err}").into()),
            preference_error: None,
            output_format: session.output_format(),
            user_chose_format: false,
            output_menu_open: false,
            format_filter_input,
            settings_open: false,
            settings_section: SettingsSection::Converters,
            settings_tab_direction: crate::ui::animation::SettingsTabDirection::Enter,
            recipes,
            active_recipe: None,
            recipe_modified: false,
            recipe_preferred_module: None,
            recipe_name_input,
            recipe_naming_input,
            recipe_status,
            onboarding_step: (!session.onboarding_completed).then_some(OnboardingStep::Welcome),
            onboarding_nav: crate::ui::animation::OnboardingNavDirection::Enter,
            dependency_installing: false,
            dependency_install_status: None,
            dependency_install_outcome: None,
            dependency_install_cancel: Arc::new(AtomicBool::new(false)),
            dependency_capabilities: dependency_capabilities.clone(),
            dependency_selection: dependency_capabilities,
            ui_font_family: session.resolved_ui_font_family().to_owned(),
            shortcuts_help_open: false,
            show_command_inspect: false,
            module_priority,
            registry,
            diagnostics: None,
            diagnostics_loading: false,
            cached_available_outputs,
            cached_ready_outputs,
            url_input,
            history,
            next_history_id,
            active_history_id: None,
            history_search,
            history_limit_input,
            history_limit: session.history_limit,
            show_archived: session.show_archived,
            cached_history_visible,
            cached_history_filter,
            history_cache_dirty: true,
            history_dirty_ids: HashMap::new(),
            history_deleted_ids: HashMap::new(),
            history_persist_revision: 0,
            history_save_in_flight: false,
            history_save_failures: 0,
            history_load_error,
            history_load_incomplete,
            history_sidebar_width,
            output_panel_width,
            panel_resize: None,
            batch_queue,
            batch_output_dir: session.batch_output_dir.clone(),
            batch_running: false,
            batch_generation: 0,
            batch_cancel: Arc::new(AtomicBool::new(false)),
            batch_status: None,
            batch_format_menu: None,
            batch_naming_template_input,
            batch_force: session.batch_force,
            batch_item_progress: HashMap::new(),
            folder_confirm: None,
            folder_expand_generation: 0,
            staged_inputs: StagedInputs::new(),
            conversion_cancel: Arc::new(AtomicBool::new(false)),
            conversion_progress: None,
            cached_ready_path: None,
            target_size_input,
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
            docling_asr_model: options.docling.asr_model,
            docling_video_sampling_mode: options.docling.video_sampling_mode,
            docling_video_frame_interval_input,
            docling_video_cuts_per_minute_input,
            docling_video_prominence_input,
            docling_video_diarization: options.docling.video_diarization,
            sips_quality: options.sips.quality,
            sips_max_dimension: options.sips.max_dimension,
            sips_rotate_degrees: options.sips.rotate_degrees,
            sips_flip: options.sips.flip,
            sips_strip_color_profile: options.sips.strip_color_profile,
            spreadsheet_sheet_name_input,
            spreadsheet_sheet_index_input,
            defuddle_frontmatter: options.defuddle.frontmatter,
            defuddle_lang_input,
            pandoc_standalone: options.pandoc.standalone,
            pandoc_toc: options.pandoc.toc,
            pandoc_citations: options.pandoc.citations,
            pandoc_pdf_engine: options.pandoc.pdf_engine.clone(),
            pandoc_reference_doc: options.pandoc.reference_doc.clone(),
            pdf_page_from_input,
            pdf_page_to_input,
            pdf_password_input,
            pdf_rotate_degrees: options.pdf.rotate_degrees,
            pdf_compression: options.pdf.compression,
            pdf_linearize: options.pdf.linearize,
            pdf_split_pages_input,
            markitdown_keep_data_uris: options.markitdown.keep_data_uris,
        }
    }

    const MAX_HISTORY_RENDERED: usize = 200;

    /// Recompute the cached available/ready output formats for the current
    /// selection and diagnostics. Called when the selection, module priority,
    /// or diagnostics change.
    pub(crate) fn rebuild_output_caches(&mut self) {
        self.cached_available_outputs = if self.selected_url.is_some() {
            self.registry.available_url_outputs()
        } else if let Some(path) = self.selected_file.as_ref() {
            self.registry.available_outputs(path)
        } else {
            OutputFormat::ALL.to_vec()
        };

        self.cached_ready_outputs = self.diagnostics.as_ref().and_then(|report| {
            if self.selected_url.is_some() {
                Some(available_ready_url_outputs(&self.registry, report))
            } else {
                self.selected_file
                    .as_ref()
                    .map(|path| available_ready_outputs(&self.registry, report, path))
            }
        });
    }

    /// Rebuild the filtered, capped history list only when the search query,
    /// archived filter, or history contents have changed.
    pub(crate) fn ensure_history_cache(&mut self, cx: &mut Context<Self>) {
        let search = self.history_search.read(cx).content().to_lowercase();
        let show_archived = self.show_archived;
        let len = self.history.len();
        if !self.history_cache_dirty
            && search == self.cached_history_filter.0
            && show_archived == self.cached_history_filter.1
            && len == self.cached_history_filter.2
        {
            return;
        }

        self.cached_history_filter = (search.clone(), show_archived, len);
        self.cached_history_visible = self
            .history
            .iter()
            .filter(|entry| {
                (show_archived || !entry.archived)
                    && (search.is_empty() || history_matches_search(entry, &search))
            })
            .take(Self::MAX_HISTORY_RENDERED)
            .cloned()
            .collect();
        self.history_cache_dirty = false;
    }

    /// Invalidate the cached history list so the next render rebuilds it.
    pub(crate) fn mark_history_cache_dirty(&mut self) {
        self.history_cache_dirty = true;
    }

    pub(crate) fn refresh_diagnostics(&mut self, cx: &mut Context<Self>) {
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
                let _ = this.rebuild_registry_with_recipe_preference();
                this.diagnostics = Some(Arc::new(report));
                this.diagnostics_loading = false;
                this.rebuild_output_caches();
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn ensure_diagnostics(&mut self, cx: &mut Context<Self>) {
        if self.diagnostics.is_none() && !self.diagnostics_loading {
            self.refresh_diagnostics(cx);
        }
    }

    pub(crate) fn choose_file(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn choose_output_folder(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn set_batch_output_dir(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.batch_running {
            self.batch_status =
                Some("Cannot change output folder while a batch is running.".into());
            cx.notify();
            return;
        }
        file_picker::remember_directory(&path);
        self.batch_output_dir = Some(path.clone());
        self.mark_recipe_modified();
        if let (Ok(options), Ok(naming_template)) = (
            self.build_conversion_options(cx),
            self.current_recipe_naming_template(cx),
        ) {
            self.batch_queue.apply_snapshot_to_queued(
                self.output_format,
                &options,
                Some(path.as_path()),
                self.batch_force,
                &naming_template,
                self.recipe_preferred_module.as_deref(),
            );
        } else {
            self.batch_queue.set_output_dir(Some(path.as_path()));
        }
        self.batch_status = Some(format!("Output folder: {}", path.display()).into());
        self.persist_session_settings(cx);
        cx.notify();
    }

    pub(crate) fn ingest_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        self.folder_expand_generation = self.folder_expand_generation.wrapping_add(1);
        let folder_generation = self.folder_expand_generation;
        let has_dir = paths.iter().any(|path| path.is_dir());
        // An external drop is an intentional first action. Close the
        // introduction before showing the normal conversion workspace.
        if self.onboarding_step.is_some() {
            self.finish_onboarding(cx);
        }
        if has_dir {
            // Folder expansion can walk large trees; never block the UI thread.
            self.folder_confirm = None;
            self.batch_status = Some("Expanding folders…".into());
            cx.notify();
            let task = cx
                .background_executor()
                .spawn(async move { expand_input_paths_preserving_roots(&paths, true) });
            cx.spawn(async move |this, cx| {
                let result = task.await;
                let _ = this.update(cx, |this, cx| {
                    if this.folder_expand_generation != folder_generation {
                        return;
                    }
                    match result {
                        Ok(expanded) => {
                            if expanded.is_empty() {
                                this.batch_status = Some(
                                    "No convertible files found in the selected folder(s).".into(),
                                );
                                this.folder_confirm = None;
                            } else {
                                this.folder_confirm = Some(FolderExpandConfirm {
                                    expanded: expanded.clone(),
                                });
                                this.batch_status = Some(
                                    format!(
                                        "Expand folders? {} file(s) (cap {}). Confirm to queue or dismiss.",
                                        expanded.len(),
                                        MAX_EXPAND_FILES
                                    )
                                    .into(),
                                );
                            }
                            cx.notify();
                        }
                        Err(error) => {
                            this.folder_confirm = None;
                            this.batch_status = Some(error.to_string().into());
                            cx.notify();
                        }
                    }
                });
            })
            .detach();
            return;
        }
        // Global admission for multi-file drops (not only recursive expand).
        if paths.len() > MAX_EXPAND_FILES {
            self.batch_status = Some(
                format!("Too many files (limit is {MAX_EXPAND_FILES}); narrow the selection.")
                    .into(),
            );
            cx.notify();
            return;
        }
        if paths.len() == 1 && self.batch_queue.is_empty() {
            self.set_selected_file(paths[0].clone(), cx);
            return;
        }
        // Queue only; user picks Folder (optional) then Start.
        self.enqueue_paths(paths, false, cx);
    }

    pub(crate) fn confirm_folder_expand(&mut self, cx: &mut Context<Self>) {
        self.folder_expand_generation = self.folder_expand_generation.wrapping_add(1);
        let Some(confirm) = self.folder_confirm.take() else {
            return;
        };
        self.enqueue_expanded_paths(confirm.expanded, false, cx);
    }

    pub(crate) fn dismiss_folder_confirm(&mut self, cx: &mut Context<Self>) {
        self.folder_expand_generation = self.folder_expand_generation.wrapping_add(1);
        self.folder_confirm = None;
        self.batch_status = Some("Folder expansion cancelled.".into());
        cx.notify();
    }

    pub(crate) fn toggle_batch_format_menu(&mut self, id: BatchItemId, cx: &mut Context<Self>) {
        if self.batch_running
            || self
                .batch_queue
                .get(id)
                .is_none_or(|item| !matches!(item.state, BatchItemState::Queued))
        {
            return;
        }
        self.batch_format_menu = (self.batch_format_menu != Some(id)).then_some(id);
        cx.notify();
    }

    pub(crate) fn select_batch_item_format(
        &mut self,
        id: BatchItemId,
        format: OutputFormat,
        cx: &mut Context<Self>,
    ) {
        if self.batch_running {
            return;
        }
        let supported = self
            .batch_queue
            .get(id)
            .map(|item| available_outputs_for_batch_source(&self.registry, &item.source))
            .is_some_and(|available| available.contains(&format));
        if !supported {
            self.batch_status = Some("That output is not supported for this source.".into());
            cx.notify();
            return;
        }
        let already_added = self.batch_queue.get(id).is_some_and(|item| {
            self.batch_queue.items().iter().any(|other| {
                other.id != id
                    && other.group_id == item.group_id
                    && other.resolved_format() == format
            })
        });
        if already_added {
            self.batch_status = Some("That output is already queued for this source.".into());
            cx.notify();
            return;
        }
        if self.batch_queue.set_item_format_selection(
            id,
            BatchFormatSelection::Override(format),
            self.output_format,
            self.batch_output_dir.as_deref(),
        ) {
            self.batch_format_menu = None;
            self.batch_status = Some(format!("Item output set to {}.", format.label()).into());
            cx.notify();
        }
    }

    /// Compatibility action for existing shortcuts/tests: pin the current
    /// global format, then return to inheritance on the next toggle. The batch
    /// row's user-facing picker uses [`Self::select_batch_item_format`] to
    /// choose genuine capability-filtered alternatives.
    #[cfg(test)]
    pub(crate) fn toggle_batch_item_format(&mut self, id: BatchItemId, cx: &mut Context<Self>) {
        if self.batch_running {
            return;
        }
        // Format changes also re-resolve a recipe naming template. Capture the
        // complete current snapshot before mutating the item so invalid custom
        // templates do not leave a partially-updated batch behind.
        let options = match self.build_conversion_options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.batch_status = Some(error.into());
                cx.notify();
                return;
            }
        };
        let naming_template = match self.current_recipe_naming_template(cx) {
            Ok(template) => template,
            Err(error) => {
                self.batch_status = Some(error.into());
                cx.notify();
                return;
            }
        };
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
            // `set_item_format_selection` intentionally has no recipe
            // knowledge. Reapply the complete snapshot here so a format
            // override keeps the saved naming policy, conversion options,
            // overwrite behavior, and preferred module intact.
            self.batch_queue.apply_snapshot_to_queued(
                self.output_format,
                &options,
                self.batch_output_dir.as_deref(),
                self.batch_force,
                &naming_template,
                self.recipe_preferred_module.as_deref(),
            );
            self.batch_status = Some(match selection {
                BatchFormatSelection::Inherit => "Item format: inherit global.".into(),
                BatchFormatSelection::Override(format) => {
                    format!("Item format pinned to {}.", format.label()).into()
                }
            });
            cx.notify();
        }
    }

    pub(crate) fn inherit_batch_item_format(&mut self, id: BatchItemId, cx: &mut Context<Self>) {
        if self.batch_running {
            return;
        }
        if self.batch_queue.set_item_format_selection(
            id,
            BatchFormatSelection::Inherit,
            self.output_format,
            self.batch_output_dir.as_deref(),
        ) {
            self.batch_status = Some("Item output now follows the global format.".into());
            cx.notify();
        }
    }

    pub(crate) fn add_batch_item_output(&mut self, id: BatchItemId, cx: &mut Context<Self>) {
        if self.batch_running {
            return;
        }
        let Some(item) = self.batch_queue.get(id) else {
            return;
        };
        let available = available_outputs_for_batch_source(&self.registry, &item.source);
        let used = self.batch_queue.group_formats(id);
        let Some(format) = available.into_iter().find(|format| !used.contains(format)) else {
            self.batch_status = Some("Every supported output is already queued.".into());
            cx.notify();
            return;
        };
        if self
            .batch_queue
            .add_output_for_item(id, format, self.batch_output_dir.as_deref())
            .is_some()
        {
            self.batch_status =
                Some(format!("Added {} output for this source.", format.label()).into());
            cx.notify();
        }
    }

    pub(crate) fn remove_batch_item(&mut self, id: BatchItemId, cx: &mut Context<Self>) {
        if self.batch_running {
            return;
        }
        if self.batch_queue.remove(id) {
            self.cleanup_unreferenced_staged_inputs();
            if self.batch_format_menu == Some(id) {
                self.batch_format_menu = None;
            }
            self.batch_item_progress.remove(&id.0);
            self.batch_status = Some("Removed queued output.".into());
            cx.notify();
        }
    }

    pub(crate) fn apply_batch_naming_template(&mut self, cx: &mut Context<Self>) {
        if self.batch_running {
            self.batch_status = Some("Cannot change naming while a batch is running.".into());
            cx.notify();
            return;
        }
        let value = self.batch_naming_template_input.read(cx).content();
        match value.parse::<BatchNamingTemplate>() {
            Ok(template) => {
                self.batch_queue.set_naming_template_for_queued(
                    template.clone(),
                    self.batch_output_dir.as_deref(),
                );
                self.batch_status = Some(format!("Naming: {template}").into());
                self.persist_session_settings(cx);
            }
            Err(error) => {
                self.batch_status = Some(error.to_string().into());
            }
        }
        cx.notify();
    }

    pub(crate) fn enqueue_expanded_paths(
        &mut self,
        expanded: Vec<ExpandedInputPath>,
        auto_start: bool,
        cx: &mut Context<Self>,
    ) {
        let mut inputs = Vec::with_capacity(expanded.len());
        for expanded in expanded {
            let relative_parent = expanded.relative_parent().map(Path::to_path_buf);
            let source = BatchSource::File(expanded.path);
            let input = if let Some(relative_parent) = relative_parent {
                match BatchInput::with_relative_parent(source, relative_parent) {
                    Ok(input) => input,
                    Err(error) => {
                        self.batch_status = Some(error.to_string().into());
                        cx.notify();
                        return;
                    }
                }
            } else {
                BatchInput::new(source)
            };
            inputs.push(input);
        }
        self.enqueue_batch_inputs(inputs, auto_start, cx);
    }

    pub(crate) fn enqueue_paths(
        &mut self,
        paths: Vec<PathBuf>,
        auto_start: bool,
        cx: &mut Context<Self>,
    ) {
        let mut sources = Vec::new();
        let mut errors = Vec::new();
        for path in paths {
            match BatchSource::try_from_path_or_url(&path) {
                Ok(source) => sources.push(source),
                Err(error) => errors.push(error),
            }
        }
        if !errors.is_empty() {
            let detail = errors.join("; ");
            self.batch_status = Some(
                if sources.is_empty() {
                    format!("Could not add paths: {detail}")
                } else {
                    format!("Some paths skipped: {detail}")
                }
                .into(),
            );
            if sources.is_empty() {
                cx.notify();
                return;
            }
        }
        self.enqueue_sources(sources, auto_start, cx);
    }

    pub(crate) fn enqueue_sources(
        &mut self,
        sources: Vec<BatchSource>,
        auto_start: bool,
        cx: &mut Context<Self>,
    ) {
        self.enqueue_batch_inputs(
            sources.into_iter().map(BatchInput::new).collect(),
            auto_start,
            cx,
        );
    }

    fn enqueue_batch_inputs(
        &mut self,
        inputs: Vec<BatchInput>,
        auto_start: bool,
        cx: &mut Context<Self>,
    ) {
        if inputs.is_empty() {
            return;
        }
        if self.batch_running {
            self.batch_status =
                Some("Cannot add files while a batch is running. Wait or Cancel first.".into());
            cx.notify();
            return;
        }
        if let Err(error) = self.batch_queue.check_admission(inputs.len()) {
            self.batch_status = Some(error.to_string().into());
            cx.notify();
            return;
        }
        for input in &inputs {
            if let Some(path) = input.source.as_file() {
                file_picker::remember_directory(path);
            }
        }
        let options = match self.build_conversion_options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.batch_status = Some(error.into());
                cx.notify();
                return;
            }
        };
        let naming_template = match self
            .batch_naming_template_input
            .read(cx)
            .content()
            .parse::<BatchNamingTemplate>()
        {
            Ok(template) => template,
            Err(error) => {
                self.batch_status = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        let enqueue = BatchEnqueueOptions {
            output_format: self.output_format,
            conversion: options,
            output_dir: self.batch_output_dir.clone(),
            naming_template,
            force: self.batch_force,
        };
        let count = inputs.len();
        for input in inputs {
            self.batch_queue.enqueue_input_with_recipe(
                input,
                &enqueue,
                self.recipe_preferred_module.as_deref(),
            );
        }
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

    pub(crate) fn start_batch(&mut self, cx: &mut Context<Self>) {
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
        let naming_template = match self
            .batch_naming_template_input
            .read(cx)
            .content()
            .parse::<BatchNamingTemplate>()
        {
            Ok(template) => template,
            Err(error) => {
                self.batch_status = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        self.batch_queue.apply_snapshot_to_queued(
            self.output_format,
            &options,
            self.batch_output_dir.as_deref(),
            self.batch_force,
            &naming_template,
            self.recipe_preferred_module.as_deref(),
        );
        self.batch_item_progress.clear();
        self.batch_format_menu = None;

        // Fresh cancel flag per run so a prior Clear/Cancel cannot be undone by
        // a later start, and so an abandoned worker keeps its own flag.
        self.batch_cancel = Arc::new(AtomicBool::new(false));
        self.batch_running = true;
        self.batch_generation = self.batch_generation.wrapping_add(1);
        let generation = self.batch_generation;
        let registry = Arc::clone(&self.registry);
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
                match done_rx.try_recv() {
                    Ok((queue, summary)) => {
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
                                this.staged_inputs.cleanup();
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
                            if summary.failed == 0 && summary.cancelled == 0 {
                                this.staged_inputs.cleanup();
                            }
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
                    Err(TryRecvError::Disconnected) => {
                        let _ = this.update(cx, |this, cx| {
                            if this.batch_generation == generation {
                                this.batch_running = false;
                                this.batch_status =
                                    Some("Batch worker stopped unexpectedly.".into());
                                cx.notify();
                            } else {
                                this.batch_running = false;
                                this.staged_inputs.cleanup();
                                cx.notify();
                            }
                        });
                        break;
                    }
                    Err(TryRecvError::Empty) => {}
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

    pub(crate) fn toggle_batch_force(&mut self, cx: &mut Context<Self>) {
        if self.batch_running {
            self.batch_status = Some("Cannot change Overwrite while a batch is running.".into());
            cx.notify();
            return;
        }
        self.batch_force = !self.batch_force;
        self.mark_recipe_modified();
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

    pub(crate) fn apply_batch_event(&mut self, event: BatchEvent) {
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
                provenance,
            } => {
                if let Some(item) = self.batch_queue.get_mut(id) {
                    item.state = BatchItemState::Succeeded {
                        written_path: path.clone(),
                        module_id: module_id.clone(),
                        byte_len,
                    };
                    item.destination = path.clone();
                    item.provenance = Some(provenance);
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

    pub(crate) fn cancel_batch(&mut self, cx: &mut Context<Self>) {
        self.batch_cancel.store(true, Ordering::SeqCst);
        if !self.batch_running {
            let n = self.batch_queue.cancel_queued();
            self.batch_status = Some(format!("Cancelled {n} queued item(s)").into());
        } else {
            self.batch_status = Some("Cancelling batch…".into());
        }
        cx.notify();
    }

    pub(crate) fn retry_batch_item(&mut self, id: BatchItemId, cx: &mut Context<Self>) {
        if self.batch_queue.retry(id) {
            self.batch_status = Some("Item re-queued.".into());
            cx.notify();
            if !self.batch_running {
                self.start_batch(cx);
            }
        }
    }

    pub(crate) fn retry_failed_batch(&mut self, cx: &mut Context<Self>) {
        let n = self.batch_queue.retry_failed();
        self.batch_status = Some(format!("Re-queued {n} item(s)").into());
        cx.notify();
        if n > 0 && !self.batch_running {
            self.start_batch(cx);
        }
    }

    pub(crate) fn clear_batch_queue(&mut self, cx: &mut Context<Self>) {
        if self.batch_running {
            self.batch_cancel.store(true, Ordering::SeqCst);
            // Discard the worker result so Clear is durable when the run finishes.
            // Keep batch_running true until that worker exits so start_batch cannot
            // spawn a second concurrent run_batch against overlapping destinations.
            self.batch_generation = self.batch_generation.wrapping_add(1);
            self.batch_status = Some("Clearing queue…".into());
        } else {
            self.batch_status = None;
            self.staged_inputs.cleanup();
        }
        self.batch_queue.clear();
        self.batch_format_menu = None;
        cx.notify();
    }

    /// Keep staged sources alive while any queued item still references them.
    /// The focused file can be changed independently of the batch queue.
    fn staged_path_is_referenced_by_batch(&self, path: &Path) -> bool {
        self.batch_queue
            .items()
            .iter()
            .any(|item| item.source.as_file().is_some_and(|source| source == path))
    }

    fn release_staged_path_if_unreferenced(&mut self, path: &Path) {
        if !self.staged_path_is_referenced_by_batch(path) {
            self.staged_inputs.release(path);
        }
    }

    fn cleanup_unreferenced_staged_inputs(&mut self) {
        let paths = self.staged_inputs.paths().to_vec();
        for path in paths {
            self.release_staged_path_if_unreferenced(&path);
        }
    }

    pub(crate) fn set_selected_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        file_picker::remember_directory(&path);
        self.cancel_active_conversion();
        self.ensure_diagnostics(cx);
        self.selection_generation = self.selection_generation.wrapping_add(1);
        let generation = self.selection_generation;

        if let Some(prev) = self.selected_file.clone() {
            self.release_staged_path_if_unreferenced(&prev);
        }

        self.selected_url = None;
        self.url_input
            .update(cx, |input, cx| input.set_content("", cx));
        // Secrets are source-scoped: never reuse a prior PDF password on a new file.
        self.pdf_password_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.file_preview = Some(build_file_preview_with_size(&path, "…".into()));
        self.selected_file = Some(path.clone());
        self.rebuild_output_caches();
        let available_outputs = &self.cached_available_outputs;
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
        self.conversion = ConversionState::Empty;
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
    }

    pub(crate) fn submit_magic_paste_from_input(&mut self, cx: &mut Context<Self>) {
        let text = self.url_input.read(cx).content().to_owned();
        self.submit_magic_paste_text(text, cx);
    }

    pub(crate) fn submit_magic_paste_text(&mut self, text: String, cx: &mut Context<Self>) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let paste = match parse_magic_paste(trimmed) {
            Ok(paste) => paste,
            Err(error) => {
                self.fail_magic_paste(&error.to_string(), cx);
                return;
            }
        };
        if paste.is_empty() {
            let message = if trimmed.to_ascii_lowercase().starts_with("file:") {
                "Invalid file:// URL — use a local path or a valid file:// path."
            } else {
                "Paste a page URL, file path, file:// link, direct file URL, or image."
            };
            self.fail_magic_paste(message, cx);
            return;
        }
        self.begin_magic_paste_resolve(paste, Some(trimmed.to_owned()), cx);
    }

    pub(crate) fn ingest_clipboard_image(
        &mut self,
        bytes: Vec<u8>,
        extension: &str,
        cx: &mut Context<Self>,
    ) {
        if bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
            self.fail_magic_paste(
                &format!(
                    "clipboard image exceeds size limit ({} bytes)",
                    MAX_CLIPBOARD_IMAGE_BYTES
                ),
                cx,
            );
            return;
        }
        // Clipboard images can be large, so write to the staging directory on
        // the background executor instead of blocking the UI thread.
        let extension = extension.to_owned();
        let task = cx
            .background_executor()
            .spawn(async move { stage_pasted_image(&bytes, &extension) });

        cx.spawn(async move |this, cx| match task.await {
            Ok(path) => this.update(cx, |this, cx| {
                this.url_input
                    .update(cx, |input, cx| input.set_content("", cx));
                this.set_selected_file(path, cx);
            }),
            Err(error) => this.update(cx, |this, cx| {
                this.fail_magic_paste(&error.to_string(), cx);
            }),
        })
        .detach();
    }

    pub(crate) fn fail_magic_paste(&mut self, message: &str, cx: &mut Context<Self>) {
        // Invalidate any in-flight conversion so a late success cannot
        // replace this validation error with an unrelated Ready state.
        self.cancel_active_conversion();
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.conversion_generation = self.conversion_generation.wrapping_add(1);
        if let Some(prev) = self.selected_file.clone() {
            self.release_staged_path_if_unreferenced(&prev);
        }
        self.selected_file = None;
        self.selected_url = None;
        self.file_preview = None;
        self.conversion = ConversionState::Failed(message.to_owned().into());
        self.save_status = None;
        self.output_menu_open = false;
        cx.notify();
    }

    pub(crate) fn begin_magic_paste_resolve(
        &mut self,
        paste: MagicPaste,
        display_text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.cancel_active_conversion();
        self.conversion_cancel = Arc::new(AtomicBool::new(false));
        self.ensure_diagnostics(cx);
        self.selection_generation = self.selection_generation.wrapping_add(1);
        let generation = self.selection_generation;
        if let Some(prev) = self.selected_file.clone() {
            self.release_staged_path_if_unreferenced(&prev);
        }

        // Optimistic preview while downloads / path checks run.
        match paste.tokens() {
            [PasteToken::PageUrl(url)] | [PasteToken::RemoteFileUrl(url)] => {
                self.selected_file = None;
                self.selected_url = Some(url.clone());
                self.file_preview = Some(build_url_preview(url));
                if let Some(text) = display_text.clone() {
                    self.url_input
                        .update(cx, |input, cx| input.set_content(text, cx));
                }
            }
            [PasteToken::LocalPath(path)] => {
                let path = path.clone();
                self.selected_url = None;
                self.selected_file = Some(path.clone());
                self.file_preview = Some(build_file_preview_with_size(&path, "…".into()));
                if let Some(text) = display_text.clone() {
                    self.url_input
                        .update(cx, |input, cx| input.set_content(text, cx));
                }
            }
            _ => {
                self.selected_file = None;
                self.selected_url = None;
                self.file_preview = None;
                if let Some(text) = display_text.clone() {
                    self.url_input
                        .update(cx, |input, cx| input.set_content(text, cx));
                }
            }
        }

        self.conversion = ConversionState::Converting;
        self.conversion_progress = None;
        self.cached_ready_path = None;
        self.show_command_inspect = false;
        self.save_status = None;
        self.output_menu_open = false;
        self.active_history_id = None;
        cx.notify();

        let label = paste
            .tokens()
            .iter()
            .find_map(|token| match token {
                PasteToken::RemoteFileUrl(url) | PasteToken::PageUrl(url) => {
                    Some(format!("Fetching {}…", url_display_host(url)))
                }
                _ => None,
            })
            .unwrap_or_else(|| "Resolving…".into());
        self.conversion_progress = Some((None, label.into()));
        cx.notify();

        let cancel = Arc::clone(&self.conversion_cancel);
        let resolve = cx
            .background_executor()
            .spawn(async move { materialize_magic_paste_detailed(&paste, Some(cancel)) });

        cx.spawn(async move |this, cx| {
            let result = resolve.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation != generation {
                    return;
                }
                match result {
                    Ok(sources) => {
                        this.apply_materialized_sources_detailed(sources, display_text, cx)
                    }
                    Err(error) => {
                        this.selected_file = None;
                        this.selected_url = None;
                        this.file_preview = None;
                        this.conversion_progress = None;
                        this.conversion = ConversionState::Failed(error.to_string().into());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub(crate) fn apply_materialized_sources_detailed(
        &mut self,
        materialized: Vec<MaterializedSource>,
        display_text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let mut sources = Vec::with_capacity(materialized.len());
        for mut item in materialized {
            if let Some(path) = item.take_staged_path() {
                self.staged_inputs.track(path);
            }
            sources.push(item.source.clone());
        }
        self.apply_materialized_sources(sources, display_text, cx);
    }

    pub(crate) fn apply_materialized_sources(
        &mut self,
        sources: Vec<BatchSource>,
        display_text: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.conversion_progress = None;
        match sources.as_slice() {
            [] => {
                self.fail_magic_paste("Nothing to convert from paste.", cx);
            }
            [BatchSource::File(path)] if self.batch_queue.is_empty() => {
                let path = path.clone();
                self.set_selected_file(path, cx);
                if let Some(text) = display_text {
                    self.url_input
                        .update(cx, |input, cx| input.set_content(text, cx));
                }
            }
            [BatchSource::Url(url)] if self.batch_queue.is_empty() => {
                self.set_selected_url(url.clone(), cx);
            }
            _ => {
                // Multiple sources, or a single source while a queue already exists.
                self.enqueue_sources(sources, false, cx);
                if let Some(text) = display_text {
                    self.url_input
                        .update(cx, |input, cx| input.set_content(text, cx));
                }
                // Clear single-file converting state; queue owns the work.
                if !matches!(self.conversion, ConversionState::Ready { .. }) {
                    self.conversion = ConversionState::Empty;
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn set_selected_url(&mut self, url: String, cx: &mut Context<Self>) {
        let url = url.trim().to_owned();
        if url.is_empty() {
            return;
        }
        if !looks_like_url(&url) {
            self.fail_magic_paste(
                "Enter a full http:// or https:// URL to extract with Defuddle.",
                cx,
            );
            return;
        }

        self.cancel_active_conversion();
        self.ensure_diagnostics(cx);
        self.selection_generation = self.selection_generation.wrapping_add(1);
        if let Some(prev) = self.selected_file.clone() {
            self.release_staged_path_if_unreferenced(&prev);
        }
        self.selected_file = None;
        self.selected_url = Some(url.clone());
        self.file_preview = Some(build_url_preview(&url));
        self.url_input
            .update(cx, |input, cx| input.set_content(url.clone(), cx));
        // Secrets are source-scoped: clear any prior PDF password when the source changes.
        self.pdf_password_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.rebuild_output_caches();

        let available_outputs = &self.cached_available_outputs;
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
        self.conversion = ConversionState::Empty;
        self.conversion_progress = None;
        self.cached_ready_path = None;
        self.show_command_inspect = false;
        self.save_status = None;
        self.output_menu_open = false;
        self.active_history_id = None;
        cx.notify();
    }

    pub(crate) fn advance_onboarding(&mut self, cx: &mut Context<Self>) {
        self.onboarding_step = match self.onboarding_step {
            Some(OnboardingStep::Welcome) => {
                self.onboarding_nav = crate::ui::animation::OnboardingNavDirection::Forward;
                Some(OnboardingStep::HowItWorks)
            }
            Some(OnboardingStep::HowItWorks) => {
                self.onboarding_nav = crate::ui::animation::OnboardingNavDirection::Forward;
                Some(OnboardingStep::Dependencies)
            }
            Some(OnboardingStep::Dependencies) => {
                self.onboarding_nav = crate::ui::animation::OnboardingNavDirection::Forward;
                Some(OnboardingStep::Ready)
            }
            Some(OnboardingStep::Ready) => {
                self.finish_onboarding(cx);
                return;
            }
            None => return,
        };
        cx.notify();
    }

    pub(crate) fn previous_onboarding(&mut self, cx: &mut Context<Self>) {
        self.onboarding_step = match self.onboarding_step {
            Some(OnboardingStep::HowItWorks) => {
                self.onboarding_nav = crate::ui::animation::OnboardingNavDirection::Back;
                Some(OnboardingStep::Welcome)
            }
            Some(OnboardingStep::Ready) => {
                self.onboarding_nav = crate::ui::animation::OnboardingNavDirection::Back;
                Some(OnboardingStep::Dependencies)
            }
            Some(OnboardingStep::Dependencies) => {
                self.onboarding_nav = crate::ui::animation::OnboardingNavDirection::Back;
                Some(OnboardingStep::HowItWorks)
            }
            _ => return,
        };
        cx.notify();
    }

    /// Download every managed component published for this release and CPU.
    /// This deliberately ignores the onboarding selection so the Settings
    /// action remains an actual "install all" operation.
    pub(crate) fn install_all_dependencies(&mut self, cx: &mut Context<Self>) {
        if self.dependency_installing {
            return;
        }
        match available_capabilities() {
            Ok(capabilities) if !capabilities.is_empty() => {
                self.start_dependency_install(capabilities, cx)
            }
            Ok(_) => {
                self.dependency_install_status =
                    Some("Managed dependencies are published with official Shift releases.".into());
                cx.notify();
            }
            Err(error) => {
                self.dependency_install_status =
                    Some(format!("Dependency setup is unavailable: {error}").into());
                cx.notify();
            }
        }
    }

    /// Download only the capability groups selected in onboarding.
    pub(crate) fn install_selected_dependencies(&mut self, cx: &mut Context<Self>) {
        self.start_dependency_install(self.dependency_selection.clone(), cx);
    }

    fn start_dependency_install(
        &mut self,
        capabilities: Vec<DependencyCapability>,
        cx: &mut Context<Self>,
    ) {
        if self.dependency_installing {
            return;
        }
        if capabilities.is_empty() {
            self.dependency_install_status =
                Some("Select at least one dependency group to install.".into());
            cx.notify();
            return;
        }
        self.dependency_installing = true;
        self.dependency_install_status = Some("Downloading verified dependencies…".into());
        self.dependency_install_outcome = None;
        self.dependency_install_cancel = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&self.dependency_install_cancel);
        cx.notify();
        let task = cx.background_executor().spawn(async move {
            install_selected(
                &InstallSelection {
                    capabilities,
                    replace_with_managed: Vec::new(),
                },
                &cancel,
                |_| {},
            )
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.dependency_installing = false;
                match result {
                    Ok(outcome) => {
                        let installed = outcome.installed.len();
                        let failed = outcome.failed.len();
                        this.dependency_install_status = Some(if installed == 0 && failed == 0 {
                            "No selected dependency components are available for this release."
                                .into()
                        } else if failed == 0 {
                            format!("Installed {installed} verified dependency component(s).")
                                .into()
                        } else {
                            format!("Installed {installed}; {failed} component(s) need retry.")
                                .into()
                        });
                        this.dependency_install_outcome = Some(outcome);
                        let _ = this.rebuild_registry_with_recipe_preference();
                        this.diagnostics = None;
                        this.refresh_diagnostics(cx);
                    }
                    Err(error) => {
                        this.dependency_install_status =
                            Some(format!("Dependency installation failed: {error}").into())
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn toggle_dependency_capability(
        &mut self,
        capability: DependencyCapability,
        cx: &mut Context<Self>,
    ) {
        if self.dependency_installing {
            return;
        }
        if let Some(index) = self
            .dependency_selection
            .iter()
            .position(|selected| *selected == capability)
        {
            self.dependency_selection.remove(index);
        } else {
            self.dependency_selection.push(capability);
        }
        cx.notify();
    }

    pub(crate) fn cancel_dependency_install(&mut self, cx: &mut Context<Self>) {
        if self.dependency_installing {
            self.dependency_install_cancel
                .store(true, Ordering::Relaxed);
            self.dependency_install_status = Some("Cancelling dependency installation…".into());
            cx.notify();
        }
    }

    pub(crate) fn finish_onboarding(&mut self, cx: &mut Context<Self>) {
        if self.onboarding_step.is_none() {
            return;
        }
        self.onboarding_step = None;
        self.onboarding_nav = crate::ui::animation::OnboardingNavDirection::Enter;
        self.persist_session_settings(cx);
        cx.notify();
    }

    pub(crate) fn start_conversion(&mut self, cx: &mut Context<Self>) {
        // Multi-file work goes through start_batch / run_batch only.
        if !self.batch_queue.is_empty() {
            return;
        }
        // Keep session knobs durable when options change reconverts.
        self.persist_session_settings(cx);
        if let Some(path) = self.selected_file.clone() {
            self.start_source_conversion(BatchSource::File(path), cx);
            return;
        }
        if let Some(url) = self.selected_url.clone() {
            self.start_source_conversion(BatchSource::Url(url), cx);
            return;
        }
        self.conversion = ConversionState::Empty;
        cx.notify();
    }

    pub(crate) fn active_option_modules(&self) -> Vec<&'static str> {
        if self.selected_url.is_some() {
            return self
                .registry
                .url_route_module_ids(self.output_format)
                .unwrap_or_default();
        }
        if let Some(path) = self.selected_file.as_ref() {
            return self
                .registry
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

    pub(crate) fn conversion_options_visible(&self) -> bool {
        !self.active_option_modules().is_empty()
    }

    pub(crate) fn build_conversion_options(&self, cx: &App) -> Result<ConversionOptions, String> {
        let start_secs = parse_optional_secs(self.ffmpeg_start_input.read(cx).content())?;
        let duration_secs = parse_optional_secs(self.ffmpeg_duration_input.read(cx).content())?;
        let frame_secs = parse_optional_secs(self.ffmpeg_frame_input.read(cx).content())?;
        let frame_interval_secs =
            parse_optional_secs(self.ffmpeg_frame_interval_input.read(cx).content())?;
        let fps = parse_optional_secs(self.ffmpeg_fps_input.read(cx).content())?;
        let audio_stream = parse_optional_u32(self.ffmpeg_audio_stream_input.read(cx).content())?;
        let subtitle_stream =
            parse_optional_u32(self.ffmpeg_subtitle_stream_input.read(cx).content())?;
        let target_size_bytes =
            parse_optional_target_megabytes(self.target_size_input.read(cx).content())?;
        let page_from = parse_optional_u32(self.pdf_page_from_input.read(cx).content())?;
        let page_to = parse_optional_u32(self.pdf_page_to_input.read(cx).content())?;
        // Only send split_pages for ZIP output so a leftover field from a prior
        // PDF Pages (ZIP) conversion (or session restore) cannot brick PDF rewrites.
        let split_pages = if self.output_format == OutputFormat::PDF_PAGES_ZIP {
            let split_pages = parse_optional_u32(self.pdf_split_pages_input.read(cx).content())?;
            if split_pages == Some(0) {
                return Err("PDF split group must be at least 1 page".into());
            }
            split_pages
        } else {
            None
        };
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
        let docling_defaults = DoclingOptions::default();
        let docling_video_frame_interval_secs =
            parse_optional_secs(self.docling_video_frame_interval_input.read(cx).content())?
                .unwrap_or(docling_defaults.video_frame_interval_secs);
        if docling_video_frame_interval_secs <= 0.0 {
            return Err("Docling video frame interval must be greater than zero".into());
        }
        let docling_video_cuts_per_minute =
            parse_optional_secs(self.docling_video_cuts_per_minute_input.read(cx).content())?
                .unwrap_or(docling_defaults.video_cuts_per_minute);
        let docling_video_prominence =
            parse_optional_secs(self.docling_video_prominence_input.read(cx).content())?
                .unwrap_or(docling_defaults.video_prominence);
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
                citations: self.pandoc_citations,
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
                asr_model: self.docling_asr_model,
                video_sampling_mode: self.docling_video_sampling_mode,
                video_frame_interval_secs: docling_video_frame_interval_secs,
                video_cuts_per_minute: docling_video_cuts_per_minute,
                video_prominence: docling_video_prominence,
                video_diarization: self.docling_video_diarization,
            },
            sips: SipsOptions {
                max_dimension: self.sips_max_dimension,
                quality: self.sips_quality,
                rotate_degrees: self.sips_rotate_degrees,
                flip: self.sips_flip,
                strip_color_profile: self.sips_strip_color_profile,
            },
            spreadsheet: {
                let sheet_name = self
                    .spreadsheet_sheet_name_input
                    .read(cx)
                    .content()
                    .trim()
                    .to_owned();
                let sheet_index =
                    parse_optional_u32(self.spreadsheet_sheet_index_input.read(cx).content())?;
                if sheet_index == Some(0) {
                    return Err("sheet index is 1-based".into());
                }
                SpreadsheetOptions {
                    sheet_index,
                    sheet_name: if sheet_name.is_empty() {
                        None
                    } else {
                        Some(sheet_name)
                    },
                }
            },
            pdf: PdfInputOptions {
                password,
                page_from,
                page_to,
                rotate_degrees: self.pdf_rotate_degrees,
                compression: self.pdf_compression,
                linearize: self.pdf_linearize,
                split_pages,
            },
            target_size_bytes,
            cancel: None,
            progress: None,
        })
    }

    /// Resolve the active file-name template for batch/recipe work.
    ///
    /// Non-empty recipe naming input wins (so a saved recipe can diverge from the
    /// session default while editing). Otherwise the shared batch template is
    /// used. Both paths go through [`BatchNamingTemplate`] so recipes cannot
    /// persist a pattern the queue cannot render.
    pub(crate) fn current_recipe_naming_template(
        &self,
        cx: &App,
    ) -> Result<BatchNamingTemplate, String> {
        let recipe = self
            .recipe_naming_input
            .read(cx)
            .content()
            .trim()
            .to_owned();
        let source = if recipe.is_empty() {
            self.batch_naming_template_input
                .read(cx)
                .content()
                .to_owned()
        } else {
            recipe
        };
        source
            .parse::<BatchNamingTemplate>()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn mark_recipe_modified(&mut self) {
        if self.active_recipe.is_some() {
            self.recipe_modified = true;
        }
    }

    fn rebuild_registry_with_recipe_preference(&mut self) -> Result<(), String> {
        let mut priority = self.module_priority.clone();
        if let Some(preferred) = self.recipe_preferred_module.as_deref() {
            let registry = ConversionRegistry::default();
            if !registry.has_module(preferred) {
                return Err(format!("Recipe prefers unknown module `{preferred}`."));
            }
            priority.retain(|module| module != preferred);
            priority.insert(0, preferred.to_owned());
        }
        self.registry = Arc::new(ConversionRegistry::default().with_priority(&priority));
        Ok(())
    }

    fn populate_conversion_options(&mut self, options: &ConversionOptions, cx: &mut Context<Self>) {
        let update_text = |input: &Entity<TextInput>, value: String, cx: &mut Context<Self>| {
            input.update(cx, |input, cx| input.set_content(value, cx));
        };
        update_text(
            &self.ffmpeg_start_input,
            options
                .ffmpeg
                .start_secs
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        update_text(
            &self.ffmpeg_duration_input,
            options
                .ffmpeg
                .duration_secs
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        update_text(
            &self.ffmpeg_frame_input,
            options
                .ffmpeg
                .frame_secs
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        update_text(
            &self.ffmpeg_frame_interval_input,
            options
                .ffmpeg
                .frame_interval_secs
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        update_text(
            &self.ffmpeg_fps_input,
            options
                .ffmpeg
                .fps
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        update_text(
            &self.ffmpeg_audio_stream_input,
            options
                .ffmpeg
                .audio_stream
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        update_text(
            &self.ffmpeg_subtitle_stream_input,
            options
                .ffmpeg
                .subtitle_stream
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        update_text(
            &self.docling_ocr_lang_input,
            options.docling.ocr_lang.clone().unwrap_or_default(),
            cx,
        );
        update_text(
            &self.defuddle_lang_input,
            options.defuddle.lang.clone().unwrap_or_default(),
            cx,
        );
        update_text(
            &self.spreadsheet_sheet_name_input,
            options.spreadsheet.sheet_name.clone().unwrap_or_default(),
            cx,
        );
        update_text(
            &self.spreadsheet_sheet_index_input,
            options
                .spreadsheet
                .sheet_index
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        update_text(
            &self.pdf_page_from_input,
            options
                .pdf
                .page_from
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        update_text(
            &self.pdf_page_to_input,
            options
                .pdf
                .page_to
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        // A recipe never supplies a password; clear any previous source secret
        // so it cannot leak into a conversion started by applying the recipe.
        update_text(&self.pdf_password_input, String::new(), cx);

        self.ffmpeg_quality = options.ffmpeg.quality;
        self.ffmpeg_encode_mode = options.ffmpeg.encode_mode;
        self.ffmpeg_mono = options.ffmpeg.mono;
        self.ffmpeg_mute = options.ffmpeg.mute;
        self.ffmpeg_normalize = options.ffmpeg.normalize_audio;
        self.ffmpeg_burn_subs = options.ffmpeg.burn_subtitles;
        self.ffmpeg_sample_rate_hz = options.ffmpeg.sample_rate_hz;
        self.ffmpeg_scale_width = options.ffmpeg.scale_width;
        self.docling_images = options.docling.image_export_mode;
        self.docling_ocr = options.docling.ocr;
        self.docling_tables = options.docling.tables;
        self.docling_table_mode = options.docling.table_mode;
        self.sips_quality = options.sips.quality;
        self.sips_max_dimension = options.sips.max_dimension;
        self.sips_rotate_degrees = options.sips.rotate_degrees;
        self.sips_flip = options.sips.flip;
        self.sips_strip_color_profile = options.sips.strip_color_profile;
        self.defuddle_frontmatter = options.defuddle.frontmatter;
        self.pandoc_standalone = options.pandoc.standalone;
        self.pandoc_toc = options.pandoc.toc;
        self.pandoc_citations = options.pandoc.citations;
        self.pandoc_pdf_engine = options.pandoc.pdf_engine.clone();
        self.pandoc_reference_doc = options.pandoc.reference_doc.clone();
        self.markitdown_keep_data_uris = options.markitdown.keep_data_uris;
    }

    pub(crate) fn save_recipe_from_input(&mut self, cx: &mut Context<Self>) {
        let name = self.recipe_name_input.read(cx).content().trim().to_owned();
        let options = match self.build_conversion_options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.recipe_status = Some(error.into());
                cx.notify();
                return;
            }
        };
        let naming_template = match self.current_recipe_naming_template(cx) {
            Ok(template) => template,
            Err(error) => {
                self.recipe_status = Some(error.into());
                cx.notify();
                return;
            }
        };
        let destination = RecipeDestination {
            output_dir: self.batch_output_dir.clone(),
            naming_template: Some(naming_template.to_string()),
            overwrite: self.batch_force,
        };
        let recipe = match ConversionRecipe::new(
            name.clone(),
            self.output_format,
            self.recipe_preferred_module.clone(),
            &options,
            Some(destination),
        ) {
            Ok(recipe) => recipe,
            Err(error) => {
                self.recipe_status = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        let mut store = match load_default_recipe_store() {
            Ok(store) => store,
            Err(error) => {
                self.recipe_status = Some(format!("Could not load recipes: {error}").into());
                cx.notify();
                return;
            }
        };
        let replaced = match store.upsert(recipe) {
            Ok(replaced) => replaced,
            Err(error) => {
                self.recipe_status = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        if let Err(error) = save_default_recipe_store(&store) {
            self.recipe_status = Some(format!("Could not save recipe: {error}").into());
            cx.notify();
            return;
        }
        self.recipes = store.recipes;
        self.active_recipe = Some(name.clone());
        self.recipe_modified = false;
        self.recipe_status = Some(
            format!(
                "{} recipe “{name}”.",
                if replaced { "Updated" } else { "Saved" }
            )
            .into(),
        );
        cx.notify();
    }

    pub(crate) fn apply_recipe(&mut self, name: &str, cx: &mut Context<Self>) {
        if self.batch_running {
            self.recipe_status = Some("Cannot apply a recipe while a batch is running.".into());
            cx.notify();
            return;
        }
        let Some(recipe) = self
            .recipes
            .iter()
            .find(|recipe| recipe.name.eq_ignore_ascii_case(name))
            .cloned()
        else {
            self.recipe_status = Some(format!("Recipe “{name}” was not found.").into());
            cx.notify();
            return;
        };
        let output_format = match recipe.parsed_output_format() {
            Ok(format) => format,
            Err(error) => {
                self.recipe_status = Some(error.to_string().into());
                cx.notify();
                return;
            }
        };
        if let Some(preferred) = recipe.preferred_module.as_deref()
            && !ConversionRegistry::default().has_module(preferred)
        {
            self.recipe_status =
                Some(format!("Recipe prefers unknown module `{preferred}`.").into());
            cx.notify();
            return;
        }
        let options = recipe.to_conversion_options();
        self.populate_conversion_options(&options, cx);
        self.output_format = output_format;
        self.user_chose_format = true;
        self.output_menu_open = false;
        self.recipe_preferred_module = recipe.preferred_module.clone();
        if let Err(error) = self.rebuild_registry_with_recipe_preference() {
            self.recipe_status = Some(error.into());
            cx.notify();
            return;
        }
        let destination = recipe.destination.clone().unwrap_or_default();
        self.batch_output_dir = destination.output_dir.clone();
        self.batch_force = destination.overwrite;
        self.recipe_naming_input.update(cx, |input, cx| {
            input.set_content(destination.naming_template.clone().unwrap_or_default(), cx);
        });
        let naming_template = destination
            .naming_template
            .as_deref()
            .unwrap_or(BatchNamingTemplate::DEFAULT)
            .parse::<BatchNamingTemplate>()
            .unwrap_or_default();
        self.batch_naming_template_input.update(cx, |input, cx| {
            input.set_content(naming_template.to_string(), cx);
        });
        self.recipe_name_input.update(cx, |input, cx| {
            input.set_content(recipe.name.clone(), cx);
        });
        self.active_recipe = Some(recipe.name.clone());
        self.recipe_modified = false;
        self.recipe_status = Some(format!("Applied recipe “{}”.", recipe.name).into());
        self.rebuild_output_caches();
        self.batch_queue.apply_snapshot_to_queued(
            output_format,
            &options,
            destination.output_dir.as_deref(),
            destination.overwrite,
            &naming_template,
            recipe.preferred_module.as_deref(),
        );
        self.persist_session_settings(cx);
        if self.batch_queue.is_empty() {
            self.start_conversion(cx);
        } else {
            self.batch_status =
                Some(format!("Queued items updated from recipe “{}”.", recipe.name).into());
            cx.notify();
        }
    }

    pub(crate) fn delete_recipe(&mut self, name: &str, cx: &mut Context<Self>) {
        let mut store = match load_default_recipe_store() {
            Ok(store) => store,
            Err(error) => {
                self.recipe_status = Some(format!("Could not load recipes: {error}").into());
                cx.notify();
                return;
            }
        };
        if !store.delete(name) {
            self.recipe_status = Some(format!("Recipe “{name}” was not found.").into());
            cx.notify();
            return;
        }
        if let Err(error) = save_default_recipe_store(&store) {
            self.recipe_status = Some(format!("Could not delete recipe: {error}").into());
            cx.notify();
            return;
        }
        self.recipes = store.recipes;
        if self
            .active_recipe
            .as_deref()
            .is_some_and(|active| active.eq_ignore_ascii_case(name))
        {
            self.active_recipe = None;
            self.recipe_modified = false;
            self.recipe_preferred_module = None;
            let _ = self.rebuild_registry_with_recipe_preference();
            self.rebuild_output_caches();
        }
        self.recipe_status = Some(format!("Deleted recipe “{name}”.").into());
        cx.notify();
    }

    pub(crate) fn set_recipe_preferred_module(
        &mut self,
        module: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.recipe_preferred_module = module;
        self.mark_recipe_modified();
        match self.rebuild_registry_with_recipe_preference() {
            Ok(()) => {
                self.recipe_status = None;
                self.rebuild_output_caches();
                // A batch owns a snapshot of dispatch settings. Keep queued
                // work aligned when the recipe's preferred engine changes;
                // running and terminal entries remain untouched.
                if !self.batch_queue.is_empty() {
                    match (
                        self.build_conversion_options(cx),
                        self.current_recipe_naming_template(cx),
                    ) {
                        (Ok(options), Ok(naming_template)) => {
                            self.batch_queue.apply_snapshot_to_queued(
                                self.output_format,
                                &options,
                                self.batch_output_dir.as_deref(),
                                self.batch_force,
                                &naming_template,
                                self.recipe_preferred_module.as_deref(),
                            );
                        }
                        (Err(error), _) | (_, Err(error)) => {
                            self.recipe_status = Some(error.into());
                            cx.notify();
                            return;
                        }
                    }
                }
                self.start_conversion(cx);
            }
            Err(error) => {
                self.recipe_status = Some(error.into());
                cx.notify();
            }
        }
    }

    pub(crate) fn persist_session_settings(&self, cx: &App) {
        // Never silently clobber a newer schema or quarantine without recovery.
        let loaded = shift_core::load_default_session_settings_detailed();
        if loaded.write_blocked() {
            return;
        }
        let mut settings = loaded.settings();
        settings.set_output_format(self.output_format);
        settings.batch_output_dir = self.batch_output_dir.clone();
        settings.batch_force = self.batch_force;
        if let Ok(template) = self
            .batch_naming_template_input
            .read(cx)
            .content()
            .parse::<BatchNamingTemplate>()
        {
            settings.batch_naming_template = template.to_string();
        }
        settings.history_sidebar_width = self.history_sidebar_width;
        settings.output_panel_width = self.output_panel_width;
        settings.ui_font_family = self.ui_font_family.clone();
        settings.history_limit = self.history_limit;
        settings.show_archived = self.show_archived;
        settings.onboarding_completed = self.onboarding_step.is_none();
        if let Ok(options) = self.build_conversion_options(cx) {
            settings.apply_conversion_options(&options);
        }
        let _ = save_default_session_settings(&settings);
    }

    pub(crate) fn set_ui_font_family(&mut self, family: String, cx: &mut Context<Self>) {
        let family = family.trim().to_owned();
        let family = if family.is_empty() {
            DEFAULT_UI_FONT.to_owned()
        } else {
            family
        };
        if self.ui_font_family == family {
            return;
        }
        self.ui_font_family = family;
        self.persist_session_settings(cx);
        cx.notify();
    }

    pub(crate) fn begin_panel_resize(
        &mut self,
        target: PanelResizeTarget,
        start_x: f32,
        cx: &mut Context<Self>,
    ) {
        let start_width = match target {
            PanelResizeTarget::History => self.history_sidebar_width,
            PanelResizeTarget::Output => self.output_panel_width,
        };
        self.panel_resize = Some(PanelResizeDrag {
            target,
            start_x,
            start_width,
        });
        cx.notify();
    }

    pub(crate) fn handle_panel_resize_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.panel_resize else {
            return;
        };
        let window_width = f32::from(window.viewport_size().width);
        let x = f32::from(event.position.x);
        let delta = x - drag.start_x;
        match drag.target {
            PanelResizeTarget::History => {
                self.history_sidebar_width = clamp_history_sidebar_width(
                    drag.start_width + delta,
                    window_width,
                    self.output_panel_width,
                );
            }
            PanelResizeTarget::Output => {
                // Divider sits to the left of the output panel: drag left → wider output.
                self.output_panel_width = clamp_output_panel_width(
                    drag.start_width - delta,
                    window_width,
                    self.history_sidebar_width,
                );
            }
        }
        cx.notify();
    }

    pub(crate) fn end_panel_resize(&mut self, cx: &mut Context<Self>) {
        if self.panel_resize.take().is_some() {
            self.persist_session_settings(cx);
            cx.notify();
        }
    }

    pub(crate) fn install_hints_for_failure(&self) -> Vec<(SharedString, SharedString)> {
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

    /// Return the already-staged export path if it still matches the active
    /// artifact's bytes. This check is cheap (metadata + hash), so it is safe
    /// on the UI thread.
    fn ready_cached_path(&self) -> Option<PathBuf> {
        let ConversionState::Ready(artifact) = &self.conversion else {
            return None;
        };
        if let Some(path) = self.cached_ready_path.clone() {
            if path.is_file() && export_matches_bytes(&path, &artifact.bytes) {
                return Some(path);
            }
        }
        None
    }

    /// Cache an in-memory artifact and stage it under a user-facing file name.
    ///
    /// This writes bytes to disk and **copies** into the export staging path (never
    /// hard-links the canonical cache entry), so it must run on the background
    /// executor, not the UI thread. After a successful store, runs [`purge_now`]
    /// so TTL/size budgets apply promptly; the app also purges on startup and
    /// should keep calling `purge_now` periodically (idle timer is fine).
    fn stage_ready_artifact(artifact: &ConversionArtifact) -> Result<PathBuf, io::Error> {
        let cache_path = cache_artifact_bytes(&artifact.file_name, &artifact.bytes)?;
        let staged = stage_export_file(&artifact.file_name, &cache_path)?;
        // Best-effort: free expired/oversized cache entries after each write.
        // Leased paths (if any) are skipped inside purge.
        let _ = shift_core::purge_now();
        Ok(staged)
    }

    /// Mark conversion Ready. Export staging is lazy (first Reveal / Open / drag)
    /// so large artifacts do not hitch the UI thread at Ready time.
    pub(crate) fn set_ready_artifact(&mut self, artifact: Arc<ConversionArtifact>) {
        self.conversion = ConversionState::Ready(artifact);
        self.cached_ready_path = None;
    }

    pub(crate) fn copy_output(&mut self, cx: &mut Context<Self>) {
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

        if let Some(path) = self.ready_cached_path() {
            cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
            self.save_status = Some("Copied artifact path to clipboard.".into());
            cx.notify();
            return;
        }

        // Large binary artifacts are staged on the background executor so the
        // UI thread does not block on disk writes / copies.
        let selection_generation = self.selection_generation;
        let conversion_generation = self.conversion_generation;
        let artifact = Arc::clone(artifact);
        let task = cx
            .background_executor()
            .spawn(async move { Self::stage_ready_artifact(&artifact) });
        cx.spawn(async move |this, cx| match task.await {
            Ok(path) => this.update(cx, |this, cx| {
                if this.selection_generation != selection_generation
                    || this.conversion_generation != conversion_generation
                {
                    return;
                }
                this.cached_ready_path = Some(path.clone());
                cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
                this.save_status = Some("Copied artifact path to clipboard.".into());
                cx.notify();
            }),
            Err(error) => this.update(cx, |this, cx| {
                if this.selection_generation != selection_generation
                    || this.conversion_generation != conversion_generation
                {
                    return;
                }
                this.save_status = Some(format!("Could not cache artifact: {error}").into());
                cx.notify();
            }),
        })
        .detach();
    }

    pub(crate) fn reveal_output(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.ready_cached_path() {
            file_picker::reveal_in_finder(&path);
            self.save_status = Some(format!("Revealed · {}", path.display()).into());
            cx.notify();
            return;
        }

        let Some(artifact) = self.conversion.ready_artifact() else {
            return;
        };
        let selection_generation = self.selection_generation;
        let conversion_generation = self.conversion_generation;
        let artifact = Arc::clone(&artifact);
        let task = cx
            .background_executor()
            .spawn(async move { Self::stage_ready_artifact(&artifact) });
        cx.spawn(async move |this, cx| match task.await {
            Ok(path) => this.update(cx, |this, cx| {
                if this.selection_generation != selection_generation
                    || this.conversion_generation != conversion_generation
                {
                    return;
                }
                this.cached_ready_path = Some(path.clone());
                file_picker::reveal_in_finder(&path);
                this.save_status = Some(format!("Revealed · {}", path.display()).into());
                cx.notify();
            }),
            Err(error) => this.update(cx, |this, cx| {
                if this.selection_generation != selection_generation
                    || this.conversion_generation != conversion_generation
                {
                    return;
                }
                this.save_status = Some(format!("Could not cache artifact: {error}").into());
                cx.notify();
            }),
        })
        .detach();
    }

    pub(crate) fn open_output(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.ready_cached_path() {
            file_picker::open_path(&path);
            self.save_status = Some(format!("Opened · {}", path.display()).into());
            cx.notify();
            return;
        }

        let Some(artifact) = self.conversion.ready_artifact() else {
            return;
        };
        let selection_generation = self.selection_generation;
        let conversion_generation = self.conversion_generation;
        let artifact = Arc::clone(&artifact);
        let task = cx
            .background_executor()
            .spawn(async move { Self::stage_ready_artifact(&artifact) });
        cx.spawn(async move |this, cx| match task.await {
            Ok(path) => this.update(cx, |this, cx| {
                if this.selection_generation != selection_generation
                    || this.conversion_generation != conversion_generation
                {
                    return;
                }
                this.cached_ready_path = Some(path.clone());
                file_picker::open_path(&path);
                this.save_status = Some(format!("Opened · {}", path.display()).into());
                cx.notify();
            }),
            Err(error) => this.update(cx, |this, cx| {
                if this.selection_generation != selection_generation
                    || this.conversion_generation != conversion_generation
                {
                    return;
                }
                this.save_status = Some(format!("Could not cache artifact: {error}").into());
                cx.notify();
            }),
        })
        .detach();
    }

    pub(crate) fn pick_reference_doc(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn cancel_active_conversion(&mut self) {
        self.conversion_cancel.store(true, Ordering::SeqCst);
    }

    /// User-facing cancel for the active single-file / single-URL conversion.
    pub(crate) fn cancel_conversion(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn apply_conversion_options(&mut self, cx: &mut Context<Self>) {
        self.mark_recipe_modified();
        self.persist_session_settings(cx);
        self.start_conversion(cx);
    }

    /// Apply a session option change from Settings (reconvert only when relevant).
    pub(crate) fn apply_session_option_change(&mut self, cx: &mut Context<Self>) {
        self.mark_recipe_modified();
        self.persist_session_settings(cx);
        if self.conversion_options_visible() {
            self.start_conversion(cx);
        } else {
            cx.notify();
        }
    }

    pub(crate) fn start_source_conversion(&mut self, source: BatchSource, cx: &mut Context<Self>) {
        // Kill any previous single convert before starting a new one.
        self.cancel_active_conversion();
        self.conversion_cancel = Arc::new(AtomicBool::new(false));
        self.conversion_generation = self.conversion_generation.wrapping_add(1);
        let conversion_generation = self.conversion_generation;
        let generation = self.selection_generation;
        let output_format = self.output_format;
        let registry = Arc::clone(&self.registry);
        let mut options = match self.build_conversion_options(cx) {
            Ok(options) => options,
            Err(error) => {
                self.conversion = ConversionState::Failed(error.into());
                cx.notify();
                return;
            }
        };
        options.cancel = Some(Arc::clone(&self.conversion_cancel));

        let (progress_tx, mut progress_rx) = mpsc::unbounded::<ConversionProgress>();
        options.progress = Some(Arc::new(move |progress| {
            let _ = progress_tx.unbounded_send(progress);
        }));

        self.conversion = ConversionState::Converting;
        let progress_label = match &source {
            BatchSource::Url(url) => {
                format!(
                    "Fetching {} → {}…",
                    url_display_host(url),
                    output_format.label()
                )
            }
            BatchSource::File(_) => format!("Converting to {}…", output_format.label()),
        };
        self.conversion_progress = Some((None, progress_label.into()));
        self.cached_ready_path = None;
        self.save_status = None;
        self.active_history_id = None;
        cx.notify();

        let source_for_check = source.clone();
        let (done_tx, done_rx) = oneshot::channel();
        cx.background_executor()
            .spawn(async move {
                let result = match source {
                    BatchSource::File(path) => {
                        registry.convert_to_with_options(&path, output_format, &options)
                    }
                    BatchSource::Url(url) => {
                        registry.convert_url_with_options(&url, output_format, &options)
                    }
                };
                let _ = done_tx.send(result);
            })
            .detach();

        cx.spawn(async move |this, cx| {
            while let Some(progress) = progress_rx.next().await {
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
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let result = done_rx.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation == generation
                    && this.conversion_generation == conversion_generation
                    && this.source_matches(&source_for_check)
                {
                    this.conversion_progress = None;
                    match result {
                        Ok(Ok(artifact)) => {
                            let artifact = Arc::new(artifact);
                            this.record_history(HistoryOutcome::Ready(Arc::clone(&artifact)), cx);
                            this.set_ready_artifact(artifact);
                            if let BatchSource::File(path) = &source_for_check {
                                this.staged_inputs.release(path);
                            }
                        }
                        Ok(Err(error)) if error.is_cancelled() => {
                            this.conversion =
                                ConversionState::Failed("Conversion cancelled.".into());
                        }
                        Ok(Err(error)) => {
                            let message: SharedString = error.to_string().into();
                            this.record_history(HistoryOutcome::Failed(message.clone()), cx);
                            this.conversion = ConversionState::Failed(message);
                        }
                        Err(_) => {
                            let message: SharedString =
                                "Conversion worker stopped unexpectedly.".into();
                            this.record_history(HistoryOutcome::Failed(message.clone()), cx);
                            this.conversion = ConversionState::Failed(message);
                        }
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn source_matches(&self, source: &BatchSource) -> bool {
        match (source, &self.selected_file, &self.selected_url) {
            (BatchSource::File(path), Some(selected), _) => selected == path,
            (BatchSource::Url(url), _, Some(selected)) => selected == url,
            _ => false,
        }
    }

    pub(crate) fn set_output_format(&mut self, format: OutputFormat, cx: &mut Context<Self>) {
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
        self.mark_recipe_modified();
        self.persist_session_settings(cx);
        if !self.batch_queue.is_empty() {
            let options = match self.build_conversion_options(cx) {
                Ok(options) => options,
                Err(error) => {
                    self.batch_status = Some(error.into());
                    cx.notify();
                    return;
                }
            };
            let naming_template = match self.current_recipe_naming_template(cx) {
                Ok(template) => template,
                Err(error) => {
                    self.batch_status = Some(error.into());
                    cx.notify();
                    return;
                }
            };
            self.batch_queue.apply_snapshot_to_queued(
                format,
                &options,
                self.batch_output_dir.as_deref(),
                self.batch_force,
                &naming_template,
                self.recipe_preferred_module.as_deref(),
            );
            self.batch_status = Some(format!("Queued items updated to {}.", format.label()).into());
            cx.notify();
            // Multi-file mode uses Start / run_batch, not single-file conversion.
            return;
        }
        self.start_conversion(cx);
    }

    pub(crate) fn move_module(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from >= self.module_priority.len() || to >= self.module_priority.len() || from == to {
            return;
        }
        let module = self.module_priority.remove(from);
        self.module_priority.insert(to, module);
        self.recipe_preferred_module = None;
        self.mark_recipe_modified();
        let _ = self.rebuild_registry_with_recipe_preference();
        self.rebuild_output_caches();
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

    pub(crate) fn clear_selected_file(&mut self, cx: &mut Context<Self>) {
        self.cancel_active_conversion();
        self.selection_generation = self.selection_generation.wrapping_add(1);
        if let Some(prev) = self.selected_file.clone() {
            self.release_staged_path_if_unreferenced(&prev);
        }
        self.selected_file = None;
        self.selected_url = None;
        self.url_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.pdf_password_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.file_preview = None;
        self.conversion = ConversionState::Empty;
        self.conversion_progress = None;
        self.cached_ready_path = None;
        self.show_command_inspect = false;
        self.save_status = None;
        self.output_menu_open = false;
        self.active_history_id = None;
        self.rebuild_output_caches();
        cx.notify();
    }

    pub(crate) fn build_app_menus(&self) -> Vec<Menu> {
        vec![
            Menu {
                name: APP_NAME.into(),
                items: vec![
                    MenuItem::action(format!("About {APP_NAME}"), OpenAbout),
                    MenuItem::separator(),
                    MenuItem::action("Preferences…", OpenSettings),
                    MenuItem::separator(),
                    MenuItem::os_submenu("Services", SystemMenuType::Services),
                    MenuItem::separator(),
                    MenuItem::action(format!("Quit {APP_NAME}"), Quit),
                ],
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("Open…", OpenFile),
                    MenuItem::submenu(Menu {
                        name: "Open Recent".into(),
                        items: self.recent_file_menu_items(),
                    }),
                    MenuItem::separator(),
                    MenuItem::action("Save…", SaveOutput),
                ],
            },
            Menu {
                name: "Edit".into(),
                items: vec![
                    MenuItem::action("Cut", text_input::Cut),
                    MenuItem::action("Copy", text_input::Copy),
                    MenuItem::action("Paste", text_input::Paste),
                    MenuItem::separator(),
                    MenuItem::action("Select All", text_input::SelectAll),
                ],
            },
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("Format Menu", ToggleFormatMenu),
                    MenuItem::action("Shortcuts", ShowShortcuts),
                    MenuItem::separator(),
                    MenuItem::action("Toggle Full Screen", ToggleFullScreen),
                ],
            },
            Menu {
                name: "Window".into(),
                items: vec![
                    MenuItem::action("Minimize", Minimize),
                    MenuItem::action("Zoom", Zoom),
                ],
            },
            Menu {
                name: "Help".into(),
                items: vec![MenuItem::action("Keyboard Shortcuts", ShowShortcuts)],
            },
        ]
    }

    pub(crate) fn recent_file_menu_items(&self) -> Vec<MenuItem> {
        let mut seen = std::collections::HashSet::new();
        let mut items = Vec::new();
        for entry in &self.history {
            if let HistorySource::File(path) = &entry.source {
                if seen.insert(path.clone()) {
                    let label = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned());
                    items.push(MenuItem::action(
                        label,
                        OpenRecent {
                            path: path.to_string_lossy().into_owned(),
                        },
                    ));
                    if items.len() >= 10 {
                        break;
                    }
                }
            }
        }
        if items.is_empty() {
            items.push(MenuItem::action(
                "No Recent Items",
                OpenRecent {
                    path: String::new(),
                },
            ));
        } else {
            items.push(MenuItem::separator());
            items.push(MenuItem::action("Clear Recent", ClearRecent));
        }
        items
    }

    pub(crate) fn rebuild_app_menus(&self, cx: &App) {
        cx.set_menus(self.build_app_menus());
    }

    pub(crate) fn action_save_output(
        &mut self,
        _: &SaveOutput,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_output(cx);
    }

    pub(crate) fn action_copy_output(
        &mut self,
        _: &CopyOutput,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_output(cx);
    }

    pub(crate) fn action_reveal_output(
        &mut self,
        _: &RevealOutput,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reveal_output(cx);
    }

    pub(crate) fn action_toggle_format(
        &mut self,
        _: &ToggleFormatMenu,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.output_menu_open = !self.output_menu_open;
        cx.notify();
    }

    pub(crate) fn action_open_settings(
        &mut self,
        _: &OpenSettings,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.output_menu_open = false;
        if !self.settings_open {
            self.settings_tab_direction = crate::ui::animation::SettingsTabDirection::Enter;
        }
        self.settings_open = !self.settings_open;
        if self.settings_open {
            self.ensure_diagnostics(cx);
        }
        cx.notify();
    }

    pub(crate) fn action_show_shortcuts(
        &mut self,
        _: &ShowShortcuts,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shortcuts_help_open = !self.shortcuts_help_open;
        cx.notify();
    }

    pub(crate) fn action_cancel_work(
        &mut self,
        _: &CancelWork,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.onboarding_step.is_some() {
            self.finish_onboarding(cx);
            return;
        }
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

    pub(crate) fn action_open_file(
        &mut self,
        _: &OpenFile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.choose_file(cx);
    }

    pub(crate) fn action_open_about(
        &mut self,
        _: &OpenAbout,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_open {
            self.settings_tab_direction = self
                .settings_section
                .transition_direction_to(SettingsSection::About);
        } else {
            self.settings_tab_direction = crate::ui::animation::SettingsTabDirection::Enter;
        }
        self.settings_section = SettingsSection::About;
        self.settings_open = true;
        cx.notify();
    }

    pub(crate) fn action_open_recent(
        &mut self,
        action: &OpenRecent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if action.path.is_empty() {
            return;
        }
        let path = PathBuf::from(&action.path);
        // Bump the selection generation so any prior in-flight recent-file check
        // becomes stale and cannot overwrite a newer user action.
        self.selection_generation = self.selection_generation.wrapping_add(1);
        let generation = self.selection_generation;
        // Check existence on the background executor so the UI thread does not
        // block on a slow/missing network or removable volume.
        let task = cx.background_executor().spawn({
            let path = path.clone();
            async move { path.exists() }
        });
        cx.spawn(async move |this, cx| {
            let exists = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation != generation {
                    return;
                }
                if exists {
                    this.set_selected_file(path, cx);
                } else {
                    this.conversion = ConversionState::Failed(
                        format!("Recent file not found: {}", path.display()).into(),
                    );
                    this.selected_file = Some(path);
                    this.selected_url = None;
                    this.file_preview = None;
                    this.cached_ready_path = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(crate) fn action_minimize(
        &mut self,
        _: &Minimize,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        window.minimize_window();
    }

    pub(crate) fn action_zoom(&mut self, _: &Zoom, window: &mut Window, _cx: &mut Context<Self>) {
        window.zoom_window();
    }

    pub(crate) fn action_toggle_fullscreen(
        &mut self,
        _: &ToggleFullScreen,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        window.toggle_fullscreen();
    }

    pub(crate) fn action_clear_recent(
        &mut self,
        _: &ClearRecent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_history(cx);
    }

    pub(crate) fn record_history(&mut self, outcome: HistoryOutcome, cx: &mut Context<Self>) {
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

        // Prefer SQLite-backed transactional allocation so concurrent processes
        // sharing the same store cannot mint the same id. Fall back to the
        // in-memory counter when the DB path is unavailable.
        let id = shift_core::history::allocate_history_id_default().unwrap_or_else(|| {
            let id = self.next_history_id;
            self.next_history_id = self.next_history_id.wrapping_add(1).max(1);
            id
        });
        self.next_history_id = self.next_history_id.max(id.saturating_add(1)).max(1);
        let mut entry = ConversionHistoryEntry {
            id,
            source,
            name: preview.name,
            detail: "".into(),
            extension_label: preview.extension_label,
            badge_color: preview.badge_color,
            badge_text_color: preview.badge_text_color,
            output_format: self.output_format,
            outcome,
            archived: false,
            artifact_deferred: false,
        };
        entry.detail = history_entry_stored_detail(&entry).into();
        self.history.insert(0, entry);
        self.history_persist_revision += 1;
        let rev = self.history_persist_revision;
        if self.history.len() > self.history_limit {
            for removed in self.history.split_off(self.history_limit) {
                self.history_deleted_ids.insert(removed.id, rev);
                self.history_dirty_ids.remove(&removed.id);
            }
        }
        self.history_dirty_ids.insert(id, rev);
        self.active_history_id = Some(id);
        self.mark_history_cache_dirty();
        self.persist_history(cx);
        self.rebuild_app_menus(cx);
    }

    pub(crate) fn persist_history(&mut self, cx: &mut Context<Self>) {
        if self.history_dirty_ids.is_empty() && self.history_deleted_ids.is_empty() {
            return;
        }
        if self.history_save_in_flight {
            // Another save will be triggered when the current one finishes.
            return;
        }
        // After a corrupt load, refuse full-reconcile style wipes driven by an
        // empty in-memory list. Delta upserts of explicit dirty ids still run.
        if self.history_load_incomplete
            && self.history.is_empty()
            && self.history_dirty_ids.is_empty()
            && !self.history_deleted_ids.is_empty()
        {
            // Explicit clear while incomplete: fall through so deleted_ids apply.
        }
        let Some(db_path) = history_db_path() else {
            return;
        };
        self.history_save_in_flight = true;
        let history_epoch = shift_core::history::history_store_epoch();

        // Snapshot the revision at submission time. On completion we only clear
        // IDs whose revision is <= this value, so newer mutations remain dirty.
        let snapshot_revision = self.history_persist_revision;
        let stored: Vec<StoredHistoryEntry> = self.history.iter().map(to_stored_entry).collect();
        let changed: Vec<u64> = self.history_dirty_ids.keys().copied().collect();
        let deleted: Vec<u64> = self.history_deleted_ids.keys().copied().collect();

        let task = cx.background_executor().spawn(async move {
            save_history_delta_to_if_current(db_path, &stored, &changed, &deleted, history_epoch)
                .map(|_| ())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.history_save_in_flight = false;
                match result {
                    Ok(()) => {
                        this.history_save_failures = 0;
                        // Only remove entries whose revision has not been bumped
                        // since this snapshot was taken. Newer mutations survive.
                        this.history_dirty_ids
                            .retain(|_id, rev| *rev > snapshot_revision);
                        this.history_deleted_ids
                            .retain(|_id, rev| *rev > snapshot_revision);
                        // Clear a prior save-error banner on success.
                        if this
                            .save_status
                            .as_ref()
                            .is_some_and(|s| s.as_ref().starts_with("Could not save history"))
                        {
                            this.save_status = None;
                        }
                        // If mutations arrived while we were writing, start another save.
                        if !this.history_dirty_ids.is_empty()
                            || !this.history_deleted_ids.is_empty()
                        {
                            this.persist_history(cx);
                        }
                    }
                    Err(error) => {
                        this.history_save_failures = this.history_save_failures.saturating_add(1);
                        let failures = this.history_save_failures;
                        let message = format!("Could not save history: {error}");
                        this.save_status = Some(message.into());
                        cx.notify();
                        // Dirty IDs stay; schedule exponential backoff rather than
                        // hot-looping immediate retries. Cap retries so a permanent
                        // failure does not spin forever.
                        if failures <= shift_core::history::HISTORY_SAVE_MAX_RETRIES {
                            let shift = failures.saturating_sub(1).min(5);
                            let delay_ms = shift_core::history::HISTORY_SAVE_BASE_DELAY_MS
                                .saturating_mul(1u64 << shift);
                            let delay = Duration::from_millis(delay_ms);
                            cx.spawn(async move |this, cx| {
                                cx.background_executor().timer(delay).await;
                                let _ = this.update(cx, |this, cx| {
                                    if !this.history_dirty_ids.is_empty()
                                        || !this.history_deleted_ids.is_empty()
                                    {
                                        this.persist_history(cx);
                                    }
                                });
                            })
                            .detach();
                        }
                        // Beyond max retries: keep dirty, surface error, stop auto-retry.
                        // A subsequent user mutation bumps revision and restarts saves.
                    }
                }
            });
        })
        .detach();
    }

    /// Synchronously flush pending history writes. Used on quit so dirty
    /// mutations are not lost when the process exits before the background
    /// save task completes.
    pub(crate) fn flush_history_blocking(&mut self) {
        if self.history_dirty_ids.is_empty() && self.history_deleted_ids.is_empty() {
            return;
        }
        let Some(db_path) = history_db_path() else {
            return;
        };
        let stored: Vec<StoredHistoryEntry> = self.history.iter().map(to_stored_entry).collect();
        let changed: Vec<u64> = self.history_dirty_ids.keys().copied().collect();
        let deleted: Vec<u64> = self.history_deleted_ids.keys().copied().collect();
        match save_history_delta_to(db_path, &stored, &changed, &deleted) {
            Ok(()) => {
                self.history_dirty_ids.clear();
                self.history_deleted_ids.clear();
                self.history_save_failures = 0;
            }
            Err(error) => {
                self.save_status = Some(format!("Could not save history on quit: {error}").into());
            }
        }
    }

    pub(crate) fn action_quit(&mut self, _: &Quit, _window: &mut Window, cx: &mut Context<Self>) {
        self.flush_history_blocking();
        cx.quit();
    }

    pub(crate) fn restore_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
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
                // Show the captured snapshot immediately, then update from disk
                // on the background executor so the UI thread does not block on
                // metadata/exists calls.
                let snapshot = FilePreview {
                    name: entry.name.clone(),
                    subtitle: entry.detail.clone(),
                    extension_label: entry.extension_label.clone(),
                    badge_color: entry.badge_color,
                    badge_text_color: entry.badge_text_color,
                };
                self.file_preview = Some(snapshot);
                let path = path.clone();
                let preview_path = path.clone();
                let generation = self.selection_generation;
                let task = cx.background_executor().spawn(async move {
                    preview_path
                        .exists()
                        .then(|| build_file_preview(&preview_path))
                });
                cx.spawn(async move |this, cx| {
                    if let Some(preview) = task.await {
                        let _ = this.update(cx, |this, cx| {
                            if this.selection_generation == generation
                                && this.selected_file.as_ref() == Some(&path)
                            {
                                this.file_preview = Some(preview);
                                cx.notify();
                            }
                        });
                    }
                })
                .detach();
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
                // Metadata-first loads leave empty bytes; fetch the BLOB on demand.
                if artifact.bytes.is_empty() {
                    let id = entry.id;
                    let artifact = Arc::clone(&artifact);
                    let task = cx.background_executor().spawn(async move {
                        shift_core::history::load_history_artifact_default(id)
                    });
                    cx.spawn(async move |this, cx| {
                        let loaded = task.await.ok().flatten();
                        let _ = this.update(cx, |this, cx| {
                            if this.active_history_id != Some(id) {
                                return;
                            }
                            if let Some(bytes) = loaded {
                                this.set_ready_artifact(Arc::new(ConversionArtifact {
                                    file_name: artifact.file_name.clone(),
                                    media_type: artifact.media_type,
                                    bytes,
                                    format: artifact.format,
                                    module_id: artifact.module_id,
                                    pipeline: artifact.pipeline.clone(),
                                    invocations: artifact.invocations.clone(),
                                }));
                            } else {
                                this.set_ready_artifact(artifact);
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                } else {
                    self.set_ready_artifact(artifact);
                    cx.notify();
                }
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

    pub(crate) fn clear_history(&mut self, cx: &mut Context<Self>) {
        self.history_persist_revision += 1;
        let rev = self.history_persist_revision;
        for entry in &self.history {
            self.history_deleted_ids.insert(entry.id, rev);
        }
        self.history.clear();
        self.history_dirty_ids.clear();
        self.active_history_id = None;
        self.history_load_incomplete = false;
        self.history_load_error = None;
        self.mark_history_cache_dirty();
        // Remove SQLite + legacy + legacy.bak immediately so clear is durable
        // even if a later delta save is skipped; also resets id sequence.
        let _ = shift_core::history::clear_history_store();
        self.history_deleted_ids.clear();
        self.next_history_id = 1;
        cx.notify();
        self.rebuild_app_menus(cx);
    }

    pub(crate) fn set_history_limit(&mut self, limit: usize, cx: &mut Context<Self>) {
        let limit = limit.clamp(MIN_HISTORY_LIMIT, MAX_HISTORY_LIMIT);
        if self.history_limit == limit {
            return;
        }
        self.history_limit = limit;
        if self.history.len() > self.history_limit {
            self.history_persist_revision += 1;
            let rev = self.history_persist_revision;
            for removed in self.history.split_off(self.history_limit) {
                self.history_deleted_ids.insert(removed.id, rev);
                self.history_dirty_ids.remove(&removed.id);
            }
        }
        self.history_limit_input.update(cx, |input, cx| {
            input.set_content(limit.to_string(), cx);
        });
        self.mark_history_cache_dirty();
        self.persist_session_settings(cx);
        self.persist_history(cx);
        cx.notify();
        self.rebuild_app_menus(cx);
    }

    pub(crate) fn archive_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(entry) = self.history.iter_mut().find(|entry| entry.id == id) {
            entry.archived = !entry.archived;
            if entry.archived && !self.show_archived && self.active_history_id == Some(id) {
                self.active_history_id = None;
            }
            self.history_persist_revision += 1;
            self.history_dirty_ids
                .insert(id, self.history_persist_revision);
            self.mark_history_cache_dirty();
            self.persist_history(cx);
            cx.notify();
            self.rebuild_app_menus(cx);
        }
    }

    pub(crate) fn delete_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        self.history.retain(|entry| entry.id != id);
        self.history_persist_revision += 1;
        let rev = self.history_persist_revision;
        self.history_deleted_ids.insert(id, rev);
        self.history_dirty_ids.remove(&id);
        if self.active_history_id == Some(id) {
            self.active_history_id = None;
        }
        self.mark_history_cache_dirty();
        self.persist_history(cx);
        cx.notify();
        self.rebuild_app_menus(cx);
    }

    pub(crate) fn save_output(&mut self, cx: &mut Context<Self>) {
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

            let save_path = path.clone();
            let result = cx
                .background_executor()
                .spawn(async move { artifact.write_to(&save_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Only attribute save status to the selection that started it.
                if this.selection_generation != selection_generation
                    || this.conversion_generation != conversion_generation
                {
                    return;
                }
                this.save_status = Some(match result {
                    Ok(()) => {
                        // Stage the saved path so Reveal opens the user's file,
                        // not only the cache copy.
                        this.cached_ready_path = Some(path.clone());
                        format!("Saved to {}", path.display()).into()
                    }
                    Err(error) => format!("Could not save: {error}").into(),
                });
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for Shift {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preview = self.file_preview.clone();
        let has_selection = self.selected_file.is_some() || self.selected_url.is_some();
        let conversion = self.conversion.clone();
        let can_convert = has_selection && !matches!(conversion, ConversionState::Converting);
        let save_status = self.save_status.clone();
        let available_outputs = self.cached_available_outputs.clone();
        let ready_outputs = self.cached_ready_outputs.clone();
        self.ensure_history_cache(cx);
        let output_format = self.output_format;
        let onboarding_step = self.onboarding_step;
        let onboarding_nav = self.onboarding_nav;
        let output_menu_open = self.output_menu_open;
        let format_filter_input = self.format_filter_input.clone();
        let format_filter = self.format_filter_input.read(cx).content().to_owned();
        let settings_open = self.settings_open;
        let ui_font_family = self.ui_font_family.clone();
        let recipes = self.recipes.clone();
        let active_recipe = self.active_recipe.clone();
        let recipe_modified = self.recipe_modified;
        let recipe_preferred_module = self.recipe_preferred_module.clone();
        let recipe_name_input = self.recipe_name_input.clone();
        let recipe_naming_input = self.recipe_naming_input.clone();
        let recipe_status = self.recipe_status.clone();
        let shortcuts_help_open = self.shortcuts_help_open;
        let show_command_inspect = self.show_command_inspect;
        let conversion_progress = self.conversion_progress.clone();
        let install_hints = if matches!(self.conversion, ConversionState::Failed(_)) {
            self.install_hints_for_failure()
        } else {
            Vec::new()
        };
        let dependency_installing = self.dependency_installing;
        let dependency_install_status = self.dependency_install_status.clone();
        let dependency_capabilities = self.dependency_capabilities.clone();
        let folder_confirm = self.folder_confirm.clone();
        let settings_section = self.settings_section;
        let settings_tab_direction = self.settings_tab_direction;
        let module_priority = self.module_priority.clone();
        let preference_error = self.preference_error.clone();
        let url_input = self.url_input.clone();
        let history_count = self.history.len();
        let visible_history = &self.cached_history_visible;
        let active_history_id = self.active_history_id;
        let history_sidebar_width = self.history_sidebar_width;
        let history_search = self.history_search.clone();
        let history_limit_input = self.history_limit_input.clone();
        let history_limit = self.history_limit;
        let show_archived = self.show_archived;
        let output_panel_width = self.output_panel_width;
        let resizing_history = matches!(
            self.panel_resize,
            Some(PanelResizeDrag {
                target: PanelResizeTarget::History,
                ..
            })
        );
        let resizing_output = matches!(
            self.panel_resize,
            Some(PanelResizeDrag {
                target: PanelResizeTarget::Output,
                ..
            })
        );
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
        let docling_asr_model = self.docling_asr_model;
        let docling_video_sampling_mode = self.docling_video_sampling_mode;
        let docling_video_frame_interval_input = self.docling_video_frame_interval_input.clone();
        let docling_video_cuts_per_minute_input = self.docling_video_cuts_per_minute_input.clone();
        let docling_video_prominence_input = self.docling_video_prominence_input.clone();
        let docling_video_diarization = self.docling_video_diarization;
        let docling_video_input = self
            .selected_file
            .as_deref()
            .is_some_and(is_docling_video_input);
        let docling_timed_input = self
            .selected_file
            .as_deref()
            .is_some_and(is_docling_timed_input);
        let sips_quality = self.sips_quality;
        let sips_max_dimension = self.sips_max_dimension;
        let sips_rotate_degrees = self.sips_rotate_degrees;
        let sips_flip = self.sips_flip;
        let sips_strip_color_profile = self.sips_strip_color_profile;
        let spreadsheet_sheet_name_input = self.spreadsheet_sheet_name_input.clone();
        let spreadsheet_sheet_index_input = self.spreadsheet_sheet_index_input.clone();
        let defuddle_frontmatter = self.defuddle_frontmatter;
        let defuddle_lang_input = self.defuddle_lang_input.clone();
        let pandoc_standalone = self.pandoc_standalone;
        let pandoc_toc = self.pandoc_toc;
        let pandoc_citations = self.pandoc_citations;
        let pandoc_pdf_engine = self.pandoc_pdf_engine.clone();
        let pandoc_reference_doc = self.pandoc_reference_doc.clone();
        let pdf_page_from_input = self.pdf_page_from_input.clone();
        let pdf_page_to_input = self.pdf_page_to_input.clone();
        let pdf_password_input = self.pdf_password_input.clone();
        let pdf_rotate_degrees = self.pdf_rotate_degrees;
        let pdf_compression = self.pdf_compression;
        let pdf_linearize = self.pdf_linearize;
        let pdf_split_pages_input = self.pdf_split_pages_input.clone();
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
        let batch_naming_template_input = self.batch_naming_template_input.clone();
        let batch_format_menu = self.batch_format_menu;
        let batch_available_formats: HashMap<u64, Vec<OutputFormat>> = batch_items
            .iter()
            .map(|item| {
                (
                    item.id.0,
                    available_outputs_for_batch_source(&self.registry, &item.source),
                )
            })
            .collect();
        let cached_ready_path = self.cached_ready_path.clone();
        let target_size_input = self.target_size_input.clone();
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
            .on_action(cx.listener(Self::action_open_file))
            .on_action(cx.listener(Self::action_open_about))
            .on_action(cx.listener(Self::action_open_recent))
            .on_action(cx.listener(Self::action_minimize))
            .on_action(cx.listener(Self::action_zoom))
            .on_action(cx.listener(Self::action_toggle_fullscreen))
            .on_action(cx.listener(Self::action_clear_recent))
            .on_action(cx.listener(Self::action_quit))
            .relative()
            .flex()
            .size_full()
            .bg(THEME.background)
            .text_color(THEME.text)
            .font_family(ui_font_family.clone())
            .on_click(cx.listener(|this, _, _, cx| {
                if this.output_menu_open {
                    this.output_menu_open = false;
                    cx.notify();
                }
                if this.batch_format_menu.take().is_some() {
                    cx.notify();
                }
            }))
            .child(history_sidebar(
                visible_history,
                history_count,
                history_search,
                active_history_id,
                history_sidebar_width,
                cx,
            ))
            .child(vertical_resize_handle(
                "resize-history",
                PanelResizeTarget::History,
                resizing_history,
                false,
                cx,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .p_8()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(url_input_bar(url_input, window, cx))
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
                            .bg(THEME.drop_target)
                            .hover(|style| style.bg(THEME.drop_target_hover))
                            .cursor_pointer()
                            .active(|style| style.opacity(THEME.active_opacity))
                            .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(THEME.elevated))
                            .child(rounded_dashed_border(has_selection || show_batch))
                            .when_some(preview, |zone, preview| {
                                zone.child(file_preview_card(preview, can_convert, cx))
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
            .child(vertical_resize_handle(
                "resize-output",
                PanelResizeTarget::Output,
                resizing_output,
                true,
                cx,
            ))
            .child(
                div()
                    .w(px(output_panel_width))
                    .flex_shrink_0()
                    .h_full()
                    .min_h_0()
                    .overflow_hidden()
                    .bg(THEME.background)
                    .when(show_batch, |panel| {
                        panel.child(batch_queue_panel(
                            BatchPanelView {
                                items: &batch_items,
                                output_dir: batch_output_dir.as_deref(),
                                running: batch_running,
                                force: batch_force,
                                status: batch_status,
                                item_progress: &batch_item_progress,
                                naming_template_input: batch_naming_template_input,
                                format_menu: batch_format_menu,
                                available_formats: &batch_available_formats,
                            },
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
                                active_recipe: active_recipe
                                    .clone()
                                    .map(|name| (name, recipe_modified)),
                                show_conversion_options,
                                cached_ready_path,
                                conversion_options: ConversionPanelView {
                                    active_modules: active_option_modules,
                                    output_format,
                                    target_size_input,
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
                                    docling_asr_model,
                                    docling_video_sampling_mode,
                                    docling_video_frame_interval_input,
                                    docling_video_cuts_per_minute_input,
                                    docling_video_prominence_input,
                                    docling_video_diarization,
                                    docling_video_input,
                                    docling_timed_input,
                                    sips_quality,
                                    sips_max_dimension,
                                    sips_rotate_degrees,
                                    sips_flip,
                                    sips_strip_color_profile,
                                    spreadsheet_sheet_name_input,
                                    spreadsheet_sheet_index_input,
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
                                    pdf_rotate_degrees,
                                    pdf_compression,
                                    pdf_linearize,
                                    pdf_split_pages_input,
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
                    .bg(THEME.surface)
                    .border_1()
                    .border_color(THEME.border)
                    .text_color(THEME.text_secondary)
                    .cursor_pointer()
                    .hover(|style| style.bg(THEME.hover).text_color(THEME.text_primary))
                    .active(|style| style.opacity(THEME.active_opacity))
                    .child("⚙")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.output_menu_open = false;
                        this.settings_tab_direction =
                            crate::ui::animation::SettingsTabDirection::Enter;
                        this.settings_open = true;
                        this.ensure_diagnostics(cx);
                        cx.notify();
                    })),
            )
            // Full-window hit target while dragging so moves outside the thin handle still track.
            .when(resizing_history || resizing_output, |root| {
                root.child(
                    div()
                        .id("panel-resize-capture")
                        .absolute()
                        .inset_0()
                        .cursor(CursorStyle::ResizeColumn)
                        .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                            this.handle_panel_resize_move(event, window, cx);
                            cx.stop_propagation();
                        }))
                        .on_mouse_up(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.end_panel_resize(cx);
                                cx.stop_propagation();
                            }),
                        )
                        .on_mouse_up_out(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.end_panel_resize(cx);
                            }),
                        ),
                )
            })
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
                        .bg(THEME.scrim)
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
                                .bg(THEME.elevated)
                                .border_1()
                                .border_color(THEME.border_strong)
                                .shadow(card_shadow())
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
                                        .text_color(THEME.text_secondary)
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
                                )
                                .with_animation(
                                    "folder-confirm-dialog-in",
                                    Animation::new(animation::DIALOG_DURATION)
                                        .with_easing(ease_out_quint()),
                                    |element, progress| element.opacity(0.12 + 0.88 * progress),
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
                        .bg(THEME.scrim)
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
                                .bg(THEME.elevated)
                                .border_1()
                                .border_color(THEME.border_strong)
                                .shadow(card_shadow())
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
                                                    .text_color(THEME.text_primary)
                                                    .child(key),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(THEME.text_secondary)
                                                    .child(desc),
                                            )
                                    }),
                                )
                                .with_animation(
                                    "shortcuts-help-dialog-in",
                                    Animation::new(animation::DIALOG_DURATION)
                                        .with_easing(ease_out_quint()),
                                    |element, progress| element.opacity(0.12 + 0.88 * progress),
                                ),
                        ),
                )
            })
            .when(settings_open, |root| {
                root.child(settings_screen(
                    SettingsView {
                        section: settings_section,
                        tab_direction: settings_tab_direction,
                        priority: module_priority,
                        preference_error,
                        output_format,
                        recipes,
                        active_recipe,
                        recipe_modified,
                        recipe_preferred_module,
                        recipe_name_input,
                        recipe_naming_input,
                        recipe_status,
                        batch_output_dir: batch_output_dir.clone(),
                        batch_force,
                        history_count,
                        history_limit,
                        history_limit_input,
                        show_archived,
                        ui_font_family,
                        quality: ffmpeg_quality,
                        encode_mode: ffmpeg_encode_mode,
                        mono: ffmpeg_mono,
                        docling_images,
                        docling_ocr,
                        docling_tables,
                        docling_table_mode,
                        docling_asr_model,
                        docling_video_sampling_mode,
                        docling_video_diarization,
                        defuddle_frontmatter,
                        pandoc_standalone,
                        pandoc_toc,
                        pandoc_citations,
                        markitdown_keep_data_uris,
                        diagnostics,
                        diagnostics_loading,
                        dependency_installing,
                        dependency_install_status,
                    },
                    cx,
                ))
            })
            .when_some(onboarding_step, |root, step| {
                root.child(onboarding_overlay(
                    step,
                    onboarding_nav,
                    self.dependency_installing,
                    self.dependency_install_status.clone(),
                    dependency_capabilities,
                    self.dependency_selection.clone(),
                    cx,
                ))
            })
    }
}

/// Finder's `Open With` and `open -a Shift file1 file2` hand local paths to
/// the app process. Ignore launch-services bookkeeping flags and only accept
/// paths that currently exist; normal UI selection still handles errors.
fn startup_file_paths() -> Vec<PathBuf> {
    startup_file_paths_from(std::env::args_os().skip(1))
}

fn startup_file_paths_from(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Vec<PathBuf> {
    arguments
        .into_iter()
        .filter_map(|argument| {
            let text = argument.to_string_lossy();
            if text.starts_with('-') || text.starts_with("psn_") {
                return None;
            }
            let path = PathBuf::from(argument);
            path.exists().then_some(path)
        })
        .collect()
}

/// Convert LaunchServices `application:openURLs:` payloads into existing local
/// paths. Remote URLs continue to require an intentional paste/CLI action.
fn file_paths_from_open_urls(urls: impl IntoIterator<Item = String>) -> Vec<PathBuf> {
    urls.into_iter()
        .filter_map(|url| url::Url::parse(&url).ok()?.to_file_path().ok())
        .filter(|path| path.exists())
        .collect()
}

pub(crate) fn main() {
    let startup_paths = startup_file_paths();
    // GPUI forwards macOS `application:openURLs:` events here. A channel
    // buffers events which arrive during startup until the window entity is
    // available below; the small polling task then uses the same ingestion
    // path as Finder launch arguments and drag/drop.
    let (open_url_tx, open_url_rx) = std::sync::mpsc::channel::<Vec<String>>();
    let application = Application::new();
    application.on_open_urls(move |urls| {
        let _ = open_url_tx.send(urls);
    });
    application.run(move |cx: &mut App| {
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-o", OpenFile, Some("Shift")),
            KeyBinding::new("cmd-s", SaveOutput, Some("Shift")),
            KeyBinding::new("cmd-c", CopyOutput, Some("Shift")),
            KeyBinding::new("cmd-r", RevealOutput, Some("Shift")),
            KeyBinding::new("cmd-shift-f", ToggleFormatMenu, Some("Shift")),
            KeyBinding::new("cmd-,", OpenSettings, Some("Shift")),
            KeyBinding::new("cmd-/", ShowShortcuts, Some("Shift")),
            KeyBinding::new("cmd-m", Minimize, Some("Shift")),
            KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, Some("Shift")),
            KeyBinding::new("escape", CancelWork, Some("Shift")),
        ]);
        text_input::bind_keys(cx);

        // Load bundled Geist variable fonts (SIL Open Font License 1.1).
        // These are available as selectable UI font families in Theme settings.
        if let Err(e) = cx.text_system().add_fonts(vec![
            Cow::Borrowed(include_bytes!("../assets/fonts/Geist-Variable.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../assets/fonts/Geist-Italic-Variable.ttf").as_slice()),
            Cow::Borrowed(include_bytes!("../assets/fonts/GeistMono-Variable.ttf").as_slice()),
            Cow::Borrowed(
                include_bytes!("../assets/fonts/GeistMono-Italic-Variable.ttf").as_slice(),
            ),
        ]) {
            eprintln!("shift: failed to load bundled Geist fonts: {e}");
        }

        let bounds = Bounds::centered(None, size(px(1180.0), px(720.0)), cx);
        let initial_window_width = f32::from(bounds.size.width);

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
            move |window, cx| {
                let shift_entity = cx.new(|cx| Shift::new(cx, initial_window_width));

                window.focus(&shift_entity.read(cx).focus_handle);
                cx.set_menus(shift_entity.read(cx).build_app_menus());

                // Route Enter / magic paste from the input bar back to the app entity.
                let parent = shift_entity.downgrade();
                let url_input = shift_entity.read(cx).url_input.clone();
                let history_limit_input = shift_entity.read(cx).history_limit_input.clone();

                history_limit_input.update(cx, |input, _cx| {
                    let parent_limit = parent.clone();
                    input.set_on_submit(move |text, cx| {
                        let parent = parent_limit.clone();
                        let text = text.to_owned();
                        cx.defer(move |cx| {
                            let _ = parent.update(cx, |this, cx| {
                                if let Ok(limit) = text.trim().parse::<usize>() {
                                    this.set_history_limit(limit, cx);
                                }
                            });
                        });
                    });
                });

                url_input.update(cx, |input, _cx| {
                    let parent_submit = parent.clone();
                    input.set_on_submit(move |text, cx| {
                        let parent = parent_submit.clone();
                        let text = text.to_owned();
                        cx.defer(move |cx| {
                            let _ = parent.update(cx, |this, cx| {
                                this.submit_magic_paste_text(text, cx);
                            });
                        });
                    });

                    let parent_paste = parent.clone();
                    input.set_on_paste(move |item, _window, cx| {
                        let Some(image) = item.entries().iter().find_map(|entry| match entry {
                            ClipboardEntry::Image(image) => Some(image),
                            ClipboardEntry::String(_) => None,
                        }) else {
                            return false;
                        };
                        let extension = match image.format() {
                            ImageFormat::Png => "png",
                            ImageFormat::Jpeg => "jpg",
                            ImageFormat::Webp => "webp",
                            ImageFormat::Gif => "gif",
                            ImageFormat::Svg => "svg",
                            ImageFormat::Bmp => "bmp",
                            ImageFormat::Tiff => "tiff",
                        };
                        let bytes = image.bytes.clone();
                        let parent = parent_paste.clone();
                        cx.defer(move |cx| {
                            let _ = parent.update(cx, |this, cx| {
                                this.ingest_clipboard_image(bytes, extension, cx);
                            });
                        });
                        true
                    });
                });

                // One file follows the normal preview route; multiple files
                // enter the existing queue. This is intentionally the same
                // entrypoint used by drag/drop and the Open panel.
                if !startup_paths.is_empty() {
                    shift_entity.update(cx, |this, cx| {
                        this.ingest_paths(startup_paths.clone(), cx);
                    });
                }

                shift_entity.update(cx, |_, entity_cx| {
                    entity_cx
                        .spawn(async move |open_target, cx| {
                            loop {
                                let mut received = false;
                                while let Ok(urls) = open_url_rx.try_recv() {
                                    received = true;
                                    let paths = file_paths_from_open_urls(urls);
                                    if paths.is_empty() {
                                        continue;
                                    }
                                    let _ = open_target.update(cx, |this, cx| {
                                        this.ingest_paths(paths, cx);
                                    });
                                }
                                if open_target.upgrade().is_none() {
                                    break;
                                }
                                if !received {
                                    cx.background_executor()
                                        .timer(Duration::from_millis(40))
                                        .await;
                                }
                            }
                        })
                        .detach();
                });

                shift_entity
            },
        )
        .expect("failed to open the main window");

        // Warm the open/save panel service so the first click is fast.
        file_picker::prewarm();
        // Periodic artifact-cache hygiene: startup purge (TTL + size budget).
        // Also invoked after each staged write via `stage_ready_artifact` → purge_now.
        // Optional: schedule additional idle `purge_now` calls for long sessions.
        let _ = shift_core::purge_now();

        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    //! Pure unit tests for conversion/history helpers at the top of `app.rs`.
    //!
    //! These cover `ConversionState`, store round-trips, and `history_from_store`
    //! without GPUI, disk I/O, or external converters.

    use super::*;
    use shift_core::conversion::{ConversionArtifact, OutputFormat};
    use shift_core::history::{
        LoadedHistory, StoredHistoryEntry, StoredOutcome, StoredSource, intern_module_id,
    };
    use std::path::PathBuf;
    use std::sync::Arc;

    fn artifact(
        file_name: &str,
        format: OutputFormat,
        module_id: &'static str,
        bytes: Vec<u8>,
    ) -> ConversionArtifact {
        ConversionArtifact {
            file_name: file_name.to_owned(),
            media_type: format.media_type(),
            bytes,
            format,
            module_id,
            pipeline: vec![module_id],
            invocations: Vec::new(),
        }
    }

    fn history_entry(
        id: u64,
        source: HistorySource,
        output_format: OutputFormat,
        outcome: HistoryOutcome,
        archived: bool,
    ) -> ConversionHistoryEntry {
        let name = match &source {
            HistorySource::File(path) => path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into()),
            HistorySource::Url(url) => url.clone(),
        };
        ConversionHistoryEntry {
            id,
            source,
            name: name.into(),
            detail: "detail".into(),
            extension_label: "EXT".into(),
            badge_color: 0x1a1a1a,
            badge_text_color: 0xcccccc,
            output_format,
            outcome,
            archived,
            artifact_deferred: false,
        }
    }

    fn assert_ready_outcome_eq(left: &HistoryOutcome, right: &HistoryOutcome) {
        // HistoryOutcome does not derive Eq; compare Ready paths via stored form.
        let left_entry = history_entry(
            0,
            HistorySource::File(PathBuf::from("/tmp/a")),
            OutputFormat::MARKDOWN,
            left.clone(),
            false,
        );
        let right_entry = history_entry(
            0,
            HistorySource::File(PathBuf::from("/tmp/a")),
            OutputFormat::MARKDOWN,
            right.clone(),
            false,
        );
        assert_eq!(
            to_stored_entry(&left_entry).outcome,
            to_stored_entry(&right_entry).outcome
        );
    }

    // --- ConversionState ---

    #[test]
    fn ready_artifact_some_only_for_ready() {
        let art = Arc::new(artifact(
            "out.md",
            OutputFormat::MARKDOWN,
            "pandoc",
            b"# hi".to_vec(),
        ));

        assert!(ConversionState::Empty.ready_artifact().is_none());
        assert!(ConversionState::Converting.ready_artifact().is_none());
        assert!(
            ConversionState::Failed("boom".into())
                .ready_artifact()
                .is_none()
        );

        let ready = ConversionState::Ready(Arc::clone(&art));
        let got = ready.ready_artifact().expect("Ready yields artifact");
        assert!(Arc::ptr_eq(&got, &art));
        assert_eq!(got.file_name, "out.md");
        assert_eq!(got.bytes, b"# hi");
    }

    #[test]
    fn conversion_state_ready_clone_shares_arc() {
        let art = Arc::new(artifact(
            "large.bin",
            OutputFormat::PDF,
            "docling",
            vec![0u8; 64 * 1024],
        ));
        let state = ConversionState::Ready(Arc::clone(&art));
        let cloned = state.clone();

        match (&state, &cloned) {
            (ConversionState::Ready(a), ConversionState::Ready(b)) => {
                assert!(Arc::ptr_eq(a, b));
                // Original local + state + clone = 3 strong refs.
                assert_eq!(Arc::strong_count(a), 3);
            }
            _ => panic!("expected Ready/Ready"),
        }

        // ready_artifact also returns a clone of the same Arc.
        let via_method = state.ready_artifact().unwrap();
        assert!(Arc::ptr_eq(&via_method, &art));
        assert_eq!(Arc::strong_count(&art), 4);
    }

    // --- HistorySource equality ---

    #[test]
    fn history_source_equality() {
        let path_a = PathBuf::from("/tmp/doc.pdf");
        let path_b = PathBuf::from("/tmp/other.pdf");
        assert_eq!(
            HistorySource::File(path_a.clone()),
            HistorySource::File(path_a.clone())
        );
        assert_ne!(
            HistorySource::File(path_a.clone()),
            HistorySource::File(path_b)
        );
        assert_eq!(
            HistorySource::Url("https://example.com/a".into()),
            HistorySource::Url("https://example.com/a".into())
        );
        assert_ne!(
            HistorySource::Url("https://example.com/a".into()),
            HistorySource::Url("https://example.com/b".into())
        );
        assert_ne!(
            HistorySource::File(path_a),
            HistorySource::Url("https://example.com/a".into())
        );
    }

    // --- to_stored_entry / from_stored_entry round-trips ---

    #[test]
    fn store_round_trip_file_ready_markdown() {
        let entry = history_entry(
            7,
            HistorySource::File(PathBuf::from("/Users/me/Notes/report.docx")),
            OutputFormat::MARKDOWN,
            HistoryOutcome::Ready(Arc::new(artifact(
                "report.md",
                OutputFormat::MARKDOWN,
                "pandoc",
                b"# Report\n".to_vec(),
            ))),
            false,
        );
        let stored = to_stored_entry(&entry);
        assert_eq!(stored.id, 7);
        assert_eq!(
            stored.source,
            StoredSource::File(PathBuf::from("/Users/me/Notes/report.docx"))
        );
        assert_eq!(stored.output_format, "markdown");
        assert!(!stored.archived);
        match &stored.outcome {
            StoredOutcome::Ready {
                module_id,
                file_name,
                format,
                bytes,
            } => {
                assert_eq!(module_id, "pandoc");
                assert_eq!(file_name, "report.md");
                assert_eq!(format, "markdown");
                assert_eq!(bytes, b"# Report\n");
            }
            other => panic!("expected Ready, got {other:?}"),
        }

        let back = from_stored_entry(stored).expect("valid entry");
        assert_eq!(back.id, entry.id);
        assert_eq!(back.source, entry.source);
        assert_eq!(back.name.as_ref(), entry.name.as_ref());
        assert_eq!(back.detail.as_ref(), entry.detail.as_ref());
        assert_eq!(
            back.extension_label.as_ref(),
            entry.extension_label.as_ref()
        );
        assert_eq!(back.badge_color, entry.badge_color);
        assert_eq!(back.badge_text_color, entry.badge_text_color);
        assert_eq!(back.output_format, entry.output_format);
        assert_eq!(back.archived, entry.archived);
        assert_ready_outcome_eq(&back.outcome, &entry.outcome);

        if let HistoryOutcome::Ready(restored) = &back.outcome {
            // Pipeline/invocations are not persisted; restore clears them.
            assert!(restored.pipeline.is_empty());
            assert!(restored.invocations.is_empty());
            assert_eq!(restored.module_id, "pandoc");
            assert_eq!(restored.media_type, OutputFormat::MARKDOWN.media_type());
        } else {
            panic!("expected Ready outcome");
        }
    }

    #[test]
    fn store_round_trip_file_ready_pdf() {
        let bytes = vec![0x25, 0x50, 0x44, 0x46]; // %PDF
        let entry = history_entry(
            2,
            HistorySource::File(PathBuf::from("/tmp/slides.md")),
            OutputFormat::PDF,
            HistoryOutcome::Ready(Arc::new(artifact(
                "slides.pdf",
                OutputFormat::PDF,
                "pandoc",
                bytes.clone(),
            ))),
            false,
        );
        let back = from_stored_entry(to_stored_entry(&entry)).expect("round trip");
        assert_eq!(back.output_format, OutputFormat::PDF);
        match back.outcome {
            HistoryOutcome::Ready(a) => {
                assert_eq!(a.format, OutputFormat::PDF);
                assert_eq!(a.bytes, bytes);
                assert_eq!(a.media_type, "application/pdf");
            }
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn store_round_trip_file_ready_media_formats() {
        for (format, module_id, file_name, payload) in [
            (
                OutputFormat::MP3,
                "ffmpeg",
                "clip.mp3",
                b"ID3fake".as_slice(),
            ),
            (OutputFormat::WAV, "ffmpeg", "clip.wav", b"RIFFfake"),
            (OutputFormat::MP4, "ffmpeg", "clip.mp4", b"ftyp"),
            (
                OutputFormat::SRT,
                "ffmpeg",
                "clip.srt",
                b"1\n00:00:00,000 --> 00:00:01,000\nhi\n",
            ),
            (OutputFormat::PNG, "ffmpeg", "frame.png", b"\x89PNG"),
        ] {
            let entry = history_entry(
                11,
                HistorySource::File(PathBuf::from("/tmp/clip.mkv")),
                format,
                HistoryOutcome::Ready(Arc::new(artifact(
                    file_name,
                    format,
                    module_id,
                    payload.to_vec(),
                ))),
                false,
            );
            let stored = to_stored_entry(&entry);
            assert_eq!(stored.output_format, format.id());
            let back = from_stored_entry(stored).expect("media round trip");
            assert_eq!(back.output_format, format);
            match back.outcome {
                HistoryOutcome::Ready(a) => {
                    assert_eq!(a.format, format);
                    assert_eq!(a.file_name, file_name);
                    assert_eq!(a.bytes, payload);
                    assert_eq!(a.module_id, module_id);
                    assert_eq!(a.media_type, format.media_type());
                }
                _ => panic!("expected Ready for {}", format.id()),
            }
        }
    }

    #[test]
    fn store_url_source_redacts_credentials() {
        let secret_url = "https://user:pass@host.example/path?q=1".to_owned();
        let entry = history_entry(
            3,
            HistorySource::Url(secret_url.clone()),
            OutputFormat::MARKDOWN,
            HistoryOutcome::Ready(Arc::new(artifact(
                "article.md",
                OutputFormat::MARKDOWN,
                "defuddle",
                b"# Article".to_vec(),
            ))),
            false,
        );

        let stored = to_stored_entry(&entry);
        match &stored.source {
            StoredSource::Url(url) => {
                assert!(!url.contains("pass"), "password must not be stored: {url}");
                assert!(!url.contains("user:"), "userinfo must not be stored: {url}");
                assert!(url.contains("host.example"));
                assert!(url.contains("/path"));
                // Matches defuddle redaction: https://host.example/path?q=1
                assert_eq!(url, "https://host.example/path?q=1");
            }
            other => panic!("expected Url source, got {other:?}"),
        }

        // In-memory entry still holds the original; only the stored form is redacted.
        assert_eq!(entry.source, HistorySource::Url(secret_url));

        let back = from_stored_entry(stored).expect("round trip");
        assert_eq!(
            back.source,
            HistorySource::Url("https://host.example/path?q=1".into())
        );
    }

    #[test]
    fn store_round_trip_ready_large() {
        let entry = history_entry(
            9,
            HistorySource::File(PathBuf::from("/tmp/movie.mp4")),
            OutputFormat::MP3,
            HistoryOutcome::ReadyLarge {
                module_id: "ffmpeg".into(),
                byte_len: 8_000_000,
            },
            false,
        );
        let stored = to_stored_entry(&entry);
        match &stored.outcome {
            StoredOutcome::ReadyLarge {
                module_id,
                byte_len,
            } => {
                assert_eq!(module_id, "ffmpeg");
                assert_eq!(*byte_len, 8_000_000);
            }
            other => panic!("expected ReadyLarge, got {other:?}"),
        }
        let back = from_stored_entry(stored).expect("round trip");
        match back.outcome {
            HistoryOutcome::ReadyLarge {
                module_id,
                byte_len,
            } => {
                assert_eq!(module_id.as_ref(), "ffmpeg");
                assert_eq!(byte_len, 8_000_000);
            }
            _ => panic!("expected ReadyLarge"),
        }
        assert_eq!(back.output_format, OutputFormat::MP3);
    }

    #[test]
    fn store_round_trip_failed() {
        let entry = history_entry(
            4,
            HistorySource::File(PathBuf::from("/tmp/bad.pdf")),
            OutputFormat::HTML,
            HistoryOutcome::Failed("engine missing: docling".into()),
            false,
        );
        let stored = to_stored_entry(&entry);
        assert_eq!(
            stored.outcome,
            StoredOutcome::Failed("engine missing: docling".into())
        );
        let back = from_stored_entry(stored).expect("round trip");
        match back.outcome {
            HistoryOutcome::Failed(msg) => assert_eq!(msg.as_ref(), "engine missing: docling"),
            _ => panic!("expected Failed"),
        }
    }

    #[test]
    fn store_round_trip_archived_flags() {
        for archived in [false, true] {
            let entry = history_entry(
                5,
                HistorySource::File(PathBuf::from("/tmp/a.md")),
                OutputFormat::MARKDOWN,
                HistoryOutcome::Failed("x".into()),
                archived,
            );
            let stored = to_stored_entry(&entry);
            assert_eq!(stored.archived, archived);
            let back = from_stored_entry(stored).expect("round trip");
            assert_eq!(back.archived, archived);
        }
    }

    #[test]
    fn from_stored_entry_none_for_invalid_output_format() {
        let entry = StoredHistoryEntry {
            id: 1,
            source: StoredSource::File(PathBuf::from("/tmp/x.bin")),
            name: "x.bin".into(),
            detail: "d".into(),
            extension_label: "BIN".into(),
            badge_color: 1,
            badge_text_color: 2,
            output_format: "not-a-real-format".into(),
            outcome: StoredOutcome::Failed("nope".into()),
            archived: false,
            artifact_deferred: false,
        };
        assert!(from_stored_entry(entry).is_none());
    }

    #[test]
    fn from_stored_entry_ready_unparsable_artifact_format_falls_back_to_output_format() {
        // When the nested Ready.format string is unknown, restore uses entry.output_format.
        let entry = StoredHistoryEntry {
            id: 42,
            source: StoredSource::File(PathBuf::from("/tmp/doc.docx")),
            name: "doc.docx".into(),
            detail: "d".into(),
            extension_label: "DOCX".into(),
            badge_color: 1,
            badge_text_color: 2,
            output_format: "markdown".into(),
            outcome: StoredOutcome::Ready {
                module_id: "pandoc".into(),
                file_name: "doc.md".into(),
                format: "totally-unknown-format-xyz".into(),
                bytes: b"# body".to_vec(),
            },
            archived: false,
            artifact_deferred: false,
        };
        let back = from_stored_entry(entry).expect("output_format is valid");
        assert_eq!(back.output_format, OutputFormat::MARKDOWN);
        match back.outcome {
            HistoryOutcome::Ready(a) => {
                assert_eq!(
                    a.format,
                    OutputFormat::MARKDOWN,
                    "unparsable artifact format must fall back to entry output_format"
                );
                assert_eq!(a.media_type, OutputFormat::MARKDOWN.media_type());
                assert_eq!(a.bytes, b"# body");
                assert_eq!(a.file_name, "doc.md");
            }
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn from_stored_entry_ready_parsable_artifact_format_kept_even_if_differs_from_entry() {
        // Fallback only applies when parse fails; a valid nested format is preserved.
        let entry = StoredHistoryEntry {
            id: 43,
            source: StoredSource::File(PathBuf::from("/tmp/doc.md")),
            name: "doc.md".into(),
            detail: "d".into(),
            extension_label: "MD".into(),
            badge_color: 1,
            badge_text_color: 2,
            output_format: "markdown".into(),
            outcome: StoredOutcome::Ready {
                module_id: "pandoc".into(),
                file_name: "doc.html".into(),
                format: "html".into(),
                bytes: b"<p>hi</p>".to_vec(),
            },
            archived: false,
            artifact_deferred: false,
        };
        let back = from_stored_entry(entry).expect("valid");
        assert_eq!(back.output_format, OutputFormat::MARKDOWN);
        match back.outcome {
            HistoryOutcome::Ready(a) => {
                assert_eq!(a.format, OutputFormat::HTML);
                assert_eq!(a.media_type, OutputFormat::HTML.media_type());
            }
            _ => panic!("expected Ready"),
        }
    }

    #[test]
    fn from_stored_entry_interns_known_module_ids() {
        for module in [
            "markitdown",
            "pandoc",
            "defuddle",
            "docling",
            "spreadsheet",
            "ffmpeg",
        ] {
            let entry = StoredHistoryEntry {
                id: 1,
                source: StoredSource::File(PathBuf::from("/tmp/in")),
                name: "in".into(),
                detail: "d".into(),
                extension_label: "X".into(),
                badge_color: 0,
                badge_text_color: 0,
                output_format: "markdown".into(),
                outcome: StoredOutcome::Ready {
                    module_id: module.to_owned(),
                    file_name: "out.md".into(),
                    format: "markdown".into(),
                    bytes: vec![],
                },
                archived: false,
                artifact_deferred: false,
            };
            let back = from_stored_entry(entry).expect("valid");
            match back.outcome {
                HistoryOutcome::Ready(a) => {
                    assert_eq!(a.module_id, module);
                    // Must be the same static as intern_module_id returns.
                    assert!(std::ptr::eq(a.module_id, intern_module_id(module)));
                }
                _ => panic!("expected Ready"),
            }
        }
    }

    #[test]
    fn from_stored_entry_unknown_module_id_becomes_unknown() {
        let entry = StoredHistoryEntry {
            id: 1,
            source: StoredSource::File(PathBuf::from("/tmp/in")),
            name: "in".into(),
            detail: "d".into(),
            extension_label: "X".into(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "markdown".into(),
            outcome: StoredOutcome::Ready {
                module_id: "custom-plugin-v2".into(),
                file_name: "out.md".into(),
                format: "markdown".into(),
                bytes: vec![],
            },
            archived: false,
            artifact_deferred: false,
        };
        let back = from_stored_entry(entry).expect("valid");
        match back.outcome {
            HistoryOutcome::Ready(a) => {
                assert_eq!(a.module_id, "unknown");
                assert!(std::ptr::eq(
                    a.module_id,
                    intern_module_id("custom-plugin-v2")
                ));
            }
            _ => panic!("expected Ready"),
        }
    }

    // --- history_from_store ---

    #[test]
    fn history_from_store_empty_defaults_next_id_to_one() {
        let (entries, next_id) = history_from_store(LoadedHistory {
            entries: Vec::new(),
            next_id: 0,
            load_error: None,
            load_incomplete: false,
        });
        assert!(entries.is_empty());
        assert_eq!(next_id, 1, "next_id must be at least 1");

        let (entries, next_id) = history_from_store(LoadedHistory::default());
        assert!(entries.is_empty());
        assert_eq!(next_id, 1);
    }

    #[test]
    fn history_from_store_next_id_is_max_of_loaded_and_max_entry_plus_one() {
        let make = |id: u64| StoredHistoryEntry {
            id,
            source: StoredSource::File(PathBuf::from(format!("/tmp/{id}.md"))),
            name: format!("{id}.md"),
            detail: "d".into(),
            extension_label: "MD".into(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "markdown".into(),
            outcome: StoredOutcome::Failed("x".into()),
            archived: false,
            artifact_deferred: false,
        };

        // loaded.next_id already ahead of max entry id.
        let (entries, next_id) = history_from_store(LoadedHistory {
            entries: vec![make(1), make(3), make(2)],
            next_id: 10,
            load_error: None,
            load_incomplete: false,
        });
        assert_eq!(entries.len(), 3);
        assert_eq!(next_id, 10);

        // max entry id forces next_id up (loaded.next_id is stale/low).
        let (entries, next_id) = history_from_store(LoadedHistory {
            entries: vec![make(1), make(5), make(2)],
            next_id: 2,
            load_error: None,
            load_incomplete: false,
        });
        assert_eq!(entries.len(), 3);
        assert_eq!(next_id, 6); // max_id(5) + 1

        // Equal case: loaded.next_id == max_id + 1.
        let (_, next_id) = history_from_store(LoadedHistory {
            entries: vec![make(4)],
            next_id: 5,
            load_error: None,
            load_incomplete: false,
        });
        assert_eq!(next_id, 5);
    }

    #[test]
    fn history_from_store_skips_unparsable_entries_keeps_valid() {
        let valid = |id: u64| StoredHistoryEntry {
            id,
            source: StoredSource::File(PathBuf::from(format!("/tmp/{id}.md"))),
            name: format!("{id}.md"),
            detail: "ok".into(),
            extension_label: "MD".into(),
            badge_color: 1,
            badge_text_color: 2,
            output_format: "markdown".into(),
            outcome: StoredOutcome::Failed(format!("fail-{id}")),
            archived: id % 2 == 0,
            artifact_deferred: false,
        };
        let invalid = |id: u64| StoredHistoryEntry {
            id,
            source: StoredSource::Url(format!("https://example.com/{id}")),
            name: format!("{id}"),
            detail: "bad".into(),
            extension_label: "?".into(),
            badge_color: 0,
            badge_text_color: 0,
            output_format: "not-a-format".into(),
            outcome: StoredOutcome::Failed("ignored".into()),
            archived: false,
            artifact_deferred: false,
        };

        let (entries, next_id) = history_from_store(LoadedHistory {
            entries: vec![valid(1), invalid(2), valid(3), invalid(99), valid(4)],
            next_id: 1,
            load_error: None,
            load_incomplete: false,
        });
        // Invalid formats skipped; max_id still considers their ids (99).
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
        assert_eq!(next_id, 100); // max(1, 99+1)

        match &entries[0].outcome {
            HistoryOutcome::Failed(msg) => assert_eq!(msg.as_ref(), "fail-1"),
            _ => panic!("expected Failed"),
        }
        assert!(!entries[1].archived);
        assert!(entries[2].archived);
    }

    #[test]
    fn history_from_store_next_id_at_least_one_with_only_invalid_entries() {
        // All entries unparsable: entries empty, but max_id still advances next_id.
        let (entries, next_id) = history_from_store(LoadedHistory {
            entries: vec![StoredHistoryEntry {
                id: 0,
                source: StoredSource::File(PathBuf::from("/tmp/x")),
                name: "x".into(),
                detail: "d".into(),
                extension_label: "X".into(),
                badge_color: 0,
                badge_text_color: 0,
                output_format: "nope".into(),
                outcome: StoredOutcome::Failed("x".into()),
                archived: false,
                artifact_deferred: false,
            }],
            next_id: 0,
            load_error: None,
            load_incomplete: false,
        });
        assert!(entries.is_empty());
        // max_id = 0 → saturating_add(1) = 1; max(loaded 0, 1).max(1) = 1
        assert_eq!(next_id, 1);
    }

    #[test]
    fn history_from_store_preserves_mixed_outcomes_and_sources() {
        let stored = vec![
            to_stored_entry(&history_entry(
                1,
                HistorySource::File(PathBuf::from("/tmp/a.docx")),
                OutputFormat::MARKDOWN,
                HistoryOutcome::Ready(Arc::new(artifact(
                    "a.md",
                    OutputFormat::MARKDOWN,
                    "markitdown",
                    b"a".to_vec(),
                ))),
                false,
            )),
            to_stored_entry(&history_entry(
                2,
                HistorySource::Url("https://user:secret@news.example/story".into()),
                OutputFormat::HTML,
                HistoryOutcome::ReadyLarge {
                    module_id: "defuddle".into(),
                    byte_len: 1_000_000,
                },
                true,
            )),
            to_stored_entry(&history_entry(
                3,
                HistorySource::File(PathBuf::from("/tmp/b.mp4")),
                OutputFormat::MP3,
                HistoryOutcome::Failed("ffmpeg missing".into()),
                false,
            )),
        ];
        // Confirm credentials were redacted before load.
        match &stored[1].source {
            StoredSource::Url(u) => assert!(!u.contains("secret")),
            _ => panic!("url"),
        }

        let (entries, next_id) = history_from_store(LoadedHistory {
            entries: stored,
            next_id: 0,
            load_error: None,
            load_incomplete: false,
        });
        assert_eq!(entries.len(), 3);
        assert_eq!(next_id, 4);
        assert!(matches!(entries[0].outcome, HistoryOutcome::Ready(_)));
        assert!(matches!(
            entries[1].outcome,
            HistoryOutcome::ReadyLarge { .. }
        ));
        assert!(matches!(entries[2].outcome, HistoryOutcome::Failed(_)));
        assert_eq!(
            entries[1].source,
            HistorySource::Url("https://news.example/story".into())
        );
        assert!(entries[1].archived);
    }

    #[test]
    fn ready_outcomes_equal_via_stored_form() {
        let a = HistoryOutcome::Ready(Arc::new(artifact(
            "x.md",
            OutputFormat::MARKDOWN,
            "pandoc",
            b"same".to_vec(),
        )));
        let b = HistoryOutcome::Ready(Arc::new(artifact(
            "x.md",
            OutputFormat::MARKDOWN,
            "pandoc",
            b"same".to_vec(),
        )));
        let c = HistoryOutcome::Ready(Arc::new(artifact(
            "x.md",
            OutputFormat::MARKDOWN,
            "pandoc",
            b"different".to_vec(),
        )));
        assert_ready_outcome_eq(&a, &b);
        let a_stored = to_stored_entry(&history_entry(
            1,
            HistorySource::File(PathBuf::from("/t")),
            OutputFormat::MARKDOWN,
            a,
            false,
        ));
        let c_stored = to_stored_entry(&history_entry(
            1,
            HistorySource::File(PathBuf::from("/t")),
            OutputFormat::MARKDOWN,
            c,
            false,
        ));
        assert_ne!(a_stored.outcome, c_stored.outcome);
    }

    #[test]
    fn double_round_trip_is_stable_for_file_ready() {
        let entry = history_entry(
            100,
            HistorySource::File(PathBuf::from("/data/input.rst")),
            OutputFormat::MARKDOWN,
            HistoryOutcome::Ready(Arc::new(artifact(
                "input.md",
                OutputFormat::MARKDOWN,
                "pandoc",
                b"stable".to_vec(),
            ))),
            true,
        );
        let once = from_stored_entry(to_stored_entry(&entry)).unwrap();
        let twice = from_stored_entry(to_stored_entry(&once)).unwrap();
        assert_eq!(to_stored_entry(&once), to_stored_entry(&twice));
        assert_eq!(once.id, twice.id);
        assert!(once.archived);
    }

    #[test]
    fn conversion_state_clone_empty_converting_failed() {
        assert!(matches!(
            ConversionState::Empty.clone(),
            ConversionState::Empty
        ));
        assert!(matches!(
            ConversionState::Converting.clone(),
            ConversionState::Converting
        ));
        let failed = ConversionState::Failed("err".into());
        match failed.clone() {
            ConversionState::Failed(msg) => assert_eq!(msg.as_ref(), "err"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn to_stored_entry_preserves_all_badge_fields() {
        let entry = ConversionHistoryEntry {
            id: 77,
            source: HistorySource::File(PathBuf::from("/tmp/x.docx")),
            name: "x.docx".into(),
            detail: "detail-line".into(),
            extension_label: "DOCX".into(),
            badge_color: 0xabcdef,
            badge_text_color: 0x123456,
            output_format: OutputFormat::HTML,
            outcome: HistoryOutcome::Failed("nope".into()),
            archived: true,
            artifact_deferred: false,
        };
        let stored = to_stored_entry(&entry);
        assert_eq!(stored.badge_color, 0xabcdef);
        assert_eq!(stored.badge_text_color, 0x123456);
        assert_eq!(stored.extension_label, "DOCX");
        assert_eq!(stored.detail, "detail-line");
        assert_eq!(stored.name, "x.docx");
        assert_eq!(stored.output_format, "html");
        assert!(stored.archived);
    }

    #[test]
    fn deferred_history_round_trip_preserves_lazy_artifact_marker() {
        let stored = StoredHistoryEntry {
            id: 12,
            source: StoredSource::File(PathBuf::from("/tmp/large.mp4")),
            name: "large.mp4".to_owned(),
            detail: "MP4 → MP3".to_owned(),
            extension_label: "MP4".to_owned(),
            badge_color: BADGE_FILL,
            badge_text_color: BADGE_TEXT,
            output_format: OutputFormat::MARKDOWN.id().to_owned(),
            outcome: StoredOutcome::Ready {
                module_id: "ffmpeg".to_owned(),
                file_name: "large.md".to_owned(),
                format: OutputFormat::MARKDOWN.id().to_owned(),
                bytes: Vec::new(),
            },
            archived: false,
            artifact_deferred: true,
        };

        let entry = from_stored_entry(stored).expect("valid deferred history row");
        assert!(entry.artifact_deferred);
        match &entry.outcome {
            HistoryOutcome::Ready(artifact) => assert!(artifact.bytes.is_empty()),
            _ => panic!("expected deferred Ready outcome"),
        }
        let round_trip = to_stored_entry(&entry);
        assert!(round_trip.artifact_deferred);
        assert!(matches!(
            round_trip.outcome,
            StoredOutcome::Ready { bytes, .. } if bytes.is_empty()
        ));
    }

    #[test]
    fn from_stored_entry_url_ready_large_round_trip() {
        let entry = history_entry(
            8,
            HistorySource::Url("https://news.example/a".into()),
            OutputFormat::MARKDOWN,
            HistoryOutcome::ReadyLarge {
                module_id: "defuddle".into(),
                byte_len: 42,
            },
            false,
        );
        let back = from_stored_entry(to_stored_entry(&entry)).unwrap();
        assert_eq!(
            back.source,
            HistorySource::Url("https://news.example/a".into())
        );
        match back.outcome {
            HistoryOutcome::ReadyLarge {
                module_id,
                byte_len,
            } => {
                assert_eq!(module_id.as_ref(), "defuddle");
                assert_eq!(byte_len, 42);
            }
            _ => panic!("expected ReadyLarge"),
        }
    }

    #[test]
    fn history_from_store_single_max_id_zero_entry() {
        let (entries, next_id) = history_from_store(LoadedHistory {
            entries: vec![StoredHistoryEntry {
                id: 0,
                source: StoredSource::File(PathBuf::from("/tmp/z")),
                name: "z".into(),
                detail: "d".into(),
                extension_label: "Z".into(),
                badge_color: 0,
                badge_text_color: 0,
                output_format: "markdown".into(),
                outcome: StoredOutcome::Failed("x".into()),
                archived: false,
                artifact_deferred: false,
            }],
            next_id: 0,
            load_error: None,
            load_incomplete: false,
        });
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, 0);
        assert_eq!(next_id, 1);
    }

    #[test]
    fn store_round_trip_html_and_docx_formats() {
        for format in [OutputFormat::HTML, OutputFormat::DOCX] {
            let entry = history_entry(
                1,
                HistorySource::File(PathBuf::from("/tmp/doc.md")),
                format,
                HistoryOutcome::Ready(Arc::new(artifact(
                    "out",
                    format,
                    "pandoc",
                    b"body".to_vec(),
                ))),
                false,
            );
            let back = from_stored_entry(to_stored_entry(&entry)).unwrap();
            assert_eq!(back.output_format, format);
            match back.outcome {
                HistoryOutcome::Ready(a) => {
                    assert_eq!(a.format, format);
                    assert_eq!(a.media_type, format.media_type());
                }
                _ => panic!("expected Ready"),
            }
        }
    }

    #[test]
    fn ready_artifact_method_on_state_matches_conversion_state_impl() {
        let art = Arc::new(artifact(
            "m.md",
            OutputFormat::MARKDOWN,
            "markitdown",
            b"m".to_vec(),
        ));
        let state = ConversionState::Ready(Arc::clone(&art));
        assert!(state.ready_artifact().is_some());
        assert!(ConversionState::Empty.ready_artifact().is_none());
        assert!(ConversionState::Converting.ready_artifact().is_none());
        assert!(
            ConversionState::Failed("f".into())
                .ready_artifact()
                .is_none()
        );
    }

    #[test]
    fn startup_file_handoff_ignores_launch_flags_and_missing_paths() {
        let path = std::env::temp_dir().join(format!(
            "shift-open-with-test-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "opened by Finder").unwrap();
        let paths = startup_file_paths_from(vec![
            std::ffi::OsString::from("-psn_0_123"),
            path.clone().into_os_string(),
            std::ffi::OsString::from("/definitely/missing/shift-file.md"),
        ]);
        assert_eq!(paths, vec![path.clone()]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn launch_services_open_urls_accepts_existing_file_urls_only() {
        let path = std::env::temp_dir().join(format!(
            "shift-open-url-test-{}-{}.md",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "opened via LaunchServices").unwrap();
        let file_url = url::Url::from_file_path(&path).unwrap().to_string();
        let paths = file_paths_from_open_urls(vec![
            file_url,
            "https://example.com/page".to_owned(),
            "not a URL".to_owned(),
        ]);
        assert_eq!(paths, vec![path.clone()]);
        let _ = std::fs::remove_file(path);
    }
}
