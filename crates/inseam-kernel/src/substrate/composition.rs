//! Composition: the declarative tree of plugin entries a node runs
//! (`design/composition.md`). Layers patch earlier layers by entry id —
//! distribution base, then node config, then invocation overlays — and the
//! same pure layering function answers `inseam config --resolved`, so what
//! prints is what boots.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompositionError {
    #[error("could not read composition `{path}`: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("composition `{path}` is not valid TOML: {source}")]
    Toml {
        path: String,
        source: Box<toml::de::Error>,
    },
    #[error("composition entry is missing an id")]
    MissingId,
    #[error("entry `{0}` appears twice in one layer")]
    DuplicateInLayer(String),
    #[error("entry `{0}` patches nothing and names no plugin")]
    PatchWithoutTarget(String),
}

/// One entry: a plugin mounted with a config. Groups are ordinary entries
/// with children, so subtrees can be toggled and shipped as units.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    /// Plugin ref: a native name from the distribution, or a scheme ref the
    /// distribution registered a resolver for (`wasm:<artifact>`). Absent in
    /// a patch layer entry that only overrides config/disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub config: toml::Table,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<Entry>,
}

impl Entry {
    pub fn new(id: &str, plugin: &str) -> Self {
        Self {
            id: id.to_string(),
            plugin: Some(plugin.to_string()),
            ..Self::default()
        }
    }

    pub fn with_config(mut self, config: toml::Table) -> Self {
        self.config = config;
        self
    }
}

/// A whole composition layer, as a TOML document: `[[entry]]` tables.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Composition {
    #[serde(rename = "entry", default)]
    pub entries: Vec<Entry>,
}

impl Composition {
    pub fn parse(toml_text: &str, path: &str) -> Result<Self, CompositionError> {
        let composition: Self =
            toml::from_str(toml_text).map_err(|source| CompositionError::Toml {
                path: path.to_string(),
                source: Box::new(source),
            })?;
        composition.validate()?;
        Ok(composition)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, CompositionError> {
        let raw = std::fs::read_to_string(path).map_err(|source| CompositionError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&raw, &path.display().to_string())
    }

    fn validate(&self) -> Result<(), CompositionError> {
        fn walk(entries: &[Entry], seen: &mut std::collections::HashSet<String>) -> Result<(), CompositionError> {
            for e in entries {
                if e.id.is_empty() {
                    return Err(CompositionError::MissingId);
                }
                if !seen.insert(e.id.clone()) {
                    return Err(CompositionError::DuplicateInLayer(e.id.clone()));
                }
                walk(&e.entries, seen)?;
            }
            Ok(())
        }
        walk(&self.entries, &mut std::collections::HashSet::new())
    }

    /// Patch this composition with a later layer, by entry id. Semantics are
    /// whole-entry replacement of `config` (simple over clever, per the
    /// design's open-question resolution); `disabled` and `plugin` override
    /// when present; unmatched ids append as new entries.
    pub fn layered(mut self, over: Composition) -> Result<Composition, CompositionError> {
        for patch in over.entries {
            Self::apply_patch(&mut self.entries, patch)?;
        }
        self.validate()?;
        Ok(self)
    }

    fn apply_patch(entries: &mut Vec<Entry>, patch: Entry) -> Result<(), CompositionError> {
        fn find<'a>(entries: &'a mut Vec<Entry>, id: &str) -> Option<&'a mut Entry> {
            for e in entries.iter_mut() {
                if e.id == id {
                    return Some(e);
                }
                if let Some(hit) = find(&mut e.entries, id) {
                    return Some(hit);
                }
            }
            None
        }
        match find(entries, &patch.id) {
            Some(target) => {
                if let Some(plugin) = patch.plugin {
                    target.plugin = Some(plugin);
                }
                if !patch.config.is_empty() {
                    target.config = patch.config;
                }
                target.disabled = patch.disabled;
                for child in patch.entries {
                    Self::apply_patch(&mut target.entries, child)?;
                }
                Ok(())
            }
            None => {
                if patch.plugin.is_none() {
                    return Err(CompositionError::PatchWithoutTarget(patch.id));
                }
                entries.push(patch);
                Ok(())
            }
        }
    }

    /// Flatten the tree into mountable entries: groups dissolve, a disabled
    /// entry prunes its whole subtree. Entries without a plugin ref are pure
    /// groups and mount nothing themselves.
    pub fn resolved(&self) -> Vec<Entry> {
        fn walk(entries: &[Entry], out: &mut Vec<Entry>) {
            for e in entries {
                if e.disabled {
                    continue;
                }
                if e.plugin.is_some() {
                    out.push(Entry {
                        entries: Vec::new(),
                        ..e.clone()
                    });
                }
                walk(&e.entries, out);
            }
        }
        let mut out = Vec::new();
        walk(&self.entries, &mut out);
        out
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Composition {
        Composition::parse(
            r#"
            [[entry]]
            id = "fs"
            plugin = "connection-fs"

            [[entry]]
            id = "index"
            plugin = "sweep"
            [entry.config]
            max_sources = 10

            [[entry]]
            id = "extras"
            [[entry.entries]]
            id = "entities"
            plugin = "transform-entities"
            "#,
            "base",
        )
        .expect("base parses")
    }

    #[test]
    fn rejects_duplicate_ids_in_a_layer() {
        let err = Composition::parse(
            "[[entry]]\nid = \"a\"\nplugin = \"x\"\n[[entry]]\nid = \"a\"\nplugin = \"y\"",
            "dup",
        )
        .expect_err("duplicate ids refuse");
        assert!(matches!(err, CompositionError::DuplicateInLayer(id) if id == "a"));
    }

    #[test]
    fn layering_replaces_config_wholesale_and_appends_new_entries() {
        let over = Composition::parse(
            r#"
            [[entry]]
            id = "index"
            [entry.config]
            max_depth = 3

            [[entry]]
            id = "ocr"
            plugin = "wasm:./ocr.wasm"
            "#,
            "over",
        )
        .expect("overlay parses");
        let layered = base().layered(over).expect("layers");
        let resolved = layered.resolved();
        let index = resolved.iter().find(|e| e.id == "index").expect("kept");
        assert!(index.config.contains_key("max_depth"));
        assert!(
            !index.config.contains_key("max_sources"),
            "config patches replace, not merge"
        );
        assert!(resolved.iter().any(|e| e.id == "ocr"));
    }

    #[test]
    fn disabling_a_group_prunes_its_subtree() {
        let over = Composition::parse("[[entry]]\nid = \"extras\"\ndisabled = true", "over")
            .expect("parses");
        let layered = base().layered(over).expect("layers");
        assert!(layered.resolved().iter().all(|e| e.id != "entities"));
    }

    #[test]
    fn patch_naming_nothing_is_loud() {
        let over = Composition::parse("[[entry]]\nid = \"ghost\"", "over").expect("parses");
        assert!(matches!(
            base().layered(over),
            Err(CompositionError::PatchWithoutTarget(id)) if id == "ghost"
        ));
    }

    #[test]
    fn groups_dissolve_on_resolve() {
        let resolved = base().resolved();
        let ids: Vec<&str> = resolved.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["fs", "index", "entities"]);
    }
}
