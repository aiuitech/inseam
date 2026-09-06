//! The first-party settings projection (`design/composition.md`): the
//! typed document a GUI edits instead of the composition file. Reading
//! projects the layered composition through the real plugin config types
//! with their defaults applied; writing validates a complete document and
//! turns it into patch entries — whole-config replacement per id, the
//! layering rule — that land in the node's overlay. The macOS app writes
//! the overlay file itself and reopens; the web console sends the same
//! document through the `operations` seam and the running node applies it
//! as a composition edit. One document, one rule, two transports.

use inseam_kernel::address::HostId;
use inseam_kernel::substrate::{Composition, Entry, parse_config};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::connection_fs::{configured_roots, FsConnectionConfig, WalkConfig};
use crate::connection_google::{GoogleConnection, GoogleConnectionConfig};
use crate::embedder::{EmbedderConfig, Provider};
use crate::finder::FinderConfig;
use crate::llm_endpoint::LlmEndpointConfig;
use crate::oauth::{OAuthConfig, OAuthPlugin};
use crate::sweep::SweepConfig;
use crate::sweep::ignore::IgnoreSet;
use crate::transform_chunker::ChunkerConfig;
use crate::transform_entities::EntityExtractorConfig;
use crate::transform_summarizer::SummarizerConfig;
use inseam_seams::dates::parse_ymd_epoch;

/// Why a document could not be read from a composition or written back:
/// a field that fails the plugin's own validation, or an entry the
/// distribution does not carry.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct SettingsError {
    message: String,
}

impl From<String> for SettingsError {
    fn from(message: String) -> Self {
        Self { message }
    }
}

/// Every first-party entry with its enable switch and, for the configured
/// ones, its complete config with the plugin's defaults filled in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsDocument {
    pub connections: Toggle,
    pub fs: Configurable<FsConnectionConfig>,
    pub oauth: Configurable<OAuthConfig>,
    pub google: Configurable<GoogleConnectionConfig>,
    pub llm: Configurable<LlmEndpointConfig>,
    pub embedder: Configurable<EmbedderConfig>,
    pub transforms: Toggle,
    pub markdown: Toggle,
    pub chunker: Configurable<ChunkerConfig>,
    pub summarizer: Configurable<SummarizerConfig>,
    pub entities: Configurable<EntityExtractorConfig>,
    pub finder: Configurable<FinderConfig>,
    pub sweep: Configurable<SweepConfig>,
    pub operations: Toggle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Configurable<T> {
    pub enabled: bool,
    pub config: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Toggle {
    pub enabled: bool,
}

/// Which entries a write touches. An app shell that mounts its own
/// embedder beneath the owner's overlay preserves that entry, because
/// writing endpoint defaults over it would mask the on-device provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    All,
    PreserveEmbedder,
}

impl SettingsDocument {
    /// Project a layered composition — what the node runs — into the
    /// document, defaults applied by each plugin's config type.
    pub fn from_composition(composition: &Composition) -> Result<Self, SettingsError> {
        Ok(Self {
            connections: toggle(composition, "connections")?,
            fs: configurable(composition, "fs")?,
            oauth: configurable(composition, "oauth")?,
            google: configurable(composition, "google")?,
            llm: configurable(composition, "llm")?,
            embedder: configurable(composition, "embedder")?,
            transforms: toggle(composition, "transforms")?,
            markdown: toggle(composition, "markdown")?,
            chunker: configurable(composition, "chunker")?,
            summarizer: configurable(composition, "summarizer")?,
            entities: configurable(composition, "entities")?,
            finder: configurable(composition, "finder")?,
            sweep: configurable(composition, "sweep")?,
            operations: toggle(composition, "operations")?,
        })
    }

    /// Every field checked the way its plugin would check it at mount, so
    /// a bad value is refused before any file or fiber changes.
    pub fn validate(&self) -> Result<(), SettingsError> {
        validate_source(&self.fs.config)?;
        validate_oauth(&self.oauth.config)?;
        validate_google(&self.google.config)?;
        validate_models(&self.llm.config, &self.embedder.config)?;
        validate_transforms(self)?;
        validate_finder(&self.finder.config)?;
        validate_sweep(&self.sweep.config)?;
        Ok(())
    }

    /// The document as patch entries for an overlay: a configured entry
    /// carries its whole config table and its toggle; a toggle-only entry
    /// carries no config, so whatever the overlay already holds for it
    /// stays. Apply with [`Composition::configure_entry`], in order.
    pub fn into_patches(self, mode: WriteMode) -> Result<Vec<Entry>, SettingsError> {
        let mut patches = vec![
            toggle_patch("connections", self.connections),
            config_patch("fs", self.fs)?,
            config_patch("oauth", self.oauth)?,
            config_patch("google", self.google)?,
            config_patch("llm", self.llm)?,
        ];
        match mode {
            WriteMode::All => patches.push(config_patch("embedder", self.embedder)?),
            WriteMode::PreserveEmbedder => {}
        }
        patches.extend([
            toggle_patch("transforms", self.transforms),
            toggle_patch("markdown", self.markdown),
            config_patch("chunker", self.chunker)?,
            config_patch("summarizer", self.summarizer)?,
            config_patch("entities", self.entities)?,
            config_patch("finder", self.finder)?,
            config_patch("sweep", self.sweep)?,
            toggle_patch("operations", self.operations),
        ]);
        Ok(patches)
    }

    /// Validate and land the document in `overlay` — the file-writing
    /// path an app shell takes before it reopens the node.
    pub fn apply(self, overlay: &mut Composition, mode: WriteMode) -> Result<(), SettingsError> {
        self.validate()?;
        for patch in self.into_patches(mode)? {
            overlay
                .configure_entry(&patch)
                .map_err(|error| SettingsError::from(error.to_string()))?;
        }
        Ok(())
    }
}

fn configurable<T>(composition: &Composition, id: &str) -> Result<Configurable<T>, SettingsError>
where
    T: DeserializeOwned,
{
    let entry = resolved_entry(composition, id)?;
    let config = parse_config(&entry.config).map_err(|error| format!("{id}: {error}"))?;
    Ok(Configurable {
        enabled: !entry.is_disabled(),
        config,
    })
}

fn toggle(composition: &Composition, id: &str) -> Result<Toggle, SettingsError> {
    let entry = resolved_entry(composition, id)?;
    Ok(Toggle {
        enabled: !entry.is_disabled(),
    })
}

fn resolved_entry<'a>(composition: &'a Composition, id: &str) -> Result<&'a Entry, SettingsError> {
    composition
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| SettingsError::from(format!("the distribution has no `{id}` entry")))
}

fn config_patch<T>(id: &str, setting: Configurable<T>) -> Result<Entry, SettingsError>
where
    T: Serialize,
{
    let value = toml::Value::try_from(setting.config)
        .map_err(|error| format!("serialize `{id}` config: {error}"))?;
    let toml::Value::Table(config) = value else {
        return Err(SettingsError::from(format!(
            "`{id}` config did not serialize as a table"
        )));
    };
    Ok(Entry {
        id: id.to_string(),
        config,
        disabled: Some(!setting.enabled),
        ..Entry::default()
    })
}

fn toggle_patch(id: &str, setting: Toggle) -> Entry {
    Entry {
        id: id.to_string(),
        disabled: Some(!setting.enabled),
        ..Entry::default()
    }
}

fn validate_oauth(config: &OAuthConfig) -> Result<(), SettingsError> {
    let value = toml::Value::try_from(config)
        .map_err(|error| format!("serialize `oauth` config: {error}"))?;
    let toml::Value::Table(table) = value else {
        return Err(SettingsError::from("`oauth` config did not serialize as a table".to_string()));
    };
    OAuthPlugin::from_config(&table).map_err(|error| format!("oauth: {error}"))?;
    Ok(())
}

fn validate_google(config: &GoogleConnectionConfig) -> Result<(), SettingsError> {
    let value = toml::Value::try_from(config)
        .map_err(|error| format!("serialize `google` config: {error}"))?;
    let toml::Value::Table(table) = value else {
        return Err(SettingsError::from("`google` config did not serialize as a table".to_string()));
    };
    GoogleConnection::from_config(&table).map_err(|error| format!("google: {error}"))?;
    Ok(())
}

fn validate_source(config: &FsConnectionConfig) -> Result<(), SettingsError> {
    if let Some(host_id) = &config.host_id {
        HostId::new(host_id).map_err(|error| format!("fs.host_id: {error}"))?;
    }
    WalkConfig::compile(config).map_err(|error| format!("fs.ignore: {error}"))?;
    configured_roots(&config.roots).map_err(|error| format!("fs.roots: {error}"))?;
    Ok(())
}

fn validate_models(llm: &LlmEndpointConfig, embedder: &EmbedderConfig) -> Result<(), SettingsError> {
    if llm.base_url.trim().is_empty() {
        return Err(SettingsError::from("llm.base_url must not be empty".to_string()));
    }
    // An empty name is a keyless endpoint (a local ollama), not a bad name.
    if !llm.api_key_env.trim().is_empty() && !is_environment_name(llm.api_key_env.trim()) {
        return Err(SettingsError::from(format!(
            "llm.api_key_env `{}` is not a valid name",
            llm.api_key_env
        )));
    }
    if embedder.provider != Provider::None && embedder.dimensions == Some(0) {
        return Err(SettingsError::from(
            "embedder.dimensions must be greater than zero, or unset for the model's native width"
                .to_string(),
        ));
    }
    Ok(())
}

fn validate_transforms(settings: &SettingsDocument) -> Result<(), SettingsError> {
    if settings.chunker.config.target_chars == 0 {
        return Err(SettingsError::from("chunker.target_chars must be greater than zero".to_string()));
    }
    if settings.summarizer.config.target_chars == 0 {
        return Err(SettingsError::from("summarizer.target_chars must be greater than zero".to_string()));
    }
    if settings.entities.config.max_per_source == 0 {
        return Err(SettingsError::from("entities.max_per_source must be greater than zero".to_string()));
    }
    Ok(())
}

fn validate_finder(config: &FinderConfig) -> Result<(), SettingsError> {
    config.validate_query_bounds().map_err(SettingsError::from)?;
    finite_positive("finder.rrf_k", config.rrf_k)?;
    finite_nonnegative("finder.damping", config.damping)?;
    if config.damping > 1.0 {
        return Err(SettingsError::from("finder.damping must not exceed one".to_string()));
    }
    finite_positive("finder.epsilon", config.epsilon)?;
    finite_nonnegative("finder.max_vector_distance", config.max_vector_distance)?;
    finite_nonnegative("finder.weights.default", config.weights.default)?;
    for (kind, weight) in &config.weights.by_kind {
        finite_nonnegative(&format!("finder.weights.by_kind.{kind}"), *weight)?;
    }
    Ok(())
}

fn validate_sweep(config: &SweepConfig) -> Result<(), SettingsError> {
    if config.max_fragments_per_source == 0 {
        return Err(SettingsError::from("sweep.max_fragments_per_source must be greater than zero".to_string()));
    }
    if config.max_depth == 0 {
        return Err(SettingsError::from("sweep.max_depth must be greater than zero".to_string()));
    }
    if config.max_content_bytes == 0 {
        return Err(SettingsError::from("sweep.max_content_bytes must be greater than zero".to_string()));
    }
    if let Some(date) = config
        .modified_after
        .as_deref()
        .filter(|date| !date.is_empty())
    {
        parse_ymd_epoch(date).map_err(|error| format!("sweep.modified_after: {error}"))?;
    }
    IgnoreSet::compile(&config.ignore).map_err(|error| format!("sweep.ignore: {error}"))?;
    Ok(())
}

fn finite_positive(name: &str, value: f64) -> Result<(), SettingsError> {
    if !value.is_finite() || value <= 0.0 {
        return Err(SettingsError::from(format!("{name} must be a finite number greater than zero")));
    }
    Ok(())
}

fn finite_nonnegative(name: &str, value: f64) -> Result<(), SettingsError> {
    if !value.is_finite() || value < 0.0 {
        return Err(SettingsError::from(format!("{name} must be a finite nonnegative number")));
    }
    Ok(())
}

fn is_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    let first_valid = first.is_ascii_alphabetic() || first == b'_';
    first_valid && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The first-party entries as a distribution base ships them.
    fn base() -> Composition {
        let text = concat!(
            "[[entry]]\nid = \"connections\"\nplugin = \"connections\"\n",
            "[[entry]]\nid = \"fs\"\nplugin = \"connection-fs\"\n",
            "[[entry]]\nid = \"oauth\"\nplugin = \"oauth\"\n",
            "[[entry]]\nid = \"google\"\nplugin = \"connection-google\"\n",
            "[[entry]]\nid = \"llm\"\nplugin = \"llm-endpoint\"\n",
            "[[entry]]\nid = \"embedder\"\nplugin = \"embedder\"\n",
            "[[entry]]\nid = \"transforms\"\nplugin = \"transforms\"\n",
            "[[entry]]\nid = \"markdown\"\nplugin = \"transform-markdown\"\n",
            "[[entry]]\nid = \"directory\"\nplugin = \"transform-directory\"\n",
            "[[entry]]\nid = \"chunker\"\nplugin = \"transform-chunker\"\n",
            "[[entry]]\nid = \"summarizer\"\nplugin = \"transform-summarizer\"\n",
            "[[entry]]\nid = \"entities\"\nplugin = \"transform-entities\"\n",
            "[[entry]]\nid = \"finder\"\nplugin = \"finder\"\n",
            "[[entry]]\nid = \"sweep\"\nplugin = \"sweep\"\n",
            "[[entry]]\nid = \"operations\"\nplugin = \"operations\"\n",
        );
        Composition::parse(text, "test").unwrap()
    }

    #[test]
    fn a_document_round_trips_through_patches() {
        let mut document = SettingsDocument::from_composition(&base()).unwrap();
        document.fs.config.skip_hidden = false;
        document.embedder.enabled = false;
        document.sweep.config.max_depth = 9;
        let mut overlay = Composition::default();
        document.apply(&mut overlay, WriteMode::All).unwrap();
        let layered = base().layered(overlay).unwrap();
        let reread = SettingsDocument::from_composition(&layered).unwrap();
        assert!(!reread.fs.config.skip_hidden);
        assert!(!reread.embedder.enabled);
        assert_eq!(reread.sweep.config.max_depth, 9);
    }

    #[test]
    fn toggle_patches_carry_no_config() {
        let document = SettingsDocument::from_composition(&base()).unwrap();
        let patches = document.into_patches(WriteMode::PreserveEmbedder).unwrap();
        let transforms = patches.iter().find(|patch| patch.id == "transforms").unwrap();
        assert!(transforms.config.is_empty());
        assert_eq!(transforms.disabled, Some(false));
        assert!(patches.iter().all(|patch| patch.id != "embedder"));
        assert!(patches.iter().any(|patch| patch.id == "sweep"));
    }

    #[test]
    fn invalid_fields_are_refused_by_name() {
        let mut document = SettingsDocument::from_composition(&base()).unwrap();
        document.sweep.config.max_depth = 0;
        let error = document.validate().unwrap_err().to_string();
        assert!(error.contains("sweep.max_depth"), "{error}");
        let mut document = SettingsDocument::from_composition(&base()).unwrap();
        document.google.config.services.clear();
        assert!(document.validate().is_err());
        let mut document = SettingsDocument::from_composition(&base()).unwrap();
        document.fs.config.roots = vec!["notes".to_string()];
        let error = document.validate().unwrap_err().to_string();
        assert!(error.contains("fs.roots"), "{error}");
    }
}
