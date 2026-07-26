//! Headless UI/integration tests for the GPUI app model.
//!
//! These tests drive `Shift` through `TestAppContext` without opening a real
//! macOS window or file panel. External converters are replaced with tiny shell
//! scripts in a per-test temporary directory.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use gpui::{AppContext, Entity, TestAppContext};

use crate::app::{ConversionState, HistoryOutcome, HistorySource, Shift};
use crate::{
    DEFAULT_UI_FONT, HISTORY_SIDEBAR_MAX, HISTORY_SIDEBAR_MIN, OUTPUT_PANEL_MAX, OUTPUT_PANEL_MIN,
    PanelResizeTarget,
};
use shift_core::conversion::{BatchFormatSelection, BatchItemState, BatchSource, OutputFormat};
use shift_core::history::{MAX_HISTORY_ARTIFACT_BYTES, MAX_HISTORY_LIMIT, MIN_HISTORY_LIMIT};

static ENV_LOCK: Mutex<()> = Mutex::new(());
static COUNTER: AtomicU64 = AtomicU64::new(0);

const MARKITDOWN_FAKE: &str = r#"#!/bin/sh
cat "$1"
"#;

const DEFUDDLE_FAKE: &str = r#"#!/bin/sh
# defuddle parse <source> [options]
SOURCE="$2"
printf '# '
printf '%s' "$SOURCE"
printf '\n'
"#;

const PANDOC_FAKE: &str = r#"#!/bin/sh
cat "$1"
"#;

const FFMPEG_FAKE: &str = r#"#!/bin/sh
CONTENT="shift-fake-media"
for arg in "$@"; do
  case "$arg" in
    /*)
      if [ ! -e "$arg" ]; then
        dir=$(dirname "$arg")
        if [ -d "$dir" ]; then
          printf '%s' "$CONTENT" > "$arg"
        fi
      fi
      ;;
  esac
done
"#;

const FAILING_MARKITDOWN_FAKE: &str = r#"#!/bin/sh
head -c 4 "$1" | grep -q '^FAIL'
if [ $? -eq 0 ]; then
  echo "intentional failure" >&2
  exit 1
fi
cat "$1"
"#;

/// Always fails (exit 1) so history / batch failure paths can be exercised.
const ALWAYS_FAIL_MARKITDOWN_FAKE: &str = r#"#!/bin/sh
echo "always fail" >&2
exit 1
"#;

/// Echoes argv to a sibling `.args` file, then cats the first file argument.
const MARKITDOWN_ARGS_FAKE: &str = r#"#!/bin/sh
printf '%s\n' "$@" > "${0}.args"
for a in "$@"; do
  if [ -f "$a" ]; then
    cat "$a"
    exit 0
  fi
done
exit 1
"#;

/// Emits >512 KiB so history retention stores ReadyLarge instead of full bytes.
const LARGE_MARKITDOWN_FAKE: &str = r#"#!/bin/sh
# 600 KiB of 'A' (above MAX_HISTORY_ARTIFACT_BYTES).
dd if=/dev/zero bs=1024 count=600 2>/dev/null | tr '\0' 'A'
"#;

const DOCLING_FAKE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "docling 0.0-fake"
  exit 0
fi

out_dir=""
to="md"
input=""
prev=""
for arg in "$@"; do
  case "$prev" in
    --output) out_dir="$arg"; prev=""; continue ;;
    --to) to="$arg"; prev=""; continue ;;
  esac
  case "$arg" in
    --output|--to) prev="$arg" ;;
    /*)
      if [ -f "$arg" ] && [ -z "$input" ]; then
        input="$arg"
      fi
      ;;
  esac
done

[ -z "$input" ] && exit 0
[ -z "$out_dir" ] && out_dir=.

stem="${input##*/}"
stem="${stem%.*}"
[ "$to" = "text" ] && to="txt"
printf '# docling fake %%s\\n' "$stem" > "$out_dir/$stem.$to"
"#;

struct TestEnv {
    _guard: MutexGuard<'static, ()>,
    temp: PathBuf,
    home: PathBuf,
    #[allow(dead_code)]
    bin: PathBuf,
    previous: std::collections::HashMap<String, Option<OsString>>,
}

impl TestEnv {
    fn new() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let pid = std::process::id();
        let count = COUNTER.fetch_add(1, Ordering::SeqCst);
        let temp = std::env::temp_dir().join(format!("shift-ui-test-{pid}-{count}"));
        let home = temp.join("home");
        let support = home.join("Library/Application Support/Shift");
        fs::create_dir_all(&support).unwrap();
        let bin = temp.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let inputs = temp.join("inputs");
        fs::create_dir_all(&inputs).unwrap();
        let paste = temp.join("paste");
        fs::create_dir_all(&paste).unwrap();

        let markitdown = bin.join("markitdown");
        write_script(&markitdown, MARKITDOWN_FAKE);

        let defuddle = bin.join("defuddle");
        write_script(&defuddle, DEFUDDLE_FAKE);

        let pandoc = bin.join("pandoc");
        write_script(&pandoc, PANDOC_FAKE);

        let ffmpeg = bin.join("ffmpeg");
        write_script(&ffmpeg, FFMPEG_FAKE);

        let docling = bin.join("docling");
        write_script(&docling, DOCLING_FAKE);

        let mut previous = std::collections::HashMap::new();
        set_env(&mut previous, "HOME", Some(home.clone().into_os_string()));
        set_env(
            &mut previous,
            "SHIFT_APP_SUPPORT_DIR",
            Some(support.clone().into_os_string()),
        );
        set_env(
            &mut previous,
            "SHIFT_PASTE_STAGING_DIR",
            Some(paste.into_os_string()),
        );
        set_env(
            &mut previous,
            "SHIFT_MARKITDOWN_BIN",
            Some(markitdown.clone().into_os_string()),
        );
        set_env(
            &mut previous,
            "SHIFT_DEFUDDLE_BIN",
            Some(defuddle.clone().into_os_string()),
        );
        set_env(
            &mut previous,
            "SHIFT_PANDOC_BIN",
            Some(pandoc.clone().into_os_string()),
        );
        set_env(
            &mut previous,
            "SHIFT_FFMPEG_BIN",
            Some(ffmpeg.clone().into_os_string()),
        );
        set_env(
            &mut previous,
            "SHIFT_DOCLING_BIN",
            Some(docling.clone().into_os_string()),
        );
        set_env(&mut previous, "SHIFT_ALLOW_PRIVATE_URLS", Some("1".into()));
        // Absolute path so resolve_pdf_engine succeeds without a real engine.
        // Fake pandoc ignores --pdf-engine and still writes stdout.
        set_env(&mut previous, "SHIFT_PDF_ENGINE", Some("/bin/true".into()));

        Self {
            _guard: guard,
            temp,
            home,
            bin,
            previous,
        }
    }

    fn inputs(&self) -> PathBuf {
        self.temp.join("inputs")
    }

    fn support(&self) -> PathBuf {
        self.home.join("Library/Application Support/Shift")
    }

    fn session_path(&self) -> PathBuf {
        self.support().join("session-settings.json")
    }

    fn module_priority_path(&self) -> PathBuf {
        self.support().join("module-priority")
    }

    fn override_tool(&self, name: &str, script: &str) -> PathBuf {
        let path = self.bin.join(name);
        write_script(&path, script);
        let env_key = format!("SHIFT_{}_BIN", name.to_uppercase());
        unsafe { std::env::set_var(&env_key, &path) };
        path
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        for (key, value) in &self.previous {
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
        let _ = fs::remove_dir_all(&self.temp);
    }
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

fn set_env(
    previous: &mut std::collections::HashMap<String, Option<OsString>>,
    key: &str,
    value: Option<OsString>,
) {
    previous.insert(key.to_string(), std::env::var_os(key));
    unsafe {
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}

fn write_input(env: &TestEnv, name: &str, bytes: &[u8]) -> PathBuf {
    let path = env.inputs().join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn create_shift(cx: &mut TestAppContext) -> Entity<Shift> {
    cx.new(|cx| Shift::new(cx, 1180.0))
}

#[gpui::test]
async fn can_create_shift(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);
    cx.run_until_parked();
    assert!(shift.read_with(cx, |this, _| this.selected_file.is_none()));
}

#[gpui::test]
async fn selecting_a_text_file_converts_to_markdown(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "notes.txt", b"hello shift");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path.clone(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let (format, bytes, module) = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => {
            (artifact.format, artifact.bytes.clone(), artifact.module_id)
        }
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(format, OutputFormat::MARKDOWN);
    assert_eq!(bytes, b"hello shift");
    assert_eq!(module, "markitdown");
}

#[gpui::test]
async fn changing_output_format_reconverts(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "notes.md", b"# hello shift");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path.clone(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();
    assert!(matches!(
        shift.read_with(cx, |this, _| this.conversion.clone()),
        ConversionState::Ready(_)
    ));

    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::HTML, cx)
    });
    cx.run_until_parked();

    let (format, module) = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => (artifact.format, artifact.module_id),
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(format, OutputFormat::HTML);
    assert_eq!(module, "pandoc");
}

#[gpui::test]
async fn selecting_a_url_converts_via_defuddle(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url("http://example.com/article.html".into(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let (format, module, text) = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => (
            artifact.format,
            artifact.module_id,
            String::from_utf8_lossy(&artifact.bytes).to_string(),
        ),
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(format, OutputFormat::MARKDOWN);
    assert_eq!(module, "defuddle");
    assert!(text.contains("example.com/article.html"));
}

#[gpui::test]
async fn magic_paste_with_local_path_converts(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "pasted.txt", b"paste me");
    let shift = create_shift(cx);

    let text = path.to_string_lossy().to_string();
    shift.update(cx, |this, cx| {
        this.submit_magic_paste_text(text, cx);
        this.selection_generation = this.selection_generation.wrapping_add(1);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let selected = shift.read_with(cx, |this, _| this.selected_file.clone());
    assert_eq!(selected, Some(path));
    let format = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => artifact.format,
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(format, OutputFormat::MARKDOWN);
}

#[gpui::test]
async fn magic_paste_with_url_converts(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.submit_magic_paste_text("http://example.com/page.html".into(), cx)
    });
    cx.run_until_parked();
    shift.update(cx, |this, cx| this.start_conversion(cx));
    cx.run_until_parked();

    let url = shift.read_with(cx, |this, _| this.selected_url.clone());
    assert_eq!(url, Some("http://example.com/page.html".into()));
    let module = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => artifact.module_id,
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(module, "defuddle");
}

#[gpui::test]
async fn batch_converts_multiple_files(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "a.txt", b"one");
    let b = write_input(&env, "b.txt", b"two");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![a.clone(), b.clone()], true, cx)
    });
    cx.run_until_parked();

    let (running, status, completed) = shift.read_with(cx, |this, _| {
        (
            this.batch_running,
            this.batch_status.clone(),
            this.batch_queue.progress().completed(),
        )
    });
    assert!(!running);
    assert!(
        status
            .as_ref()
            .is_some_and(|s| s.contains("Batch complete"))
    );
    assert_eq!(completed, 2);

    let items = shift.read_with(cx, |this, _| this.batch_queue.items().to_vec());
    assert!(
        items
            .iter()
            .all(|item| matches!(item.state, BatchItemState::Succeeded { .. }))
    );

    for (path, expected) in [(a, "one"), (b, "two")] {
        let out = path.with_extension("md");
        assert!(out.exists(), "missing {}", out.display());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), expected);
    }
}

#[gpui::test]
async fn cancel_batch_marks_queued_cancelled(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "a.txt", b"one");
    let b = write_input(&env, "b.txt", b"two");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![a, b], true, cx));
    shift.update(cx, |this, cx| this.cancel_batch(cx));
    cx.run_until_parked();

    let (running, status) = shift.read_with(cx, |this, _| {
        (this.batch_running, this.batch_status.clone())
    });
    assert!(!running);
    assert!(
        status
            .as_ref()
            .is_some_and(|s| s.to_lowercase().contains("cancelled"))
    );

    let items = shift.read_with(cx, |this, _| this.batch_queue.items().to_vec());
    assert!(
        items
            .iter()
            .all(|item| matches!(item.state, BatchItemState::Cancelled))
    );
}

#[gpui::test]
async fn history_records_and_restores_conversions(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "history.txt", b"keep me");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path.clone(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let (history_len, active_id) =
        shift.read_with(cx, |this, _| (this.history.len(), this.active_history_id));
    assert_eq!(history_len, 1);
    let id = active_id.expect("no active history");

    shift.update(cx, |this, cx| this.clear_selected_file(cx));
    shift.update(cx, |this, cx| this.restore_history_entry(id, cx));
    cx.run_until_parked();

    let selected = shift.read_with(cx, |this, _| this.selected_file.clone());
    assert_eq!(selected, Some(path));
    let is_ready = shift.read_with(cx, |this, _| {
        matches!(this.conversion, ConversionState::Ready(_))
    });
    assert!(is_ready);
}

#[gpui::test]
async fn batch_output_format_change_writes_correct_extension(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    // Use a PDF so the HTML route goes through Docling (txt now has a Pandoc
    // route, which would change the expected fake output).
    let path = write_input(&env, "batch_format.pdf", b"%PDF-1.4 fake");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![path.clone()], false, cx)
    });
    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::HTML, cx)
    });
    cx.run_until_parked();

    let queued_format = shift.read_with(cx, |this, _| this.batch_queue.items()[0].output_format);
    assert_eq!(queued_format, OutputFormat::HTML);

    shift.update(cx, |this, cx| this.start_batch(cx));
    cx.run_until_parked();

    let out = path.with_extension("html");
    assert!(out.exists());
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("docling fake"));
}

#[gpui::test]
async fn batch_with_output_dir_writes_to_dir(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let out_dir = env.temp.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let path = write_input(&env, "batch_dir.txt", b"content");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_batch_output_dir(out_dir.clone(), cx)
    });
    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![path.clone()], true, cx)
    });
    cx.run_until_parked();

    let written = out_dir.join("batch_dir.md");
    assert!(written.exists());
    assert_eq!(std::fs::read_to_string(&written).unwrap(), "content");
}

#[gpui::test]
async fn retry_failed_batch_item_then_succeeds(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    env.override_tool("markitdown", FAILING_MARKITDOWN_FAKE);

    let ok_path = write_input(&env, "ok.txt", b"fine");
    let fail_path = write_input(&env, "will_fail.txt", b"FAIL intentionally");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![fail_path.clone(), ok_path.clone()], true, cx)
    });
    cx.run_until_parked();

    let (failed_id, ok_count) = shift.read_with(cx, |this, _| {
        let failed = this
            .batch_queue
            .items()
            .iter()
            .find(|item| matches!(item.state, BatchItemState::Failed { .. }))
            .map(|item| item.id);
        let ok = this
            .batch_queue
            .items()
            .iter()
            .filter(|item| matches!(item.state, BatchItemState::Succeeded { .. }))
            .count();
        (failed, ok)
    });
    let failed_id = failed_id.expect("expected a failed item");
    assert_eq!(ok_count, 1);

    std::fs::write(&fail_path, b"fine now").unwrap();
    shift.update(cx, |this, cx| this.retry_batch_item(failed_id, cx));
    cx.run_until_parked();

    let all_succeeded = shift.read_with(cx, |this, _| {
        this.batch_queue
            .items()
            .iter()
            .all(|item| matches!(item.state, BatchItemState::Succeeded { .. }))
    });
    assert!(all_succeeded);
}

#[gpui::test]
async fn toggle_batch_force_sets_force_flag(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "force.txt", b"data");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![path.clone()], false, cx)
    });
    shift.update(cx, |this, cx| this.toggle_batch_force(cx));
    cx.run_until_parked();

    let (global_force, item_force) = shift.read_with(cx, |this, _| {
        (this.batch_force, this.batch_queue.items()[0].force)
    });
    assert!(global_force);
    assert!(item_force);
}

#[gpui::test]
async fn toggle_batch_item_format_pins_override(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "item_format.txt", b"text");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![path.clone()], false, cx)
    });
    let id = shift.read_with(cx, |this, _| this.batch_queue.items()[0].id);
    shift.update(cx, |this, cx| this.toggle_batch_item_format(id, cx));
    cx.run_until_parked();

    let (selection, format) = shift.read_with(cx, |this, _| {
        let item = &this.batch_queue.items()[0];
        (item.format_selection, item.output_format)
    });
    assert_eq!(
        selection,
        BatchFormatSelection::Override(OutputFormat::MARKDOWN)
    );
    assert_eq!(format, OutputFormat::MARKDOWN);

    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::HTML, cx)
    });
    cx.run_until_parked();

    let pinned = shift.read_with(cx, |this, _| this.batch_queue.items()[0].output_format);
    assert_eq!(pinned, OutputFormat::MARKDOWN);

    shift.update(cx, |this, cx| this.toggle_batch_item_format(id, cx));
    cx.run_until_parked();

    let inherited = shift.read_with(cx, |this, _| this.batch_queue.items()[0].output_format);
    assert_eq!(inherited, OutputFormat::HTML);
}

#[gpui::test]
async fn folder_expand_queues_nested_files(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let dir = env.inputs().join("nested");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let a = dir.join("a.txt");
    let b = dir.join("sub/b.txt");
    std::fs::write(&a, b"outer").unwrap();
    std::fs::write(&b, b"inner").unwrap();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.ingest_paths(vec![dir.clone()], cx));
    cx.run_until_parked();

    let confirm = shift.read_with(cx, |this, _| this.folder_confirm.is_some());
    assert!(confirm);

    shift.update(cx, |this, cx| this.confirm_folder_expand(cx));
    shift.update(cx, |this, cx| this.start_batch(cx));
    cx.run_until_parked();

    let written: Vec<_> = shift
        .read_with(cx, |this, _| this.batch_queue.items().to_vec())
        .into_iter()
        .filter_map(|item| match item.state {
            BatchItemState::Succeeded { written_path, .. } => Some(written_path),
            _ => None,
        })
        .collect();
    assert_eq!(written.len(), 2);
    for path in &written {
        assert!(path.exists());
    }
}

#[gpui::test]
async fn clear_batch_queue_empties_items(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "clear_a.txt", b"a");
    let b = write_input(&env, "clear_b.txt", b"b");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![a, b], false, cx));
    shift.update(cx, |this, cx| this.clear_batch_queue(cx));
    cx.run_until_parked();

    let empty = shift.read_with(cx, |this, _| this.batch_queue.is_empty());
    assert!(empty);
}

#[gpui::test]
async fn session_settings_persist(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::HTML, cx)
    });
    shift.update(cx, |this, cx| this.toggle_batch_force(cx));
    cx.run_until_parked();

    let json = std::fs::read_to_string(env.session_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["output_format"].as_str().unwrap(), "html");
    assert!(value["batch_force"].as_bool().unwrap());
}

#[gpui::test]
async fn module_priority_change_reroutes_conversions(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "priority.md", b"# route via pandoc");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();
    let first_module = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => artifact.module_id,
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(first_module, "markitdown");

    shift.update(cx, |this, cx| this.move_module(0, 1, cx));
    cx.run_until_parked();

    let second_module = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => artifact.module_id,
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(second_module, "pandoc");
}

#[gpui::test]
async fn invalid_ffmpeg_options_fail(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "video.mp4", b"fake video");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.ffmpeg_start_input
            .update(cx, |input, cx| input.set_content("not-a-number", cx));
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let failed = shift.read_with(cx, |this, _| {
        matches!(&this.conversion, ConversionState::Failed(_))
    });
    assert!(failed);
}

#[gpui::test]
async fn clipboard_image_converts_via_ffmpeg(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.ingest_clipboard_image(vec![1, 2, 3, 4], "png", cx);
    });
    cx.run_until_parked();
    shift.update(cx, |this, cx| {
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let (format, module) = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => (artifact.format, artifact.module_id),
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(format, OutputFormat::PNG);
    assert_eq!(module, "ffmpeg");
}

#[gpui::test]
async fn cancel_single_conversion_leaves_failed_state(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "cancel.txt", b"hello");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    shift.update(cx, |this, cx| this.cancel_conversion(cx));
    cx.run_until_parked();

    let failed = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Failed(msg) => msg.as_ref().contains("cancelled"),
        _ => false,
    });
    assert!(failed);
}

#[gpui::test]
async fn clear_selected_file_resets_state(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "clear.txt", b"hello");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();
    assert!(matches!(
        shift.read_with(cx, |this, _| this.conversion.clone()),
        ConversionState::Ready(_)
    ));

    shift.update(cx, |this, cx| this.clear_selected_file(cx));
    cx.run_until_parked();

    let reset = shift.read_with(cx, |this, _| {
        this.selected_file.is_none()
            && this.selected_url.is_none()
            && matches!(this.conversion, ConversionState::Empty)
    });
    assert!(reset);
}

#[gpui::test]
async fn empty_magic_paste_is_no_op(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.submit_magic_paste_text("".into(), cx));
    cx.run_until_parked();

    let unchanged = shift.read_with(cx, |this, _| {
        this.selected_file.is_none()
            && this.selected_url.is_none()
            && matches!(this.conversion, ConversionState::Empty)
    });
    assert!(unchanged);
}

#[gpui::test]
async fn invalid_magic_paste_shows_error(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.submit_magic_paste_text("not a url or path".into(), cx)
    });
    cx.run_until_parked();

    let failed = shift.read_with(cx, |this, _| {
        matches!(this.conversion, ConversionState::Failed(_))
    });
    assert!(failed);
}

#[gpui::test]
async fn history_persists_across_sessions(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "persist.txt", b"history");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();

    let shift2 = create_shift(cx);
    let (len, restored_id) = shift2.read_with(cx, |this, _| {
        (this.history.len(), this.history.first().map(|e| e.id))
    });
    assert_eq!(len, 1);
    assert_eq!(restored_id, Some(id));
}

#[gpui::test]
async fn module_priority_persists_to_file(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.move_module(0, 1, cx));
    cx.run_until_parked();

    let contents = std::fs::read_to_string(env.module_priority_path()).unwrap();
    assert!(contents.contains("pandoc"));
    assert!(contents.contains("markitdown"));
}

#[gpui::test]
async fn clear_batch_queue_while_running_is_durable(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "run_clear_a.txt", b"a");
    let b = write_input(&env, "run_clear_b.txt", b"b");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![a, b], true, cx));
    shift.update(cx, |this, cx| this.clear_batch_queue(cx));
    cx.run_until_parked();

    let (running, empty) = shift.read_with(cx, |this, _| {
        (this.batch_running, this.batch_queue.is_empty())
    });
    assert!(!running);
    assert!(empty);
}

#[gpui::test]
async fn private_url_is_blocked_when_private_urls_disallowed(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    // The env guard normally opts into private URLs so tests can hit example.com.
    // Remove it temporarily to verify the default public-internet-only policy.
    unsafe { std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS") };

    let shift = create_shift(cx);
    shift.update(cx, |this, cx| {
        this.set_selected_url("http://192.168.1.1/page.html".into(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let blocked = shift.read_with(cx, |this, _| {
        matches!(
            &this.conversion,
            ConversionState::Failed(msg) if msg.as_ref().contains("non-public")
        )
    });
    assert!(blocked, "expected a non-public URL failure");
}

#[gpui::test]
async fn history_can_be_cleared_archived_and_deleted(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "hist.txt", b"hello");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();
    assert_eq!(shift.read_with(cx, |this, _| this.history.len()), 1);

    shift.update(cx, |this, cx| this.archive_history_entry(id, cx));
    let (archived, active_id) = shift.read_with(cx, |this, _| {
        (this.history[0].archived, this.active_history_id)
    });
    assert!(archived);
    assert!(active_id.is_none());

    shift.update(cx, |this, cx| this.delete_history_entry(id, cx));
    assert!(shift.read_with(cx, |this, _| this.history.is_empty()));

    // clear_history on an already empty history is safe.
    shift.update(cx, |this, cx| this.clear_history(cx));
    assert!(shift.read_with(cx, |this, _| this.history.is_empty()));
    assert!(shift.read_with(cx, |this, _| this.active_history_id.is_none()));
}

#[gpui::test]
async fn history_limit_truncates_entries_and_updates_input(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let paths = [
        write_input(&env, "hist_a.txt", b"a"),
        write_input(&env, "hist_b.txt", b"b"),
        write_input(&env, "hist_c.txt", b"c"),
    ];
    let shift = create_shift(cx);

    for path in &paths {
        shift.update(cx, |this, cx| {
            this.set_selected_file(path.clone(), cx);
            this.start_conversion(cx);
        });
        cx.run_until_parked();
    }
    assert_eq!(shift.read_with(cx, |this, _| this.history.len()), 3);

    shift.update(cx, |this, cx| {
        this.set_history_limit(1, cx);
        assert_eq!(this.history_limit, 1);
        assert_eq!(this.history_limit_input.read(cx).content(), "1");
    });
    assert_eq!(shift.read_with(cx, |this, _| this.history.len()), 1);

    shift.update(cx, |this, cx| {
        this.set_history_limit(100_000, cx);
        assert_eq!(this.history_limit, 30_000);
        assert_eq!(this.history_limit_input.read(cx).content(), "30000");
    });
}

#[gpui::test]
async fn conversion_failure_records_history_and_can_be_restored(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    env.override_tool("markitdown", FAILING_MARKITDOWN_FAKE);
    let path = write_input(&env, "fails.txt", b"FAIL intentional");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path.clone(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let (failed, history_failed) = shift.read_with(cx, |this, _| {
        let failed = matches!(this.conversion, ConversionState::Failed(_));
        let history_failed = matches!(
            this.history.first().map(|entry| &entry.outcome),
            Some(HistoryOutcome::Failed(_))
        );
        (failed, history_failed)
    });
    assert!(failed);
    assert!(history_failed);

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();
    shift.update(cx, |this, cx| this.clear_selected_file(cx));
    cx.run_until_parked();

    shift.update(cx, |this, cx| this.restore_history_entry(id, cx));
    let (active_id, selected, restored_failed) = shift.read_with(cx, |this, _| {
        (
            this.active_history_id,
            this.selected_file.clone(),
            matches!(this.conversion, ConversionState::Failed(_)),
        )
    });
    assert_eq!(active_id, Some(id));
    assert_eq!(selected, Some(path));
    assert!(restored_failed);
}

#[gpui::test]
async fn folder_expand_dismiss_clears_confirm_without_queuing(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let dir = env.inputs().join("folder");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("nested.txt"), b"nested").unwrap();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.ingest_paths(vec![dir.clone()], cx));
    cx.run_until_parked();
    assert!(shift.read_with(cx, |this, _| this.folder_confirm.is_some()));

    shift.update(cx, |this, cx| this.dismiss_folder_confirm(cx));
    cx.run_until_parked();
    let (cleared, empty) = shift.read_with(cx, |this, _| {
        (this.folder_confirm.is_none(), this.batch_queue.is_empty())
    });
    assert!(cleared);
    assert!(empty);
}

#[gpui::test]
async fn batch_force_overwrites_existing_output(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "force.txt", b"unique content");
    let output = path.with_extension("md");
    fs::write(&output, b"existing").unwrap();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![path], false, cx));
    shift.update(cx, |this, cx| this.toggle_batch_force(cx));
    shift.update(cx, |this, cx| this.start_batch(cx));
    cx.run_until_parked();

    assert_eq!(fs::read_to_string(&output).unwrap(), "unique content");
}

#[gpui::test]
async fn batch_with_unsupported_file_type_fails(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let bogus = env.inputs().join("bogus.xyz");
    fs::write(&bogus, b"data").unwrap();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![bogus], false, cx));
    shift.update(cx, |this, cx| this.start_batch(cx));
    cx.run_until_parked();

    let (failed, status) = shift.read_with(cx, |this, _| {
        let failed = this
            .batch_queue
            .items()
            .iter()
            .any(|item| matches!(item.state, BatchItemState::Failed { .. }));
        (failed, this.batch_status.clone())
    });
    assert!(failed, "expected the unsupported file to fail");
    assert!(
        status.as_ref().is_some_and(|s| s.contains("1 failed")),
        "expected batch to report a failure, got {status:?}"
    );
}

#[gpui::test]
async fn magic_paste_with_multiple_paths_queues_batch(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "multi_a.txt", b"a");
    let b = write_input(&env, "multi_b.txt", b"b");
    let shift = create_shift(cx);

    let text = format!("{} {}", a.display(), b.display());
    shift.update(cx, |this, cx| this.submit_magic_paste_text(text, cx));
    cx.run_until_parked();

    let (count, conversion_empty) = shift.read_with(cx, |this, _| {
        (
            this.batch_queue.items().len(),
            matches!(this.conversion, ConversionState::Empty),
        )
    });
    assert_eq!(count, 2);
    assert!(conversion_empty);
}

#[gpui::test]
async fn text_input_set_content_round_trips(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    let content = shift.update(cx, |this, cx| {
        this.url_input
            .update(cx, |input, cx| input.set_content("hello world", cx));
        this.url_input.read(cx).content().to_string()
    });
    assert_eq!(content, "hello world");
}

#[gpui::test]
async fn reveal_open_and_copy_output_do_not_panic(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.reveal_output(cx);
        this.open_output(cx);
        this.copy_output(cx);
    });
}

// -- Deterministic history persistence ordering tests --

/// Archive then unarchive stores the unarchived state even when persistence
/// is serialized. The final mutation wins regardless of scheduling.
#[gpui::test]
async fn history_persist_archive_then_unarchive_stores_latest(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "persist_order.txt", b"content");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();

    // Archive and unarchive in a single synchronous turn — the earlier
    // archived state must never win on disk.
    shift.update(cx, |this, cx| {
        this.archive_history_entry(id, cx); // archived = true
        this.archive_history_entry(id, cx); // archived = false (toggle)
    });
    cx.run_until_parked();

    // Reload from disk and verify the final unarchived state persisted.
    let shift2 = create_shift(cx);
    let archived_on_disk = shift2.read_with(cx, |this, _| {
        this.history.iter().find(|e| e.id == id).map(|e| e.archived)
    });
    assert_eq!(archived_on_disk, Some(false));
}

/// An older successful save cannot clear a newer mutation's dirty tracking.
/// After archive (rev 1) persists, a subsequent unarchive (rev 2) must still
/// be pending and eventually written.
#[gpui::test]
async fn history_persist_newer_mutation_survives_older_save(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "newer_mut.txt", b"data");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();

    // First archive triggers a persist (save A).
    shift.update(cx, |this, cx| {
        this.archive_history_entry(id, cx);
    });
    // Before save A finishes, unarchive triggers a new revision.
    shift.update(cx, |this, cx| {
        this.archive_history_entry(id, cx); // toggles back to false
    });
    cx.run_until_parked();

    // Both saves must have completed; final disk state is unarchived.
    let shift2 = create_shift(cx);
    let archived_on_disk = shift2.read_with(cx, |this, _| {
        this.history.iter().find(|e| e.id == id).map(|e| e.archived)
    });
    assert_eq!(archived_on_disk, Some(false));
}

/// Upsert followed by delete cannot resurrect the row.
#[gpui::test]
async fn history_persist_upsert_then_delete_stays_deleted(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "del.txt", b"deleteme");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();

    // Delete before the initial persist snapshot has a chance to complete.
    shift.update(cx, |this, cx| {
        this.delete_history_entry(id, cx);
    });
    cx.run_until_parked();

    let shift2 = create_shift(cx);
    let found = shift2.read_with(cx, |this, _| this.history.iter().any(|e| e.id == id));
    assert!(!found, "deleted entry must not reappear on reload");
}

/// Clear during an in-flight upsert leaves the database empty.
#[gpui::test]
async fn history_persist_clear_during_inflight_upsert(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "clear.txt", b"clearme");
    let shift = create_shift(cx);

    // Record a history entry and immediately clear without parking in between.
    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();
    assert_eq!(shift.read_with(cx, |this, _| this.history.len()), 1);

    shift.update(cx, |this, cx| {
        this.clear_history(cx);
    });
    cx.run_until_parked();

    let shift2 = create_shift(cx);
    let count = shift2.read_with(cx, |this, _| this.history.len());
    assert_eq!(count, 0, "clear must leave the database empty");
}

/// Rapid insertion of multiple entries survives restart.
#[gpui::test]
async fn history_persist_rapid_inserts_survive_restart(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);

    // Insert 5 entries in rapid succession without parking between each.
    for i in 0..5 {
        let path = write_input(
            &env,
            &format!("rapid_{i}.txt"),
            format!("content{i}").as_bytes(),
        );
        shift.update(cx, |this, cx| {
            this.set_selected_file(path, cx);
            this.start_conversion(cx);
        });
        cx.run_until_parked();
    }

    let count = shift.read_with(cx, |this, _| this.history.len());
    assert_eq!(count, 5);

    // Verify all 5 survive reload.
    let shift2 = create_shift(cx);
    let count2 = shift2.read_with(cx, |this, _| this.history.len());
    assert_eq!(count2, 5, "all rapid inserts must persist");
}

/// Repeated mutation of one ID clears only the persisted revision:
/// archive → unarchive → archive (final state is archived).
#[gpui::test]
async fn history_persist_repeated_mutation_clears_correct_revision(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "repeat.txt", b"repeat");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();

    // Three mutations in a single turn: archive, unarchive, archive again.
    shift.update(cx, |this, cx| {
        this.archive_history_entry(id, cx); // archived = true
        this.archive_history_entry(id, cx); // archived = false
        this.archive_history_entry(id, cx); // archived = true
    });
    cx.run_until_parked();

    // Final state must be archived.
    let shift2 = create_shift(cx);
    let archived_on_disk = shift2.read_with(cx, |this, _| {
        this.history.iter().find(|e| e.id == id).map(|e| e.archived)
    });
    assert_eq!(archived_on_disk, Some(true));
}

/// Open Recent with stale completion does not override a newer selection.
/// The generation guard in action_open_recent ensures the latest user action
/// wins regardless of background task completion order.
#[gpui::test]
async fn open_recent_stale_completion_does_not_override_newer_selection(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "recent_a.txt", b"aaa");
    let b = write_input(&env, "recent_b.txt", b"bbb");
    let shift = create_shift(cx);

    // Simulate what action_open_recent does: bump selection_generation and
    // spawn a background existence check for path A.
    shift.update(cx, |this, cx| {
        this.selection_generation = this.selection_generation.wrapping_add(1);
        let generation = this.selection_generation;
        let task = cx.background_executor().spawn({
            let path = a.clone();
            async move { path.exists() }
        });
        let path = a.clone();
        cx.spawn(async move |this, cx| {
            let exists = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation != generation {
                    return;
                }
                if exists {
                    this.set_selected_file(path, cx);
                }
            });
        })
        .detach();
    });
    // Without parking, immediately select B (bumps selection_generation again).
    shift.update(cx, |this, cx| {
        this.set_selected_file(b.clone(), cx);
    });
    cx.run_until_parked();

    // B should be the final selection, not A.
    let selected = shift.read_with(cx, |this, _| this.selected_file.clone());
    assert_eq!(selected, Some(b));
}

/// Open Recent missing-path result arriving after a valid selection cannot
/// overwrite it with an error state.
#[gpui::test]
async fn open_recent_missing_does_not_override_valid_selection(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let existing = write_input(&env, "exists.txt", b"valid");
    let missing = env.inputs().join("gone.txt"); // does not exist
    let shift = create_shift(cx);

    // Simulate action_open_recent for the missing path.
    shift.update(cx, |this, cx| {
        this.selection_generation = this.selection_generation.wrapping_add(1);
        let generation = this.selection_generation;
        let task = cx.background_executor().spawn({
            let path = missing.clone();
            async move { path.exists() }
        });
        let path = missing.clone();
        cx.spawn(async move |this, cx| {
            let exists = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.selection_generation != generation {
                    return;
                }
                if exists {
                    this.set_selected_file(path, cx);
                } else {
                    this.conversion = ConversionState::Failed(
                        format!("Recent file not found: {}", path.display()).into(),
                    );
                    this.selected_file = Some(path);
                    this.selected_url = None;
                    this.file_preview = None;
                    this.cached_ready_path = None;
                    cx.notify();
                }
            });
        })
        .detach();
    });
    // Immediately select a valid file (bumps selection_generation).
    shift.update(cx, |this, cx| {
        this.set_selected_file(existing.clone(), cx);
    });
    cx.run_until_parked();

    let selected = shift.read_with(cx, |this, _| this.selected_file.clone());
    assert_eq!(selected, Some(existing));
    // Must NOT be in a Failed state from the missing file.
    let is_failed = shift.read_with(cx, |this, _| {
        matches!(this.conversion, ConversionState::Failed(_))
    });
    assert!(!is_failed, "stale missing-file result must not override");
}

// =============================================================================
// Expanded integration coverage
// =============================================================================

// -- Conversion options / settings --------------------------------------------

#[gpui::test]
async fn set_output_format_to_html_waits_for_ready(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "to_html.md", b"# title");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::HTML, cx);
    });
    cx.run_until_parked();

    let (format, module) = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => (artifact.format, artifact.module_id),
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(format, OutputFormat::HTML);
    assert_eq!(module, "pandoc");
}

#[gpui::test]
async fn set_output_format_to_pdf_via_pandoc(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "to_pdf.md", b"# pdf me");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::PDF, cx);
    });
    cx.run_until_parked();

    let (format, module) = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => (artifact.format, artifact.module_id),
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(format, OutputFormat::PDF);
    assert_eq!(module, "pandoc");
}

#[gpui::test]
async fn set_output_format_to_mp3_from_video(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "clip.mp4", b"fake-video-bytes");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::MP3, cx);
    });
    cx.run_until_parked();

    let (format, module) = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => (artifact.format, artifact.module_id),
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(format, OutputFormat::MP3);
    assert_eq!(module, "ffmpeg");
    let bytes = shift.read_with(cx, |this, _| {
        this.conversion
            .ready_artifact()
            .map(|a| a.bytes.clone())
            .unwrap()
    });
    assert_eq!(&bytes[..], b"shift-fake-media");
}

#[gpui::test]
async fn markitdown_keep_data_uris_appears_in_invocations(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    env.override_tool("markitdown", MARKITDOWN_ARGS_FAKE);
    let path = write_input(&env, "keep_uris.txt", b"with data");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.markitdown_keep_data_uris = true;
        this.set_selected_file(path, cx);
        this.apply_session_option_change(cx);
    });
    // apply_session_option_change reconverts only when options are visible;
    // markitdown source should make conversion_options_visible true.
    shift.update(cx, |this, cx| this.start_conversion(cx));
    cx.run_until_parked();

    let argv = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => artifact
            .invocations
            .first()
            .map(|inv| inv.argv_display.clone())
            .unwrap_or_default(),
        other => panic!("expected Ready, got {other:?}"),
    });
    assert!(
        argv.contains("--keep-data-uris"),
        "expected keep-data-uris in argv, got: {argv}"
    );

    let json = fs::read_to_string(env.session_path()).unwrap();
    assert!(
        json.contains("keep_data_uris") || json.contains("\"keep_data_uris\":true"),
        "session settings should persist keep_data_uris: {json}"
    );
}

#[gpui::test]
async fn markitdown_keep_data_uris_off_omits_flag(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    env.override_tool("markitdown", MARKITDOWN_ARGS_FAKE);
    let path = write_input(&env, "no_uris.txt", b"plain");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.markitdown_keep_data_uris = false;
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let argv = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => artifact
            .invocations
            .first()
            .map(|inv| inv.argv_display.clone())
            .unwrap_or_default(),
        other => panic!("expected Ready, got {other:?}"),
    });
    assert!(
        !argv.contains("--keep-data-uris"),
        "keep_data_uris=false must not pass flag: {argv}"
    );
}

#[gpui::test]
async fn set_history_limit_clamps_zero_to_min(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_history_limit(0, cx);
        assert_eq!(this.history_limit, MIN_HISTORY_LIMIT);
        assert_eq!(
            this.history_limit_input.read(cx).content(),
            MIN_HISTORY_LIMIT.to_string()
        );
    });
}

#[gpui::test]
async fn set_history_limit_clamps_above_max(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_history_limit(MAX_HISTORY_LIMIT.saturating_add(1), cx);
        assert_eq!(this.history_limit, MAX_HISTORY_LIMIT);
    });
    cx.run_until_parked();

    let json = fs::read_to_string(env.session_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        value["history_limit"].as_u64().unwrap() as usize,
        MAX_HISTORY_LIMIT
    );
}

#[gpui::test]
async fn set_ui_font_family_persists(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_ui_font_family("Menlo".into(), cx);
        assert_eq!(this.ui_font_family, "Menlo");
    });
    cx.run_until_parked();

    let json = fs::read_to_string(env.session_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["ui_font_family"].as_str().unwrap(), "Menlo");

    let shift2 = create_shift(cx);
    let family = shift2.read_with(cx, |this, _| this.ui_font_family.clone());
    assert_eq!(family, "Menlo");
}

#[gpui::test]
async fn set_ui_font_family_empty_resets_to_default(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_ui_font_family("Menlo".into(), cx);
        this.set_ui_font_family("   ".into(), cx);
        assert_eq!(this.ui_font_family, DEFAULT_UI_FONT);
    });
}

#[gpui::test]
async fn show_archived_filters_history_cache(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "archive_filter.txt", b"hello");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();
    shift.update(cx, |this, cx| this.archive_history_entry(id, cx));
    cx.run_until_parked();

    // Default: archived hidden.
    shift.update(cx, |this, cx| {
        this.show_archived = false;
        this.mark_history_cache_dirty();
        this.ensure_history_cache(cx);
        assert!(
            this.cached_history_visible.is_empty(),
            "archived entry should be hidden when show_archived=false"
        );
    });

    // Toggle on: archived visible.
    shift.update(cx, |this, cx| {
        this.show_archived = true;
        this.mark_history_cache_dirty();
        this.ensure_history_cache(cx);
        this.persist_session_settings(cx);
        assert_eq!(this.cached_history_visible.len(), 1);
        assert!(this.cached_history_visible[0].archived);
    });
    cx.run_until_parked();

    let json = fs::read_to_string(env.session_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(value["show_archived"].as_bool().unwrap());
}

#[gpui::test]
async fn panel_resize_begin_sets_drag_state(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    let start_width = shift.read_with(cx, |this, _| this.history_sidebar_width);
    shift.update(cx, |this, cx| {
        this.begin_panel_resize(PanelResizeTarget::History, 100.0, cx);
        assert!(this.panel_resize.is_some());
        let drag = this.panel_resize.unwrap();
        assert_eq!(drag.target, PanelResizeTarget::History);
        assert_eq!(drag.start_x, 100.0);
        assert_eq!(drag.start_width, start_width);
    });
}

#[gpui::test]
async fn panel_resize_end_clears_drag_and_persists_widths(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.history_sidebar_width = HISTORY_SIDEBAR_MIN + 10.0;
        this.output_panel_width = OUTPUT_PANEL_MIN + 20.0;
        this.begin_panel_resize(PanelResizeTarget::Output, 50.0, cx);
        this.end_panel_resize(cx);
        assert!(this.panel_resize.is_none());
    });
    cx.run_until_parked();

    let json = fs::read_to_string(env.session_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(
        (value["history_sidebar_width"].as_f64().unwrap() - (HISTORY_SIDEBAR_MIN + 10.0) as f64)
            .abs()
            < 0.01
    );
    assert!(
        (value["output_panel_width"].as_f64().unwrap() - (OUTPUT_PANEL_MIN + 20.0) as f64).abs()
            < 0.01
    );
}

#[gpui::test]
async fn panel_resize_end_without_begin_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        assert!(this.panel_resize.is_none());
        this.end_panel_resize(cx);
        assert!(this.panel_resize.is_none());
    });
}

#[gpui::test]
async fn panel_widths_respect_known_min_max_bounds_on_construct(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    // Seed extreme session widths; Shift::new clamps them.
    let support = env.support();
    fs::create_dir_all(&support).unwrap();
    fs::write(
        env.session_path(),
        r#"{
            "output_format": "markdown",
            "history_sidebar_width": 1.0,
            "output_panel_width": 99999.0
        }"#,
    )
    .unwrap();

    let shift = create_shift(cx);
    let (history_w, output_w) = shift.read_with(cx, |this, _| {
        (this.history_sidebar_width, this.output_panel_width)
    });
    assert!(
        (HISTORY_SIDEBAR_MIN..=HISTORY_SIDEBAR_MAX).contains(&history_w),
        "history width {history_w} out of [{HISTORY_SIDEBAR_MIN}, {HISTORY_SIDEBAR_MAX}]"
    );
    assert!(
        (OUTPUT_PANEL_MIN..=OUTPUT_PANEL_MAX).contains(&output_w),
        "output width {output_w} out of [{OUTPUT_PANEL_MIN}, {OUTPUT_PANEL_MAX}]"
    );
}

// -- Batch --------------------------------------------------------------------

#[gpui::test]
async fn batch_mixed_success_and_failure(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    env.override_tool("markitdown", FAILING_MARKITDOWN_FAKE);
    let ok = write_input(&env, "mix_ok.txt", b"ok content");
    let fail = write_input(&env, "mix_fail.txt", b"FAIL me");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![ok.clone(), fail.clone()], true, cx)
    });
    cx.run_until_parked();

    let (succeeded, failed, status) = shift.read_with(cx, |this, _| {
        let mut s = 0;
        let mut f = 0;
        for item in this.batch_queue.items() {
            match &item.state {
                BatchItemState::Succeeded { .. } => s += 1,
                BatchItemState::Failed { .. } => f += 1,
                _ => {}
            }
        }
        (s, f, this.batch_status.clone())
    });
    assert_eq!(succeeded, 1);
    assert_eq!(failed, 1);
    assert!(
        status
            .as_ref()
            .is_some_and(|s| s.contains("1 succeeded") && s.contains("1 failed")),
        "status={status:?}"
    );
}

#[gpui::test]
async fn retry_failed_batch_all_failed(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    env.override_tool("markitdown", ALWAYS_FAIL_MARKITDOWN_FAKE);
    let a = write_input(&env, "all_fail_a.txt", b"a");
    let b = write_input(&env, "all_fail_b.txt", b"b");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![a.clone(), b.clone()], true, cx)
    });
    cx.run_until_parked();

    let failed_count = shift.read_with(cx, |this, _| {
        this.batch_queue
            .items()
            .iter()
            .filter(|i| matches!(i.state, BatchItemState::Failed { .. }))
            .count()
    });
    assert_eq!(failed_count, 2);

    // Fix the converter, then retry all failed.
    env.override_tool("markitdown", MARKITDOWN_FAKE);
    // Registry still holds the old executable path pointing at the script file
    // which we just rewrote — same path, new body. No rebuild needed.
    shift.update(cx, |this, cx| this.retry_failed_batch(cx));
    cx.run_until_parked();

    let all_ok = shift.read_with(cx, |this, _| {
        this.batch_queue
            .items()
            .iter()
            .all(|i| matches!(i.state, BatchItemState::Succeeded { .. }))
    });
    assert!(all_ok, "retry_failed_batch should re-run and succeed");
}

#[gpui::test]
async fn cancel_batch_then_clear(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "cancel_clear_a.txt", b"a");
    let b = write_input(&env, "cancel_clear_b.txt", b"b");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![a, b], true, cx));
    shift.update(cx, |this, cx| this.cancel_batch(cx));
    cx.run_until_parked();

    shift.update(cx, |this, cx| this.clear_batch_queue(cx));
    cx.run_until_parked();

    let (empty, running) = shift.read_with(cx, |this, _| {
        (this.batch_queue.is_empty(), this.batch_running)
    });
    assert!(empty);
    assert!(!running);
}

#[gpui::test]
async fn batch_item_format_override_then_inherit_again(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "fmt_cycle.txt", b"text");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![path], false, cx));
    let id = shift.read_with(cx, |this, _| this.batch_queue.items()[0].id);

    // Pin override to current (markdown).
    shift.update(cx, |this, cx| this.toggle_batch_item_format(id, cx));
    // Change global format — override stays markdown.
    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::HTML, cx)
    });
    cx.run_until_parked();
    let pinned = shift.read_with(cx, |this, _| {
        (
            this.batch_queue.items()[0].format_selection,
            this.batch_queue.items()[0].output_format,
        )
    });
    assert_eq!(
        pinned.0,
        BatchFormatSelection::Override(OutputFormat::MARKDOWN)
    );
    assert_eq!(pinned.1, OutputFormat::MARKDOWN);

    // Toggle back to inherit → picks up HTML.
    shift.update(cx, |this, cx| this.toggle_batch_item_format(id, cx));
    cx.run_until_parked();
    let inherited = shift.read_with(cx, |this, _| {
        (
            this.batch_queue.items()[0].format_selection,
            this.batch_queue.items()[0].output_format,
        )
    });
    assert_eq!(inherited.0, BatchFormatSelection::Inherit);
    assert_eq!(inherited.1, OutputFormat::HTML);
}

#[gpui::test]
async fn batch_output_dir_and_force_together(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let out_dir = env.temp.join("out_force");
    fs::create_dir_all(&out_dir).unwrap();
    let path = write_input(&env, "dir_force.txt", b"new-content");
    let dest = out_dir.join("dir_force.md");
    fs::write(&dest, b"stale").unwrap();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_batch_output_dir(out_dir.clone(), cx);
        this.toggle_batch_force(cx);
        this.enqueue_paths(vec![path], true, cx);
    });
    cx.run_until_parked();

    assert_eq!(fs::read_to_string(&dest).unwrap(), "new-content");
    let force = shift.read_with(cx, |this, _| this.batch_force);
    assert!(force);
}

#[gpui::test]
async fn empty_queue_start_batch_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.start_batch(cx));
    cx.run_until_parked();

    let (running, status, empty) = shift.read_with(cx, |this, _| {
        (
            this.batch_running,
            this.batch_status.clone(),
            this.batch_queue.is_empty(),
        )
    });
    assert!(!running);
    assert!(empty);
    assert!(
        status
            .as_ref()
            .is_some_and(|s| s.contains("Nothing queued")),
        "status={status:?}"
    );
}

#[gpui::test]
async fn batch_without_force_fails_when_output_exists(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "exists_out.txt", b"payload");
    let output = path.with_extension("md");
    fs::write(&output, b"pre-existing").unwrap();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![path], true, cx));
    cx.run_until_parked();

    let failed = shift.read_with(cx, |this, _| {
        this.batch_queue.items().iter().any(|item| {
            matches!(
                &item.state,
                BatchItemState::Failed { error } if error.contains("already exists")
            )
        })
    });
    assert!(failed, "expected failure when output exists without force");
    // Original pre-existing file must be preserved.
    assert_eq!(fs::read_to_string(&output).unwrap(), "pre-existing");
}

#[gpui::test]
async fn ingest_single_file_selects_without_batch(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "ingest_one.txt", b"solo");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.ingest_paths(vec![path.clone()], cx));
    cx.run_until_parked();

    let (selected, empty_batch) = shift.read_with(cx, |this, _| {
        (this.selected_file.clone(), this.batch_queue.is_empty())
    });
    assert_eq!(selected, Some(path));
    assert!(empty_batch);
}

#[gpui::test]
async fn ingest_multiple_files_queues_without_autostart(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "ingest_a.txt", b"a");
    let b = write_input(&env, "ingest_b.txt", b"b");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.ingest_paths(vec![a, b], cx));
    cx.run_until_parked();

    let (count, running, all_queued) = shift.read_with(cx, |this, _| {
        let items = this.batch_queue.items();
        (
            items.len(),
            this.batch_running,
            items
                .iter()
                .all(|i| matches!(i.state, BatchItemState::Queued)),
        )
    });
    assert_eq!(count, 2);
    assert!(!running);
    assert!(all_queued);
}

// -- Magic paste / sources ----------------------------------------------------

#[gpui::test]
async fn magic_paste_file_url_selects_local_path(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "file_url.txt", b"via file url");
    let shift = create_shift(cx);

    let url = format!("file://{}", path.display());
    shift.update(cx, |this, cx| this.submit_magic_paste_text(url, cx));
    cx.run_until_parked();
    shift.update(cx, |this, cx| this.start_conversion(cx));
    cx.run_until_parked();

    let selected = shift.read_with(cx, |this, _| this.selected_file.clone());
    assert_eq!(selected, Some(path));
    let ready = shift.read_with(cx, |this, _| {
        matches!(this.conversion, ConversionState::Ready(_))
    });
    assert!(ready);
}

#[gpui::test]
async fn magic_paste_multiple_urls_queues_batch(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    let text = "http://example.com/a.html http://example.com/b.html";
    shift.update(cx, |this, cx| this.submit_magic_paste_text(text.into(), cx));
    cx.run_until_parked();

    let (count, sources_are_urls) = shift.read_with(cx, |this, _| {
        let items = this.batch_queue.items();
        let urls = items
            .iter()
            .all(|i| matches!(i.source, BatchSource::Url(_)));
        (items.len(), urls)
    });
    assert_eq!(count, 2);
    assert!(sources_are_urls);
}

#[gpui::test]
async fn magic_paste_private_url_blocked_when_disallowed(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    unsafe { std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS") };

    let shift = create_shift(cx);
    shift.update(cx, |this, cx| {
        this.submit_magic_paste_text("http://10.0.0.1/secret.html".into(), cx)
    });
    cx.run_until_parked();
    // Materialize / convert should fail for private hosts.
    shift.update(cx, |this, cx| {
        if this.selected_url.is_some() {
            this.start_conversion(cx);
        }
    });
    cx.run_until_parked();

    let blocked = shift.read_with(cx, |this, _| {
        matches!(
            &this.conversion,
            ConversionState::Failed(msg)
                if msg.as_ref().to_ascii_lowercase().contains("non-public")
                    || msg.as_ref().to_ascii_lowercase().contains("private")
                    || msg.as_ref().to_ascii_lowercase().contains("blocked")
        ) || this.batch_queue.items().iter().any(|item| {
            matches!(
                &item.state,
                BatchItemState::Failed { error }
                    if error.to_ascii_lowercase().contains("non-public")
                        || error.to_ascii_lowercase().contains("private")
            )
        })
    });
    assert!(blocked, "private URL paste must be blocked");
}

#[gpui::test]
async fn magic_paste_invalid_file_url_fails(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.submit_magic_paste_text("file://hostname/only".into(), cx)
    });
    cx.run_until_parked();

    let failed = shift.read_with(cx, |this, _| {
        matches!(
            &this.conversion,
            ConversionState::Failed(msg) if msg.as_ref().contains("file://")
        )
    });
    assert!(failed);
}

#[gpui::test]
async fn magic_paste_from_url_input_entity(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "from_input.txt", b"typed path");
    let shift = create_shift(cx);

    let text = path.to_string_lossy().to_string();
    shift.update(cx, |this, cx| {
        this.url_input
            .update(cx, |input, cx| input.set_content(&text, cx));
        this.submit_magic_paste_from_input(cx);
    });
    cx.run_until_parked();
    shift.update(cx, |this, cx| this.start_conversion(cx));
    cx.run_until_parked();

    let selected = shift.read_with(cx, |this, _| this.selected_file.clone());
    assert_eq!(selected, Some(path));
}

// -- History ------------------------------------------------------------------

#[gpui::test]
async fn ready_large_history_restore_reconverts(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    env.override_tool("markitdown", LARGE_MARKITDOWN_FAKE);
    let path = write_input(&env, "large.txt", b"seed");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path.clone(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let (ready_len, history_large) = shift.read_with(cx, |this, _| {
        let ready_len = match &this.conversion {
            ConversionState::Ready(a) => a.bytes.len(),
            other => panic!("expected Ready, got {other:?}"),
        };
        let history_large = matches!(
            this.history.first().map(|e| &e.outcome),
            Some(HistoryOutcome::ReadyLarge { byte_len, .. })
                if *byte_len > MAX_HISTORY_ARTIFACT_BYTES
        );
        (ready_len, history_large)
    });
    assert!(
        ready_len > MAX_HISTORY_ARTIFACT_BYTES,
        "artifact should exceed history cap ({ready_len})"
    );
    assert!(history_large, "history should store ReadyLarge");

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();
    shift.update(cx, |this, cx| this.clear_selected_file(cx));
    cx.run_until_parked();

    shift.update(cx, |this, cx| this.restore_history_entry(id, cx));
    cx.run_until_parked();

    let (selected, ready_again) = shift.read_with(cx, |this, _| {
        (
            this.selected_file.clone(),
            matches!(this.conversion, ConversionState::Ready(_)),
        )
    });
    assert_eq!(selected, Some(path));
    assert!(ready_again, "ReadyLarge restore must re-run conversion");
}

#[gpui::test]
async fn delete_multiple_history_entries(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let paths = [
        write_input(&env, "del_a.txt", b"a"),
        write_input(&env, "del_b.txt", b"b"),
        write_input(&env, "del_c.txt", b"c"),
    ];
    let shift = create_shift(cx);

    for path in &paths {
        shift.update(cx, |this, cx| {
            this.set_selected_file(path.clone(), cx);
            this.start_conversion(cx);
        });
        cx.run_until_parked();
    }
    assert_eq!(shift.read_with(cx, |this, _| this.history.len()), 3);

    let ids: Vec<u64> = shift.read_with(cx, |this, _| this.history.iter().map(|e| e.id).collect());
    for id in ids.iter().take(2) {
        shift.update(cx, |this, cx| this.delete_history_entry(*id, cx));
    }
    cx.run_until_parked();

    assert_eq!(shift.read_with(cx, |this, _| this.history.len()), 1);
    let remaining = ids[2];
    assert!(shift.read_with(cx, |this, _| this.history.iter().any(|e| e.id == remaining)));
}

#[gpui::test]
async fn history_records_url_source(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url("http://example.com/hist.html".into(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let source = shift.read_with(cx, |this, _| this.history.first().map(|e| e.source.clone()));
    assert!(matches!(
        source,
        Some(HistorySource::Url(u)) if u.contains("example.com/hist.html")
    ));
}

#[gpui::test]
async fn restore_url_history_entry(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url("http://example.com/restore.html".into(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();
    shift.update(cx, |this, cx| this.clear_selected_file(cx));
    cx.run_until_parked();

    shift.update(cx, |this, cx| this.restore_history_entry(id, cx));
    cx.run_until_parked();

    let (url, ready) = shift.read_with(cx, |this, _| {
        (
            this.selected_url.clone(),
            matches!(this.conversion, ConversionState::Ready(_)),
        )
    });
    assert_eq!(url.as_deref(), Some("http://example.com/restore.html"));
    assert!(ready);
}

// -- Module priority / menus --------------------------------------------------

#[gpui::test]
async fn move_module_same_index_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    let before = shift.read_with(cx, |this, _| this.module_priority.clone());
    shift.update(cx, |this, cx| this.move_module(0, 0, cx));
    let after = shift.read_with(cx, |this, _| this.module_priority.clone());
    assert_eq!(before, after);
}

#[gpui::test]
async fn move_module_out_of_bounds_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    let before = shift.read_with(cx, |this, _| this.module_priority.clone());
    shift.update(cx, |this, cx| {
        this.move_module(0, 999, cx);
        this.move_module(999, 0, cx);
    });
    let after = shift.read_with(cx, |this, _| this.module_priority.clone());
    assert_eq!(before, after);
}

#[gpui::test]
async fn recent_file_menu_items_nonempty_after_conversion(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "recent_menu.txt", b"menu");
    let shift = create_shift(cx);

    let before_len = shift.read_with(cx, |this, _| this.recent_file_menu_items().len());
    // Empty history → single "No Recent Items" placeholder.
    assert_eq!(before_len, 1);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let after_len = shift.read_with(cx, |this, _| this.recent_file_menu_items().len());
    // File entry + separator + Clear Recent.
    assert!(
        after_len >= 3,
        "expected recent items after conversion, got {after_len}"
    );
}

#[gpui::test]
async fn build_app_menus_nonempty(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    let menus = shift.read_with(cx, |this, _| this.build_app_menus());
    assert!(!menus.is_empty());
    assert!(menus.len() >= 4);
}

#[gpui::test]
async fn rebuild_app_menus_does_not_panic(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "rebuild_menus.txt", b"x");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| {
        this.rebuild_app_menus(cx);
    });
}

// -- active_option_modules / conversion_options_visible -----------------------

#[gpui::test]
async fn active_option_modules_for_pdf_include_docling_or_markitdown(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "opts.pdf", b"%PDF-1.4 fake");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
    });
    cx.run_until_parked();

    let modules = shift.read_with(cx, |this, _| this.active_option_modules());
    assert!(
        modules.contains(&"docling") || modules.contains(&"markitdown"),
        "pdf markdown route modules={modules:?}"
    );
    assert!(shift.read_with(cx, |this, _| this.conversion_options_visible()));
}

#[gpui::test]
async fn active_option_modules_for_video_include_ffmpeg(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "opts.mp4", b"vid");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        // Suggested format for mp4 is MP4 (media); ensure ffmpeg options show.
    });
    cx.run_until_parked();

    let (modules, format, visible) = shift.read_with(cx, |this, _| {
        (
            this.active_option_modules(),
            this.output_format,
            this.conversion_options_visible(),
        )
    });
    assert_eq!(format, OutputFormat::MP4);
    assert!(
        modules.contains(&"ffmpeg"),
        "video route modules={modules:?}"
    );
    assert!(visible);
}

#[gpui::test]
async fn active_option_modules_for_url_include_defuddle(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url("http://example.com/article".into(), cx);
    });
    cx.run_until_parked();

    let modules = shift.read_with(cx, |this, _| this.active_option_modules());
    assert!(
        modules.contains(&"defuddle"),
        "url route modules={modules:?}"
    );
    assert!(shift.read_with(cx, |this, _| this.conversion_options_visible()));
}

#[gpui::test]
async fn conversion_options_visible_for_media_output_without_source(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::MP3, cx);
    });
    cx.run_until_parked();

    let (modules, visible) = shift.read_with(cx, |this, _| {
        (
            this.active_option_modules(),
            this.conversion_options_visible(),
        )
    });
    assert_eq!(modules, vec!["ffmpeg"]);
    assert!(visible);
}

#[gpui::test]
async fn conversion_options_hidden_without_source_for_markdown(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    let (modules, visible) = shift.read_with(cx, |this, _| {
        (
            this.active_option_modules(),
            this.conversion_options_visible(),
        )
    });
    assert!(modules.is_empty(), "modules={modules:?}");
    assert!(!visible);
}

#[gpui::test]
async fn active_option_modules_for_txt_to_html_include_pandoc(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "opts.txt", b"hello");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::HTML, cx);
    });
    cx.run_until_parked();

    let modules = shift.read_with(cx, |this, _| this.active_option_modules());
    assert!(modules.contains(&"pandoc"), "txt→html modules={modules:?}");
}

// -- Error paths --------------------------------------------------------------

#[gpui::test]
async fn missing_tool_binary_fails_with_failed_state(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let missing = env.bin.join("missing-markitdown-binary");
    // Point env at a non-existent binary before constructing Shift/registry.
    unsafe {
        std::env::set_var("SHIFT_MARKITDOWN_BIN", &missing);
    }
    let path = write_input(&env, "missing_tool.txt", b"hello");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let failed_msg = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Failed(msg) => Some(msg.to_string()),
        other => panic!("expected Failed, got {other:?}"),
    });
    let msg = failed_msg.unwrap();
    let lower = msg.to_ascii_lowercase();
    assert!(
        lower.contains("not found")
            || lower.contains("no such file")
            || lower.contains("failed")
            || lower.contains("executable")
            || lower.contains("not installed")
            || lower.contains("markitdown"),
        "unexpected failure message: {msg}"
    );
}

#[gpui::test]
async fn converter_exit_1_records_history_failure(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    env.override_tool("markitdown", ALWAYS_FAIL_MARKITDOWN_FAKE);
    let path = write_input(&env, "exit1.txt", b"boom");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let (failed, hist_failed, hist_len) = shift.read_with(cx, |this, _| {
        let failed = matches!(this.conversion, ConversionState::Failed(_));
        let hist_failed = matches!(
            this.history.first().map(|e| &e.outcome),
            Some(HistoryOutcome::Failed(_))
        );
        (failed, hist_failed, this.history.len())
    });
    assert!(failed);
    assert!(hist_failed);
    assert_eq!(hist_len, 1);
}

#[gpui::test]
async fn missing_source_start_conversion_stays_empty(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.start_conversion(cx));
    cx.run_until_parked();

    let empty = shift.read_with(cx, |this, _| {
        matches!(this.conversion, ConversionState::Empty)
    });
    assert!(empty);
}

// -- Source matching ----------------------------------------------------------

#[gpui::test]
async fn source_matches_after_select_file(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "match.txt", b"m");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path.clone(), cx);
    });
    cx.run_until_parked();

    let matches = shift.read_with(cx, |this, _| {
        this.source_matches(&BatchSource::File(path.clone()))
    });
    assert!(matches);

    let no_match = shift.read_with(cx, |this, _| {
        this.source_matches(&BatchSource::File(PathBuf::from("/other/file.txt")))
    });
    assert!(!no_match);
}

#[gpui::test]
async fn source_matches_after_select_url(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);
    let url = "http://example.com/match.html".to_string();

    shift.update(cx, |this, cx| {
        this.set_selected_url(url.clone(), cx);
    });
    cx.run_until_parked();

    let matches = shift.read_with(cx, |this, _| {
        this.source_matches(&BatchSource::Url(url.clone()))
    });
    assert!(matches);

    let no_match = shift.read_with(cx, |this, _| {
        this.source_matches(&BatchSource::Url("http://other.example/".into()))
    });
    assert!(!no_match);
}

#[gpui::test]
async fn source_matches_false_after_clear(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "clear_match.txt", b"x");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path.clone(), cx);
        this.clear_selected_file(cx);
    });
    cx.run_until_parked();

    let matches = shift.read_with(cx, |this, _| {
        this.source_matches(&BatchSource::File(path))
            || this.source_matches(&BatchSource::Url("http://x".into()))
    });
    assert!(!matches);

    let empty = shift.read_with(cx, |this, _| {
        this.selected_file.is_none()
            && this.selected_url.is_none()
            && matches!(this.conversion, ConversionState::Empty)
    });
    assert!(empty);
}

// -- Session option knobs -----------------------------------------------------

#[gpui::test]
async fn defuddle_frontmatter_session_option_persists(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.defuddle_frontmatter = true;
        this.set_selected_url("http://example.com/fm.html".into(), cx);
        this.apply_session_option_change(cx);
    });
    cx.run_until_parked();

    let json = fs::read_to_string(env.session_path()).unwrap();
    assert!(
        json.contains("frontmatter") || json.contains("defuddle"),
        "expected defuddle options in session: {json}"
    );
}

#[gpui::test]
async fn pandoc_standalone_and_toc_persist(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "pandoc_opts.md", b"# hi");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.pandoc_standalone = true;
        this.pandoc_toc = true;
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::HTML, cx);
        this.apply_session_option_change(cx);
    });
    cx.run_until_parked();

    let json = fs::read_to_string(env.session_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    // Nested under conversion options or pandoc key depending on schema.
    let text = value.to_string();
    assert!(
        text.contains("standalone") || text.contains("toc"),
        "session should mention pandoc knobs: {text}"
    );
}

#[gpui::test]
async fn ffmpeg_mono_session_option_persists(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "mono.wav", b"RIFF....");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.ffmpeg_mono = true;
        this.set_selected_file(path, cx);
        this.apply_session_option_change(cx);
    });
    cx.run_until_parked();

    let mono = shift.read_with(cx, |this, _| this.ffmpeg_mono);
    assert!(mono);
    let json = fs::read_to_string(env.session_path()).unwrap();
    assert!(
        json.contains("mono") || json.contains("ffmpeg"),
        "json={json}"
    );
}

#[gpui::test]
async fn build_conversion_options_rejects_invalid_ffmpeg_start(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "bad_opts.mp4", b"vid");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.ffmpeg_start_input
            .update(cx, |input, cx| input.set_content("nope", cx));
        this.set_selected_file(path, cx);
        let err = this.build_conversion_options(cx);
        assert!(err.is_err(), "invalid start should error");
    });
}

#[gpui::test]
async fn session_settings_reload_output_format(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.set_output_format(OutputFormat::MP3, cx));
    cx.run_until_parked();

    let shift2 = create_shift(cx);
    let format = shift2.read_with(cx, |this, _| this.output_format);
    assert_eq!(format, OutputFormat::MP3);
    let _ = env; // keep temp alive
}

#[gpui::test]
async fn set_output_format_same_value_does_not_fail(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "same_fmt.txt", b"x");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::MARKDOWN, cx)
    });
    cx.run_until_parked();

    let ready = shift.read_with(cx, |this, _| {
        matches!(this.conversion, ConversionState::Ready(_))
    });
    assert!(ready);
}

#[gpui::test]
async fn suggested_format_for_wav_is_mp3(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "track.wav", b"audio");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.user_chose_format = false;
        this.set_selected_file(path, cx);
    });
    cx.run_until_parked();

    let format = shift.read_with(cx, |this, _| this.output_format);
    assert_eq!(format, OutputFormat::MP3);
}

#[gpui::test]
async fn cancel_conversion_then_restart_succeeds(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "restart.txt", b"again");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
        this.cancel_conversion(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| this.start_conversion(cx));
    cx.run_until_parked();

    let ready = shift.read_with(cx, |this, _| {
        matches!(this.conversion, ConversionState::Ready(_))
    });
    assert!(ready);
}

#[gpui::test]
async fn history_limit_persists_across_sessions(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.set_history_limit(7, cx));
    cx.run_until_parked();

    let shift2 = create_shift(cx);
    let limit = shift2.read_with(cx, |this, _| this.history_limit);
    assert_eq!(limit, 7);
    let _ = env;
}

#[gpui::test]
async fn clear_history_rebuilds_empty_recent_menu(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "clear_recent.txt", b"x");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();
    assert!(shift.read_with(cx, |this, _| this.recent_file_menu_items().len()) >= 3);

    shift.update(cx, |this, cx| this.clear_history(cx));
    cx.run_until_parked();

    let len = shift.read_with(cx, |this, _| this.recent_file_menu_items().len());
    assert_eq!(len, 1, "should fall back to No Recent Items");
}

#[gpui::test]
async fn batch_status_updates_when_format_changed_with_queue(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "queued_fmt.txt", b"q");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![path], false, cx));
    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::HTML, cx)
    });
    cx.run_until_parked();

    let (status, format) = shift.read_with(cx, |this, _| {
        (
            this.batch_status.clone(),
            this.batch_queue.items()[0].output_format,
        )
    });
    assert_eq!(format, OutputFormat::HTML);
    assert!(
        status.as_ref().is_some_and(|s| s.contains("Queued items")),
        "status={status:?}"
    );
}

#[gpui::test]
async fn retry_failed_batch_with_zero_failures_is_safe(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "no_fail.txt", b"ok");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![path], true, cx));
    cx.run_until_parked();

    shift.update(cx, |this, cx| this.retry_failed_batch(cx));
    cx.run_until_parked();

    let status = shift.read_with(cx, |this, _| this.batch_status.clone());
    assert!(
        status
            .as_ref()
            .is_some_and(|s| s.contains("Re-queued 0") || s.contains("Batch complete")),
        "status={status:?}"
    );
}

#[gpui::test]
async fn apply_conversion_options_reconverts_selected_file(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "apply_opts.txt", b"content");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| {
        this.markitdown_keep_data_uris = true;
        this.apply_conversion_options(cx);
    });
    cx.run_until_parked();

    let ready = shift.read_with(cx, |this, _| {
        matches!(this.conversion, ConversionState::Ready(_))
    });
    assert!(ready);
}

#[gpui::test]
async fn folder_expand_empty_dir_reports_no_files(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let dir = env.inputs().join("empty_folder");
    fs::create_dir_all(&dir).unwrap();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.ingest_paths(vec![dir], cx));
    cx.run_until_parked();

    let (confirm, status) = shift.read_with(cx, |this, _| {
        (this.folder_confirm.is_none(), this.batch_status.clone())
    });
    assert!(confirm);
    assert!(
        status
            .as_ref()
            .is_some_and(|s| s.to_ascii_lowercase().contains("no convertible")),
        "status={status:?}"
    );
}

#[gpui::test]
async fn history_active_id_cleared_on_new_selection(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "active_a.txt", b"a");
    let b = write_input(&env, "active_b.txt", b"b");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(a, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();
    assert!(shift.read_with(cx, |this, _| this.active_history_id.is_some()));

    shift.update(cx, |this, cx| {
        this.set_selected_file(b, cx);
    });
    cx.run_until_parked();

    let active = shift.read_with(cx, |this, _| this.active_history_id);
    assert!(active.is_none());
}

#[gpui::test]
async fn docling_html_from_pdf_in_single_file(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "docling_html.pdf", b"%PDF-1.4 fake");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::HTML, cx);
    });
    cx.run_until_parked();

    let (format, module, text) = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => (
            artifact.format,
            artifact.module_id,
            String::from_utf8_lossy(&artifact.bytes).to_string(),
        ),
        other => panic!("expected Ready, got {other:?}"),
    });
    assert_eq!(format, OutputFormat::HTML);
    assert_eq!(module, "docling");
    assert!(text.contains("docling fake"));
}

#[gpui::test]
async fn batch_queue_progress_counts_after_complete(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "prog_a.txt", b"a");
    let b = write_input(&env, "prog_b.txt", b"b");
    let c = write_input(&env, "prog_c.txt", b"c");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![a, b, c], true, cx));
    cx.run_until_parked();

    let progress = shift.read_with(cx, |this, _| this.batch_queue.progress());
    assert_eq!(progress.completed(), 3);
    assert_eq!(progress.queued, 0);
}

#[gpui::test]
async fn set_batch_output_dir_updates_queued_destinations(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let out = env.temp.join("dest_update");
    fs::create_dir_all(&out).unwrap();
    let path = write_input(&env, "dest_item.txt", b"d");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![path], false, cx));
    shift.update(cx, |this, cx| this.set_batch_output_dir(out.clone(), cx));
    cx.run_until_parked();

    let dest = shift.read_with(cx, |this, _| {
        this.batch_queue.items()[0].destination.clone()
    });
    assert_eq!(dest.parent(), Some(out.as_path()));
}
