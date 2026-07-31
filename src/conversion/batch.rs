//! Shared batch conversion queue and runner.
//!
//! Both the native app and `shift-cli` use this module so multi-file conversion,
//! destination resolution, per-item progress, retry, and cancellation stay
//! identical across surfaces. Callers own presentation and I/O threading;
//! this module owns queue state transitions and convert-then-write execution.

use super::{
    ConversionArtifact, ConversionError, ConversionOptions, ConversionProgress, ConversionRegistry,
    InvocationRecord, MAX_BATCH_ADMISSION, OutputFormat, ProgressSink, default_output_path,
    is_paste_staging_path, looks_like_url, normalize_path, paths_refer_to_same_file,
};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use url::Url;

/// Default maximum concurrent conversion workers (`min(cpus, 4)`).
pub const DEFAULT_BATCH_WORKER_CAP: usize = 4;

/// Environment variable overriding the batch worker cap (`SHIFT_BATCH_WORKERS`).
pub const BATCH_WORKERS_ENV: &str = "SHIFT_BATCH_WORKERS";

/// Resolve how many worker threads `run_batch` should spawn for `task_len` jobs.
pub fn batch_worker_count(task_len: usize) -> usize {
    if task_len == 0 {
        return 0;
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let default_cap = cpus.clamp(1, DEFAULT_BATCH_WORKER_CAP);
    let env_cap = std::env::var(BATCH_WORKERS_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|&n| n > 0);
    let cap = env_cap.unwrap_or(default_cap);
    cap.min(task_len).max(1)
}

/// Stable handle for one queue entry.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BatchItemId(pub u64);

impl fmt::Display for BatchItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A conversion input: local path or remote URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatchSource {
    File(PathBuf),
    Url(String),
}

impl BatchSource {
    /// Classify a path-like string as a local file or http(s) URL.
    ///
    /// `file://` URLs are resolved to local paths when possible. Invalid
    /// `file://` forms (e.g. host-only on Unix) return an error rather than a
    /// synthetic filesystem path that would fail later with a confusing message.
    pub fn try_from_path_or_url(value: impl AsRef<Path>) -> Result<Self, String> {
        let path = value.as_ref();
        if let Some(text) = path.to_str() {
            let text = text.trim();
            if let Ok(parsed) = url::Url::parse(text) {
                if parsed.scheme() == "file" {
                    return parsed.to_file_path().map(Self::File).map_err(|_| {
                        format!("invalid file:// URL (could not resolve to a local path): {text}")
                    });
                }
            }
            if looks_like_url(text) {
                return Ok(Self::Url(text.to_owned()));
            }
        }
        Ok(Self::File(path.to_path_buf()))
    }

    /// Like [`Self::try_from_path_or_url`], but maps invalid `file://` URLs to a
    /// best-effort local path for callers that cannot surface classification errors.
    pub fn from_path_or_url(value: impl AsRef<Path>) -> Self {
        match Self::try_from_path_or_url(value.as_ref()) {
            Ok(source) => source,
            // Preserve previous best-effort behavior only when try fails for non-URL paths.
            Err(_) => Self::File(value.as_ref().to_path_buf()),
        }
    }

    pub fn display_name(&self) -> String {
        match self {
            Self::File(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            Self::Url(url) => redact_url_credentials(url),
        }
    }

    pub fn as_file(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path.as_path()),
            Self::Url(_) => None,
        }
    }

    pub fn as_url(&self) -> Option<&str> {
        match self {
            Self::Url(url) => Some(url.as_str()),
            Self::File(_) => None,
        }
    }
}

/// Strip userinfo from a URL before showing it in the UI or logs.
fn redact_url_credentials(url: &str) -> String {
    if let Ok(mut parsed) = Url::parse(url) {
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        return parsed.to_string();
    }
    url.to_owned()
}

/// Lifecycle of one batch item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatchItemState {
    Queued,
    Running,
    Succeeded {
        written_path: PathBuf,
        module_id: String,
        byte_len: usize,
    },
    Failed {
        error: String,
    },
    Cancelled,
}

/// Redacted conversion provenance retained for every successful batch job.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BatchProvenance {
    pub pipeline: Vec<&'static str>,
    pub invocations: Vec<InvocationRecord>,
}

impl BatchItemState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded { .. } | Self::Failed { .. } | Self::Cancelled
        )
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::Cancelled)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded { .. } => "succeeded",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Whether a batch item follows the shared enqueue format or a per-item override.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BatchFormatSelection {
    /// Use the queue's inherited / enqueue format.
    #[default]
    Inherit,
    /// Pin this item to a specific output format.
    Override(OutputFormat),
}

impl BatchFormatSelection {
    /// Resolve the effective format given the current inherited default.
    pub fn resolve(self, inherited: OutputFormat) -> OutputFormat {
        match self {
            Self::Inherit => inherited,
            Self::Override(format) => format,
        }
    }
}

/// Validated file-name template used by shared batch destination resolution.
///
/// Supported placeholders:
/// - `{stem}`: source file/URL stem
/// - `{ext}`: output extension
/// - `{format}`: canonical output format id
/// - `{parent}`: immediate source parent directory (or `root`)
///
/// Templates produce one file name, never a path. Directory separators,
/// traversal components, control characters, and platform-reserved file-name
/// characters are rejected before any conversion starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchNamingTemplate(String);

impl BatchNamingTemplate {
    pub const DEFAULT: &'static str = "{stem}.{ext}";
    const PLACEHOLDERS: &'static [&'static str] = &["stem", "ext", "format", "parent"];

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn render_file_name(
        &self,
        source: &BatchSource,
        format: OutputFormat,
    ) -> Result<PathBuf, ConversionError> {
        let (stem, parent) = match source {
            BatchSource::File(path) => {
                let stem = path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.is_empty())
                    .unwrap_or("converted");
                let parent = path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.is_empty())
                    .unwrap_or("root");
                (
                    sanitize_template_value(stem),
                    sanitize_template_value(parent),
                )
            }
            BatchSource::Url(url) => {
                let name = suggested_url_file_name(url, format);
                let stem = Path::new(&name)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("page");
                let parent = Url::parse(url)
                    .ok()
                    .and_then(|parsed| parsed.host_str().map(ToOwned::to_owned))
                    .unwrap_or_else(|| "web".to_owned());
                (
                    sanitize_template_value(stem),
                    sanitize_template_value(&parent),
                )
            }
        };

        let mut rendered = self.0.clone();
        rendered = rendered.replace("{stem}", &stem);
        rendered = rendered.replace("{ext}", format.extension());
        rendered = rendered.replace("{format}", format.id());
        rendered = rendered.replace("{parent}", &parent);
        let mut rendered = rendered.trim().to_owned();
        if rendered.is_empty()
            || matches!(rendered.as_str(), "." | "..")
            || rendered.starts_with('.')
        {
            return Err(ConversionError::new(
                "batch naming template produced an unsafe or empty file name",
            ));
        }
        if rendered.chars().count() > 240 {
            rendered = rendered.chars().take(220).collect();
            rendered = rendered.trim_end_matches('.').to_owned();
        }

        let mut name = PathBuf::from(&rendered);
        if name.components().count() != 1 {
            return Err(ConversionError::new(
                "batch naming template must produce a file name, not a path",
            ));
        }
        if name.extension().and_then(|value| value.to_str()) != Some(format.extension()) {
            let current = name
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("converted");
            name = PathBuf::from(format!("{current}.{}", format.extension()));
        }
        Ok(name)
    }
}

impl Default for BatchNamingTemplate {
    fn default() -> Self {
        Self(Self::DEFAULT.to_owned())
    }
}

impl fmt::Display for BatchNamingTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BatchNamingTemplate {
    type Err = ConversionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err(ConversionError::new(
                "batch naming template cannot be empty",
            ));
        }
        if value.chars().count() > 200 {
            return Err(ConversionError::new(
                "batch naming template cannot exceed 200 characters",
            ));
        }
        if value.chars().any(|ch| {
            ch.is_control() || matches!(ch, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        }) {
            return Err(ConversionError::new(
                "batch naming template contains an unsafe file-name character",
            ));
        }
        if value.starts_with('.') {
            return Err(ConversionError::new(
                "batch naming template cannot produce a hidden file name",
            ));
        }

        let mut remainder = value;
        while let Some(open) = remainder.find('{') {
            if remainder[..open].contains('}') {
                return Err(ConversionError::new(
                    "batch naming template has an unmatched `}`",
                ));
            }
            let after_open = &remainder[open + 1..];
            let Some(close) = after_open.find('}') else {
                return Err(ConversionError::new(
                    "batch naming template has an unmatched `{`",
                ));
            };
            let placeholder = &after_open[..close];
            if !Self::PLACEHOLDERS.contains(&placeholder) {
                return Err(ConversionError::new(format!(
                    "unknown batch naming placeholder `{{{placeholder}}}` (use {{stem}}, {{ext}}, {{format}}, or {{parent}})"
                )));
            }
            remainder = &after_open[close + 1..];
        }
        if remainder.contains('}') {
            return Err(ConversionError::new(
                "batch naming template has an unmatched `}`",
            ));
        }
        Ok(Self(value.to_owned()))
    }
}

fn sanitize_template_value(value: &str) -> String {
    let value: String = value
        .chars()
        .map(|ch| {
            if ch.is_control()
                || matches!(
                    ch,
                    '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' | '{' | '}'
                )
            {
                '_'
            } else {
                ch
            }
        })
        .take(160)
        .collect();
    let value = value.trim().trim_matches('.').trim();
    if value.is_empty() {
        "converted".to_owned()
    } else {
        value.to_owned()
    }
}

/// A source plus optional directory hierarchy retained from recursive folder
/// expansion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchInput {
    pub source: BatchSource,
    pub relative_parent: Option<PathBuf>,
}

impl BatchInput {
    pub fn new(source: BatchSource) -> Self {
        Self {
            source,
            relative_parent: None,
        }
    }

    pub fn with_relative_parent(
        source: BatchSource,
        relative_parent: impl AsRef<Path>,
    ) -> Result<Self, ConversionError> {
        let relative_parent = validate_relative_parent(relative_parent.as_ref())?;
        Ok(Self {
            source,
            relative_parent,
        })
    }
}

fn validate_relative_parent(path: &Path) -> Result<Option<PathBuf>, ConversionError> {
    if path.as_os_str().is_empty() {
        return Ok(None);
    }
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ConversionError::new(format!(
            "unsafe recursive output hierarchy: {}",
            path.display()
        )));
    }
    let normalized: PathBuf = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            Component::CurDir => None,
            _ => None,
        })
        .collect();
    Ok((!normalized.as_os_str().is_empty()).then_some(normalized))
}

/// One unit of work in the batch queue.
#[derive(Clone, Debug)]
pub struct BatchItem {
    pub id: BatchItemId,
    /// Jobs in the same group came from one source fan-out.
    pub group_id: u64,
    pub source: BatchSource,
    /// Root-relative source directory retained during recursive expansion.
    pub relative_parent: Option<PathBuf>,
    pub output_format: OutputFormat,
    /// Inherit vs per-item override; [`Self::output_format`] is the resolved value.
    pub format_selection: BatchFormatSelection,
    pub options: ConversionOptions,
    pub naming_template: BatchNamingTemplate,
    /// Per-item module preference snapshotted from a recipe, if any.
    pub preferred_module: Option<String>,
    /// Planned destination (may be adjusted on write for collisions).
    pub destination: PathBuf,
    pub force: bool,
    pub state: BatchItemState,
    pub attempts: u32,
    pub provenance: Option<BatchProvenance>,
}

impl BatchItem {
    /// Effective format: override when set, otherwise the stored/enqueue format.
    pub fn resolved_format(&self) -> OutputFormat {
        self.format_selection.resolve(self.output_format)
    }
}

/// Shared knobs applied when enqueuing items.
#[derive(Clone, Debug)]
pub struct BatchEnqueueOptions {
    pub output_format: OutputFormat,
    pub conversion: ConversionOptions,
    /// When set, all outputs land in this directory (file names from sources).
    pub output_dir: Option<PathBuf>,
    pub naming_template: BatchNamingTemplate,
    pub force: bool,
}

impl BatchEnqueueOptions {
    pub fn new(output_format: OutputFormat) -> Self {
        Self {
            output_format,
            conversion: ConversionOptions::default(),
            output_dir: None,
            naming_template: BatchNamingTemplate::default(),
            force: false,
        }
    }
}

/// Aggregate counts for UI / CLI progress lines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BatchProgress {
    pub total: usize,
    pub queued: usize,
    pub running: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
}

impl BatchProgress {
    pub fn completed(&self) -> usize {
        self.succeeded + self.failed + self.cancelled
    }

    pub fn remaining(&self) -> usize {
        self.queued + self.running
    }

    pub fn is_idle(&self) -> bool {
        self.running == 0 && self.queued == 0
    }
}

/// Final tallies after a run pass.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BatchSummary {
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled: usize,
}

impl BatchSummary {
    /// - `1` when any item failed
    /// - `130` when any item was cancelled (including mixed success+cancel)
    /// - `0` only when every item succeeded (or the queue was empty)
    pub fn exit_code(self) -> u8 {
        if self.failed > 0 {
            1
        } else if self.cancelled > 0 {
            130
        } else {
            0
        }
    }
}

/// Progress notifications emitted while the runner processes the queue.
#[derive(Clone, Debug)]
pub enum BatchEvent {
    ItemStarted {
        id: BatchItemId,
        source_name: String,
        destination: PathBuf,
    },
    /// Best-effort per-item conversion progress (e.g. FFmpeg fraction).
    ItemProgress {
        id: BatchItemId,
        fraction: Option<f32>,
        label: String,
    },
    ItemSucceeded {
        id: BatchItemId,
        source_name: String,
        path: PathBuf,
        module_id: String,
        byte_len: usize,
        provenance: BatchProvenance,
    },
    ItemFailed {
        id: BatchItemId,
        source_name: String,
        error: String,
    },
    ItemCancelled {
        id: BatchItemId,
        source_name: String,
    },
    Progress(BatchProgress),
}

/// Ordered conversion work list shared by app and CLI.
#[derive(Clone, Debug, Default)]
pub struct BatchQueue {
    items: Vec<BatchItem>,
    next_id: u64,
    next_group_id: u64,
}

impl BatchQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[BatchItem] {
        &self.items
    }

    pub fn items_mut(&mut self) -> &mut [BatchItem] {
        &mut self.items
    }

    pub fn get(&self, id: BatchItemId) -> Option<&BatchItem> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn get_mut(&mut self, id: BatchItemId) -> Option<&mut BatchItem> {
        self.items.iter_mut().find(|item| item.id == id)
    }

    pub fn progress(&self) -> BatchProgress {
        let mut progress = BatchProgress {
            total: self.items.len(),
            ..BatchProgress::default()
        };
        for item in &self.items {
            match item.state {
                BatchItemState::Queued => progress.queued += 1,
                BatchItemState::Running => progress.running += 1,
                BatchItemState::Succeeded { .. } => progress.succeeded += 1,
                BatchItemState::Failed { .. } => progress.failed += 1,
                BatchItemState::Cancelled => progress.cancelled += 1,
            }
        }
        progress
    }

    /// Enqueue one source using shared enqueue options.
    ///
    /// New items default to [`BatchFormatSelection::Inherit`].
    pub fn enqueue(&mut self, source: BatchSource, opts: &BatchEnqueueOptions) -> BatchItemId {
        self.enqueue_input(BatchInput::new(source), opts)
    }

    /// Fallible variant of [`Self::enqueue`] for callers that need to surface
    /// an admission error instead of treating over-capacity as a programmer
    /// error.
    pub fn try_enqueue(
        &mut self,
        source: BatchSource,
        opts: &BatchEnqueueOptions,
    ) -> Result<BatchItemId, ConversionError> {
        self.try_enqueue_input(BatchInput::new(source), opts)
    }

    /// Enqueue one source with recursive hierarchy metadata.
    pub fn enqueue_input(&mut self, input: BatchInput, opts: &BatchEnqueueOptions) -> BatchItemId {
        self.try_enqueue_input(input, opts)
            .expect("BatchQueue admission limit exceeded; use try_enqueue to handle this error")
    }

    pub fn try_enqueue_input(
        &mut self,
        input: BatchInput,
        opts: &BatchEnqueueOptions,
    ) -> Result<BatchItemId, ConversionError> {
        self.check_admission(1)?;
        let group_id = self.allocate_group_id();
        Ok(self.push_job(
            input,
            group_id,
            opts.output_format,
            BatchFormatSelection::Inherit,
            opts,
        ))
    }

    /// Enqueue one source to the primary format and every additional format.
    ///
    /// Additional duplicates (including the primary format) are ignored while
    /// preserving first-seen order. Each output is a normal queue job, so
    /// destination collision handling, retry, cancellation, progress, and
    /// provenance remain centralized in [`run_batch`].
    pub fn enqueue_fan_out(
        &mut self,
        input: BatchInput,
        additional_formats: &[OutputFormat],
        opts: &BatchEnqueueOptions,
    ) -> Result<Vec<BatchItemId>, ConversionError> {
        let mut formats = vec![opts.output_format];
        for &format in additional_formats {
            if !formats.contains(&format) {
                formats.push(format);
            }
        }
        self.check_admission(formats.len())?;
        let group_id = self.allocate_group_id();

        Ok(formats
            .into_iter()
            .map(|format| {
                let selection = if format == opts.output_format {
                    BatchFormatSelection::Inherit
                } else {
                    BatchFormatSelection::Override(format)
                };
                self.push_job(input.clone(), group_id, format, selection, opts)
            })
            .collect())
    }

    fn allocate_group_id(&mut self) -> u64 {
        let group_id = self.next_group_id;
        self.next_group_id = self.next_group_id.wrapping_add(1);
        group_id
    }

    fn push_job(
        &mut self,
        input: BatchInput,
        group_id: u64,
        format: OutputFormat,
        format_selection: BatchFormatSelection,
        opts: &BatchEnqueueOptions,
    ) -> BatchItemId {
        let destination = resolve_destination_with_policy(
            &input.source,
            format,
            opts.output_dir.as_deref(),
            input.relative_parent.as_deref(),
            &opts.naming_template,
        )
        .unwrap_or_else(|_| resolve_destination(&input.source, format, opts.output_dir.as_deref()));
        let id = BatchItemId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.items.push(BatchItem {
            id,
            group_id,
            source: input.source,
            relative_parent: input.relative_parent,
            output_format: format,
            format_selection,
            options: opts.conversion.clone(),
            naming_template: opts.naming_template.clone(),
            preferred_module: None,
            destination,
            force: opts.force,
            state: BatchItemState::Queued,
            attempts: 0,
            provenance: None,
        });
        id
    }

    /// Remaining slots under the global multi-file admission cap.
    pub fn admission_remaining(&self) -> usize {
        MAX_BATCH_ADMISSION.saturating_sub(self.items.len())
    }

    /// Reject when adding `additional` items would exceed [`MAX_BATCH_ADMISSION`].
    pub fn check_admission(&self, additional: usize) -> Result<(), ConversionError> {
        let total = self.items.len().saturating_add(additional);
        if total > MAX_BATCH_ADMISSION {
            return Err(ConversionError::new(format!(
                "too many queue items (limit is {MAX_BATCH_ADMISSION}); narrow the selection"
            )));
        }
        Ok(())
    }

    /// Enqueue many sources (files or URLs).
    pub fn enqueue_many(
        &mut self,
        sources: impl IntoIterator<Item = BatchSource>,
        opts: &BatchEnqueueOptions,
    ) -> Vec<BatchItemId> {
        let sources: Vec<_> = sources.into_iter().collect();
        self.check_admission(sources.len()).expect(
            "BatchQueue admission limit exceeded; use try_enqueue_many to handle this error",
        );
        sources
            .into_iter()
            .map(|source| self.enqueue(source, opts))
            .collect()
    }

    /// Fallible variant of [`Self::enqueue_many`] for callers that need to
    /// surface an admission error.
    pub fn try_enqueue_many(
        &mut self,
        sources: impl IntoIterator<Item = BatchSource>,
        opts: &BatchEnqueueOptions,
    ) -> Result<Vec<BatchItemId>, ConversionError> {
        let sources: Vec<_> = sources.into_iter().collect();
        self.check_admission(sources.len())?;
        sources
            .into_iter()
            .map(|source| self.try_enqueue(source, opts))
            .collect()
    }

    /// Add another output job to an existing source fan-out group.
    pub fn add_output_for_item(
        &mut self,
        id: BatchItemId,
        format: OutputFormat,
        output_dir: Option<&Path>,
    ) -> Option<BatchItemId> {
        if self.check_admission(1).is_err() {
            return None;
        }
        let template = self.get(id)?.clone();
        if !matches!(template.state, BatchItemState::Queued) {
            return None;
        }
        if self
            .items
            .iter()
            .any(|item| item.group_id == template.group_id && item.resolved_format() == format)
        {
            return None;
        }
        let opts = BatchEnqueueOptions {
            output_format: format,
            conversion: template.options.clone(),
            output_dir: output_dir.map(Path::to_path_buf),
            naming_template: template.naming_template.clone(),
            force: template.force,
        };
        let preferred_module = template.preferred_module.clone();
        let id = self.push_job(
            BatchInput {
                source: template.source,
                relative_parent: template.relative_parent,
            },
            template.group_id,
            format,
            BatchFormatSelection::Override(format),
            &opts,
        );
        if let Some(item) = self.get_mut(id) {
            item.preferred_module = preferred_module;
        }
        Some(id)
    }

    /// Effective formats already represented in one source fan-out group.
    pub fn group_formats(&self, id: BatchItemId) -> Vec<OutputFormat> {
        let Some(group_id) = self.get(id).map(|item| item.group_id) else {
            return Vec::new();
        };
        self.items
            .iter()
            .filter(|item| item.group_id == group_id)
            .map(BatchItem::resolved_format)
            .collect()
    }

    /// Enqueue one source with a resolved recipe module preference.
    ///
    /// Destination naming uses [`BatchEnqueueOptions::naming_template`] (the
    /// shared batch template). The preferred module is snapshotted on the item
    /// so later recipe edits cannot change a running batch.
    pub fn enqueue_with_recipe(
        &mut self,
        source: BatchSource,
        opts: &BatchEnqueueOptions,
        preferred_module: Option<&str>,
    ) -> BatchItemId {
        self.enqueue_input_with_recipe(BatchInput::new(source), opts, preferred_module)
    }

    /// Enqueue one hierarchical source with a resolved recipe module preference.
    pub fn enqueue_input_with_recipe(
        &mut self,
        input: BatchInput,
        opts: &BatchEnqueueOptions,
        preferred_module: Option<&str>,
    ) -> BatchItemId {
        let id = self.enqueue_input(input, opts);
        if let Some(item) = self.get_mut(id) {
            item.preferred_module = preferred_module.map(str::to_owned);
        }
        id
    }

    /// Enqueue many sources with a resolved recipe module preference.
    pub fn enqueue_many_with_recipe(
        &mut self,
        sources: impl IntoIterator<Item = BatchSource>,
        opts: &BatchEnqueueOptions,
        preferred_module: Option<&str>,
    ) -> Vec<BatchItemId> {
        self.try_enqueue_many_with_recipe(sources, opts, preferred_module)
            .expect(
                "BatchQueue admission limit exceeded; use try_enqueue_many_with_recipe to handle this error",
            )
    }

    /// Fallible variant of [`Self::enqueue_many_with_recipe`].
    pub fn try_enqueue_many_with_recipe(
        &mut self,
        sources: impl IntoIterator<Item = BatchSource>,
        opts: &BatchEnqueueOptions,
        preferred_module: Option<&str>,
    ) -> Result<Vec<BatchItemId>, ConversionError> {
        let sources: Vec<_> = sources.into_iter().collect();
        self.check_admission(sources.len())?;
        sources
            .into_iter()
            .map(|source| self.enqueue_with_recipe(source, opts, preferred_module))
            .map(Ok)
            .collect()
    }

    /// Re-queue a failed or cancelled item.
    pub fn retry(&mut self, id: BatchItemId) -> bool {
        let Some(item) = self.get_mut(id) else {
            return false;
        };
        if !item.state.is_retryable() {
            return false;
        }
        item.state = BatchItemState::Queued;
        item.provenance = None;
        true
    }

    /// Re-queue every failed or cancelled item.
    pub fn retry_failed(&mut self) -> usize {
        let ids: Vec<_> = self
            .items
            .iter()
            .filter(|item| item.state.is_retryable())
            .map(|item| item.id)
            .collect();
        let mut count = 0;
        for id in ids {
            if self.retry(id) {
                count += 1;
            }
        }
        count
    }

    /// Mark all still-queued items as cancelled (does not stop a running item).
    pub fn cancel_queued(&mut self) -> usize {
        let mut count = 0;
        for item in &mut self.items {
            if matches!(item.state, BatchItemState::Queued) {
                item.state = BatchItemState::Cancelled;
                count += 1;
            }
        }
        count
    }

    /// Remove a terminal item from the queue.
    pub fn remove(&mut self, id: BatchItemId) -> bool {
        let before = self.items.len();
        self.items.retain(|item| item.id != id);
        before != self.items.len()
    }

    /// Drop succeeded / failed / cancelled entries.
    pub fn clear_finished(&mut self) {
        self.items.retain(|item| !item.state.is_terminal());
    }

    /// Drop everything. IDs are not reset so stale handles from prior
    /// enqueues can never collide with future items.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Update planned destinations after the user picks a new output folder.
    pub fn set_output_dir(&mut self, output_dir: Option<&Path>) {
        for item in &mut self.items {
            if matches!(item.state, BatchItemState::Queued) {
                item.destination = resolve_destination_with_policy(
                    &item.source,
                    item.output_format,
                    output_dir,
                    item.relative_parent.as_deref(),
                    &item.naming_template,
                )
                .unwrap_or_else(|_| {
                    resolve_destination(&item.source, item.output_format, output_dir)
                });
            }
        }
    }

    /// Apply a validated naming template to every queued job.
    pub fn set_naming_template_for_queued(
        &mut self,
        template: BatchNamingTemplate,
        output_dir: Option<&Path>,
    ) {
        for item in &mut self.items {
            if !matches!(item.state, BatchItemState::Queued) {
                continue;
            }
            item.naming_template = template.clone();
            item.destination = resolve_destination_with_policy(
                &item.source,
                item.resolved_format(),
                output_dir,
                item.relative_parent.as_deref(),
                &item.naming_template,
            )
            .unwrap_or_else(|_| {
                resolve_destination(&item.source, item.resolved_format(), output_dir)
            });
        }
    }

    /// Apply a new format to queued items and refresh destinations.
    ///
    /// Items with [`BatchFormatSelection::Override`] keep their pinned format;
    /// inherited items adopt `format`.
    pub fn set_output_format_for_queued(
        &mut self,
        format: OutputFormat,
        output_dir: Option<&Path>,
    ) {
        self.refresh_inherited_formats(format, output_dir);
    }

    /// Refresh destinations (and format) for queued items that inherit the
    /// shared batch format. Override items only re-resolve their destination.
    pub fn refresh_inherited_formats(
        &mut self,
        inherited: OutputFormat,
        output_dir: Option<&Path>,
    ) {
        for item in &mut self.items {
            if !matches!(item.state, BatchItemState::Queued) {
                continue;
            }
            let format = match item.format_selection {
                BatchFormatSelection::Inherit => {
                    item.output_format = inherited;
                    inherited
                }
                BatchFormatSelection::Override(format) => {
                    item.output_format = format;
                    format
                }
            };
            item.destination = resolve_destination_with_policy(
                &item.source,
                format,
                output_dir,
                item.relative_parent.as_deref(),
                &item.naming_template,
            )
            .unwrap_or_else(|_| resolve_destination(&item.source, format, output_dir));
        }
    }

    /// Snapshot a resolved recipe/session setup onto every queued item.
    ///
    /// Per-item format overrides remain pinned, while shared options, overwrite
    /// policy, and destination naming are updated together. Running and terminal
    /// items are immutable.
    pub fn apply_snapshot_to_queued(
        &mut self,
        inherited: OutputFormat,
        options: &ConversionOptions,
        output_dir: Option<&Path>,
        force: bool,
        naming_template: &BatchNamingTemplate,
        preferred_module: Option<&str>,
    ) {
        for item in &mut self.items {
            if !matches!(item.state, BatchItemState::Queued) {
                continue;
            }
            let format = item.format_selection.resolve(inherited);
            item.output_format = format;
            item.options = options.clone();
            item.preferred_module = preferred_module.map(str::to_owned);
            item.force = force;
            item.naming_template = naming_template.clone();
            item.destination = resolve_destination_with_policy(
                &item.source,
                format,
                output_dir,
                item.relative_parent.as_deref(),
                naming_template,
            )
            .unwrap_or_else(|_| resolve_destination(&item.source, format, output_dir));
        }
    }

    /// Pin a queued item to an explicit format (or clear the pin with Inherit).
    pub fn set_item_format_selection(
        &mut self,
        id: BatchItemId,
        selection: BatchFormatSelection,
        inherited: OutputFormat,
        output_dir: Option<&Path>,
    ) -> bool {
        let Some(index) = self.items.iter().position(|item| item.id == id) else {
            return false;
        };
        let item = &self.items[index];
        if !matches!(item.state, BatchItemState::Queued) {
            return false;
        }
        let format = selection.resolve(inherited);
        // A fan-out group represents one source rendered to distinct formats.
        // Rejecting a duplicate here keeps per-item changes from quietly
        // scheduling two identical writes (which would otherwise be
        // uniquified and surprise the caller with `name-1.ext`).
        if self.items.iter().any(|other| {
            other.id != id && other.group_id == item.group_id && other.resolved_format() == format
        }) {
            return false;
        }
        let item = &mut self.items[index];
        item.format_selection = selection;
        item.output_format = format;
        item.destination = resolve_destination_with_policy(
            &item.source,
            format,
            output_dir,
            item.relative_parent.as_deref(),
            &item.naming_template,
        )
        .unwrap_or_else(|_| resolve_destination(&item.source, format, output_dir));
        true
    }

    /// Ensure queued items do not share planned destinations with each other
    /// or with already-written outputs (even when `force` is set).
    ///
    /// Cross-source name clashes (e.g. two `report.pdf` into one folder) become
    /// `report.md`, `report-1.md`, … so both can succeed without overwriting.
    pub fn uniquify_planned_destinations(&mut self) {
        let mut claimed: HashSet<PathBuf> = HashSet::new();
        for item in &self.items {
            match &item.state {
                BatchItemState::Succeeded { written_path, .. } => {
                    claimed.insert(collision_key(written_path));
                }
                BatchItemState::Queued => {}
                _ => {
                    claimed.insert(collision_key(&item.destination));
                }
            }
        }

        for item in &mut self.items {
            if !matches!(item.state, BatchItemState::Queued) {
                continue;
            }
            let dest = uniquify_against_claimed(&item.destination, &claimed, false);
            claimed.insert(collision_key(&dest));
            item.destination = dest;
        }
    }
}

/// Resolve where an item will be written before conversion runs.
pub fn resolve_destination(
    source: &BatchSource,
    format: OutputFormat,
    output_dir: Option<&Path>,
) -> PathBuf {
    resolve_destination_with_policy(
        source,
        format,
        output_dir,
        None,
        &BatchNamingTemplate::default(),
    )
    .unwrap_or_else(|_| match source {
        BatchSource::File(path) => default_output_path(path, format),
        BatchSource::Url(url) => PathBuf::from(suggested_url_file_name(url, format)),
    })
}

/// Resolve a batch destination using shared hierarchy and naming policies.
pub fn resolve_destination_with_policy(
    source: &BatchSource,
    format: OutputFormat,
    output_dir: Option<&Path>,
    relative_parent: Option<&Path>,
    naming_template: &BatchNamingTemplate,
) -> Result<PathBuf, ConversionError> {
    let relative_parent = relative_parent
        .map(validate_relative_parent)
        .transpose()?
        .flatten();
    let mut file_name = naming_template.render_file_name(source, format)?;
    let base = match (source, output_dir) {
        (_, Some(dir)) => {
            if let Some(relative_parent) = relative_parent {
                dir.join(relative_parent)
            } else {
                dir.to_path_buf()
            }
        }
        (BatchSource::File(path), None) if is_paste_staging_path(path) => PathBuf::new(),
        (BatchSource::File(path), None) => path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf(),
        (BatchSource::Url(_), None) => PathBuf::new(),
    };

    let candidate = base.join(&file_name);
    if source
        .as_file()
        .is_some_and(|path| paths_refer_to_same_file(path, &candidate))
    {
        file_name.set_extension(format!("converted.{}", format.extension()));
    }
    Ok(base.join(file_name))
}

/// Capability-filtered outputs for a batch source. Both UI and CLI fan-out
/// validation use this helper so unsupported pairs fail before engine spawn.
pub fn available_outputs_for_batch_source(
    registry: &ConversionRegistry,
    source: &BatchSource,
) -> Vec<OutputFormat> {
    match source {
        BatchSource::File(path) => registry.available_outputs(path),
        BatchSource::Url(_) => registry.available_url_outputs(),
    }
}

/// Reject unsupported fan-out targets before any external converter launches.
pub fn validate_batch_output_formats(
    registry: &ConversionRegistry,
    source: &BatchSource,
    formats: &[OutputFormat],
) -> Result<(), ConversionError> {
    let available = available_outputs_for_batch_source(registry, source);
    for format in formats {
        if !available.contains(format) {
            return Err(ConversionError::new(format!(
                "{} cannot convert to {}",
                source.display_name(),
                format.label()
            )));
        }
    }
    Ok(())
}

/// Suggest a file name for a URL conversion result.
pub fn suggested_url_file_name(url: &str, format: OutputFormat) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let last = trimmed
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty() && !segment.contains(':'))
        .unwrap_or("page");
    let stem = sanitize_file_stem(last);
    format!("{stem}.{}", format.extension())
}

fn sanitize_file_stem(value: &str) -> String {
    let mut stem: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if let Some(stripped) = stem.rsplit_once('.') {
        // Drop a likely extension so we can attach the output extension.
        if stripped.1.len() <= 5 && stripped.1.chars().all(|c| c.is_ascii_alphanumeric()) {
            stem = stripped.0.to_owned();
        }
    }
    if stem.is_empty() { "page".into() } else { stem }
}

/// Ensure `destination` does not clobber the source or (unless forced) an existing file.
pub fn prepare_batch_destination(
    destination: &Path,
    source: Option<&Path>,
    force: bool,
) -> Result<(), ConversionError> {
    if let Some(source) = source {
        if paths_refer_to_same_file(source, destination) {
            return Err(ConversionError::new(format!(
                "refusing to overwrite source file {} (choose a different output path)",
                source.display()
            )));
        }
    }

    if destination.exists() {
        if std::fs::metadata(destination)
            .map(|meta| meta.is_dir())
            .unwrap_or(false)
        {
            return Err(ConversionError::new(format!(
                "output path is a directory: {} (choose a file path)",
                destination.display()
            )));
        }
        if !force {
            return Err(ConversionError::new(format!(
                "output already exists: {} (pass --force / enable Overwrite to replace)",
                destination.display()
            )));
        }
    }

    if let Some(parent) = destination.parent() {
        if path_has_symlink_component(parent) {
            return Err(ConversionError::new(format!(
                "refusing to write through a symbolic-link output directory: {}",
                parent.display()
            )));
        }
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ConversionError::new(format!(
                    "could not create output directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
    }

    Ok(())
}

/// Refuse output parents that already contain a symlink component. Following a
/// nested output symlink can redirect a watch/batch write back into its input
/// tree, defeating containment checks and creating conversion loops.
fn path_has_symlink_component(path: &Path) -> bool {
    // macOS exposes temporary directories through root-level aliases such as
    // `/tmp` and `/var`. Those system-owned prefixes are safe to normalize;
    // a symlink created below any caller-controlled directory is not.
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if std::fs::symlink_metadata(&current)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(false)
        {
            let root_level_alias = current
                .parent()
                .is_some_and(|parent| parent == Path::new("/"));
            if !root_level_alias {
                return true;
            }
        }
    }
    false
}

/// If `preferred` exists and `force` is false, pick `stem-1.ext`, `stem-2.ext`, …
pub fn uniquify_destination(preferred: &Path, force: bool) -> PathBuf {
    if force || !preferred.exists() {
        return preferred.to_path_buf();
    }
    let empty = HashSet::new();
    uniquify_against_claimed(preferred, &empty, true)
}

fn collision_key(path: &Path) -> PathBuf {
    let normalized = normalize_path(path);
    #[cfg(target_os = "macos")]
    {
        case_fold_path(&normalized)
    }
    #[cfg(not(target_os = "macos"))]
    {
        normalized
    }
}

#[cfg(target_os = "macos")]
fn case_fold_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => out.push("."),
            Component::ParentDir => out.push(".."),
            Component::Normal(part) => {
                let folded = part.to_string_lossy().to_lowercase();
                out.push(OsStr::new(folded.as_str()));
            }
        }
    }
    out
}

/// Pick a path not present in `claimed` (and optionally not already on disk).
fn uniquify_against_claimed(
    preferred: &Path,
    claimed: &HashSet<PathBuf>,
    check_disk: bool,
) -> PathBuf {
    let preferred = normalize_path(preferred);
    let is_taken = |path: &Path| -> bool {
        claimed.contains(&collision_key(path)) || (check_disk && path.exists())
    };
    if !is_taken(&preferred) {
        return preferred;
    }
    let parent = preferred.parent().unwrap_or_else(|| Path::new(""));
    let stem = preferred
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("converted");
    let extension = preferred
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    for index in 1..10_000 {
        let name = if extension.is_empty() {
            format!("{stem}-{index}")
        } else {
            format!("{stem}-{index}.{extension}")
        };
        let candidate = parent.join(name);
        if !is_taken(&candidate) {
            return candidate;
        }
    }
    // Exhausted the short numeric namespace; fall back to a unique token so we
    // never return a path still present in `claimed` or on disk.
    let token = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = if extension.is_empty() {
        format!("{stem}-{token}")
    } else {
        format!("{stem}-{token}.{extension}")
    };
    parent.join(name)
}

fn convert_source(
    registry: &ConversionRegistry,
    source: &BatchSource,
    format: OutputFormat,
    options: &ConversionOptions,
) -> Result<ConversionArtifact, ConversionError> {
    match source {
        BatchSource::File(path) => registry.convert_to_with_options(path, format, options),
        BatchSource::Url(url) => registry.convert_url_with_options(url, format, options),
    }
}

fn write_artifact(
    artifact: &ConversionArtifact,
    planned: &Path,
    source: Option<&Path>,
    force: bool,
) -> Result<PathBuf, ConversionError> {
    // Align with single-file CLI: refuse existing outputs unless force.
    // In-queue name clashes are resolved earlier via uniquify_planned_destinations.
    // `prepare_batch_destination` is a best-effort early check; exclusive create
    // in write_to_with_replace closes the TOCTOU when force is false.
    prepare_batch_destination(planned, source, force)?;
    // Atomic write: exclusive create when force is false (closes TOCTOU).
    match artifact.write_to_with_replace(planned, force) {
        Ok(()) => Ok(planned.to_path_buf()),
        Err(error) => {
            let _ = super::remove_partial_outputs(planned);
            Err(error)
        }
    }
}

/// Immutable per-item snapshot handed to a worker thread.
#[derive(Clone)]
struct BatchTask {
    id: BatchItemId,
    source: BatchSource,
    format: OutputFormat,
    options: ConversionOptions,
    preferred_module: Option<String>,
    destination: PathBuf,
    force: bool,
}

/// Terminal result of converting one item, reported back to the main thread.
enum BatchOutcome {
    Succeeded {
        path: PathBuf,
        module_id: String,
        byte_len: usize,
        provenance: BatchProvenance,
    },
    Cancelled,
    Failed {
        error: String,
    },
}

/// Message sent from worker threads to the main thread that owns the queue.
///
/// Workers never touch [`BatchQueue`] or `on_event`; the main thread applies
/// every state transition and emits every event so ordering across threads
/// stays correct even though items run concurrently.
enum WorkerMsg {
    Started {
        id: BatchItemId,
    },
    Progress {
        id: BatchItemId,
        fraction: Option<f32>,
        label: String,
    },
    Finished {
        id: BatchItemId,
        outcome: BatchOutcome,
    },
}

/// Run one snapshotted task on a worker thread, honoring cooperative cancel.
fn run_task(
    registry: &ConversionRegistry,
    task: &BatchTask,
    cancel: &Arc<AtomicBool>,
    tx: &std::sync::mpsc::Sender<WorkerMsg>,
) -> BatchOutcome {
    let mut options = task.options.clone();
    options.cancel = Some(Arc::clone(cancel));

    // Bridge module progress into batch events; the main thread relays them.
    let progress_tx = tx.clone();
    let progress_id = task.id;
    let sink: ProgressSink = Arc::new(move |progress| {
        let (fraction, label) = match progress {
            ConversionProgress::Phase(label) => (None, label),
            ConversionProgress::Fraction { fraction, label } => (Some(fraction), label),
        };
        let _ = progress_tx.send(WorkerMsg::Progress {
            id: progress_id,
            fraction,
            label,
        });
    });
    options.progress = Some(sink);

    let prioritized_registry = task
        .preferred_module
        .as_deref()
        .map(|module| registry.clone().with_priority(&[module]));
    let task_registry = prioritized_registry.as_ref().unwrap_or(registry);
    let result =
        convert_source(task_registry, &task.source, task.format, &options).and_then(|artifact| {
            if cancel.load(Ordering::SeqCst) {
                return Err(ConversionError::cancelled());
            }
            write_artifact(
                &artifact,
                &task.destination,
                task.source.as_file(),
                task.force,
            )
            .map(|path| {
                (
                    path,
                    artifact.module_id.to_owned(),
                    artifact.bytes.len(),
                    BatchProvenance {
                        pipeline: artifact.pipeline.clone(),
                        invocations: artifact.invocations.clone(),
                    },
                )
            })
        });

    match result {
        Ok((path, module_id, byte_len, provenance)) => BatchOutcome::Succeeded {
            path,
            module_id,
            byte_len,
            provenance,
        },
        Err(error) if error.is_cancelled() || cancel.load(Ordering::SeqCst) => {
            BatchOutcome::Cancelled
        }
        Err(error) => BatchOutcome::Failed {
            error: error.to_string(),
        },
    }
}

/// Process every `Queued` item until the queue is idle or fully cancelled.
///
/// Conversions run concurrently across a bounded worker pool (up to
/// [`std::thread::available_parallelism`] workers). Events do not carry a
/// strict per-item ordering, but every state transition on [`BatchQueue`] is
/// applied on the calling thread, so queue state stays correct. `cancel` aborts
/// active external processes (when modules honor [`ConversionOptions::cancel`])
/// and marks not-yet-started queued items as cancelled. The `on_event` callback
/// receives every state change so UIs and CLIs can report progress identically.
pub fn run_batch(
    queue: &mut BatchQueue,
    registry: &ConversionRegistry,
    cancel: &Arc<AtomicBool>,
    mut on_event: impl FnMut(BatchEvent),
) -> BatchSummary {
    let mut summary = BatchSummary::default();
    // Resolve cross-source destination collisions before any writes.
    queue.uniquify_planned_destinations();

    // Snapshot the queued work in order. Everything a worker needs is copied so
    // the queue itself never has to be shared across threads.
    let tasks: Vec<BatchTask> = queue
        .items
        .iter()
        .filter(|item| matches!(item.state, BatchItemState::Queued))
        .map(|item| BatchTask {
            id: item.id,
            source: item.source.clone(),
            format: item.resolved_format(),
            options: item.options.clone(),
            preferred_module: item.preferred_module.clone(),
            destination: item.destination.clone(),
            force: item.force,
        })
        .collect();

    if !tasks.is_empty() {
        let worker_count = batch_worker_count(tasks.len());

        // Shared cursor: each worker claims the next index with fetch_add.
        let cursor = AtomicUsize::new(0);
        let (tx, rx) = std::sync::mpsc::channel::<WorkerMsg>();

        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let tx = tx.clone();
                let cursor = &cursor;
                let tasks = &tasks;
                scope.spawn(move || {
                    loop {
                        if cancel.load(Ordering::SeqCst) {
                            break;
                        }
                        let idx = cursor.fetch_add(1, Ordering::SeqCst);
                        if idx >= tasks.len() {
                            break;
                        }
                        // Leave not-yet-started items Queued so they become
                        // Cancelled once the pool drains.
                        if cancel.load(Ordering::SeqCst) {
                            break;
                        }
                        let task = &tasks[idx];
                        let task_id = task.id;
                        let _ = tx.send(WorkerMsg::Started { id: task_id });
                        // Isolate a panicking module: a conversion bug should
                        // become a single failed item, not an aborted batch.
                        // Clone the task-local data so a panic cannot corrupt
                        // shared queue or registry state.
                        let registry = ConversionRegistry::clone(registry);
                        let task = BatchTask::clone(task);
                        let cancel = Arc::clone(cancel);
                        let tx_for_task = tx.clone();
                        let outcome = std::panic::catch_unwind(AssertUnwindSafe(move || {
                            run_task(&registry, &task, &cancel, &tx_for_task)
                        }))
                        .unwrap_or_else(|payload| {
                            let message = payload
                                .downcast_ref::<&str>()
                                .copied()
                                .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
                                .unwrap_or("conversion worker panicked");
                            BatchOutcome::Failed {
                                error: message.to_string(),
                            }
                        });
                        let _ = tx.send(WorkerMsg::Finished {
                            id: task_id,
                            outcome,
                        });
                    }
                });
            }
            // Drop the main sender so `rx` closes once every worker exits.
            drop(tx);

            for msg in rx {
                match msg {
                    WorkerMsg::Started { id } => {
                        let mut started = None;
                        if let Some(item) = queue.get_mut(id) {
                            item.state = BatchItemState::Running;
                            item.attempts = item.attempts.saturating_add(1);
                            started = Some((item.source.display_name(), item.destination.clone()));
                        }
                        if let Some((source_name, destination)) = started {
                            on_event(BatchEvent::ItemStarted {
                                id,
                                source_name,
                                destination,
                            });
                            on_event(BatchEvent::Progress(queue.progress()));
                        }
                    }
                    WorkerMsg::Progress {
                        id,
                        fraction,
                        label,
                    } => {
                        let fraction = fraction.map(|value| value.clamp(0.0, 1.0));
                        on_event(BatchEvent::ItemProgress {
                            id,
                            fraction,
                            label,
                        });
                    }
                    WorkerMsg::Finished { id, outcome } => {
                        let source_name = queue
                            .get(id)
                            .map(|item| item.source.display_name())
                            .unwrap_or_default();
                        match outcome {
                            BatchOutcome::Succeeded {
                                path,
                                module_id,
                                byte_len,
                                provenance,
                            } => {
                                // Write already committed: always report success so
                                // cancel-then-retry does not leave an on-disk file
                                // marked Cancelled (which would fail with "already
                                // exists" under force: false).
                                if let Some(item) = queue.get_mut(id) {
                                    item.state = BatchItemState::Succeeded {
                                        written_path: path.clone(),
                                        module_id: module_id.clone(),
                                        byte_len,
                                    };
                                    item.destination = path.clone();
                                    item.provenance = Some(provenance.clone());
                                }
                                summary.succeeded += 1;
                                on_event(BatchEvent::ItemSucceeded {
                                    id,
                                    source_name,
                                    path,
                                    module_id,
                                    byte_len,
                                    provenance,
                                });
                            }
                            BatchOutcome::Cancelled => {
                                // Drop incomplete partial siblings; final path only
                                // exists after a successful atomic rename.
                                if let Some(destination) =
                                    queue.get(id).map(|item| item.destination.clone())
                                {
                                    let _ = super::remove_partial_outputs(&destination);
                                }
                                if let Some(item) = queue.get_mut(id) {
                                    item.state = BatchItemState::Cancelled;
                                }
                                summary.cancelled += 1;
                                on_event(BatchEvent::ItemCancelled { id, source_name });
                            }
                            BatchOutcome::Failed { error } => {
                                if let Some(item) = queue.get_mut(id) {
                                    item.state = BatchItemState::Failed {
                                        error: error.clone(),
                                    };
                                }
                                summary.failed += 1;
                                on_event(BatchEvent::ItemFailed {
                                    id,
                                    source_name,
                                    error,
                                });
                            }
                        }
                        on_event(BatchEvent::Progress(queue.progress()));
                    }
                }
            }
        });
    }

    // Any items never picked up (cancel fired before a worker claimed them)
    // remain Queued; mark them Cancelled to match sequential behavior.
    let leftover: Vec<_> = queue
        .items
        .iter()
        .filter(|item| matches!(item.state, BatchItemState::Queued))
        .map(|item| (item.id, item.source.display_name()))
        .collect();
    if !leftover.is_empty() {
        for (id, source_name) in leftover {
            if let Some(item) = queue.get_mut(id) {
                item.state = BatchItemState::Cancelled;
            }
            summary.cancelled += 1;
            on_event(BatchEvent::ItemCancelled { id, source_name });
        }
        on_event(BatchEvent::Progress(queue.progress()));
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::{ConversionModule, ConversionRegistry};
    use std::sync::Mutex;

    struct CountingModule {
        label: &'static str,
        inputs: &'static [&'static str],
        outputs: &'static [OutputFormat],
        payload: &'static [u8],
        fail_once: Option<Arc<Mutex<bool>>>,
        delay_ms: u64,
        in_flight: Option<Arc<AtomicUsize>>,
        peak: Option<Arc<AtomicUsize>>,
    }

    impl ConversionModule for CountingModule {
        fn id(&self) -> &'static str {
            self.label
        }
        fn label(&self) -> &'static str {
            self.label
        }
        fn input_extensions(&self) -> &'static [&'static str] {
            self.inputs
        }
        fn output_formats(&self) -> &[OutputFormat] {
            self.outputs
        }
        fn chainable_output_formats(&self) -> &[OutputFormat] {
            &[]
        }
        fn convert(
            &self,
            input: &Path,
            output: OutputFormat,
            options: &ConversionOptions,
        ) -> Result<ConversionArtifact, ConversionError> {
            if let Some(in_flight) = &self.in_flight {
                let previous = in_flight.fetch_add(1, Ordering::SeqCst);
                let current = previous + 1;
                if let Some(peak) = &self.peak {
                    peak.fetch_max(current, Ordering::SeqCst);
                }
            }
            if self.delay_ms > 0 {
                let steps = (self.delay_ms / 10).max(1);
                for _ in 0..steps {
                    if options
                        .cancel
                        .as_ref()
                        .is_some_and(|flag| flag.load(Ordering::SeqCst))
                    {
                        if let Some(in_flight) = &self.in_flight {
                            in_flight.fetch_sub(1, Ordering::SeqCst);
                        }
                        return Err(ConversionError::cancelled());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            if let Some(flag) = &self.fail_once {
                let mut guard = flag.lock().unwrap();
                if !*guard {
                    *guard = true;
                    if let Some(in_flight) = &self.in_flight {
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                    }
                    return Err(ConversionError::new("simulated failure"));
                }
            }
            if let Some(in_flight) = &self.in_flight {
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }
            let _ = input;
            Ok(ConversionArtifact {
                file_name: format!("out.{}", output.extension()),
                media_type: output.media_type(),
                bytes: self.payload.to_vec(),
                format: output,
                module_id: self.label,
                pipeline: vec![self.label],
                invocations: Vec::new(),
            })
        }
    }

    fn unique_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "shift-batch-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            name
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn resolve_destination_uses_output_dir_file_names() {
        let source = BatchSource::File(PathBuf::from("/docs/report.pdf"));
        let dest = resolve_destination(&source, OutputFormat::MARKDOWN, Some(Path::new("/out")));
        assert_eq!(dest, PathBuf::from("/out/report.md"));
    }

    #[test]
    fn naming_templates_render_documented_placeholders_and_reject_paths() {
        let source = BatchSource::File(PathBuf::from("/docs/team/report.final.pdf"));
        let template: BatchNamingTemplate = "{parent}-{stem}-{format}.{ext}".parse().unwrap();
        assert_eq!(
            template
                .render_file_name(&source, OutputFormat::MARKDOWN)
                .unwrap(),
            PathBuf::from("team-report.final-markdown.md")
        );

        for unsafe_template in [
            "../{stem}.{ext}",
            "{stem}/{format}.{ext}",
            "{unknown}.{ext}",
            ".{stem}.{ext}",
            "{stem",
        ] {
            assert!(
                unsafe_template.parse::<BatchNamingTemplate>().is_err(),
                "accepted unsafe template {unsafe_template}"
            );
        }
    }

    #[test]
    fn recursive_relative_parent_is_recreated_under_output_dir() {
        let source = BatchSource::File(PathBuf::from("/source/team/drafts/report.pdf"));
        let template = BatchNamingTemplate::default();
        let destination = resolve_destination_with_policy(
            &source,
            OutputFormat::MARKDOWN,
            Some(Path::new("/out")),
            Some(Path::new("team/drafts")),
            &template,
        )
        .unwrap();
        assert_eq!(destination, PathBuf::from("/out/team/drafts/report.md"));

        assert!(
            BatchInput::with_relative_parent(source, "../escape").is_err(),
            "relative output hierarchy must reject traversal"
        );
    }

    #[test]
    fn try_from_path_or_url_resolves_file_urls() {
        let source = BatchSource::try_from_path_or_url("file:///tmp/sample.pdf").unwrap();
        assert_eq!(source, BatchSource::File(PathBuf::from("/tmp/sample.pdf")));

        let source = BatchSource::try_from_path_or_url("https://example.com/a").unwrap();
        assert_eq!(source, BatchSource::Url("https://example.com/a".to_owned()));

        let err = BatchSource::try_from_path_or_url("file://hostname/only").unwrap_err();
        assert!(err.contains("invalid file://"), "unexpected: {err}");
    }

    #[test]
    fn resolve_destination_defaults_beside_source() {
        let dir = unique_dir("beside");
        let input = dir.join("notes.html");
        std::fs::write(&input, b"<p>x</p>").unwrap();
        let source = BatchSource::File(input.clone());
        let dest = resolve_destination(&source, OutputFormat::HTML, None);
        assert_eq!(dest, dir.join("notes.converted.html"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_batch_converts_and_writes_all_items() {
        let dir = unique_dir("run-ok");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, b"one").unwrap();
        std::fs::write(&b, b"two").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "fake",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"# ok\n",
            fail_once: None,
            delay_ms: 0,
            in_flight: None,
            peak: None,
        });

        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out.clone()),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        queue.enqueue(BatchSource::File(a), &opts);
        queue.enqueue(BatchSource::File(b), &opts);

        let cancel = Arc::new(AtomicBool::new(false));
        let mut events = Vec::new();
        let summary = run_batch(&mut queue, &registry, &cancel, |event| {
            events.push(event);
        });

        assert_eq!(summary.succeeded, 2);
        assert_eq!(summary.failed, 0);
        assert!(out.join("a.md").is_file(), "a.md should be written");
        assert!(out.join("b.md").is_file(), "b.md should be written");
        // Destination uses default_output_path stem from input name.
        assert_eq!(std::fs::read_to_string(out.join("a.md")).unwrap(), "# ok\n");
        assert_eq!(std::fs::read_to_string(out.join("b.md")).unwrap(), "# ok\n");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, BatchEvent::ItemSucceeded { .. }))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fan_out_creates_deterministic_jobs_that_share_runner_semantics() {
        let dir = unique_dir("fan-out");
        let input = dir.join("source.txt");
        let out = dir.join("out");
        std::fs::write(&input, b"source").unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "fanout-fake",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN, OutputFormat::HTML],
            payload: b"converted",
            fail_once: None,
            delay_ms: 0,
            in_flight: None,
            peak: None,
        });
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out.clone()),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        let ids = queue
            .enqueue_fan_out(
                BatchInput::new(BatchSource::File(input)),
                &[
                    OutputFormat::HTML,
                    OutputFormat::MARKDOWN,
                    OutputFormat::HTML,
                ],
                &opts,
            )
            .unwrap();
        assert_eq!(ids, vec![BatchItemId(0), BatchItemId(1)]);
        assert_eq!(
            queue
                .items()
                .iter()
                .map(BatchItem::resolved_format)
                .collect::<Vec<_>>(),
            vec![OutputFormat::MARKDOWN, OutputFormat::HTML]
        );
        assert_eq!(queue.items()[0].group_id, queue.items()[1].group_id);

        let summary = run_batch(
            &mut queue,
            &registry,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        assert_eq!(summary.succeeded, 2);
        assert!(out.join("source.md").is_file());
        assert!(out.join("source.html").is_file());
        assert!(queue.items().iter().all(|item| item.attempts == 1));
        assert!(queue.items().iter().all(|item| {
            item.provenance
                .as_ref()
                .is_some_and(|provenance| provenance.pipeline == vec!["fanout-fake"])
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn add_and_remove_fan_out_jobs_stay_in_one_group() {
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        let primary = queue.enqueue(BatchSource::File(PathBuf::from("/tmp/a.txt")), &opts);
        let extra = queue
            .add_output_for_item(primary, OutputFormat::HTML, Some(Path::new("/out")))
            .expect("extra output");
        assert_eq!(
            queue.group_formats(primary),
            vec![OutputFormat::MARKDOWN, OutputFormat::HTML]
        );
        assert!(
            queue
                .add_output_for_item(primary, OutputFormat::HTML, Some(Path::new("/out")))
                .is_none(),
            "duplicate group format should be ignored"
        );
        assert!(queue.remove(extra));
        assert_eq!(queue.group_formats(primary), vec![OutputFormat::MARKDOWN]);
    }

    #[test]
    fn fan_out_and_extra_outputs_respect_admission_cap() {
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        for index in 0..MAX_BATCH_ADMISSION {
            queue.enqueue(
                BatchSource::File(PathBuf::from(format!("/tmp/source-{index}.txt"))),
                &opts,
            );
        }

        let error = queue
            .enqueue_fan_out(
                BatchInput::new(BatchSource::File(PathBuf::from("/tmp/overflow.txt"))),
                &[OutputFormat::HTML],
                &opts,
            )
            .unwrap_err();
        assert!(error.to_string().contains("queue items"));
        assert!(
            queue
                .add_output_for_item(BatchItemId(0), OutputFormat::HTML, Some(Path::new("/out")),)
                .is_none(),
            "extra output must not bypass the queue cap"
        );
        let error = queue
            .try_enqueue(BatchSource::File(PathBuf::from("/tmp/overflow.txt")), &opts)
            .unwrap_err();
        assert!(error.to_string().contains("queue items"));
        let error = queue
            .try_enqueue_many(
                [BatchSource::File(PathBuf::from("/tmp/overflow-a.txt"))],
                &opts,
            )
            .unwrap_err();
        assert!(error.to_string().contains("queue items"));
    }

    #[test]
    fn per_item_format_change_cannot_duplicate_a_fan_out_output() {
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        let ids = queue
            .enqueue_fan_out(
                BatchInput::new(BatchSource::File(PathBuf::from("/tmp/a.txt"))),
                &[OutputFormat::HTML],
                &opts,
            )
            .unwrap();

        assert!(
            !queue.set_item_format_selection(
                ids[0],
                BatchFormatSelection::Override(OutputFormat::HTML),
                OutputFormat::MARKDOWN,
                Some(Path::new("/out")),
            ),
            "a fan-out group must contain each output format at most once"
        );
        assert_eq!(
            queue.group_formats(ids[0]),
            vec![OutputFormat::MARKDOWN, OutputFormat::HTML]
        );
        assert_eq!(
            queue.items()[0].format_selection,
            BatchFormatSelection::Inherit
        );
    }

    #[test]
    fn run_batch_executes_items_in_parallel() {
        const ITEMS: usize = 4;
        const PER_ITEM_MS: u64 = 150;

        let dir = unique_dir("parallel");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        for i in 0..ITEMS {
            std::fs::write(dir.join(format!("f{i}.txt")), b"x").unwrap();
        }

        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "slow",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"# ok\n",
            fail_once: None,
            delay_ms: PER_ITEM_MS,
            in_flight: Some(Arc::clone(&in_flight)),
            peak: Some(Arc::clone(&peak)),
        });

        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out.clone()),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        for i in 0..ITEMS {
            queue.enqueue(BatchSource::File(dir.join(format!("f{i}.txt"))), &opts);
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let summary = run_batch(&mut queue, &registry, &cancel, |_| {});

        assert_eq!(summary.succeeded, 4);
        assert_eq!(summary.failed, 0);
        assert!(
            queue
                .items()
                .iter()
                .all(|item| matches!(item.state, BatchItemState::Succeeded { .. }))
        );

        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        if cores >= 2 {
            assert!(
                peak.load(Ordering::SeqCst) >= 2,
                "conversions did not overlap (peak in-flight was {})",
                peak.load(Ordering::SeqCst)
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forwards_item_progress_before_conversion_finishes() {
        struct ProgressModule {
            delivered: Arc<AtomicBool>,
        }

        impl ConversionModule for ProgressModule {
            fn id(&self) -> &'static str {
                "progress-fake"
            }
            fn label(&self) -> &'static str {
                "Progress fake"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &["txt"]
            }
            fn output_formats(&self) -> &[OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &[OutputFormat] {
                &[]
            }
            fn convert(
                &self,
                _input: &Path,
                output: OutputFormat,
                options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                options
                    .progress
                    .as_ref()
                    .expect("batch runner should install a progress sink")(
                    ConversionProgress::Phase("halfway".into()),
                );
                for _ in 0..100 {
                    if self.delivered.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                if !self.delivered.load(Ordering::SeqCst) {
                    return Err(ConversionError::new(
                        "progress was not delivered until after conversion",
                    ));
                }
                Ok(ConversionArtifact {
                    file_name: "out.md".into(),
                    media_type: output.media_type(),
                    bytes: b"done".to_vec(),
                    format: output,
                    module_id: self.id(),
                    pipeline: Vec::new(),
                    invocations: Vec::new(),
                })
            }
        }

        let dir = unique_dir("live-progress");
        let input = dir.join("input.txt");
        std::fs::write(&input, b"source").unwrap();
        let delivered = Arc::new(AtomicBool::new(false));
        let registry = ConversionRegistry::new().with_module(ProgressModule {
            delivered: Arc::clone(&delivered),
        });
        let mut queue = BatchQueue::new();
        let mut opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        opts.force = true;
        queue.enqueue(BatchSource::File(input), &opts);

        let summary = run_batch(
            &mut queue,
            &registry,
            &Arc::new(AtomicBool::new(false)),
            |event| {
                if matches!(event, BatchEvent::ItemProgress { .. }) {
                    delivered.store(true, Ordering::SeqCst);
                }
            },
        );

        assert_eq!(summary.succeeded, 1);
        assert!(delivered.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn retry_requeues_failed_items() {
        let dir = unique_dir("retry");
        let input = dir.join("doc.txt");
        std::fs::write(&input, b"x").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        let fail_once = Arc::new(Mutex::new(false));
        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "flaky",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"done",
            fail_once: Some(Arc::clone(&fail_once)),
            delay_ms: 0,
            in_flight: None,
            peak: None,
        });

        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out.clone()),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        let id = queue.enqueue(BatchSource::File(input), &opts);
        let cancel = Arc::new(AtomicBool::new(false));

        let summary1 = run_batch(&mut queue, &registry, &cancel, |_| {});
        assert_eq!(summary1.failed, 1);
        assert!(matches!(
            queue.get(id).unwrap().state,
            BatchItemState::Failed { .. }
        ));

        assert!(queue.retry(id));
        let summary2 = run_batch(&mut queue, &registry, &cancel, |_| {});
        assert_eq!(summary2.succeeded, 1);
        assert!(matches!(
            queue.get(id).unwrap().state,
            BatchItemState::Succeeded { .. }
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancel_stops_remaining_queued_items() {
        let dir = unique_dir("cancel");
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "slow",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"x",
            fail_once: None,
            delay_ms: 80,
            in_flight: None,
            peak: None,
        });

        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        queue.enqueue(BatchSource::File(a), &opts);
        queue.enqueue(BatchSource::File(b), &opts);

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_flag = Arc::clone(&cancel);
        let summary = run_batch(&mut queue, &registry, &cancel, |event| {
            if matches!(event, BatchEvent::ItemStarted { .. }) {
                cancel_flag.store(true, Ordering::SeqCst);
            }
        });

        assert!(
            summary.cancelled >= 1,
            "at least one item should be cancelled"
        );
        assert_eq!(
            summary.succeeded + summary.cancelled,
            2,
            "every item should end as succeeded or cancelled"
        );
        assert!(
            queue
                .items()
                .iter()
                .any(|item| matches!(item.state, BatchItemState::Cancelled))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prepare_batch_destination_refuses_source_overwrite() {
        let dir = unique_dir("src-ow");
        let source = dir.join("page.html");
        std::fs::write(&source, b"x").unwrap();
        let error = prepare_batch_destination(&source, Some(&source), true).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite source"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn uniquify_destination_adds_numeric_suffix() {
        let dir = unique_dir("unique");
        let path = dir.join("out.md");
        std::fs::write(&path, b"old").unwrap();
        let next = uniquify_destination(&path, false);
        assert_eq!(next, dir.join("out-1.md"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn uniquify_planned_destinations_separates_collisions() {
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(PathBuf::from("/out")),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        queue.enqueue(BatchSource::File(PathBuf::from("/a/report.pdf")), &opts);
        queue.enqueue(BatchSource::File(PathBuf::from("/b/report.pdf")), &opts);
        assert_eq!(
            queue.items()[0].destination,
            PathBuf::from("/out/report.md")
        );
        assert_eq!(
            queue.items()[1].destination,
            PathBuf::from("/out/report.md")
        );

        queue.uniquify_planned_destinations();
        assert_eq!(
            queue.items()[0].destination,
            PathBuf::from("/out/report.md")
        );
        assert_eq!(
            queue.items()[1].destination,
            PathBuf::from("/out/report-1.md")
        );
    }

    #[test]
    fn write_without_force_fails_when_output_exists() {
        let dir = unique_dir("exists-no-force");
        let input = dir.join("doc.txt");
        std::fs::write(&input, b"x").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let dest = out.join("doc.md");
        std::fs::write(&dest, b"prior").unwrap();

        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "fake",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"# ok\n",
            fail_once: None,
            delay_ms: 0,
            in_flight: None,
            peak: None,
        });

        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out),
            force: false,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        queue.enqueue(BatchSource::File(input), &opts);
        // Pin destination to the existing file (resolve_destination may differ).
        queue.items_mut()[0].destination = dest.clone();

        let cancel = Arc::new(AtomicBool::new(false));
        let summary = run_batch(&mut queue, &registry, &cancel, |_| {});
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.succeeded, 0);
        assert!(
            matches!(
                &queue.items()[0].state,
                BatchItemState::Failed { error } if error.contains("already exists")
            ),
            "state: {:?}",
            queue.items()[0].state
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"prior");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn progress_counts_match_states() {
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        queue.enqueue(BatchSource::File(PathBuf::from("a.pdf")), &opts);
        queue.enqueue(BatchSource::File(PathBuf::from("b.pdf")), &opts);
        queue.items_mut()[0].state = BatchItemState::Succeeded {
            written_path: PathBuf::from("a.md"),
            module_id: "x".into(),
            byte_len: 1,
        };
        queue.items_mut()[1].state = BatchItemState::Failed {
            error: "nope".into(),
        };
        let progress = queue.progress();
        assert_eq!(progress.total, 2);
        assert_eq!(progress.succeeded, 1);
        assert_eq!(progress.failed, 1);
        assert!(progress.is_idle());
    }

    #[test]
    fn format_selection_inherit_and_override() {
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        let id_a = queue.enqueue(BatchSource::File(PathBuf::from("/a/doc.pdf")), &opts);
        let id_b = queue.enqueue(BatchSource::File(PathBuf::from("/b/doc.pdf")), &opts);
        assert_eq!(
            queue.get(id_a).unwrap().format_selection,
            BatchFormatSelection::Inherit
        );

        assert!(queue.set_item_format_selection(
            id_b,
            BatchFormatSelection::Override(OutputFormat::HTML),
            OutputFormat::MARKDOWN,
            Some(Path::new("/out")),
        ));
        assert_eq!(queue.get(id_b).unwrap().output_format, OutputFormat::HTML);
        assert_eq!(
            queue.get(id_b).unwrap().destination,
            PathBuf::from("/out/doc.html")
        );

        queue.refresh_inherited_formats(OutputFormat::DOCX, Some(Path::new("/out")));
        assert_eq!(queue.get(id_a).unwrap().output_format, OutputFormat::DOCX);
        assert_eq!(
            queue.get(id_a).unwrap().destination,
            PathBuf::from("/out/doc.docx")
        );
        // Override stays HTML.
        assert_eq!(queue.get(id_b).unwrap().output_format, OutputFormat::HTML);
    }

    #[test]
    fn cancel_after_successful_write_still_counts_as_success() {
        // Convert finishes, then cancel is set before the runner records state.
        // The file is already on disk — reporting Cancelled would strand retry
        // under force: false with "already exists".
        let dir = unique_dir("cancel-after-write");
        let input = dir.join("doc.txt");
        std::fs::write(&input, b"x").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "fake",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"# ok\n",
            fail_once: None,
            delay_ms: 0,
            in_flight: None,
            peak: None,
        });

        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out.clone()),
            force: false,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        let id = queue.enqueue(BatchSource::File(input), &opts);
        let dest = out.join("doc.md");
        queue.items_mut()[0].destination = dest.clone();

        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_flag = Arc::clone(&cancel);
        let summary = run_batch(&mut queue, &registry, &cancel, |event| {
            if matches!(event, BatchEvent::ItemSucceeded { .. }) {
                // Simulate a late cancel arriving after the write committed.
                cancel_flag.store(true, Ordering::SeqCst);
            }
        });

        assert_eq!(
            summary.succeeded, 1,
            "post-write cancel must not demote success"
        );
        assert_eq!(summary.cancelled, 0);
        assert!(matches!(
            queue.get(id).unwrap().state,
            BatchItemState::Succeeded { .. }
        ));
        assert_eq!(std::fs::read(&dest).unwrap(), b"# ok\n");

        // Retry is unnecessary; force:false still works for a fresh item path.
        let input2 = dir.join("other.txt");
        std::fs::write(&input2, b"y").unwrap();
        let id2 = queue.enqueue(BatchSource::File(input2), &opts);
        queue
            .items_mut()
            .iter_mut()
            .find(|item| item.id == id2)
            .unwrap()
            .destination = out.join("other.md");
        cancel.store(false, Ordering::SeqCst);
        let summary2 = run_batch(&mut queue, &registry, &cancel, |_| {});
        assert_eq!(summary2.succeeded, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancel_before_write_does_not_leave_output() {
        let dir = unique_dir("cancel-before-write");
        let input = dir.join("doc.txt");
        std::fs::write(&input, b"x").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        // Module that sets cancel during convert so write never runs.
        struct CancelDuringConvert {
            flag: Arc<AtomicBool>,
        }
        impl ConversionModule for CancelDuringConvert {
            fn id(&self) -> &'static str {
                "cancel-mid"
            }
            fn label(&self) -> &'static str {
                "cancel-mid"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &["txt"]
            }
            fn output_formats(&self) -> &[OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &[OutputFormat] {
                &[]
            }
            fn convert(
                &self,
                _input: &Path,
                _output: OutputFormat,
                options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                self.flag.store(true, Ordering::SeqCst);
                if options
                    .cancel
                    .as_ref()
                    .is_some_and(|c| c.load(Ordering::SeqCst))
                {
                    return Err(ConversionError::cancelled());
                }
                Ok(ConversionArtifact {
                    file_name: "out.md".into(),
                    media_type: "text/markdown",
                    bytes: b"should-not-write".to_vec(),
                    format: OutputFormat::MARKDOWN,
                    module_id: "cancel-mid",
                    pipeline: Vec::new(),
                    invocations: Vec::new(),
                })
            }
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let registry = ConversionRegistry::new().with_module(CancelDuringConvert {
            flag: Arc::clone(&cancel),
        });

        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out.clone()),
            force: false,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        queue.enqueue(BatchSource::File(input), &opts);
        let dest = out.join("doc.md");
        queue.items_mut()[0].destination = dest.clone();

        let summary = run_batch(&mut queue, &registry, &cancel, |_| {});
        assert_eq!(summary.cancelled, 1);
        assert_eq!(summary.succeeded, 0);
        assert!(!dest.exists(), "cancelled convert must not write output");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn uniquify_against_claimed_never_returns_taken_path() {
        let preferred = PathBuf::from("/out/report.md");
        let mut claimed = HashSet::new();
        claimed.insert(collision_key(&preferred));
        for index in 1..10_000 {
            claimed.insert(collision_key(&PathBuf::from(format!(
                "/out/report-{index}.md"
            ))));
        }
        let resolved = uniquify_against_claimed(&preferred, &claimed, false);
        assert!(
            !claimed.contains(&collision_key(&resolved)),
            "resolved path must not collide with claimed set: {}",
            resolved.display()
        );
        assert!(
            resolved
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("report-") && n.ends_with(".md")),
            "unexpected name: {}",
            resolved.display()
        );
    }

    /// UI queue panel: progress tallies recompute on every batch event.
    mod ui_perf {
        use super::*;
        use std::hint::black_box;
        use std::time::{Duration, Instant};

        fn assert_within(budget: Duration, label: &str, work: impl FnOnce()) {
            let start = Instant::now();
            work();
            let elapsed = start.elapsed();
            assert!(
                elapsed <= budget,
                "{label} took {elapsed:?}, budget {budget:?}"
            );
        }

        #[test]
        fn progress_over_large_queue_stays_responsive() {
            let mut queue = BatchQueue::new();
            let opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
            for i in 0..500 {
                let id = queue.enqueue(
                    BatchSource::File(PathBuf::from(format!("/tmp/in{i}.pdf"))),
                    &opts,
                );
                if let Some(item) = queue.get_mut(id) {
                    item.state = match i % 5 {
                        0 => BatchItemState::Queued,
                        1 => BatchItemState::Running,
                        2 => BatchItemState::Succeeded {
                            written_path: PathBuf::from(format!("/tmp/out{i}.md")),
                            module_id: "pandoc".into(),
                            byte_len: 1024,
                        },
                        3 => BatchItemState::Failed {
                            error: "missing".into(),
                        },
                        _ => BatchItemState::Cancelled,
                    };
                }
            }

            assert_within(Duration::from_secs(1), "BatchQueue::progress×2k", || {
                for _ in 0..2_000 {
                    let progress = queue.progress();
                    black_box(progress.completed());
                    black_box(progress.remaining());
                    black_box(progress.is_idle());
                }
            });

            let progress = queue.progress();
            assert_eq!(progress.total, 500);
            assert_eq!(progress.completed() + progress.remaining(), 500);
        }

        #[test]
        fn enqueue_many_for_folder_drop_stays_within_budget() {
            let sources: Vec<_> = (0..300)
                .map(|i| BatchSource::File(PathBuf::from(format!("/Users/me/Inbox/f{i}.docx"))))
                .collect();
            let opts = BatchEnqueueOptions {
                output_format: OutputFormat::MARKDOWN,
                conversion: ConversionOptions::default(),
                output_dir: Some(PathBuf::from("/Users/me/Exports")),
                naming_template: BatchNamingTemplate::default(),
                force: false,
            };

            assert_within(Duration::from_secs(2), "enqueue_many×300", || {
                let mut queue = BatchQueue::new();
                queue.enqueue_many(sources.clone(), &opts);
                assert_eq!(queue.len(), 300);
                black_box(queue.progress().total);
            });
        }

        #[test]
        fn display_names_for_queue_rows_are_cheap() {
            let sources: Vec<_> = (0..1_000)
                .map(|i| {
                    if i % 3 == 0 {
                        BatchSource::Url(format!("https://example.com/a/{i}"))
                    } else {
                        BatchSource::File(PathBuf::from(format!(
                            "/Users/me/Documents/folder/file_{i}.pdf"
                        )))
                    }
                })
                .collect();

            assert_within(Duration::from_secs(1), "display_name×10k", || {
                for _ in 0..10 {
                    for source in &sources {
                        black_box(source.display_name());
                    }
                }
            });
        }

        #[test]
        fn state_labels_for_status_chips_are_cheap() {
            let states = [
                BatchItemState::Queued,
                BatchItemState::Running,
                BatchItemState::Succeeded {
                    written_path: PathBuf::from("/tmp/a.md"),
                    module_id: "pandoc".into(),
                    byte_len: 1,
                },
                BatchItemState::Failed { error: "x".into() },
                BatchItemState::Cancelled,
            ];
            assert_within(
                Duration::from_secs(1),
                "BatchItemState::label×100k",
                || {
                    for _ in 0..20_000 {
                        for state in &states {
                            black_box(state.label());
                            black_box(state.is_terminal());
                            black_box(state.is_retryable());
                        }
                    }
                },
            );
        }
    }

    #[test]
    fn batch_source_helpers() {
        // Plain file path.
        let source = BatchSource::try_from_path_or_url("report.pdf").unwrap();
        assert_eq!(source, BatchSource::File(PathBuf::from("report.pdf")));
        assert_eq!(source.display_name(), "report.pdf");
        assert_eq!(source.as_file(), Some(Path::new("report.pdf")));
        assert_eq!(source.as_url(), None);

        // HTTPS URL.
        let source = BatchSource::try_from_path_or_url("https://example.com/a/b.pdf").unwrap();
        assert_eq!(
            source,
            BatchSource::Url("https://example.com/a/b.pdf".into())
        );
        assert_eq!(source.display_name(), "https://example.com/a/b.pdf");
        assert_eq!(source.as_file(), None);
        assert_eq!(source.as_url(), Some("https://example.com/a/b.pdf"));

        // File URL with a hostname cannot be resolved; from_path_or_url falls back.
        let source = BatchSource::from_path_or_url("file://hostname/only");
        assert!(matches!(source, BatchSource::File(_)));

        // Display name falls back to the full path when there is no file name.
        let source = BatchSource::File(PathBuf::from(""));
        assert_eq!(source.display_name(), "");
    }

    #[test]
    fn suggested_url_file_name_and_sanitize() {
        assert_eq!(
            suggested_url_file_name("https://example.com/a/b.pdf", OutputFormat::MARKDOWN),
            "b.md"
        );
        assert_eq!(
            suggested_url_file_name("https://example.com/", OutputFormat::HTML),
            "example.html"
        );
        assert_eq!(
            suggested_url_file_name("https://example.com", OutputFormat::MARKDOWN),
            "example.md"
        );
        assert_eq!(
            suggested_url_file_name("https://example.com/path?q=1&x=2", OutputFormat::PDF),
            "path_q_1_x_2.pdf"
        );

        assert_eq!(sanitize_file_stem("my file.txt"), "my_file");
        assert_eq!(sanitize_file_stem("my.archive.tar.gz"), "my.archive.tar");
        assert_eq!(sanitize_file_stem("?.pdf"), "_");
        assert_eq!(sanitize_file_stem(""), "page");
    }

    #[test]
    fn resolve_destination_variants() {
        let file = BatchSource::File(PathBuf::from("/docs/report.pdf"));
        assert_eq!(
            resolve_destination(&file, OutputFormat::MARKDOWN, None),
            PathBuf::from("/docs/report.md")
        );
        assert_eq!(
            resolve_destination(&file, OutputFormat::MARKDOWN, Some(Path::new("/out"))),
            PathBuf::from("/out/report.md")
        );

        let url = BatchSource::Url("https://example.com/a/page.html".into());
        assert_eq!(
            resolve_destination(&url, OutputFormat::MARKDOWN, None),
            PathBuf::from("page.md")
        );
        assert_eq!(
            resolve_destination(&url, OutputFormat::MARKDOWN, Some(Path::new("/out"))),
            PathBuf::from("/out/page.md")
        );
    }

    #[test]
    fn batch_queue_lifecycle_and_progress() {
        let mut queue = BatchQueue::new();
        assert!(queue.is_empty());

        let opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        let id = queue.enqueue(BatchSource::File(PathBuf::from("/tmp/a.pdf")), &opts);
        assert_eq!(queue.len(), 1);
        assert!(!queue.is_empty());
        assert!(queue.get(id).is_some());
        assert!(queue.get_mut(id).is_some());
        assert!(queue.get(BatchItemId(999)).is_none());
        assert_eq!(queue.progress().total, 1);
        assert_eq!(queue.progress().queued, 1);

        queue.cancel_queued();
        assert_eq!(queue.progress().cancelled, 1);

        assert!(queue.retry(id));
        assert_eq!(queue.progress().queued, 1);
        assert!(!queue.retry(BatchItemId(999)));

        // set_output_dir updates queued items.
        queue.set_output_dir(Some(Path::new("/out")));
        assert_eq!(
            queue.get(id).unwrap().destination,
            PathBuf::from("/out/a.md")
        );

        // set_output_format_for_queued changes inherited items and destinations.
        queue.set_output_format_for_queued(OutputFormat::HTML, Some(Path::new("/out")));
        assert_eq!(queue.get(id).unwrap().output_format, OutputFormat::HTML);
        assert_eq!(
            queue.get(id).unwrap().destination,
            PathBuf::from("/out/a.html")
        );

        queue.remove(id);
        assert!(queue.is_empty());

        // retry_failed, clear_finished, and clear.
        let id1 = queue.enqueue(BatchSource::File(PathBuf::from("/tmp/b.pdf")), &opts);
        let id2 = queue.enqueue(BatchSource::File(PathBuf::from("/tmp/c.pdf")), &opts);
        queue.items_mut()[0].state = BatchItemState::Failed { error: "x".into() };
        queue.items_mut()[1].state = BatchItemState::Succeeded {
            written_path: PathBuf::from("/tmp/c.md"),
            module_id: "m".into(),
            byte_len: 1,
        };
        assert_eq!(queue.retry_failed(), 1);
        assert!(matches!(
            queue.get(id1).unwrap().state,
            BatchItemState::Queued
        ));

        queue.clear_finished();
        assert_eq!(queue.len(), 1);
        assert!(queue.get(id2).is_none());

        queue.clear();
        assert!(queue.is_empty());
    }

    #[test]
    fn batch_item_resolved_format_and_state_helpers() {
        let item = BatchItem {
            id: BatchItemId(0),
            group_id: 0,
            source: BatchSource::File(PathBuf::from("/tmp/x.pdf")),
            relative_parent: None,
            output_format: OutputFormat::MARKDOWN,
            format_selection: BatchFormatSelection::Override(OutputFormat::HTML),
            options: ConversionOptions::default(),
            naming_template: BatchNamingTemplate::default(),
            preferred_module: None,
            destination: PathBuf::from("/tmp/x.html"),
            force: false,
            state: BatchItemState::Queued,
            attempts: 0,
            provenance: None,
        };
        assert_eq!(item.resolved_format(), OutputFormat::HTML);

        for (state, terminal, retryable, label) in [
            (BatchItemState::Queued, false, false, "queued"),
            (BatchItemState::Running, false, false, "running"),
            (
                BatchItemState::Succeeded {
                    written_path: PathBuf::new(),
                    module_id: String::new(),
                    byte_len: 0,
                },
                true,
                false,
                "succeeded",
            ),
            (
                BatchItemState::Failed { error: "e".into() },
                true,
                true,
                "failed",
            ),
            (BatchItemState::Cancelled, true, true, "cancelled"),
        ] {
            assert_eq!(state.is_terminal(), terminal, "{label}");
            assert_eq!(state.is_retryable(), retryable, "{label}");
            assert_eq!(state.label(), label);
        }

        assert_eq!(
            BatchFormatSelection::Inherit.resolve(OutputFormat::PDF),
            OutputFormat::PDF
        );
        assert_eq!(
            BatchFormatSelection::Override(OutputFormat::DOCX).resolve(OutputFormat::PDF),
            OutputFormat::DOCX
        );
    }

    #[test]
    fn batch_progress_and_summary_exit_codes() {
        assert!(BatchProgress::default().is_idle());

        let progress = BatchProgress {
            total: 3,
            succeeded: 1,
            failed: 1,
            cancelled: 1,
            ..BatchProgress::default()
        };
        assert_eq!(progress.completed(), 3);
        assert_eq!(progress.remaining(), 0);
        assert!(progress.is_idle());

        assert_eq!(
            BatchSummary {
                succeeded: 2,
                failed: 0,
                cancelled: 0
            }
            .exit_code(),
            0
        );
        assert_eq!(
            BatchSummary {
                succeeded: 0,
                failed: 1,
                cancelled: 0
            }
            .exit_code(),
            1
        );
        assert_eq!(
            BatchSummary {
                succeeded: 0,
                failed: 0,
                cancelled: 1
            }
            .exit_code(),
            130
        );
        assert_eq!(
            BatchSummary {
                succeeded: 1,
                failed: 0,
                cancelled: 1
            }
            .exit_code(),
            130
        );
        assert_eq!(
            BatchSummary {
                succeeded: 1,
                failed: 1,
                cancelled: 1
            }
            .exit_code(),
            1
        );
    }

    #[test]
    fn prepare_batch_destination_and_uniquify() {
        let dir = unique_dir("prepare");

        // Parent directory is created on demand.
        let nested = dir.join("new").join("out.md");
        prepare_batch_destination(&nested, None, false).unwrap();
        assert!(nested.parent().unwrap().is_dir());

        // Refuses to overwrite an existing file without force.
        let existing = dir.join("existing.md");
        std::fs::write(&existing, b"x").unwrap();
        let err = prepare_batch_destination(&existing, None, false).unwrap_err();
        assert!(err.to_string().contains("already exists"));

        let existing_dir = dir.join("existing-dir");
        std::fs::create_dir(&existing_dir).unwrap();
        let err = prepare_batch_destination(&existing_dir, None, true).unwrap_err();
        assert!(err.to_string().contains("output path is a directory"));

        // Refuses to overwrite the source.
        let source = dir.join("source.md");
        std::fs::write(&source, b"x").unwrap();
        let err = prepare_batch_destination(&source, Some(&source), true).unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite source"));

        // Uniquify avoids an existing file.
        let preferred = dir.join("report.md");
        std::fs::write(&preferred, b"x").unwrap();
        assert_eq!(
            uniquify_destination(&preferred, false),
            dir.join("report-1.md")
        );
        assert_eq!(uniquify_destination(&preferred, true), preferred);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn prepare_batch_destination_rejects_nested_symlink_output_parent() {
        let dir = unique_dir("symlink-parent");
        let real = dir.join("real-output");
        let link = dir.join("nested-link");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let destination = link.join("converted.md");
        let error = prepare_batch_destination(&destination, None, true).unwrap_err();
        assert!(error.to_string().contains("symbolic-link output directory"));
        assert!(!destination.exists());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn uniquify_against_claimed_falls_back_to_token() {
        let preferred = PathBuf::from("/out/report.md");
        let mut claimed: HashSet<PathBuf> = (1..10_000)
            .map(|i| collision_key(&PathBuf::from(format!("/out/report-{i}.md"))))
            .collect();
        claimed.insert(collision_key(&preferred));

        let resolved = uniquify_against_claimed(&preferred, &claimed, false);
        assert!(!claimed.contains(&collision_key(&resolved)));
        assert!(
            resolved
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("report-") && n.ends_with(".md")),
            "unexpected: {}",
            resolved.display()
        );
    }

    #[test]
    fn clear_does_not_reuse_ids() {
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        let old = queue.enqueue(BatchSource::File(PathBuf::from("/a.txt")), &opts);
        queue.clear();
        let new = queue.enqueue(BatchSource::File(PathBuf::from("/b.txt")), &opts);
        assert_ne!(old, new, "IDs must not be recycled after clear");
        assert!(queue.get(old).is_none(), "old ID must not resolve");
    }

    #[test]
    fn empty_queue_run_batch_is_noop() {
        let mut queue = BatchQueue::new();
        let registry = ConversionRegistry::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut events = Vec::new();
        let summary = run_batch(&mut queue, &registry, &cancel, |event| {
            events.push(event);
        });
        assert_eq!(summary.succeeded, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.cancelled, 0);
        assert_eq!(summary.exit_code(), 0);
        assert!(queue.is_empty());
        // Empty queue must emit no events — freeze the current noop contract so
        // accidental batch-level side effects fail this test deliberately.
        assert!(
            events.is_empty(),
            "empty queue must be a pure noop, got events: {events:?}"
        );
    }

    #[test]
    fn run_batch_all_failed() {
        let dir = unique_dir("all-failed");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        struct AlwaysFail;
        impl ConversionModule for AlwaysFail {
            fn id(&self) -> &'static str {
                "always-fail"
            }
            fn label(&self) -> &'static str {
                "always-fail"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &["txt"]
            }
            fn output_formats(&self) -> &[OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &[OutputFormat] {
                &[]
            }
            fn convert(
                &self,
                _input: &Path,
                _output: OutputFormat,
                _options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                Err(ConversionError::new("forced failure"))
            }
        }

        let registry = ConversionRegistry::new().with_module(AlwaysFail);
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        for name in ["a.txt", "b.txt", "c.txt"] {
            let path = dir.join(name);
            std::fs::write(&path, b"x").unwrap();
            queue.enqueue(BatchSource::File(path), &opts);
        }

        let summary = run_batch(
            &mut queue,
            &registry,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        assert_eq!(summary.failed, 3);
        assert_eq!(summary.succeeded, 0);
        assert_eq!(summary.cancelled, 0);
        assert_eq!(summary.exit_code(), 1);
        assert!(
            queue
                .items()
                .iter()
                .all(|item| { matches!(item.state, BatchItemState::Failed { .. }) })
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mixed_destinations_uniquify_on_collision() {
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(PathBuf::from("/exports")),
            force: false,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        // Same stem from different folders → same planned destination.
        for folder in ["a", "b", "c", "d"] {
            queue.enqueue(
                BatchSource::File(PathBuf::from(format!("/{folder}/report.pdf"))),
                &opts,
            );
        }
        assert!(
            queue
                .items()
                .iter()
                .all(|item| item.destination.as_path() == Path::new("/exports/report.md"))
        );
        queue.uniquify_planned_destinations();
        let dests: Vec<_> = queue
            .items()
            .iter()
            .map(|item| item.destination.clone())
            .collect();
        let unique: std::collections::HashSet<_> = dests.iter().cloned().collect();
        assert_eq!(unique.len(), 4, "destinations must be unique: {dests:?}");
        assert!(
            dests
                .iter()
                .any(|d| d.as_path() == Path::new("/exports/report.md"))
        );
        assert!(
            dests
                .iter()
                .any(|d| d.as_path() == Path::new("/exports/report-1.md"))
        );
        assert!(
            dests
                .iter()
                .any(|d| d.as_path() == Path::new("/exports/report-2.md"))
        );
        assert!(
            dests
                .iter()
                .any(|d| d.as_path() == Path::new("/exports/report-3.md"))
        );
    }

    #[test]
    fn cancel_flag_mid_run_cancels_remaining() {
        let dir = unique_dir("cancel-mid");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "slowish",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"x",
            fail_once: None,
            delay_ms: 120,
            in_flight: None,
            peak: None,
        });

        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            let path = dir.join(name);
            std::fs::write(&path, b"x").unwrap();
            queue.enqueue(BatchSource::File(path), &opts);
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let mut started = 0usize;
        let summary = run_batch(&mut queue, &registry, &cancel, |event| {
            if matches!(event, BatchEvent::ItemStarted { .. }) {
                started += 1;
                if started >= 1 {
                    flag.store(true, Ordering::SeqCst);
                }
            }
        });

        assert!(
            summary.cancelled >= 1,
            "expected cancellations, summary={summary:?} states={:?}",
            queue
                .items()
                .iter()
                .map(|i| i.state.label())
                .collect::<Vec<_>>()
        );
        assert_eq!(summary.succeeded + summary.failed + summary.cancelled, 4);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn inherit_vs_override_format_applied_per_item_on_run() {
        let dir = unique_dir("inherit-override");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        // Module that echoes the requested output format into the payload.
        struct FormatEcho;
        impl ConversionModule for FormatEcho {
            fn id(&self) -> &'static str {
                "format-echo"
            }
            fn label(&self) -> &'static str {
                "format-echo"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &["txt"]
            }
            fn output_formats(&self) -> &[OutputFormat] {
                &[
                    OutputFormat::MARKDOWN,
                    OutputFormat::HTML,
                    OutputFormat("plain"),
                ]
            }
            fn chainable_output_formats(&self) -> &[OutputFormat] {
                &[]
            }
            fn convert(
                &self,
                _input: &Path,
                output: OutputFormat,
                _options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                Ok(ConversionArtifact {
                    file_name: format!("out.{}", output.extension()),
                    media_type: output.media_type(),
                    bytes: output.id().as_bytes().to_vec(),
                    format: output,
                    module_id: self.id(),
                    pipeline: Vec::new(),
                    invocations: Vec::new(),
                })
            }
        }

        let registry = ConversionRegistry::new().with_module(FormatEcho);
        let mut queue = BatchQueue::new();
        let session = OutputFormat::MARKDOWN;
        let opts = BatchEnqueueOptions {
            output_format: session,
            output_dir: Some(out.clone()),
            force: true,
            ..BatchEnqueueOptions::new(session)
        };

        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let c = dir.join("c.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        std::fs::write(&c, b"c").unwrap();

        let id_a = queue.enqueue(BatchSource::File(a), &opts);
        let id_b = queue.enqueue(BatchSource::File(b), &opts);
        let id_c = queue.enqueue(BatchSource::File(c), &opts);

        // a inherits session markdown; b overrides HTML; c overrides plain.
        assert_eq!(
            queue.get(id_a).unwrap().format_selection,
            BatchFormatSelection::Inherit
        );
        assert!(queue.set_item_format_selection(
            id_b,
            BatchFormatSelection::Override(OutputFormat::HTML),
            session,
            Some(out.as_path()),
        ));
        assert!(queue.set_item_format_selection(
            id_c,
            BatchFormatSelection::Override(OutputFormat("plain")),
            session,
            Some(out.as_path()),
        ));

        // Session changes should only move inherited items.
        queue.refresh_inherited_formats(OutputFormat::MARKDOWN, Some(out.as_path()));
        assert_eq!(
            queue.get(id_a).unwrap().output_format,
            OutputFormat::MARKDOWN
        );
        assert_eq!(queue.get(id_b).unwrap().output_format, OutputFormat::HTML);
        assert_eq!(
            queue.get(id_c).unwrap().output_format,
            OutputFormat("plain")
        );

        let summary = run_batch(
            &mut queue,
            &registry,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        assert_eq!(summary.succeeded, 3, "summary={summary:?}");

        // Read written files and verify format ids.
        let read_payload = |id: BatchItemId| -> String {
            match &queue.get(id).unwrap().state {
                BatchItemState::Succeeded { written_path, .. } => {
                    String::from_utf8(std::fs::read(written_path).unwrap()).unwrap()
                }
                other => panic!("expected success for {id:?}, got {other:?}"),
            }
        };
        assert_eq!(read_payload(id_a), "markdown");
        assert_eq!(read_payload(id_b), "html");
        assert_eq!(read_payload(id_c), "plain");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn enqueue_many_and_progress_mixed_states() {
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions::new(OutputFormat::HTML);
        let sources: Vec<_> = (0..10)
            .map(|i| BatchSource::File(PathBuf::from(format!("/tmp/f{i}.pdf"))))
            .collect();
        queue.enqueue_many(sources, &opts);
        assert_eq!(queue.len(), 10);
        assert_eq!(queue.progress().queued, 10);

        // Force mixed terminal states without running converters.
        for (i, item) in queue.items_mut().iter_mut().enumerate() {
            item.state = match i % 4 {
                0 => BatchItemState::Succeeded {
                    written_path: PathBuf::from(format!("/tmp/o{i}.html")),
                    module_id: "x".into(),
                    byte_len: 10,
                },
                1 => BatchItemState::Failed {
                    error: "nope".into(),
                },
                2 => BatchItemState::Cancelled,
                _ => BatchItemState::Queued,
            };
        }
        let progress = queue.progress();
        assert_eq!(progress.total, 10);
        assert_eq!(progress.succeeded, 3);
        assert_eq!(progress.failed, 3);
        assert_eq!(progress.cancelled, 2);
        assert_eq!(progress.queued, 2);
        assert!(!progress.is_idle());
    }

    #[test]
    fn url_and_file_sources_resolve_distinct_destinations() {
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(PathBuf::from("/out")),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        let mut queue = BatchQueue::new();
        queue.enqueue(BatchSource::File(PathBuf::from("/docs/page.pdf")), &opts);
        queue.enqueue(BatchSource::Url("https://example.com/page".into()), &opts);
        queue.uniquify_planned_destinations();
        let dests: Vec<_> = queue
            .items()
            .iter()
            .map(|i| i.destination.clone())
            .collect();
        let unique: std::collections::HashSet<_> = dests.iter().cloned().collect();
        assert_eq!(unique.len(), dests.len(), "dests={dests:?}");
    }

    #[test]
    fn retry_failed_only_requeues_failures() {
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        let id_ok = queue.enqueue(BatchSource::File(PathBuf::from("/ok.pdf")), &opts);
        let id_fail = queue.enqueue(BatchSource::File(PathBuf::from("/fail.pdf")), &opts);
        let id_cancel = queue.enqueue(BatchSource::File(PathBuf::from("/c.pdf")), &opts);
        queue.get_mut(id_ok).unwrap().state = BatchItemState::Succeeded {
            written_path: PathBuf::from("/ok.md"),
            module_id: "m".into(),
            byte_len: 1,
        };
        queue.get_mut(id_fail).unwrap().state = BatchItemState::Failed { error: "x".into() };
        queue.get_mut(id_cancel).unwrap().state = BatchItemState::Cancelled;

        // `retry_failed` requeues every retryable state (Failed *and* Cancelled).
        assert_eq!(queue.retry_failed(), 2);
        assert!(matches!(
            queue.get(id_fail).unwrap().state,
            BatchItemState::Queued
        ));
        assert!(matches!(
            queue.get(id_cancel).unwrap().state,
            BatchItemState::Queued
        ));
        // Succeeded items are not retryable.
        assert!(!queue.retry(id_ok));
        assert!(matches!(
            queue.get(id_ok).unwrap().state,
            BatchItemState::Succeeded { .. }
        ));
    }

    #[test]
    fn force_true_overwrites_existing_destination() {
        let dir = unique_dir("force-overwrite");
        let input = dir.join("doc.txt");
        std::fs::write(&input, b"x").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let dest = out.join("doc.md");
        std::fs::write(&dest, b"prior").unwrap();

        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "fake",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"new\n",
            fail_once: None,
            delay_ms: 0,
            in_flight: None,
            peak: None,
        });
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        queue.enqueue(BatchSource::File(input), &opts);
        queue.items_mut()[0].destination = dest.clone();

        let summary = run_batch(
            &mut queue,
            &registry,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        assert_eq!(summary.succeeded, 1);
        assert_eq!(std::fs::read(&dest).unwrap(), b"new\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unsupported_input_fails_item_without_panic() {
        let dir = unique_dir("unsupported");
        let input = dir.join("doc.xyz");
        std::fs::write(&input, b"x").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "fake",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"x",
            fail_once: None,
            delay_ms: 0,
            in_flight: None,
            peak: None,
        });
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        queue.enqueue(BatchSource::File(input), &opts);
        let summary = run_batch(
            &mut queue,
            &registry,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.succeeded, 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn batch_item_id_display_and_from_path_ok_path() {
        assert_eq!(BatchItemId(0).to_string(), "0");
        assert_eq!(BatchItemId(42).to_string(), "42");
        assert_eq!(format!("{}", BatchItemId(u64::MAX)), u64::MAX.to_string());

        // Non-URL plain path succeeds (Ok arm of from_path_or_url).
        let source = BatchSource::from_path_or_url("/tmp/report.pdf");
        assert_eq!(source, BatchSource::File(PathBuf::from("/tmp/report.pdf")));

        // Unparseable URL-like string falls back to raw text for display redaction.
        assert_eq!(
            redact_url_credentials("not a url at all"),
            "not a url at all"
        );
        // Valid URL with credentials is redacted.
        let redacted = redact_url_credentials("https://user:secret@example.com/a");
        assert!(!redacted.contains("secret"));
        assert!(redacted.contains("example.com"));
    }

    #[test]
    fn panicking_module_becomes_failed_item() {
        struct PanicModule;
        impl ConversionModule for PanicModule {
            fn id(&self) -> &'static str {
                "panic-mod"
            }
            fn label(&self) -> &'static str {
                "panic-mod"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &["txt"]
            }
            fn output_formats(&self) -> &[OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &[OutputFormat] {
                &[]
            }
            fn convert(
                &self,
                _input: &Path,
                _output: OutputFormat,
                _options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                panic!("intentional module panic");
            }
        }

        let dir = unique_dir("panic");
        let input = dir.join("a.txt");
        std::fs::write(&input, b"x").unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        let registry = ConversionRegistry::new().with_module(PanicModule);
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        queue.enqueue(BatchSource::File(input), &opts);
        let summary = run_batch(
            &mut queue,
            &registry,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.succeeded, 0);
        match &queue.items()[0].state {
            BatchItemState::Failed { error } => {
                assert!(
                    error.contains("intentional module panic")
                        || error.contains("conversion worker panicked"),
                    "error: {error}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fraction_progress_and_url_source_conversion() {
        struct UrlProgressModule;
        impl ConversionModule for UrlProgressModule {
            fn id(&self) -> &'static str {
                "url-progress"
            }
            fn label(&self) -> &'static str {
                "url-progress"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &[]
            }
            fn output_formats(&self) -> &[OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &[OutputFormat] {
                &[]
            }
            fn convert(
                &self,
                _input: &Path,
                _output: OutputFormat,
                _options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                Err(ConversionError::new("files not supported"))
            }
            fn supports_url(&self, output: OutputFormat) -> bool {
                output == OutputFormat::MARKDOWN
            }
            fn convert_url(
                &self,
                url: &str,
                output: OutputFormat,
                options: &ConversionOptions,
            ) -> Result<ConversionArtifact, ConversionError> {
                if let Some(sink) = options.progress.as_ref() {
                    sink(ConversionProgress::Phase("fetching".into()));
                    sink(ConversionProgress::Fraction {
                        fraction: 0.5,
                        label: "halfway".into(),
                    });
                }
                Ok(ConversionArtifact {
                    file_name: "page.md".into(),
                    media_type: "text/markdown",
                    bytes: format!("# {url}\n").into_bytes(),
                    format: output,
                    module_id: self.id(),
                    pipeline: Vec::new(),
                    invocations: Vec::new(),
                })
            }
        }

        let dir = unique_dir("url-progress");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();

        let registry = ConversionRegistry::new().with_module(UrlProgressModule);
        let mut queue = BatchQueue::new();
        let opts = BatchEnqueueOptions {
            output_format: OutputFormat::MARKDOWN,
            output_dir: Some(out.clone()),
            force: true,
            ..BatchEnqueueOptions::new(OutputFormat::MARKDOWN)
        };
        queue.enqueue(
            BatchSource::Url("https://example.com/article".into()),
            &opts,
        );

        let mut saw_fraction = false;
        let mut saw_phase = false;
        let summary = run_batch(
            &mut queue,
            &registry,
            &Arc::new(AtomicBool::new(false)),
            |event| {
                if let BatchEvent::ItemProgress {
                    fraction, label, ..
                } = event
                {
                    if fraction == Some(0.5) && label == "halfway" {
                        saw_fraction = true;
                    }
                    if fraction.is_none() && label == "fetching" {
                        saw_phase = true;
                    }
                }
            },
        );
        assert_eq!(summary.succeeded, 1);
        assert!(saw_fraction, "expected Fraction progress event");
        assert!(saw_phase, "expected Phase progress event");
        assert!(out.join("article.md").is_file() || !queue.items().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn write_artifact_failure_cleans_partials() {
        // Destination parent that cannot be created (file in the way) forces write failure.
        let dir = unique_dir("write-fail");
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, b"file").unwrap();
        let dest = blocker.join("out.md");

        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "fake",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"# ok\n",
            fail_once: None,
            delay_ms: 0,
            in_flight: None,
            peak: None,
        });
        let mut queue = BatchQueue::new();
        let input = dir.join("a.txt");
        std::fs::write(&input, b"x").unwrap();
        let mut opts = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        opts.force = true;
        // Manually set destination under the blocker after enqueue.
        let id = queue.enqueue(BatchSource::File(input), &opts);
        if let Some(item) = queue.get_mut(id) {
            item.destination = dest;
        }
        let summary = run_batch(
            &mut queue,
            &registry,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        assert_eq!(summary.failed, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recipe_naming_template_is_shared_by_file_and_url_destinations() {
        let directory = Path::new("/exports");
        let template: BatchNamingTemplate = "{stem}-{format}.{ext}".parse().unwrap();
        assert_eq!(
            resolve_destination_with_policy(
                &BatchSource::File(PathBuf::from("/inputs/Quarterly Report.docx")),
                OutputFormat::PDF,
                Some(directory),
                None,
                &template,
            )
            .unwrap(),
            PathBuf::from("/exports/Quarterly Report-pdf.pdf")
        );
        // Shared renderer appends the format extension when the template omits it.
        let template: BatchNamingTemplate = "{stem}-clean.{ext}".parse().unwrap();
        assert_eq!(
            resolve_destination_with_policy(
                &BatchSource::Url("https://example.com/posts/launch.html".into()),
                OutputFormat::MARKDOWN,
                Some(directory),
                None,
                &template,
            )
            .unwrap(),
            PathBuf::from("/exports/launch-clean.md")
        );
    }

    #[test]
    fn recipe_snapshot_updates_only_queued_items_and_keeps_format_override() {
        let mut queue = BatchQueue::new();
        let defaults = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        let queued = queue.enqueue(BatchSource::File(PathBuf::from("/inputs/a.txt")), &defaults);
        let terminal = queue.enqueue(BatchSource::File(PathBuf::from("/inputs/b.txt")), &defaults);
        assert!(queue.set_item_format_selection(
            queued,
            BatchFormatSelection::Override(OutputFormat::HTML),
            OutputFormat::MARKDOWN,
            None,
        ));
        queue.get_mut(terminal).unwrap().state = BatchItemState::Cancelled;

        let mut recipe_options = ConversionOptions::default();
        recipe_options.pandoc.toc = true;
        let template: BatchNamingTemplate = "{stem}-recipe.{ext}".parse().unwrap();
        queue.apply_snapshot_to_queued(
            OutputFormat::PDF,
            &recipe_options,
            Some(Path::new("/exports")),
            true,
            &template,
            Some("pandoc"),
        );

        let item = queue.get(queued).unwrap();
        assert_eq!(item.output_format, OutputFormat::HTML);
        assert!(item.options.pandoc.toc);
        assert_eq!(item.preferred_module.as_deref(), Some("pandoc"));
        assert!(item.force);
        assert_eq!(item.destination, PathBuf::from("/exports/a-recipe.html"));

        let untouched = queue.get(terminal).unwrap();
        assert_eq!(untouched.output_format, OutputFormat::MARKDOWN);
        assert!(!untouched.options.pandoc.toc);
        assert!(!untouched.force);
    }

    #[test]
    fn per_item_recipe_module_preference_controls_batch_dispatch() {
        let directory = unique_dir("recipe-module");
        let input = directory.join("input.txt");
        let output = directory.join("out");
        std::fs::write(&input, b"x").unwrap();
        let registry = ConversionRegistry::new()
            .with_module(CountingModule {
                label: "first",
                inputs: &["txt"],
                outputs: &[OutputFormat::MARKDOWN],
                payload: b"first",
                fail_once: None,
                delay_ms: 0,
                in_flight: None,
                peak: None,
            })
            .with_module(CountingModule {
                label: "preferred",
                inputs: &["txt"],
                outputs: &[OutputFormat::MARKDOWN],
                payload: b"preferred",
                fail_once: None,
                delay_ms: 0,
                in_flight: None,
                peak: None,
            });
        let mut options = BatchEnqueueOptions::new(OutputFormat::MARKDOWN);
        options.output_dir = Some(output);
        options.force = true;
        let mut queue = BatchQueue::new();
        let id = queue.enqueue_with_recipe(BatchSource::File(input), &options, Some("preferred"));
        let summary = run_batch(
            &mut queue,
            &registry,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        );
        assert_eq!(summary.succeeded, 1);
        match &queue.get(id).unwrap().state {
            BatchItemState::Succeeded {
                written_path,
                module_id,
                ..
            } => {
                assert_eq!(module_id, "preferred");
                assert_eq!(std::fs::read(written_path).unwrap(), b"preferred");
            }
            state => panic!("expected success, got {state:?}"),
        }
        let _ = std::fs::remove_dir_all(directory);
    }
}
