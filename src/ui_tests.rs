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

use crate::app::{ConversionState, HistoryOutcome, Shift};
use shift_core::conversion::{BatchFormatSelection, BatchItemState, OutputFormat};

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
    let path = write_input(&env, "batch_format.txt", b"hello");
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
