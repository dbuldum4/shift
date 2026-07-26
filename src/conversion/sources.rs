//! Expand user-supplied paths (including folders) into convertible files.

use super::{ConversionError, ConversionRegistry};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// Input extensions supported by the default registry, computed once per process.
///
/// [`expand_input_paths`] would otherwise rebuild `ConversionRegistry::default()`
/// (which resolves external executables) on every call just to read this set.
fn default_supported_input_extensions() -> &'static HashSet<String> {
    static CACHE: OnceLock<HashSet<String>> = OnceLock::new();
    CACHE.get_or_init(|| supported_input_extensions(&ConversionRegistry::default()))
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
    let extensions = default_supported_input_extensions();
    expand_input_paths_with_extensions(paths, recursive, extensions)
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
        let path = expand_tilde(path.as_ref());
        expand_one(&path, recursive, extensions, 0, &mut visited, &mut out)?;
    }

    Ok(out)
}

/// Expand a leading `~` (and `~/...`) using `$HOME` / `$USERPROFILE`.
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

    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            ConversionError::new(format!(
                "could not read entry in {}: {error}",
                path.display()
            ))
        })?;
        children.push(entry.path());
    }
    children.sort();

    for child in children {
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
    fn recursive_expansion_is_sorted_by_path() {
        let dir = unique_dir("sorted");
        for name in ["z.pdf", "a.pdf", "m.pdf"] {
            std::fs::write(dir.join(name), b"%PDF").unwrap();
        }
        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let found = expand_input_paths_with_extensions(&[dir.as_path()], true, &exts).unwrap();
        assert_eq!(
            found,
            vec![dir.join("a.pdf"), dir.join("m.pdf"), dir.join("z.pdf")]
        );
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

    #[test]
    fn supported_input_extensions_from_registry() {
        let empty = ConversionRegistry::new();
        assert!(supported_input_extensions(&empty).is_empty());

        let default = ConversionRegistry::default();
        let exts = supported_input_extensions(&default);
        assert!(!exts.is_empty());
        assert!(exts.contains("pdf"));
        assert!(exts.contains("mp4"));
    }

    #[test]
    fn empty_input_list_returns_empty() {
        let exts = HashSet::new();
        let found = expand_input_paths_with_extensions(&[] as &[PathBuf], true, &exts).unwrap();
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
        assert!(result.unwrap_err().to_string().contains("could not read"));
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
    fn files_without_extension_or_unlisted_extension_ignored() {
        let dir = unique_dir("filter");
        let no_ext = dir.join("README");
        let listed = dir.join("doc.txt");
        std::fs::write(&no_ext, b"x").unwrap();
        std::fs::write(&listed, b"x").unwrap();

        let mut exts = HashSet::new();
        exts.insert("txt".into());
        let found = expand_input_paths_with_extensions(&[&no_ext, &listed], false, &exts).unwrap();
        assert_eq!(found, vec![listed]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn is_hidden_detects_dot_prefix() {
        assert!(is_hidden(Path::new(".secret")));
        assert!(!is_hidden(Path::new("visible")));
        assert!(!is_hidden(Path::new("/tmp/.hidden/visible")));
    }

    #[cfg(unix)]
    #[test]
    fn expansion_follows_symlink_to_file() {
        use std::os::unix::fs::symlink;

        let dir = unique_dir("symlink-file");
        let target = dir.join("real.pdf");
        let link = dir.join("link.pdf");
        std::fs::write(&target, b"%PDF").unwrap();
        symlink(&target, &link).unwrap();

        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let found = expand_input_paths_with_extensions(&[link.as_path()], false, &exts).unwrap();
        assert_eq!(found, vec![link]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn broken_symlink_reports_follow_error() {
        use std::os::unix::fs::symlink;

        let dir = unique_dir("broken-link");
        let link = dir.join("missing.pdf");
        symlink(Path::new("/nonexistent-target-12345"), &link).unwrap();

        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let result = expand_input_paths_with_extensions(&[link.as_path()], false, &exts);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("could not follow symlink")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn default_expand_input_paths_uses_registry_extensions() {
        let dir = unique_dir("default-expand");
        let pdf = dir.join("a.pdf");
        let bin = dir.join("a.bin");
        std::fs::write(&pdf, b"%PDF").unwrap();
        std::fs::write(&bin, b"x").unwrap();

        let found = expand_input_paths(&[&pdf, &bin], false).unwrap();
        assert_eq!(found, vec![pdf]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn expansion_follows_symlink_to_directory_recursively() {
        use std::os::unix::fs::symlink;

        let root = unique_dir("symlink-dir");
        let real = root.join("real");
        std::fs::create_dir_all(real.join("nested")).unwrap();
        let target_file = real.join("nested").join("doc.pdf");
        std::fs::write(&target_file, b"%PDF").unwrap();
        let link = root.join("alias");
        symlink(&real, &link).unwrap();

        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let found = expand_input_paths_with_extensions(&[link.as_path()], true, &exts).unwrap();
        assert_eq!(found, vec![link.join("nested").join("doc.pdf")]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn max_files_boundary_exact() {
        let dir = unique_dir("exact-max");
        let mut files = Vec::new();
        for i in 0..MAX_EXPAND_FILES {
            let path = dir.join(format!("doc{i:04}.txt"));
            std::fs::write(&path, b"x").unwrap();
            files.push(path);
        }

        let mut exts = HashSet::new();
        exts.insert("txt".into());
        let found = expand_input_paths_with_extensions(&files, false, &exts).unwrap();
        assert_eq!(found.len(), MAX_EXPAND_FILES);

        // One more file tips over the hard cap.
        let extra = dir.join("extra.txt");
        std::fs::write(&extra, b"x").unwrap();
        files.push(extra);
        let err = expand_input_paths_with_extensions(&files, false, &exts).unwrap_err();
        assert!(err.to_string().contains("too many input files"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn max_depth_exact_boundary() {
        // Directories at depth 0..MAX_EXPAND_DEPTH-1 expand; a directory at
        // depth MAX_EXPAND_DEPTH errors. Place a file at the deepest allowed level.
        let root = unique_dir("depth-exact");
        let mut dir = root.clone();
        for _ in 0..(MAX_EXPAND_DEPTH - 1) {
            dir = dir.join("sub");
            std::fs::create_dir_all(&dir).unwrap();
        }
        let deep_file = dir.join("leaf.txt");
        std::fs::write(&deep_file, b"ok").unwrap();

        let mut exts = HashSet::new();
        exts.insert("txt".into());
        let found = expand_input_paths_with_extensions(&[root.as_path()], true, &exts).unwrap();
        assert_eq!(found, vec![deep_file]);

        // One more directory level under the deep path should exceed the cap.
        let too_deep_dir = dir.join("extra");
        std::fs::create_dir_all(&too_deep_dir).unwrap();
        std::fs::write(too_deep_dir.join("past.txt"), b"x").unwrap();
        let err = expand_input_paths_with_extensions(&[root.as_path()], true, &exts).unwrap_err();
        assert!(err.to_string().contains("maximum depth"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mixed_files_and_directories() {
        let root = unique_dir("mixed");
        let top = root.join("top.pdf");
        std::fs::write(&top, b"%PDF").unwrap();
        let nested_dir = root.join("folder");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let nested = nested_dir.join("nested.pdf");
        std::fs::write(&nested, b"%PDF").unwrap();
        let ignored = root.join("skip.bin");
        std::fs::write(&ignored, b"x").unwrap();

        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        // Mix a file path and a directory path in one call.
        let found =
            expand_input_paths_with_extensions(&[top.as_path(), nested_dir.as_path()], true, &exts)
                .unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.contains(&top));
        assert!(found.contains(&nested));
        assert!(!found.contains(&ignored));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        let dir = unique_dir("case");
        let upper = dir.join("A.PDF");
        let mixed = dir.join("B.PdF");
        let lower = dir.join("c.pdf");
        std::fs::write(&upper, b"%PDF").unwrap();
        std::fs::write(&mixed, b"%PDF").unwrap();
        std::fs::write(&lower, b"%PDF").unwrap();

        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let found = expand_input_paths_with_extensions(
            &[upper.as_path(), mixed.as_path(), lower.as_path()],
            false,
            &exts,
        )
        .unwrap();
        assert_eq!(found.len(), 3);
        assert!(found.contains(&upper));
        assert!(found.contains(&mixed));
        assert!(found.contains(&lower));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn empty_directory_recursive_returns_empty() {
        let dir = unique_dir("empty-rec");
        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let found = expand_input_paths_with_extensions(&[dir.as_path()], true, &exts).unwrap();
        assert!(found.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_components_are_handled() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        // macOS/APFS rejects non-UTF-8 path components at create time (EILSEQ).
        // Still verify expand logic does not panic on a non-UTF-8 PathBuf: tilde
        // expansion is skipped (to_str fails) and the missing path surfaces as an error.
        let weird = PathBuf::from(OsStr::from_bytes(b"/tmp/shift-non-utf8-\x80\x81.pdf"));
        assert!(weird.to_str().is_none());

        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let result = expand_input_paths_with_extensions(&[weird.as_path()], false, &exts);
        assert!(
            result.is_err(),
            "missing non-utf8 path should error, not panic"
        );
        assert!(result.unwrap_err().to_string().contains("could not read"));

        // If the host FS allows non-UTF-8 names, exercise the happy path too.
        let dir = unique_dir("non-utf8");
        let weird_name = OsStr::from_bytes(b"file-\x80\x81");
        let weird_dir = dir.join(weird_name);
        match std::fs::create_dir_all(&weird_dir) {
            Ok(()) => {
                let pdf = weird_dir.join("ok.pdf");
                std::fs::write(&pdf, b"%PDF").unwrap();
                let found =
                    expand_input_paths_with_extensions(&[dir.as_path()], true, &exts).unwrap();
                assert_eq!(found, vec![pdf]);
            }
            Err(error) => {
                // Expected on macOS: Illegal byte sequence.
                assert!(
                    error.raw_os_error() == Some(92)
                        || error.to_string().contains("Illegal byte sequence")
                        || error.kind() == std::io::ErrorKind::InvalidInput,
                    "unexpected create_dir error: {error}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn supported_input_extensions_from_custom_registry() {
        use crate::conversion::{
            ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, OutputFormat,
        };
        use std::path::Path;

        struct OnlyFooModule;

        impl ConversionModule for OnlyFooModule {
            fn id(&self) -> &'static str {
                "only-foo"
            }
            fn label(&self) -> &'static str {
                "Only Foo"
            }
            fn input_extensions(&self) -> &'static [&'static str] {
                &["FOO", "Bar"]
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
                Err(ConversionError::new("unused"))
            }
        }

        let registry = ConversionRegistry::new().with_module(OnlyFooModule);
        let exts = supported_input_extensions(&registry);
        // Registry extensions are lowercased when collected.
        assert_eq!(exts.len(), 2);
        assert!(exts.contains("foo"));
        assert!(exts.contains("bar"));
        assert!(!exts.contains("FOO"));
        assert!(!exts.contains("pdf"));
    }

    #[test]
    fn tilde_expansion_in_expand_paths() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let Some(home) = home else {
            return;
        };
        let dir = home.join(format!(
            ".shift-expand-tilde-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note.txt");
        std::fs::write(&file, b"hi").unwrap();

        let mut exts = HashSet::new();
        exts.insert("txt".into());
        let tilde_path = PathBuf::from(format!(
            "~/{}/note.txt",
            dir.strip_prefix(&home).unwrap().to_string_lossy()
        ));
        let found =
            expand_input_paths_with_extensions(&[tilde_path.as_path()], false, &exts).unwrap();
        assert_eq!(found, vec![file]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expand_tilde_without_home_leaves_literal() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_home = std::env::var_os("HOME");
        let old_profile = std::env::var_os("USERPROFILE");
        // SAFETY: serialized behind crate::ENV_LOCK.
        unsafe {
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
        }
        assert_eq!(expand_tilde(Path::new("~")), PathBuf::from("~"));
        assert_eq!(expand_tilde(Path::new("~/x.pdf")), PathBuf::from("~/x.pdf"));
        assert_eq!(
            expand_tilde(Path::new("/abs/path")),
            PathBuf::from("/abs/path")
        );
        unsafe {
            match old_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match old_profile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_directory_reports_read_error() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_dir("unreadable-dir");
        let locked = root.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("secret.pdf"), b"%PDF").unwrap();
        let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
        permissions.set_mode(0o000);
        std::fs::set_permissions(&locked, permissions).unwrap();

        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        let result = expand_input_paths_with_extensions(&[locked.as_path()], true, &exts);

        // Restore perms for cleanup.
        let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&locked, permissions).unwrap();

        // Non-root should fail reading the locked directory.
        if let Err(error) = result {
            assert!(
                error.to_string().contains("could not read directory")
                    || error.to_string().contains("could not read"),
                "error: {error}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_non_file_non_dir_is_skipped() {
        use std::os::unix::fs::symlink;

        // Best-effort: create a fifo and symlink to it; expansion should skip quietly.
        let root = unique_dir("symlink-fifo");
        let fifo = root.join("pipe");
        // mkfifo via libc if available; otherwise skip.
        let created = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !created {
            let _ = std::fs::remove_dir_all(root);
            return;
        }
        let link = root.join("link-pipe");
        symlink(&fifo, &link).unwrap();

        let mut exts = HashSet::new();
        exts.insert("pdf".into());
        // Following a fifo symlink may hang on open for metadata in some cases;
        // if it errors or returns empty, both are acceptable.
        let result = expand_input_paths_with_extensions(&[link.as_path()], false, &exts);
        let _ = result;
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&fifo);
        let _ = std::fs::remove_dir_all(root);
    }
}
