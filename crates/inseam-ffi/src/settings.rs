use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use inseam_kernel::address::HostId;
use inseam_kernel::substrate::{Composition, ENTRY_COUNT_MAX, Entry, parse_config};
use inseam_plugins::connection_fs::{FsConnectionConfig, WalkConfig};
use inseam_plugins::embedder::{EmbedderConfig, Provider};
use inseam_plugins::finder::FinderConfig;
use inseam_plugins::llm_endpoint::LlmEndpointConfig;
use inseam_plugins::oauth::{OAuthConfig, OAuthPlugin};
use inseam_plugins::sweep::SweepConfig;
use inseam_plugins::sweep::ignore::IgnoreSet;
use inseam_plugins::transform_chunker::ChunkerConfig;
use inseam_plugins::transform_entities::EntityExtractorConfig;
use inseam_plugins::transform_summarizer::SummarizerConfig;
use inseam_seams::dates::parse_ymd_epoch;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::BASE_COMPOSITION;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SettingsDocument {
    connections: Toggle,
    fs: Configurable<FsConnectionConfig>,
    oauth: Configurable<OAuthConfig>,
    llm: Configurable<LlmEndpointConfig>,
    embedder: Configurable<EmbedderConfig>,
    transforms: Toggle,
    markdown: Toggle,
    chunker: Configurable<ChunkerConfig>,
    summarizer: Configurable<SummarizerConfig>,
    entities: Configurable<EntityExtractorConfig>,
    finder: Configurable<FinderConfig>,
    sweep: Configurable<SweepConfig>,
    operations: Toggle,
}

#[derive(Debug, Serialize, Deserialize)]
struct Configurable<T> {
    enabled: bool,
    config: T,
}

#[derive(Debug, Serialize, Deserialize)]
struct Toggle {
    enabled: bool,
}

pub(crate) fn read(path: &Path) -> Result<SettingsDocument, String> {
    let base =
        Composition::parse(BASE_COMPOSITION, "<ffi base>").expect("the base composition is valid");
    let composition = match path.exists() {
        true => base
            .layered(Composition::load(path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?,
        false => base,
    };
    from_composition(&composition)
}

pub(crate) fn write(path: &Path, json: &str) -> Result<(), String> {
    let settings: SettingsDocument =
        serde_json::from_str(json).map_err(|error| format!("settings JSON: {error}"))?;
    settings.validate()?;
    let mut overlay = match path.exists() {
        true => Composition::load(path).map_err(|error| error.to_string())?,
        false => Composition::default(),
    };
    settings.apply(&mut overlay)?;
    let rendered = overlay.to_toml();
    Composition::parse(&rendered, &path.display().to_string())
        .map_err(|error| error.to_string())?;
    write_atomic(path, rendered.as_bytes())
}

fn from_composition(composition: &Composition) -> Result<SettingsDocument, String> {
    Ok(SettingsDocument {
        connections: toggle(composition, "connections")?,
        fs: configurable(composition, "fs")?,
        oauth: configurable(composition, "oauth")?,
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

fn configurable<T>(composition: &Composition, id: &str) -> Result<Configurable<T>, String>
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

fn toggle(composition: &Composition, id: &str) -> Result<Toggle, String> {
    let entry = resolved_entry(composition, id)?;
    Ok(Toggle {
        enabled: !entry.is_disabled(),
    })
}

fn resolved_entry<'a>(composition: &'a Composition, id: &str) -> Result<&'a Entry, String> {
    composition
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| format!("the distribution has no `{id}` entry"))
}

impl SettingsDocument {
    fn validate(&self) -> Result<(), String> {
        validate_source(&self.fs.config)?;
        validate_oauth(&self.oauth.config)?;
        validate_models(&self.llm.config, &self.embedder.config)?;
        validate_transforms(self)?;
        validate_finder(&self.finder.config)?;
        validate_sweep(&self.sweep.config)
    }

    fn apply(self, composition: &mut Composition) -> Result<(), String> {
        apply_toggle(composition, "connections", self.connections);
        apply_config(composition, "fs", self.fs)?;
        apply_config(composition, "oauth", self.oauth)?;
        apply_config(composition, "llm", self.llm)?;
        apply_config(composition, "embedder", self.embedder)?;
        apply_toggle(composition, "transforms", self.transforms);
        apply_toggle(composition, "markdown", self.markdown);
        apply_config(composition, "chunker", self.chunker)?;
        apply_config(composition, "summarizer", self.summarizer)?;
        apply_config(composition, "entities", self.entities)?;
        apply_config(composition, "finder", self.finder)?;
        apply_config(composition, "sweep", self.sweep)?;
        apply_toggle(composition, "operations", self.operations);
        Ok(())
    }
}

fn validate_oauth(config: &OAuthConfig) -> Result<(), String> {
    let value = toml::Value::try_from(config)
        .map_err(|error| format!("serialize `oauth` config: {error}"))?;
    let toml::Value::Table(table) = value else {
        return Err("`oauth` config did not serialize as a table".to_string());
    };
    OAuthPlugin::from_config(&table)
        .map(|_| ())
        .map_err(|error| format!("oauth: {error}"))
}

fn validate_source(config: &FsConnectionConfig) -> Result<(), String> {
    if let Some(host_id) = &config.host_id {
        HostId::new(host_id).map_err(|error| format!("fs.host_id: {error}"))?;
    }
    WalkConfig::compile(config).map_err(|error| format!("fs.ignore: {error}"))?;
    Ok(())
}

fn validate_models(llm: &LlmEndpointConfig, embedder: &EmbedderConfig) -> Result<(), String> {
    if llm.base_url.trim().is_empty() {
        return Err("llm.base_url must not be empty".to_string());
    }
    if !is_environment_name(&llm.api_key_env) {
        return Err(format!(
            "llm.api_key_env `{}` is not a valid name",
            llm.api_key_env
        ));
    }
    if embedder.provider != Provider::None && embedder.dimensions == 0 {
        return Err("embedder.dimensions must be greater than zero".to_string());
    }
    Ok(())
}

fn validate_transforms(settings: &SettingsDocument) -> Result<(), String> {
    if settings.chunker.config.target_chars == 0 {
        return Err("chunker.target_chars must be greater than zero".to_string());
    }
    if settings.summarizer.config.target_chars == 0 {
        return Err("summarizer.target_chars must be greater than zero".to_string());
    }
    if settings.entities.config.max_per_source == 0 {
        return Err("entities.max_per_source must be greater than zero".to_string());
    }
    Ok(())
}

fn validate_finder(config: &FinderConfig) -> Result<(), String> {
    finite_positive("finder.rrf_k", config.rrf_k)?;
    finite_nonnegative("finder.damping", config.damping)?;
    if config.damping > 1.0 {
        return Err("finder.damping must not exceed one".to_string());
    }
    finite_positive("finder.epsilon", config.epsilon)?;
    finite_nonnegative("finder.max_vector_distance", config.max_vector_distance)?;
    finite_nonnegative("finder.weights.default", config.weights.default)?;
    for (kind, weight) in &config.weights.by_kind {
        finite_nonnegative(&format!("finder.weights.by_kind.{kind}"), *weight)?;
    }
    Ok(())
}

fn validate_sweep(config: &SweepConfig) -> Result<(), String> {
    if config.max_fragments_per_source == 0 {
        return Err("sweep.max_fragments_per_source must be greater than zero".to_string());
    }
    if config.max_depth == 0 {
        return Err("sweep.max_depth must be greater than zero".to_string());
    }
    if config.max_content_bytes == 0 {
        return Err("sweep.max_content_bytes must be greater than zero".to_string());
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

fn finite_positive(name: &str, value: f64) -> Result<(), String> {
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("{name} must be a finite number greater than zero"));
    }
    Ok(())
}

fn finite_nonnegative(name: &str, value: f64) -> Result<(), String> {
    if !value.is_finite() || value < 0.0 {
        return Err(format!("{name} must be a finite nonnegative number"));
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

fn apply_config<T>(
    composition: &mut Composition,
    id: &str,
    setting: Configurable<T>,
) -> Result<(), String>
where
    T: Serialize,
{
    let value = toml::Value::try_from(setting.config)
        .map_err(|error| format!("serialize `{id}` config: {error}"))?;
    let table = match value {
        toml::Value::Table(table) => table,
        _ => return Err(format!("`{id}` config did not serialize as a table")),
    };
    apply_entry(composition, id, setting.enabled, table);
    Ok(())
}

fn apply_toggle(composition: &mut Composition, id: &str, setting: Toggle) {
    let table = entry_path(&composition.entries, id)
        .map(|path| entry_at(&composition.entries, &path).config.clone())
        .unwrap_or_default();
    apply_entry(composition, id, setting.enabled, table);
}

fn apply_entry(composition: &mut Composition, id: &str, enabled: bool, config: toml::Table) {
    match entry_path(&composition.entries, id) {
        Some(path) => {
            let entry = entry_at_mut(&mut composition.entries, &path);
            entry.config = config;
            entry.disabled = Some(!enabled);
        }
        None => composition.entries.push(Entry {
            id: id.to_string(),
            config,
            disabled: Some(!enabled),
            ..Entry::default()
        }),
    }
}

fn entry_path(entries: &[Entry], id: &str) -> Option<Vec<usize>> {
    let mut stack: Vec<(&Entry, Vec<usize>)> = entries
        .iter()
        .enumerate()
        .rev()
        .map(|(index, entry)| (entry, vec![index]))
        .collect();
    let mut visited: usize = 0;
    while let Some((entry, path)) = stack.pop() {
        visited += 1;
        assert!(visited <= ENTRY_COUNT_MAX, "composition was validated");
        if entry.id == id {
            return Some(path);
        }
        for (index, child) in entry.entries.iter().enumerate().rev() {
            let mut child_path = path.clone();
            child_path.push(index);
            stack.push((child, child_path));
        }
    }
    None
}

fn entry_at<'a>(entries: &'a [Entry], path: &[usize]) -> &'a Entry {
    let (index, parents) = path.split_last().expect("an entry path is nonempty");
    let mut current = entries;
    for parent in parents {
        current = &current[*parent].entries;
    }
    &current[*index]
}

fn entry_at_mut<'a>(entries: &'a mut Vec<Entry>, path: &[usize]) -> &'a mut Entry {
    let (index, parents) = path.split_last().expect("an entry path is nonempty");
    let mut current = entries;
    for parent in parents {
        current = &mut current[*parent].entries;
    }
    &mut current[*index]
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("`{}` has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create `{}`: {error}", parent.display()))?;
    let temporary = temporary_path(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("create `{}`: {error}", temporary.display()))?;
    let result = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|error| format!("write `{}`: {error}", path.display()))
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_read_when_the_file_is_absent() {
        let directory = tempfile::tempdir().unwrap();
        let settings = read(&directory.path().join("composition.toml")).unwrap();
        assert!(settings.connections.enabled);
        assert!(settings.fs.enabled);
        assert!(settings.fs.config.skip_hidden);
        assert!(settings.oauth.enabled);
        assert_eq!(settings.oauth.config.callback_port, 47_781);
        assert_eq!(settings.embedder.config.dimensions, 1536);
        assert_eq!(settings.finder.config.weights.by_kind["mentions"], 0.8);
    }

    #[test]
    fn write_round_trips_fields_and_preserves_custom_entries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("composition.toml");
        std::fs::write(
            &path,
            "[[entry]]\nid = \"ocr\"\nplugin = \"wasm:ocr.wasm\"\n",
        )
        .unwrap();
        let mut settings = read(&path).unwrap();
        settings.fs.config.skip_hidden = false;
        settings.embedder.enabled = false;
        settings.sweep.config.max_depth = 9;
        write(&path, &serde_json::to_string(&settings).unwrap()).unwrap();
        let saved = Composition::load(&path).unwrap();
        assert!(saved.entries.iter().any(|entry| entry.id == "ocr"));
        let reread = read(&path).unwrap();
        assert!(!reread.fs.config.skip_hidden);
        assert!(!reread.embedder.enabled);
        assert_eq!(reread.sweep.config.max_depth, 9);
    }

    #[test]
    fn invalid_json_does_not_change_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("composition.toml");
        let original = "[[entry]]\nid = \"llm\"\ndisabled = true\n";
        std::fs::write(&path, original).unwrap();
        let result = write(&path, r#"{"fs":{"enabled":true}}"#);
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn invalid_field_does_not_change_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("composition.toml");
        let original = "[[entry]]\nid = \"llm\"\ndisabled = true\n";
        std::fs::write(&path, original).unwrap();
        let mut settings = read(&path).unwrap();
        settings.sweep.config.max_depth = 0;
        let result = write(&path, &serde_json::to_string(&settings).unwrap());
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }
}
