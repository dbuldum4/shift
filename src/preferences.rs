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
    use std::ffi::OsString;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        home: Option<OsString>,
        module_priority: Option<OsString>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                home: std::env::var_os("HOME"),
                module_priority: std::env::var_os("SHIFT_MODULE_PRIORITY"),
            }
        }

        fn apply(&self, home: Option<&std::path::Path>, module_priority: Option<&str>) {
            unsafe {
                if let Some(path) = home {
                    std::env::set_var("HOME", path.as_os_str());
                } else {
                    std::env::remove_var("HOME");
                }

                if let Some(value) = module_priority {
                    std::env::set_var("SHIFT_MODULE_PRIORITY", value);
                } else {
                    std::env::remove_var("SHIFT_MODULE_PRIORITY");
                }
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.home.take() {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match self.module_priority.take() {
                    Some(value) => std::env::set_var("SHIFT_MODULE_PRIORITY", value),
                    None => std::env::remove_var("SHIFT_MODULE_PRIORITY"),
                }
            }
        }
    }

    fn unique_temp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "shift-prefs-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn normalization_removes_unknowns_and_appends_missing_modules() {
        assert_eq!(
            normalize(["pandoc", "unknown", "pandoc"]),
            vec!["pandoc", "markitdown", "defuddle", "docling", "ffmpeg"]
        );
    }

    #[test]
    fn load_priority_from_env_var() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let guard = EnvGuard::new();
        guard.apply(None, Some("docling,pandoc,unknown,docling"));

        assert_eq!(
            load_module_priority(),
            vec!["docling", "pandoc", "markitdown", "defuddle", "ffmpeg"]
        );
    }

    #[test]
    fn load_priority_from_file() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp("file");
        std::fs::create_dir_all(home.join("Library/Application Support/Shift")).unwrap();
        std::fs::write(
            home.join("Library/Application Support/Shift/module-priority"),
            "pandoc\ndocling\n",
        )
        .unwrap();

        let guard = EnvGuard::new();
        guard.apply(Some(&home), None);

        assert_eq!(
            load_module_priority(),
            vec!["pandoc", "docling", "markitdown", "defuddle", "ffmpeg"]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_file_uses_defaults() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp("missing");
        std::fs::create_dir_all(&home).unwrap();

        let guard = EnvGuard::new();
        guard.apply(Some(&home), None);

        assert_eq!(load_module_priority(), default_module_priority());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn save_and_load_priority_round_trips() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp("save");
        std::fs::create_dir_all(&home).unwrap();

        let guard = EnvGuard::new();
        guard.apply(Some(&home), None);

        save_module_priority(&[
            "pandoc".to_owned(),
            "unknown".to_owned(),
            "docling".to_owned(),
            "pandoc".to_owned(),
        ])
        .unwrap();

        let saved =
            std::fs::read_to_string(home.join("Library/Application Support/Shift/module-priority"))
                .unwrap();
        assert_eq!(saved, "pandoc\ndocling\nmarkitdown\ndefuddle\nffmpeg");

        assert_eq!(
            load_module_priority(),
            vec!["pandoc", "docling", "markitdown", "defuddle", "ffmpeg"]
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
