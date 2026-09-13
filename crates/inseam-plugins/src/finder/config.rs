//! The finder's configuration: every dial here is query-time tier
//! (`design/index-maintenance.md`), so tuning one never re-indexes, and a
//! request may override any of them for itself alone
//! (`design/vocabulary.md`, observability) — the benchmark harness sweeps a
//! matrix of settings over one index that way.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use inseam_kernel::fragment::RelationKind;
use inseam_kernel::store::RowKind;
use inseam_seams::SeamError;
use inseam_seams::finder::SeedChannel;

/// The `k` in reciprocal rank fusion: the finder's default for fusing its
/// seed lists, and the constant the cross-node merge reuses to fuse
/// ranked lists from differently-profiled indexes (`design/finder.md`).
/// Sixty is the value the RRF paper settled on; a smaller `k` lets a
/// single top rank dominate, a larger one flattens every list together.
pub const RRF_K_DEFAULT: f64 = 60.0;

/// Most overrides one request may carry; a matrix cell needs a handful.
pub const OVERRIDES_MAX: usize = 32;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FinderConfig {
    /// Fragments retrieved from each seed list (full-text and vector).
    pub seed_k: usize,
    /// Which of the text and vector lists run: both, or one alone. The
    /// older spelling of the per-list `enabled` dials; it narrows
    /// `seed_lists` and is kept so compositions that set it keep working.
    pub seeds: SeedLists,
    /// The `k` constant in reciprocal rank fusion.
    pub rrf_k: f64,
    /// The older spelling of `seed_lists.lexical.weight`: multiplies it.
    pub lexical_weight: f64,
    /// Personalized PageRank damping: probability a walk continues instead
    /// of restarting at the seeds. Keeps the boost local.
    pub damping: f64,
    pub iterations: usize,
    pub epsilon: f64,
    /// Fragment hints attached to each result.
    pub max_hints: usize,
    /// Vector hits farther than this cosine distance are noise, not seeds:
    /// nearest-k always returns something, even when nothing is close.
    pub max_vector_distance: f64,
    /// Relation hops loaded around the fused seeds before propagation.
    pub graph_hops: u32,
    /// Hard cap on relations in one query's local graph.
    pub graph_relation_limit: u32,
    pub weights: RelationWeights,
    /// Every seed list's dials: enabled, its vote in fusion, its rank gate.
    pub seed_lists: SeedListTable,
    /// The hub bound (`design/vocabulary.md`): a vertex with more relations
    /// than this contributes no edges to the walk. `0` bounds nothing.
    pub hub_degree_max: u32,
    /// Clusters whose vector's cosine with the query's clears this floor
    /// ground the query by paraphrase.
    pub cluster_query_cosine: f64,
    /// Most clusters one query grounds through.
    pub clusters_per_query_max: u32,
    /// Neighbours named per result in the ledger, under `explain`.
    pub explain_rows_max: u32,
}

/// The seed lists a query runs before fusion — the older two-list dial.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SeedLists {
    #[default]
    Both,
    FullText,
    Vector,
}

impl SeedLists {
    fn runs_full_text(self) -> bool {
        match self {
            Self::Both | Self::FullText => true,
            Self::Vector => false,
        }
    }

    fn runs_vector(self) -> bool {
        match self {
            Self::Both | Self::Vector => true,
            Self::FullText => false,
        }
    }
}

/// One seed list's dials.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SeedList {
    pub enabled: bool,
    /// The list's vote in fusion; `1.0` is an equal vote.
    pub weight: f64,
    /// Only this many of the list's best enter fusion.
    pub ranks_max: u32,
}

impl Default for SeedList {
    fn default() -> Self {
        Self {
            enabled: true,
            weight: 1.0,
            ranks_max: 1_000,
        }
    }
}

/// The five seed lists (`design/vocabulary.md`, retrieval). The vocabulary
/// lists enter at the rank-gated, down-weighted shape the cue vectors
/// measured best at: their job is to add candidates the text lists lack,
/// never to reorder what full-text already carries.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SeedListTable {
    pub prose: SeedList,
    pub lexical: SeedList,
    pub vector: SeedList,
    pub exact: SeedList,
    pub cluster: SeedList,
}

impl Default for SeedListTable {
    fn default() -> Self {
        Self {
            prose: SeedList::default(),
            lexical: SeedList::default(),
            vector: SeedList::default(),
            exact: SeedList {
                enabled: true,
                weight: 1.0,
                ranks_max: 20,
            },
            cluster: SeedList {
                enabled: true,
                weight: 0.3,
                ranks_max: 5,
            },
        }
    }
}

impl SeedListTable {
    pub fn get(&self, channel: SeedChannel) -> SeedList {
        match channel {
            SeedChannel::Prose => self.prose,
            SeedChannel::Lexical => self.lexical,
            SeedChannel::Vector => self.vector,
            SeedChannel::Exact => self.exact,
            SeedChannel::Cluster => self.cluster,
        }
    }
}

impl Default for FinderConfig {
    fn default() -> Self {
        Self {
            seed_k: 60,
            seeds: SeedLists::Both,
            rrf_k: RRF_K_DEFAULT,
            lexical_weight: 1.0,
            damping: 0.5,
            iterations: 12,
            epsilon: 1e-6,
            max_hints: 3,
            max_vector_distance: 0.75,
            graph_hops: 2,
            graph_relation_limit: 20_000,
            weights: RelationWeights::default(),
            seed_lists: SeedListTable::default(),
            hub_degree_max: 500,
            cluster_query_cosine: 0.55,
            clusters_per_query_max: 3,
            explain_rows_max: 3,
        }
    }
}

impl FinderConfig {
    pub fn validate_query_bounds(&self) -> Result<(), String> {
        if !self.lexical_weight.is_finite() {
            return Err("finder.lexical_weight must be finite".to_string());
        }
        if self.lexical_weight <= 0.0 {
            return Err("finder.lexical_weight must be greater than zero".to_string());
        }
        if self.lexical_weight > 1.0 {
            return Err("finder.lexical_weight must not exceed one".to_string());
        }
        if self.seed_k == 0 {
            return Err("finder.seed_k must be greater than zero".to_string());
        }
        if self.graph_hops == 0 {
            return Err("finder.graph_hops must be greater than zero".to_string());
        }
        if self.graph_hops > inseam_kernel::store::RELATION_HOPS_MAX {
            return Err("finder.graph_hops must not exceed four".to_string());
        }
        if self.graph_relation_limit == 0 {
            return Err("finder.graph_relation_limit must be greater than zero".to_string());
        }
        if self.graph_relation_limit > inseam_kernel::store::RELATION_LIMIT_MAX {
            return Err("finder.graph_relation_limit must not exceed 100000".to_string());
        }
        self.validate_lists()
    }

    fn validate_lists(&self) -> Result<(), String> {
        for channel in SeedChannel::ALL {
            let list = self.seed_lists.get(channel);
            if !list.weight.is_finite() || list.weight < 0.0 {
                return Err(format!(
                    "finder.seed_lists.{channel}.weight must be finite and at least zero"
                ));
            }
            if list.ranks_max == 0 {
                return Err(format!(
                    "finder.seed_lists.{channel}.ranks_max must be greater than zero"
                ));
            }
        }
        if !(0.0..1.0).contains(&self.damping) {
            return Err("finder.damping must be in [0, 1)".to_string());
        }
        if !(-1.0..=1.0).contains(&self.cluster_query_cosine) {
            return Err("finder.cluster_query_cosine must be in [-1, 1]".to_string());
        }
        Ok(())
    }

    /// The effective dials of one list once the older spellings are
    /// applied: `seeds` narrows which text and vector lists run, and
    /// `lexical_weight` multiplies the lexical vote.
    pub fn list(&self, channel: SeedChannel) -> SeedList {
        let mut list = self.seed_lists.get(channel);
        match channel {
            SeedChannel::Prose | SeedChannel::Lexical => {
                list.enabled = list.enabled && self.seeds.runs_full_text();
            }
            SeedChannel::Vector => {
                list.enabled = list.enabled && self.seeds.runs_vector();
            }
            SeedChannel::Exact | SeedChannel::Cluster => {}
        }
        if channel == SeedChannel::Lexical {
            list.weight *= self.lexical_weight;
        }
        list
    }

    /// The hub bound as an option: `0` is unbounded.
    pub fn hub_bound(&self) -> Option<u32> {
        if self.hub_degree_max == 0 {
            None
        } else {
            Some(self.hub_degree_max)
        }
    }

    /// This configuration with a request's `key=value` overrides applied.
    /// The keys are the configuration's own (`seed_lists.cluster.weight`,
    /// `weights.by_kind.mentions`, `hub_degree_max`); an unknown key or an
    /// unparsable value is a typed client error, and the result is validated
    /// like a composition's.
    pub fn with_overrides(&self, overrides: &[String]) -> Result<Self, SeamError> {
        if overrides.is_empty() {
            return Ok(self.clone());
        }
        if overrides.len() > OVERRIDES_MAX {
            return Err(SeamError::Invalid(format!(
                "at most {OVERRIDES_MAX} finder overrides per request"
            )));
        }
        let mut table = toml::Table::try_from(self.clone())
            .map_err(|e| SeamError::failed(format!("finder config is not a table: {e}")))?;
        for override_ in overrides {
            let (path, value) = parse_override(override_)?;
            set_path(&mut table, &path, value)?;
        }
        let mut config: Self = table
            .try_into()
            .map_err(|e| SeamError::Invalid(format!("finder override: {e}")))?;
        config.weights = config.weights.over_defaults();
        config
            .validate_query_bounds()
            .map_err(|e| SeamError::Invalid(format!("finder override: {e}")))?;
        Ok(config)
    }
}

/// `a.b.c=value` into its path and a TOML value: a number, a boolean, or a
/// string. Kind names may hold `-` (`links-to`), so a segment is anything
/// but a dot.
fn parse_override(text: &str) -> Result<(Vec<String>, toml::Value), SeamError> {
    let (key, raw) = text
        .split_once('=')
        .ok_or_else(|| SeamError::Invalid(format!("finder override `{text}` is not key=value")))?;
    let path: Vec<String> = key.trim().split('.').map(str::to_string).collect();
    if path.iter().any(String::is_empty) {
        return Err(SeamError::Invalid(format!(
            "finder override `{text}` has an empty key segment"
        )));
    }
    let raw = raw.trim();
    let value = if let Ok(b) = raw.parse::<bool>() {
        toml::Value::Boolean(b)
    } else if let Ok(i) = raw.parse::<i64>() {
        toml::Value::Integer(i)
    } else if let Ok(f) = raw.parse::<f64>() {
        toml::Value::Float(f)
    } else {
        toml::Value::String(raw.trim_matches('"').to_string())
    };
    Ok((path, value))
}

/// Set a nested path in a table, creating tables along the way. A value
/// that is an integer where the config wants a float is coerced, since
/// `weight=1` is what a person types.
fn set_path(table: &mut toml::Table, path: &[String], value: toml::Value) -> Result<(), SeamError> {
    assert!(!path.is_empty());
    let (last, parents) = path.split_last().expect("path is non-empty");
    let mut current = table;
    for segment in parents {
        current = current
            .entry(segment.clone())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| {
                SeamError::Invalid(format!("finder override `{segment}` is not a table"))
            })?;
    }
    let coerced = match (current.get(last), value) {
        (Some(toml::Value::Float(_)), toml::Value::Integer(i)) => {
            toml::Value::Float(f64::from(i32::try_from(i).unwrap_or(i32::MAX)))
        }
        (_, value) => value,
    };
    current.insert(last.clone(), coerced);
    Ok(())
}

/// How strongly each relation kind conducts relevance during propagation.
/// Kinds are an open vocabulary (`design/kernel.md`), so this is a map by
/// kind name plus a default for kinds it does not list; the built-in table
/// tunes the kinds the first-party transforms emit, and a composition may
/// add or override entries (`[entry.config.weights]`). A second axis weighs
/// the **row kind** at the far end of an edge (`design/vocabulary.md`): a
/// shared ticket number says more than a shared jargon word.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelationWeights {
    /// Weight for any kind `by_kind` does not name.
    pub default: f64,
    /// Weight per relation kind name (`"links-to" = 0.4`).
    pub by_kind: BTreeMap<String, f64>,
    /// Weight per row kind of the fragment mass flows *into*.
    pub by_row_kind: BTreeMap<RowKind, f64>,
}

impl Default for RelationWeights {
    fn default() -> Self {
        Self {
            default: 0.5,
            by_kind: BTreeMap::from([
                ("contains".to_string(), 1.0),
                ("derives".to_string(), 0.9),
                ("links-to".to_string(), 0.4),
                ("mentions".to_string(), 0.8),
                ("aliases".to_string(), 0.8),
                ("authored".to_string(), 0.8),
                ("faceted".to_string(), 0.5),
                ("transcribes".to_string(), 1.0),
            ]),
            by_row_kind: BTreeMap::from([
                (RowKind::Prose, 1.0),
                (RowKind::Summary, 1.0),
                (RowKind::Entry, 1.0),
                (RowKind::Identifier, 1.0),
                (RowKind::Entity, 0.8),
                (RowKind::Term, 0.6),
                (RowKind::Facet, 0.5),
                (RowKind::Alias, 0.4),
                (RowKind::Other, 0.5),
            ]),
        }
    }
}

impl RelationWeights {
    pub fn weight(&self, kind: &RelationKind) -> f64 {
        self.by_kind
            .get(kind.as_str())
            .copied()
            .unwrap_or(self.default)
    }

    pub fn row_weight(&self, kind: RowKind) -> f64 {
        self.by_row_kind.get(&kind).copied().unwrap_or(1.0)
    }

    /// Whether any row kind conducts at other than full weight — when none
    /// does, the walk needs no row kinds at all.
    pub fn row_kinds_matter(&self) -> bool {
        self.by_row_kind
            .values()
            .any(|w| (*w - 1.0).abs() > f64::EPSILON)
    }

    /// Layer configured weights over the built-in table, so naming one kind
    /// in a composition does not silently zero the rest.
    pub(crate) fn over_defaults(self) -> Self {
        let mut merged = Self {
            default: self.default,
            ..Self::default()
        };
        merged.by_kind.extend(self.by_kind);
        merged.by_row_kind.extend(self.by_row_kind);
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_set_nested_dials_and_reject_unknown_keys() {
        let base = FinderConfig::default();
        let over = base
            .with_overrides(&[
                "seed_lists.cluster.weight=0".to_string(),
                "weights.by_kind.mentions=0.1".to_string(),
                "hub_degree_max=12".to_string(),
                "seeds=full-text".to_string(),
            ])
            .expect("applies");
        assert_eq!(over.seed_lists.cluster.weight, 0.0);
        assert_eq!(
            over.weights
                .weight(&RelationKind::new("mentions").expect("valid")),
            0.1
        );
        assert_eq!(over.hub_degree_max, 12);
        assert_eq!(over.seeds, SeedLists::FullText);
        assert!(!over.list(SeedChannel::Vector).enabled);
        assert!(base.with_overrides(&["nope=1".to_string()]).is_err());
        assert!(base.with_overrides(&["damping".to_string()]).is_err());
        assert!(base.with_overrides(&["damping=2".to_string()]).is_err());
    }

    #[test]
    fn the_older_spellings_narrow_the_lists() {
        let config = FinderConfig {
            seeds: SeedLists::Vector,
            lexical_weight: 0.5,
            ..FinderConfig::default()
        };
        assert!(!config.list(SeedChannel::Prose).enabled);
        assert!(!config.list(SeedChannel::Lexical).enabled);
        assert!(config.list(SeedChannel::Vector).enabled);
        assert!(config.list(SeedChannel::Exact).enabled);
        let both = FinderConfig::default();
        assert_eq!(both.list(SeedChannel::Lexical).weight, 1.0);
        let halved = FinderConfig {
            lexical_weight: 0.5,
            ..FinderConfig::default()
        };
        assert_eq!(halved.list(SeedChannel::Lexical).weight, 0.5);
    }

    #[test]
    fn configured_weights_layer_over_both_tables() {
        let configured: RelationWeights = toml::from_str(
            "default = 0.1
[by_kind]
cites = 0.7
[by_row_kind]
term = 0.9",
        )
        .expect("parses");
        let merged = configured.over_defaults();
        assert_eq!(merged.weight(&RelationKind::contains()), 1.0);
        assert_eq!(
            merged.weight(&RelationKind::new("cites").expect("valid")),
            0.7
        );
        assert_eq!(merged.row_weight(RowKind::Term), 0.9);
        assert_eq!(merged.row_weight(RowKind::Alias), 0.4);
        assert!(merged.row_kinds_matter());
    }

    #[test]
    fn hub_bound_zero_is_unbounded() {
        assert_eq!(
            FinderConfig {
                hub_degree_max: 0,
                ..FinderConfig::default()
            }
            .hub_bound(),
            None
        );
        assert_eq!(FinderConfig::default().hub_bound(), Some(500));
    }
}
