//! Fast, reliable native file picking on macOS.
//!
//! Improvements over GPUI's built-in `prompt_for_paths`:
//! - Prewarms the open/save panel XPC service during app launch instead of on first click.
//! - Presents as a sheet on the key window when possible.
//! - Elevates panel level and activates the app so the dialog can't get buried.
//! - Resolves aliases/symlinks.
//! - Guards against re-entrant opens (double-click / multi-dialog crashes).
//! - Restores focus to the previous key window after dismissal.
//! - Remembers the last directory for faster subsequent navigation.
//!
//! On non-Apple platforms the public functions compile to no-op stubs so the
//! crate (and its GUI tests) can be built and run elsewhere.

#![allow(unexpected_cfgs)] // objc `msg_send!` cfg noise
#![allow(unsafe_op_in_unsafe_fn)]

use futures::channel::oneshot;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static DIALOG_OPEN: AtomicBool = AtomicBool::new(false);
static LAST_DIRECTORY: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Pre-create an `NSOpenPanel` so macOS spins up `openAndSavePanelService`
/// during app launch instead of on the first user click.
///
/// Must be called on the main thread after `NSApplication` exists.
#[cfg(target_os = "macos")]
pub fn prewarm() {
    use cocoa::appkit::NSOpenPanel;
    use cocoa::base::{NO, YES, id, nil};
    use objc::{msg_send, sel, sel_impl};

    unsafe {
        let panel = NSOpenPanel::openPanel(nil);
        panel.setCanChooseFiles_(YES);
        panel.setCanChooseDirectories_(NO);
        panel.setResolvesAliases_(YES);
        let _: () = msg_send![panel, setAllowsOtherFileTypes: YES];
        let _: id = msg_send![panel, retain];
        let _: () = msg_send![panel, release];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn prewarm() {}

/// Returns `true` if a file dialog is already open.
pub fn is_busy() -> bool {
    DIALOG_OPEN.load(Ordering::Acquire)
}

/// Remember a directory for the next browse session (e.g. after a drag-drop).
pub fn remember_directory(path: &Path) {
    let dir = if path.is_dir() {
        Some(path.to_path_buf())
    } else {
        path.parent().map(|p| p.to_path_buf())
    };
    if let Some(dir) = dir.filter(|p| p.is_dir()) {
        if let Ok(mut guard) = LAST_DIRECTORY.lock() {
            *guard = Some(dir);
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn begin_dialog() -> bool {
    DIALOG_OPEN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

#[cfg(any(target_os = "macos", test))]
fn resolve_start_dir(starting_directory: Option<PathBuf>) -> Option<PathBuf> {
    starting_directory
        .filter(|p| p.is_dir())
        .or_else(|| {
            LAST_DIRECTORY
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
                .filter(|p| p.is_dir())
        })
        .or_else(default_start_directory)
}

#[cfg(any(target_os = "macos", test))]
fn default_start_directory() -> Option<PathBuf> {
    home_dir()
        .map(|h| h.join("Documents"))
        .filter(|p| p.is_dir())
        .or_else(home_dir)
}

#[cfg(any(target_os = "macos", test))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(target_os = "macos")]
mod macos {
    pub use super::*;
    use cocoa::appkit::{NSApp, NSApplication, NSModalResponse, NSOpenPanel, NSSavePanel};
    use cocoa::base::{BOOL, NO, YES, id, nil};
    use cocoa::foundation::{NSPoint, NSRect, NSSize, NSString, NSURL};
    use objc::{msg_send, sel, sel_impl};
    use std::cell::Cell;
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;
    use std::process::Command;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGShieldingWindowLevel() -> i32;
    }

    #[derive(Clone, Copy)]
    enum OpenPanelMode {
        SingleFile,
        MultipleFiles,
        Directory,
    }

    pub fn pick_file(starting_directory: Option<PathBuf>) -> oneshot::Receiver<Option<PathBuf>> {
        let (tx, rx) = oneshot::channel();

        if !begin_dialog() {
            let _ = tx.send(None);
            return rx;
        }

        let start_dir = resolve_start_dir(starting_directory);
        unsafe {
            present_open_panel(start_dir, OpenPanelMode::SingleFile, move |paths| {
                let _ = tx.send(paths.into_iter().next());
            });
        }

        rx
    }

    pub fn pick_files(starting_directory: Option<PathBuf>) -> oneshot::Receiver<Vec<PathBuf>> {
        let (tx, rx) = oneshot::channel();

        if !begin_dialog() {
            let _ = tx.send(Vec::new());
            return rx;
        }

        let start_dir = resolve_start_dir(starting_directory);
        unsafe {
            present_open_panel(start_dir, OpenPanelMode::MultipleFiles, move |paths| {
                let _ = tx.send(paths);
            });
        }

        rx
    }

    pub fn pick_directory(
        starting_directory: Option<PathBuf>,
    ) -> oneshot::Receiver<Option<PathBuf>> {
        let (tx, rx) = oneshot::channel();

        if !begin_dialog() {
            let _ = tx.send(None);
            return rx;
        }

        let start_dir = resolve_start_dir(starting_directory);
        unsafe {
            present_open_panel(start_dir, OpenPanelMode::Directory, move |paths| {
                let _ = tx.send(paths.into_iter().next());
            });
        }

        rx
    }

    pub fn pick_save_file(
        suggested_name: &str,
        starting_directory: Option<PathBuf>,
    ) -> oneshot::Receiver<Option<PathBuf>> {
        let (tx, rx) = oneshot::channel();

        if !begin_dialog() {
            let _ = tx.send(None);
            return rx;
        }

        let start_dir = resolve_start_dir(starting_directory);

        unsafe {
            present_save_panel(suggested_name, start_dir, tx);
        }

        rx
    }

    pub fn reveal_in_finder(path: &Path) {
        let _ = Command::new("/usr/bin/open").arg("-R").arg(path).spawn();
    }

    pub fn open_path(path: &Path) {
        let _ = Command::new("/usr/bin/open").arg(path).spawn();
    }

    pub fn begin_file_drag(path: &Path) -> bool {
        if !path.is_file() {
            return false;
        }
        unsafe {
            let app = NSApp();
            if app == nil {
                return false;
            }
            let event: id = msg_send![app, currentEvent];
            if event == nil {
                return false;
            }

            let mut window: id = msg_send![app, keyWindow];
            if window == nil {
                window = msg_send![app, mainWindow];
            }
            if window == nil {
                let windows: id = msg_send![app, windows];
                if windows != nil {
                    let count: usize = msg_send![windows, count];
                    if count > 0 {
                        window = msg_send![windows, objectAtIndex: 0usize];
                    }
                }
            }
            if window == nil {
                return false;
            }

            let view: id = msg_send![window, contentView];
            if view == nil {
                return false;
            }

            let path_str = path.to_string_lossy();
            let ns_path = NSString::alloc(nil).init_str(&path_str);
            if ns_path == nil {
                return false;
            }

            let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
            let ok: BOOL = msg_send![
                view,
                dragFile: ns_path
                fromRect: rect
                slideBack: YES
                event: event
            ];
            let _: () = msg_send![ns_path, release];
            ok == YES
        }
    }

    unsafe fn present_open_panel(
        start_dir: Option<PathBuf>,
        mode: OpenPanelMode,
        on_complete: impl FnOnce(Vec<PathBuf>) + Send + 'static,
    ) {
        let panel = NSOpenPanel::openPanel(nil);
        let panel: id = msg_send![panel, retain];

        match mode {
            OpenPanelMode::SingleFile => {
                panel.setCanChooseFiles_(YES);
                panel.setCanChooseDirectories_(NO);
                panel.setAllowsMultipleSelection_(NO);
                panel.setCanCreateDirectories(NO);
            }
            OpenPanelMode::MultipleFiles => {
                panel.setCanChooseFiles_(YES);
                panel.setCanChooseDirectories_(NO);
                panel.setAllowsMultipleSelection_(YES);
                panel.setCanCreateDirectories(NO);
            }
            OpenPanelMode::Directory => {
                panel.setCanChooseFiles_(NO);
                panel.setCanChooseDirectories_(YES);
                panel.setAllowsMultipleSelection_(NO);
                panel.setCanCreateDirectories(YES);
            }
        }
        panel.setResolvesAliases_(YES);

        if let Some(dir) = start_dir.as_ref() {
            set_directory_url(panel, dir);
        }

        let prompt = match mode {
            OpenPanelMode::Directory => "Choose Folder",
            OpenPanelMode::MultipleFiles => "Add Files",
            OpenPanelMode::SingleFile => "Choose",
        };
        let prompt = NSString::alloc(nil).init_str(prompt);
        let _: () = msg_send![panel, setPrompt: prompt];
        let _: () = msg_send![prompt, release];

        let level = CGShieldingWindowLevel() as i64;
        let _: () = msg_send![panel, setLevel: level];

        let app = NSApp();
        app.activateIgnoringOtherApps_(YES);

        let previous_key: id = msg_send![app, keyWindow];
        if previous_key != nil {
            let _: id = msg_send![previous_key, retain];
        }

        let parent = sheet_parent(app);

        let done = Cell::new(Some(OpenCompletionState {
            on_complete: Some(Box::new(on_complete)),
            panel,
            previous_key,
        }));

        let block = block::ConcreteBlock::new(move |response: NSModalResponse| {
            let Some(mut state) = done.take() else {
                return;
            };

            let result = if response == NSModalResponse::NSModalResponseOk {
                selected_paths(state.panel)
            } else {
                Vec::new()
            };

            if let Some(path) = result.first() {
                if path.is_dir() {
                    if let Ok(mut guard) = LAST_DIRECTORY.lock() {
                        *guard = Some(path.clone());
                    }
                } else if let Some(parent) = path.parent() {
                    if let Ok(mut guard) = LAST_DIRECTORY.lock() {
                        *guard = Some(parent.to_path_buf());
                    }
                }
            }

            let _: () = msg_send![state.panel, orderOut: nil];
            let _: () = msg_send![state.panel, release];

            if state.previous_key != nil {
                let _: () = msg_send![state.previous_key, makeKeyAndOrderFront: nil];
                let _: () = msg_send![state.previous_key, release];
            }

            DIALOG_OPEN.store(false, Ordering::Release);
            if let Some(on_complete) = state.on_complete.take() {
                on_complete(result);
            }
        });
        let block = block.copy();

        if parent != nil {
            let _: () = msg_send![
                panel,
                beginSheetModalForWindow: parent
                completionHandler: block
            ];
        } else {
            let _: () = msg_send![panel, beginWithCompletionHandler: block];
        }
    }

    unsafe fn present_save_panel(
        suggested_name: &str,
        start_dir: Option<PathBuf>,
        tx: oneshot::Sender<Option<PathBuf>>,
    ) {
        let panel = NSSavePanel::savePanel(nil);
        let panel: id = msg_send![panel, retain];
        panel.setCanCreateDirectories(YES);

        if let Some(dir) = start_dir.as_ref() {
            set_directory_url(panel, dir);
        }

        let name = NSString::alloc(nil).init_str(suggested_name);
        let _: () = msg_send![panel, setNameFieldStringValue: name];
        let _: () = msg_send![name, release];

        let prompt = NSString::alloc(nil).init_str("Save");
        let _: () = msg_send![panel, setPrompt: prompt];
        let _: () = msg_send![prompt, release];

        let level = CGShieldingWindowLevel() as i64;
        let _: () = msg_send![panel, setLevel: level];

        let app = NSApp();
        app.activateIgnoringOtherApps_(YES);
        let previous_key: id = msg_send![app, keyWindow];
        if previous_key != nil {
            let _: id = msg_send![previous_key, retain];
        }
        let parent = sheet_parent(app);

        let done = Cell::new(Some(SaveCompletionState {
            tx,
            panel,
            previous_key,
        }));
        let block = block::ConcreteBlock::new(move |response: NSModalResponse| {
            let Some(state) = done.take() else {
                return;
            };

            let result = if response == NSModalResponse::NSModalResponseOk {
                selected_save_path(state.panel)
            } else {
                None
            };

            if let Some(parent) = result.as_ref().and_then(|path| path.parent()) {
                if let Ok(mut guard) = LAST_DIRECTORY.lock() {
                    *guard = Some(parent.to_path_buf());
                }
            }

            let _: () = msg_send![state.panel, orderOut: nil];
            let _: () = msg_send![state.panel, release];
            if state.previous_key != nil {
                let _: () = msg_send![state.previous_key, makeKeyAndOrderFront: nil];
                let _: () = msg_send![state.previous_key, release];
            }

            DIALOG_OPEN.store(false, Ordering::Release);
            let _ = state.tx.send(result);
        });
        let block = block.copy();

        if parent != nil {
            let _: () = msg_send![
                panel,
                beginSheetModalForWindow: parent
                completionHandler: block
            ];
        } else {
            let _: () = msg_send![panel, beginWithCompletionHandler: block];
        }
    }

    struct OpenCompletionState {
        on_complete: Option<Box<dyn FnOnce(Vec<PathBuf>) + Send>>,
        panel: id,
        previous_key: id,
    }

    struct SaveCompletionState {
        tx: oneshot::Sender<Option<PathBuf>>,
        panel: id,
        previous_key: id,
    }

    // `id` is a raw pointer; the completion block always runs on the main thread.
    unsafe impl Send for OpenCompletionState {}
    unsafe impl Send for SaveCompletionState {}

    unsafe fn sheet_parent(app: id) -> id {
        let key: id = msg_send![app, keyWindow];
        if key != nil {
            return key;
        }
        let main: id = msg_send![app, mainWindow];
        if main != nil {
            return main;
        }
        let windows: id = msg_send![app, windows];
        if windows != nil {
            let count: usize = msg_send![windows, count];
            if count > 0 {
                return msg_send![windows, objectAtIndex: 0usize];
            }
        }
        nil
    }

    unsafe fn set_directory_url(panel: id, dir: &Path) {
        let path_str = dir.to_string_lossy();
        let ns_path = NSString::alloc(nil).init_str(&path_str);
        let url = NSURL::fileURLWithPath_isDirectory_(nil, ns_path, YES);
        panel.setDirectoryURL(url);
        let _: () = msg_send![ns_path, release];
    }

    fn selected_paths(panel: id) -> Vec<PathBuf> {
        unsafe {
            let urls = panel.URLs();
            if urls == nil {
                return Vec::new();
            }
            let count: usize = msg_send![urls, count];
            let mut paths = Vec::with_capacity(count);
            for i in 0..count {
                let url: id = msg_send![urls, objectAtIndex: i];
                if url == nil {
                    continue;
                }
                let is_file: bool = msg_send![url, isFileURL];
                if !is_file {
                    continue;
                }
                if let Some(path) = ns_url_to_path(url) {
                    paths.push(path);
                }
            }
            paths
        }
    }

    fn selected_save_path(panel: id) -> Option<PathBuf> {
        unsafe {
            let url: id = msg_send![panel, URL];
            if url == nil {
                return None;
            }
            ns_url_to_path(url)
        }
    }

    fn ns_url_to_path(url: id) -> Option<PathBuf> {
        unsafe {
            let path: *const i8 = msg_send![url, fileSystemRepresentation];
            if path.is_null() {
                let ns_path: id = msg_send![url, path];
                if ns_path == nil {
                    return None;
                }
                let utf8: *const i8 = msg_send![ns_path, UTF8String];
                if utf8.is_null() {
                    return None;
                }
                let cstr = CStr::from_ptr(utf8);
                return Some(PathBuf::from(OsStr::from_bytes(cstr.to_bytes())));
            }
            let cstr = CStr::from_ptr(path);
            Some(PathBuf::from(OsStr::from_bytes(cstr.to_bytes())))
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(not(target_os = "macos"))]
mod stub {
    pub use super::*;

    pub fn pick_file(_starting_directory: Option<PathBuf>) -> oneshot::Receiver<Option<PathBuf>> {
        let (tx, rx) = oneshot::channel();
        DIALOG_OPEN.store(false, Ordering::Release);
        let _ = tx.send(None);
        rx
    }

    pub fn pick_files(_starting_directory: Option<PathBuf>) -> oneshot::Receiver<Vec<PathBuf>> {
        let (tx, rx) = oneshot::channel();
        DIALOG_OPEN.store(false, Ordering::Release);
        let _ = tx.send(Vec::new());
        rx
    }

    pub fn pick_directory(
        _starting_directory: Option<PathBuf>,
    ) -> oneshot::Receiver<Option<PathBuf>> {
        let (tx, rx) = oneshot::channel();
        DIALOG_OPEN.store(false, Ordering::Release);
        let _ = tx.send(None);
        rx
    }

    pub fn pick_save_file(
        _suggested_name: &str,
        _starting_directory: Option<PathBuf>,
    ) -> oneshot::Receiver<Option<PathBuf>> {
        let (tx, rx) = oneshot::channel();
        DIALOG_OPEN.store(false, Ordering::Release);
        let _ = tx.send(None);
        rx
    }

    pub fn reveal_in_finder(_path: &Path) {}
    pub fn open_path(_path: &Path) {}

    pub fn begin_file_drag(_path: &Path) -> bool {
        false
    }
}

#[cfg(not(target_os = "macos"))]
pub use stub::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    static LOCK: Mutex<()> = Mutex::new(());

    struct HomeGuard {
        previous: Option<OsString>,
    }

    impl HomeGuard {
        fn set(home: &std::path::Path) -> Self {
            let previous = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("HOME", home.as_os_str());
            }
            Self { previous }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe {
                    std::env::set_var("HOME", value);
                },
                None => unsafe {
                    std::env::remove_var("HOME");
                },
            }
        }
    }

    fn reset() {
        *LAST_DIRECTORY.lock().unwrap_or_else(|e| e.into_inner()) = None;
        DIALOG_OPEN.store(false, Ordering::SeqCst);
    }

    fn unique_temp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shift-picker-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    #[test]
    fn default_start_directory_prefers_documents() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp("documents");
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        let _home_guard = HomeGuard::set(&home);
        reset();

        assert_eq!(default_start_directory(), Some(home.join("Documents")));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn default_start_directory_falls_back_to_home() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp("home");
        std::fs::create_dir_all(&home).unwrap();
        let _home_guard = HomeGuard::set(&home);
        reset();

        assert_eq!(default_start_directory(), Some(home.clone()));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_start_dir_prefers_provided_directory() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp("provided");
        let provided = home.join("provided");
        std::fs::create_dir_all(&provided).unwrap();
        let _home_guard = HomeGuard::set(&home);
        reset();

        assert_eq!(resolve_start_dir(Some(provided.clone())), Some(provided));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_start_dir_falls_back_to_last_directory() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp("last");
        let last = home.join("last");
        std::fs::create_dir_all(&last).unwrap();
        let _home_guard = HomeGuard::set(&home);
        reset();

        *LAST_DIRECTORY.lock().unwrap_or_else(|e| e.into_inner()) = Some(last.clone());
        assert_eq!(resolve_start_dir(None), Some(last));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_start_dir_falls_back_to_default() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp("default");
        std::fs::create_dir_all(home.join("Documents")).unwrap();
        let _home_guard = HomeGuard::set(&home);
        reset();

        assert_eq!(resolve_start_dir(None), Some(home.join("Documents")));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn remember_directory_stores_parent_for_files_and_directory_itself() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = unique_temp("remember");
        let parent = home.join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let file = parent.join("file.txt");
        std::fs::write(&file, b"").unwrap();
        reset();

        remember_directory(&file);
        assert_eq!(
            *LAST_DIRECTORY.lock().unwrap_or_else(|e| e.into_inner()),
            Some(parent.clone())
        );

        remember_directory(&parent);
        assert_eq!(
            *LAST_DIRECTORY.lock().unwrap_or_else(|e| e.into_inner()),
            Some(parent)
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn begin_dialog_and_is_busy_guard_reentry() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();

        assert!(!is_busy());
        assert!(begin_dialog());
        assert!(is_busy());
        assert!(!begin_dialog());

        DIALOG_OPEN.store(false, Ordering::SeqCst);
        assert!(!is_busy());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn stub_pickers_return_empty_selections() {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();

        // `try_recv` on a oneshot receiver returns `Result<Option<T>, Canceled>`.
        assert_eq!(pick_file(None).try_recv(), Ok(Some(None)));
        assert_eq!(pick_files(None).try_recv(), Ok(Some(Vec::new())));
        assert_eq!(pick_directory(None).try_recv(), Ok(Some(None)));
        assert_eq!(pick_save_file("out.md", None).try_recv(), Ok(Some(None)));
        assert!(!begin_file_drag(Path::new("/tmp")));
        assert!(!is_busy());

        // These are no-ops on non-Apple platforms; just ensure they don't panic.
        reveal_in_finder(Path::new("/tmp"));
        open_path(Path::new("/tmp"));
        prewarm();
    }
}
