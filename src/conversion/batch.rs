//! Shared batch conversion queue and runner.
//!
//! Both the native app and `shift-cli` use this module so multi-file conversion,
//! destination resolution, per-item progress, retry, and cancellation stay
//! identical across surfaces. Callers own presentation and I/O threading;
//! this module owns queue state transitions and convert-then-write execution.

use super::{
    ConversionArtifact, ConversionError, ConversionOptions, ConversionProgress, ConversionRegistry,
    OutputFormat, ProgressSink, default_output_path, looks_like_url, paths_refer_to_same_file,
};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
            Self::Url(url) => url.clone(),
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

/// One unit of work in the batch queue.
#[derive(Clone, Debug)]
pub struct BatchItem {
    pub id: BatchItemId,
    pub source: BatchSource,
    pub output_format: OutputFormat,
    /// Inherit vs per-item override; [`Self::output_format`] is the resolved value.
    pub format_selection: BatchFormatSelection,
    pub options: ConversionOptions,
    /// Planned destination (may be adjusted on write for collisions).
    pub destination: PathBuf,
    pub force: bool,
    pub state: BatchItemState,
    pub attempts: u32,
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
    pub force: bool,
}

impl BatchEnqueueOptions {
    pub fn new(output_format: OutputFormat) -> Self {
        Self {
            output_format,
            conversion: ConversionOptions::default(),
            output_dir: None,
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
    pub fn exit_code(self) -> u8 {
        if self.failed > 0 {
            1
        } else if self.cancelled > 0 && self.succeeded == 0 {
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
        let destination =
            resolve_destination(&source, opts.output_format, opts.output_dir.as_deref());
        let id = BatchItemId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        self.items.push(BatchItem {
            id,
            source,
            output_format: opts.output_format,
            format_selection: BatchFormatSelection::Inherit,
            options: opts.conversion.clone(),
            destination,
            force: opts.force,
            state: BatchItemState::Queued,
            attempts: 0,
        });
        id
    }

    /// Enqueue many sources (files or URLs).
    pub fn enqueue_many(
        &mut self,
        sources: impl IntoIterator<Item = BatchSource>,
        opts: &BatchEnqueueOptions,
    ) -> Vec<BatchItemId> {
        sources
            .into_iter()
            .map(|source| self.enqueue(source, opts))
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

    /// Drop everything.
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Update planned destinations after the user picks a new output folder.
    pub fn set_output_dir(&mut self, output_dir: Option<&Path>) {
        for item in &mut self.items {
            if matches!(item.state, BatchItemState::Queued) {
                item.destination =
                    resolve_destination(&item.source, item.output_format, output_dir);
            }
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
            item.destination = resolve_destination(&item.source, format, output_dir);
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
        let Some(item) = self.get_mut(id) else {
            return false;
        };
        if !matches!(item.state, BatchItemState::Queued) {
            return false;
        }
        item.format_selection = selection;
        let format = selection.resolve(inherited);
        item.output_format = format;
        item.destination = resolve_destination(&item.source, format, output_dir);
        true
    }

    /// Ensure queued items do not share planned destinations with each other
    /// or with already-written outputs (even when `force` is set).
    ///
    /// Cross-source name clashes (e.g. two `report.pdf` into one folder) become
    /// `report.md`, `report-1.md`, … so both can succeed without overwriting.
    pub fn uniquify_planned_destinations(&mut self) {
        let mut claimed: Vec<PathBuf> = Vec::new();
        for item in &self.items {
            match &item.state {
                BatchItemState::Succeeded { written_path, .. } => {
                    claimed.push(written_path.clone());
                }
                BatchItemState::Queued => {}
                _ => {
                    // Running / failed / cancelled still reserve their planned path
                    // so a re-queue later can uniquify against them if needed.
                    claimed.push(item.destination.clone());
                }
            }
        }

        for item in &mut self.items {
            if !matches!(item.state, BatchItemState::Queued) {
                continue;
            }
            // Only avoid paths claimed by this queue — on-disk existence without
            // force is enforced at write time (same as single-file).
            let dest = uniquify_against_claimed(&item.destination, &claimed, false);
            claimed.push(dest.clone());
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
    match source {
        BatchSource::File(path) => {
            let default = default_output_path(path, format);
            if let Some(dir) = output_dir {
                let name = default
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(format!("converted.{}", format.extension())));
                dir.join(name)
            } else {
                default
            }
        }
        BatchSource::Url(url) => {
            let name = suggested_url_file_name(url, format);
            if let Some(dir) = output_dir {
                dir.join(name)
            } else {
                PathBuf::from(name)
            }
        }
    }
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

    if destination.exists() && !force {
        return Err(ConversionError::new(format!(
            "output already exists: {} (pass --force / enable Overwrite to replace)",
            destination.display()
        )));
    }

    if let Some(parent) = destination.parent() {
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

/// If `preferred` exists and `force` is false, pick `stem-1.ext`, `stem-2.ext`, …
pub fn uniquify_destination(preferred: &Path, force: bool) -> PathBuf {
    if force || !preferred.exists() {
        return preferred.to_path_buf();
    }
    uniquify_against_claimed(preferred, &[], true)
}

/// Pick a path not present in `claimed` (and optionally not already on disk).
fn uniquify_against_claimed(preferred: &Path, claimed: &[PathBuf], check_disk: bool) -> PathBuf {
    let is_taken = |path: &Path| -> bool {
        claimed.iter().any(|other| other == path) || (check_disk && path.exists())
    };
    if !is_taken(preferred) {
        return preferred.to_path_buf();
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
    prepare_batch_destination(planned, source, force)?;
    // Atomic write: partial sibling then rename. On failure scrub any leftovers.
    match artifact.write_to(planned) {
        Ok(()) => Ok(planned.to_path_buf()),
        Err(error) => {
            let _ = super::remove_partial_outputs(planned);
            Err(error)
        }
    }
}

/// Immutable per-item snapshot handed to a worker thread.
struct BatchTask {
    id: BatchItemId,
    source: BatchSource,
    format: OutputFormat,
    options: ConversionOptions,
    destination: PathBuf,
    force: bool,
}

/// Terminal result of converting one item, reported back to the main thread.
enum BatchOutcome {
    Succeeded {
        path: PathBuf,
        module_id: String,
        byte_len: usize,
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

    let result =
        convert_source(registry, &task.source, task.format, &options).and_then(|artifact| {
            if cancel.load(Ordering::SeqCst) {
                return Err(ConversionError::cancelled());
            }
            write_artifact(
                &artifact,
                &task.destination,
                task.source.as_file(),
                task.force,
            )
            .map(|path| (path, artifact.module_id.to_owned(), artifact.bytes.len()))
        });

    match result {
        Ok((path, module_id, byte_len)) => BatchOutcome::Succeeded {
            path,
            module_id,
            byte_len,
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
            destination: item.destination.clone(),
            force: item.force,
        })
        .collect();

    if !tasks.is_empty() {
        let parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        let worker_count = parallelism.min(tasks.len()).max(1);

        // Shared cursor: each worker claims the next index with fetch_add.
        let cursor = std::sync::atomic::AtomicUsize::new(0);
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
                        let _ = tx.send(WorkerMsg::Started { id: task.id });
                        // Isolate a panicking module: a conversion bug should
                        // become a single failed item, not an aborted batch.
                        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
                            run_task(registry, task, cancel, &tx)
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
                            id: task.id,
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
                                }
                                summary.succeeded += 1;
                                on_event(BatchEvent::ItemSucceeded {
                                    id,
                                    source_name,
                                    path,
                                    module_id,
                                    byte_len,
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
        fn output_formats(&self) -> &'static [OutputFormat] {
            self.outputs
        }
        fn chainable_output_formats(&self) -> &'static [OutputFormat] {
            &[]
        }
        fn convert(
            &self,
            input: &Path,
            output: OutputFormat,
            options: &ConversionOptions,
        ) -> Result<ConversionArtifact, ConversionError> {
            if self.delay_ms > 0 {
                let steps = (self.delay_ms / 10).max(1);
                for _ in 0..steps {
                    if options
                        .cancel
                        .as_ref()
                        .is_some_and(|flag| flag.load(Ordering::SeqCst))
                    {
                        return Err(ConversionError::cancelled());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            if let Some(flag) = &self.fail_once {
                let mut guard = flag.lock().unwrap();
                if !*guard {
                    *guard = true;
                    return Err(ConversionError::new("simulated failure"));
                }
            }
            let _ = input;
            Ok(ConversionArtifact {
                file_name: format!("out.{}", output.extension()),
                media_type: output.media_type(),
                bytes: self.payload.to_vec(),
                format: output,
                module_id: self.label,
                pipeline: Vec::new(),
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
        assert!(out.join("a.md").is_file() || out.join("out.md").is_file());
        // Destination uses default_output_path stem from input name.
        assert!(
            std::fs::read_to_string(out.join("a.md")).unwrap_or_default() == "# ok\n"
                || queue
                    .items()
                    .iter()
                    .all(|item| matches!(item.state, BatchItemState::Succeeded { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, BatchEvent::ItemSucceeded { .. }))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn run_batch_executes_items_in_parallel() {
        // Each conversion sleeps for `per_item`; run enough items that a
        // sequential runner would clearly exceed the parallel wall time.
        const ITEMS: usize = 4;
        const PER_ITEM_MS: u64 = 150;

        let dir = unique_dir("parallel");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        for i in 0..ITEMS {
            std::fs::write(dir.join(format!("f{i}.txt")), b"x").unwrap();
        }

        let registry = ConversionRegistry::new().with_module(CountingModule {
            label: "slow",
            inputs: &["txt"],
            outputs: &[OutputFormat::MARKDOWN],
            payload: b"# ok\n",
            fail_once: None,
            delay_ms: PER_ITEM_MS,
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
        let start = std::time::Instant::now();
        let summary = run_batch(&mut queue, &registry, &cancel, |_| {});
        let elapsed = start.elapsed();

        // Correctness holds regardless of core count.
        assert_eq!(summary.succeeded, ITEMS);
        assert_eq!(summary.failed, 0);
        assert!(
            queue
                .items()
                .iter()
                .all(|item| matches!(item.state, BatchItemState::Succeeded { .. }))
        );

        // On multi-core machines the pool must beat the sequential baseline.
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        if cores >= 2 {
            let sequential = std::time::Duration::from_millis(PER_ITEM_MS * ITEMS as u64);
            assert!(
                elapsed < sequential,
                "parallel run took {elapsed:?}, expected well under sequential {sequential:?}"
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
            fn output_formats(&self) -> &'static [OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &'static [OutputFormat] {
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

        assert!(summary.cancelled >= 1 || summary.succeeded + summary.cancelled == 2);
        assert!(
            queue
                .items()
                .iter()
                .any(|item| matches!(item.state, BatchItemState::Cancelled))
                || queue.progress().cancelled >= 1
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
            fn output_formats(&self) -> &'static [OutputFormat] {
                &[OutputFormat::MARKDOWN]
            }
            fn chainable_output_formats(&self) -> &'static [OutputFormat] {
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
        // Claim preferred and every numeric suffix the short loop tries.
        let mut claimed = vec![preferred.clone()];
        for index in 1..10_000 {
            claimed.push(PathBuf::from(format!("/out/report-{index}.md")));
        }
        let resolved = uniquify_against_claimed(&preferred, &claimed, false);
        assert!(
            !claimed.contains(&resolved),
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
            source: BatchSource::File(PathBuf::from("/tmp/x.pdf")),
            output_format: OutputFormat::MARKDOWN,
            format_selection: BatchFormatSelection::Override(OutputFormat::HTML),
            options: ConversionOptions::default(),
            destination: PathBuf::from("/tmp/x.html"),
            force: false,
            state: BatchItemState::Queued,
            attempts: 0,
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
            0
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

    #[test]
    fn uniquify_against_claimed_falls_back_to_token() {
        let preferred = PathBuf::from("/out/report.md");
        let mut claimed: Vec<PathBuf> = (1..10_000)
            .map(|i| PathBuf::from(format!("/out/report-{i}.md")))
            .collect();
        claimed.push(preferred.clone());

        let resolved = uniquify_against_claimed(&preferred, &claimed, false);
        assert!(!claimed.contains(&resolved));
        assert!(
            resolved
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("report-") && n.ends_with(".md")),
            "unexpected: {}",
            resolved.display()
        );
    }
}
