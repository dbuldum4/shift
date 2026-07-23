//! Expand user-supplied paths (including folders) into convertible files.

use super::{ConversionError, ConversionRegistry};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Maximum directory nesting when expanding folders recursively.
pub const MAX_EXPAND_DEPTH: usize = 8;

/// Hard cap on files returned from a single expand call.
pub const MAX_EXPAND_FILES: usize = 500;

/// Collect the union of input extensions from every module in `registry`.
pub fn supported_input_extensions(registry: &ConversionRegistry) -> HashSet<String> {
    let mut set = HashSet::new();
    for module in registry.modules() {
        for ext in module.input_extensions() {
            set.insert(ext.to_ascii_lowercase());
        }
    }
    set
}

/// Expand files and directories into a flat list of convertible file paths.
///
/// - Files are included when their extension is in `extensions` (case-insensitive).
/// - Directories require `recursive == true`; otherwise an error asks for `--recursive`.
/// - Hidden entries (name starts with `.`) are skipped.
/// - Symlink cycles are avoided via visited real-path tracking.
/// - Depth is capped at [`MAX_EXPAND_DEPTH`]; file count at [`MAX_EXPAND_FILES`].
pub fn expand_input_paths(
    paths: &[impl AsRef<Path>],
    recursive: bool,
) -> Result<Vec<PathBuf>, ConversionError> {
    let extensions = supported_input_extensions(&ConversionRegistry::default());
    expand_input_paths_with_extensions(paths, recursive, &extensions)
}

/// Like [`expand_input_paths`], but uses an explicit extension allow-list.
pub fn expand_input_paths_with_extensions(
    paths: &[impl AsRef<Path>],
    recursive: bool,
    extensions: &HashSet<String>,
) -> Result<Vec<PathBuf>, ConversionError> {
    let mut out = Vec::new();
    let mut visited = HashSet::new();

    for path in paths {
        let path = path.as_ref();
        expand_one(path, recursive, extensions, 0, &mut visited, &mut out)?;
    }

    Ok(out)
}

fn expand_one(
    path: &Path,
    recursive: bool,
    extensions: &HashSet<String>,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) -> Result<(), ConversionError> {
    if out.len() >= MAX_EXPAND_FILES {
        return Err(ConversionError::new(format!(
            "too many input files (limit is {MAX_EXPAND_FILES}); narrow the selection"
        )));
    }

    if is_hidden(path) {
        return Ok(());
    }

    // Resolve for cycle detection; fall back to the given path if it does not exist yet.
    let identity = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(identity) {
        return Ok(());
    }

    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        ConversionError::new(format!("could not read {}: {error}", path.display()))
    })?;

    if metadata.file_type().is_symlink() {
        // Follow once via the non-symlink metadata / recurse into target.
        let target_meta = std::fs::metadata(path).map_err(|error| {
            ConversionError::new(format!(
                "could not follow symlink {}: {error}",
                path.display()
            ))
        })?;
        if target_meta.is_dir() {
            return expand_directory(path, recursive, extensions, depth, visited, out);
        }
        if target_meta.is_file() {
            maybe_push_file(path, extensions, out);
            return Ok(());
        }
        return Ok(());
    }

    if metadata.is_dir() {
        return expand_directory(path, recursive, extensions, depth, visited, out);
    }

    if metadata.is_file() {
        maybe_push_file(path, extensions, out);
    }

    Ok(())
}

fn expand_directory(
    path: &Path,
    recursive: bool,
    extensions: &HashSet<String>,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
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

    for entry in entries {
        let entry = entry.map_err(|error| {
            ConversionError::new(format!(
                "could not read entry in {}: {error}",
                path.display()
            ))
        })?;
        let child = entry.path();
        if is_hidden(&child) {
            continue;
        }
        expand_one(&child, recursive, extensions, depth + 1, visited, out)?;
        if out.len() > MAX_EXPAND_FILES {
            return Err(ConversionError::new(format!(
                "too many input files (limit is {MAX_EXPAND_FILES}); narrow the selection"
            )));
        }
    }
    Ok(())
}

fn maybe_push_file(path: &Path, extensions: &HashSet<String>, out: &mut Vec<PathBuf>) {
    let Some(ext) = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
    else {
        return;
    };
    if extensions.contains(&ext) {
        out.push(path.to_path_buf());
    }
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
        let hidden_file = hidden_dir.join("secret.pdf");
        std::fs::write(&visible, b"%PDF").unwrap();
        std::fs::write(&hidden_file, b"%PDF").unwrap();
        std::fs::write(dir.join(".hidden.pdf"), b"%PDF").unwrap();

        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let found = expand_input_paths_with_extensions(&[dir.as_path()], true, &exts).unwrap();
        assert_eq!(found, vec![visible]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn default_registry_extensions_include_common_types() {
        let exts = supported_input_extensions(&ConversionRegistry::default());
        assert!(exts.contains("pdf"));
        assert!(exts.contains("mp4"));
        assert!(exts.contains("docx"));
    }

    #[test]
    fn expansion_honors_maximum_depth() {
        let mut dir = unique_dir("deep");
        let root = dir.clone();
        // Build a chain of nested directories deeper than MAX_EXPAND_DEPTH.
        for _ in 0..MAX_EXPAND_DEPTH + 1 {
            dir = dir.join("sub");
            std::fs::create_dir_all(&dir).unwrap();
        }

        let mut exts = HashSet::new();
        exts.insert("txt".into());
        let result = expand_input_paths_with_extensions(&[root.as_path()], true, &exts);
        assert!(result.is_err(), "expected depth limit error");
        assert!(result.unwrap_err().to_string().contains("maximum depth"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn expansion_skips_symlink_cycles() {
        use std::os::unix::fs::symlink;

        let dir = unique_dir("cycle");
        let target = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        let link = dir.join("link");
        symlink(&target, &link).unwrap();

        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let found = expand_input_paths_with_extensions(&[dir.as_path()], true, &exts).unwrap();
        // Should not recurse infinitely and should not return the symlink itself as a file.
        assert!(found.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
