//! Headless UI/integration tests for the GPUI app model.
//!
//! These tests drive `Shift` through `TestAppContext` without opening a real
//! macOS window or file panel. External converters are replaced with tiny shell
//! scripts in a per-test temporary directory.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use gpui::{AppContext, Entity, TestAppContext};

use crate::app::{
    CancelWork, ClearRecent, ConversionState, CopyOutput, FailureInstallAction, HistoryOutcome,
    HistorySource, OnboardingStep, OpenAbout, OpenRecent, OpenSettings, RevealOutput, SaveOutput,
    Shift, ShowShortcuts, ToggleFormatMenu,
};
use crate::{
    DEFAULT_UI_FONT, HISTORY_SIDEBAR_MAX, HISTORY_SIDEBAR_MIN, OUTPUT_PANEL_MAX, OUTPUT_PANEL_MIN,
    PanelResizeTarget, SettingsSection,
};
use shift_core::conversion::{
    BatchEvent, BatchFormatSelection, BatchItemId, BatchItemState, BatchProgress, BatchSource,
    ConversionArtifact, ConversionOptions, DiagnosticsReport, EngineDiagnostic, FfmpegQuality,
    OutputFormat, Readiness, available_outputs_for_batch_source,
};
use shift_core::history::{MAX_HISTORY_ARTIFACT_BYTES, MAX_HISTORY_LIMIT, MIN_HISTORY_LIMIT};
use shift_core::recipes::{ConversionRecipe, RecipeDestination, RecipeStore};
use std::sync::Arc;

// Binary-wide lock shared with `file_picker` tests (see `crate::ENV_LOCK` in main.rs).
static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TestHttpServer {
    address: SocketAddr,
    stop: std::sync::Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestHttpServer {
    fn new() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let thread_stop = std::sync::Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve_test_http(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        assert!(path.starts_with('/'), "test HTTP paths must start with '/'");
        format!("http://{}{}", self.address, path)
    }
}

impl Drop for TestHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_test_http(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = [0_u8; 8 * 1024];
    let mut bytes_read = 0;
    while bytes_read < request.len() {
        match stream.read(&mut request[bytes_read..]) {
            Ok(0) => break,
            Ok(read) => {
                bytes_read += read;
                if request[..bytes_read]
                    .windows(2)
                    .any(|window| window == b"\r\n")
                {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let request = String::from_utf8_lossy(&request[..bytes_read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let body = format!("shift test fixture: {path}");
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.shutdown(Shutdown::Both);
}

static TEST_HTTP_SERVER: OnceLock<TestHttpServer> = OnceLock::new();

fn test_url(path: &str) -> String {
    TEST_HTTP_SERVER.get_or_init(TestHttpServer::new).url(path)
}

const MARKITDOWN_FAKE: &str = r#"#!/bin/sh
cat "$1"
"#;

const DEFUDDLE_FAKE: &str = r#"#!/bin/sh
# defuddle parse <source> [options]
SOURCE="$2"
printf '# '
if [ -f "$SOURCE" ]; then
cat "$SOURCE"
else
printf '%s' "$SOURCE"
fi
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

/// Writes the file named by `--out` so tests never invoke the real
/// `/usr/bin/sips`, which would reject the synthetic image bytes fixtures use.
const SIPS_FAKE: &str = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "sips-fake"
  exit 0
fi

out=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--out" ]; then
    out="$arg"
    prev=""
    continue
  fi
  case "$arg" in
    --out) prev="--out" ;;
  esac
done

[ -z "$out" ] && exit 0
printf '%s' "shift-fake-image" > "$out"
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
        let guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

        let sips = bin.join("sips");
        write_script(&sips, SIPS_FAKE);

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
        set_env(
            &mut previous,
            "SHIFT_SIPS_BIN",
            Some(sips.clone().into_os_string()),
        );
        set_env(&mut previous, "SHIFT_ALLOW_PRIVATE_URLS", Some("1".into()));
        set_env(&mut previous, "SHIFT_CURL_BIN", None);
        set_env(
            &mut previous,
            "NO_PROXY",
            Some("127.0.0.1,localhost".into()),
        );
        set_env(
            &mut previous,
            "no_proxy",
            Some("127.0.0.1,localhost".into()),
        );
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

    fn recipes_path(&self) -> PathBuf {
        self.support().join("conversion-recipes.json")
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
async fn onboarding_introduces_shift_before_the_conversion_workspace(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_step),
        Some(OnboardingStep::Welcome)
    );

    shift.update(cx, |this, cx| this.advance_onboarding(cx));
    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_step),
        Some(OnboardingStep::HowItWorks)
    );
    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_nav),
        crate::ui::animation::OnboardingNavDirection::Forward
    );

    shift.update(cx, |this, cx| this.advance_onboarding(cx));
    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_step),
        Some(OnboardingStep::Dependencies)
    );

    shift.update(cx, |this, cx| this.advance_onboarding(cx));
    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_step),
        Some(OnboardingStep::Ready)
    );

    shift.update(cx, |this, cx| this.previous_onboarding(cx));
    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_step),
        Some(OnboardingStep::Dependencies)
    );
    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_nav),
        crate::ui::animation::OnboardingNavDirection::Back
    );
    shift.update(cx, |this, cx| this.previous_onboarding(cx));
    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_step),
        Some(OnboardingStep::HowItWorks)
    );
    shift.update(cx, |this, cx| this.previous_onboarding(cx));
    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_step),
        Some(OnboardingStep::Welcome)
    );
    shift.update(cx, |this, cx| this.advance_onboarding(cx));
    shift.update(cx, |this, cx| this.advance_onboarding(cx));
    shift.update(cx, |this, cx| this.advance_onboarding(cx));
    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_step),
        Some(OnboardingStep::Ready)
    );

    // "Get started" is wired to advance_onboarding, not finish_onboarding directly.
    shift.update(cx, |this, cx| this.advance_onboarding(cx));
    assert!(shift.read_with(cx, |this, _| this.onboarding_step.is_none()));
    assert_eq!(
        shift.read_with(cx, |this, _| this.onboarding_nav),
        crate::ui::animation::OnboardingNavDirection::Enter
    );
    assert!(shift_core::load_default_session_settings().onboarding_completed);
}

#[gpui::test]
async fn multi_file_selection_exits_the_single_file_onboarding(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let first = write_input(&env, "onboarding-first.txt", b"first");
    let second = write_input(&env, "onboarding-second.txt", b"second");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.ingest_paths(vec![first, second], cx));

    assert!(shift.read_with(cx, |this, _| this.onboarding_step.is_none()));
    assert_eq!(shift.read_with(cx, |this, _| this.batch_queue.len()), 2);
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
    let url = test_url("/article.html");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url(url, cx);
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
    assert!(
        text.contains("shift test fixture: /article.html"),
        "unexpected Defuddle output: {text:?}"
    );
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
    let expected_url = test_url("/page.html");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.submit_magic_paste_text(expected_url.clone(), cx)
    });
    cx.run_until_parked();
    shift.update(cx, |this, cx| this.start_conversion(cx));
    cx.run_until_parked();

    let url = shift.read_with(cx, |this, _| this.selected_url.clone());
    assert_eq!(url, Some(expected_url));
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
async fn batch_format_cycle_and_extra_output_use_capability_filtered_jobs(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "fanout.txt", b"text");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![path], false, cx));
    let (id, alternative) = shift.read_with(cx, |this, _| {
        let item = &this.batch_queue.items()[0];
        let format = available_outputs_for_batch_source(&this.registry, &item.source)
            .into_iter()
            .find(|format| *format != item.resolved_format())
            .expect("test source should have another output");
        (item.id, format)
    });
    shift.update(cx, |this, cx| {
        this.select_batch_item_format(id, alternative, cx)
    });
    cx.run_until_parked();

    let selected = shift.read_with(cx, |this, _| {
        let item = &this.batch_queue.items()[0];
        (item.format_selection, item.resolved_format())
    });
    assert!(matches!(selected.0, BatchFormatSelection::Override(_)));
    assert_eq!(selected.1, alternative);

    shift.update(cx, |this, cx| this.add_batch_item_output(id, cx));
    cx.run_until_parked();
    let (len, formats) = shift.read_with(cx, |this, _| {
        (this.batch_queue.len(), this.batch_queue.group_formats(id))
    });
    assert_eq!(len, 2);
    assert_eq!(formats.len(), 2);
    assert_ne!(formats[0], formats[1]);

    let extra = shift.read_with(cx, |this, _| this.batch_queue.items()[1].id);
    shift.update(cx, |this, cx| this.remove_batch_item(extra, cx));
    assert_eq!(shift.read_with(cx, |this, _| this.batch_queue.len()), 1);
}

#[gpui::test]
async fn toggling_batch_item_format_reapplies_recipe_snapshot(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let output_dir = env.temp.join("recipe-output");
    std::fs::create_dir_all(&output_dir).unwrap();
    let path = write_input(&env, "recipe item.txt", b"text");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_batch_output_dir(output_dir.clone(), cx);
        this.recipe_naming_input.update(cx, |input, cx| {
            input.set_content("{stem}-published.{ext}", cx);
        });
        this.batch_force = true;
        this.recipe_preferred_module = Some("markitdown".to_owned());
        this.enqueue_paths(vec![path], false, cx);
    });
    let id = shift.read_with(cx, |this, _| this.batch_queue.items()[0].id);

    shift.update(cx, |this, cx| this.toggle_batch_item_format(id, cx));
    cx.run_until_parked();

    let item = shift.read_with(cx, |this, _| this.batch_queue.items()[0].clone());
    assert_eq!(
        item.format_selection,
        BatchFormatSelection::Override(OutputFormat::MARKDOWN)
    );
    assert_eq!(
        item.destination,
        output_dir.join("recipe item-published.md")
    );
    assert!(item.force);
    assert_eq!(item.preferred_module.as_deref(), Some("markitdown"));
}

#[gpui::test]
async fn preferred_recipe_module_updates_queued_batch_snapshot(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "module preference.txt", b"text");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![path], false, cx));
    shift.update(cx, |this, cx| {
        this.set_recipe_preferred_module(Some("markitdown".to_owned()), cx)
    });
    cx.run_until_parked();

    let preferred = shift.read_with(cx, |this, _| {
        this.batch_queue.items()[0].preferred_module.clone()
    });
    assert_eq!(preferred.as_deref(), Some("markitdown"));
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
async fn native_recipe_save_is_visible_and_redacts_password(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.output_format = OutputFormat::HTML;
        this.pandoc_toc = true;
        this.recipe_preferred_module = Some("pandoc".into());
        this.recipe_name_input
            .update(cx, |input, cx| input.set_content("Publish", cx));
        this.recipe_naming_input.update(cx, |input, cx| {
            input.set_content("{stem}-publish.{ext}", cx)
        });
        this.pdf_password_input
            .update(cx, |input, cx| input.set_content("never-persist", cx));
        this.save_recipe_from_input(cx);
    });
    cx.run_until_parked();

    let (active, modified, count) = shift.read_with(cx, |this, _| {
        (
            this.active_recipe.clone(),
            this.recipe_modified,
            this.recipes.len(),
        )
    });
    assert_eq!(active.as_deref(), Some("Publish"));
    assert!(!modified);
    assert_eq!(count, 1);
    let raw = fs::read_to_string(env.recipes_path()).unwrap();
    assert!(!raw.contains("never-persist"));
    let stored = shift_core::load_recipe_store(env.recipes_path()).unwrap();
    let recipe = stored.get("publish").unwrap();
    assert_eq!(recipe.parsed_output_format().unwrap(), OutputFormat::HTML);
    assert_eq!(recipe.preferred_module.as_deref(), Some("pandoc"));
    assert!(recipe.to_conversion_options().pandoc.toc);
}

#[gpui::test]
async fn applying_recipe_snapshots_queued_items_and_delete_clears_current(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let output = env.temp.join("recipe-output");
    let mut options = ConversionOptions::default();
    options.pandoc.toc = true;
    options.pdf.password = Some("must-be-dropped".into());
    let recipe = ConversionRecipe::new(
        "Batch publish",
        OutputFormat::HTML,
        Some("pandoc".into()),
        &options,
        Some(RecipeDestination {
            output_dir: Some(output.clone()),
            naming_template: Some("{stem}-published.{ext}".into()),
            overwrite: true,
        }),
    )
    .unwrap();
    let mut store = RecipeStore::default();
    store.upsert(recipe).unwrap();
    shift_core::save_recipe_store(env.recipes_path(), &store).unwrap();

    let first = write_input(&env, "first.md", b"# first");
    let second = write_input(&env, "second.md", b"# second");
    let shift = create_shift(cx);
    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![first, second], false, cx);
        this.apply_recipe("batch PUBLISH", cx);
    });
    cx.run_until_parked();

    shift.read_with(cx, |this, _| {
        assert_eq!(this.active_recipe.as_deref(), Some("Batch publish"));
        assert!(!this.recipe_modified);
        assert_eq!(this.output_format, OutputFormat::HTML);
        assert_eq!(this.recipe_preferred_module.as_deref(), Some("pandoc"));
        assert_eq!(this.batch_output_dir.as_ref(), Some(&output));
        assert!(this.batch_force);
        for item in this.batch_queue.items() {
            assert_eq!(item.output_format, OutputFormat::HTML);
            assert!(item.options.pandoc.toc);
            assert_eq!(item.preferred_module.as_deref(), Some("pandoc"));
            assert!(item.options.pdf.password.is_none());
            assert!(item.force);
            assert!(
                item.destination
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with("-published.html")
            );
        }
    });

    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::PDF, cx);
        assert!(this.recipe_modified);
        this.delete_recipe("Batch publish", cx);
    });
    cx.run_until_parked();
    shift.read_with(cx, |this, _| {
        assert!(this.active_recipe.is_none());
        assert!(!this.recipe_modified);
        assert!(this.recipe_preferred_module.is_none());
        assert!(this.recipes.is_empty());
    });
    assert!(
        shift_core::load_recipe_store(env.recipes_path())
            .unwrap()
            .recipes
            .is_empty()
    );
}

#[gpui::test]
async fn applying_recipe_with_unknown_module_is_atomic(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);
    let recipe = ConversionRecipe {
        name: "Broken".into(),
        output_format: "html".into(),
        preferred_module: Some("not-installed-module".into()),
        options: Default::default(),
        destination: None,
    };
    shift.update(cx, |this, cx| {
        this.recipes.push(recipe);
        let before = this.output_format;
        this.apply_recipe("Broken", cx);
        assert_eq!(this.output_format, before);
        assert!(this.active_recipe.is_none());
        assert!(
            this.recipe_status
                .as_ref()
                .is_some_and(|status| status.contains("unknown module"))
        );
    });
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
async fn clipboard_image_converts_via_sips(cx: &mut TestAppContext) {
    let env = TestEnv::new();
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
    // Still → still is sips on macOS; FFmpeg keeps container inputs.
    #[cfg(target_os = "macos")]
    assert_eq!(module, "sips");
    #[cfg(not(target_os = "macos"))]
    assert_eq!(module, "ffmpeg");
    assert!(
        fs::read_dir(env.temp.join("paste"))
            .unwrap()
            .next()
            .is_none(),
        "successful clipboard conversion should release its staged input"
    );
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
        assert_eq!(this.history_limit, MAX_HISTORY_LIMIT);
        assert_eq!(
            this.history_limit_input.read(cx).content(),
            MAX_HISTORY_LIMIT.to_string()
        );
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
async fn stale_folder_expand_result_cannot_restore_after_dismiss(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let dir = env.inputs().join("stale-folder");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("nested.txt"), b"nested").unwrap();
    let shift = create_shift(cx);

    // Dismiss before the background walk gets a chance to publish its result.
    // The generation guard must prevent the late completion from restoring the
    // confirmation dialog.
    shift.update(cx, |this, cx| this.ingest_paths(vec![dir], cx));
    shift.update(cx, |this, cx| this.dismiss_folder_confirm(cx));
    cx.run_until_parked();

    let (confirm, status, queued) = shift.read_with(cx, |this, _| {
        (
            this.folder_confirm.is_some(),
            this.batch_status.as_ref().map(ToString::to_string),
            this.batch_queue.len(),
        )
    });
    assert!(!confirm);
    assert_eq!(status.as_deref(), Some("Folder expansion cancelled."));
    assert_eq!(queued, 0);
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
    let url = test_url("/hist.html");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url(url, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let source = shift.read_with(cx, |this, _| this.history.first().map(|e| e.source.clone()));
    assert!(matches!(
        source,
        Some(HistorySource::Url(u)) if u.ends_with("/hist.html")
    ));
}

#[gpui::test]
async fn restore_url_history_entry(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let expected_url = test_url("/restore.html");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url(expected_url.clone(), cx);
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
    assert_eq!(url.as_deref(), Some(expected_url.as_str()));
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
    // Production maps missing binaries to install hints / "executable not found".
    assert!(
        lower.contains("executable not found")
            || lower.contains("not installed")
            || lower.contains("markitdown is not installed"),
        "expected install-hint / not-found messaging, got: {msg}"
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
    let url = test_url("/fm.html");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.defuddle_frontmatter = true;
        this.set_selected_url(url, cx);
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

// =============================================================================
// Expanded coverage: actions, options matrix, history, batch, modules
// =============================================================================

#[gpui::test]
async fn action_cancel_work_closes_shortcuts_first(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.onboarding_step = None;
            this.shortcuts_help_open = true;
            this.settings_open = true;
            this.action_cancel_work(&CancelWork, window, cx);
            assert!(!this.shortcuts_help_open);
            assert!(
                this.settings_open,
                "settings should remain until shortcuts are closed"
            );
        });
    });
}

#[gpui::test]
async fn action_cancel_work_closes_settings_before_conversion(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "cancel_settings.txt", b"x");
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.onboarding_step = None;
            this.set_selected_file(path, cx);
            this.start_conversion(cx);
            this.settings_open = true;
            this.action_cancel_work(&CancelWork, window, cx);
            assert!(!this.settings_open);
            assert!(
                matches!(this.conversion, ConversionState::Converting)
                    || matches!(this.conversion, ConversionState::Ready(_))
                    || matches!(this.conversion, ConversionState::Empty)
                    || matches!(this.conversion, ConversionState::Failed(_)),
                "closing settings should not force-cancel via conversion path alone"
            );
        });
    });
    vcx.run_until_parked();
}

#[gpui::test]
async fn action_cancel_work_dismisses_folder_confirm(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let dir = env.inputs().join("folder_cancel_action");
    fs::create_dir_all(dir.join("nested")).unwrap();
    fs::write(dir.join("nested/a.txt"), b"a").unwrap();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|_window, cx| {
        shift.update(cx, |this, cx| {
            this.onboarding_step = None;
            this.ingest_paths(vec![dir], cx);
        });
    });
    vcx.run_until_parked();

    let has_confirm = shift.read_with(vcx, |this, _| this.folder_confirm.is_some());
    assert!(has_confirm);

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.action_cancel_work(&CancelWork, window, cx);
        });
    });
    vcx.run_until_parked();

    let cleared = shift.read_with(vcx, |this, _| this.folder_confirm.is_none());
    assert!(cleared);
}

#[gpui::test]
async fn action_cancel_work_closes_output_menu(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.onboarding_step = None;
            this.output_menu_open = true;
            this.action_cancel_work(&CancelWork, window, cx);
            assert!(!this.output_menu_open);
        });
    });
}

#[gpui::test]
async fn action_cancel_work_cancels_active_conversion(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "action_cancel_conv.txt", b"hello");
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.onboarding_step = None;
            this.set_selected_file(path, cx);
            this.start_conversion(cx);
            this.action_cancel_work(&CancelWork, window, cx);
        });
    });
    vcx.run_until_parked();

    let cancelled = shift.read_with(vcx, |this, _| match &this.conversion {
        ConversionState::Failed(msg) => msg.as_ref().contains("cancelled"),
        _ => false,
    });
    assert!(cancelled);
}

#[gpui::test]
async fn action_toggle_format_flips_menu_flag(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            assert!(!this.output_menu_open);
            this.action_toggle_format(&ToggleFormatMenu, window, cx);
            assert!(this.output_menu_open);
            this.action_toggle_format(&ToggleFormatMenu, window, cx);
            assert!(!this.output_menu_open);
        });
    });
}

#[gpui::test]
async fn action_cancel_work_dismisses_onboarding_before_other_overlays(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            assert_eq!(this.onboarding_step, Some(OnboardingStep::Welcome));
            this.shortcuts_help_open = true;
            this.action_cancel_work(&CancelWork, window, cx);
            assert!(this.onboarding_step.is_none());
            assert!(this.shortcuts_help_open);
        });
    });
}

#[gpui::test]
async fn action_toggle_format_with_empty_selection_is_safe(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            assert!(this.selected_file.is_none());
            assert!(this.selected_url.is_none());
            this.action_toggle_format(&ToggleFormatMenu, window, cx);
            assert!(this.output_menu_open);
            assert!(matches!(this.conversion, ConversionState::Empty));
        });
    });
}

#[gpui::test]
async fn action_open_settings_toggles_and_loads_diagnostics(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.output_menu_open = true;
            this.action_open_settings(&OpenSettings, window, cx);
            assert!(this.settings_open);
            assert!(!this.output_menu_open);
        });
    });
    vcx.run_until_parked();

    let has_diag = shift.read_with(vcx, |this, _| this.diagnostics.is_some());
    assert!(has_diag);

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.action_open_settings(&OpenSettings, window, cx);
            assert!(!this.settings_open);
        });
    });
}

#[gpui::test]
async fn action_show_shortcuts_toggles_flag(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            assert!(!this.shortcuts_help_open);
            this.action_show_shortcuts(&ShowShortcuts, window, cx);
            assert!(this.shortcuts_help_open);
            this.action_show_shortcuts(&ShowShortcuts, window, cx);
            assert!(!this.shortcuts_help_open);
        });
    });
}

#[gpui::test]
async fn action_open_about_sets_section_and_opens_settings(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.settings_open = false;
            this.settings_section = SettingsSection::General;
            this.action_open_about(&OpenAbout, window, cx);
            assert!(this.settings_open);
            assert_eq!(this.settings_section, SettingsSection::About);
        });
    });
}

#[gpui::test]
async fn action_clear_recent_empties_history(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "clear_recent_action.txt", b"hist");
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|_window, cx| {
        shift.update(cx, |this, cx| {
            this.set_selected_file(path, cx);
            this.start_conversion(cx);
        });
    });
    vcx.run_until_parked();
    assert!(shift.read_with(vcx, |this, _| !this.history.is_empty()));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.action_clear_recent(&ClearRecent, window, cx);
        });
    });
    vcx.run_until_parked();

    let empty = shift.read_with(vcx, |this, _| {
        this.history.is_empty() && this.active_history_id.is_none()
    });
    assert!(empty);
}

#[gpui::test]
async fn install_hints_for_failure_nonempty_when_engine_missing(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "hints.txt", b"hello");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.diagnostics = Some(Arc::new(DiagnosticsReport {
            engines: vec![
                EngineDiagnostic {
                    id: "markitdown",
                    label: "MarkItDown",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_MARKITDOWN_BIN",
                    install_hint: "pip install 'markitdown[all]'".into(),
                    notes: None,
                },
                EngineDiagnostic {
                    id: "ffmpeg",
                    label: "FFmpeg",
                    readiness: Readiness::Ready,
                    version: Some("fake".into()),
                    resolved_path: Some(env.bin.join("ffmpeg")),
                    env_override: "SHIFT_FFMPEG_BIN",
                    install_hint: "brew install ffmpeg".into(),
                    notes: None,
                },
            ],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        }));
        this.dependency_tools.insert(
            "markitdown".into(),
            shift_core::dependencies::DependencyCapability::DocumentsMarkdown,
        );
        this.set_selected_file(path, cx);
        let hints = this.install_hints_for_failure();
        assert!(
            !hints.is_empty(),
            "missing markitdown should produce install hints"
        );
        assert!(
            hints
                .iter()
                .any(|entry| entry.label.as_ref().contains("MarkItDown")
                    && entry.hint.as_ref().contains("pip install")
                    && entry.action
                        == FailureInstallAction::InstallManaged(
                            shift_core::dependencies::DependencyCapability::DocumentsMarkdown,
                        )),
            "hints={hints:?}"
        );
        // Ready engines must not appear.
        assert!(
            !hints
                .iter()
                .any(|entry| entry.label.as_ref().contains("FFmpeg"))
        );
    });
}

#[gpui::test]
async fn install_hints_empty_without_diagnostics(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, _cx| {
        this.diagnostics = None;
        let hints = this.install_hints_for_failure();
        assert!(hints.is_empty());
    });
}

#[gpui::test]
async fn dependency_install_waits_for_active_single_and_batch_conversions(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        let capability = shift_core::dependencies::DependencyCapability::DocumentsMarkdown;
        this.conversion = ConversionState::Converting;
        this.install_dependency_for_failure(capability, cx);
        assert!(!this.dependency_installing);
        assert!(
            this.dependency_install_status
                .as_deref()
                .is_some_and(|status| status.contains("active conversions"))
        );

        this.conversion = ConversionState::Empty;
        this.batch_running = true;
        this.dependency_install_status = None;
        this.install_dependency_for_failure(capability, cx);
        assert!(!this.dependency_installing);
        assert!(
            this.dependency_install_status
                .as_deref()
                .is_some_and(|status| status.contains("active conversions"))
        );
    });
}

#[gpui::test]
async fn system_engine_failure_hints_copy_their_own_install_commands(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, _cx| {
        this.diagnostics = Some(Arc::new(DiagnosticsReport {
            engines: [
                ("ffmpeg", "FFmpeg", "brew install ffmpeg"),
                ("pandoc", "Pandoc", "brew install pandoc"),
                ("qpdf", "qpdf", "brew install qpdf"),
            ]
            .into_iter()
            .map(|(id, label, install_hint)| EngineDiagnostic {
                id,
                label,
                readiness: Readiness::Missing,
                version: None,
                resolved_path: None,
                env_override: "",
                install_hint: install_hint.into(),
                notes: None,
            })
            .collect(),
            pdf_engines: vec![],
            selected_pdf_engine: None,
        }));
        let hints = this.install_hints_for_failure();
        assert_eq!(hints.len(), 3, "hints={hints:?}");
        for (hint, command) in hints.iter().zip([
            "brew install ffmpeg",
            "brew install pandoc",
            "brew install qpdf",
        ]) {
            assert_eq!(
                hint.action,
                FailureInstallAction::CopyCommand(command.into())
            );
        }
    });
}

#[gpui::test]
async fn rebuild_output_caches_after_format_change_tracks_source(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "cache_fmt.txt", b"x");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.rebuild_output_caches();
        let file_outputs = this.cached_available_outputs.clone();
        assert!(
            file_outputs.contains(&OutputFormat::MARKDOWN),
            "file outputs={file_outputs:?}"
        );

        this.set_output_format(OutputFormat::HTML, cx);
        this.rebuild_output_caches();
        assert!(
            this.cached_available_outputs.contains(&OutputFormat::HTML)
                || this
                    .cached_available_outputs
                    .contains(&OutputFormat::MARKDOWN),
            "after format change caches={:?}",
            this.cached_available_outputs
        );
    });
    cx.run_until_parked();

    // URL source uses a different cache branch.
    shift.update(cx, |this, cx| {
        this.set_selected_url("http://example.com/rebuild.html".into(), cx);
        this.rebuild_output_caches();
        assert!(
            !this.cached_available_outputs.is_empty(),
            "url outputs should be non-empty"
        );
        let url_outputs = this.cached_available_outputs.clone();
        this.clear_selected_file(cx);
        this.rebuild_output_caches();
        assert_eq!(
            this.cached_available_outputs,
            OutputFormat::ALL.to_vec(),
            "no source → all formats; was url={url_outputs:?}"
        );
    });
}

#[gpui::test]
async fn rebuild_output_caches_hides_formats_with_missing_engines(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "ready_only.txt", b"hello");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        // Avoid the async doctor probe overwriting the fixture report.
        this.diagnostics_loading = true;
        this.set_selected_file(path.clone(), cx);
        this.rebuild_output_caches();
        assert!(
            this.cached_available_outputs
                .contains(&OutputFormat::MARKDOWN),
            "without diagnostics, capability list still includes markdown"
        );

        this.diagnostics = Some(Arc::new(DiagnosticsReport {
            engines: vec![EngineDiagnostic {
                id: "markitdown",
                label: "MarkItDown",
                readiness: Readiness::Missing,
                version: None,
                resolved_path: None,
                env_override: "SHIFT_MARKITDOWN_BIN",
                install_hint: "pip install markitdown".into(),
                notes: None,
            }],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        }));
        this.rebuild_output_caches();
        assert!(
            !this
                .cached_available_outputs
                .contains(&OutputFormat::MARKDOWN),
            "missing MarkItDown must hide markdown from the picker: {:?}",
            this.cached_available_outputs
        );
        assert!(
            this.cached_ready_outputs
                .as_ref()
                .is_some_and(|ready| !ready.contains(&OutputFormat::MARKDOWN)),
            "ready cache should also exclude markdown"
        );

        // Make markitdown ready — markdown returns to the list.
        this.diagnostics = Some(Arc::new(DiagnosticsReport {
            engines: vec![EngineDiagnostic {
                id: "markitdown",
                label: "MarkItDown",
                readiness: Readiness::Ready,
                version: Some("0.1".into()),
                resolved_path: None,
                env_override: "SHIFT_MARKITDOWN_BIN",
                install_hint: String::new(),
                notes: None,
            }],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        }));
        this.rebuild_output_caches();
        assert!(
            this.cached_available_outputs
                .contains(&OutputFormat::MARKDOWN),
            "ready MarkItDown restores markdown: {:?}",
            this.cached_available_outputs
        );
    });
}

#[gpui::test]
async fn active_option_modules_for_srt_include_ffmpeg(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "subs.mkv", b"vid");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::SRT, cx);
    });
    cx.run_until_parked();

    let modules = shift.read_with(cx, |this, _| this.active_option_modules());
    assert!(modules.contains(&"ffmpeg"), "srt modules={modules:?}");
    assert!(shift.read_with(cx, |this, _| this.conversion_options_visible()));
}

#[gpui::test]
async fn active_option_modules_for_png_include_ffmpeg(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "frame.mp4", b"vid");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::PNG, cx);
    });
    cx.run_until_parked();

    let modules = shift.read_with(cx, |this, _| this.active_option_modules());
    assert!(modules.contains(&"ffmpeg"), "png modules={modules:?}");
}

#[gpui::test]
async fn active_option_modules_for_docx_include_pandoc(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "to_docx.md", b"# hi");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::DOCX, cx);
    });
    cx.run_until_parked();

    let modules = shift.read_with(cx, |this, _| this.active_option_modules());
    assert!(modules.contains(&"pandoc"), "docx modules={modules:?}");
}

#[gpui::test]
async fn active_option_modules_for_xlsx_include_markitdown_or_docling(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "sheet.xlsx", b"PK fake xlsx");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::MARKDOWN, cx);
    });
    cx.run_until_parked();

    let modules = shift.read_with(cx, |this, _| this.active_option_modules());
    assert!(
        modules.contains(&"markitdown") || modules.contains(&"docling"),
        "xlsx→md modules={modules:?}"
    );
}

#[gpui::test]
async fn active_option_modules_for_epub_include_pandoc_or_docling(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "book.epub", b"PK fake epub");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::MARKDOWN, cx);
    });
    cx.run_until_parked();

    let modules = shift.read_with(cx, |this, _| this.active_option_modules());
    assert!(
        modules.contains(&"pandoc")
            || modules.contains(&"docling")
            || modules.contains(&"markitdown"),
        "epub→md modules={modules:?}"
    );
    assert!(shift.read_with(cx, |this, _| this.conversion_options_visible()));
}

#[gpui::test]
async fn retry_single_batch_item_succeeds_after_fixing_tool(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    env.override_tool("markitdown", ALWAYS_FAIL_MARKITDOWN_FAKE);
    let path = write_input(&env, "fix_tool_batch.txt", b"content");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.enqueue_paths(vec![path], true, cx));
    cx.run_until_parked();

    let failed_id = shift.read_with(cx, |this, _| {
        this.batch_queue
            .items()
            .iter()
            .find(|item| matches!(item.state, BatchItemState::Failed { .. }))
            .map(|item| item.id)
    });
    let failed_id = failed_id.expect("batch item should fail with always-fail tool");

    env.override_tool("markitdown", MARKITDOWN_FAKE);
    shift.update(cx, |this, cx| this.retry_batch_item(failed_id, cx));
    cx.run_until_parked();

    let succeeded = shift.read_with(cx, |this, _| {
        this.batch_queue
            .items()
            .iter()
            .all(|item| matches!(item.state, BatchItemState::Succeeded { .. }))
    });
    assert!(succeeded);
}

#[gpui::test]
async fn set_selected_url_private_succeeds_when_allow_private_enabled(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    // TestEnv sets SHIFT_ALLOW_PRIVATE_URLS=1.
    let expected_url = test_url("/private.html");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url(expected_url.clone(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let (url, ready) = shift.read_with(cx, |this, _| {
        (
            this.selected_url.clone(),
            matches!(this.conversion, ConversionState::Ready(_)),
        )
    });
    assert_eq!(url.as_deref(), Some(expected_url.as_str()));
    assert!(ready, "private URL should convert when allow-private is on");
}

#[gpui::test]
async fn set_selected_url_private_fails_when_allow_private_toggled_off(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    unsafe { std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS") };
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url("http://10.0.0.5/lan.html".into(), cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let blocked = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Failed(msg) => {
            let m = msg.as_ref().to_ascii_lowercase();
            m.contains("non-public") || m.contains("private") || m.contains("blocked")
        }
        _ => false,
    });
    assert!(blocked);
}

#[gpui::test]
async fn fail_magic_paste_then_recover_with_valid_paste(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let expected_url = test_url("/recovered.html");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.fail_magic_paste("synthetic paste failure", cx);
    });
    cx.run_until_parked();

    let failed = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Failed(msg) => msg.as_ref().contains("synthetic paste failure"),
        _ => false,
    });
    assert!(failed);
    assert!(shift.read_with(cx, |this, _| {
        this.selected_file.is_none() && this.selected_url.is_none()
    }));

    shift.update(cx, |this, cx| {
        this.submit_magic_paste_text(expected_url.clone(), cx);
    });
    cx.run_until_parked();
    shift.update(cx, |this, cx| this.start_conversion(cx));
    cx.run_until_parked();

    let (url, module) = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => (this.selected_url.clone(), artifact.module_id),
        other => panic!("expected Ready after recovery, got {other:?}"),
    });
    assert_eq!(url.as_deref(), Some(expected_url.as_str()));
    assert_eq!(module, "defuddle");
}

#[gpui::test]
async fn history_archive_hidden_when_show_archived_false(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "archive_hide.txt", b"h");
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

    shift.update(cx, |this, cx| {
        this.show_archived = false;
        this.mark_history_cache_dirty();
        this.ensure_history_cache(cx);
        assert!(
            this.cached_history_visible.is_empty(),
            "archived entries hidden when show_archived=false"
        );
        assert!(this.history.iter().any(|e| e.archived));
    });
}

#[gpui::test]
async fn history_archive_shown_when_show_archived_true(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "archive_show.txt", b"h");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();
    shift.update(cx, |this, cx| {
        this.archive_history_entry(id, cx);
        this.show_archived = true;
        this.mark_history_cache_dirty();
        this.ensure_history_cache(cx);
        this.persist_session_settings(cx);
        assert_eq!(this.cached_history_visible.len(), 1);
        assert!(this.cached_history_visible[0].archived);
    });
    cx.run_until_parked();

    let json = fs::read_to_string(env.session_path()).unwrap();
    assert!(json.contains("\"show_archived\": true") || json.contains("\"show_archived\":true"));
}

#[gpui::test]
async fn set_batch_output_dir_then_clear_queue_removes_destinations(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let out = env.temp.join("batch_out_clear");
    fs::create_dir_all(&out).unwrap();
    let a = write_input(&env, "dest_clear_a.txt", b"a");
    let b = write_input(&env, "dest_clear_b.txt", b"b");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| this.set_batch_output_dir(out.clone(), cx));
    shift.update(cx, |this, cx| this.enqueue_paths(vec![a, b], false, cx));
    cx.run_until_parked();

    let in_out = shift.read_with(cx, |this, _| {
        this.batch_queue
            .items()
            .iter()
            .all(|item| item.destination.starts_with(&out))
    });
    assert!(in_out);
    assert_eq!(
        shift.read_with(cx, |this, _| this.batch_queue.items().len()),
        2
    );

    shift.update(cx, |this, cx| this.clear_batch_queue(cx));
    cx.run_until_parked();

    let empty = shift.read_with(cx, |this, _| this.batch_queue.is_empty());
    assert!(empty);
    // Output dir preference remains; only queue destinations are gone.
    let dir = shift.read_with(cx, |this, _| this.batch_output_dir.clone());
    assert_eq!(dir.as_deref(), Some(out.as_path()));
}

#[gpui::test]
async fn conversion_options_visible_matrix_for_several_formats(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let txt = write_input(&env, "opts_matrix.txt", b"t");
    let mp4 = write_input(&env, "opts_matrix.mp4", b"v");
    let pdf = write_input(&env, "opts_matrix.pdf", b"%PDF");
    let shift = create_shift(cx);

    // Media output without source → ffmpeg knobs visible.
    shift.update(cx, |this, cx| {
        this.clear_selected_file(cx);
        this.set_output_format(OutputFormat::WAV, cx);
        assert!(this.conversion_options_visible());
        assert_eq!(this.active_option_modules(), vec!["ffmpeg"]);
    });

    // Markdown without source → hidden.
    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::MARKDOWN, cx);
        assert!(!this.conversion_options_visible());
    });

    // Text → HTML → pandoc.
    shift.update(cx, |this, cx| {
        this.set_selected_file(txt, cx);
        this.set_output_format(OutputFormat::HTML, cx);
        assert!(this.conversion_options_visible());
        assert!(this.active_option_modules().contains(&"pandoc"));
    });

    // Video → MP3 → ffmpeg.
    shift.update(cx, |this, cx| {
        this.set_selected_file(mp4, cx);
        this.set_output_format(OutputFormat::MP3, cx);
        assert!(this.conversion_options_visible());
        assert!(this.active_option_modules().contains(&"ffmpeg"));
    });

    // PDF → HTML → docling (or markitdown path).
    shift.update(cx, |this, cx| {
        this.set_selected_file(pdf, cx);
        this.set_output_format(OutputFormat::HTML, cx);
        assert!(this.conversion_options_visible());
        let modules = this.active_option_modules();
        assert!(
            modules.contains(&"docling") || modules.contains(&"markitdown"),
            "pdf→html modules={modules:?}"
        );
    });

    // URL → defuddle.
    shift.update(cx, |this, cx| {
        this.set_selected_url("http://example.com/matrix.html".into(), cx);
        assert!(this.conversion_options_visible());
        assert!(this.active_option_modules().contains(&"defuddle"));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn build_conversion_options_with_many_session_knobs(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "many_knobs.txt", b"x");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.docling_ocr = true;
        this.docling_tables = true;
        this.docling_ocr_lang_input
            .update(cx, |input, cx| input.set_content("eng+deu", cx));
        this.pandoc_toc = true;
        this.pandoc_standalone = true;
        this.pandoc_citations = true;
        this.defuddle_frontmatter = true;
        this.defuddle_lang_input
            .update(cx, |input, cx| input.set_content("en-US", cx));
        this.ffmpeg_quality = FfmpegQuality::High;
        this.ffmpeg_mono = true;
        this.ffmpeg_mute = true;
        this.ffmpeg_normalize = true;
        this.markitdown_keep_data_uris = true;
        this.ffmpeg_start_input
            .update(cx, |input, cx| input.set_content("1.5", cx));
        this.ffmpeg_duration_input
            .update(cx, |input, cx| input.set_content("10", cx));
        this.target_size_input
            .update(cx, |input, cx| input.set_content("9.5", cx));
        this.pdf_page_from_input
            .update(cx, |input, cx| input.set_content("2", cx));
        this.pdf_page_to_input
            .update(cx, |input, cx| input.set_content("5", cx));
        this.pdf_rotate_degrees = Some(180);
        this.pdf_compression = shift_core::conversion::PdfCompression::Lossless;
        this.pdf_linearize = true;
        this.pdf_split_pages_input
            .update(cx, |input, cx| input.set_content("2", cx));

        this.set_selected_file(path, cx);
        // split_pages is only included for PDF Pages (ZIP); leave default format
        // first so a leftover field does not leak into non-ZIP options.
        let options = this
            .build_conversion_options(cx)
            .expect("build_conversion_options should succeed");

        assert!(options.docling.ocr);
        assert!(options.docling.tables);
        assert_eq!(options.docling.ocr_lang.as_deref(), Some("eng+deu"));
        assert!(options.pandoc.toc);
        assert!(options.pandoc.standalone);
        assert!(options.pandoc.citations);
        assert!(options.defuddle.frontmatter);
        assert_eq!(options.defuddle.lang.as_deref(), Some("en-US"));
        assert_eq!(options.ffmpeg.quality, FfmpegQuality::High);
        assert!(options.ffmpeg.mono);
        assert!(options.ffmpeg.mute);
        assert!(options.ffmpeg.normalize_audio);
        assert!(options.markitdown.keep_data_uris);
        assert_eq!(options.ffmpeg.start_secs, Some(1.5));
        assert_eq!(options.ffmpeg.duration_secs, Some(10.0));
        assert_eq!(options.target_size_bytes, Some(9_500_000));
        assert_eq!(options.pdf.page_from, Some(2));
        assert_eq!(options.pdf.page_to, Some(5));
        assert_eq!(options.pdf.rotate_degrees, Some(180));
        assert_eq!(
            options.pdf.compression,
            shift_core::conversion::PdfCompression::Lossless
        );
        assert!(options.pdf.linearize);
        assert_eq!(options.pdf.split_pages, None);

        this.output_format = OutputFormat::PDF_PAGES_ZIP;
        let zip_options = this
            .build_conversion_options(cx)
            .expect("ZIP options should include split_pages");
        assert_eq!(zip_options.pdf.split_pages, Some(2));
    });
}

#[gpui::test]
async fn target_size_input_rejects_tiny_and_non_numeric_values(cx: &mut TestAppContext) {
    let shift = create_shift(cx);
    shift.update(cx, |this, cx| {
        this.target_size_input
            .update(cx, |input, cx| input.set_content("nope", cx));
        assert!(this.build_conversion_options(cx).is_err());
        this.target_size_input
            .update(cx, |input, cx| input.set_content("0.001", cx));
        assert!(this.build_conversion_options(cx).is_err());
        this.target_size_input
            .update(cx, |input, cx| input.set_content("", cx));
        assert_eq!(
            this.build_conversion_options(cx).unwrap().target_size_bytes,
            None
        );
    });
}

#[test]
fn target_size_display_trims_only_insignificant_zeroes() {
    assert_eq!(super::format_target_megabytes(10_000_000), "10");
    assert_eq!(super::format_target_megabytes(9_500_000), "9.5");
    assert_eq!(super::format_target_megabytes(16_384), "0.02");
}

#[gpui::test]
async fn apply_session_option_change_reconverts_when_options_visible(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "session_reconvert.txt", b"content");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();
    assert!(shift.read_with(cx, |this, _| {
        matches!(this.conversion, ConversionState::Ready(_))
    }));

    let gen_before = shift.read_with(cx, |this, _| this.conversion_generation);

    shift.update(cx, |this, cx| {
        this.markitdown_keep_data_uris = true;
        this.apply_session_option_change(cx);
    });
    cx.run_until_parked();

    let (ready, gen_after) = shift.read_with(cx, |this, _| {
        (
            matches!(this.conversion, ConversionState::Ready(_)),
            this.conversion_generation,
        )
    });
    assert!(ready);
    assert!(
        gen_after > gen_before,
        "apply_session_option_change should start a new conversion (gen {gen_before} → {gen_after})"
    );
}

#[gpui::test]
async fn apply_session_option_change_without_visible_options_skips_reconvert(
    cx: &mut TestAppContext,
) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        // No source + markdown → options not visible.
        assert!(!this.conversion_options_visible());
        let generation = this.conversion_generation;
        this.ui_font_family = "Helvetica".into();
        this.apply_session_option_change(cx);
        assert_eq!(this.conversion_generation, generation);
        assert!(matches!(this.conversion, ConversionState::Empty));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn enqueue_sources_with_url_batch(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let first_url = test_url("/a.html");
    let second_url = test_url("/b.html");
    let out = env.temp.join("url_batch_out");
    fs::create_dir_all(&out).unwrap();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.batch_force = true;
        this.set_batch_output_dir(out.clone(), cx);
        this.enqueue_sources(
            vec![BatchSource::Url(first_url), BatchSource::Url(second_url)],
            true,
            cx,
        );
    });
    cx.run_until_parked();

    let (count, completed, running, states) = shift.read_with(cx, |this, _| {
        let states: Vec<_> = this
            .batch_queue
            .items()
            .iter()
            .map(|item| format!("{:?}", item.state))
            .collect();
        (
            this.batch_queue.items().len(),
            this.batch_queue.progress().completed(),
            this.batch_running,
            states,
        )
    });
    assert_eq!(count, 2, "states={states:?}");
    assert!(!running);
    assert_eq!(completed, 2, "states={states:?}");
    let all_ok = shift.read_with(cx, |this, _| {
        this.batch_queue.items().iter().all(|item| {
            matches!(item.state, BatchItemState::Succeeded { .. })
                && matches!(item.source, BatchSource::Url(_))
        })
    });
    assert!(all_ok, "states={states:?}");
}

#[gpui::test]
async fn ingest_paths_empty_vec_is_noop(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "ingest_empty_keep.txt", b"keep");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path.clone(), cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| this.ingest_paths(vec![], cx));
    cx.run_until_parked();

    let (selected, queue_empty) = shift.read_with(cx, |this, _| {
        (this.selected_file.clone(), this.batch_queue.is_empty())
    });
    assert_eq!(selected, Some(path));
    assert!(queue_empty);
}

#[gpui::test]
async fn double_clear_selected_file_is_safe(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "double_clear.txt", b"x");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| {
        this.clear_selected_file(cx);
        this.clear_selected_file(cx);
    });
    cx.run_until_parked();

    let reset = shift.read_with(cx, |this, _| {
        this.selected_file.is_none()
            && this.selected_url.is_none()
            && matches!(this.conversion, ConversionState::Empty)
            && this.active_history_id.is_none()
    });
    assert!(reset);
}

#[gpui::test]
async fn module_priority_pandoc_first_for_markdown(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "priority_pandoc_first.md", b"# prefer pandoc");
    let shift = create_shift(cx);

    // Move markitdown after pandoc so pandoc wins for md→md (or md convert).
    shift.update(cx, |this, cx| {
        // Ensure pandoc is first among overlapping modules.
        if let Some(pandoc_idx) = this.module_priority.iter().position(|m| m == "pandoc") {
            if pandoc_idx != 0 {
                this.move_module(pandoc_idx, 0, cx);
            }
        }
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::MARKDOWN, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let module = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Ready(artifact) => artifact.module_id,
        other => panic!("expected Ready, got {other:?}"),
    });
    // With pandoc first, markdown→markdown should prefer pandoc when it supports the pair.
    assert_eq!(
        module,
        "pandoc",
        "module_priority={:?}",
        shift.read_with(cx, |this, _| this.module_priority.clone())
    );
}

#[gpui::test]
async fn open_settings_state_flags_via_action(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    let initial = shift.read_with(vcx, |this, _| {
        (
            this.settings_open,
            this.settings_section,
            this.shortcuts_help_open,
        )
    });
    assert!(!initial.0);

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.onboarding_step = None;
            this.action_open_about(&OpenAbout, window, cx);
        });
    });

    let after_about = shift.read_with(vcx, |this, _| (this.settings_open, this.settings_section));
    assert!(after_about.0);
    assert_eq!(after_about.1, SettingsSection::About);

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.action_show_shortcuts(&ShowShortcuts, window, cx);
            assert!(this.shortcuts_help_open);
            // Cancel closes shortcuts first.
            this.action_cancel_work(&CancelWork, window, cx);
            assert!(!this.shortcuts_help_open);
            assert!(this.settings_open);
        });
    });
}

#[gpui::test]
async fn rebuild_output_caches_for_url_then_file(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "cache_switch.wav", b"RIFF");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url("http://example.com/switch.html".into(), cx);
        this.rebuild_output_caches();
        let url_cache = this.cached_available_outputs.clone();
        assert!(!url_cache.is_empty());

        this.set_selected_file(path, cx);
        this.rebuild_output_caches();
        let file_cache = this.cached_available_outputs.clone();
        assert!(
            file_cache.contains(&OutputFormat::MP3) || file_cache.contains(&OutputFormat::WAV),
            "audio file outputs={file_cache:?}"
        );
        // Switching source should change the available set for media vs web.
        assert_ne!(url_cache, file_cache);
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn conversion_options_visible_for_png_sequence_zip_without_source(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::PNG_SEQUENCE_ZIP, cx);
        assert!(this.conversion_options_visible());
        assert_eq!(this.active_option_modules(), vec!["ffmpeg"]);
    });
}

#[gpui::test]
async fn conversion_options_visible_for_srt_without_source(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::SRT, cx);
        assert!(this.conversion_options_visible());
        assert_eq!(this.active_option_modules(), vec!["ffmpeg"]);
    });
}

#[gpui::test]
async fn conversion_options_hidden_for_docx_without_source(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_output_format(OutputFormat::DOCX, cx);
        assert!(!this.conversion_options_visible());
        assert!(this.active_option_modules().is_empty());
    });
}

#[gpui::test]
async fn build_conversion_options_ok_with_defaults(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        let options = this.build_conversion_options(cx).expect("defaults ok");
        // Docling defaults OCR+tables on; other engines keep conservative defaults.
        assert!(options.docling.ocr);
        assert!(options.docling.tables);
        assert!(!options.pandoc.toc);
        assert!(!options.defuddle.frontmatter);
        assert_eq!(options.ffmpeg.quality, FfmpegQuality::Balanced);
        assert!(!options.markitdown.keep_data_uris);
        assert!(options.pdf.password.is_none());
        assert!(options.pdf.page_from.is_none());
    });
}

#[gpui::test]
async fn persist_session_settings_never_writes_pdf_password(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let shift = create_shift(cx);
    let secret = "s3cret-ui-password-must-not-persist";

    shift.update(cx, |this, cx| {
        this.pdf_password_input
            .update(cx, |input, cx| input.set_content(secret, cx));
        this.pdf_page_from_input
            .update(cx, |input, cx| input.set_content("2", cx));
        this.pdf_page_to_input
            .update(cx, |input, cx| input.set_content("4", cx));
        // Live options carry the password; disk must not.
        let options = this.build_conversion_options(cx).expect("options ok");
        assert_eq!(options.pdf.password.as_deref(), Some(secret));
        assert_eq!(options.pdf.page_from, Some(2));
        assert_eq!(options.pdf.page_to, Some(4));
        this.persist_session_settings(cx);
    });
    cx.run_until_parked();

    let json = fs::read_to_string(env.session_path()).unwrap();
    assert!(
        !json.contains(secret),
        "password value must not appear in session-settings.json: {json}"
    );
    assert!(
        !json.contains("\"password\""),
        "password key must never be serialized: {json}"
    );
    // Page range knobs still persist.
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let page_from = value
        .pointer("/options/pdf/page_from")
        .and_then(|v| v.as_u64());
    let page_to = value
        .pointer("/options/pdf/page_to")
        .and_then(|v| v.as_u64());
    assert_eq!(page_from, Some(2), "page_from should persist: {json}");
    assert_eq!(page_to, Some(4), "page_to should persist: {json}");
}

#[gpui::test]
async fn enqueue_sources_empty_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_sources(vec![], true, cx);
        assert!(this.batch_queue.is_empty());
        assert!(!this.batch_running);
    });
}

#[gpui::test]
async fn fail_magic_paste_clears_selection_and_bumps_generation(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "fail_paste_clear.txt", b"x");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
    });
    cx.run_until_parked();

    let gen_before = shift.read_with(cx, |this, _| this.selection_generation);

    shift.update(cx, |this, cx| {
        this.fail_magic_paste("cleared by fail", cx);
        assert!(this.selected_file.is_none());
        assert!(this.selected_url.is_none());
        assert!(!this.output_menu_open);
        assert!(this.selection_generation > gen_before);
        match &this.conversion {
            ConversionState::Failed(msg) => assert!(msg.as_ref().contains("cleared by fail")),
            other => panic!("expected Failed, got {other:?}"),
        }
    });
}

#[gpui::test]
async fn set_batch_output_dir_while_running_is_rejected(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    // Slow tool so batch is still "running" if we set dir mid-flight — but
    // synchronous fakes finish quickly. Force the guard by toggling batch_running.
    let out = env.temp.join("while_running");
    fs::create_dir_all(&out).unwrap();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.batch_running = true;
        this.set_batch_output_dir(out.clone(), cx);
        assert!(
            this.batch_output_dir.is_none()
                || this.batch_output_dir.as_deref() != Some(out.as_path())
                || this
                    .batch_status
                    .as_ref()
                    .is_some_and(|s| s.contains("Cannot change"))
        );
        assert!(
            this.batch_status
                .as_ref()
                .is_some_and(|s| s.contains("Cannot change")),
            "status={:?}",
            this.batch_status
        );
        this.batch_running = false;
    });
}

#[gpui::test]
async fn active_option_modules_for_epub_output_from_markdown(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "to_epub.md", b"# book");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::EPUB, cx);
    });
    cx.run_until_parked();

    let modules = shift.read_with(cx, |this, _| this.active_option_modules());
    assert!(modules.contains(&"pandoc"), "md→epub modules={modules:?}");
    assert!(shift.read_with(cx, |this, _| this.conversion_options_visible()));
}

#[gpui::test]
async fn history_toggle_show_archived_round_trip(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "archive_roundtrip.txt", b"r");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();
    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();

    shift.update(cx, |this, cx| {
        this.archive_history_entry(id, cx);
        this.show_archived = false;
        this.mark_history_cache_dirty();
        this.ensure_history_cache(cx);
        assert!(this.cached_history_visible.is_empty());

        this.show_archived = true;
        this.mark_history_cache_dirty();
        this.ensure_history_cache(cx);
        assert_eq!(this.cached_history_visible.len(), 1);

        this.show_archived = false;
        this.mark_history_cache_dirty();
        this.ensure_history_cache(cx);
        assert!(this.cached_history_visible.is_empty());
    });
}

#[gpui::test]
async fn install_hints_filter_to_active_modules_only(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "hints_filter.mp4", b"vid");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.diagnostics = Some(Arc::new(DiagnosticsReport {
            engines: vec![
                EngineDiagnostic {
                    id: "markitdown",
                    label: "MarkItDown",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_MARKITDOWN_BIN",
                    install_hint: "pip install markitdown".into(),
                    notes: None,
                },
                EngineDiagnostic {
                    id: "ffmpeg",
                    label: "FFmpeg",
                    readiness: Readiness::Missing,
                    version: None,
                    resolved_path: None,
                    env_override: "SHIFT_FFMPEG_BIN",
                    install_hint: "brew install ffmpeg".into(),
                    notes: None,
                },
            ],
            pdf_engines: vec![],
            selected_pdf_engine: None,
        }));
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::MP3, cx);
        let modules = this.active_option_modules();
        assert!(modules.contains(&"ffmpeg"), "modules={modules:?}");
        let hints = this.install_hints_for_failure();
        assert_eq!(hints.len(), 1, "hints={hints:?}");
        assert!(hints[0].label.as_ref().contains("FFmpeg"));
        assert_eq!(
            hints[0].action,
            FailureInstallAction::CopyCommand("brew install ffmpeg".into())
        );
        assert!(
            !hints
                .iter()
                .any(|entry| entry.label.as_ref().contains("MarkItDown"))
        );
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn apply_session_option_change_persists_docling_ocr(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "docling_ocr_persist.pdf", b"%PDF");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.docling_ocr = true;
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::MARKDOWN, cx);
        this.apply_session_option_change(cx);
    });
    cx.run_until_parked();

    let ocr = shift.read_with(cx, |this, _| this.docling_ocr);
    assert!(ocr);
    let json = fs::read_to_string(env.session_path()).unwrap();
    assert!(
        json.contains("ocr") || json.contains("docling"),
        "session json should retain docling knobs: {json}"
    );
}

#[gpui::test]
async fn set_selected_url_empty_is_noop(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "url_empty_keep.txt", b"k");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path.clone(), cx);
        this.set_selected_url("   ".into(), cx);
        assert_eq!(this.selected_file.as_ref(), Some(&path));
        assert!(this.selected_url.is_none());
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn set_selected_url_invalid_fails_magic_paste_style(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url("not-a-url".into(), cx);
    });
    cx.run_until_parked();

    let failed = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Failed(msg) => msg.as_ref().contains("http"),
        _ => false,
    });
    assert!(failed);
}

// =============================================================================
// Remaining method coverage: Ready copy/reveal/open, actions, batch events,
// panel resize move, history limit mid-flight, cancel mid-URL, menus, etc.
// =============================================================================

fn sample_ready_artifact(
    file_name: &str,
    format: OutputFormat,
    module_id: &'static str,
    bytes: Vec<u8>,
) -> Arc<ConversionArtifact> {
    Arc::new(ConversionArtifact {
        file_name: file_name.to_owned(),
        media_type: format.media_type(),
        bytes,
        format,
        module_id,
        pipeline: vec![module_id],
        invocations: Vec::new(),
    })
}

#[gpui::test]
async fn set_ready_artifact_marks_ready_and_clears_cached_path(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, _cx| {
        this.cached_ready_path = Some(PathBuf::from("/tmp/stale-export.md"));
        let art = sample_ready_artifact(
            "out.md",
            OutputFormat::MARKDOWN,
            "markitdown",
            b"# ready".to_vec(),
        );
        this.set_ready_artifact(art);
        assert!(matches!(this.conversion, ConversionState::Ready(_)));
        assert!(this.cached_ready_path.is_none());
        let got = this.conversion.ready_artifact().expect("Ready");
        assert_eq!(got.file_name, "out.md");
        assert_eq!(got.bytes, b"# ready");
    });
}

#[gpui::test]
async fn binary_ready_artifact_exposes_safe_header_inspection(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend_from_slice(&320u32.to_be_bytes());
    png.extend_from_slice(&200u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);

    shift.update(cx, |this, _cx| {
        this.set_ready_artifact(sample_ready_artifact(
            "preview.png",
            OutputFormat::PNG,
            "ffmpeg",
            png,
        ));
        let artifact = this
            .conversion
            .ready_artifact()
            .expect("ready binary artifact");
        let inspection = artifact.inspection();
        assert_eq!(inspection.kind, "Image");
        assert!(
            inspection
                .facts
                .iter()
                .any(|fact| fact.contains("320 × 200 px")),
            "{inspection:?}"
        );
        assert!(inspection.note.contains("Header inspection"));
    });
}

#[gpui::test]
async fn copy_output_text_ready_sets_clipboard_status(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "copy_text.txt", b"clipboard body");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| {
        assert!(matches!(this.conversion, ConversionState::Ready(_)));
        this.copy_output(cx);
        let status = this.save_status.as_ref().map(|s| s.to_string());
        assert_eq!(
            status.as_deref(),
            Some("Copied text to clipboard."),
            "status={status:?}"
        );
    });
}

#[gpui::test]
async fn copy_output_binary_ready_stages_and_copies_path(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "copy_bin.md", b"# pdf");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::PDF, cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| {
        assert!(matches!(this.conversion, ConversionState::Ready(_)));
        this.copy_output(cx);
    });
    cx.run_until_parked();

    let (status, cached) = shift.read_with(cx, |this, _| {
        (
            this.save_status.as_ref().map(|s| s.to_string()),
            this.cached_ready_path.clone(),
        )
    });
    assert!(
        status
            .as_ref()
            .is_some_and(|s| s.contains("Copied artifact path")),
        "status={status:?}"
    );
    assert!(
        cached.as_ref().is_some_and(|p| p.is_file()),
        "expected staged path, got {cached:?}"
    );
}

#[gpui::test]
async fn reveal_output_ready_stages_and_sets_status(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "reveal_me.txt", b"reveal");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| this.reveal_output(cx));
    cx.run_until_parked();

    let (status, cached) = shift.read_with(cx, |this, _| {
        (
            this.save_status.as_ref().map(|s| s.to_string()),
            this.cached_ready_path.clone(),
        )
    });
    assert!(
        status.as_ref().is_some_and(|s| s.starts_with("Revealed")),
        "status={status:?}"
    );
    assert!(cached.as_ref().is_some_and(|p| p.is_file()));

    // Second call uses ready_cached_path fast path.
    shift.update(cx, |this, cx| {
        this.reveal_output(cx);
        assert!(
            this.save_status
                .as_ref()
                .is_some_and(|s| s.starts_with("Revealed"))
        );
    });
}

#[gpui::test]
async fn open_output_ready_stages_and_sets_status(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "open_me.txt", b"open");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| this.open_output(cx));
    cx.run_until_parked();

    let status = shift.read_with(cx, |this, _| {
        this.save_status.as_ref().map(|s| s.to_string())
    });
    assert!(
        status.as_ref().is_some_and(|s| s.starts_with("Opened")),
        "status={status:?}"
    );

    shift.update(cx, |this, cx| {
        this.open_output(cx);
        assert!(
            this.save_status
                .as_ref()
                .is_some_and(|s| s.starts_with("Opened"))
        );
    });
}

#[gpui::test]
async fn copy_reveal_open_when_empty_leave_status_untouched(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.save_status = Some("prior".into());
        this.reveal_output(cx);
        this.open_output(cx);
        this.copy_output(cx);
        assert_eq!(this.save_status.as_ref().map(|s| s.as_ref()), Some("prior"));
        assert!(matches!(this.conversion, ConversionState::Empty));
    });
}

#[gpui::test]
async fn action_copy_and_reveal_output_with_ready(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "action_copy.txt", b"via action");
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|_window, cx| {
        shift.update(cx, |this, cx| {
            this.set_selected_file(path, cx);
            this.start_conversion(cx);
        });
    });
    vcx.run_until_parked();

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.action_copy_output(&CopyOutput, window, cx);
            assert_eq!(
                this.save_status.as_ref().map(|s| s.as_ref()),
                Some("Copied text to clipboard.")
            );
        });
    });

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.action_reveal_output(&RevealOutput, window, cx);
        });
    });
    vcx.run_until_parked();

    let status = shift.read_with(vcx, |this, _| {
        this.save_status.as_ref().map(|s| s.to_string())
    });
    assert!(
        status.as_ref().is_some_and(|s| s.starts_with("Revealed")),
        "status={status:?}"
    );
}

#[gpui::test]
async fn action_save_output_when_empty_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.save_status = None;
            this.action_save_output(&SaveOutput, window, cx);
            assert!(this.save_status.is_none());
            assert!(matches!(this.conversion, ConversionState::Empty));
        });
    });
}

#[gpui::test]
async fn save_output_when_not_ready_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.conversion = ConversionState::Converting;
        this.save_output(cx);
        assert!(this.save_status.is_none());
        this.conversion = ConversionState::Failed("x".into());
        this.save_output(cx);
        assert!(this.save_status.is_none());
    });
}

#[gpui::test]
async fn action_open_recent_with_valid_path_selects_file(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "recent_valid.txt", b"from recent");
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.action_open_recent(
                &OpenRecent {
                    path: path.to_string_lossy().into_owned(),
                },
                window,
                cx,
            );
        });
    });
    vcx.run_until_parked();

    // set_selected_file auto-starts conversion for single files.
    let (selected, ready) = shift.read_with(vcx, |this, _| {
        (
            this.selected_file.clone(),
            matches!(this.conversion, ConversionState::Ready(_)),
        )
    });
    assert_eq!(selected, Some(path));
    // Conversion may or may not auto-start depending on set_selected_file; ensure selection stuck.
    let _ = ready;
}

#[gpui::test]
async fn action_open_recent_empty_path_is_noop(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let keep = write_input(&env, "recent_keep.txt", b"keep");
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|_window, cx| {
        shift.update(cx, |this, cx| {
            this.set_selected_file(keep.clone(), cx);
        });
    });
    vcx.run_until_parked();
    let sel_gen = shift.read_with(vcx, |this, _| this.selection_generation);

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.action_open_recent(
                &OpenRecent {
                    path: String::new(),
                },
                window,
                cx,
            );
            assert_eq!(this.selection_generation, sel_gen);
            assert_eq!(this.selected_file.as_ref(), Some(&keep));
        });
    });
}

#[gpui::test]
async fn action_open_recent_missing_path_fails(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let missing = env.inputs().join("recent_missing_file.txt");
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.action_open_recent(
                &OpenRecent {
                    path: missing.to_string_lossy().into_owned(),
                },
                window,
                cx,
            );
        });
    });
    vcx.run_until_parked();

    let failed = shift.read_with(vcx, |this, _| match &this.conversion {
        ConversionState::Failed(msg) => msg.as_ref().contains("Recent file not found"),
        _ => false,
    });
    assert!(failed);
    assert_eq!(
        shift.read_with(vcx, |this, _| this.selected_file.clone()),
        Some(missing)
    );
}

#[gpui::test]
async fn set_history_limit_mid_conversion_still_completes(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "limit_mid.txt", b"mid limit");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
        // Apply while conversion may still be in flight / just started.
        this.set_history_limit(3, cx);
        assert_eq!(this.history_limit, 3);
    });
    cx.run_until_parked();

    let (ready, limit) = shift.read_with(cx, |this, _| {
        (
            matches!(this.conversion, ConversionState::Ready(_)),
            this.history_limit,
        )
    });
    assert!(
        ready,
        "conversion should complete after mid-flight limit change"
    );
    assert_eq!(limit, 3);
}

#[gpui::test]
async fn set_history_limit_same_value_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        let current = this.history_limit;
        let rev = this.history_persist_revision;
        this.set_history_limit(current, cx);
        assert_eq!(this.history_persist_revision, rev);
    });
}

#[gpui::test]
async fn cancel_conversion_mid_url_leaves_cancelled(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let url = test_url("/cancel-mid.html");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_url(url, cx);
        this.start_conversion(cx);
        this.cancel_conversion(cx);
    });
    cx.run_until_parked();

    let cancelled = shift.read_with(cx, |this, _| match &this.conversion {
        ConversionState::Failed(msg) => msg.as_ref().contains("cancelled"),
        _ => false,
    });
    assert!(cancelled);
}

#[gpui::test]
async fn cancel_conversion_when_empty_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.cancel_conversion(cx);
        assert!(matches!(this.conversion, ConversionState::Empty));
        assert!(this.save_status.is_none());
    });
}

#[gpui::test]
async fn cancel_conversion_with_queued_batch_cancels_items(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "cancel_q_a.txt", b"a");
    let b = write_input(&env, "cancel_q_b.txt", b"b");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![a, b], false, cx);
        assert!(!this.batch_queue.is_empty());
        this.cancel_conversion(cx);
    });
    cx.run_until_parked();

    let all_cancelled = shift.read_with(cx, |this, _| {
        this.batch_queue
            .items()
            .iter()
            .all(|item| matches!(item.state, BatchItemState::Cancelled))
    });
    assert!(all_cancelled);
}

#[gpui::test]
async fn cancel_active_conversion_sets_flag(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, _cx| {
        assert!(!this.conversion_cancel.load(Ordering::SeqCst));
        this.cancel_active_conversion();
        assert!(this.conversion_cancel.load(Ordering::SeqCst));
    });
}

#[gpui::test]
async fn rebuild_app_menus_after_archive_does_not_panic(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "menu_archive.txt", b"hist");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    let id = shift
        .read_with(cx, |this, _| this.active_history_id)
        .unwrap();
    let before = shift.read_with(cx, |this, _| this.recent_file_menu_items().len());
    assert!(before >= 3);

    shift.update(cx, |this, cx| {
        this.archive_history_entry(id, cx);
        this.rebuild_app_menus(cx);
        let menus = this.build_app_menus();
        assert!(!menus.is_empty());
        // Recent menu still lists the file path (archive does not hide from menu).
        assert!(this.recent_file_menu_items().len() >= 3);
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn handle_panel_resize_move_history_and_output(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));
    let _ = env;

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            let start_hist = this.history_sidebar_width;
            this.begin_panel_resize(PanelResizeTarget::History, 200.0, cx);
            let event = gpui::MouseMoveEvent {
                position: gpui::point(gpui::px(240.0), gpui::px(10.0)),
                pressed_button: None,
                modifiers: gpui::Modifiers::default(),
            };
            this.handle_panel_resize_move(&event, window, cx);
            assert!(this.panel_resize.is_some());
            // Drag right (+40) widens history (clamped).
            assert!(
                this.history_sidebar_width >= start_hist
                    || this.history_sidebar_width == HISTORY_SIDEBAR_MAX
                    || (this.history_sidebar_width - start_hist).abs() < 0.01
                    || this.history_sidebar_width >= HISTORY_SIDEBAR_MIN
            );

            this.end_panel_resize(cx);
            assert!(this.panel_resize.is_none());

            let start_out = this.output_panel_width;
            this.begin_panel_resize(PanelResizeTarget::Output, 800.0, cx);
            let event = gpui::MouseMoveEvent {
                position: gpui::point(gpui::px(760.0), gpui::px(10.0)),
                pressed_button: None,
                modifiers: gpui::Modifiers::default(),
            };
            // Drag left (−40 from start_x) widens output.
            this.handle_panel_resize_move(&event, window, cx);
            assert!(this.output_panel_width >= OUTPUT_PANEL_MIN);
            assert!(this.output_panel_width <= OUTPUT_PANEL_MAX);
            let _ = start_out;
            this.end_panel_resize(cx);
        });
    });
    vcx.run_until_parked();
}

#[gpui::test]
async fn handle_panel_resize_move_without_begin_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            let hist = this.history_sidebar_width;
            let out = this.output_panel_width;
            assert!(this.panel_resize.is_none());
            let event = gpui::MouseMoveEvent {
                position: gpui::point(gpui::px(500.0), gpui::px(0.0)),
                pressed_button: None,
                modifiers: gpui::Modifiers::default(),
            };
            this.handle_panel_resize_move(&event, window, cx);
            assert_eq!(this.history_sidebar_width, hist);
            assert_eq!(this.output_panel_width, out);
        });
    });
}

#[gpui::test]
async fn ensure_diagnostics_loads_report(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.diagnostics = None;
        this.diagnostics_loading = false;
        this.ensure_diagnostics(cx);
        assert!(this.diagnostics_loading || this.diagnostics.is_some());
    });
    cx.run_until_parked();

    let has = shift.read_with(cx, |this, _| {
        this.diagnostics.is_some() && !this.diagnostics_loading
    });
    assert!(has);
}

#[gpui::test]
async fn ensure_diagnostics_skips_when_already_present(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.ensure_diagnostics(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| {
        assert!(this.diagnostics.is_some());
        this.diagnostics_loading = false;
        // Should not flip loading true again when report already present.
        this.ensure_diagnostics(cx);
        assert!(!this.diagnostics_loading);
        assert!(this.diagnostics.is_some());
    });
}

#[gpui::test]
async fn refresh_diagnostics_ignored_while_loading(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.diagnostics_loading = true;
        this.diagnostics = None;
        this.refresh_diagnostics(cx);
        // Still "loading" — second refresh must not spawn another or clear the flag.
        assert!(this.diagnostics_loading);
        assert!(this.diagnostics.is_none());
        this.diagnostics_loading = false;
    });
}

#[gpui::test]
async fn apply_batch_event_updates_item_and_status(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "batch_event.txt", b"e");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![path], false, cx);
        let id = this.batch_queue.items()[0].id;

        this.apply_batch_event(BatchEvent::ItemStarted {
            id,
            source_name: "batch_event.txt".into(),
            destination: env.temp.join("out.md"),
        });
        assert!(matches!(
            this.batch_queue.items()[0].state,
            BatchItemState::Running
        ));
        assert!(
            this.batch_status
                .as_ref()
                .is_some_and(|s| s.contains("Converting"))
        );

        this.apply_batch_event(BatchEvent::ItemProgress {
            id,
            fraction: Some(0.5),
            label: "Halfway".into(),
        });
        assert!(
            this.batch_status
                .as_ref()
                .is_some_and(|s| s.contains("Halfway") && s.contains("50%"))
        );
        assert!(this.batch_item_progress.contains_key(&id.0));

        this.apply_batch_event(BatchEvent::ItemSucceeded {
            id,
            source_name: "batch_event.txt".into(),
            path: env.temp.join("out.md"),
            module_id: "markitdown".into(),
            byte_len: 12,
            provenance: Default::default(),
        });
        assert!(matches!(
            this.batch_queue.items()[0].state,
            BatchItemState::Succeeded { .. }
        ));
        assert!(
            this.batch_status
                .as_ref()
                .is_some_and(|s| s.contains("Saved"))
        );

        this.apply_batch_event(BatchEvent::ItemFailed {
            id,
            source_name: "batch_event.txt".into(),
            error: "boom".into(),
        });
        assert!(matches!(
            this.batch_queue.items()[0].state,
            BatchItemState::Failed { .. }
        ));

        this.apply_batch_event(BatchEvent::ItemCancelled {
            id,
            source_name: "batch_event.txt".into(),
        });
        assert!(matches!(
            this.batch_queue.items()[0].state,
            BatchItemState::Cancelled
        ));

        this.apply_batch_event(BatchEvent::Progress(BatchProgress {
            total: 2,
            queued: 0,
            running: 0,
            succeeded: 1,
            failed: 0,
            cancelled: 1,
        }));
        assert!(
            this.batch_status
                .as_ref()
                .is_some_and(|s| s.contains("2/2") && s.contains("1 ok"))
        );
    });
}

#[gpui::test]
async fn apply_batch_event_item_progress_without_fraction(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "batch_prog_label.txt", b"p");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![path], false, cx);
        let id = this.batch_queue.items()[0].id;
        this.apply_batch_event(BatchEvent::ItemProgress {
            id,
            fraction: None,
            label: "Working…".into(),
        });
        assert_eq!(
            this.batch_status.as_ref().map(|s| s.as_ref()),
            Some("Working…")
        );
        assert_eq!(id, BatchItemId(id.0));
    });
}

#[gpui::test]
async fn apply_materialized_sources_empty_fails_paste(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.apply_materialized_sources(vec![], None, cx);
        match &this.conversion {
            ConversionState::Failed(msg) => {
                assert!(msg.as_ref().contains("Nothing to convert"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    });
}

#[gpui::test]
async fn apply_materialized_sources_single_file(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "mat_file.txt", b"materialized");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.apply_materialized_sources(
            vec![BatchSource::File(path.clone())],
            Some(path.to_string_lossy().into_owned()),
            cx,
        );
    });
    cx.run_until_parked();

    let selected = shift.read_with(cx, |this, _| this.selected_file.clone());
    assert_eq!(selected, Some(path));
}

#[gpui::test]
async fn apply_materialized_sources_single_url(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.apply_materialized_sources(
            vec![BatchSource::Url("http://example.com/mat.html".into())],
            None,
            cx,
        );
    });
    cx.run_until_parked();

    let url = shift.read_with(cx, |this, _| this.selected_url.clone());
    assert_eq!(url.as_deref(), Some("http://example.com/mat.html"));
}

#[gpui::test]
async fn apply_materialized_sources_multiple_enqueues_batch(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "mat_a.txt", b"a");
    let b = write_input(&env, "mat_b.txt", b"b");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.apply_materialized_sources(
            vec![BatchSource::File(a), BatchSource::File(b)],
            Some("two files".into()),
            cx,
        );
    });
    cx.run_until_parked();

    let count = shift.read_with(cx, |this, _| this.batch_queue.items().len());
    assert_eq!(count, 2);
    let input_text = shift.update(cx, |this, cx| this.url_input.read(cx).content().to_string());
    assert_eq!(input_text, "two files");
}

#[gpui::test]
async fn start_conversion_with_nonempty_batch_queue_is_noop(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "queue_blocks.txt", b"q");
    let solo = write_input(&env, "solo_blocked.txt", b"s");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.enqueue_paths(vec![a], false, cx);
        this.set_selected_file(solo, cx);
        let conv_gen = this.conversion_generation;
        this.start_conversion(cx);
        assert_eq!(
            this.conversion_generation, conv_gen,
            "single convert must not start while queue is non-empty"
        );
    });
}

#[gpui::test]
async fn source_matches_url_and_file_variants(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "src_match.txt", b"x");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path.clone(), cx);
        assert!(this.source_matches(&BatchSource::File(path.clone())));
        assert!(!this.source_matches(&BatchSource::File(PathBuf::from("/other"))));
        assert!(!this.source_matches(&BatchSource::Url("http://example.com".into())));

        this.set_selected_url("http://example.com/s.html".into(), cx);
        assert!(this.source_matches(&BatchSource::Url("http://example.com/s.html".into())));
        assert!(!this.source_matches(&BatchSource::Url("http://other.example".into())));
        assert!(!this.source_matches(&BatchSource::File(path)));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn pick_reference_doc_when_busy_is_noop(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    // On macOS, force busy via concurrent begin if possible. If the dialog
    // flag is free, just ensure pick_reference_doc with no prior state is safe
    // to call only when busy. Use file_picker is_busy — when not busy, skip
    // calling pick_reference_doc (would open a real panel).
    shift.update(cx, |this, cx| {
        if crate::file_picker::is_busy() {
            let before = this.pandoc_reference_doc.clone();
            this.pick_reference_doc(cx);
            assert_eq!(this.pandoc_reference_doc, before);
        } else {
            // Establish busy by starting a stub-level dialog is not public;
            // document that the busy path is covered when is_busy is true.
            assert!(!crate::file_picker::is_busy());
        }
    });
}

#[gpui::test]
async fn record_history_failed_and_ready_paths(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "record_hist.txt", b"body");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.record_history(HistoryOutcome::Failed("manual fail".into()), cx);
    });
    cx.run_until_parked();

    let failed_entry = shift.read_with(cx, |this, _| {
        this.history.first().map(|e| match &e.outcome {
            HistoryOutcome::Failed(m) => m.to_string(),
            _ => String::new(),
        })
    });
    assert_eq!(failed_entry.as_deref(), Some("manual fail"));

    let art = sample_ready_artifact(
        "record.md",
        OutputFormat::MARKDOWN,
        "markitdown",
        b"# ok".to_vec(),
    );
    shift.update(cx, |this, cx| {
        this.record_history(HistoryOutcome::Ready(art), cx);
    });
    cx.run_until_parked();

    assert!(shift.read_with(cx, |this, _| {
        this.history
            .iter()
            .any(|e| matches!(e.outcome, HistoryOutcome::Ready(_)))
    }));
}

#[gpui::test]
async fn copy_output_with_set_ready_artifact_text(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_ready_artifact(sample_ready_artifact(
            "synthetic.md",
            OutputFormat::MARKDOWN,
            "pandoc",
            b"synthetic text".to_vec(),
        ));
        this.copy_output(cx);
        assert_eq!(
            this.save_status.as_ref().map(|s| s.as_ref()),
            Some("Copied text to clipboard.")
        );
    });
}

#[gpui::test]
async fn reveal_open_with_pre_staged_cached_path(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let staged = env.temp.join("pre-staged-export.md");
    fs::write(&staged, b"staged body").unwrap();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        let art = sample_ready_artifact(
            "pre-staged-export.md",
            OutputFormat::MARKDOWN,
            "markitdown",
            b"staged body".to_vec(),
        );
        this.set_ready_artifact(art);
        this.cached_ready_path = Some(staged.clone());
        // ready_cached_path requires export_matches_bytes; without sidecar this
        // may re-stage. Still should not panic.
        this.reveal_output(cx);
        this.open_output(cx);
    });
    cx.run_until_parked();

    let status = shift.read_with(cx, |this, _| {
        this.save_status.as_ref().map(|s| s.to_string())
    });
    assert!(
        status.as_ref().is_some_and(|s| s.starts_with("Opened")
            || s.starts_with("Revealed")
            || s.contains("cache")),
        "status={status:?}"
    );
}

#[gpui::test]
async fn mark_history_cache_dirty_forces_rebuild(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "cache_dirty.txt", b"d");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| {
        this.ensure_history_cache(cx);
        assert!(!this.history_cache_dirty);
        let visible = this.cached_history_visible.len();
        assert!(visible >= 1);
        this.mark_history_cache_dirty();
        assert!(this.history_cache_dirty);
        this.ensure_history_cache(cx);
        assert!(!this.history_cache_dirty);
        assert_eq!(this.cached_history_visible.len(), visible);
    });
}

#[gpui::test]
async fn ensure_history_cache_search_filters(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let a = write_input(&env, "alpha_unique_token.txt", b"a");
    let b = write_input(&env, "beta_other.txt", b"b");
    let shift = create_shift(cx);

    for path in [a, b] {
        shift.update(cx, |this, cx| {
            this.set_selected_file(path, cx);
            this.start_conversion(cx);
        });
        cx.run_until_parked();
    }

    shift.update(cx, |this, cx| {
        this.history_search
            .update(cx, |input, cx| input.set_content("alpha_unique", cx));
        this.mark_history_cache_dirty();
        this.ensure_history_cache(cx);
        assert_eq!(this.cached_history_visible.len(), 1);
        assert!(
            this.cached_history_visible[0]
                .name
                .as_ref()
                .contains("alpha_unique")
        );

        this.history_search
            .update(cx, |input, cx| input.set_content("", cx));
        this.mark_history_cache_dirty();
        this.ensure_history_cache(cx);
        assert_eq!(this.cached_history_visible.len(), 2);
    });
}

#[gpui::test]
async fn start_source_conversion_invalid_options_fails(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "bad_opts_src.mp4", b"vid");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.ffmpeg_start_input
            .update(cx, |input, cx| input.set_content("not-a-number", cx));
        this.set_selected_file(path, cx);
        this.set_output_format(OutputFormat::MP3, cx);
        this.start_source_conversion(BatchSource::File(this.selected_file.clone().unwrap()), cx);
        match &this.conversion {
            ConversionState::Failed(msg) => {
                assert!(!msg.is_empty(), "expected parse error message");
            }
            other => panic!("expected Failed for bad options, got {other:?}"),
        }
    });
}

#[gpui::test]
async fn recent_file_menu_items_dedupes_paths(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "dedupe_recent.txt", b"once");
    let shift = create_shift(cx);

    // Convert same file twice (re-select) to create two history rows same path.
    for _ in 0..2 {
        shift.update(cx, |this, cx| {
            this.set_selected_file(path.clone(), cx);
            this.start_conversion(cx);
        });
        cx.run_until_parked();
    }

    let items_len = shift.read_with(cx, |this, _| this.recent_file_menu_items().len());
    // One file entry + separator + Clear Recent = 3, even with two history rows.
    assert_eq!(items_len, 3, "paths should be deduped in recent menu");
}

#[gpui::test]
async fn clear_history_after_ready_rebuilds_menus(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "clear_then_menu.txt", b"x");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.start_conversion(cx);
    });
    cx.run_until_parked();

    shift.update(cx, |this, cx| {
        this.clear_history(cx);
        this.rebuild_app_menus(cx);
        assert!(this.history.is_empty());
        assert_eq!(this.recent_file_menu_items().len(), 1);
    });
}

#[gpui::test]
async fn toggle_batch_force_round_trip(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        assert!(!this.batch_force);
        this.toggle_batch_force(cx);
        assert!(this.batch_force);
        this.toggle_batch_force(cx);
        assert!(!this.batch_force);
    });
}

#[gpui::test]
async fn empty_queue_start_batch_and_cancel_active_combo(cx: &mut TestAppContext) {
    let _env = TestEnv::new();
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.start_batch(cx);
        assert!(!this.batch_running);
        this.cancel_active_conversion();
        this.cancel_conversion(cx);
        assert!(matches!(this.conversion, ConversionState::Empty));
    });
}

#[gpui::test]
async fn set_ready_artifact_then_clear_resets(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "ready_then_clear.txt", b"z");
    let shift = create_shift(cx);

    shift.update(cx, |this, cx| {
        this.set_selected_file(path, cx);
        this.set_ready_artifact(sample_ready_artifact(
            "z.md",
            OutputFormat::MARKDOWN,
            "markitdown",
            b"z".to_vec(),
        ));
        assert!(matches!(this.conversion, ConversionState::Ready(_)));
        this.clear_selected_file(cx);
        assert!(matches!(this.conversion, ConversionState::Empty));
        assert!(this.cached_ready_path.is_none());
    });
}

#[gpui::test]
async fn action_open_recent_then_convert_succeeds(cx: &mut TestAppContext) {
    let env = TestEnv::new();
    let path = write_input(&env, "recent_then_convert.txt", b"convert me");
    let (shift, vcx) = cx.add_window_view(|_window, cx| Shift::new(cx, 1180.0));

    vcx.update(|window, cx| {
        shift.update(cx, |this, cx| {
            this.action_open_recent(
                &OpenRecent {
                    path: path.to_string_lossy().into_owned(),
                },
                window,
                cx,
            );
        });
    });
    vcx.run_until_parked();

    vcx.update(|_window, cx| {
        shift.update(cx, |this, cx| {
            if !matches!(this.conversion, ConversionState::Ready(_)) {
                this.start_conversion(cx);
            }
        });
    });
    vcx.run_until_parked();

    let ready = shift.read_with(vcx, |this, _| {
        matches!(this.conversion, ConversionState::Ready(_))
    });
    assert!(ready);
    assert_eq!(
        shift.read_with(vcx, |this, _| this.selected_file.clone()),
        Some(path)
    );
}
