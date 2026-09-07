//! Composition: the declarative tree of plugin entries a node runs
//! (`design/composition.md`). Layers patch earlier layers by entry id —
//! distribution base, then node config, then invocation overlays — and the
//! same pure layering function answers `inseam config --resolved`, so what
//! prints is what boots.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Most entries one composition may hold, groups and leaves together — far
/// above any real node, present so every walk over the tree has a bound.
/// `usize` because it bounds `Vec` lengths.
pub const ENTRY_COUNT_MAX: usize = 1024;

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
    #[error("composition has more than {ENTRY_COUNT_MAX} entries")]
    TooManyEntries,
}

/// One entry: a plugin mounted with a config. Groups are ordinary entries
/// with children, so subtrees can be toggled and shipped as units.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    /// Plugin ref: a linked plugin's name from the distribution, or a scheme ref the
    /// distribution registered a resolver for (`wasm:<artifact>`). Absent in
    /// a patch layer entry that only overrides config/disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub config: toml::Table,
    /// Mount toggle. Optional so a patch can leave it alone: a config-only
    /// patch must not re-enable an entry its base layer disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
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

    pub fn is_disabled(&self) -> bool {
        self.disabled == Some(true)
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

    /// Every entry has an id, ids are unique across the whole tree, and the
    /// tree is within bounds.
    fn validate(&self) -> Result<(), CompositionError> {
        let mut seen = std::collections::HashSet::new();
        for entry in walk(&self.entries, |_| false)? {
            if entry.id.is_empty() {
                return Err(CompositionError::MissingId);
            }
            if !seen.insert(entry.id.as_str()) {
                return Err(CompositionError::DuplicateInLayer(entry.id.clone()));
            }
        }
        Ok(())
    }

    /// Patch this composition with a later layer, by entry id. Semantics are
    /// whole-entry replacement of `config` (simple over clever, per the
    /// design's open-question resolution); `disabled` and `plugin` override
    /// when present; unmatched ids append as new entries — at the top level
    /// for the overlay's own entries, under the matched parent for nested
    /// ones.
    pub fn layered(mut self, over: Composition) -> Result<Composition, CompositionError> {
        // Worklist of (patch, path of the entry list an unmatched patch
        // appends to). Iterative and bounded, like every tree walk here.
        let mut pending: Vec<(Entry, Vec<usize>)> = over
            .entries
            .into_iter()
            .rev()
            .map(|patch| (patch, Vec::new()))
            .collect();
        let mut applied: usize = 0;
        while let Some((patch, append_at)) = pending.pop() {
            applied += 1;
            if applied > ENTRY_COUNT_MAX {
                return Err(CompositionError::TooManyEntries);
            }
            match path_of(&self.entries, &patch.id)? {
                Some(path) => {
                    let target = entry_at_mut(&mut self.entries, &path);
                    if let Some(plugin) = patch.plugin {
                        target.plugin = Some(plugin);
                    }
                    if !patch.config.is_empty() {
                        target.config = patch.config;
                    }
                    if let Some(disabled) = patch.disabled {
                        target.disabled = Some(disabled);
                    }
                    pending.extend(
                        patch
                            .entries
                            .into_iter()
                            .rev()
                            .map(|child| (child, path.clone())),
                    );
                }
                None => {
                    if patch.plugin.is_none() {
                        return Err(CompositionError::PatchWithoutTarget(patch.id));
                    }
                    entries_at_mut(&mut self.entries, &append_at).push(patch);
                }
            }
        }
        self.validate()?;
        Ok(self)
    }

    /// Flatten the tree into mountable entries: groups dissolve, a disabled
    /// entry prunes its whole subtree. Entries without a plugin ref are pure
    /// groups and mount nothing themselves.
    pub fn resolved(&self) -> Vec<Entry> {
        // Every construction path (`parse`, `load`, `layered`) validated the
        // bound; an oversized hand-built tree is a programmer error.
        walk(&self.entries, Entry::is_disabled)
            .expect("a composition is validated before it is resolved")
            .into_iter()
            .filter(|e| e.plugin.is_some())
            .map(|e| Entry {
                entries: Vec::new(),
                ..e.clone()
            })
            .collect()
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Put `patch` into this layer with the same semantics layering gives
    /// a patch entry: an existing entry with its id takes the patch's
    /// `plugin` and `disabled` when present and its `config` when
    /// nonempty; an unknown id is appended at the top level. This is how a
    /// settings write lands in an overlay — the file is the truth a fresh
    /// boot converges to, so the overlay is edited, never the base.
    pub fn configure_entry(&mut self, patch: &Entry) -> Result<(), CompositionError> {
        match path_of(&self.entries, &patch.id)? {
            Some(path) => {
                let target = entry_at_mut(&mut self.entries, &path);
                if let Some(plugin) = &patch.plugin {
                    target.plugin = Some(plugin.clone());
                }
                if !patch.config.is_empty() {
                    target.config = patch.config.clone();
                }
                if let Some(disabled) = patch.disabled {
                    target.disabled = Some(disabled);
                }
            }
            None => {
                if self.entries.len() >= ENTRY_COUNT_MAX {
                    return Err(CompositionError::TooManyEntries);
                }
                self.entries.push(patch.clone());
            }
        }
        Ok(())
    }
}

/// Pre-order walk of an entry tree, without recursion: an explicit stack,
/// bounded by [`ENTRY_COUNT_MAX`]. An entry `prune` accepts is skipped along
/// with its whole subtree.
fn walk(
    entries: &[Entry],
    prune: impl Fn(&Entry) -> bool,
) -> Result<Vec<&Entry>, CompositionError> {
    let mut out: Vec<&Entry> = Vec::new();
    let mut stack: Vec<&Entry> = entries.iter().rev().collect();
    while let Some(entry) = stack.pop() {
        if prune(entry) {
            continue;
        }
        if out.len() >= ENTRY_COUNT_MAX {
            return Err(CompositionError::TooManyEntries);
        }
        out.push(entry);
        stack.extend(entry.entries.iter().rev());
    }
    Ok(out)
}

/// The index path (child indices, root first) of the entry with `id`,
/// anywhere in the tree.
fn path_of(entries: &[Entry], id: &str) -> Result<Option<Vec<usize>>, CompositionError> {
    let mut stack: Vec<(&Entry, Vec<usize>)> = entries
        .iter()
        .enumerate()
        .rev()
        .map(|(index, entry)| (entry, vec![index]))
        .collect();
    let mut visited: usize = 0;
    while let Some((entry, path)) = stack.pop() {
        visited += 1;
        if visited > ENTRY_COUNT_MAX {
            return Err(CompositionError::TooManyEntries);
        }
        if entry.id == id {
            return Ok(Some(path));
        }
        for (index, child) in entry.entries.iter().enumerate().rev() {
            let mut child_path = path.clone();
            child_path.push(index);
            stack.push((child, child_path));
        }
    }
    Ok(None)
}

/// The entry list at `path` — the root list for the empty path, otherwise
/// the children of the entry the path names.
fn entries_at_mut<'a>(entries: &'a mut Vec<Entry>, path: &[usize]) -> &'a mut Vec<Entry> {
    let mut current = entries;
    for index in path {
        current = &mut current[*index].entries;
    }
    current
}

fn entry_at_mut<'a>(entries: &'a mut Vec<Entry>, path: &[usize]) -> &'a mut Entry {
    let (last, parents) = path.split_last().expect("a path names at least one entry");
    &mut entries_at_mut(entries, parents)[*last]
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

            [[entry]]
            id = "parked"
            plugin = "ocr"
            disabled = true
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
    fn rejects_oversized_trees() {
        let text: String = (0..=ENTRY_COUNT_MAX)
            .map(|i| format!("[[entry]]\nid = \"e{i}\"\nplugin = \"x\"\n"))
            .collect();
        let err = Composition::parse(&text, "big").expect_err("too many entries refuse");
        assert!(matches!(err, CompositionError::TooManyEntries));
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
    fn nested_patches_append_under_their_parent() {
        let over = Composition::parse(
            r#"
            [[entry]]
            id = "extras"
            [[entry.entries]]
            id = "links"
            plugin = "transform-links"
            "#,
            "over",
        )
        .expect("overlay parses");
        let layered = base().layered(over).expect("layers");
        let extras = layered
            .entries
            .iter()
            .find(|e| e.id == "extras")
            .expect("group kept");
        let ids: Vec<&str> = extras.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["entities", "links"]);
    }

    #[test]
    fn disabling_a_group_prunes_its_subtree() {
        let over = Composition::parse("[[entry]]\nid = \"extras\"\ndisabled = true", "over")
            .expect("parses");
        let layered = base().layered(over).expect("layers");
        assert!(layered.resolved().iter().all(|e| e.id != "entities"));
    }

    #[test]
    fn configure_entry_patches_in_place_or_appends() {
        let mut overlay = Composition::parse(
            "[[entry]]\nid = \"llm\"\ndisabled = true\n[entry.config]\nagent_model = \"a\"\n",
            "test",
        )
        .unwrap();
        // A toggle-only patch keeps the config the overlay already holds.
        overlay
            .configure_entry(&Entry {
                id: "llm".to_string(),
                disabled: Some(false),
                ..Entry::default()
            })
            .unwrap();
        assert_eq!(overlay.entries[0].disabled, Some(false));
        assert_eq!(overlay.entries[0].config["agent_model"].as_str(), Some("a"));
        // A config patch replaces the table wholesale.
        let mut config = toml::Table::new();
        config.insert("target_chars".to_string(), toml::Value::Integer(9));
        overlay
            .configure_entry(&Entry {
                id: "chunker".to_string(),
                config: config.clone(),
                disabled: Some(false),
                ..Entry::default()
            })
            .unwrap();
        assert_eq!(overlay.entries.len(), 2);
        assert_eq!(overlay.entries[1].id, "chunker");
        assert_eq!(overlay.entries[1].config, config);
    }

    #[test]
    fn config_only_patch_keeps_the_base_disabled_flag() {
        let over = Composition::parse(
            "[[entry]]\nid = \"parked\"\n[entry.config]\nlang = \"eng\"",
            "over",
        )
        .expect("parses");
        let layered = base().layered(over).expect("layers");
        assert!(
            layered.resolved().iter().all(|e| e.id != "parked"),
            "a patch that says nothing about `disabled` leaves it alone"
        );
        let enable = Composition::parse("[[entry]]\nid = \"parked\"\ndisabled = false", "over")
            .expect("parses");
        let layered = base().layered(enable).expect("layers");
        assert!(layered.resolved().iter().any(|e| e.id == "parked"));
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
