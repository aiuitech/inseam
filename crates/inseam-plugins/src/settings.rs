//! The first-party settings projection (`design/composition.md`): the
//! typed document a GUI edits instead of the composition file. Reading
//! projects the layered composition through the real plugin config types
//! with their defaults applied; writing validates a complete document and
//! turns it into patch entries — whole-config replacement per id, the
//! layering rule — that land in the node's overlay. The macOS app writes
//! the overlay file itself and reopens; the web console sends the same
//! document through the `operations` seam and the running node applies it
//! as a composition edit. One document, one rule, two transports.

use inseam_kernel::substrate::{Composition, Entry, parse_config};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::connection_fs::machine::MACHINE_IDENTITY_CHARS_MAX;
use crate::connection_fs::{configured_roots, FsConnectionConfig, WalkConfig};
use crate::connection_google::{GoogleConnection, GoogleConnectionConfig};
use crate::embedder::{EmbedderConfig, Provider};
use crate::finder::FinderConfig;
use crate::llm_endpoint::LlmEndpointConfig;
use crate::node::{NodeConfig, NodeFactory};
use crate::oauth::{OAuthConfig, OAuthPlugin};
use crate::roster::{RosterConfig, RosterPlugin};
use crate::routing::{RoutingConfig, RoutingPlugin};
use crate::sweep::SweepConfig;
use crate::sweep::ignore::IgnoreSet;
use crate::sync::{SyncConfig, SyncPlugin};
use crate::transform_chunker::ChunkerConfig;
use crate::transform_entities::EntityExtractorConfig;
use crate::transform_summarizer::SummarizerConfig;
use crate::transport_iroh::{Settings as TransportSettings, TransportConfig};
use inseam_kernel::substrate::PluginFactory;
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
    /// The network entries (`design/roster.md`): identity and presentation,
    /// the iroh transport, the roster, replication, and routing.
    pub node: Configurable<NodeConfig>,
    pub transport: Configurable<TransportConfig>,
    pub roster: Configurable<RosterConfig>,
    pub sync: Configurable<SyncConfig>,
    pub routing: Configurable<RoutingConfig>,
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
            node: configurable(composition, "node")?,
            transport: configurable(composition, "transport")?,
            roster: configurable(composition, "roster")?,
            sync: configurable(composition, "sync")?,
            routing: configurable(composition, "routing")?,
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
        validate_network(self)?;
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
            config_patch("node", self.node)?,
            config_patch("transport", self.transport)?,
            config_patch("roster", self.roster)?,
            config_patch("sync", self.sync)?,
            config_patch("routing", self.routing)?,
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
    if let Some(machine_id) = &config.machine_id {
        if machine_id.trim().is_empty() {
            return Err(SettingsError::from(
                "fs.machine_id: may not be empty; leave it unset to use this machine's own id"
                    .to_string(),
            ));
        }
        if machine_id.chars().count() > MACHINE_IDENTITY_CHARS_MAX {
            return Err(SettingsError::from(format!(
                "fs.machine_id: longer than {MACHINE_IDENTITY_CHARS_MAX} characters"
            )));
        }
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

/// The network entries checked the way their plugins check them at mount:
/// each factory (or the transport's settings parser) is the one validator,
/// so the document can never accept what the node would refuse.
fn validate_network(settings: &SettingsDocument) -> Result<(), SettingsError> {
    NodeFactory
        .build(&config_table("node", &settings.node.config)?)
        .map_err(|error| format!("node: {error}"))?;
    TransportSettings::try_from(&settings.transport.config)
        .map_err(|error| format!("transport: {error}"))?;
    RosterPlugin::from_config(&config_table("roster", &settings.roster.config)?)
        .map_err(|error| format!("roster: {error}"))?;
    SyncPlugin::from_config(&config_table("sync", &settings.sync.config)?)
        .map_err(|error| format!("sync: {error}"))?;
    RoutingPlugin::from_config(&config_table("routing", &settings.routing.config)?)
        .map_err(|error| format!("routing: {error}"))?;
    Ok(())
}

/// A config as the TOML table its plugin's factory parses.
fn config_table<T: Serialize>(id: &str, config: &T) -> Result<toml::Table, SettingsError> {
    let value = toml::Value::try_from(config)
        .map_err(|error| format!("serialize `{id}` config: {error}"))?;
    match value {
        toml::Value::Table(table) => Ok(table),
        _ => Err(SettingsError::from(format!(
            "`{id}` config did not serialize as a table"
        ))),
    }
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
            "[[entry]]\nid = \"node\"\nplugin = \"node\"\n",
            "[[entry]]\nid = \"transport\"\nplugin = \"transport-iroh\"\n",
            "[[entry]]\nid = \"roster\"\nplugin = \"roster\"\n",
            "[[entry]]\nid = \"sync\"\nplugin = \"sync\"\n",
            "[[entry]]\nid = \"routing\"\nplugin = \"routing\"\n",
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

    #[test]
    fn network_entries_round_trip_with_their_defaults() {
        let document = SettingsDocument::from_composition(&base()).unwrap();
        assert!(document.node.enabled);
        assert_eq!(document.node.config.display_name, None);
        assert!(document.node.config.deep_index);
        assert_eq!(document.transport.config.relay, "n0");
        assert_eq!(document.transport.config.bind_port, 0);
        assert_eq!(document.roster.config.endpoint_poll_secs, 30);
        assert_eq!(document.sync.config.interval_secs, 60);
        assert!(document.routing.config.fan_out);

        let mut edited = document;
        edited.node.config.display_name = Some("Greg's mini".to_string());
        edited.node.config.always_on = true;
        edited.transport.config.relay = "none".to_string();
        edited.transport.config.bind_port = 4433;
        edited.sync.config.interval_secs = 15;
        edited.routing.enabled = false;
        let mut overlay = Composition::default();
        edited.apply(&mut overlay, WriteMode::All).unwrap();
        let layered = base().layered(overlay).unwrap();
        let reread = SettingsDocument::from_composition(&layered).unwrap();
        assert_eq!(reread.node.config.display_name.as_deref(), Some("Greg's mini"));
        assert!(reread.node.config.always_on);
        assert_eq!(reread.transport.config.relay, "none");
        assert_eq!(reread.transport.config.bind_port, 4433);
        assert_eq!(reread.sync.config.interval_secs, 15);
        assert!(!reread.routing.enabled);
    }

    /// One bad edit to an otherwise valid document.
    type BadEdit = Box<dyn Fn(&mut SettingsDocument)>;

    #[test]
    fn invalid_network_fields_are_refused_by_entry() {
        let cases: Vec<(&str, BadEdit)> = vec![
            ("node", Box::new(|d| d.node.config.display_name = Some("   ".to_string()))),
            ("transport", Box::new(|d| d.transport.config.relay = "ftp://relay".to_string())),
            ("transport", Box::new(|d| d.transport.config.idle_timeout_secs = 1)),
            ("roster", Box::new(|d| d.roster.config.endpoint_poll_secs = 0)),
            ("sync", Box::new(|d| d.sync.config.interval_secs = 0)),
            ("sync", Box::new(|d| d.sync.config.peers_per_round_max = 0)),
            ("routing", Box::new(|d| d.routing.config.fan_out_timeout_ms = 0)),
        ];
        for (entry, edit) in cases {
            let mut document = SettingsDocument::from_composition(&base()).unwrap();
            edit(&mut document);
            let error = document.validate().unwrap_err().to_string();
            assert!(error.starts_with(&format!("{entry}:")), "{entry}: {error}");
        }
    }
}
