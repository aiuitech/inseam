use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use inseam_kernel::substrate::Composition;
pub(crate) use inseam_plugins::settings::SettingsDocument;
use inseam_plugins::settings::WriteMode;

use crate::BASE_COMPOSITION;

/// The document for the overlay at `path` (absent: the base alone),
/// projected through the shared settings types.
pub(crate) fn read(path: &Path) -> Result<SettingsDocument, String> {
    let base =
        Composition::parse(BASE_COMPOSITION, "<ffi base>").expect("the base composition is valid");
    let composition = match path.exists() {
        true => base
            .layered(Composition::load(path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?,
        false => base,
    };
    SettingsDocument::from_composition(&composition).map_err(|error| error.to_string())
}

pub(crate) fn write(path: &Path, json: &str) -> Result<(), String> {
    write_mode(path, json, WriteMode::All)
}

/// Save the first-party settings an app shell can own while leaving its
/// `embedder` overlay untouched. The shell supplies that provider beneath
/// the owner's composition, and writing endpoint defaults would mask it.
pub(crate) fn write_preserving_embedder(path: &Path, json: &str) -> Result<(), String> {
    write_mode(path, json, WriteMode::PreserveEmbedder)
}

fn write_mode(path: &Path, json: &str, mode: WriteMode) -> Result<(), String> {
    let settings: SettingsDocument =
        serde_json::from_str(json).map_err(|error| format!("settings JSON: {error}"))?;
    let mut overlay = match path.exists() {
        true => Composition::load(path).map_err(|error| error.to_string())?,
        false => Composition::default(),
    };
    settings
        .apply(&mut overlay, mode)
        .map_err(|error| error.to_string())?;
    let rendered = overlay.to_toml();
    Composition::parse(&rendered, &path.display().to_string())
        .map_err(|error| error.to_string())?;
    write_atomic(path, rendered.as_bytes())
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
        assert!(settings.google.enabled);
        assert_eq!(settings.google.config.client_id_env, "GOOGLE_CLIENT_ID");
        assert_eq!(settings.google.config.services.len(), 5);
        assert_eq!(settings.embedder.config.dimensions, None);
        assert_eq!(settings.finder.config.weights.by_kind["mentions"], 0.8);
        assert!(settings.node.enabled);
        assert_eq!(settings.node.config.display_name, None);
        assert!(settings.transport.enabled);
        assert_eq!(settings.transport.config.relay, "n0");
        assert!(settings.roster.enabled);
        assert!(settings.sync.enabled);
        assert!(settings.routing.enabled);
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
    fn shell_settings_write_leaves_the_owner_embedder_entry_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("composition.toml");
        let embedder = "[[entry]]\nid = \"embedder\"\nplugin = \"embedder\"\n[entry.config]\nprovider = \"none\"\n";
        std::fs::write(&path, embedder).unwrap();
        let mut settings = read(&path).unwrap();
        settings.embedder.enabled = false;
        settings.sweep.config.max_sources = 17;

        write_preserving_embedder(&path, &serde_json::to_string(&settings).unwrap()).unwrap();

        let overlay = Composition::load(&path).unwrap();
        let saved = overlay
            .entries
            .iter()
            .find(|entry| entry.id == "embedder")
            .unwrap();
        assert_eq!(saved.plugin.as_deref(), Some("embedder"));
        assert!(!saved.is_disabled());
        assert_eq!(saved.config["provider"].as_str(), Some("none"));
        assert_eq!(read(&path).unwrap().sweep.config.max_sources, 17);
    }

    #[test]
    fn oauth_grant_fields_round_trip_through_settings_json() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("composition.toml");
        let mut settings = serde_json::to_value(read(&path).unwrap()).unwrap();
        settings["oauth"]["config"]["grants"] = serde_json::json!([{
            "id": "google",
            "authorization_url": "https://accounts.example/authorize",
            "token_url": "https://accounts.example/token",
            "scopes": ["mail.read"],
            "client_id_env": "GOOGLE_CLIENT_ID",
            "client_secret_env": "GOOGLE_CLIENT_SECRET",
            "authorization_params": {"access_type": "offline"}
        }]);
        write(&path, &settings.to_string()).unwrap();
        let reread = serde_json::to_value(read(&path).unwrap()).unwrap();
        let grant = &reread["oauth"]["config"]["grants"][0];
        assert_eq!(grant["id"], "google");
        assert_eq!(grant["scopes"][0], "mail.read");
        assert_eq!(grant["authorization_params"]["access_type"], "offline");
    }

    #[test]
    fn google_services_round_trip_and_an_empty_list_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("composition.toml");
        let mut settings = serde_json::to_value(read(&path).unwrap()).unwrap();
        settings["google"]["config"]["services"] = serde_json::json!(["gmail", "calendar"]);
        // TOML has no null, so "no client secret" is the empty name — which
        // the plugin reads as none.
        settings["google"]["config"]["client_secret_env"] = serde_json::json!("");
        write(&path, &settings.to_string()).unwrap();
        let reread = serde_json::to_value(read(&path).unwrap()).unwrap();
        assert_eq!(
            reread["google"]["config"]["services"],
            serde_json::json!(["gmail", "calendar"])
        );
        assert_eq!(reread["google"]["config"]["client_secret_env"], "");
        settings["google"]["config"]["services"] = serde_json::json!([]);
        assert!(write(&path, &settings.to_string()).is_err());
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
