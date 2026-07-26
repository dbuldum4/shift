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
    if let Some(override_dir) = std::env::var_os("SHIFT_APP_SUPPORT_DIR") {
        return Some(PathBuf::from(override_dir).join("module-priority"));
    }
    if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library/Application Support/Shift/module-priority"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(|xdg| PathBuf::from(xdg).join("shift/module-priority"))
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| PathBuf::from(home).join(".config/shift/module-priority"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    struct EnvGuard {
        home: Option<OsString>,
        app_support_dir: Option<OsString>,
        module_priority: Option<OsString>,
    }

    impl EnvGuard {
        fn new() -> Self {
            Self {
                home: std::env::var_os("HOME"),
                app_support_dir: std::env::var_os("SHIFT_APP_SUPPORT_DIR"),
                module_priority: std::env::var_os("SHIFT_MODULE_PRIORITY"),
            }
        }

        fn apply(&self, home: Option<&std::path::Path>, module_priority: Option<&str>) {
            self.apply_with_app_support(home, None, module_priority);
        }

        fn apply_with_app_support(
            &self,
            home: Option<&std::path::Path>,
            app_support_dir: Option<&std::path::Path>,
            module_priority: Option<&str>,
        ) {
            unsafe {
                if let Some(path) = home {
                    std::env::set_var("HOME", path.as_os_str());
                } else {
                    std::env::remove_var("HOME");
                }

                if let Some(path) = app_support_dir {
                    std::env::set_var("SHIFT_APP_SUPPORT_DIR", path.as_os_str());
                } else {
                    std::env::remove_var("SHIFT_APP_SUPPORT_DIR");
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
                match self.app_support_dir.take() {
                    Some(value) => std::env::set_var("SHIFT_APP_SUPPORT_DIR", value),
                    None => std::env::remove_var("SHIFT_APP_SUPPORT_DIR"),
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let guard = EnvGuard::new();
        guard.apply(None, Some("docling,pandoc,unknown,docling"));

        assert_eq!(
            load_module_priority(),
            vec!["docling", "pandoc", "markitdown", "defuddle", "ffmpeg"]
        );
    }

    #[test]
    fn load_priority_from_file() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp("missing");
        std::fs::create_dir_all(&home).unwrap();

        let guard = EnvGuard::new();
        guard.apply(Some(&home), None);

        assert_eq!(load_module_priority(), default_module_priority());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn save_and_load_priority_round_trips() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    #[test]
    fn app_support_dir_override_is_preferred_for_preferences_path_and_save_load() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env_guard = EnvGuard::new();
        let home = unique_temp("home");
        let override_dir = unique_temp("override");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&override_dir).unwrap();
        env_guard.apply_with_app_support(Some(&home), Some(&override_dir), None);

        assert_eq!(
            preferences_path(),
            Some(override_dir.join("module-priority"))
        );

        save_module_priority(&["docling".to_owned(), "ffmpeg".to_owned()]).unwrap();
        let saved = std::fs::read_to_string(override_dir.join("module-priority")).unwrap();
        assert_eq!(saved, "docling\nffmpeg\nmarkitdown\npandoc\ndefuddle");

        assert_eq!(
            load_module_priority(),
            vec!["docling", "ffmpeg", "markitdown", "pandoc", "defuddle"]
        );

        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&override_dir);
    }

    #[test]
    fn module_priority_env_with_whitespace_and_empty_segments() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env_guard = EnvGuard::new();
        env_guard.apply_with_app_support(None, None, Some("  pandoc , , ffmpeg  "));

        assert_eq!(
            load_module_priority(),
            vec!["pandoc", "ffmpeg", "markitdown", "defuddle", "docling"]
        );
    }

    #[test]
    fn empty_or_unknown_env_module_priority_yields_defaults() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env_guard = EnvGuard::new();
        for value in ["", "unknown,also-unknown"] {
            env_guard.apply_with_app_support(None, None, Some(value));
            assert_eq!(
                load_module_priority(),
                default_module_priority(),
                "value: {value:?}"
            );
        }
    }

    #[test]
    fn env_module_priority_overrides_file() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env_guard = EnvGuard::new();
        let home = unique_temp("env-over-file");
        std::fs::create_dir_all(home.join("Library/Application Support/Shift")).unwrap();
        std::fs::write(
            home.join("Library/Application Support/Shift/module-priority"),
            "pandoc\ndocling\n",
        )
        .unwrap();

        env_guard.apply_with_app_support(Some(&home), None, Some("ffmpeg,markitdown"));

        assert_eq!(
            load_module_priority(),
            vec!["ffmpeg", "markitdown", "pandoc", "defuddle", "docling"]
        );

        let _ = std::fs::remove_dir_all(&home);
    }
}
