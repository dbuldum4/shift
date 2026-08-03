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
use std::collections::{BTreeMap, BTreeSet};
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
    /// Additional capabilities provided by the same archive. For example,
    /// the Apple Silicon document archive also contains Docling's ASR stack.
    #[serde(default)]
    pub provides: Vec<DependencyCapability>,
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

impl DependencyComponent {
    fn supports_capability(&self, capability: DependencyCapability) -> bool {
        self.capability == capability || self.provides.contains(&capability)
    }
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

pub fn managed_state() -> ManagedDependencyState {
    let Some(root) = dependency_root() else {
        return ManagedDependencyState::default();
    };
    managed_state_at(&root)
}

fn managed_state_at(root: &Path) -> ManagedDependencyState {
    let Ok(bytes) = fs::read(root.join(STATE_FILE)) else {
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
    managed_runtime_tool_from(&manifest, &root, &state, name)
}

fn managed_runtime_tool_from(
    manifest: &DependencyManifest,
    root: &Path,
    state: &ManagedDependencyState,
    name: &str,
) -> Option<PathBuf> {
    for component in &manifest.components {
        let Some(relative) = component.executables.get(name) else {
            continue;
        };
        let Some(active) = state.components.get(&component.id) else {
            continue;
        };
        // Component archives are immutable and authenticated by their digest.
        // A new app release may continue to publish the exact same component,
        // so the app-level release version must not invalidate it.
        if active.sha256 != component.sha256 {
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

fn manifest_architecture_for(target_arch: &str) -> &str {
    match target_arch {
        // The release packager uses `uname -m`, which calls Apple Silicon
        // `arm64`; Rust's target architecture name is `aarch64`.
        "aarch64" => "arm64",
        other => other,
    }
}

fn current_manifest_architecture() -> &'static str {
    manifest_architecture_for(std::env::consts::ARCH)
}

fn capabilities_for_components(
    components: &[DependencyComponent],
    architecture: &str,
) -> Vec<DependencyCapability> {
    let mut capabilities = BTreeSet::new();
    for component in components
        .iter()
        .filter(|component| component.architecture == architecture)
    {
        capabilities.insert(component.capability);
        capabilities.extend(component.provides.iter().copied());
    }
    capabilities.into_iter().collect()
}

/// Capabilities backed by at least one component for this release and CPU.
/// The onboarding UI uses this instead of advertising future/unpublished
/// dependency groups that would otherwise install nothing.
pub fn available_capabilities() -> Result<Vec<DependencyCapability>, DependencyError> {
    let manifest = embedded_manifest()?;
    Ok(capabilities_for_components(
        &manifest.components,
        current_manifest_architecture(),
    ))
}

pub fn components_for_selection(
    selection: &InstallSelection,
) -> Result<Vec<DependencyComponent>, DependencyError> {
    let manifest = embedded_manifest()?;
    let arch = current_manifest_architecture();
    let mut wanted: BTreeMap<String, DependencyComponent> = manifest
        .components
        .into_iter()
        .filter(|component| component.architecture == arch)
        .filter(|component| {
            selection
                .capabilities
                .iter()
                .copied()
                .any(|capability| component.supports_capability(capability))
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
    let activation_id = STAGING_ID.fetch_add(1, Ordering::Relaxed);
    let directory = format!(
        "{}-{}-{}-{}",
        component.id,
        &component.sha256[..12],
        std::process::id(),
        activation_id,
    );
    let target = root.join(&directory);
    let mut state = managed_state_at(root);
    state.manifest_version = manifest.release_version.clone();
    state.components.insert(
        component.id.clone(),
        ActiveComponent {
            directory: directory.clone(),
            sha256: component.sha256.clone(),
        },
    );
    let bytes =
        serde_json::to_vec_pretty(&state).map_err(|error| DependencyError(error.to_string()))?;
    fs::rename(payload, &target)?;
    let temporary = root.join(format!(
        ".{STATE_FILE}.tmp-{}-{activation_id}",
        std::process::id()
    ));
    if let Err(error) =
        fs::write(&temporary, bytes).and_then(|()| fs::rename(&temporary, root.join(STATE_FILE)))
    {
        let _ = fs::remove_file(&temporary);
        let _ = fs::remove_dir_all(&target);
        return Err(error.into());
    }

    // State now points at the new runtime. Remove every older directory that
    // follows Shift's private naming scheme, including orphans left by an app
    // termination after the payload rename but before the state swap.
    cleanup_inactive_component_directories(root, &component.id, &directory);
    Ok(())
}

fn cleanup_inactive_component_directories(root: &Path, component_id: &str, active: &str) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == active || !is_component_directory_name(component_id, name) {
            continue;
        }
        // Do not follow or remove a symlink even if its name resembles one of
        // ours. Installed payloads are always real directories.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn is_component_directory_name(component_id: &str, name: &str) -> bool {
    let prefix = format!("{component_id}-");
    let Some(suffix) = name.strip_prefix(&prefix) else {
        return false;
    };
    let mut parts = suffix.split('-');
    let Some(digest_prefix) = parts.next() else {
        return false;
    };
    if digest_prefix.len() != 12 || !digest_prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let ids = parts.collect::<Vec<_>>();
    (ids.len() == 1 || ids.len() == 2)
        && ids
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
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

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "shift-dependencies-{label}-{}-{}",
                std::process::id(),
                STAGING_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_component(
        id: &str,
        architecture: &str,
        capability: DependencyCapability,
        provides: Vec<DependencyCapability>,
    ) -> DependencyComponent {
        DependencyComponent {
            id: id.into(),
            capability,
            architecture: architecture.into(),
            url: "https://github.com/dbuldum4/shift/releases/download/v1.0.0/test.zip".into(),
            sha256: "0".repeat(64),
            compressed_bytes: 1,
            unpacked_bytes: 1,
            requires: Vec::new(),
            provides,
            executables: BTreeMap::new(),
            files: Vec::new(),
        }
    }

    #[test]
    fn release_architecture_names_match_packager() {
        assert_eq!(manifest_architecture_for("aarch64"), "arm64");
        assert_eq!(manifest_architecture_for("x86_64"), "x86_64");
    }

    #[test]
    fn component_provides_secondary_capabilities() {
        let component = test_component(
            "documents-markdown",
            "arm64",
            DependencyCapability::DocumentsMarkdown,
            vec![DependencyCapability::MediaTranscription],
        );
        assert!(component.supports_capability(DependencyCapability::DocumentsMarkdown));
        assert!(component.supports_capability(DependencyCapability::MediaTranscription));
        assert!(!component.supports_capability(DependencyCapability::PdfPublishingToolkit));
    }

    #[test]
    fn available_capabilities_are_architecture_specific() {
        let components = vec![
            test_component(
                "documents-arm",
                "arm64",
                DependencyCapability::DocumentsMarkdown,
                vec![DependencyCapability::MediaTranscription],
            ),
            test_component(
                "documents-intel",
                "x86_64",
                DependencyCapability::DocumentsMarkdown,
                Vec::new(),
            ),
        ];
        assert_eq!(
            capabilities_for_components(&components, "arm64"),
            vec![
                DependencyCapability::DocumentsMarkdown,
                DependencyCapability::MediaTranscription
            ]
        );
        assert_eq!(
            capabilities_for_components(&components, "x86_64"),
            vec![DependencyCapability::DocumentsMarkdown]
        );
    }

    #[test]
    fn matching_component_remains_active_across_app_releases() {
        let root = TestDirectory::new("upgrade");
        let mut component = test_component(
            "documents-markdown",
            current_manifest_architecture(),
            DependencyCapability::DocumentsMarkdown,
            Vec::new(),
        );
        component
            .executables
            .insert("markitdown".into(), "bin/markitdown".into());
        let directory = "documents-markdown-000000000000-1";
        let executable = root.0.join(directory).join("bin/markitdown");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let manifest = DependencyManifest {
            schema_version: 1,
            release_version: "2.0.0".into(),
            components: vec![component.clone()],
        };
        let mut state = ManagedDependencyState {
            manifest_version: "1.0.0".into(),
            components: BTreeMap::new(),
        };
        state.components.insert(
            component.id.clone(),
            ActiveComponent {
                directory: directory.into(),
                sha256: component.sha256.clone(),
            },
        );

        assert_eq!(
            managed_runtime_tool_from(&manifest, &root.0, &state, "markitdown"),
            Some(executable)
        );

        state.components.get_mut(&component.id).unwrap().sha256 = "f".repeat(64);
        assert_eq!(
            managed_runtime_tool_from(&manifest, &root.0, &state, "markitdown"),
            None,
            "a component with a different authenticated archive must stay inactive"
        );
    }

    #[test]
    fn activation_removes_stale_component_copies_only_after_state_swap() {
        let root = TestDirectory::new("activation");
        let component = test_component(
            "documents-markdown",
            current_manifest_architecture(),
            DependencyCapability::DocumentsMarkdown,
            Vec::new(),
        );
        let manifest = DependencyManifest {
            schema_version: 1,
            release_version: "2.0.0".into(),
            components: vec![component.clone()],
        };
        let old = "documents-markdown-000000000000-7";
        let orphan = "documents-markdown-000000000000-123-8";
        let unrelated = "documents-markdown-user-backup";
        fs::create_dir(root.0.join(old)).unwrap();
        fs::create_dir(root.0.join(orphan)).unwrap();
        fs::create_dir(root.0.join(unrelated)).unwrap();
        let mut previous = ManagedDependencyState {
            manifest_version: "1.0.0".into(),
            components: BTreeMap::new(),
        };
        previous.components.insert(
            component.id.clone(),
            ActiveComponent {
                directory: old.into(),
                sha256: component.sha256.clone(),
            },
        );
        fs::write(
            root.0.join(STATE_FILE),
            serde_json::to_vec(&previous).unwrap(),
        )
        .unwrap();
        let payload = root.0.join("payload");
        fs::create_dir(&payload).unwrap();
        fs::write(payload.join("runtime"), b"new").unwrap();

        activate_component(&root.0, &manifest, &component, &payload).unwrap();

        let active = managed_state_at(&root.0);
        let active = active.components.get(&component.id).unwrap();
        assert_eq!(active.sha256, component.sha256);
        assert!(root.0.join(&active.directory).join("runtime").is_file());
        assert!(!root.0.join(old).exists());
        assert!(!root.0.join(orphan).exists());
        assert!(root.0.join(unrelated).is_dir());
    }

    #[test]
    fn component_directory_names_are_strictly_scoped() {
        assert!(is_component_directory_name(
            "web-extraction",
            "web-extraction-012345abcdef-4"
        ));
        assert!(is_component_directory_name(
            "web-extraction",
            "web-extraction-012345abcdef-123-4"
        ));
        assert!(!is_component_directory_name(
            "web-extraction",
            "web-extraction-user-backup"
        ));
        assert!(!is_component_directory_name(
            "web-extraction",
            "documents-markdown-012345abcdef-4"
        ));
    }

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
