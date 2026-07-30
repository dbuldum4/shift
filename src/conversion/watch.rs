//! Safe, polling watched-folder discovery shared by workflow surfaces.
//!
//! Dispatched state is updated only after the conversion outcome is known.

use super::{ConversionError, ExpandedInputPath, expand_input_paths_soft};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileFingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackedState {
    Pending,
    InFlight,
    Done,
}

#[derive(Clone, Debug)]
struct TrackedFile {
    fingerprint: FileFingerprint,
    observed_at: SystemTime,
    state: TrackedState,
}

#[derive(Clone, Debug, Default)]
pub struct WatchTracker {
    files: HashMap<PathBuf, TrackedFile>,
}

impl WatchTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(
        &mut self,
        input_dir: &Path,
    ) -> Result<Vec<ExpandedInputPath>, ConversionError> {
        let paths = expand_input_paths_soft(&[input_dir], true)?;
        let now = SystemTime::now();
        self.files.clear();
        let mut ready = Vec::new();
        for expanded in paths {
            let Some(fingerprint) = fingerprint_soft(&expanded.path)? else {
                continue;
            };
            self.files.insert(
                watch_identity(&expanded.path),
                TrackedFile {
                    fingerprint,
                    observed_at: now,
                    state: TrackedState::InFlight,
                },
            );
            ready.push(expanded);
        }
        Ok(ready)
    }

    pub fn poll(
        &mut self,
        input_dir: &Path,
        now: SystemTime,
        debounce: Duration,
    ) -> Result<Vec<ExpandedInputPath>, ConversionError> {
        let paths = expand_input_paths_soft(&[input_dir], true)?;
        let mut present = HashMap::with_capacity(paths.len());
        let mut ready = Vec::new();

        for expanded in paths {
            let identity = watch_identity(&expanded.path);
            let Some(current) = fingerprint_soft(&expanded.path)? else {
                continue;
            };
            present.insert(identity.clone(), current.clone());

            match self.files.get_mut(&identity) {
                Some(entry) if entry.fingerprint == current => match entry.state {
                    TrackedState::Pending
                        if now
                            .duration_since(entry.observed_at)
                            .unwrap_or(Duration::ZERO)
                            >= debounce =>
                    {
                        entry.state = TrackedState::InFlight;
                        ready.push(expanded);
                    }
                    TrackedState::InFlight | TrackedState::Done | TrackedState::Pending => {}
                },
                Some(entry) => {
                    entry.fingerprint = current;
                    entry.observed_at = now;
                    entry.state = TrackedState::Pending;
                }
                None => {
                    self.files.insert(
                        identity,
                        TrackedFile {
                            fingerprint: current,
                            observed_at: now,
                            state: TrackedState::Pending,
                        },
                    );
                }
            }
        }

        self.files.retain(|path, _| present.contains_key(path));
        Ok(ready)
    }

    pub fn report_outcome(&mut self, path: &Path, success: bool) {
        let identity = watch_identity(path);
        if let Some(entry) = self.files.get_mut(&identity) {
            entry.state = if success {
                TrackedState::Done
            } else {
                TrackedState::Pending
            };
        }
    }

    pub fn report_outcomes(&mut self, results: impl IntoIterator<Item = (PathBuf, bool)>) {
        for (path, success) in results {
            self.report_outcome(&path, success);
        }
    }
}

pub fn validate_watch_directories(
    input_dir: &Path,
    output_dir: &Path,
) -> Result<(), ConversionError> {
    let input = std::fs::canonicalize(input_dir).map_err(|error| {
        ConversionError::new(format!(
            "could not open watched folder {}: {error}",
            input_dir.display()
        ))
    })?;
    if !input.is_dir() {
        return Err(ConversionError::new(format!(
            "watched path is not a directory: {}",
            input_dir.display()
        )));
    }

    let output = canonicalize_future_path(output_dir)?;
    if output == input || output.starts_with(&input) {
        return Err(ConversionError::new(format!(
            "watched-folder output must be outside the input folder to prevent a conversion loop ({} is inside {})",
            output_dir.display(),
            input_dir.display()
        )));
    }
    Ok(())
}

fn fingerprint_soft(path: &Path) -> Result<Option<FileFingerprint>, ConversionError> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(FileFingerprint {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConversionError::new(format!(
            "could not inspect watched file {}: {error}",
            path.display()
        ))),
    }
}

fn watch_identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn canonicalize_future_path(path: &Path) -> Result<PathBuf, ConversionError> {
    let mut tail = Vec::new();
    let mut ancestor = path;
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return Err(ConversionError::new(format!(
                "could not resolve output folder {}",
                path.display()
            )));
        };
        tail.push(name.to_os_string());
        ancestor = ancestor.parent().ok_or_else(|| {
            ConversionError::new(format!(
                "could not resolve output folder {}",
                path.display()
            ))
        })?;
    }
    let mut resolved = std::fs::canonicalize(ancestor).map_err(|error| {
        ConversionError::new(format!(
            "could not resolve output folder {}: {error}",
            path.display()
        ))
    })?;
    for segment in tail.iter().rev() {
        resolved.push(segment);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "shift-watch-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
            name
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn ready_paths(items: &[ExpandedInputPath]) -> Vec<PathBuf> {
        items.iter().map(|item| item.path.clone()).collect()
    }

    #[test]
    fn watcher_debounces_and_does_not_requeue_unchanged_files() {
        let dir = unique_dir("debounce");
        let file = dir.join("note.md");
        fs::write(&file, "first").unwrap();
        let start = SystemTime::now();
        let mut watcher = WatchTracker::new();
        assert!(
            watcher
                .poll(&dir, start, Duration::from_secs(2))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ready_paths(
                &watcher
                    .poll(&dir, start + Duration::from_secs(2), Duration::from_secs(2))
                    .unwrap()
            ),
            vec![file.clone()]
        );
        assert!(
            watcher
                .poll(
                    &dir,
                    start + Duration::from_secs(10),
                    Duration::from_secs(2)
                )
                .unwrap()
                .is_empty()
        );
        watcher.report_outcome(&file, true);
        assert!(
            watcher
                .poll(
                    &dir,
                    start + Duration::from_secs(20),
                    Duration::from_secs(2)
                )
                .unwrap()
                .is_empty()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_conversion_is_retried_on_next_poll() {
        let dir = unique_dir("retry");
        let file = dir.join("note.md");
        fs::write(&file, "first").unwrap();
        let start = SystemTime::now();
        let mut watcher = WatchTracker::new();
        let _ = watcher.poll(&dir, start, Duration::ZERO).unwrap();
        assert_eq!(
            ready_paths(&watcher.poll(&dir, start, Duration::ZERO).unwrap()),
            vec![file.clone()]
        );
        watcher.report_outcome(&file, false);
        assert_eq!(
            ready_paths(
                &watcher
                    .poll(&dir, start + Duration::from_secs(1), Duration::ZERO)
                    .unwrap()
            ),
            vec![file]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn changed_file_becomes_eligible_after_another_debounce() {
        let dir = unique_dir("change");
        let file = dir.join("note.md");
        fs::write(&file, "first").unwrap();
        let start = SystemTime::now();
        let mut watcher = WatchTracker::new();
        let _ = watcher.poll(&dir, start, Duration::ZERO).unwrap();
        assert_eq!(
            ready_paths(&watcher.poll(&dir, start, Duration::ZERO).unwrap()),
            vec![file.clone()]
        );
        watcher.report_outcome(&file, true);
        fs::write(&file, "changed content").unwrap();
        assert!(
            watcher
                .poll(&dir, start + Duration::from_secs(1), Duration::ZERO)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ready_paths(
                &watcher
                    .poll(&dir, start + Duration::from_secs(1), Duration::ZERO)
                    .unwrap()
            ),
            vec![file]
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshot_preserves_nested_relative_parents() {
        let dir = unique_dir("nested");
        let nested = dir.join("team").join("drafts");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("note.md");
        fs::write(&file, "nested").unwrap();
        let mut watcher = WatchTracker::new();
        let snapshot = watcher.snapshot(&dir).unwrap();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].path, file);
        assert_eq!(
            snapshot[0].relative_parent(),
            Some(Path::new("team/drafts"))
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn output_inside_input_is_rejected() {
        let dir = unique_dir("loop");
        let err = validate_watch_directories(&dir, &dir.join("out")).unwrap_err();
        assert!(err.to_string().contains("prevent a conversion loop"));
        assert!(validate_watch_directories(&dir, &unique_dir("outside")).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn soft_fingerprint_skips_missing_files() {
        let missing = PathBuf::from("/definitely-missing-watch-file-12345.md");
        assert!(fingerprint_soft(&missing).unwrap().is_none());
    }
}
