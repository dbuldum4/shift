//! Classify free-form paste / input bar text into conversion sources.
//!
//! Accepts page URLs, local paths, `file://` URLs, remote file downloads,
//! and (via helpers) clipboard image bytes staged as temporary files.

use super::defuddle::{
    block_private_urls, ensure_public_url_fetch_allowed, looks_like_url, redact_url_credentials,
};
use super::process::run_command_cancellable;
use super::sources::supported_input_extensions;
use super::{BatchSource, ConversionError, ConversionRegistry};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use url::Url;

/// Soft cap for remote file downloads (512 MiB).
pub const MAX_REMOTE_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Wall-clock budget for remote downloads (allows large files on moderate links).
pub const REMOTE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Max HTTP redirects when private-URL blocking revalidates each hop.
const MAX_DOWNLOAD_REDIRECTS: u32 = 10;

/// One classified paste token before materialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PasteToken {
    /// Existing or well-formed local file path (may still fail later if missing).
    LocalPath(PathBuf),
    /// http(s) page URL for article extraction (Defuddle).
    PageUrl(String),
    /// http(s) URL whose path looks like a convertible file (download, then convert).
    RemoteFileUrl(String),
}

/// Parsed result of a magic-paste string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MagicPaste {
    Empty,
    Single(PasteToken),
    Multiple(Vec<PasteToken>),
}

impl MagicPaste {
    pub fn tokens(&self) -> &[PasteToken] {
        match self {
            Self::Empty => &[],
            Self::Single(token) => std::slice::from_ref(token),
            Self::Multiple(tokens) => tokens.as_slice(),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }
}

/// Split and classify free-form paste text into tokens.
///
/// Supports whitespace-separated items and double/single-quoted paths with spaces.
/// Does not hit the network; remote file URLs are only classified.
pub fn parse_magic_paste(input: &str) -> MagicPaste {
    let tokens: Vec<PasteToken> = tokenize_paste(input)
        .into_iter()
        .filter_map(|raw| classify_token(&raw))
        .collect();
    match tokens.len() {
        0 => MagicPaste::Empty,
        1 => MagicPaste::Single(tokens.into_iter().next().expect("len checked")),
        _ => MagicPaste::Multiple(tokens),
    }
}

/// Resolve a classified token into a [`BatchSource`], downloading remote files when needed.
///
/// `cancel` may be `None` for short-lived callers; when supplied, it is polled
/// before each network fetch and passed to curl so a running download can be
/// aborted.
pub fn materialize_paste_token(
    token: &PasteToken,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<BatchSource, ConversionError> {
    if cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
    {
        return Err(ConversionError::cancelled());
    }
    match token {
        PasteToken::LocalPath(path) => {
            let path = expand_user_path(path);
            if path.is_dir() {
                return Err(ConversionError::new(format!(
                    "{} is a directory; drop it on the queue or pass --recursive on the CLI",
                    path.display()
                )));
            }
            if !path.is_file() {
                return Err(ConversionError::new(format!(
                    "file not found: {}",
                    path.display()
                )));
            }
            Ok(BatchSource::File(path))
        }
        PasteToken::PageUrl(url) => {
            ensure_url_fetch_allowed(url)?;
            Ok(BatchSource::Url(url.clone()))
        }
        PasteToken::RemoteFileUrl(url) => {
            ensure_url_fetch_allowed(url)?;
            let path = download_remote_file(url, cancel)?;
            Ok(BatchSource::File(path))
        }
    }
}

/// Materialize every token in a magic-paste parse result.
pub fn materialize_magic_paste(
    paste: &MagicPaste,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Vec<BatchSource>, ConversionError> {
    paste
        .tokens()
        .iter()
        .map(|token| materialize_paste_token(token, cancel.clone()))
        .collect()
}

/// Stage raw image bytes from the clipboard as a temporary source file.
///
/// `extension` should be a lowercase extension without the leading dot (e.g. `png`).
/// Rejects formats that no conversion module accepts as input (e.g. bare SVG).
pub fn stage_pasted_image(bytes: &[u8], extension: &str) -> Result<PathBuf, ConversionError> {
    if bytes.is_empty() {
        return Err(ConversionError::new("clipboard image is empty"));
    }
    let ext = extension
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    if ext.is_empty() {
        return Err(ConversionError::new("clipboard image has no format"));
    }
    if !convertible_extensions().contains(&ext) {
        return Err(ConversionError::new(format!(
            "clipboard image format .{ext} is not a supported conversion input"
        )));
    }
    let dir = paste_staging_dir()?;
    let stamp = unique_stamp();
    let path = dir.join(format!("clipboard-image-{stamp}.{ext}"));
    if let Err(error) = fs::write(&path, bytes) {
        let _ = fs::remove_file(&path);
        return Err(ConversionError::new(format!(
            "could not stage clipboard image {}: {error}",
            path.display()
        )));
    }
    Ok(path)
}

/// True when an http(s) URL path ends with a known convertible file extension
/// (excluding HTML, which is treated as a page for Defuddle).
pub fn url_looks_like_remote_file(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url.trim()) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let Some(ext) = path_extension_from_url_path(parsed.path()) else {
        return false;
    };
    if is_page_extension(&ext) {
        return false;
    }
    convertible_extensions().contains(&ext)
}

fn classify_token(raw: &str) -> Option<PasteToken> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // file:// must resolve to a real path or be rejected (no soft-fail to LocalPath).
    if let Ok(parsed) = Url::parse(raw) {
        if parsed.scheme() == "file" {
            return parsed.to_file_path().ok().map(PasteToken::LocalPath);
        }
    }

    if looks_like_url(raw) {
        let url = raw.trim().to_owned();
        if url_looks_like_remote_file(&url) {
            return Some(PasteToken::RemoteFileUrl(url));
        }
        return Some(PasteToken::PageUrl(url));
    }

    // Bare paths: absolute, home-relative, or existing relative.
    if looks_like_path_token(raw) {
        return Some(PasteToken::LocalPath(PathBuf::from(raw)));
    }

    None
}

fn looks_like_path_token(raw: &str) -> bool {
    let path = Path::new(raw);
    if path.is_absolute() || raw.starts_with("~/") || raw == "~" {
        return true;
    }
    // Relative path with separators or a known extension, or an existing file.
    if raw.contains('/') || raw.contains('\\') {
        return true;
    }
    if let Some(ext) = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
    {
        if convertible_extensions().contains(&ext) {
            return true;
        }
    }
    path.exists()
}

fn tokenize_paste(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes: Option<char> = None;

    for ch in input.chars() {
        if let Some(quote) = in_quotes {
            if ch == quote {
                in_quotes = None;
            } else {
                current.push(ch);
            }
            continue;
        }

        match ch {
            '"' | '\'' => {
                in_quotes = Some(ch);
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn expand_user_path(path: &Path) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };
    if raw == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn path_extension_from_url_path(path: &str) -> Option<String> {
    let last = path.rsplit('/').next().unwrap_or("");
    if last.is_empty() || !last.contains('.') {
        return None;
    }
    let ext = last.rsplit('.').next()?.to_ascii_lowercase();
    if ext.is_empty() || ext.len() > 12 {
        return None;
    }
    // Reject query-looking junk that slipped into the path segment.
    if !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext)
}

fn is_page_extension(ext: &str) -> bool {
    matches!(
        ext,
        "html" | "htm" | "php" | "asp" | "aspx" | "jsp" | "cgi" | "shtml"
    )
}

fn convertible_extensions() -> &'static HashSet<String> {
    static CACHE: OnceLock<HashSet<String>> = OnceLock::new();
    CACHE.get_or_init(|| supported_input_extensions(&ConversionRegistry::default()))
}

fn ensure_url_fetch_allowed(url: &str) -> Result<(), ConversionError> {
    ensure_public_url_fetch_allowed(url)
}

fn download_remote_file(
    url: &str,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<PathBuf, ConversionError> {
    let display_url = redact_url_credentials(url);
    let dir = paste_staging_dir()?;
    let name = remote_file_name(url);
    let stamp = unique_stamp();
    let path = dir.join(format!("{stamp}-{name}"));

    let result = if block_private_urls() {
        // Re-validate every hop so a public URL cannot redirect into private space.
        download_with_redirect_revalidation(url, &path, cancel)
    } else {
        download_follow_redirects(url, &path, cancel)
    };

    if let Err(error) = result {
        let _ = fs::remove_file(&path);
        return Err(ConversionError::new(
            error.to_string().replace(url, &display_url),
        ));
    }

    let meta = fs::metadata(&path).map_err(|error| {
        let _ = fs::remove_file(&path);
        ConversionError::new(format!(
            "download succeeded but could not read {}: {error}",
            path.display()
        ))
    })?;
    if meta.len() == 0 {
        let _ = fs::remove_file(&path);
        return Err(ConversionError::new(format!(
            "download of {display_url} produced an empty file"
        )));
    }
    if meta.len() > MAX_REMOTE_FILE_BYTES {
        let _ = fs::remove_file(&path);
        return Err(ConversionError::new(format!(
            "download of {display_url} exceeded size limit ({} bytes)",
            MAX_REMOTE_FILE_BYTES
        )));
    }

    Ok(path)
}

/// Fast path: curl follows redirects (no private-URL policy).
fn download_follow_redirects(
    url: &str,
    path: &Path,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<(), ConversionError> {
    let display_url = redact_url_credentials(url);
    if cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
    {
        return Err(ConversionError::cancelled());
    }
    let mut command = Command::new("curl");
    apply_curl_http_only(&mut command);
    command
        .arg("-fsSL")
        .arg("--max-filesize")
        .arg(MAX_REMOTE_FILE_BYTES.to_string())
        .arg("--connect-timeout")
        .arg("15")
        .arg("-o")
        .arg(path)
        .arg(url);

    let output = run_command_cancellable(command, REMOTE_DOWNLOAD_TIMEOUT, 1024 * 1024, cancel)
        .map_err(|error| {
            if error.is_cancelled() {
                error
            } else {
                ConversionError::new(format!(
                    "could not download {display_url}: {error}. Install curl or open the file locally."
                ))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(ConversionError::new(if detail.is_empty() {
            format!(
                "could not download {display_url} (curl exit {})",
                output.status
            )
        } else if detail.contains("maximum file size exceeded") || detail.contains("max-filesize") {
            format!(
                "could not download {display_url}: exceeded size limit ({} bytes)",
                MAX_REMOTE_FILE_BYTES
            )
        } else if detail.contains("timed out") || detail.contains("Timeout") {
            format!("could not download {display_url}: download timed out")
        } else {
            format!("could not download {display_url}: {detail}")
        }));
    }
    Ok(())
}

/// Safer path when private URLs are blocked (default): no auto-follow; re-check each Location.
fn download_with_redirect_revalidation(
    url: &str,
    path: &Path,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<(), ConversionError> {
    let mut current = url.to_string();
    for hop in 0..=MAX_DOWNLOAD_REDIRECTS {
        let display_current = redact_url_credentials(&current);
        if cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            let _ = fs::remove_file(path);
            return Err(ConversionError::cancelled());
        }
        ensure_url_fetch_allowed(&current)?;

        let header_path = path.with_extension(format!("hdr{hop}"));
        let mut command = Command::new("curl");
        // No -L / max-redirs 0: surface 3xx so we can validate the next hop.
        // No -f: we need to inspect redirect status codes ourselves.
        apply_curl_http_only(&mut command);
        command
            .arg("-sS")
            .arg("--max-redirs")
            .arg("0")
            .arg("--max-filesize")
            .arg(MAX_REMOTE_FILE_BYTES.to_string())
            .arg("--connect-timeout")
            .arg("15")
            .arg("-D")
            .arg(&header_path)
            .arg("-o")
            .arg(path)
            .arg("-w")
            .arg("%{http_code}")
            .arg(&current);

        let output = run_command_cancellable(
            command,
            REMOTE_DOWNLOAD_TIMEOUT,
            1024 * 1024,
            cancel.clone(),
        )
        .map_err(|error| {
            let _ = fs::remove_file(&header_path);
            if error.is_cancelled() {
                error
            } else {
                ConversionError::new(format!(
                    "could not download {display_current}: {error}. Install curl or open the file locally."
                ))
            }
        })?;

        let headers = fs::read_to_string(&header_path).unwrap_or_default();
        let _ = fs::remove_file(&header_path);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            return Err(ConversionError::new(if detail.is_empty() {
                format!(
                    "could not download {display_current} (curl exit {})",
                    output.status
                )
            } else {
                format!("could not download {display_current}: {detail}")
            }));
        }

        let code_text = String::from_utf8_lossy(&output.stdout);
        let code: u16 = code_text.trim().parse().unwrap_or(0);

        if (300..400).contains(&code) {
            let Some(location) = location_header(&headers) else {
                return Err(ConversionError::new(format!(
                    "could not download {display_current}: redirect ({code}) without Location header"
                )));
            };
            current = resolve_redirect_url(&current, &location)?;
            let _ = fs::remove_file(path);
            continue;
        }

        if code == 200 || code == 0 {
            // Some curl builds omit write-out on success; treat 0 + non-empty body as ok later.
            if code == 200 || path.is_file() {
                return Ok(());
            }
        }

        return Err(ConversionError::new(format!(
            "could not download {display_current}: HTTP {code}"
        )));
    }

    Err(ConversionError::new(format!(
        "could not download {}: too many redirects (max {MAX_DOWNLOAD_REDIRECTS})",
        redact_url_credentials(url)
    )))
}

fn location_header(headers: &str) -> Option<String> {
    for line in headers.lines() {
        let line = line.trim();
        // Status lines (`HTTP/1.1 302 Found`) have no colon — skip them.
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("location") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn resolve_redirect_url(base: &str, location: &str) -> Result<String, ConversionError> {
    let location = location.trim();
    let resolved = if let Ok(absolute) = Url::parse(location) {
        // Absolute URL in Location — only http(s) may proceed.
        if !matches!(absolute.scheme(), "http" | "https") {
            return Err(ConversionError::new(format!(
                "refusing non-http(s) redirect Location '{location}'"
            )));
        }
        absolute
    } else {
        let base = Url::parse(base).map_err(|error| {
            ConversionError::new(format!("invalid redirect base URL {base}: {error}"))
        })?;
        base.join(location).map_err(|error| {
            ConversionError::new(format!(
                "could not resolve redirect Location '{location}': {error}"
            ))
        })?
    };

    if !matches!(resolved.scheme(), "http" | "https") || resolved.host().is_none() {
        return Err(ConversionError::new(format!(
            "refusing non-http(s) redirect target '{resolved}'"
        )));
    }
    // Drop userinfo so credentials in Location cannot be forwarded blindly.
    if !resolved.username().is_empty() || resolved.password().is_some() {
        return Err(ConversionError::new(format!(
            "refusing redirect Location with credentials: {location}"
        )));
    }
    Ok(resolved.to_string())
}

/// Limit curl to http(s) only (blocks file:// and other protocol smuggling).
fn apply_curl_http_only(command: &mut Command) {
    command
        .arg("--proto")
        .arg("=https,http")
        .arg("--proto-redir")
        .arg("=https,http");
}

fn remote_file_name(url: &str) -> String {
    if let Ok(parsed) = Url::parse(url) {
        if let Some(last) = parsed
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|s| !s.is_empty())
        {
            return sanitize_file_name(last);
        }
    }
    format!("download-{}", unique_stamp())
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.');
    if trimmed.is_empty() {
        format!("download-{}", unique_stamp())
    } else {
        trimmed.chars().take(180).collect()
    }
}

fn paste_staging_dir() -> Result<PathBuf, ConversionError> {
    let dir = if let Some(override_dir) = std::env::var_os("SHIFT_PASTE_STAGING_DIR") {
        PathBuf::from(override_dir)
    } else if let Some(cache) = crate::artifact_cache::default_paste_staging_dir() {
        cache
    } else {
        std::env::temp_dir().join("shift-paste-staging")
    };
    fs::create_dir_all(&dir).map_err(|error| {
        ConversionError::new(format!(
            "could not create paste staging directory {}: {error}",
            dir.display()
        ))
    })?;
    Ok(dir)
}

fn unique_stamp() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(unix)]
    fn write_curl_script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("curl");
        fs::write(&path, body).unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    fn prepend_to_path(dir: &Path) -> Option<std::ffi::OsString> {
        let current = std::env::var_os("PATH").unwrap_or_default();
        let mut parts: Vec<PathBuf> = vec![dir.to_path_buf()];
        parts.extend(std::env::split_paths(&current));
        std::env::join_paths(parts).ok()
    }

    #[test]
    fn parses_page_urls() {
        let paste = parse_magic_paste("https://example.com/article");
        assert_eq!(
            paste,
            MagicPaste::Single(PasteToken::PageUrl("https://example.com/article".into()))
        );
    }

    #[test]
    fn parses_remote_file_urls() {
        let paste = parse_magic_paste("https://cdn.example.com/docs/report.pdf");
        assert_eq!(
            paste,
            MagicPaste::Single(PasteToken::RemoteFileUrl(
                "https://cdn.example.com/docs/report.pdf".into()
            ))
        );
        // Query string still classifies from path extension.
        let paste = parse_magic_paste("https://cdn.example.com/f/doc.docx?dl=1");
        assert!(matches!(
            paste,
            MagicPaste::Single(PasteToken::RemoteFileUrl(_))
        ));
    }

    #[test]
    fn html_urls_are_pages_not_files() {
        let paste = parse_magic_paste("https://example.com/index.html");
        assert_eq!(
            paste,
            MagicPaste::Single(PasteToken::PageUrl("https://example.com/index.html".into()))
        );
    }

    #[test]
    fn parses_file_urls() {
        let paste = parse_magic_paste("file:///tmp/sample.pdf");
        match paste {
            MagicPaste::Single(PasteToken::LocalPath(path)) => {
                assert_eq!(path, PathBuf::from("/tmp/sample.pdf"));
            }
            other => panic!("expected local path, got {other:?}"),
        }
    }

    #[test]
    fn invalid_file_urls_are_rejected() {
        // Host-form file URLs cannot be converted to a local path on Unix.
        let paste = parse_magic_paste("file://hostname/only");
        assert_eq!(paste, MagicPaste::Empty);
    }

    #[test]
    fn parses_absolute_and_quoted_paths() {
        let paste = parse_magic_paste("/Users/me/report.pdf");
        assert_eq!(
            paste,
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from("/Users/me/report.pdf")))
        );

        let paste = parse_magic_paste("\"/Users/me/My Docs/file.pdf\"");
        assert_eq!(
            paste,
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from(
                "/Users/me/My Docs/file.pdf"
            )))
        );
    }

    #[test]
    fn parses_multiple_mixed_tokens() {
        let paste =
            parse_magic_paste("https://example.com/a /tmp/x.pdf https://cdn.example.com/y.png");
        match paste {
            MagicPaste::Multiple(tokens) => {
                assert_eq!(tokens.len(), 3);
                assert!(matches!(&tokens[0], PasteToken::PageUrl(_)));
                assert!(matches!(&tokens[1], PasteToken::LocalPath(_)));
                assert!(matches!(&tokens[2], PasteToken::RemoteFileUrl(_)));
            }
            other => panic!("expected multiple, got {other:?}"),
        }
    }

    #[test]
    fn empty_and_noise_yield_empty() {
        assert_eq!(parse_magic_paste(""), MagicPaste::Empty);
        assert_eq!(parse_magic_paste("   "), MagicPaste::Empty);
        assert_eq!(parse_magic_paste("not a path or url"), MagicPaste::Empty);
    }

    #[test]
    fn materialize_missing_local_path_errors() {
        let token = PasteToken::LocalPath(PathBuf::from(
            "/tmp/shift-magic-paste-definitely-missing-xyz.pdf",
        ));
        let err = materialize_paste_token(&token, None).unwrap_err();
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn materialize_existing_local_path() {
        let dir =
            std::env::temp_dir().join(format!("shift-magic-paste-exist-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.md");
        fs::write(&path, b"# hi\n").unwrap();
        let source = materialize_paste_token(&PasteToken::LocalPath(path.clone()), None).unwrap();
        assert_eq!(source, BatchSource::File(path));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_honours_cancel_flag() {
        let cancel = Arc::new(AtomicBool::new(true));
        let token = PasteToken::LocalPath(PathBuf::from("/tmp/shift-magic-paste-cancel-test.txt"));
        let err = materialize_paste_token(&token, Some(cancel)).unwrap_err();
        assert!(err.is_cancelled());
    }

    #[test]
    fn stages_clipboard_image_bytes() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir =
            std::env::temp_dir().join(format!("shift-magic-paste-img-{}", std::process::id()));
        // SAFETY: test-only env override for staging location.
        unsafe {
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &dir);
        }
        let path = stage_pasted_image(b"\x89PNG\r\nfake", "png").unwrap();
        assert!(path.extension().is_some_and(|e| e == "png"));
        assert_eq!(fs::read(&path).unwrap(), b"\x89PNG\r\nfake");
        let _ = fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
    }

    #[test]
    fn rejects_unsupported_clipboard_image_format() {
        let err = stage_pasted_image(b"<svg/>", "svg").unwrap_err();
        assert!(
            err.to_string().contains("not a supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn url_looks_like_remote_file_helpers() {
        assert!(url_looks_like_remote_file("https://example.com/a/b/c.mp4"));
        assert!(!url_looks_like_remote_file("https://example.com/post"));
        assert!(!url_looks_like_remote_file("https://example.com/page.html"));
    }

    #[test]
    fn resolve_redirect_joins_relative_location() {
        let next = resolve_redirect_url("https://cdn.example.com/a/b.pdf", "../c.pdf").unwrap();
        assert_eq!(next, "https://cdn.example.com/c.pdf");
    }

    #[test]
    fn location_header_skips_status_line() {
        let headers = "HTTP/1.1 302 Found\r\n\
             Server: cloudflare\r\n\
             Location: https://cdn.example.com/real.pdf\r\n\
             Content-Length: 0\r\n\r\n";
        assert_eq!(
            location_header(headers).as_deref(),
            Some("https://cdn.example.com/real.pdf")
        );
        // LF-only dumps and lower-case names.
        let headers = "HTTP/2 301\nlocation: /next.pdf\n";
        assert_eq!(location_header(headers).as_deref(), Some("/next.pdf"));
        assert_eq!(location_header("HTTP/1.1 200 OK\n"), None);
    }

    #[test]
    fn resolve_redirect_rejects_non_http_schemes() {
        let err = resolve_redirect_url("https://cdn.example.com/a.pdf", "file:///etc/passwd")
            .unwrap_err();
        assert!(
            err.to_string().contains("non-http"),
            "unexpected error: {err}"
        );
        let err =
            resolve_redirect_url("https://cdn.example.com/a.pdf", "gopher://evil/").unwrap_err();
        assert!(err.to_string().contains("non-http"), "unexpected: {err}");
    }

    #[test]
    fn resolve_redirect_rejects_credentials_in_location() {
        let err = resolve_redirect_url(
            "https://cdn.example.com/a.pdf",
            "https://user:pass@evil.example/x.pdf",
        )
        .unwrap_err();
        assert!(err.to_string().contains("credentials"), "unexpected: {err}");
    }

    #[test]
    fn remote_download_timeout_exceeds_default_process_timeout() {
        assert!(REMOTE_DOWNLOAD_TIMEOUT > super::super::process::DEFAULT_PROCESS_TIMEOUT);
    }

    #[cfg(unix)]
    const FAKE_CURL_SCRIPT: &str = r###"#!/bin/sh
header=""
output=""
status_arg=""
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -D) header="$2"; shift 2;;
    -o) output="$2"; shift 2;;
    -w) status_arg="$2"; shift 2;;
    -*) shift;;
    *) url="$1"; shift;;
  esac
done

case "$url" in
  *huge*)
    code=200
    if [ -n "$output" ]; then
      rm -f "$output"
      dd if=/dev/zero of="$output" bs=1048576 seek=513 count=0 2>/dev/null
    fi
    ;;
  *empty*)
    code=200
    if [ -n "$output" ]; then
      rm -f "$output"
      : > "$output"
    fi
    ;;
  *fail*)
    echo "boom" >&2
    exit 1
    ;;
  *nolocation*)
    code=302
    ;;
  *loop*)
    code=302
    location="$url"
    ;;
  *redirect*)
    code=302
    location="http://example.com/final"
    ;;
  *final*)
    code=200
    if [ -n "$output" ]; then
      rm -f "$output"
      printf 'fake' > "$output"
    fi
    ;;
  *)
    code=200
    if [ -n "$output" ]; then
      rm -f "$output"
      printf 'fake' > "$output"
    fi
    ;;
esac

if [ -n "$header" ]; then
  if [ "$code" -eq 302 ] && [ -n "$location" ]; then
    cat > "$header" <<EOF
HTTP/1.1 302 Found
Location: $location

EOF
  elif [ "$code" -eq 302 ]; then
    cat > "$header" <<EOF
HTTP/1.1 302 Found
Server: x

EOF
  else
    cat > "$header" <<EOF
HTTP/1.1 200 OK
Content-Length: 4

EOF
  fi
fi

if [ -n "$status_arg" ]; then
  printf '%s' "$code"
fi
exit 0
"###;

    #[test]
    fn magic_paste_tokens_and_is_empty() {
        assert!(MagicPaste::Empty.is_empty());
        assert_eq!(MagicPaste::Empty.tokens(), &[] as &[PasteToken]);
        let single = MagicPaste::Single(PasteToken::PageUrl("https://example.com".into()));
        assert!(!single.is_empty());
        assert_eq!(single.tokens().len(), 1);
        let multiple = MagicPaste::Multiple(vec![
            PasteToken::PageUrl("https://a.com".into()),
            PasteToken::PageUrl("https://b.com".into()),
        ]);
        assert!(!multiple.is_empty());
        assert_eq!(multiple.tokens().len(), 2);
    }

    #[test]
    fn url_looks_like_remote_file_rejects_non_http_and_malformed() {
        assert!(!url_looks_like_remote_file("ftp://example.com/file.pdf"));
        assert!(!url_looks_like_remote_file("not a url"));
    }

    #[test]
    fn looks_like_path_token_branches() {
        assert_eq!(
            parse_magic_paste("relative/path/file.pdf"),
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from(
                "relative/path/file.pdf"
            )))
        );
        assert_eq!(
            parse_magic_paste("relative\\path\\file.pdf"),
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from(
                "relative\\path\\file.pdf"
            )))
        );
        assert_eq!(
            parse_magic_paste("document.pdf"),
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from("document.pdf")))
        );

        let dir = std::env::temp_dir().join(format!("shift-magic-exist-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("note");
        fs::write(&file, b"hi").unwrap();
        assert_eq!(
            parse_magic_paste(file.to_str().unwrap()),
            MagicPaste::Single(PasteToken::LocalPath(file.clone()))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_user_path_tilde() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("shift-magic-home-{}", std::process::id()));
        fs::create_dir_all(&home).unwrap();
        let old = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }
        assert_eq!(expand_user_path(Path::new("~")), home);
        assert_eq!(expand_user_path(Path::new("~/Docs")), home.join("Docs"));
        if let Some(old) = old {
            unsafe {
                std::env::set_var("HOME", old);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn expand_user_path_non_utf8() {
        let raw = OsString::from_vec(vec![0x80, 0x81, 0x82]);
        let path = PathBuf::from(&raw);
        assert_eq!(expand_user_path(path.as_path()), path);
    }

    #[test]
    fn path_extension_from_url_path_edge_cases() {
        assert_eq!(
            path_extension_from_url_path("https://example.com/file."),
            None
        );
        assert_eq!(
            path_extension_from_url_path("https://example.com/file.verylongextensionname"),
            None
        );
        assert_eq!(
            path_extension_from_url_path("https://example.com/file.p$d"),
            None
        );
        assert_eq!(
            path_extension_from_url_path("https://example.com/dir/"),
            None
        );
        assert_eq!(
            path_extension_from_url_path("https://example.com/file"),
            None
        );
    }

    #[test]
    fn sanitize_file_name_and_remote_file_name() {
        assert_eq!(sanitize_file_name("file@#$%^&*().pdf"), "file_________.pdf");
        assert_eq!(sanitize_file_name("...hidden"), "hidden");
        assert!(sanitize_file_name("   ").starts_with("download-"));
        let long = "a".repeat(200) + ".pdf";
        assert_eq!(sanitize_file_name(&long).len(), 180);

        assert!(
            remote_file_name("https://example.com/path/document.pdf").ends_with("document.pdf")
        );
        assert!(remote_file_name("not-a-url").starts_with("download-"));
    }

    #[test]
    fn unique_stamp_contains_pid_and_nanos() {
        let stamp = unique_stamp();
        assert!(stamp.starts_with(&format!("{}-", std::process::id())));
    }

    #[test]
    fn paste_staging_dir_env_override_and_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("shift-paste-staging-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        unsafe {
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &dir);
        }
        assert_eq!(paste_staging_dir().unwrap(), dir);
        unsafe {
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
        let _ = fs::remove_dir_all(&dir);

        let file =
            std::env::temp_dir().join(format!("shift-paste-staging-file-{}", std::process::id()));
        fs::write(&file, b"x").unwrap();
        unsafe {
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &file);
        }
        let err = paste_staging_dir().unwrap_err();
        assert!(
            err.to_string()
                .contains("could not create paste staging directory")
        );
        unsafe {
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn stage_pasted_image_edge_cases() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("shift-magic-img-edge-{}", std::process::id()));
        unsafe {
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &dir);
        }
        fs::create_dir_all(&dir).unwrap();

        let err = stage_pasted_image(&[], "png").unwrap_err();
        assert!(err.to_string().contains("clipboard image is empty"));
        let err = stage_pasted_image(b"x", "").unwrap_err();
        assert!(err.to_string().contains("clipboard image has no format"));
        let err = stage_pasted_image(b"<svg/>", "svg").unwrap_err();
        assert!(err.to_string().contains("not a supported conversion input"));

        unsafe {
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn stage_pasted_image_write_failure() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("shift-magic-img-ro-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let mut permissions = fs::metadata(&dir).unwrap().permissions();
        permissions.set_mode(0o555);
        fs::set_permissions(&dir, permissions).unwrap();
        unsafe {
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &dir);
        }

        let err = stage_pasted_image(b"\x89PNG", "png").unwrap_err();
        assert!(err.to_string().contains("could not stage clipboard image"));

        let mut permissions = fs::metadata(&dir).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&dir, permissions).unwrap();
        unsafe {
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_directory_path_errors() {
        let dir = std::env::temp_dir().join(format!("shift-magic-dir-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let err = materialize_paste_token(&PasteToken::LocalPath(dir.clone()), None).unwrap_err();
        assert!(err.to_string().contains("is a directory"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn materialize_magic_paste_multiple_and_cancel() {
        let dir = std::env::temp_dir().join(format!("shift-magic-multi-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("a.md");
        fs::write(&p1, b"a").unwrap();
        let p2 = dir.join("b.md");
        fs::write(&p2, b"b").unwrap();
        let paste = MagicPaste::Multiple(vec![
            PasteToken::LocalPath(p1.clone()),
            PasteToken::LocalPath(p2.clone()),
        ]);
        let sources = materialize_magic_paste(&paste, None).unwrap();
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().all(|s| matches!(s, BatchSource::File(_))));

        let cancel = Arc::new(AtomicBool::new(true));
        let err = materialize_magic_paste(&paste, Some(cancel)).unwrap_err();
        assert!(err.is_cancelled());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_redirect_url_edge_cases() {
        let err = resolve_redirect_url("not-a-url", "/x").unwrap_err();
        assert!(err.to_string().contains("invalid redirect base URL"));
    }

    #[test]
    fn apply_curl_http_only_adds_protocol_args() {
        let mut cmd = Command::new("curl");
        apply_curl_http_only(&mut cmd);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let joined = args.join(" ");
        assert!(joined.contains("--proto"));
        assert!(joined.contains("=https,http"));
        assert!(joined.contains("--proto-redir"));
    }

    #[cfg(unix)]
    #[test]
    fn materialize_remote_file_url() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let staging =
            std::env::temp_dir().join(format!("shift-magic-remote-{}", std::process::id()));
        fs::create_dir_all(&staging).unwrap();
        let fake_dir =
            std::env::temp_dir().join(format!("shift-magic-fakecurl-{}", std::process::id()));
        fs::create_dir_all(&fake_dir).unwrap();
        let _ = write_curl_script(&fake_dir, FAKE_CURL_SCRIPT);
        let old_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", prepend_to_path(&fake_dir).unwrap());
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &staging);
        }

        let source = materialize_paste_token(
            &PasteToken::RemoteFileUrl("http://example.com/ok".into()),
            None,
        )
        .unwrap();
        assert!(matches!(source, BatchSource::File(_)));

        if let Some(p) = old_path {
            unsafe {
                std::env::set_var("PATH", p);
            }
        } else {
            unsafe {
                std::env::remove_var("PATH");
            }
        }
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_dir_all(&fake_dir);
    }

    #[cfg(unix)]
    #[test]
    fn download_remote_file_scenarios() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let staging =
            std::env::temp_dir().join(format!("shift-magic-download-{}", std::process::id()));
        fs::create_dir_all(&staging).unwrap();
        let fake_dir =
            std::env::temp_dir().join(format!("shift-magic-fakecurl-{}", std::process::id()));
        fs::create_dir_all(&fake_dir).unwrap();
        let _ = write_curl_script(&fake_dir, FAKE_CURL_SCRIPT);
        let old_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", prepend_to_path(&fake_dir).unwrap());
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &staging);
        }

        let path = download_remote_file("http://example.com/ok", None).unwrap();
        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), "fake");
        let _ = fs::remove_file(&path);

        let err = download_remote_file("http://example.com/empty", None).unwrap_err();
        assert!(err.to_string().contains("produced an empty file"));

        let err = download_remote_file("http://example.com/huge", None).unwrap_err();
        assert!(err.to_string().contains("exceeded size limit"));

        let err = download_remote_file("http://example.com/fail", None).unwrap_err();
        assert!(err.to_string().contains("could not download"));
        assert!(err.to_string().contains("boom"));

        if let Some(p) = old_path {
            unsafe {
                std::env::set_var("PATH", p);
            }
        } else {
            unsafe {
                std::env::remove_var("PATH");
            }
        }
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_dir_all(&fake_dir);
    }

    #[cfg(unix)]
    #[test]
    fn download_with_redirect_revalidation_scenarios() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let staging =
            std::env::temp_dir().join(format!("shift-magic-redirect-{}", std::process::id()));
        fs::create_dir_all(&staging).unwrap();
        let fake_dir =
            std::env::temp_dir().join(format!("shift-magic-fakecurl2-{}", std::process::id()));
        fs::create_dir_all(&fake_dir).unwrap();
        let _ = write_curl_script(&fake_dir, FAKE_CURL_SCRIPT);
        let old_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", prepend_to_path(&fake_dir).unwrap());
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &staging);
        }

        let out = staging.join("downloaded");
        download_with_redirect_revalidation("http://example.com/redirect", &out, None).unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap(), "fake");

        let out2 = staging.join("missing");
        let err = download_with_redirect_revalidation("http://example.com/nolocation", &out2, None)
            .unwrap_err();
        assert!(err.to_string().contains("without Location header"));

        let out3 = staging.join("loop");
        let err = download_with_redirect_revalidation("http://example.com/loop", &out3, None)
            .unwrap_err();
        assert!(err.to_string().contains("too many redirects"));

        if let Some(p) = old_path {
            unsafe {
                std::env::set_var("PATH", p);
            }
        } else {
            unsafe {
                std::env::remove_var("PATH");
            }
        }
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_dir_all(&fake_dir);
    }

    #[test]
    fn parse_quotes_multiline_and_windows_like_paths() {
        // Single-quoted path with spaces.
        let paste = parse_magic_paste("'/Users/me/My Docs/file.pdf'");
        assert_eq!(
            paste,
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from(
                "/Users/me/My Docs/file.pdf"
            )))
        );

        // Nested-looking quotes: outer double, inner single kept as content.
        let paste = parse_magic_paste(r#""/tmp/outer 'inner' still.pdf""#);
        assert_eq!(
            paste,
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from(
                "/tmp/outer 'inner' still.pdf"
            )))
        );

        // Single outer with double inside.
        let paste = parse_magic_paste(r#"'"quoted name".pdf'"#);
        assert_eq!(
            paste,
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from(r#""quoted name".pdf"#)))
        );

        // Multiline whitespace separation.
        let paste = parse_magic_paste("/tmp/a.pdf\n/tmp/b.pdf\r\nhttps://example.com/c");
        match paste {
            MagicPaste::Multiple(tokens) => {
                assert_eq!(tokens.len(), 3);
                assert!(matches!(&tokens[0], PasteToken::LocalPath(_)));
                assert!(matches!(&tokens[1], PasteToken::LocalPath(_)));
                assert!(matches!(&tokens[2], PasteToken::PageUrl(_)));
            }
            other => panic!("expected multiple, got {other:?}"),
        }

        // Windows-like path with backslashes is a local path token.
        let paste = parse_magic_paste(r"C:\Users\me\report.pdf");
        assert_eq!(
            paste,
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from(
                r"C:\Users\me\report.pdf"
            )))
        );

        // Mixed tabs and multiple spaces.
        let paste = parse_magic_paste("  /tmp/a.pdf\t\t/tmp/b.md  ");
        assert!(matches!(paste, MagicPaste::Multiple(ref t) if t.len() == 2));

        // Unclosed quote still yields the buffered content as a token.
        let tokens = tokenize_paste("\"/tmp/unclosed.pdf");
        assert_eq!(tokens, vec!["/tmp/unclosed.pdf"]);
    }

    #[test]
    fn materialize_cancel_mid_multi_tokens() {
        let dir = std::env::temp_dir().join(format!(
            "shift-magic-mid-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let p1 = dir.join("first.md");
        let p2 = dir.join("second.md");
        fs::write(&p1, b"1").unwrap();
        fs::write(&p2, b"2").unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        // First token succeeds while cancel is clear.
        let first =
            materialize_paste_token(&PasteToken::LocalPath(p1.clone()), Some(cancel.clone()))
                .unwrap();
        assert_eq!(first, BatchSource::File(p1));

        // Flip cancel before the next token — mid-multi abort.
        cancel.store(true, Ordering::SeqCst);
        let err =
            materialize_paste_token(&PasteToken::LocalPath(p2), Some(cancel.clone())).unwrap_err();
        assert!(err.is_cancelled());

        // materialize_magic_paste also aborts on the first cancelled token.
        let paste = MagicPaste::Multiple(vec![
            PasteToken::LocalPath(dir.join("first.md")),
            PasteToken::LocalPath(dir.join("second.md")),
        ]);
        let err = materialize_magic_paste(&paste, Some(cancel)).unwrap_err();
        assert!(err.is_cancelled());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_file_extension_detection_matrix() {
        // Convertible remote files.
        for url in [
            "https://cdn.example.com/a.pdf",
            "https://cdn.example.com/a.PDF",
            "https://cdn.example.com/path/doc.docx",
            "https://cdn.example.com/v.mp4",
            "https://cdn.example.com/a.wav?token=1",
            "http://cdn.example.com/sub/file.md",
            "https://cdn.example.com/x.epub",
        ] {
            assert!(
                url_looks_like_remote_file(url),
                "expected remote file: {url}"
            );
            assert!(
                matches!(
                    parse_magic_paste(url),
                    MagicPaste::Single(PasteToken::RemoteFileUrl(_))
                ),
                "parse should classify remote file: {url}"
            );
        }

        // Pages / non-files.
        for url in [
            "https://example.com/post",
            "https://example.com/index.html",
            "https://example.com/page.htm",
            "https://example.com/app.php",
            "https://example.com/",
            "https://example.com/dir/",
            "ftp://cdn.example.com/a.pdf",
            "file:///tmp/a.pdf",
        ] {
            assert!(
                !url_looks_like_remote_file(url),
                "expected not remote file: {url}"
            );
        }
    }

    #[test]
    fn stage_pasted_image_various_extensions() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "shift-magic-img-ext-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        unsafe {
            std::env::set_var("SHIFT_PASTE_STAGING_DIR", &dir);
        }
        fs::create_dir_all(&dir).unwrap();

        // Leading-dot and mixed-case extensions normalize.
        let png = stage_pasted_image(b"\x89PNG", ".PNG").unwrap();
        assert_eq!(png.extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(fs::read(&png).unwrap(), b"\x89PNG");

        let jpg = stage_pasted_image(b"\xff\xd8", "JPG").unwrap();
        assert_eq!(jpg.extension().and_then(|e| e.to_str()), Some("jpg"));

        // Empty bytes rejected (already covered lightly; keep with matrix).
        let err = stage_pasted_image(&[], "png").unwrap_err();
        assert!(err.to_string().contains("empty"));

        // Whitespace-only extension after trim → no format.
        let err = stage_pasted_image(b"x", "   ").unwrap_err();
        assert!(err.to_string().contains("no format"));

        // Dot-only after trim_start_matches.
        let err = stage_pasted_image(b"x", ".").unwrap_err();
        assert!(err.to_string().contains("no format"));

        unsafe {
            std::env::remove_var("SHIFT_PASTE_STAGING_DIR");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_url_with_and_without_host() {
        // Standard absolute file URL (no host).
        match parse_magic_paste("file:///Users/me/doc.pdf") {
            MagicPaste::Single(PasteToken::LocalPath(path)) => {
                assert_eq!(path, PathBuf::from("/Users/me/doc.pdf"));
            }
            other => panic!("expected local path, got {other:?}"),
        }

        // Empty / bare file URL must not become a page or remote-file token.
        let paste = parse_magic_paste("file://");
        assert!(
            matches!(
                paste,
                MagicPaste::Empty | MagicPaste::Single(PasteToken::LocalPath(_))
            ),
            "file:// alone should not become a page/remote url: {paste:?}"
        );

        // Non-local host cannot convert to a path on Unix.
        assert_eq!(parse_magic_paste("file://hostname/only"), MagicPaste::Empty);

        // `localhost` is accepted by url::Url::to_file_path on Unix.
        match parse_magic_paste("file://localhost/tmp/x.pdf") {
            MagicPaste::Single(PasteToken::LocalPath(path)) => {
                assert_eq!(path, PathBuf::from("/tmp/x.pdf"));
            }
            MagicPaste::Empty => {
                // Some platforms may still reject host-form file URLs.
            }
            other => panic!("unexpected classification for localhost file URL: {other:?}"),
        }
    }

    #[test]
    fn tilde_expansion_edge_cases() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!(
            "shift-magic-tilde-edge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(home.join("Docs")).unwrap();
        let file = home.join("Docs").join("note.md");
        fs::write(&file, b"# hi").unwrap();
        let old_home = std::env::var_os("HOME");
        let old_profile = std::env::var_os("USERPROFILE");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("USERPROFILE");
        }

        assert_eq!(expand_user_path(Path::new("~")), home);
        assert_eq!(
            expand_user_path(Path::new("~/Docs/note.md")),
            home.join("Docs/note.md")
        );
        // Bare ~ is classified as a path token.
        assert_eq!(
            parse_magic_paste("~"),
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from("~")))
        );
        assert_eq!(
            parse_magic_paste("~/Docs/note.md"),
            MagicPaste::Single(PasteToken::LocalPath(PathBuf::from("~/Docs/note.md")))
        );

        // Materialize expands ~ before existence checks.
        let source = materialize_paste_token(
            &PasteToken::LocalPath(PathBuf::from("~/Docs/note.md")),
            None,
        )
        .unwrap();
        assert_eq!(source, BatchSource::File(file));

        // ~user (no slash) is not home expansion.
        assert_eq!(
            expand_user_path(Path::new("~user/file")),
            PathBuf::from("~user/file")
        );

        // Without HOME/USERPROFILE, tilde is left unchanged.
        unsafe {
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
        }
        assert_eq!(expand_user_path(Path::new("~")), PathBuf::from("~"));
        assert_eq!(expand_user_path(Path::new("~/x")), PathBuf::from("~/x"));

        if let Some(old) = old_home {
            unsafe {
                std::env::set_var("HOME", old);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
        if let Some(old) = old_profile {
            unsafe {
                std::env::set_var("USERPROFILE", old);
            }
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn is_page_extension_and_path_extension_helpers() {
        assert!(is_page_extension("html"));
        assert!(is_page_extension("htm"));
        assert!(is_page_extension("php"));
        assert!(!is_page_extension("pdf"));
        assert!(!is_page_extension("md"));

        assert_eq!(
            path_extension_from_url_path("/docs/report.pdf"),
            Some("pdf".into())
        );
        assert_eq!(
            path_extension_from_url_path("/docs/REPORT.PDF"),
            Some("pdf".into())
        );
        assert_eq!(path_extension_from_url_path("/docs/"), None);
        assert_eq!(path_extension_from_url_path("/docs/file"), None);
    }

    #[test]
    fn materialize_remote_file_cancel_before_start() {
        // materialize_paste_token checks cancel before download so Cancelled kind is preserved
        // (download_remote_file itself re-wraps errors via ConversionError::new).
        let cancel = Arc::new(AtomicBool::new(true));
        let err = materialize_paste_token(
            &PasteToken::RemoteFileUrl("https://cdn.example.com/docs/report.pdf".into()),
            Some(cancel),
        )
        .unwrap_err();
        assert!(err.is_cancelled());
        assert!(err.to_string().contains("cancel"));
    }
}
