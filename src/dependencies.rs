//! Verified, app-managed converter dependencies.
//!
//! This module deliberately has no package-manager integration.  A release
//! embeds an immutable manifest; installation downloads only its HTTPS release
//! archives, verifies them, extracts them into private staging, probes them,
//! and atomically records the active component.  Conversion code can therefore
//! use managed tools without executing a shell, `pip`, `npm`, or Homebrew.

use crate::session_settings::application_support_dir;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use url::Url;
use zip::ZipArchive;

const MANIFEST: &str = include_str!("../dependencies/manifest.json");
const STATE_FILE: &str = "state.json";
const MAX_COMPONENT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 20_000;
static STAGING_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct DependencyManifest {
    pub schema_version: u32,
    pub release_version: String,
    #[serde(default)]
    pub components: Vec<DependencyComponent>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct DependencyComponent {
    pub id: String,
    pub capability: DependencyCapability,
    pub architecture: String,
    pub url: String,
    pub sha256: String,
    pub compressed_bytes: u64,
    pub unpacked_bytes: u64,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub executables: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<DependencyFile>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyCapability {
    DocumentsMarkdown,
    WebExtraction,
    MediaTranscription,
    PdfPublishingToolkit,
}

impl DependencyCapability {
    pub fn label(self) -> &'static str {
        match self {
            Self::DocumentsMarkdown => "Documents & Markdown",
            Self::WebExtraction => "Web extraction",
            Self::MediaTranscription => "Media & transcription",
            Self::PdfPublishingToolkit => "PDF publishing & toolkit",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct DependencyFile {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InstallSelection {
    pub capabilities: Vec<DependencyCapability>,
    /// Component IDs explicitly requested even if a system copy is ready.
    pub replace_with_managed: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstallProgress {
    Downloading {
        component: String,
        completed: u64,
        total: u64,
    },
    Verifying {
        component: String,
    },
    Activating {
        component: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallOutcome {
    pub installed: Vec<String>,
    pub failed: Vec<(String, String)>,
}

impl InstallOutcome {
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ManagedDependencyState {
    pub manifest_version: String,
    #[serde(default)]
    pub components: BTreeMap<String, ActiveComponent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActiveComponent {
    pub directory: String,
    pub sha256: String,
}

#[derive(Debug)]
pub struct DependencyError(String);

impl std::fmt::Display for DependencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for DependencyError {}
impl From<io::Error> for DependencyError {
    fn from(value: io::Error) -> Self {
        Self(value.to_string())
    }
}

pub fn embedded_manifest() -> Result<DependencyManifest, DependencyError> {
    // The source manifest keeps development/test builds deterministic. Release
    // packaging writes the exact component hashes into Resources before signing
    // the app, so an installed app never learns trust data from the network.
    let manifest_text = release_manifest_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_else(|| MANIFEST.to_owned());
    let manifest: DependencyManifest = serde_json::from_str(&manifest_text).map_err(|error| {
        DependencyError(format!("invalid embedded dependency manifest: {error}"))
    })?;
    if manifest.schema_version != 1 {
        return Err(DependencyError(
            "unsupported dependency manifest schema".into(),
        ));
    }
    Ok(manifest)
}

fn release_manifest_path() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?.canonicalize().ok()?;
    executable.ancestors().find_map(|ancestor| {
        (ancestor.file_name().and_then(|name| name.to_str()) == Some("Contents"))
            .then(|| ancestor.join("Resources/dependency-manifest.json"))
    })
}

pub fn dependency_root() -> Option<PathBuf> {
    application_support_dir().map(|path| path.join("dependencies"))
}

fn state_path() -> Option<PathBuf> {
    dependency_root().map(|path| path.join(STATE_FILE))
}

pub fn managed_state() -> ManagedDependencyState {
    let Some(path) = state_path() else {
        return ManagedDependencyState::default();
    };
    let Ok(bytes) = fs::read(path) else {
        return ManagedDependencyState::default();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Returns only a tool from a component recorded by a verified install state.
/// Environment overrides remain the caller's responsibility and take priority.
pub fn managed_runtime_tool(name: &str) -> Option<PathBuf> {
    let manifest = embedded_manifest().ok()?;
    let root = dependency_root()?;
    let state = managed_state();
    for component in manifest.components {
        let Some(relative) = component.executables.get(name) else {
            continue;
        };
        let Some(active) = state.components.get(&component.id) else {
            continue;
        };
        if active.sha256 != component.sha256 || state.manifest_version != manifest.release_version {
            continue;
        }
        if !is_safe_relative(&active.directory) {
            continue;
        }
        let candidate = root.join(&active.directory).join(relative);
        if is_safe_relative(relative) && is_regular_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

pub fn components_for_selection(
    selection: &InstallSelection,
) -> Result<Vec<DependencyComponent>, DependencyError> {
    let manifest = embedded_manifest()?;
    let arch = std::env::consts::ARCH;
    let mut wanted: BTreeMap<String, DependencyComponent> = manifest
        .components
        .into_iter()
        .filter(|component| component.architecture == arch)
        .filter(|component| {
            selection.capabilities.contains(&component.capability)
                || selection.replace_with_managed.contains(&component.id)
        })
        .map(|component| (component.id.clone(), component))
        .collect();
    let all = embedded_manifest()?
        .components
        .into_iter()
        .filter(|component| component.architecture == arch)
        .map(|component| (component.id.clone(), component))
        .collect::<BTreeMap<_, _>>();
    let mut cursor: Vec<String> = wanted
        .values()
        .flat_map(|component| component.requires.clone())
        .collect();
    while let Some(id) = cursor.pop() {
        if wanted.contains_key(&id) {
            continue;
        }
        let component = all
            .get(&id)
            .cloned()
            .ok_or_else(|| DependencyError(format!("manifest dependency {id} is missing")))?;
        cursor.extend(component.requires.clone());
        wanted.insert(id, component);
    }
    Ok(wanted.into_values().collect())
}

/// Installs every selected component independently so a failure never removes
/// a previous working component. `progress` runs on this background thread.
pub fn install_selected(
    selection: &InstallSelection,
    cancelled: &std::sync::atomic::AtomicBool,
    progress: impl Fn(InstallProgress),
) -> Result<InstallOutcome, DependencyError> {
    let manifest = embedded_manifest()?;
    let mut outcome = InstallOutcome {
        installed: Vec::new(),
        failed: Vec::new(),
    };
    for component in components_for_selection(selection)? {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        match install_component(&manifest, &component, cancelled, &progress) {
            Ok(()) => outcome.installed.push(component.id),
            Err(error) => outcome.failed.push((component.id, error.to_string())),
        }
    }
    Ok(outcome)
}

fn install_component(
    manifest: &DependencyManifest,
    component: &DependencyComponent,
    cancelled: &std::sync::atomic::AtomicBool,
    progress: &impl Fn(InstallProgress),
) -> Result<(), DependencyError> {
    validate_component(component)?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(DependencyError("installation cancelled".into()));
    }
    let root = dependency_root()
        .ok_or_else(|| DependencyError("Application Support is unavailable".into()))?;
    ensure_private_dir(&root)?;
    let staging = root.join(format!(
        ".staging-{}-{}",
        std::process::id(),
        STAGING_ID.fetch_add(1, Ordering::Relaxed)
    ));
    ensure_private_dir(&staging)?;
    let archive = staging.join("component.zip");
    progress(InstallProgress::Downloading {
        component: component.id.clone(),
        completed: 0,
        total: component.compressed_bytes,
    });
    let result = (|| {
        download_component(component, &archive)?;
        if cancelled.load(Ordering::Relaxed) {
            return Err(DependencyError("installation cancelled".into()));
        }
        progress(InstallProgress::Verifying {
            component: component.id.clone(),
        });
        verify_file_digest(&archive, &component.sha256, component.compressed_bytes)?;
        let payload = staging.join("payload");
        extract_component(&archive, &payload, component)?;
        verify_component_files(&payload, component)?;
        probe_component(&payload, component)?;
        if cancelled.load(Ordering::Relaxed) {
            return Err(DependencyError("installation cancelled".into()));
        }
        progress(InstallProgress::Activating {
            component: component.id.clone(),
        });
        activate_component(&root, manifest, component, &payload)
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

fn validate_component(component: &DependencyComponent) -> Result<(), DependencyError> {
    if component.id.is_empty()
        || component.id.contains('/')
        || component.compressed_bytes == 0
        || component.compressed_bytes > MAX_COMPONENT_BYTES
        || component.unpacked_bytes > MAX_COMPONENT_BYTES
        || !valid_digest(&component.sha256)
    {
        return Err(DependencyError("invalid component manifest entry".into()));
    }
    let url = Url::parse(&component.url)
        .map_err(|_| DependencyError("invalid component download URL".into()))?;
    if url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || !url.path().starts_with("/dbuldum4/shift/releases/download/")
    {
        return Err(DependencyError(
            "component URL is outside Shift's release namespace".into(),
        ));
    }
    Ok(())
}

fn download_component(
    component: &DependencyComponent,
    destination: &Path,
) -> Result<(), DependencyError> {
    let status = Command::new("/usr/bin/curl")
        .arg("--fail")
        .arg("--location")
        .arg("--proto")
        .arg("=https")
        .arg("--tlsv1.2")
        .arg("--max-filesize")
        .arg(component.compressed_bytes.to_string())
        .arg("--output")
        .arg(destination)
        .arg(&component.url)
        .status()
        .map_err(|error| {
            DependencyError(format!("could not start macOS download service: {error}"))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(DependencyError(format!("download failed ({status})")))
    }
}

fn extract_component(
    archive: &Path,
    destination: &Path,
    component: &DependencyComponent,
) -> Result<(), DependencyError> {
    ensure_private_dir(destination)?;
    let file = File::open(archive)?;
    let mut zip = ZipArchive::new(file)
        .map_err(|error| DependencyError(format!("invalid component archive: {error}")))?;
    if zip.len() > MAX_ARCHIVE_ENTRIES {
        return Err(DependencyError(
            "component archive has too many files".into(),
        ));
    }
    let mut unpacked = 0u64;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|error| DependencyError(error.to_string()))?;
        let name = entry.name().to_owned();
        if !is_safe_relative(&name) || entry.is_symlink() {
            return Err(DependencyError(
                "component archive contains an unsafe path".into(),
            ));
        }
        unpacked = unpacked
            .checked_add(entry.size())
            .ok_or_else(|| DependencyError("component archive is too large".into()))?;
        if unpacked > component.unpacked_bytes || unpacked > MAX_COMPONENT_BYTES {
            return Err(DependencyError(
                "component archive exceeds its declared size".into(),
            ));
        }
        let output = destination.join(&name);
        if entry.is_dir() {
            ensure_private_dir(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            ensure_private_dir(parent)?;
        }
        let mut out = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&output)?;
        io::copy(&mut entry, &mut out)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                &output,
                fs::Permissions::from_mode(if entry.unix_mode().unwrap_or(0) & 0o111 != 0 {
                    0o700
                } else {
                    0o600
                }),
            )?;
        }
    }
    Ok(())
}

fn verify_component_files(
    root: &Path,
    component: &DependencyComponent,
) -> Result<(), DependencyError> {
    for expected in &component.files {
        if !is_safe_relative(&expected.path) || !valid_digest(&expected.sha256) {
            return Err(DependencyError(
                "invalid component file manifest entry".into(),
            ));
        }
        verify_file_digest(&root.join(&expected.path), &expected.sha256, expected.bytes)?;
        if expected.executable && !is_regular_executable(&root.join(&expected.path)) {
            return Err(DependencyError(format!(
                "declared executable {} is not executable",
                expected.path
            )));
        }
    }
    Ok(())
}

fn probe_component(root: &Path, component: &DependencyComponent) -> Result<(), DependencyError> {
    for path in component.executables.values() {
        let executable = root.join(path);
        if !is_safe_relative(path) || !is_regular_executable(&executable) {
            return Err(DependencyError(format!(
                "component executable {path} is unavailable"
            )));
        }
        let status = Command::new(executable)
            .arg("--version")
            .env_clear()
            .status()
            .map_err(|error| {
                DependencyError(format!("could not probe managed executable: {error}"))
            })?;
        if !status.success() {
            return Err(DependencyError(
                "managed executable did not pass its version probe".into(),
            ));
        }
    }
    Ok(())
}

fn activate_component(
    root: &Path,
    manifest: &DependencyManifest,
    component: &DependencyComponent,
    payload: &Path,
) -> Result<(), DependencyError> {
    let directory = format!(
        "{}-{}-{}",
        component.id,
        &component.sha256[..12],
        STAGING_ID.fetch_add(1, Ordering::Relaxed)
    );
    let target = root.join(&directory);
    fs::rename(payload, &target)?;
    let mut state = managed_state();
    state.manifest_version = manifest.release_version.clone();
    state.components.insert(
        component.id.clone(),
        ActiveComponent {
            directory,
            sha256: component.sha256.clone(),
        },
    );
    let bytes =
        serde_json::to_vec_pretty(&state).map_err(|error| DependencyError(error.to_string()))?;
    let temporary = root.join(format!(".{STATE_FILE}.tmp-{}", std::process::id()));
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, root.join(STATE_FILE))?;
    Ok(())
}

fn verify_file_digest(path: &Path, expected: &str, bytes: u64) -> Result<(), DependencyError> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() != bytes {
        return Err(DependencyError(
            "download size did not match the manifest".into(),
        ));
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    if format!("{:x}", hasher.finalize()) != expected.to_ascii_lowercase() {
        return Err(DependencyError("download integrity check failed".into()));
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn is_safe_relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn is_regular_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unsafe_archive_paths() {
        assert!(!is_safe_relative("../x"));
        assert!(!is_safe_relative("/x"));
        assert!(is_safe_relative("bin/tool"));
    }
    #[test]
    fn embedded_manifest_is_valid() {
        assert!(embedded_manifest().is_ok());
    }
}
