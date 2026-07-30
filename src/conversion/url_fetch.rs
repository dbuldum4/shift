//! Hardened HTTP(S) fetch helpers for public-URL downloads.
//!
//! Used by magic-paste remote files and Defuddle page conversion. Enforces:
//! - trusted absolute `curl` (never bare PATH lookup for network fetches)
//! - per-hop public-host revalidation when private-URL blocking is on
//! - DNS preflight with a deadline + optional cancel, fail-closed on errors
//! - pinned `--resolve` addresses to reduce DNS-rebinding TOCTOU
//! - credentials out of child argv (netrc-style 0600 temp, not userinfo in URL)
//! - bounded redirect header capture and collision-proof header temp names

use super::ConversionError;
use super::defuddle::{
    block_private_urls, ensure_public_url_fetch_allowed_with_cancel, redact_credentials_in_text,
    redact_url_credentials, resolve_public_url_addresses_with_cancel,
};
use super::process::{is_runnable, run_command_cancellable};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use url::Url;

/// Soft cap for remote file downloads (512 MiB).
pub const MAX_REMOTE_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Soft cap for Defuddle page HTML downloads (32 MiB).
pub const MAX_PAGE_HTML_BYTES: u64 = 32 * 1024 * 1024;

/// Wall-clock budget for remote downloads (allows large files on moderate links).
pub const REMOTE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Max HTTP redirects when private-URL blocking revalidates each hop.
pub const MAX_DOWNLOAD_REDIRECTS: u32 = 10;

/// Cap redirect response-header dumps written by curl `-D`.
pub const MAX_REDIRECT_HEADER_BYTES: u64 = 64 * 1024;

/// Options for a single download into a caller-provided path.
#[derive(Clone)]
pub struct DownloadOptions {
    pub cancel: Option<Arc<AtomicBool>>,
    pub max_bytes: u64,
    pub timeout: Duration,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            cancel: None,
            max_bytes: MAX_REMOTE_FILE_BYTES,
            timeout: REMOTE_DOWNLOAD_TIMEOUT,
        }
    }
}

/// Resolve a trusted absolute curl binary for network fetches.
///
/// Prefers `SHIFT_CURL_BIN` when it is an absolute runnable path (tests).
/// Otherwise uses `/usr/bin/curl` on macOS and `/usr/bin/curl` or `/bin/curl`
/// on Linux. Never falls back to bare `curl` on PATH.
pub fn trusted_curl_path() -> Result<PathBuf, ConversionError> {
    if let Some(raw) = std::env::var_os("SHIFT_CURL_BIN") {
        let path = PathBuf::from(&raw);
        if path.as_os_str().is_empty() {
            return Err(ConversionError::new(
                "SHIFT_CURL_BIN is set but empty; unset it or provide an absolute curl path",
            ));
        }
        if !path.is_absolute() {
            return Err(ConversionError::new(format!(
                "SHIFT_CURL_BIN must be an absolute path (got {})",
                path.display()
            )));
        }
        if !is_runnable(&path) {
            return Err(ConversionError::new(format!(
                "SHIFT_CURL_BIN is not a runnable curl binary: {}",
                path.display()
            )));
        }
        return Ok(path);
    }

    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &["/usr/bin/curl"]
    } else if cfg!(target_os = "linux") {
        &["/usr/bin/curl", "/bin/curl"]
    } else {
        &["/usr/bin/curl", "/bin/curl"]
    };

    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if is_runnable(&path) {
            return Ok(path);
        }
    }

    Err(ConversionError::new(
        "trusted curl not found at /usr/bin/curl (or /bin/curl on Linux); install curl or set SHIFT_CURL_BIN to an absolute path",
    ))
}

/// Download `url` into `path`, applying public-URL policy when active.
pub fn download_url_to_path(
    url: &str,
    path: &Path,
    options: DownloadOptions,
) -> Result<(), ConversionError> {
    let display_url = redact_url_credentials(url);
    let result = if block_private_urls() {
        download_with_redirect_revalidation(url, path, &options)
    } else {
        download_follow_redirects(url, path, &options)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.is_cancelled() => Err(error),
        Err(error) => Err(ConversionError::new(redact_credentials_in_text(
            &error.to_string().replace(url, &display_url),
        ))),
    }
}

/// Fast path: curl follows redirects (private-URL policy disabled).
fn download_follow_redirects(
    url: &str,
    path: &Path,
    options: &DownloadOptions,
) -> Result<(), ConversionError> {
    let display_url = redact_url_credentials(url);
    if options
        .cancel
        .as_ref()
        .is_some_and(|flag| flag.load(Ordering::SeqCst))
    {
        return Err(ConversionError::cancelled());
    }

    let curl = trusted_curl_path()?;
    let creds = UrlAuth::from_url(url)?;
    let netrc = creds.write_netrc_temp()?;
    let mut command = Command::new(&curl);
    apply_curl_http_only(&mut command);
    apply_curl_auth(&mut command, netrc.as_ref());
    command
        .arg("-fsSL")
        .arg("--max-filesize")
        .arg(options.max_bytes.to_string())
        .arg("--connect-timeout")
        .arg("15")
        .arg("-o")
        .arg(path)
        .arg(&creds.clean_url);

    let output = run_command_cancellable(
        command,
        options.timeout,
        1024 * 1024,
        options.cancel.clone(),
    )
    .map_err(|error| {
        drop_netrc(netrc.as_ref());
        if error.is_cancelled() {
            error
        } else {
            ConversionError::new(format!(
                "could not download {display_url}: {}. Install curl or open the file locally.",
                redact_credentials_in_text(&error.to_string())
            ))
        }
    })?;
    drop_netrc(netrc.as_ref());

    if !output.status.success() {
        let stderr = redact_credentials_in_text(&String::from_utf8_lossy(&output.stderr));
        let detail = stderr.trim();
        return Err(ConversionError::new(if detail.is_empty() {
            format!(
                "could not download {display_url} (curl exit {})",
                output.status
            )
        } else if detail.contains("maximum file size exceeded") || detail.contains("max-filesize") {
            format!(
                "could not download {display_url}: exceeded size limit ({} bytes)",
                options.max_bytes
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
pub fn download_with_redirect_revalidation(
    url: &str,
    path: &Path,
    options: &DownloadOptions,
) -> Result<(), ConversionError> {
    let mut current = url.to_string();
    for hop in 0..=MAX_DOWNLOAD_REDIRECTS {
        let display_current = redact_url_credentials(&current);
        if options
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
        {
            let _ = fs::remove_file(path);
            return Err(ConversionError::cancelled());
        }
        ensure_public_url_fetch_allowed_with_cancel(&current, options.cancel.clone())?;

        // Pin validated addresses for this hop to shrink DNS-rebinding windows.
        let pinned = resolve_public_url_addresses_with_cancel(&current, options.cancel.clone())?;

        let header_path = unique_header_path(path, hop);
        let curl = trusted_curl_path()?;
        let creds = UrlAuth::from_url(&current)?;
        let netrc = creds.write_netrc_temp()?;

        let mut command = Command::new(&curl);
        // No -L / max-redirs 0: surface 3xx so we can validate the next hop.
        // No -f: we need to inspect redirect status codes ourselves.
        apply_curl_http_only(&mut command);
        apply_curl_auth(&mut command, netrc.as_ref());
        apply_curl_resolve_pins(&mut command, &current, &pinned);
        command
            .arg("-sS")
            .arg("--max-redirs")
            .arg("0")
            .arg("--max-filesize")
            .arg(options.max_bytes.to_string())
            .arg("--connect-timeout")
            .arg("15")
            .arg("-D")
            .arg(&header_path)
            .arg("-o")
            .arg(path)
            .arg("-w")
            .arg("%{http_code}")
            .arg(&creds.clean_url);

        let output = run_command_cancellable(
            command,
            options.timeout,
            1024 * 1024,
            options.cancel.clone(),
        )
        .map_err(|error| {
            let _ = fs::remove_file(&header_path);
            drop_netrc(netrc.as_ref());
            if error.is_cancelled() {
                error
            } else {
                ConversionError::new(format!(
                    "could not download {display_current}: {}. Install curl or open the file locally.",
                    redact_credentials_in_text(&error.to_string())
                ))
            }
        })?;
        drop_netrc(netrc.as_ref());

        let headers = read_headers_bounded(&header_path)?;
        let _ = fs::remove_file(&header_path);

        if !output.status.success() {
            let stderr = redact_credentials_in_text(&String::from_utf8_lossy(&output.stderr));
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

fn unique_header_path(body_path: &Path, hop: u32) -> PathBuf {
    let stamp = unique_stamp();
    let parent = body_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = body_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    // Collision-proof across hops and concurrent downloads; stays next to the body.
    parent.join(format!(".{stem}.hdr.{hop}.{stamp}"))
}

fn read_headers_bounded(header_path: &Path) -> Result<String, ConversionError> {
    match fs::metadata(header_path) {
        Ok(meta) if meta.len() > MAX_REDIRECT_HEADER_BYTES => {
            let _ = fs::remove_file(header_path);
            return Err(ConversionError::new(format!(
                "redirect header dump exceeded size limit ({} bytes)",
                MAX_REDIRECT_HEADER_BYTES
            )));
        }
        Ok(_) => {}
        Err(_) => return Ok(String::new()),
    }
    // Bound the read even if the file grew between metadata and open.
    let file = match fs::File::open(header_path) {
        Ok(file) => file,
        Err(_) => return Ok(String::new()),
    };
    use std::io::Read;
    let mut limited = file.take(MAX_REDIRECT_HEADER_BYTES.saturating_add(1));
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf).map_err(|error| {
        ConversionError::new(format!("could not read redirect headers: {error}"))
    })?;
    if buf.len() as u64 > MAX_REDIRECT_HEADER_BYTES {
        return Err(ConversionError::new(format!(
            "redirect header dump exceeded size limit ({} bytes)",
            MAX_REDIRECT_HEADER_BYTES
        )));
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

pub(crate) fn location_header(headers: &str) -> Option<String> {
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

pub(crate) fn resolve_redirect_url(base: &str, location: &str) -> Result<String, ConversionError> {
    let location = location.trim();
    let resolved = if let Ok(absolute) = Url::parse(location) {
        // Absolute URL in Location — only http(s) may proceed.
        if !matches!(absolute.scheme(), "http" | "https") {
            return Err(ConversionError::new(format!(
                "refusing non-http(s) redirect Location '{}'",
                redact_url_credentials(location)
            )));
        }
        absolute
    } else {
        let base = Url::parse(base).map_err(|error| {
            ConversionError::new(format!(
                "invalid redirect base URL {}: {error}",
                redact_url_credentials(base)
            ))
        })?;
        base.join(location).map_err(|error| {
            ConversionError::new(format!(
                "could not resolve redirect Location '{}': {error}",
                redact_url_credentials(location)
            ))
        })?
    };

    if !matches!(resolved.scheme(), "http" | "https") || resolved.host().is_none() {
        return Err(ConversionError::new(format!(
            "refusing non-http(s) redirect target '{}'",
            redact_url_credentials(resolved.as_str())
        )));
    }
    // Drop userinfo so credentials in Location cannot be forwarded blindly.
    if !resolved.username().is_empty() || resolved.password().is_some() {
        return Err(ConversionError::new(format!(
            "refusing redirect Location with credentials: {}",
            redact_url_credentials(location)
        )));
    }
    Ok(resolved.to_string())
}

/// Limit curl to http(s) only (blocks file:// and other protocol smuggling).
pub(crate) fn apply_curl_http_only(command: &mut Command) {
    command
        .arg("--proto")
        .arg("=https,http")
        .arg("--proto-redir")
        .arg("=https,http");
}

fn apply_curl_auth(command: &mut Command, netrc: Option<&PathBuf>) {
    if let Some(path) = netrc {
        command.arg("--netrc-file").arg(path);
    }
}

fn apply_curl_resolve_pins(command: &mut Command, url: &str, addrs: &[SocketAddr]) {
    let Ok(parsed) = Url::parse(url.trim()) else {
        return;
    };
    let Some(host) = parsed.host_str().map(str::to_owned) else {
        return;
    };
    // IP-literal hosts do not need --resolve.
    if host.parse::<std::net::IpAddr>().is_ok() {
        return;
    }
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
    // Pin every validated address so curl does not re-resolve via the system resolver.
    for addr in addrs {
        command
            .arg("--resolve")
            .arg(format!("{host}:{port}:{}", addr.ip()));
    }
}

struct UrlAuth {
    clean_url: String,
    username: Option<String>,
    password: Option<String>,
    host: Option<String>,
}

impl UrlAuth {
    fn from_url(url: &str) -> Result<Self, ConversionError> {
        let mut parsed = Url::parse(url.trim()).map_err(|error| {
            ConversionError::new(format!(
                "invalid URL {}: {error}",
                redact_url_credentials(url)
            ))
        })?;
        let username = if parsed.username().is_empty() {
            None
        } else {
            Some(parsed.username().to_owned())
        };
        let password = parsed.password().map(str::to_owned);
        let host = parsed.host_str().map(str::to_owned);
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        Ok(Self {
            clean_url: parsed.to_string(),
            username,
            password,
            host,
        })
    }

    fn write_netrc_temp(&self) -> Result<Option<PathBuf>, ConversionError> {
        let Some(user) = self.username.as_deref() else {
            return Ok(None);
        };
        let Some(host) = self.host.as_deref() else {
            return Ok(None);
        };
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "shift-netrc-{}-{}",
            std::process::id(),
            unique_stamp()
        ));
        write_private_file(&path, |file| {
            writeln!(file, "machine {host}")?;
            writeln!(file, "login {user}")?;
            if let Some(password) = self.password.as_deref() {
                writeln!(file, "password {password}")?;
            }
            Ok(())
        })?;
        Ok(Some(path))
    }
}

fn write_private_file(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
) -> Result<(), ConversionError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| {
        ConversionError::new(format!(
            "could not create private credential file {}: {error}",
            path.display()
        ))
    })?;
    write(&mut file).map_err(|error| {
        let _ = fs::remove_file(path);
        ConversionError::new(format!(
            "could not write private credential file {}: {error}",
            path.display()
        ))
    })?;
    Ok(())
}

fn drop_netrc(path: Option<&PathBuf>) {
    if let Some(path) = path {
        let _ = fs::remove_file(path);
    }
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
    use std::sync::Mutex;

    // Serialize env mutations within this module's tests.
    static CURL_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn trusted_curl_path_rejects_relative_override() {
        let _guard = CURL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _crate = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("SHIFT_CURL_BIN", "curl");
        }
        let err = trusted_curl_path().unwrap_err();
        assert!(err.to_string().contains("absolute"), "error: {err}");
        unsafe {
            std::env::remove_var("SHIFT_CURL_BIN");
        }
    }

    #[test]
    fn trusted_curl_path_finds_system_curl() {
        let _guard = CURL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _crate = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("SHIFT_CURL_BIN");
        }
        // macOS / typical Linux CI images ship curl at a trusted path.
        if Path::new("/usr/bin/curl").is_file() || Path::new("/bin/curl").is_file() {
            let path = trusted_curl_path().unwrap();
            assert!(path.is_absolute());
            assert!(is_runnable(&path));
        }
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
        assert!(!err.to_string().contains("pass"));
    }

    #[test]
    fn resolve_redirect_url_edge_cases() {
        let err = resolve_redirect_url("not-a-url", "/x").unwrap_err();
        assert!(err.to_string().contains("invalid redirect base URL"));
    }

    #[test]
    fn resolve_redirect_rejects_target_without_host() {
        let err = resolve_redirect_url("https://example.com/a", "http://").unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing non-http(s) redirect target")
                || err.to_string().contains("redirect"),
            "error: {err}"
        );
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

    #[test]
    fn unique_header_paths_differ_across_hops() {
        let path = Path::new("/tmp/shift-body.bin");
        let a = unique_header_path(path, 0);
        let b = unique_header_path(path, 1);
        assert_ne!(a, b);
        assert!(a.file_name().unwrap().to_string_lossy().contains(".hdr.0."));
        assert!(b.file_name().unwrap().to_string_lossy().contains(".hdr.1."));
    }

    #[test]
    fn read_headers_bounded_rejects_oversized() {
        let dir = std::env::temp_dir().join(format!(
            "shift-hdr-bound-{}-{}",
            std::process::id(),
            unique_stamp()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("headers.txt");
        let oversized = vec![b'A'; (MAX_REDIRECT_HEADER_BYTES as usize) + 8];
        fs::write(&path, &oversized).unwrap();
        let err = read_headers_bounded(&path).unwrap_err();
        assert!(
            err.to_string().contains("exceeded size limit"),
            "error: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn url_auth_strips_userinfo_from_clean_url() {
        let auth = UrlAuth::from_url("https://user:s3cret@example.com/a?q=1").unwrap();
        assert_eq!(auth.clean_url, "https://example.com/a?q=1");
        assert_eq!(auth.username.as_deref(), Some("user"));
        assert_eq!(auth.password.as_deref(), Some("s3cret"));
        assert!(!auth.clean_url.contains("user"));
        assert!(!auth.clean_url.contains("s3cret"));
    }

    #[cfg(unix)]
    #[test]
    fn netrc_temp_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let auth = UrlAuth::from_url("https://alice:pw@example.com/x").unwrap();
        let path = auth.write_netrc_temp().unwrap().unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode was {mode:o}");
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("machine example.com"));
        assert!(body.contains("login alice"));
        assert!(body.contains("password pw"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn remote_download_timeout_exceeds_default_process_timeout() {
        assert!(REMOTE_DOWNLOAD_TIMEOUT > super::super::process::DEFAULT_PROCESS_TIMEOUT);
    }

    #[cfg(unix)]
    #[test]
    fn download_with_fake_curl_follow_and_redirects() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = CURL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _crate = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let staging = std::env::temp_dir().join(format!(
            "shift-urlfetch-{}-{}",
            std::process::id(),
            unique_stamp()
        ));
        fs::create_dir_all(&staging).unwrap();
        let fake_curl = staging.join("fake-curl");
        fs::write(
            &fake_curl,
            r###"#!/bin/sh
header=""
output=""
status_arg=""
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -D) header="$2"; shift 2;;
    -o) output="$2"; shift 2;;
    -w) status_arg="$2"; shift 2;;
    --netrc-file) shift 2;;
    --resolve) shift 2;;
    -*) shift;;
    *) url="$1"; shift;;
  esac
done
case "$url" in
  *redirect*)
    code=302
    location="http://93.184.216.34/final"
    ;;
  *nolocation*)
    code=302
    ;;
  *loop*)
    code=302
    location="$url"
    ;;
  *fail*)
    echo "boom" >&2
    exit 1
    ;;
  *final*|*)
    code=200
    if [ -n "$output" ]; then
      rm -f "$output"
      printf 'fake' > "$output"
    fi
    ;;
esac
if [ -n "$header" ]; then
  if [ "$code" -eq 302 ] && [ -n "$location" ]; then
    printf 'HTTP/1.1 302 Found\nLocation: %s\n\n' "$location" > "$header"
  elif [ "$code" -eq 302 ]; then
    printf 'HTTP/1.1 302 Found\nServer: x\n\n' > "$header"
  else
    printf 'HTTP/1.1 200 OK\nContent-Length: 4\n\n' > "$header"
  fi
fi
if [ -n "$status_arg" ]; then
  printf '%s' "$code"
fi
exit 0
"###,
        )
        .unwrap();
        let mut perms = fs::metadata(&fake_curl).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_curl, perms).unwrap();

        unsafe {
            std::env::set_var("SHIFT_CURL_BIN", &fake_curl);
            // Follow path needs ALLOW; revalidation path uses public IP literals (no DNS).
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
        }

        let out = staging.join("body");
        download_follow_redirects("http://93.184.216.34/ok", &out, &DownloadOptions::default())
            .unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap(), "fake");

        // Force revalidation path with public IP hops (no DNS required).
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
        }
        let out2 = staging.join("redir");
        download_with_redirect_revalidation(
            "http://93.184.216.34/redirect",
            &out2,
            &DownloadOptions::default(),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&out2).unwrap(), "fake");

        let out3 = staging.join("nolo");
        let err = download_with_redirect_revalidation(
            "http://93.184.216.34/nolocation",
            &out3,
            &DownloadOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("without Location header"));

        let out4 = staging.join("loop");
        let err = download_with_redirect_revalidation(
            "http://93.184.216.34/loop",
            &out4,
            &DownloadOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("too many redirects"));

        unsafe {
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
        }
        let cancel = Arc::new(AtomicBool::new(true));
        let err = download_follow_redirects(
            "http://93.184.216.34/ok",
            &staging.join("cancel"),
            &DownloadOptions {
                cancel: Some(cancel),
                ..DownloadOptions::default()
            },
        )
        .unwrap_err();
        assert!(err.is_cancelled());

        unsafe {
            std::env::remove_var("SHIFT_CURL_BIN");
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
        }
        let _ = fs::remove_dir_all(&staging);
    }

    #[cfg(unix)]
    #[test]
    fn download_strips_userinfo_from_curl_argv() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = CURL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _crate = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let staging = std::env::temp_dir().join(format!(
            "shift-urlfetch-auth-{}-{}",
            std::process::id(),
            unique_stamp()
        ));
        fs::create_dir_all(&staging).unwrap();
        let fake_curl = staging.join("fake-curl");
        let args_log = staging.join("args.log");
        fs::write(
            &fake_curl,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$@" > '{}'
output=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) output="$2"; shift 2;;
    -D) shift 2;;
    -w) shift 2;;
    --netrc-file) shift 2;;
    --resolve) shift 2;;
    -*) shift;;
    *) shift;;
  esac
done
if [ -n "$output" ]; then printf 'ok' > "$output"; fi
printf '200'
exit 0
"#,
                args_log.display()
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&fake_curl).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake_curl, perms).unwrap();

        unsafe {
            std::env::set_var("SHIFT_CURL_BIN", &fake_curl);
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
        }

        let out = staging.join("body");
        download_url_to_path(
            "https://user:s3cret@example.com/ok",
            &out,
            DownloadOptions::default(),
        )
        .unwrap();

        let args = fs::read_to_string(&args_log).unwrap();
        assert!(
            !args.contains("s3cret"),
            "password must not appear in curl argv: {args}"
        );
        assert!(
            !args.contains("user:s3cret@"),
            "userinfo must not appear in curl argv: {args}"
        );
        assert!(
            args.contains("https://example.com/ok") || args.contains("example.com/ok"),
            "clean URL missing from argv: {args}"
        );
        assert!(args.contains("--netrc-file"), "expected netrc auth: {args}");

        unsafe {
            std::env::remove_var("SHIFT_CURL_BIN");
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
        }
        let _ = fs::remove_dir_all(&staging);
    }
}
