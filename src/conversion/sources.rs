//! Expand user-supplied paths (including folders) into convertible files.

use super::{ConversionError, ConversionRegistry};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Maximum directory nesting when expanding folders recursively.
pub const MAX_EXPAND_DEPTH: usize = 8;

/// Hard cap on files returned from a single expand call (and multi-root budgets).
pub const MAX_EXPAND_FILES: usize = 500;

/// Global admission limit for multi-file drops / CLI lists (same as expand cap).
pub const MAX_BATCH_ADMISSION: usize = MAX_EXPAND_FILES;

/// One expanded local input plus its path relative to the selected folder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpandedInputPath {
    pub path: PathBuf,
    pub relative_path: Option<PathBuf>,
}

impl ExpandedInputPath {
    pub fn relative_parent(&self) -> Option<&Path> {
        self.relative_path.as_deref().and_then(Path::parent)
    }
}

/// Shared file budget and canonical-path dedupe for multi-root expansion.
#[derive(Debug, Default)]
pub struct ExpandBudget {
    max_files: usize,
    visited: HashSet<PathBuf>,
    admitted: usize,
}

impl ExpandBudget {
    pub fn new(max_files: usize) -> Self {
        Self {
            max_files,
            visited: HashSet::new(),
            admitted: 0,
        }
    }

    pub fn with_default_limit() -> Self {
        Self::new(MAX_EXPAND_FILES)
    }

    pub fn max_files(&self) -> usize {
        self.max_files
    }

    pub fn admitted(&self) -> usize {
        self.admitted
    }

    pub fn is_full(&self) -> bool {
        self.admitted >= self.max_files
    }

    fn try_admit_file(&mut self) -> Result<(), ConversionError> {
        if self.admitted >= self.max_files {
            return Err(too_many_files_error(self.max_files));
        }
        self.admitted += 1;
        Ok(())
    }

    fn mark_visited(&mut self, identity: PathBuf) -> bool {
        self.visited.insert(identity)
    }
}

pub fn enforce_admission_limit(count: usize) -> Result<(), ConversionError> {
    if count > MAX_BATCH_ADMISSION {
        return Err(ConversionError::new(format!(
            "too many inputs (limit is {MAX_BATCH_ADMISSION}); narrow the selection"
        )));
    }
    Ok(())
}

fn too_many_files_error(limit: usize) -> ConversionError {
    ConversionError::new(format!(
        "too many input files (limit is {limit}); narrow the selection"
    ))
}

pub fn supported_input_extensions(registry: &ConversionRegistry) -> HashSet<String> {
    let mut set = HashSet::new();
    for module in registry.modules() {
        for ext in module.input_extensions() {
            set.insert(ext.to_ascii_lowercase());
        }
    }
    set
}

fn default_supported_input_extensions() -> &'static HashSet<String> {
    static CACHE: OnceLock<HashSet<String>> = OnceLock::new();
    CACHE.get_or_init(|| supported_input_extensions(&ConversionRegistry::default()))
}

pub fn expand_input_paths(
    paths: &[impl AsRef<Path>],
    recursive: bool,
) -> Result<Vec<PathBuf>, ConversionError> {
    Ok(expand_input_paths_preserving_roots(paths, recursive)?
        .into_iter()
        .map(|expanded| expanded.path)
        .collect())
}

pub fn expand_input_paths_preserving_roots(
    paths: &[impl AsRef<Path>],
    recursive: bool,
) -> Result<Vec<ExpandedInputPath>, ConversionError> {
    let extensions = default_supported_input_extensions();
    expand_input_paths_preserving_roots_with_extensions(paths, recursive, extensions)
}

pub fn expand_input_paths_with_extensions(
    paths: &[impl AsRef<Path>],
    recursive: bool,
    extensions: &HashSet<String>,
) -> Result<Vec<PathBuf>, ConversionError> {
    Ok(
        expand_input_paths_preserving_roots_with_extensions(paths, recursive, extensions)?
            .into_iter()
            .map(|expanded| expanded.path)
            .collect(),
    )
}

pub fn expand_input_paths_preserving_roots_with_extensions(
    paths: &[impl AsRef<Path>],
    recursive: bool,
    extensions: &HashSet<String>,
) -> Result<Vec<ExpandedInputPath>, ConversionError> {
    let mut budget = ExpandBudget::with_default_limit();
    expand_input_paths_with_budget(paths, recursive, extensions, &mut budget)
}

pub fn expand_input_paths_with_budget(
    paths: &[impl AsRef<Path>],
    recursive: bool,
    extensions: &HashSet<String>,
    budget: &mut ExpandBudget,
) -> Result<Vec<ExpandedInputPath>, ConversionError> {
    let mut out = Vec::new();
    for path in paths {
        let path = expand_tilde(path.as_ref());
        expand_top_level(&path, recursive, extensions, budget, &mut out)?;
    }
    Ok(out)
}

pub fn expand_input_paths_preserving_roots_with_budget(
    paths: &[impl AsRef<Path>],
    recursive: bool,
    budget: &mut ExpandBudget,
) -> Result<Vec<ExpandedInputPath>, ConversionError> {
    expand_input_paths_with_budget(
        paths,
        recursive,
        default_supported_input_extensions(),
        budget,
    )
}

pub fn expand_input_paths_soft(
    paths: &[impl AsRef<Path>],
    recursive: bool,
) -> Result<Vec<ExpandedInputPath>, ConversionError> {
    let extensions = default_supported_input_extensions();
    let mut budget = ExpandBudget::with_default_limit();
    let mut out = Vec::new();
    for path in paths {
        let path = expand_tilde(path.as_ref());
        if let Err(error) = expand_top_level(&path, recursive, extensions, &mut budget, &mut out) {
            if is_not_found_message(&error.to_string()) {
                continue;
            }
            return Err(error);
        }
    }
    Ok(out)
}

fn is_not_found_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("not found")
        || lower.contains("no such file")
        || lower.contains("os error 2")
        || lower.contains("the system cannot find")
}

fn expand_tilde(path: &Path) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if raw == "~" {
        home.map(PathBuf::from)
            .unwrap_or_else(|| path.to_path_buf())
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

fn expand_top_level(
    path: &Path,
    recursive: bool,
    extensions: &HashSet<String>,
    budget: &mut ExpandBudget,
    out: &mut Vec<ExpandedInputPath>,
) -> Result<(), ConversionError> {
    if is_hidden(path) {
        return Ok(());
    }

    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        ConversionError::new(format!("could not read {}: {error}", path.display()))
    })?;

    if metadata.is_dir() || (metadata.file_type().is_symlink() && path_is_dir_follow(path)) {
        if !recursive {
            return Err(ConversionError::new(format!(
                "{} is a directory; pass --recursive to expand folders",
                path.display()
            )));
        }
        let root_canonical = std::fs::canonicalize(path).map_err(|error| {
            ConversionError::new(format!(
                "could not resolve directory {}: {error}",
                path.display()
            ))
        })?;
        return expand_one(
            path,
            Some(path),
            Some(&root_canonical),
            recursive,
            extensions,
            0,
            budget,
            out,
            false,
        );
    }

    expand_one(
        path, None, None, recursive, extensions, 0, budget, out, false,
    )
}

fn path_is_dir_follow(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

fn expand_one(
    path: &Path,
    root: Option<&Path>,
    root_canonical: Option<&Path>,
    recursive: bool,
    extensions: &HashSet<String>,
    depth: usize,
    budget: &mut ExpandBudget,
    out: &mut Vec<ExpandedInputPath>,
    soft_missing: bool,
) -> Result<(), ConversionError> {
    if budget.is_full() {
        return Err(too_many_files_error(budget.max_files()));
    }

    if is_hidden(path) {
        return Ok(());
    }

    let identity = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !budget.mark_visited(identity) {
        return Ok(());
    }

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if soft_missing && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => {
            return Err(ConversionError::new(format!(
                "could not read {}: {error}",
                path.display()
            )));
        }
    };

    if metadata.file_type().is_symlink() {
        let target_meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) if soft_missing && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => {
                return Err(ConversionError::new(format!(
                    "could not follow symlink {}: {error}",
                    path.display()
                )));
            }
        };

        if let Some(root_canonical) = root_canonical {
            let target = std::fs::canonicalize(path).map_err(|error| {
                ConversionError::new(format!(
                    "could not follow symlink {}: {error}",
                    path.display()
                ))
            })?;
            if !path_is_within_root(&target, root_canonical) {
                return Err(ConversionError::new(format!(
                    "refusing to follow symlink {} — target escapes selected folder {}",
                    path.display(),
                    root_canonical.display()
                )));
            }
        }

        if target_meta.is_dir() {
            return expand_directory(
                path,
                root,
                root_canonical,
                recursive,
                extensions,
                depth,
                budget,
                out,
            );
        }
        if target_meta.is_file() {
            maybe_push_file(path, root, extensions, budget, out)?;
            return Ok(());
        }
        return Ok(());
    }

    if metadata.is_dir() {
        return expand_directory(
            path,
            root,
            root_canonical,
            recursive,
            extensions,
            depth,
            budget,
            out,
        );
    }

    if metadata.is_file() {
        maybe_push_file(path, root, extensions, budget, out)?;
    }

    Ok(())
}

fn path_is_within_root(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn expand_directory(
    path: &Path,
    root: Option<&Path>,
    root_canonical: Option<&Path>,
    recursive: bool,
    extensions: &HashSet<String>,
    depth: usize,
    budget: &mut ExpandBudget,
    out: &mut Vec<ExpandedInputPath>,
) -> Result<(), ConversionError> {
    if !recursive {
        return Err(ConversionError::new(format!(
            "{} is a directory; pass --recursive to expand folders",
            path.display()
        )));
    }
    if depth >= MAX_EXPAND_DEPTH {
        return Err(ConversionError::new(format!(
            "directory nesting exceeds maximum depth ({MAX_EXPAND_DEPTH}) at {}",
            path.display()
        )));
    }

    let entries = std::fs::read_dir(path).map_err(|error| {
        ConversionError::new(format!(
            "could not read directory {}: {error}",
            path.display()
        ))
    })?;

    // Stream entries; stop at budget (do not sort entire listing first).
    for entry in entries {
        if budget.is_full() {
            return Err(too_many_files_error(budget.max_files()));
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ConversionError::new(format!(
                    "could not read entry in {}: {error}",
                    path.display()
                )));
            }
        };
        let child = entry.path();
        if is_hidden(&child) {
            continue;
        }
        expand_one(
            &child,
            root,
            root_canonical,
            recursive,
            extensions,
            depth + 1,
            budget,
            out,
            true,
        )?;
    }
    Ok(())
}

fn maybe_push_file(
    path: &Path,
    root: Option<&Path>,
    extensions: &HashSet<String>,
    budget: &mut ExpandBudget,
    out: &mut Vec<ExpandedInputPath>,
) -> Result<(), ConversionError> {
    let Some(ext) = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
    else {
        return Ok(());
    };
    if !extensions.contains(&ext) {
        return Ok(());
    }
    budget.try_admit_file()?;
    let relative_path = root.and_then(|root| path.strip_prefix(root).ok().map(Path::to_path_buf));
    out.push(ExpandedInputPath {
        path: path.to_path_buf(),
        relative_path,
    });
    Ok(())
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn unique_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "shift-expand-{}-{}-{}",
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
    fn expands_files_and_filters_extensions() {
        let dir = unique_dir("files");
        let pdf = dir.join("a.pdf");
        let txt = dir.join("b.txt");
        let bin = dir.join("c.bin");
        std::fs::write(&pdf, b"%PDF").unwrap();
        std::fs::write(&txt, b"hi").unwrap();
        std::fs::write(&bin, b"xx").unwrap();
        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        exts.insert("txt".into());
        let found = expand_input_paths_with_extensions(
            &[pdf.as_path(), bin.as_path(), txt.as_path()],
            false,
            &exts,
        )
        .unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.contains(&pdf));
        assert!(found.contains(&txt));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn directory_without_recursive_errors() {
        let dir = unique_dir("norec");
        let err = expand_input_paths(&[dir.as_path()], false).unwrap_err();
        assert!(err.to_string().contains("--recursive"), "error: {err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recursive_skips_hidden_and_finds_nested() {
        let dir = unique_dir("rec");
        let nested = dir.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        let visible = nested.join("doc.pdf");
        let hidden_dir = dir.join(".secret");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        std::fs::write(&visible, b"%PDF").unwrap();
        std::fs::write(hidden_dir.join("secret.pdf"), b"%PDF").unwrap();
        std::fs::write(dir.join(".hidden.pdf"), b"%PDF").unwrap();
        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let found = expand_input_paths_with_extensions(&[dir.as_path()], true, &exts).unwrap();
        assert_eq!(found, vec![visible]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn recursive_expansion_finds_all_files() {
        let dir = unique_dir("all-files");
        for name in ["z.pdf", "a.pdf", "m.pdf"] {
            std::fs::write(dir.join(name), b"%PDF").unwrap();
        }
        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let mut found = expand_input_paths_with_extensions(&[dir.as_path()], true, &exts).unwrap();
        found.sort();
        assert_eq!(
            found,
            vec![dir.join("a.pdf"), dir.join("m.pdf"), dir.join("z.pdf")]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn too_many_files_rejects_expansion() {
        let dir = unique_dir("many");
        let mut files = Vec::new();
        for i in 0..MAX_EXPAND_FILES + 1 {
            let path = dir.join(format!("doc{i}.txt"));
            std::fs::write(&path, b"x").unwrap();
            files.push(path);
        }
        let mut exts = HashSet::new();
        exts.insert("txt".into());
        let result = expand_input_paths_with_extensions(&files, false, &exts);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("too many input files")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn multi_root_budget_is_global_not_per_root() {
        let half = (MAX_EXPAND_FILES / 2) + 1;
        let root_a = unique_dir("budget-a");
        let root_b = unique_dir("budget-b");
        for i in 0..half {
            std::fs::write(root_a.join(format!("a{i}.txt")), b"x").unwrap();
            std::fs::write(root_b.join(format!("b{i}.txt")), b"x").unwrap();
        }
        let mut exts = HashSet::new();
        exts.insert("txt".into());
        let mut budget = ExpandBudget::with_default_limit();
        let result = expand_input_paths_with_budget(
            &[root_a.as_path(), root_b.as_path()],
            true,
            &exts,
            &mut budget,
        );
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    #[test]
    fn enforce_admission_limit_rejects_over_cap() {
        enforce_admission_limit(MAX_BATCH_ADMISSION).unwrap();
        let err = enforce_admission_limit(MAX_BATCH_ADMISSION + 1).unwrap_err();
        assert!(err.to_string().contains("too many inputs"));
    }

    #[cfg(unix)]
    #[test]
    fn expansion_rejects_symlink_escaping_selected_root() {
        use std::os::unix::fs::symlink;
        let outside = unique_dir("escape-out");
        std::fs::write(outside.join("secret.pdf"), b"%PDF").unwrap();
        let root = unique_dir("escape-root");
        symlink(&outside, &root.join("outside-link")).unwrap();
        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let err = expand_input_paths_with_extensions(&[root.as_path()], true, &exts).unwrap_err();
        assert!(
            err.to_string().contains("escapes selected folder")
                || err.to_string().contains("refusing to follow symlink"),
            "error: {err}"
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn expansion_skips_symlink_cycles() {
        use std::os::unix::fs::symlink;
        let dir = unique_dir("cycle");
        let target = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        symlink(&target, &dir.join("link")).unwrap();
        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let found = expand_input_paths_with_extensions(&[dir.as_path()], true, &exts).unwrap();
        assert!(found.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn soft_expand_skips_missing_top_level() {
        let missing = PathBuf::from("/definitely-does-not-exist-soft-12345");
        let found = expand_input_paths_soft(&[missing.as_path()], false).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn missing_file_errors() {
        let exts = HashSet::new();
        let result = expand_input_paths_with_extensions(
            &[PathBuf::from("/definitely-does-not-exist-12345.xyz")],
            false,
            &exts,
        );
        assert!(result.is_err());
    }

    #[test]
    fn path_is_within_root_is_component_aware() {
        assert!(path_is_within_root(
            Path::new("/tmp/root/a"),
            Path::new("/tmp/root")
        ));
        assert!(!path_is_within_root(
            Path::new("/tmp/root-evil/a"),
            Path::new("/tmp/root")
        ));
    }

    #[test]
    fn expansion_honors_maximum_depth() {
        let mut dir = unique_dir("deep");
        let root = dir.clone();
        for _ in 0..MAX_EXPAND_DEPTH + 1 {
            dir = dir.join("sub");
            std::fs::create_dir_all(&dir).unwrap();
        }
        let mut exts = HashSet::new();
        exts.insert("txt".into());
        let result = expand_input_paths_with_extensions(&[root.as_path()], true, &exts);
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn empty_input_list_returns_empty() {
        let exts = HashSet::new();
        let found = expand_input_paths_with_extensions(&[] as &[PathBuf], true, &exts).unwrap();
        assert!(found.is_empty());
    }
}
