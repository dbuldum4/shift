//! Classify free-form paste / input bar text into conversion sources.
//!
//! Accepts page URLs, local paths, `file://` URLs, remote file downloads,
//! and (via helpers) clipboard image bytes staged as temporary files.

use super::defuddle::{block_private_urls, ensure_public_url_fetch_allowed, looks_like_url};
use super::process::run_command;
use super::sources::supported_input_extensions;
use super::{BatchSource, ConversionError, ConversionRegistry};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
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
pub fn materialize_paste_token(token: &PasteToken) -> Result<BatchSource, ConversionError> {
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
            let path = download_remote_file(url)?;
            Ok(BatchSource::File(path))
        }
    }
}

/// Materialize every token in a magic-paste parse result.
pub fn materialize_magic_paste(paste: &MagicPaste) -> Result<Vec<BatchSource>, ConversionError> {
    paste.tokens().iter().map(materialize_paste_token).collect()
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
    fs::write(&path, bytes).map_err(|error| {
        ConversionError::new(format!(
            "could not stage clipboard image {}: {error}",
            path.display()
        ))
    })?;
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
    std::env::var_os("HOME").map(PathBuf::from)
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

fn download_remote_file(url: &str) -> Result<PathBuf, ConversionError> {
    let dir = paste_staging_dir()?;
    let name = remote_file_name(url);
    let stamp = unique_stamp();
    let path = dir.join(format!("{stamp}-{name}"));

    let result = if block_private_urls() {
        // Re-validate every hop so a public URL cannot redirect into private space.
        download_with_redirect_revalidation(url, &path)
    } else {
        download_follow_redirects(url, &path)
    };

    if let Err(error) = result {
        let _ = fs::remove_file(&path);
        return Err(error);
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
            "download of {url} produced an empty file"
        )));
    }
    if meta.len() > MAX_REMOTE_FILE_BYTES {
        let _ = fs::remove_file(&path);
        return Err(ConversionError::new(format!(
            "download of {url} exceeded size limit ({} bytes)",
            MAX_REMOTE_FILE_BYTES
        )));
    }

    Ok(path)
}

/// Fast path: curl follows redirects (no private-URL policy).
fn download_follow_redirects(url: &str, path: &Path) -> Result<(), ConversionError> {
    let mut command = Command::new("curl");
    command
        .arg("-fsSL")
        .arg("--max-filesize")
        .arg(MAX_REMOTE_FILE_BYTES.to_string())
        .arg("--connect-timeout")
        .arg("15")
        .arg("-o")
        .arg(path)
        .arg(url);

    let output = run_command(command, REMOTE_DOWNLOAD_TIMEOUT, 1024 * 1024).map_err(|error| {
        ConversionError::new(format!(
            "could not download {url}: {error}. Install curl or open the file locally."
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(ConversionError::new(if detail.is_empty() {
            format!("could not download {url} (curl exit {})", output.status)
        } else if detail.contains("maximum file size exceeded") || detail.contains("max-filesize") {
            format!(
                "could not download {url}: exceeded size limit ({} bytes)",
                MAX_REMOTE_FILE_BYTES
            )
        } else if detail.contains("timed out") || detail.contains("Timeout") {
            format!("could not download {url}: download timed out")
        } else {
            format!("could not download {url}: {detail}")
        }));
    }
    Ok(())
}

/// Safer path when private URLs are blocked (default): no auto-follow; re-check each Location.
fn download_with_redirect_revalidation(url: &str, path: &Path) -> Result<(), ConversionError> {
    let mut current = url.to_string();
    for hop in 0..=MAX_DOWNLOAD_REDIRECTS {
        ensure_url_fetch_allowed(&current)?;

        let header_path = path.with_extension(format!("hdr{hop}"));
        let mut command = Command::new("curl");
        // No -L / max-redirs 0: surface 3xx so we can validate the next hop.
        // No -f: we need to inspect redirect status codes ourselves.
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

        let output =
            run_command(command, REMOTE_DOWNLOAD_TIMEOUT, 1024 * 1024).map_err(|error| {
                let _ = fs::remove_file(&header_path);
                ConversionError::new(format!(
                    "could not download {current}: {error}. Install curl or open the file locally."
                ))
            })?;

        let headers = fs::read_to_string(&header_path).unwrap_or_default();
        let _ = fs::remove_file(&header_path);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            return Err(ConversionError::new(if detail.is_empty() {
                format!("could not download {current} (curl exit {})", output.status)
            } else {
                format!("could not download {current}: {detail}")
            }));
        }

        let code_text = String::from_utf8_lossy(&output.stdout);
        let code: u16 = code_text.trim().parse().unwrap_or(0);

        if (300..400).contains(&code) {
            let Some(location) = location_header(&headers) else {
                return Err(ConversionError::new(format!(
                    "could not download {current}: redirect ({code}) without Location header"
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
            "could not download {current}: HTTP {code}"
        )));
    }

    Err(ConversionError::new(format!(
        "could not download {url}: too many redirects (max {MAX_DOWNLOAD_REDIRECTS})"
    )))
}

fn location_header(headers: &str) -> Option<String> {
    for line in headers.lines() {
        let line = line.trim();
        let (name, value) = line.split_once(':')?;
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
    if looks_like_url(location) {
        return Ok(location.to_owned());
    }
    let base = Url::parse(base).map_err(|error| {
        ConversionError::new(format!("invalid redirect base URL {base}: {error}"))
    })?;
    base.join(location)
        .map(|url| url.to_string())
        .map_err(|error| {
            ConversionError::new(format!(
                "could not resolve redirect Location '{location}': {error}"
            ))
        })
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
        let err = materialize_paste_token(&token).unwrap_err();
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn materialize_existing_local_path() {
        let dir =
            std::env::temp_dir().join(format!("shift-magic-paste-exist-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.md");
        fs::write(&path, b"# hi\n").unwrap();
        let source = materialize_paste_token(&PasteToken::LocalPath(path.clone())).unwrap();
        assert_eq!(source, BatchSource::File(path));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stages_clipboard_image_bytes() {
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
    fn remote_download_timeout_exceeds_default_process_timeout() {
        assert!(REMOTE_DOWNLOAD_TIMEOUT > super::super::process::DEFAULT_PROCESS_TIMEOUT);
    }
}
