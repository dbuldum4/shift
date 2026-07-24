use crate::*;

#[derive(Clone, Debug)]
pub(crate) enum ConversionState {
    Empty,
    Converting,
    /// Shared so render clones stay cheap for large artifacts.
    Ready(Arc<ConversionArtifact>),
    Failed(SharedString),
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
}

pub(crate) fn to_stored_entry(entry: &ConversionHistoryEntry) -> StoredHistoryEntry {
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
        archived: entry.archived,
    }
}

pub(crate) fn from_stored_entry(entry: StoredHistoryEntry) -> Option<ConversionHistoryEntry> {
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
    pub(crate) expanded: Vec<PathBuf>,
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
    /// Ids that need to be persisted (upserted) in the next save.
    pub(crate) history_dirty_ids: HashSet<u64>,
    /// Ids that need to be deleted in the next save.
    pub(crate) history_deleted_ids: HashSet<u64>,
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
    /// When true, batch writes overwrite existing outputs (CLI `--force` parity).
    pub(crate) batch_force: bool,
    /// Per-item progress labels/fractions from the batch runner.
    pub(crate) batch_item_progress: HashMap<u64, (Option<f32>, SharedString)>,
    /// Pending recursive folder expansion (confirm before enqueue).
    pub(crate) folder_confirm: Option<FolderExpandConfirm>,
    /// Cooperative cancel for the active single-file / single-URL conversion.
    pub(crate) conversion_cancel: Arc<AtomicBool>,
    /// Live conversion progress (fraction when known + label).
    pub(crate) conversion_progress: Option<(Option<f32>, SharedString)>,
    /// Cached path for the ready artifact (binary copy / reveal / open).
    pub(crate) cached_ready_path: Option<PathBuf>,
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
        let pdf_password_input = cx.new(|cx| TextInput::new(cx, "password (not saved)", ""));
        let history_search = cx.new(|cx| TextInput::new(cx, "Search history…", ""));
        let history_limit_input =
            cx.new(|cx| TextInput::new(cx, "30", session.history_limit.to_string()));
        let (history, next_history_id) = history_from_store(load_history());
        let module_priority = load_module_priority();
        let registry = Arc::new(ConversionRegistry::default().with_priority(&module_priority));
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
            save_status: None,
            preference_error: None,
            output_format: session.output_format(),
            user_chose_format: false,
            output_menu_open: false,
            format_filter_input,
            settings_open: false,
            settings_section: SettingsSection::Converters,
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
            history_dirty_ids: HashSet::new(),
            history_deleted_ids: HashSet::new(),
            history_sidebar_width,
            output_panel_width,
            panel_resize: None,
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
            pandoc_citations: options.pandoc.citations,
            pandoc_pdf_engine: options.pandoc.pdf_engine.clone(),
            pandoc_reference_doc: options.pandoc.reference_doc.clone(),
            pdf_page_from_input,
            pdf_page_to_input,
            pdf_password_input,
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
                this.registry =
                    Arc::new(ConversionRegistry::default().with_priority(&this.module_priority));
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
        self.batch_queue.set_output_dir(Some(path.as_path()));
        self.batch_status = Some(format!("Output folder: {}", path.display()).into());
        self.persist_session_settings(cx);
        cx.notify();
    }

    pub(crate) fn ingest_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
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

    pub(crate) fn confirm_folder_expand(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.folder_confirm.take() else {
            return;
        };
        self.enqueue_paths(confirm.expanded, false, cx);
    }

    pub(crate) fn dismiss_folder_confirm(&mut self, cx: &mut Context<Self>) {
        self.folder_confirm = None;
        self.batch_status = Some("Folder expansion cancelled.".into());
        cx.notify();
    }

    pub(crate) fn toggle_batch_item_format(&mut self, id: BatchItemId, cx: &mut Context<Self>) {
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
        if sources.is_empty() {
            return;
        }
        if self.batch_running {
            self.batch_status =
                Some("Cannot add files while a batch is running. Wait or Cancel first.".into());
            cx.notify();
            return;
        }
        for source in &sources {
            if let Some(path) = source.as_file() {
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
        let enqueue = BatchEnqueueOptions {
            output_format: self.output_format,
            conversion: options,
            output_dir: self.batch_output_dir.clone(),
            force: self.batch_force,
        };
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

    pub(crate) fn toggle_batch_force(&mut self, cx: &mut Context<Self>) {
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
        }
        self.batch_queue.clear();
        cx.notify();
    }

    pub(crate) fn set_selected_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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
        let paste = parse_magic_paste(trimmed);
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
        match stage_pasted_image(&bytes, extension) {
            Ok(path) => {
                self.url_input
                    .update(cx, |input, cx| input.set_content("", cx));
                self.set_selected_file(path, cx);
            }
            Err(error) => {
                self.fail_magic_paste(&error.to_string(), cx);
            }
        }
    }

    pub(crate) fn fail_magic_paste(&mut self, message: &str, cx: &mut Context<Self>) {
        // Invalidate any in-flight conversion so a late success cannot
        // replace this validation error with an unrelated Ready state.
        self.cancel_active_conversion();
        self.selection_generation = self.selection_generation.wrapping_add(1);
        self.conversion_generation = self.conversion_generation.wrapping_add(1);
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
            .spawn(async move { materialize_magic_paste(&paste, Some(cancel)) });

        cx.spawn(async move |this, cx| {
            let result = resolve.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation != generation {
                    return;
                }
                match result {
                    Ok(sources) => this.apply_materialized_sources(sources, display_text, cx),
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
        self.selected_file = None;
        self.selected_url = Some(url.clone());
        self.file_preview = Some(build_url_preview(&url));
        self.url_input
            .update(cx, |input, cx| input.set_content(url.clone(), cx));
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

    pub(crate) fn persist_session_settings(&self, cx: &App) {
        let mut settings = load_default_session_settings();
        settings.set_output_format(self.output_format);
        settings.batch_output_dir = self.batch_output_dir.clone();
        settings.batch_force = self.batch_force;
        settings.history_sidebar_width = self.history_sidebar_width;
        settings.output_panel_width = self.output_panel_width;
        settings.ui_font_family = self.ui_font_family.clone();
        settings.history_limit = self.history_limit;
        settings.show_archived = self.show_archived;
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

    pub(crate) fn ensure_cached_ready_path(&mut self) -> Option<PathBuf> {
        let ConversionState::Ready(artifact) = &self.conversion else {
            return None;
        };
        // Reuse only when the staged file still matches this artifact's bytes.
        if let Some(path) = self.cached_ready_path.clone() {
            if path.is_file() && export_matches_bytes(&path, &artifact.bytes) {
                return Some(path);
            }
            self.cached_ready_path = None;
        }
        // Write once to the content-hash cache, then hard-link/copy into the
        // user-facing export name so large media is not rewritten on drag.
        let staged = (|| {
            let cache_path = cache_artifact_bytes(&artifact.file_name, &artifact.bytes)?;
            stage_export_file(&artifact.file_name, &cache_path)
        })();
        match staged {
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
        if let Some(path) = self.ensure_cached_ready_path() {
            cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
            self.save_status = Some("Copied artifact path to clipboard.".into());
            cx.notify();
        } else {
            cx.notify();
        }
    }

    pub(crate) fn reveal_output(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.ensure_cached_ready_path() {
            file_picker::reveal_in_finder(&path);
            self.save_status = Some(format!("Revealed · {}", path.display()).into());
        }
        cx.notify();
    }

    pub(crate) fn open_output(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.ensure_cached_ready_path() {
            file_picker::open_path(&path);
            self.save_status = Some(format!("Opened · {}", path.display()).into());
        }
        cx.notify();
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
        self.persist_session_settings(cx);
        self.start_conversion(cx);
    }

    /// Apply a session option change from Settings (reconvert only when relevant).
    pub(crate) fn apply_session_option_change(&mut self, cx: &mut Context<Self>) {
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
        let (progress_tx, progress_rx) = std::sync::mpsc::channel::<ConversionProgress>();
        options.progress = Some(Arc::new(move |progress| {
            let _ = progress_tx.send(progress);
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
        let (done_tx, done_rx) = std::sync::mpsc::channel();
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
                            && this.source_matches(&source_for_check)
                        {
                            this.conversion_progress = None;
                            match result {
                                Ok(artifact) => {
                                    let artifact = Arc::new(artifact);
                                    this.record_history(
                                        HistoryOutcome::Ready(Arc::clone(&artifact)),
                                        cx,
                                    );
                                    this.set_ready_artifact(artifact);
                                }
                                Err(error) if error.is_cancelled() => {
                                    this.conversion =
                                        ConversionState::Failed("Conversion cancelled.".into());
                                }
                                Err(error) => {
                                    let message: SharedString = error.to_string().into();
                                    this.record_history(
                                        HistoryOutcome::Failed(message.clone()),
                                        cx,
                                    );
                                    this.conversion = ConversionState::Failed(message);
                                }
                            }
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

    pub(crate) fn move_module(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from >= self.module_priority.len() || to >= self.module_priority.len() || from == to {
            return;
        }
        let module = self.module_priority.remove(from);
        self.module_priority.insert(to, module);
        self.registry =
            Arc::new(ConversionRegistry::default().with_priority(&self.module_priority));
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
        if !path.exists() {
            self.conversion = ConversionState::Failed(
                format!("Recent file not found: {}", path.display()).into(),
            );
            self.selected_file = Some(path);
            self.selected_url = None;
            self.file_preview = None;
            self.cached_ready_path = None;
            cx.notify();
            return;
        }
        self.set_selected_file(path, cx);
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

        let id = self.next_history_id;
        self.next_history_id = self.next_history_id.wrapping_add(1);
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
        };
        entry.detail = history_entry_stored_detail(&entry).into();
        self.history.insert(0, entry);
        if self.history.len() > self.history_limit {
            for removed in self.history.split_off(self.history_limit) {
                self.history_deleted_ids.insert(removed.id);
                self.history_dirty_ids.remove(&removed.id);
            }
        }
        self.history_dirty_ids.insert(id);
        self.active_history_id = Some(id);
        self.mark_history_cache_dirty();
        self.persist_history();
        self.rebuild_app_menus(cx);
    }

    pub(crate) fn persist_history(&mut self) {
        let stored: Vec<StoredHistoryEntry> = self.history.iter().map(to_stored_entry).collect();
        let changed: Vec<u64> = self.history_dirty_ids.iter().copied().collect();
        let deleted: Vec<u64> = self.history_deleted_ids.iter().copied().collect();
        // Best-effort: keep the in-memory list if the disk write fails.
        let _ = save_history_delta(&stored, &changed, &deleted);
        self.history_dirty_ids.clear();
        self.history_deleted_ids.clear();
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
                self.set_ready_artifact(artifact);
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

    pub(crate) fn clear_history(&mut self, cx: &mut Context<Self>) {
        for entry in &self.history {
            self.history_deleted_ids.insert(entry.id);
        }
        self.history.clear();
        self.history_dirty_ids.clear();
        self.active_history_id = None;
        self.mark_history_cache_dirty();
        self.persist_history();
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
            for removed in self.history.split_off(self.history_limit) {
                self.history_deleted_ids.insert(removed.id);
                self.history_dirty_ids.remove(&removed.id);
            }
        }
        self.history_limit_input.update(cx, |input, cx| {
            input.set_content(limit.to_string(), cx);
        });
        self.mark_history_cache_dirty();
        self.persist_session_settings(cx);
        self.persist_history();
        cx.notify();
        self.rebuild_app_menus(cx);
    }

    pub(crate) fn archive_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(entry) = self.history.iter_mut().find(|entry| entry.id == id) {
            entry.archived = !entry.archived;
            if entry.archived && !self.show_archived && self.active_history_id == Some(id) {
                self.active_history_id = None;
            }
            self.history_dirty_ids.insert(id);
            self.mark_history_cache_dirty();
            self.persist_history();
            cx.notify();
            self.rebuild_app_menus(cx);
        }
    }

    pub(crate) fn delete_history_entry(&mut self, id: u64, cx: &mut Context<Self>) {
        self.history.retain(|entry| entry.id != id);
        self.history_deleted_ids.insert(id);
        self.history_dirty_ids.remove(&id);
        if self.active_history_id == Some(id) {
            self.active_history_id = None;
        }
        self.mark_history_cache_dirty();
        self.persist_history();
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
        let can_convert = has_selection && matches!(conversion, ConversionState::Empty);
        let save_status = self.save_status.clone();
        let available_outputs = self.cached_available_outputs.clone();
        let ready_outputs = self.cached_ready_outputs.clone();
        self.ensure_history_cache(cx);
        let output_format = self.output_format;
        let output_menu_open = self.output_menu_open;
        let format_filter_input = self.format_filter_input.clone();
        let format_filter = self.format_filter_input.read(cx).content().to_owned();
        let settings_open = self.settings_open;
        let ui_font_family = self.ui_font_family.clone();
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
        let cached_ready_path = self.cached_ready_path.clone();
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
                                cached_ready_path,
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
                                    pandoc_citations,
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
                        priority: module_priority,
                        preference_error,
                        output_format,
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
                        defuddle_frontmatter,
                        pandoc_standalone,
                        pandoc_toc,
                        pandoc_citations,
                        markitdown_keep_data_uris,
                        diagnostics,
                        diagnostics_loading,
                    },
                    cx,
                ))
            })
    }
}

pub(crate) fn main() {
    Application::new().run(|cx: &mut App| {
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
            |window, cx| {
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
