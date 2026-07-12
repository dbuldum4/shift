//! Fast, reliable native file picking on macOS.
//!
//! Improvements over GPUI's built-in `prompt_for_paths`:
//! - Prewarms the open/save panel XPC service at launch (first open is near-instant)
//! - Presents as a sheet on the key window when possible
//! - Elevates panel level and activates the app so the dialog can't get buried
//! - Resolves aliases/symlinks
//! - Guards against re-entrant opens (double-click / multi-dialog crashes)
//! - Restores focus to the previous key window after dismissal
//! - Remembers the last directory for faster subsequent navigation

#![allow(unexpected_cfgs)] // objc `msg_send!` cfg noise

use cocoa::appkit::{NSApp, NSApplication, NSModalResponse, NSOpenPanel, NSSavePanel};
use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSString, NSURL};
use futures::channel::oneshot;
use objc::{msg_send, sel, sel_impl};
use std::cell::Cell;
use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static DIALOG_OPEN: AtomicBool = AtomicBool::new(false);
static LAST_DIRECTORY: Mutex<Option<PathBuf>> = Mutex::new(None);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGShieldingWindowLevel() -> i32;
}

/// Pre-create an `NSOpenPanel` so macOS spins up `openAndSavePanelService`
/// during app launch instead of on the first user click.
///
/// Must be called on the main thread after `NSApplication` exists.
pub fn prewarm() {
    unsafe {
        let panel = NSOpenPanel::openPanel(nil);
        // Touch a couple of properties so AppKit fully initializes the panel.
        panel.setCanChooseFiles_(YES);
        panel.setCanChooseDirectories_(NO);
        panel.setResolvesAliases_(YES);
        let _: () = msg_send![panel, setAllowsOtherFileTypes: YES];
        // Force a retain/release cycle so the service stays warm briefly.
        let _: id = msg_send![panel, retain];
        let _: () = msg_send![panel, release];
    }
}

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

/// Present a single-file open dialog.
///
/// Returns a oneshot that resolves to the selected path, or `None` if the user
/// cancelled / the dialog could not be shown.
pub fn pick_file(starting_directory: Option<PathBuf>) -> oneshot::Receiver<Option<PathBuf>> {
    let (tx, rx) = oneshot::channel();

    if DIALOG_OPEN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        let _ = tx.send(None);
        return rx;
    }

    let start_dir = starting_directory
        .filter(|p| p.is_dir())
        .or_else(|| {
            LAST_DIRECTORY
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
                .filter(|p| p.is_dir())
        })
        .or_else(default_start_directory);

    // SAFETY: called from the GPUI main thread (click handler / foreground spawn).
    unsafe {
        present_open_panel(start_dir, tx);
    }

    rx
}

/// Present a save dialog with a suggested output name.
pub fn pick_save_file(
    suggested_name: &str,
    starting_directory: Option<PathBuf>,
) -> oneshot::Receiver<Option<PathBuf>> {
    let (tx, rx) = oneshot::channel();

    if DIALOG_OPEN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        let _ = tx.send(None);
        return rx;
    }

    let start_dir = starting_directory
        .filter(|path| path.is_dir())
        .or_else(|| {
            LAST_DIRECTORY
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
                .filter(|path| path.is_dir())
        })
        .or_else(default_start_directory);

    // SAFETY: called from the GPUI main thread (click handler).
    unsafe {
        present_save_panel(suggested_name, start_dir, tx);
    }

    rx
}

fn default_start_directory() -> Option<PathBuf> {
    home_dir()
        .map(|h| h.join("Documents"))
        .filter(|p| p.is_dir())
        .or_else(home_dir)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

unsafe fn present_open_panel(start_dir: Option<PathBuf>, tx: oneshot::Sender<Option<PathBuf>>) {
    unsafe {
        let panel = NSOpenPanel::openPanel(nil);
        // Retain for the lifetime of the modal; released in the completion handler.
        let panel: id = msg_send![panel, retain];

        panel.setCanChooseFiles_(YES);
        panel.setCanChooseDirectories_(NO);
        panel.setAllowsMultipleSelection_(NO);
        panel.setCanCreateDirectories(NO);
        // Resolve Finder aliases / symlinks so callers always get a real path.
        panel.setResolvesAliases_(YES);

        if let Some(dir) = start_dir.as_ref() {
            set_directory_url(panel, dir);
        }

        let prompt = NSString::alloc(nil).init_str("Choose");
        let _: () = msg_send![panel, setPrompt: prompt];
        let _: () = msg_send![prompt, release];

        // Keep the panel above everything while it's active.
        let level = CGShieldingWindowLevel() as i64;
        let _: () = msg_send![panel, setLevel: level];

        let app = NSApp();
        app.activateIgnoringOtherApps_(YES);

        // Remember the previous key window so we can restore focus after dismiss.
        let previous_key: id = msg_send![app, keyWindow];
        if previous_key != nil {
            let _: id = msg_send![previous_key, retain];
        }

        let parent = sheet_parent(app);

        let done = Cell::new(Some(CompletionState {
            tx,
            panel,
            previous_key,
        }));

        let block = block::ConcreteBlock::new(move |response: NSModalResponse| {
            let Some(state) = done.take() else {
                return;
            };

            let result = if response == NSModalResponse::NSModalResponseOk {
                first_selected_path(state.panel)
            } else {
                None
            };

            if let Some(ref path) = result {
                if let Some(parent) = path.parent() {
                    if let Ok(mut guard) = LAST_DIRECTORY.lock() {
                        *guard = Some(parent.to_path_buf());
                    }
                }
            }

            // Dismiss fully before notifying waiters.
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
}

unsafe fn present_save_panel(
    suggested_name: &str,
    start_dir: Option<PathBuf>,
    tx: oneshot::Sender<Option<PathBuf>>,
) {
    unsafe {
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

        let done = Cell::new(Some(CompletionState {
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
}

struct CompletionState {
    tx: oneshot::Sender<Option<PathBuf>>,
    panel: id,
    previous_key: id,
}

// `id` is a raw pointer; the completion block always runs on the main thread.
unsafe impl Send for CompletionState {}

unsafe fn sheet_parent(app: id) -> id {
    unsafe {
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
}

unsafe fn set_directory_url(panel: id, dir: &Path) {
    unsafe {
        let path_str = dir.to_string_lossy();
        let ns_path = NSString::alloc(nil).init_str(&path_str);
        let url = NSURL::fileURLWithPath_isDirectory_(nil, ns_path, YES);
        panel.setDirectoryURL(url);
        let _: () = msg_send![ns_path, release];
    }
}

fn first_selected_path(panel: id) -> Option<PathBuf> {
    unsafe {
        let urls = panel.URLs();
        if urls == nil {
            return None;
        }
        let count: usize = msg_send![urls, count];
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
                return Some(path);
            }
        }
        None
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
            // Fallback to path string if fileSystemRepresentation fails.
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
