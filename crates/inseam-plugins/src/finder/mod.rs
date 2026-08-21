//! The default `finder` provider (`design/finder.md`): seed with hybrid
//! search — full-text and vector, fused by reciprocal rank — then let the
//! graph boost what search alone would underrank, via personalized PageRank
//! over the relation graph. Ranked fragments roll up to their sources.
//! Every config dial here is query-time tier: tuning it never re-indexes.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::Deserialize;

use inseam_kernel::fragment::{FragmentId, Relation, RelationKind};
use inseam_kernel::store::{IndexStore, SourceId, StoredSource};
use inseam_kernel::substrate::{
    parse_config, ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, STORE,
};
use inseam_seams::embedder::{Embedder, EMBEDDER};
use inseam_seams::finder::{Expansion, Finder, RankedFragment, RankedSource, FINDER};
use inseam_seams::SeamError;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FinderConfig {
    /// Fragments retrieved from each seed list (full-text and vector).
    pub seed_k: usize,
    /// The `k` constant in reciprocal rank fusion.
    pub rrf_k: f64,
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
    pub weights: RelationWeights,
}

impl Default for FinderConfig {
    fn default() -> Self {
        Self {
            seed_k: 60,
            rrf_k: 60.0,
            damping: 0.5,
            iterations: 12,
            epsilon: 1e-6,
            max_hints: 3,
            max_vector_distance: 0.75,
            weights: RelationWeights::default(),
        }
    }
}

/// How strongly each relation kind conducts relevance during propagation.
/// Kinds are an open vocabulary (`design/kernel.md`), so this is a map by
/// kind name plus a default for kinds it does not list; the built-in table
/// tunes the kinds the first-party transforms emit, and a composition may
/// add or override entries (`[entry.config.weights]`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RelationWeights {
    /// Weight for any kind `by_kind` does not name.
    pub default: f64,
    /// Weight per relation kind name (`"links-to" = 0.4`).
    pub by_kind: BTreeMap<String, f64>,
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
                ("transcribes".to_string(), 1.0),
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

    /// Layer configured weights over the built-in table, so naming one kind
    /// in a composition does not silently zero the rest.
    fn over_defaults(self) -> Self {
        let mut merged = Self {
            default: self.default,
            ..Self::default()
        };
        merged.by_kind.extend(self.by_kind);
        merged
    }
}

pub struct FinderPlugin {
    config: FinderConfig,
}

impl FinderPlugin {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        let mut config: FinderConfig = parse_config(config)?;
        config.weights = config.weights.over_defaults();
        Ok(Self { config })
    }
}

pub struct FinderFactory;

impl inseam_kernel::substrate::PluginFactory for FinderFactory {
    fn name(&self) -> &str {
        "finder"
    }

    fn build(&self, config: &toml::Table) -> Result<Box<dyn Plugin>, PluginError> {
        Ok(Box::new(FinderPlugin::from_config(config)?))
    }
}

#[async_trait::async_trait]
impl Plugin for FinderPlugin {
    fn manifest(&self) -> Manifest {
        static INJECT: &[Inject] = &[Inject::required("store"), Inject::required("embedder")];
        Manifest {
            name: "finder",
            inject: INJECT,
            provides: &["finder"],
        }
    }

    async fn apply(&self, cx: &mut ApplyCx<'_>) -> Result<(), PluginError> {
        let service = FinderService {
            store: cx.get(&STORE)?,
            embedder: cx.get(&EMBEDDER)?,
            config: self.config.clone(),
        };
        cx.provide(&FINDER, Arc::new(service) as Arc<dyn Finder>, Facts::new())?;
        Ok(())
    }
}

pub struct FinderService {
    store: Arc<IndexStore>,
    embedder: Arc<dyn Embedder>,
    config: FinderConfig,
}

impl FinderService {
    /// Construct outside the plugin tree (tests, embedding scenarios). The
    /// plugin pathway goes through [`FinderPlugin::apply`].
    pub fn new(store: Arc<IndexStore>, embedder: Arc<dyn Embedder>, config: FinderConfig) -> Self {
        Self {
            store,
            embedder,
            config,
        }
    }
}

#[async_trait::async_trait]
impl Finder for FinderService {
    async fn query(&self, text: &str, limit: usize) -> Result<Vec<RankedSource>, SeamError> {
        let fts = self.store.search_fts(text, self.config.seed_k).await?;
        let mut vector = match self.embedder.dimensions() {
            Some(_) => {
                let qvec = self.embedder.embed(&[text]).await?;
                match qvec.first() {
                    Some(v) => self.store.search_vector(v, self.config.seed_k).await?,
                    None => Vec::new(),
                }
            }
            None => Vec::new(),
        };
        // Nearest-k returns the k nearest whatever the distance; beyond the
        // floor a "neighbor" is noise and must not seed the walk.
        vector.retain(|(_, distance)| f64::from(*distance) <= self.config.max_vector_distance);

        // Both lists arrive best-first; fusion cares only about rank.
        let fts_ranked: Vec<i64> = fts.iter().map(|(id, _)| id.0).collect();
        let vec_ranked: Vec<i64> = vector.iter().map(|(id, _)| id.0).collect();
        let seeds = rrf_fuse(&[&fts_ranked, &vec_ranked], self.config.rrf_k);
        if seeds.is_empty() {
            return Ok(Vec::new());
        }

        let relations = self.store.all_relations().await?;
        let edges = weighted_edges(&relations, &self.config.weights);
        let boosted = personalized_pagerank(
            &seeds,
            &edges,
            self.config.damping,
            self.config.iterations,
            self.config.epsilon,
        );

        // Boost, never gate: activation adds to the seed score, so a
        // fragment with no useful relations keeps its seed standing.
        let mut final_scores: HashMap<i64, f64> = seeds.clone();
        for (id, score) in boosted {
            *final_scores.entry(id).or_insert(0.0) += score;
        }

        self.rollup(final_scores, limit).await
    }

    async fn expand(&self, source: &StoredSource) -> Result<Expansion, SeamError> {
        let fragments = self.store.fragments_of(source.id).await?;
        let ids: Vec<FragmentId> = fragments.iter().map(|f| f.id).collect();
        let relations = self.store.relations_touching(&ids).await?;
        let known: std::collections::HashSet<i64> = ids.iter().map(|f| f.0).collect();
        let mut foreign_ids: Vec<FragmentId> = relations
            .iter()
            .flat_map(|r| [r.from, r.to])
            .filter(|f| !known.contains(&f.0))
            .collect();
        foreign_ids.sort();
        foreign_ids.dedup();
        let neighbors = self.store.fragments(&foreign_ids).await?;
        Ok(Expansion {
            fragments,
            relations,
            neighbors,
        })
    }
}

impl FinderService {
    /// Group fragment scores by source, aggregate, and dress results with
    /// envelope, summary, and hints.
    async fn rollup(
        &self,
        final_scores: HashMap<i64, f64>,
        limit: usize,
    ) -> Result<Vec<RankedSource>, SeamError> {
        let ids: Vec<FragmentId> = final_scores.keys().map(|id| FragmentId(*id)).collect();
        let owners = self.store.sources_of_fragments(&ids).await?;

        let mut per_source: HashMap<SourceId, Vec<ScoredFragment>> = HashMap::new();
        for (fid, sid) in owners {
            let score = final_scores[&fid.0];
            per_source.entry(sid).or_default().push((fid, score));
        }

        let mut ranked: Vec<(SourceId, f64, Vec<ScoredFragment>)> = per_source
            .into_iter()
            .map(|(sid, mut frags)| {
                frags.sort_by(|a, b| b.1.total_cmp(&a.1));
                (sid, source_score(&frags), frags)
            })
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(limit);

        let top = ranked.first().map(|(_, s, _)| *s).unwrap_or(1.0);
        let norm = if top > 0.0 { top } else { 1.0 };

        let mut out = Vec::with_capacity(ranked.len());
        for (sid, score, frags) in ranked {
            let Some(source) = self.store.source(sid).await? else {
                continue;
            };
            let summary = self.store.summary_of(sid).await?;
            let mut hints = Vec::new();
            for (fid, fscore) in &frags {
                if hints.len() >= self.config.max_hints {
                    break;
                }
                let Some(fragment) = self.store.fragment(*fid).await? else {
                    continue;
                };
                // Summaries ride along separately; text-less roots hint
                // nothing.
                if fragment.mimetype.is_summary() || fragment.text.is_none() {
                    continue;
                }
                hints.push(RankedFragment {
                    fragment,
                    score: fscore / norm,
                });
            }
            out.push(RankedSource {
                source,
                score: score / norm,
                summary,
                hints,
            });
        }
        Ok(out)
    }
}

/// Reciprocal rank fusion over best-first id lists: score(d) = Σ 1/(k + rank).
/// Rank-based, so BM25 scores and cosine distances never need to be made
/// comparable — the reason RRF is the working fusion (`design/finder.md`).
/// Only an id's best rank in each list counts.
pub fn rrf_fuse(lists: &[&[i64]], k: f64) -> HashMap<i64, f64> {
    let mut fused: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        let mut seen = std::collections::HashSet::new();
        for (rank, id) in list.iter().enumerate() {
            if seen.insert(*id) {
                *fused.entry(*id).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
            }
        }
    }
    fused
}

/// Undirected weighted edges from the relation graph, weights by kind.
/// Parallel edges between a pair sum.
pub fn weighted_edges(relations: &[Relation], weights: &RelationWeights) -> Vec<(i64, i64, f64)> {
    let mut merged: HashMap<(i64, i64), f64> = HashMap::new();
    for r in relations {
        if r.from == r.to {
            continue;
        }
        let w = weights.weight(&r.kind);
        if w <= 0.0 {
            continue;
        }
        let key = if r.from.0 <= r.to.0 {
            (r.from.0, r.to.0)
        } else {
            (r.to.0, r.from.0)
        };
        *merged.entry(key).or_insert(0.0) += w;
    }
    merged.into_iter().map(|((a, b), w)| (a, b, w)).collect()
}

/// Personalized PageRank by power iteration. Seeds are the restart
/// distribution (normalized inside); edges are undirected and weighted.
/// Walks continue with probability `damping`; dangling mass restarts at the
/// seeds. Returns a distribution over every node walks can reach.
pub fn personalized_pagerank(
    seeds: &HashMap<i64, f64>,
    edges: &[(i64, i64, f64)],
    damping: f64,
    iterations: usize,
    epsilon: f64,
) -> HashMap<i64, f64> {
    let total: f64 = seeds.values().copied().sum();
    if total <= 0.0 {
        return HashMap::new();
    }

    // Node universe: seeds plus every edge endpoint.
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut nodes: Vec<i64> = Vec::new();
    let intern = |id: i64, index: &mut HashMap<i64, usize>, nodes: &mut Vec<i64>| -> usize {
        *index.entry(id).or_insert_with(|| {
            nodes.push(id);
            nodes.len() - 1
        })
    };
    for id in seeds.keys() {
        intern(*id, &mut index, &mut nodes);
    }
    for (a, b, _) in edges {
        intern(*a, &mut index, &mut nodes);
        intern(*b, &mut index, &mut nodes);
    }

    let n = nodes.len();
    let mut adjacency: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut out_weight: Vec<f64> = vec![0.0; n];
    for (a, b, w) in edges {
        let (ia, ib) = (index[a], index[b]);
        adjacency[ia].push((ib, *w));
        adjacency[ib].push((ia, *w));
        out_weight[ia] += *w;
        out_weight[ib] += *w;
    }

    let mut restart = vec![0.0; n];
    for (id, score) in seeds {
        restart[index[id]] = score / total;
    }

    let mut p = restart.clone();
    for _ in 0..iterations {
        let mut next = vec![0.0; n];
        let mut dangling = 0.0;
        for i in 0..n {
            if p[i] == 0.0 {
                continue;
            }
            if out_weight[i] == 0.0 {
                dangling += p[i];
                continue;
            }
            let share = p[i] / out_weight[i];
            for (j, w) in &adjacency[i] {
                next[*j] += share * w;
            }
        }
        let mut delta = 0.0;
        for i in 0..n {
            let value = (1.0 - damping) * restart[i] + damping * (next[i] + dangling * restart[i]);
            delta += (value - p[i]).abs();
            p[i] = value;
        }
        if delta < epsilon {
            break;
        }
    }

    nodes
        .into_iter()
        .enumerate()
        .filter(|(i, _)| p[*i] > 0.0)
        .map(|(i, id)| (id, p[i]))
        .collect()
}

/// A fragment id with its final (seed + activated) score.
type ScoredFragment = (FragmentId, f64);

/// Max plus a tapered bonus for additional independent hits: sum invites
/// long-document bias, max alone ignores corroboration (`design/finder.md`).
fn source_score(sorted: &[ScoredFragment]) -> f64 {
    let mut score = 0.0;
    for (rank, (_, s)) in sorted.iter().take(3).enumerate() {
        let weight = match rank {
            0 => 1.0,
            1 => 0.1,
            _ => 0.05,
        };
        score += weight * s;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights() -> RelationWeights {
        RelationWeights::default()
    }

    #[test]
    fn rrf_rewards_presence_in_both_lists() {
        let fused = rrf_fuse(&[&[1, 2, 3], &[2, 9]], 60.0);
        assert!(fused[&2] > fused[&1], "rank-2+rank-1 beats a single rank-1");
        assert!(fused[&1] > fused[&3]);
        assert!(fused.contains_key(&9));
    }

    #[test]
    fn rrf_of_empty_lists_is_empty() {
        assert!(rrf_fuse(&[&[], &[]], 60.0).is_empty());
    }

    #[test]
    fn ppr_flows_relevance_to_connected_neighbors() {
        let seeds = HashMap::from([(1, 1.0)]);
        let edges = vec![(1, 2, 1.0), (2, 3, 1.0), (4, 5, 1.0)];
        let p = personalized_pagerank(&seeds, &edges, 0.5, 20, 1e-9);
        assert!(p[&2] > p[&3], "one hop beats two hops");
        assert!(p[&3] > 0.0, "two hops still reached");
        assert!(!p.contains_key(&4), "disconnected components get nothing");
    }

    #[test]
    fn ppr_respects_edge_weights() {
        let seeds = HashMap::from([(1, 1.0)]);
        let heavy = personalized_pagerank(&seeds, &[(1, 2, 1.0), (1, 3, 0.1)], 0.5, 20, 1e-9);
        assert!(heavy[&2] > heavy[&3]);
    }

    #[test]
    fn ppr_without_edges_returns_the_restart_distribution() {
        let seeds = HashMap::from([(1, 3.0), (2, 1.0)]);
        let p = personalized_pagerank(&seeds, &[], 0.5, 20, 1e-9);
        assert!((p[&1] - 0.75).abs() < 1e-9);
        assert!((p[&2] - 0.25).abs() < 1e-9);
    }

    #[test]
    fn ppr_of_empty_seeds_is_empty() {
        assert!(personalized_pagerank(&HashMap::new(), &[(1, 2, 1.0)], 0.5, 10, 1e-9).is_empty());
    }

    #[test]
    fn weighted_edges_merge_parallel_and_drop_self_loops() {
        let kind = |k: &str| RelationKind::new(k).expect("valid kind");
        let relations = vec![
            Relation::new(FragmentId(1), kind("contains"), FragmentId(2)),
            Relation::new(FragmentId(2), kind("mentions"), FragmentId(1)),
            Relation::new(FragmentId(3), kind("contains"), FragmentId(3)),
        ];
        let edges = weighted_edges(&relations, &weights());
        assert_eq!(edges.len(), 1);
        let (a, b, w) = edges[0];
        assert_eq!((a, b), (1, 2));
        assert!((w - 1.8).abs() < 1e-9, "contains 1.0 + mentions 0.8");
    }

    #[test]
    fn unknown_relation_kinds_get_the_default_weight_and_config_layers_over_it() {
        let weights = weights();
        assert_eq!(weights.weight(&RelationKind::new("cites").expect("valid")), 0.5);
        let configured: RelationWeights =
            toml::from_str("default = 0.1
[by_kind]
\"links-to\" = 0.2
cites = 0.7").expect("parses");
        let merged = configured.over_defaults();
        assert_eq!(merged.weight(&RelationKind::new("links-to").expect("valid")), 0.2);
        assert_eq!(merged.weight(&RelationKind::new("cites").expect("valid")), 0.7);
        assert_eq!(merged.weight(&RelationKind::contains()), 1.0, "unnamed kinds keep the table");
        assert_eq!(merged.weight(&RelationKind::new("other").expect("valid")), 0.1);
    }

    #[test]
    fn source_score_prefers_corroborated_sources_without_length_bias() {
        let strong_single = source_score(&[(FragmentId(1), 1.0)]);
        let corroborated = source_score(&[
            (FragmentId(1), 1.0),
            (FragmentId(2), 0.8),
            (FragmentId(3), 0.5),
        ]);
        let long_weak: Vec<(FragmentId, f64)> = (0..50).map(|i| (FragmentId(i), 0.2)).collect();
        assert!(corroborated > strong_single);
        assert!(strong_single > source_score(&long_weak));
    }
}
