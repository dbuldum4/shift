//! Named, reusable conversion recipes shared by the native app and `shift-cli`.
//!
//! Recipes intentionally reuse [`SessionConversionOptions`]: it is the single
//! persisted representation of conversion knobs and excludes runtime callbacks
//! plus the PDF password. Recipe files are versioned and written atomically.

use crate::conversion::{BatchNamingTemplate, ConversionOptions, OutputFormat};
use crate::session_settings::{SessionConversionOptions, application_support_dir};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const RECIPE_STORE_VERSION: u32 = 1;
pub const RECIPE_STORE_FILE_NAME: &str = "conversion-recipes.json";
pub const MAX_RECIPE_NAME_CHARS: usize = 80;
pub const MAX_NAMING_TEMPLATE_CHARS: usize = 160;

/// Optional output location and file-name behavior captured by a recipe.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RecipeDestination {
    /// Directory used when the caller does not provide `-o` / `-O`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<PathBuf>,
    /// File-name template using the shared batch placeholders
    /// (`{stem}`, `{parent}`, `{format}`, `{ext}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub naming_template: Option<String>,
    /// Replace an existing output. The source itself is still never overwritten.
    #[serde(default)]
    pub overwrite: bool,
}

impl RecipeDestination {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// A complete reusable conversion setup. Secrets and runtime hooks are absent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConversionRecipe {
    pub name: String,
    pub output_format: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_module: Option<String>,
    #[serde(default)]
    pub options: SessionConversionOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<RecipeDestination>,
}

impl ConversionRecipe {
    pub fn new(
        name: impl Into<String>,
        output_format: OutputFormat,
        preferred_module: Option<String>,
        options: &ConversionOptions,
        destination: Option<RecipeDestination>,
    ) -> Result<Self, RecipeError> {
        let recipe = Self {
            name: name.into().trim().to_owned(),
            output_format: output_format.id().to_owned(),
            preferred_module: preferred_module
                .map(|module| module.trim().to_owned())
                .filter(|module| !module.is_empty()),
            options: SessionConversionOptions::from_conversion_options(options),
            destination: destination.filter(|policy| !policy.is_default()),
        };
        recipe.validate()?;
        Ok(recipe)
    }

    pub fn parsed_output_format(&self) -> Result<OutputFormat, RecipeError> {
        self.output_format.parse().map_err(|_| {
            RecipeError::Invalid(format!(
                "recipe `{}` has unknown output format `{}`",
                self.name, self.output_format
            ))
        })
    }

    pub fn to_conversion_options(&self) -> ConversionOptions {
        // SessionConversionOptions deliberately reconstructs password/cancel/progress as None.
        self.options.to_conversion_options()
    }

    pub fn validate(&self) -> Result<(), RecipeError> {
        validate_recipe_name(&self.name)?;
        if self.name != self.name.trim() {
            return Err(RecipeError::Invalid(
                "recipe name cannot begin or end with whitespace".to_owned(),
            ));
        }
        self.parsed_output_format()?;
        if let Some(module) = self.preferred_module.as_deref() {
            if module.trim().is_empty() {
                return Err(RecipeError::Invalid(format!(
                    "recipe `{}` has an empty preferred module",
                    self.name
                )));
            }
            if module != module.trim() {
                return Err(RecipeError::Invalid(format!(
                    "recipe `{}` has whitespace around its preferred module",
                    self.name
                )));
            }
        }
        if let Some(destination) = &self.destination
            && let Some(template) = destination.naming_template.as_deref()
        {
            validate_naming_template(template)?;
        }
        Ok(())
    }
}

/// On-disk collection. Recipe lookup is case-insensitive to match macOS usage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecipeStore {
    pub version: u32,
    #[serde(default)]
    pub recipes: Vec<ConversionRecipe>,
}

impl Default for RecipeStore {
    fn default() -> Self {
        Self {
            version: RECIPE_STORE_VERSION,
            recipes: Vec::new(),
        }
    }
}

impl RecipeStore {
    pub fn get(&self, name: &str) -> Option<&ConversionRecipe> {
        let name = name.trim();
        self.recipes
            .iter()
            .find(|recipe| recipe.name.eq_ignore_ascii_case(name))
    }

    /// Insert or replace by case-insensitive name. Returns true when replaced.
    pub fn upsert(&mut self, recipe: ConversionRecipe) -> Result<bool, RecipeError> {
        recipe.validate()?;
        let replaced = if let Some(existing) = self
            .recipes
            .iter_mut()
            .find(|existing| existing.name.eq_ignore_ascii_case(&recipe.name))
        {
            *existing = recipe;
            true
        } else {
            self.recipes.push(recipe);
            false
        };
        self.sort();
        Ok(replaced)
    }

    pub fn delete(&mut self, name: &str) -> bool {
        let before = self.recipes.len();
        self.recipes
            .retain(|recipe| !recipe.name.eq_ignore_ascii_case(name.trim()));
        before != self.recipes.len()
    }

    pub fn validate(&self) -> Result<(), RecipeError> {
        if self.version != RECIPE_STORE_VERSION {
            return Err(RecipeError::UnsupportedVersion {
                found: self.version,
                supported: RECIPE_STORE_VERSION,
            });
        }
        for recipe in &self.recipes {
            recipe.validate()?;
        }
        for (index, recipe) in self.recipes.iter().enumerate() {
            if self.recipes[..index]
                .iter()
                .any(|other| other.name.eq_ignore_ascii_case(&recipe.name))
            {
                return Err(RecipeError::Invalid(format!(
                    "duplicate recipe name `{}`",
                    recipe.name
                )));
            }
        }
        Ok(())
    }

    fn sort(&mut self) {
        self.recipes
            .sort_by_cached_key(|recipe| recipe.name.to_ascii_lowercase());
    }
}

#[derive(Debug)]
pub enum RecipeError {
    Io(io::Error),
    Invalid(String),
    UnsupportedVersion { found: u32, supported: u32 },
}

impl std::fmt::Display for RecipeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Invalid(message) => f.write_str(message),
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "recipe store version {found} is newer than supported version {supported}"
            ),
        }
    }
}

impl std::error::Error for RecipeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for RecipeError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn validate_recipe_name(name: &str) -> Result<(), RecipeError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(RecipeError::Invalid(
            "recipe name cannot be empty".to_owned(),
        ));
    }
    if trimmed.chars().count() > MAX_RECIPE_NAME_CHARS {
        return Err(RecipeError::Invalid(format!(
            "recipe name is longer than {MAX_RECIPE_NAME_CHARS} characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(RecipeError::Invalid(
            "recipe name cannot contain control characters".to_owned(),
        ));
    }
    Ok(())
}

/// Validate a recipe naming template via the shared batch policy.
///
/// Recipes and the batch queue accept the same placeholders so a saved template
/// cannot diverge from what `run_batch` will render later.
pub fn validate_naming_template(template: &str) -> Result<(), RecipeError> {
    let trimmed = template.trim();
    if trimmed.chars().count() > MAX_NAMING_TEMPLATE_CHARS {
        return Err(RecipeError::Invalid(format!(
            "naming template is longer than {MAX_NAMING_TEMPLATE_CHARS} characters"
        )));
    }
    trimmed
        .parse::<BatchNamingTemplate>()
        .map(|_| ())
        .map_err(|error| RecipeError::Invalid(error.to_string()))
}

pub fn default_recipe_store_path() -> Option<PathBuf> {
    application_support_dir().map(|directory| directory.join(RECIPE_STORE_FILE_NAME))
}

pub fn load_recipe_store(path: impl AsRef<Path>) -> Result<RecipeStore, RecipeError> {
    let path = path.as_ref();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RecipeStore::default()),
        Err(error) => return Err(error.into()),
    };
    let mut store: RecipeStore = serde_json::from_slice(&bytes).map_err(|error| {
        RecipeError::Invalid(format!(
            "could not parse recipe store {}: {error}",
            path.display()
        ))
    })?;
    store.validate()?;
    store.sort();
    Ok(store)
}

pub fn load_default_recipe_store() -> Result<RecipeStore, RecipeError> {
    let path = default_recipe_store_path().ok_or_else(|| {
        RecipeError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate recipe storage directory",
        ))
    })?;
    load_recipe_store(path)
}

pub fn save_recipe_store(path: impl AsRef<Path>, store: &RecipeStore) -> Result<(), RecipeError> {
    let path = path.as_ref();
    let mut payload = store.clone();
    payload.version = RECIPE_STORE_VERSION;
    payload.sort();
    payload.validate()?;
    let json = serde_json::to_vec_pretty(&payload)
        .map_err(|error| RecipeError::Invalid(format!("could not serialize recipes: {error}")))?;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let token = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(RECIPE_STORE_FILE_NAME);
    let temporary = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), token));

    let result = (|| -> Result<(), RecipeError> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&json)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub fn save_default_recipe_store(store: &RecipeStore) -> Result<(), RecipeError> {
    let path = default_recipe_store_path().ok_or_else(|| {
        RecipeError::Io(io::Error::new(
            io::ErrorKind::NotFound,
            "could not locate recipe storage directory",
        ))
    })?;
    save_recipe_store(path, store)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversion::{FfmpegQuality, ProgressSink};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shift-recipes-{}-{}-{name}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn round_trip_excludes_secrets_and_runtime_hooks() {
        let path = temp_path("redaction");
        let mut options = ConversionOptions::default();
        options.ffmpeg.quality = FfmpegQuality::High;
        options.pdf.password = Some("do-not-store".into());
        options.pdf.page_from = Some(2);
        options.cancel = Some(Arc::new(AtomicBool::new(false)));
        let sink: ProgressSink = Arc::new(|_| {});
        options.progress = Some(sink);

        let recipe = ConversionRecipe::new(
            "Web media",
            OutputFormat::MP4,
            Some("ffmpeg".into()),
            &options,
            Some(RecipeDestination {
                output_dir: Some(PathBuf::from("/tmp/exports")),
                naming_template: Some("{stem}-web.{ext}".into()),
                overwrite: true,
            }),
        )
        .unwrap();
        let mut store = RecipeStore::default();
        store.upsert(recipe).unwrap();
        save_recipe_store(&path, &store).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("do-not-store"));
        assert!(!raw.contains("cancel"));
        assert!(!raw.contains("progress"));
        let loaded = load_recipe_store(&path).unwrap();
        let restored = loaded.get("WEB MEDIA").unwrap().to_conversion_options();
        assert_eq!(restored.ffmpeg.quality, FfmpegQuality::High);
        assert_eq!(restored.pdf.page_from, Some(2));
        assert!(restored.pdf.password.is_none());
        assert!(restored.cancel.is_none());
        assert!(restored.progress.is_none());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn store_sorts_replaces_and_deletes_case_insensitively() {
        let options = ConversionOptions::default();
        let mut store = RecipeStore::default();
        store
            .upsert(ConversionRecipe::new("Zulu", OutputFormat::PDF, None, &options, None).unwrap())
            .unwrap();
        store
            .upsert(
                ConversionRecipe::new("alpha", OutputFormat::HTML, None, &options, None).unwrap(),
            )
            .unwrap();
        assert_eq!(
            store
                .recipes
                .iter()
                .map(|recipe| recipe.name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "Zulu"]
        );
        assert!(
            store
                .upsert(
                    ConversionRecipe::new("ALPHA", OutputFormat::DOCX, None, &options, None,)
                        .unwrap()
                )
                .unwrap()
        );
        assert_eq!(store.recipes.len(), 2);
        assert_eq!(
            store.get("alpha").unwrap().parsed_output_format().unwrap(),
            OutputFormat::DOCX
        );
        assert!(store.delete("zULu"));
        assert!(!store.delete("missing"));
    }

    #[test]
    fn rejects_future_schema_duplicate_names_and_bad_templates() {
        let future = RecipeStore {
            version: RECIPE_STORE_VERSION + 1,
            ..RecipeStore::default()
        };
        assert!(matches!(
            future.validate(),
            Err(RecipeError::UnsupportedVersion { .. })
        ));

        let options = ConversionOptions::default();
        let recipe =
            ConversionRecipe::new("One", OutputFormat::HTML, None, &options, None).unwrap();
        let duplicate = ConversionRecipe {
            name: "one".into(),
            ..recipe.clone()
        };
        let store = RecipeStore {
            version: RECIPE_STORE_VERSION,
            recipes: vec![recipe, duplicate],
        };
        assert!(store.validate().is_err());

        for invalid in [
            "../oops",
            "{unknown}.md",
            "{stem",
            "{stem}}",
            "x}y{stem}",
            "",
        ] {
            assert!(validate_naming_template(invalid).is_err(), "{invalid}");
        }
        validate_naming_template("{stem}-{format}.{ext}").unwrap();
    }

    #[test]
    fn failed_save_does_not_leave_temporary_file() {
        let directory = temp_path("parent");
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("recipes.json");
        let options = ConversionOptions::default();
        let mut original = RecipeStore::default();
        original
            .upsert(
                ConversionRecipe::new("Keep me", OutputFormat::MARKDOWN, None, &options, None)
                    .unwrap(),
            )
            .unwrap();
        save_recipe_store(&target, &original).unwrap();
        let original_bytes = fs::read(&target).unwrap();
        let bad = RecipeStore {
            version: RECIPE_STORE_VERSION,
            recipes: vec![ConversionRecipe {
                name: String::new(),
                output_format: "markdown".into(),
                preferred_module: None,
                options: SessionConversionOptions::default(),
                destination: None,
            }],
        };
        assert!(save_recipe_store(&target, &bad).is_err());
        assert_eq!(fs::read(&target).unwrap(), original_bytes);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn load_reports_corruption_and_future_store_versions() {
        let corrupt = temp_path("corrupt");
        fs::write(&corrupt, b"{not json").unwrap();
        assert!(
            load_recipe_store(&corrupt)
                .unwrap_err()
                .to_string()
                .contains("parse")
        );

        let future = temp_path("future");
        fs::write(
            &future,
            format!(
                "{{\"version\":{},\"recipes\":[]}}",
                RECIPE_STORE_VERSION + 1
            ),
        )
        .unwrap();
        assert!(matches!(
            load_recipe_store(&future),
            Err(RecipeError::UnsupportedVersion { .. })
        ));
        let _ = fs::remove_file(corrupt);
        let _ = fs::remove_file(future);
    }
}
