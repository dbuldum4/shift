//! Tiny shared preferences needed by both the native app and CLI.

use std::io;
use std::path::PathBuf;

const DEFAULT_MODULES: &[&str] = &["markitdown", "pandoc", "defuddle", "docling", "ffmpeg"];

pub fn default_module_priority() -> Vec<String> {
    DEFAULT_MODULES.iter().map(|id| (*id).to_owned()).collect()
}

pub fn load_module_priority() -> Vec<String> {
    if let Ok(value) = std::env::var("SHIFT_MODULE_PRIORITY") {
        return normalize(value.split(',').map(str::trim));
    }

    preferences_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|value| normalize(value.lines().map(str::trim)))
        .unwrap_or_else(default_module_priority)
}

pub fn save_module_priority(priority: &[String]) -> io::Result<()> {
    let Some(path) = preferences_path() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate the user home directory",
        ));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        normalize(priority.iter().map(String::as_str)).join("\n"),
    )
}

fn normalize<'a>(ids: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut result = Vec::new();
    for id in ids {
        if DEFAULT_MODULES.contains(&id) && !result.iter().any(|existing| existing == id) {
            result.push(id.to_owned());
        }
    }
    for id in DEFAULT_MODULES {
        if !result.iter().any(|existing| existing == id) {
            result.push((*id).to_owned());
        }
    }
    result
}

fn preferences_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support/Shift/module-priority"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_removes_unknowns_and_appends_missing_modules() {
        assert_eq!(
            normalize(["pandoc", "unknown", "pandoc"]),
            vec!["pandoc", "markitdown", "defuddle", "docling", "ffmpeg"]
        );
    }
}
