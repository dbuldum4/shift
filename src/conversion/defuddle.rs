use super::url_fetch::{self, DownloadOptions, MAX_PAGE_HTML_BYTES};
use super::{
    ConversionArtifact, ConversionError, ConversionModule, ConversionOptions, InvocationRecord,
    LimitedOutput, OutputFormat, bundled_runtime_tool, command_argv_parts, format_argv_display,
    map_spawn_error, max_output_bytes, process_timeout, resolve_tool_executable,
    run_command_cancellable, unique_temp_dir,
};
use std::ffi::OsString;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use url::Url;

/// Wall-clock budget for DNS preflight used by public-URL policy (fail closed).
pub const DNS_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(3);

fn looks_like_missing_node(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("node.js not found")
        || lower.contains("exec: : not found")
        || lower.contains("shift_node_bin")
        || (lower.contains("node") && lower.contains("not found") && lower.contains("defuddle"))
}

const EXTENSIONS: &[&str] = &["htm", "html"];
const OUTPUTS: &[OutputFormat] = &[OutputFormat::MARKDOWN, OutputFormat::HTML];

/// Optional knobs for Defuddle article extraction.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DefuddleOptions {
    /// Prepend YAML frontmatter (`--frontmatter`).
    pub frontmatter: bool,
    /// Preferred language BCP 47 code (`--lang`).
    pub lang: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DefuddleModule {
    executable: OsString,
}

impl Default for DefuddleModule {
    fn default() -> Self {
        Self {
            executable: discover_executable(),
        }
    }
}

fn discover_executable() -> OsString {
    // Prefer a project-local node_modules binary when present. Absolute
    // resolution matches diagnostics (PATH + common_bin_dirs).
    let mut candidates = Vec::new();
    if let Some(bundled) = bundled_runtime_tool("defuddle") {
        candidates.push(bundled);
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("node_modules/.bin/defuddle"));
    resolve_tool_executable("SHIFT_DEFUDDLE_BIN", "defuddle", &candidates)
}

impl DefuddleModule {
    pub fn with_executable(executable: impl Into<OsString>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    fn run(
        &self,
        source: &str,
        display_source: &str,
        markdown: bool,
        options: &ConversionOptions,
    ) -> Result<(LimitedOutput, InvocationRecord), ConversionError> {
        let mut command = Command::new(&self.executable);
        // Packaged `runtime/bin/defuddle` shells out to Node. GUI apps often
        // lack nvm/Homebrew on PATH; inject an absolute node when discovered.
        if std::env::var_os("SHIFT_NODE_BIN").is_none() {
            if let Some(node) = super::find_executable("node") {
                command.env("SHIFT_NODE_BIN", node);
            }
        }
        // `parse <source> [options]` — keep source as argv[2] for Defuddle and
        // the UI test shim (`SOURCE="$2"`).
        command.arg("parse");
        // Local paths are absolutized (cannot look like flags). URLs must not
        // start with `-` so Defuddle never treats them as options.
        let argv_source = if looks_like_url(source) {
            if source.starts_with('-') {
                return Err(ConversionError::new(
                    "refusing URL/source operand that looks like a CLI option",
                ));
            }
            command.arg(source);
            source.to_owned()
        } else {
            let absolute = super::push_operand_path(&mut command, Path::new(source))?;
            absolute.to_string_lossy().into_owned()
        };
        if markdown {
            command.arg("--markdown");
        }
        if options.defuddle.frontmatter {
            command.arg("--frontmatter");
        }
        if let Some(lang) = options
            .defuddle
            .lang
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            command.arg("--lang").arg(lang);
        }

        let mut display_parts = command_argv_parts(&command);
        for part in &mut display_parts {
            if part == source || part == &argv_source {
                *part = display_source.to_owned();
            }
        }
        let argv_display = format_argv_display(&display_parts);
        let invocation = InvocationRecord {
            module_id: self.id(),
            argv_display,
        };

        let output = run_command_cancellable(
            command,
            process_timeout(),
            max_output_bytes(),
            options.cancel.clone(),
        )
        .map_err(|error| {
            map_spawn_error(
                error,
                "Defuddle is not available. Packaged Shift includes Defuddle but needs Node.js \
                 (`brew install node`, or set SHIFT_NODE_BIN). For a standalone CLI install: \
                 `npm install -g defuddle`, or set SHIFT_DEFUDDLE_BIN.",
            )
        })?;
        Ok((output, invocation))
    }

    fn artifact_from_output(
        &self,
        source_label: &str,
        file_stem: &str,
        output_format: OutputFormat,
        output: LimitedOutput,
        invocation: InvocationRecord,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !output.status.success() {
            let detail = redact_credentials_in_text(&String::from_utf8_lossy(&output.stderr))
                .trim()
                .to_owned();
            let detail = if detail.is_empty() {
                format!("process exited with {}", output.status)
            } else {
                detail
            };
            if looks_like_missing_node(&detail) {
                return Err(ConversionError::new(format!(
                    "Defuddle could not convert {source_label}: Node.js not found. \
                     Install Node (for example: `brew install node`) or set SHIFT_NODE_BIN \
                     to an absolute path. Detail: {detail}"
                )));
            }
            return Err(ConversionError::new(format!(
                "Defuddle could not convert {source_label}: {detail}"
            )));
        }

        Ok(ConversionArtifact {
            file_name: format!("{file_stem}.{}", output_format.extension()),
            media_type: output_format.media_type(),
            bytes: output.stdout,
            format: output_format,
            module_id: self.id(),
            pipeline: vec![self.id()],
            invocations: vec![invocation],
        })
    }

    /// Convert a web page URL to a cleaned document via the Defuddle CLI.
    ///
    /// **Network:** Shift downloads the page with hop-validated curl (public
    /// internet only by default), then feeds the local HTML to Defuddle so the
    /// extractor never follows redirects itself. Local files should go through
    /// the file picker or a path. Opt into private targets with
    /// `SHIFT_ALLOW_PRIVATE_URLS=1` or `--allow-private-urls`.
    pub fn convert_url(
        &self,
        url: &str,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !self.supports_url(output_format) {
            return Err(ConversionError::new(format!(
                "Defuddle does not produce {}",
                output_format.label()
            )));
        }
        let url = url.trim();
        if !looks_like_url(url) {
            return Err(ConversionError::new(format!(
                "not a valid http(s) URL: {}",
                redact_url_credentials(url)
            )));
        }
        ensure_public_url_fetch_allowed_with_cancel(url, options.cancel.clone())?;

        let display_url = redact_url_credentials(url);
        // Download+local path: revalidate every redirect hop, pin DNS, keep
        // credentials out of Defuddle argv, and never let the extractor fetch.
        let temp_dir = unique_temp_dir("shift-defuddle-page")?;
        let html_path = temp_dir.join("page.html");
        let download_result = url_fetch::download_url_to_path(
            url,
            &html_path,
            DownloadOptions {
                cancel: options.cancel.clone(),
                max_bytes: MAX_PAGE_HTML_BYTES,
                ..DownloadOptions::default()
            },
        );
        if let Err(error) = download_result {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(error);
        }
        if !html_path.is_file() {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(ConversionError::new(format!(
                "download of {display_url} produced no HTML file"
            )));
        }

        let markdown = output_format == OutputFormat::MARKDOWN;
        let source = html_path
            .to_str()
            .ok_or_else(|| {
                let _ = std::fs::remove_dir_all(&temp_dir);
                ConversionError::new("downloaded page path is not valid UTF-8")
            })?
            .to_owned();
        // Never pass the original URL (or userinfo) to Defuddle — only the local HTML.
        let run_result = self.run(&source, &display_url, markdown, options);
        let artifact_result = match run_result {
            Ok((output, invocation)) => self.artifact_from_output(
                &display_url,
                &url_file_stem(url),
                output_format,
                output,
                invocation,
            ),
            Err(error) => Err(ConversionError::new(redact_credentials_in_text(
                &error.to_string(),
            ))),
        };
        let _ = std::fs::remove_dir_all(&temp_dir);
        artifact_result
    }
}

impl ConversionModule for DefuddleModule {
    fn id(&self) -> &'static str {
        "defuddle"
    }

    fn label(&self) -> &'static str {
        "Defuddle"
    }

    fn input_extensions(&self) -> &'static [&'static str] {
        EXTENSIONS
    }

    fn output_formats(&self) -> &[OutputFormat] {
        OUTPUTS
    }

    fn chainable_output_formats(&self) -> &[OutputFormat] {
        OUTPUTS
    }

    fn supports_url(&self, output: OutputFormat) -> bool {
        OUTPUTS.contains(&output)
    }

    fn convert(
        &self,
        input: &Path,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        if !OUTPUTS.contains(&output_format) {
            return Err(ConversionError::new(format!(
                "Defuddle only produces Markdown or HTML, not {}",
                output_format.label()
            )));
        }

        let markdown = output_format == OutputFormat::MARKDOWN;
        let source = input
            .to_str()
            .ok_or_else(|| ConversionError::new("input path is not valid UTF-8"))?;
        let (output, invocation) = self.run(source, source, markdown, options)?;
        let stem = input
            .file_stem()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .unwrap_or("converted");
        self.artifact_from_output(
            &input.display().to_string(),
            stem,
            output_format,
            output,
            invocation,
        )
    }

    fn convert_url(
        &self,
        url: &str,
        output_format: OutputFormat,
        options: &ConversionOptions,
    ) -> Result<ConversionArtifact, ConversionError> {
        DefuddleModule::convert_url(self, url, output_format, options)
    }
}

/// True for absolute http(s) URLs used as conversion sources.
///
/// Accepts any host syntactically, including localhost and private ranges.
/// Callers that *fetch* the URL must call [`ensure_public_url_fetch_allowed`]
/// (or honor [`block_private_urls`]) so LAN/loopback stay opt-in only.
pub fn looks_like_url(value: &str) -> bool {
    Url::parse(value.trim())
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
}

/// True when Shift should refuse private / loopback / link-local URL fetches.
///
/// **Default: true** (public internet hosts only). Local content should use the
/// file picker or a filesystem path, not `http://localhost`.
///
/// Allow private hosts when:
/// - `SHIFT_ALLOW_PRIVATE_URLS` is truthy (`1` / `true` / `yes`), or
/// - legacy `SHIFT_BLOCK_PRIVATE_URLS` is explicitly falsey (`0` / `false` / `no`).
pub fn block_private_urls() -> bool {
    if env_flag_truthy("SHIFT_ALLOW_PRIVATE_URLS") {
        return false;
    }
    match std::env::var("SHIFT_BLOCK_PRIVATE_URLS")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        // Legacy explicit opt-out of blocking.
        Some("0") | Some("false") | Some("FALSE") | Some("no") | Some("NO") => false,
        // Unset or any other value: block private hosts (safe default).
        _ => true,
    }
}

fn env_flag_truthy(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref().map(str::trim),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

/// Refuse non-public URL hosts when [`block_private_urls`] is active.
///
/// Checks literal host forms (IP / localhost-style names) and, for domain names,
/// resolves A/AAAA records with a bounded deadline so names that point at
/// LAN/loopback/metadata ranges are blocked. **DNS errors fail closed** when
/// private blocking is on (no fail-open on lookup failure). Callers that connect
/// should pin the returned addresses (see
/// [`resolve_public_url_addresses_with_cancel`]) or re-resolve immediately before
/// each hop.
pub fn ensure_public_url_fetch_allowed(url: &str) -> Result<(), ConversionError> {
    ensure_public_url_fetch_allowed_with_cancel(url, None)
}

/// Same as [`ensure_public_url_fetch_allowed`] but honors cooperative cancellation
/// during the DNS preflight wait.
pub fn ensure_public_url_fetch_allowed_with_cancel(
    url: &str,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<(), ConversionError> {
    let display_url = redact_url_credentials(url);
    if !block_private_urls() {
        return Ok(());
    }
    if url_targets_non_public_host(url) {
        return Err(ConversionError::new(format!(
            "refusing non-public URL host (public internet only; use the file picker for local files, or set SHIFT_ALLOW_PRIVATE_URLS=1 / --allow-private-urls): {display_url}"
        )));
    }
    // Domain resolution: fail closed on errors / private answers / timeout / cancel.
    let _ = resolve_public_url_addresses_with_cancel(url, cancel)?;
    Ok(())
}

/// Resolve and validate addresses for a public-URL fetch.
///
/// When private-URL blocking is active, returns only public addresses and errors
/// on DNS failure, timeout, cancellation, empty answers, or any non-public
/// record. When blocking is disabled, still resolves with a deadline (for
/// optional pinning) but does not reject private addresses.
pub fn resolve_public_url_addresses_with_cancel(
    url: &str,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Vec<SocketAddr>, ConversionError> {
    let display_url = redact_url_credentials(url);
    let parsed = Url::parse(url.trim())
        .map_err(|error| ConversionError::new(format!("invalid URL {display_url}: {error}")))?;
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });

    let host = parsed
        .host()
        .ok_or_else(|| ConversionError::new(format!("URL has no host: {display_url}")))?;

    let addrs = match host {
        url::Host::Ipv4(ip) => {
            let addr = SocketAddr::from((ip, port));
            if block_private_urls() && ipv4_is_non_public(ip) {
                return Err(ConversionError::new(format!(
                    "refusing non-public URL host (public internet only; use the file picker for local files, or set SHIFT_ALLOW_PRIVATE_URLS=1 / --allow-private-urls): {display_url}"
                )));
            }
            vec![addr]
        }
        url::Host::Ipv6(ip) => {
            let addr = SocketAddr::from((ip, port));
            if block_private_urls() && ipv6_is_non_public(ip) {
                return Err(ConversionError::new(format!(
                    "refusing non-public URL host (public internet only; use the file picker for local files, or set SHIFT_ALLOW_PRIVATE_URLS=1 / --allow-private-urls): {display_url}"
                )));
            }
            vec![addr]
        }
        url::Host::Domain(domain) => {
            if domain_is_non_public(domain) && block_private_urls() {
                return Err(ConversionError::new(format!(
                    "refusing non-public URL host (public internet only; use the file picker for local files, or set SHIFT_ALLOW_PRIVATE_URLS=1 / --allow-private-urls): {display_url}"
                )));
            }
            // Opt-in private hosts still need a resolution for optional pin.
            resolve_domain_with_deadline(domain, port, DNS_PREFLIGHT_TIMEOUT, cancel.as_deref())
                .map_err(|error| match error {
                    DnsPreflightError::Cancelled => ConversionError::cancelled(),
                    DnsPreflightError::Timeout => ConversionError::new(format!(
                        "DNS lookup timed out for {display_url} (limit {}s)",
                        DNS_PREFLIGHT_TIMEOUT.as_secs()
                    )),
                    DnsPreflightError::Lookup(detail) => ConversionError::new(format!(
                        "DNS lookup failed for {display_url}: {detail}"
                    )),
                    DnsPreflightError::Empty => ConversionError::new(format!(
                        "DNS lookup returned no addresses for {display_url}"
                    )),
                })?
        }
    };

    if addrs.is_empty() {
        return Err(ConversionError::new(format!(
            "DNS lookup returned no addresses for {display_url}"
        )));
    }

    if block_private_urls() {
        for addr in &addrs {
            let non_public = match addr {
                SocketAddr::V4(v4) => ipv4_is_non_public(*v4.ip()),
                SocketAddr::V6(v6) => ipv6_is_non_public(*v6.ip()),
            };
            if non_public {
                return Err(ConversionError::new(format!(
                    "refusing URL whose host resolves to a non-public address (public internet only; use the file picker for local files, or set SHIFT_ALLOW_PRIVATE_URLS=1 / --allow-private-urls): {display_url}"
                )));
            }
        }
    }
    Ok(addrs)
}

#[derive(Debug)]
enum DnsPreflightError {
    Cancelled,
    Timeout,
    Lookup(String),
    Empty,
}

/// Resolve `host:port` on a helper thread with a wall-clock deadline and cancel poll.
fn resolve_domain_with_deadline(
    host: &str,
    port: u16,
    deadline: Duration,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<SocketAddr>, DnsPreflightError> {
    if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err(DnsPreflightError::Cancelled);
    }
    let host_owned = host.to_owned();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (host_owned.as_str(), port)
            .to_socket_addrs()
            .map(|iter| iter.collect::<Vec<_>>())
            .map_err(|error| error.to_string());
        let _ = tx.send(result);
    });

    let start = Instant::now();
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            return Err(DnsPreflightError::Cancelled);
        }
        let remaining = deadline.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(DnsPreflightError::Timeout);
        }
        let wait = remaining.min(Duration::from_millis(50));
        match rx.recv_timeout(wait) {
            Ok(Ok(addrs)) if addrs.is_empty() => return Err(DnsPreflightError::Empty),
            Ok(Ok(addrs)) => return Ok(addrs),
            Ok(Err(detail)) => return Err(DnsPreflightError::Lookup(detail)),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(DnsPreflightError::Lookup(
                    "resolver thread exited without a result".into(),
                ));
            }
        }
    }
}

/// Display host for progress UI (“Fetching example.com…”).
pub fn url_display_host(url: &str) -> String {
    Url::parse(url.trim())
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| "url".into())
}

/// Strip URL userinfo before a URL is displayed, logged, or persisted.
pub fn redact_url_credentials(url: &str) -> String {
    if let Ok(mut parsed) = Url::parse(url.trim()) {
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        return parsed.to_string();
    }
    url.to_owned()
}

/// Redact `scheme://user:pass@host` userinfo inside free-form error / stderr text.
pub fn redact_credentials_in_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(scheme_at) = rest.find("://") {
        out.push_str(&rest[..scheme_at + 3]);
        let after_scheme = &rest[scheme_at + 3..];
        let authority_end = after_scheme
            .find(|c: char| {
                c == '/' || c == '?' || c == '#' || c.is_whitespace() || c == '"' || c == '\''
            })
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        if let Some(at) = authority.rfind('@') {
            // Drop userinfo; keep host[:port].
            out.push_str(&authority[at + 1..]);
        } else {
            out.push_str(authority);
        }
        rest = &after_scheme[authority_end..];
    }
    out.push_str(rest);
    out
}

/// True when the URL host is loopback, private, link-local, or a localhost-style name.
///
/// Used so URL fetches stay on the public internet unless explicitly opted in.
/// Literal IP and special domain checks only — see
/// [`resolve_public_url_addresses_with_cancel`] for DNS-backed policy.
pub fn url_targets_non_public_host(value: &str) -> bool {
    let Ok(parsed) = Url::parse(value.trim()) else {
        return false;
    };
    match parsed.host() {
        Some(url::Host::Domain(domain)) => domain_is_non_public(domain),
        Some(url::Host::Ipv4(ip)) => ipv4_is_non_public(ip),
        Some(url::Host::Ipv6(ip)) => ipv6_is_non_public(ip),
        None => false,
    }
}

/// True when a domain name resolves to any non-public address (or mixed public/private).
///
/// IP literals are skipped (already covered by [`url_targets_non_public_host`]).
/// **DNS failures return true (fail closed)** so policy treats unresolvable names
/// as blocked when private-URL blocking is on. Prefer
/// [`resolve_public_url_addresses_with_cancel`] for new call sites.
#[cfg_attr(not(test), allow(dead_code))]
pub fn url_resolves_to_non_public_host(value: &str) -> bool {
    let Ok(parsed) = Url::parse(value.trim()) else {
        return false;
    };
    // Only domain names need resolution; literal IPs were checked already.
    let Some(url::Host::Domain(domain)) = parsed.host() else {
        return false;
    };
    if domain_is_non_public(domain) {
        return true;
    }
    let port = parsed
        .port_or_known_default()
        .unwrap_or(if parsed.scheme() == "https" { 443 } else { 80 });
    match resolve_domain_with_deadline(domain, port, DNS_PREFLIGHT_TIMEOUT, None) {
        // Fail closed: lookup problems mean we cannot prove the host is public.
        Err(_) => true,
        Ok(addrs) => {
            if addrs.is_empty() {
                return true;
            }
            for addr in addrs {
                match addr {
                    SocketAddr::V4(v4) if ipv4_is_non_public(*v4.ip()) => return true,
                    SocketAddr::V6(v6) if ipv6_is_non_public(*v6.ip()) => return true,
                    _ => {}
                }
            }
            false
        }
    }
}

fn domain_is_non_public(domain: &str) -> bool {
    let domain = domain.to_ascii_lowercase();
    domain == "localhost"
        || domain.ends_with(".localhost")
        || domain.ends_with(".local")
        || domain == "0.0.0.0"
        // Well-known cloud metadata / internal names (not public internet).
        || domain == "metadata.google.internal"
        || domain.ends_with(".metadata.google.internal")
        || domain == "metadata"
        || domain.ends_with(".internal")
}

fn ipv4_is_non_public(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        // CGNAT / shared address space 100.64.0.0/10 (RFC 6598).
        || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
        // Benchmarking 198.18.0.0/15 (RFC 2544).
        || (octets[0] == 198 && (octets[1] & 0xfe) == 18)
        // IETF protocol assignments 192.0.0.0/24 (excluding documentation already covered).
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        // Reserved for future use 240.0.0.0/4.
        || (octets[0] & 0xf0) == 240
}

fn ipv6_is_non_public(ip: std::net::Ipv6Addr) -> bool {
    // IPv4-mapped forms (:ffff:x.x.x.x) must use the embedded v4 policy.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return ipv4_is_non_public(v4);
    }
    ip.is_loopback() || ip.is_unique_local() || ip.is_unspecified() || ip.is_multicast() || {
        // Link-local unicast fe80::/10
        let segments = ip.segments();
        (segments[0] & 0xffc0) == 0xfe80
    }
}

fn url_file_stem(url: &str) -> String {
    let parsed = Url::parse(url.trim()).ok();
    let last_path_segment = parsed.as_ref().and_then(|url| {
        url.path_segments()
            .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
    });
    let last = last_path_segment
        .and_then(|segment| Path::new(segment).file_stem())
        .and_then(|value| value.to_str())
        .or_else(|| parsed.as_ref().and_then(Url::host_str))
        .unwrap_or("page");

    let stem: String = last
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect();

    let stem = stem.trim_matches('-');
    if stem.is_empty() {
        "page".into()
    } else {
        stem.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_http_urls() {
        assert!(looks_like_url("https://example.com/article"));
        assert!(looks_like_url("  http://example.com  "));
        assert!(looks_like_url("http://localhost:3000/article"));
        assert!(looks_like_url("http://[::1]/article"));
        assert!(!looks_like_url("ftp://example.com"));
        assert!(!looks_like_url("example.com"));
        assert!(!looks_like_url("not a url"));
        assert!(!looks_like_url("report.docx"));
        assert!(!looks_like_url("https://?path.example"));
    }

    #[test]
    fn detects_non_public_url_hosts() {
        assert!(url_targets_non_public_host("http://localhost:3000/x"));
        assert!(url_targets_non_public_host("http://127.0.0.1/"));
        assert!(url_targets_non_public_host("http://192.168.1.1/"));
        assert!(url_targets_non_public_host("http://10.0.0.5/a"));
        assert!(url_targets_non_public_host("http://[::1]/"));
        // IPv4-mapped IPv6 loopback / private / link-local metadata.
        assert!(url_targets_non_public_host("http://[::ffff:127.0.0.1]/"));
        assert!(url_targets_non_public_host("http://[::ffff:10.0.0.1]/"));
        assert!(url_targets_non_public_host(
            "http://[::ffff:169.254.169.254]/"
        ));
        // CGNAT and metadata-style names.
        assert!(url_targets_non_public_host("http://100.64.1.2/"));
        assert!(url_targets_non_public_host(
            "http://metadata.google.internal/"
        ));
        assert!(url_targets_non_public_host("http://foo.internal/x"));
        assert!(!url_targets_non_public_host("https://example.com/article"));
        assert!(!url_targets_non_public_host("https://8.8.8.8/"));
    }

    /// Serializes tests that mutate `SHIFT_*_PRIVATE_URLS` process env.

    #[test]
    fn public_url_policy_blocks_private_by_default() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Isolate from the developer's shell env for this process.
        // SAFETY: serialized behind crate::ENV_LOCK.
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
        }
        assert!(block_private_urls());
        let err = ensure_public_url_fetch_allowed("http://127.0.0.1/secret").unwrap_err();
        assert!(
            err.to_string().contains("non-public") || err.to_string().contains("public internet"),
            "error: {err}"
        );
        // Literal public IP — no DNS required.
        assert!(ensure_public_url_fetch_allowed("https://8.8.8.8/").is_ok());
        // Domain path may resolve; only assert it is not rejected as a private literal.
        assert!(!url_targets_non_public_host("https://example.com/a"));
    }

    #[test]
    fn url_display_host_extracts_host() {
        assert_eq!(url_display_host("https://example.com/path"), "example.com");
        assert_eq!(url_display_host("http://localhost:3000/x"), "localhost");
    }

    #[test]
    fn redacts_url_credentials_for_display() {
        let redacted = redact_url_credentials("https://user:s3cret@example.com/private?q=1");
        assert_eq!(redacted, "https://example.com/private?q=1");
        assert!(!redacted.contains("user"));
        assert!(!redacted.contains("s3cret"));
    }

    #[test]
    fn url_stem_uses_last_path_segment() {
        assert_eq!(
            url_file_stem("https://example.com/blog/my-post?ref=1"),
            "my-post"
        );
        assert_eq!(url_file_stem("https://example.com/"), "example.com");
        assert_eq!(url_file_stem("https://example.com/page.html"), "page");
    }

    #[cfg(unix)]
    #[test]
    fn converts_a_url_source_with_markdown_flag() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir().join(format!(
            "shift-defuddle-url-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("shift-defuddle-test");
        // Echo argv so we can assert the CLI shape, then print fake markdown.
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '# Hello\\n'",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        // Fake curl: download-first path must not put userinfo or bare PATH curl on argv.
        let fake_curl = directory.join("fake-curl");
        std::fs::write(
            &fake_curl,
            r#"#!/bin/sh
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
if [ -n "$output" ]; then printf '<html><body><p>hi</p></body></html>' > "$output"; fi
printf '200'
exit 0
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_curl).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_curl, permissions).unwrap();

        unsafe {
            std::env::set_var("SHIFT_CURL_BIN", &fake_curl);
            // Skip DNS so this unit test stays offline-friendly.
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
        }

        let artifact = DefuddleModule::with_executable(&executable)
            .convert_url(
                "https://example.com/hello-world",
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();

        assert_eq!(artifact.file_name, "hello-world.md");
        assert_eq!(artifact.media_type, "text/markdown");
        assert_eq!(artifact.text(), Some("# Hello\n"));
        assert_eq!(artifact.module_id, "defuddle");

        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("parse"));
        assert!(args.contains("--markdown"));
        // Defuddle must receive a local HTML path, never the remote URL.
        assert!(
            !args.contains("https://example.com"),
            "defuddle argv must not contain the remote URL: {args}"
        );
        assert!(
            args.contains("page.html") || args.contains("shift-defuddle-page"),
            "expected local downloaded HTML path in argv: {args}"
        );

        let credentialed = DefuddleModule::with_executable(&executable)
            .convert_url(
                "https://user:s3cret@example.com/hello-world",
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap();
        assert!(
            credentialed
                .invocations
                .iter()
                .all(|record| !record.argv_display.contains("user")
                    && !record.argv_display.contains("s3cret"))
        );
        let cred_args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(
            !cred_args.contains("s3cret") && !cred_args.contains("user:"),
            "credentials must not reach defuddle argv: {cred_args}"
        );

        unsafe {
            std::env::remove_var("SHIFT_CURL_BIN");
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(unix)]
    #[test]
    fn converts_an_html_file_without_markdown_flag_for_html_output() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir();
        let suffix = std::process::id();
        let executable = directory.join(format!("shift-defuddle-html-test-{suffix}"));
        let input = directory.join(format!("shift-defuddle-input-{suffix}.html"));
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '<p>clean</p>'",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "<html><body><p>raw</p></body></html>").unwrap();

        let artifact = DefuddleModule::with_executable(&executable)
            .convert(&input, OutputFormat::HTML, &ConversionOptions::default())
            .unwrap();

        assert_eq!(
            artifact.file_name,
            format!("{}.html", input.file_stem().unwrap().to_string_lossy())
        );
        assert_eq!(artifact.bytes, b"<p>clean</p>");

        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("parse"));
        assert!(!args.contains("--markdown"));

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(format!("{}.args", executable.display()));
        let _ = std::fs::remove_file(&input);
    }

    #[cfg(unix)]
    #[test]
    fn honors_frontmatter_and_lang_options() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir().join(format!(
            "shift-defuddle-opts-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("shift-defuddle-opts");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '# Hello\\n'",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let fake_curl = directory.join("fake-curl");
        std::fs::write(
            &fake_curl,
            r#"#!/bin/sh
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
if [ -n "$output" ]; then printf '<html></html>' > "$output"; fi
printf '200'
exit 0
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_curl).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_curl, permissions).unwrap();
        unsafe {
            std::env::set_var("SHIFT_CURL_BIN", &fake_curl);
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
        }

        let options = ConversionOptions {
            defuddle: DefuddleOptions {
                frontmatter: true,
                lang: Some("en".into()),
            },
            ..ConversionOptions::default()
        };
        DefuddleModule::with_executable(&executable)
            .convert_url("https://example.com/post", OutputFormat::MARKDOWN, &options)
            .unwrap();

        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("--frontmatter"), "args: {args}");
        assert!(args.contains("--lang"), "args: {args}");
        assert!(args.contains("en"), "args: {args}");

        unsafe {
            std::env::remove_var("SHIFT_CURL_BIN");
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn supports_url_for_markdown_and_html_only() {
        let module = DefuddleModule::with_executable("defuddle");
        assert!(module.supports_url(OutputFormat::MARKDOWN));
        assert!(module.supports_url(OutputFormat::HTML));
        assert!(!module.supports_url(OutputFormat::PDF));
        assert!(!module.supports_url(OutputFormat::DOCX));
        assert!(!module.supports_url(OutputFormat::MP3));
        assert!(!module.supports_url(OutputFormat("plain")));
        assert!(!module.supports_url(OutputFormat::PNG));
        assert!(!module.supports_url(OutputFormat::SRT));
    }

    #[test]
    fn looks_like_url_edge_matrix() {
        let yes = [
            "https://example.com",
            "http://example.com/",
            "https://example.com:8443/path?q=1#frag",
            "http://user:pass@example.com/x",
            "http://127.0.0.1/",
            "http://[::1]/",
            "http://[2001:db8::1]/path",
            "HTTPS://EXAMPLE.COM/A",
            "  https://example.com  ",
        ];
        for url in yes {
            assert!(looks_like_url(url), "expected url: {url}");
        }

        let no = [
            "",
            "   ",
            "example.com",
            "www.example.com",
            "ftp://example.com",
            "file:///tmp/x.html",
            "mailto:user@example.com",
            "not a url",
            "report.docx",
            "https://",
            "http://",
            "//example.com",
            "javascript:alert(1)",
        ];
        for url in no {
            assert!(!looks_like_url(url), "expected non-url: {url}");
        }
        // `https:///path` is accepted by the URL parser with an empty authority in
        // some versions; only assert the helper does not panic on it.
        let _ = looks_like_url("https:///path");
    }

    #[test]
    fn non_public_host_edge_matrix() {
        let private = [
            "http://localhost/",
            "http://localhost:3000/x",
            "http://app.localhost/x",
            "http://foo.local/",
            "http://bar.local:8080/",
            "http://127.0.0.1/",
            "http://127.0.0.1:9/",
            "http://10.0.0.1/",
            "http://10.255.255.255/",
            "http://172.16.0.1/",
            "http://172.31.255.1/",
            "http://192.168.0.1/",
            "http://192.168.255.255/",
            "http://169.254.1.1/",
            "http://0.0.0.0/",
            "http://255.255.255.255/",
            "http://224.0.0.1/",
            "http://100.64.0.1/",
            "http://100.127.1.1/",
            "http://198.18.0.1/",
            "http://192.0.0.1/",
            "http://240.0.0.1/",
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[fc00::1]/",
            "http://[fd12:3456:789a::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:10.0.0.2]/",
            "http://[::ffff:192.168.1.1]/",
            "http://[::ffff:169.254.169.254]/",
            "http://metadata/",
            "http://metadata.google.internal/",
            "http://x.metadata.google.internal/",
            "http://svc.internal/",
            "http://0.0.0.0:80/",
        ];
        for url in private {
            assert!(
                url_targets_non_public_host(url),
                "expected non-public: {url}"
            );
        }

        let public = [
            "https://example.com/",
            "https://example.com:443/path",
            "http://8.8.8.8/",
            "http://1.1.1.1/dns",
            "https://93.184.216.34/",
            "http://[2001:4860:4860::8888]/",
            // Invalid URLs are not non-public (literal check returns false).
            "not-a-url",
            "",
        ];
        for url in public {
            assert!(
                !url_targets_non_public_host(url),
                "expected public/non-match: {url}"
            );
        }
    }

    #[test]
    fn url_display_host_and_redact_edge_matrix() {
        let host_cases = [
            ("https://example.com/path", "example.com"),
            ("http://localhost:3000/x", "localhost"),
            (
                "https://user:pass@cdn.example.org:8443/a",
                "cdn.example.org",
            ),
            ("http://127.0.0.1/x", "127.0.0.1"),
            ("http://[::1]/x", "[::1]"),
            ("not-a-url", "url"),
            ("", "url"),
        ];
        for (url, expected) in host_cases {
            assert_eq!(url_display_host(url), expected, "host for {url}");
        }

        let redact_cases = [
            (
                "https://user:s3cret@example.com/private?q=1",
                "https://example.com/private?q=1",
            ),
            ("http://onlyuser@example.com/", "http://example.com/"),
            (
                "https://example.com/no-creds",
                "https://example.com/no-creds",
            ),
            ("not-a-url", "not-a-url"),
        ];
        for (input, expected) in redact_cases {
            let redacted = redact_url_credentials(input);
            assert_eq!(redacted, expected, "redact {input}");
            assert!(!redacted.contains("s3cret"));
            assert!(!redacted.contains("user:"));
        }
    }

    #[test]
    fn url_file_stem_edge_matrix() {
        let cases = [
            ("https://example.com/blog/my-post?ref=1", "my-post"),
            ("https://example.com/", "example.com"),
            ("https://example.com", "example.com"),
            ("https://example.com/page.html", "page"),
            ("https://example.com/a/b/c.d.e.md", "c.d.e"),
            ("https://example.com/%20", "20"),
            ("https://example.com/hello%20world", "hello-20world"),
            ("https://example.com/!!!", "page"),
            ("https://example.com/ok_file-name.2", "ok_file-name"),
            ("https://sub.example.co.uk/x", "x"),
            ("http://127.0.0.1/", "127.0.0.1"),
        ];
        for (url, expected) in cases {
            assert_eq!(url_file_stem(url), expected, "stem for {url}");
        }
    }

    #[test]
    fn block_private_urls_env_matrix() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: serialized behind crate::ENV_LOCK.
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
        }
        assert!(block_private_urls());

        for truthy in ["1", "true", "TRUE", "yes", "YES"] {
            unsafe {
                std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", truthy);
            }
            assert!(
                !block_private_urls(),
                "ALLOW={truthy} should disable blocking"
            );
            unsafe {
                std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            }
        }

        for falsey in ["0", "false", "FALSE", "no", "NO"] {
            unsafe {
                std::env::set_var("SHIFT_BLOCK_PRIVATE_URLS", falsey);
            }
            assert!(
                !block_private_urls(),
                "BLOCK={falsey} should disable blocking"
            );
            unsafe {
                std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
            }
        }

        // ALLOW takes precedence over BLOCK=1.
        unsafe {
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
            std::env::set_var("SHIFT_BLOCK_PRIVATE_URLS", "1");
        }
        assert!(!block_private_urls());
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
        }
        assert!(block_private_urls());
    }

    #[test]
    fn ensure_public_url_fetch_allowed_private_hosts_matrix() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
        }

        for url in [
            "http://127.0.0.1/secret",
            "http://localhost/x",
            "http://192.168.1.10/a",
            "http://10.1.2.3/",
            "http://[::1]/",
            "http://metadata.google.internal/",
        ] {
            let err = ensure_public_url_fetch_allowed(url).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("non-public") || msg.contains("public internet"),
                "url {url}: {msg}"
            );
            // Credentials must never appear in the error.
            assert!(!msg.contains("s3cret"));
        }

        // Credentialed private URL still redacts userinfo in the error.
        let err = ensure_public_url_fetch_allowed("http://user:s3cret@127.0.0.1/x").unwrap_err();
        assert!(!err.to_string().contains("s3cret"));
        assert!(!err.to_string().contains("user:"));

        // Public literal IP allowed without DNS.
        assert!(ensure_public_url_fetch_allowed("https://8.8.8.8/").is_ok());

        // Opt-out allows private hosts.
        unsafe {
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
        }
        assert!(ensure_public_url_fetch_allowed("http://127.0.0.1/x").is_ok());
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
        }
    }

    #[cfg(unix)]
    #[test]
    fn convert_url_rejects_unsupported_format_and_invalid_url() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir();
        let suffix = format!("{}-reject", std::process::id());
        let executable = directory.join(format!("shift-defuddle-reject-{suffix}"));
        std::fs::write(&executable, "#!/bin/sh\nprintf '# should not run\\n'").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let module = DefuddleModule::with_executable(&executable);
        let opts = ConversionOptions::default();

        let err = module
            .convert_url("https://example.com/a", OutputFormat::PDF, &opts)
            .unwrap_err();
        assert!(
            err.to_string().contains("does not produce") || err.to_string().contains("PDF"),
            "{err}"
        );

        let err = module
            .convert_url("not-a-url", OutputFormat::MARKDOWN, &opts)
            .unwrap_err();
        assert!(
            err.to_string().contains("not a valid") || err.to_string().contains("URL"),
            "{err}"
        );

        // Private host rejected before the fake binary runs.
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
        }
        let err = module
            .convert_url("http://127.0.0.1/x", OutputFormat::MARKDOWN, &opts)
            .unwrap_err();
        assert!(
            err.to_string().contains("non-public") || err.to_string().contains("public internet"),
            "{err}"
        );

        let _ = std::fs::remove_file(&executable);
    }

    #[cfg(unix)]
    #[test]
    fn convert_url_html_output_and_failure_status() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let directory = std::env::temp_dir().join(format!(
            "shift-defuddle-html-url-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("shift-defuddle-html-url");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"${0}.args\"\nprintf '<article>ok</article>'",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let fake_curl = directory.join("fake-curl");
        std::fs::write(
            &fake_curl,
            r#"#!/bin/sh
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
if [ -n "$output" ]; then printf '<html><body>story</body></html>' > "$output"; fi
printf '200'
exit 0
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_curl).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_curl, permissions).unwrap();
        unsafe {
            std::env::set_var("SHIFT_CURL_BIN", &fake_curl);
            std::env::set_var("SHIFT_ALLOW_PRIVATE_URLS", "1");
        }

        let artifact = DefuddleModule::with_executable(&executable)
            .convert_url(
                "https://example.com/story",
                OutputFormat::HTML,
                &ConversionOptions::default(),
            )
            .unwrap();
        assert_eq!(artifact.bytes, b"<article>ok</article>");
        assert_eq!(artifact.format, OutputFormat::HTML);
        assert_eq!(artifact.module_id, "defuddle");
        let args = std::fs::read_to_string(format!("{}.args", executable.display())).unwrap();
        assert!(args.contains("parse"));
        assert!(
            !args.contains("--markdown"),
            "html output must omit --markdown"
        );

        // Failing process.
        let fail = directory.join("shift-defuddle-fail");
        std::fs::write(&fail, "#!/bin/sh\necho boom >&2\nexit 2\n").unwrap();
        let mut permissions = std::fs::metadata(&fail).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fail, permissions).unwrap();
        let err = DefuddleModule::with_executable(&fail)
            .convert_url(
                "https://example.com/x",
                OutputFormat::MARKDOWN,
                &ConversionOptions::default(),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("boom") || err.to_string().contains("Defuddle"),
            "{err}"
        );
        // Stderr redaction: credentials in process output must not leak.
        assert!(!err.to_string().contains("s3cret"));

        unsafe {
            std::env::remove_var("SHIFT_CURL_BIN");
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[cfg(unix)]
    #[test]
    fn convert_local_html_rejects_unsupported_output() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir();
        let suffix = format!("{}-local-bad", std::process::id());
        let executable = directory.join(format!("shift-defuddle-local-bad-{suffix}"));
        let input = directory.join(format!("shift-defuddle-input-{suffix}.html"));
        std::fs::write(&executable, "#!/bin/sh\nprintf 'x'\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        std::fs::write(&input, "<p>x</p>").unwrap();

        let err = DefuddleModule::with_executable(&executable)
            .convert(&input, OutputFormat::PDF, &ConversionOptions::default())
            .unwrap_err();
        assert!(
            err.to_string().contains("Markdown")
                || err.to_string().contains("HTML")
                || err.to_string().contains("PDF"),
            "{err}"
        );

        let _ = std::fs::remove_file(&executable);
        let _ = std::fs::remove_file(&input);
    }

    #[test]
    fn module_metadata_and_extensions() {
        let module = DefuddleModule::with_executable("defuddle");
        assert_eq!(module.id(), "defuddle");
        assert_eq!(module.label(), "Defuddle");
        assert!(module.input_extensions().contains(&"html"));
        assert!(module.input_extensions().contains(&"htm"));
        assert!(module.output_formats().contains(&OutputFormat::MARKDOWN));
        assert!(module.output_formats().contains(&OutputFormat::HTML));
        assert!(
            module
                .chainable_output_formats()
                .contains(&OutputFormat::MARKDOWN)
        );
    }

    #[test]
    fn defuddle_options_default() {
        let opts = DefuddleOptions::default();
        assert!(!opts.frontmatter);
        assert!(opts.lang.is_none());
    }

    #[test]
    fn redact_credentials_in_text_strips_userinfo_substrings() {
        let raw = "fetch failed for https://user:s3cret@example.com/a and http://onlyuser@host/x";
        let redacted = redact_credentials_in_text(raw);
        assert!(!redacted.contains("s3cret"));
        assert!(!redacted.contains("user:"));
        assert!(!redacted.contains("onlyuser@"));
        assert!(redacted.contains("https://example.com/a"));
        assert!(redacted.contains("http://host/x"));
        // Non-URL text is preserved.
        assert_eq!(redact_credentials_in_text("no urls here"), "no urls here");
    }

    #[test]
    fn dns_policy_fail_closed_on_nxdomain_when_blocking() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
        }
        assert!(block_private_urls());
        // Unresolvable name: fail closed (no fail-open on lookup Err).
        let err =
            ensure_public_url_fetch_allowed("http://this-host-should-not-resolve.invalid./path")
                .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("DNS") || msg.contains("non-public") || msg.contains("public internet"),
            "expected DNS/policy failure, got: {msg}"
        );
        assert!(!msg.contains("s3cret"));
        // Boolean helper also fails closed.
        assert!(url_resolves_to_non_public_host(
            "http://this-host-should-not-resolve.invalid./"
        ));
    }

    #[test]
    fn resolve_public_literal_ip_without_dns() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("SHIFT_ALLOW_PRIVATE_URLS");
            std::env::remove_var("SHIFT_BLOCK_PRIVATE_URLS");
        }
        let addrs = resolve_public_url_addresses_with_cancel("https://8.8.8.8/", None).unwrap();
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].ip().to_string(), "8.8.8.8");

        let err = resolve_public_url_addresses_with_cancel("http://127.0.0.1/", None).unwrap_err();
        assert!(
            err.to_string().contains("non-public") || err.to_string().contains("public internet"),
            "error: {err}"
        );
    }

    #[test]
    fn dns_preflight_honours_cancel_flag() {
        let cancel = Arc::new(AtomicBool::new(true));
        let err = resolve_public_url_addresses_with_cancel("https://example.com/", Some(cancel))
            .unwrap_err();
        assert!(err.is_cancelled(), "error: {err}");
    }
}
