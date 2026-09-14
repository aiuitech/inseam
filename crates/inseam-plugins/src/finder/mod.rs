//! The default `finder` provider (`design/finder.md`): ground the query
//! against the vocabulary, seed with hybrid search — full-text, vector,
//! exact and cluster grounding, fused by reciprocal rank — then let the
//! graph boost what search alone would underrank, via personalized
//! PageRank over the relation graph under the hub bound. Ranked fragments
//! roll up to their sources. Every config dial here is query-time tier:
//! tuning it never re-indexes, and a request may override any of them
//! (`design/vocabulary.md`, observability). Under `explain` every result
//! carries the exact decomposition of its score.

mod config;
mod seeds;
mod walk;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use inseam_kernel::address::ContentDigest;
use inseam_kernel::fragment::{FragmentId, Relation, RelationKind};
use inseam_kernel::store::{
    IndexStore, RowKind, SourceId, StoredSource, VocabularyKind, VocabularyOrigin, VocabularyRow,
};
use inseam_kernel::substrate::{
    ApplyCx, Facts, Inject, Manifest, Plugin, PluginError, STORE, parse_config,
};
use inseam_seams::SeamError;
use inseam_seams::embedder::{EMBEDDER, Embedder};
use inseam_seams::finder::{
    CarryingRow, ChannelLine, Discovery, ExcludedHub, Expansion, FINDER, Finder, FinderRequest,
    FragmentEvidence, Ledger, QueryFilters, QueryTrace, RankedFragment, RankedSource, SeedChannel,
    SourceEvidence,
};

pub use config::{
    FinderConfig, OVERRIDES_MAX, RRF_K_DEFAULT, RelationWeights, SeedList, SeedListTable, SeedLists,
};
pub use seeds::{cosine, nearest_clusters, query_tokens, rrf_fuse};
pub use walk::{Graph, personalized_pagerank, weighted_edges};

use seeds::{ClusterCache, Seeder, Seeds, SharedClusterCache};
use walk::sum_columns;

/// The rollup: a source scores its best fragment plus a tapering bonus
/// for additional hits (`design/finder.md`).
const SOURCE_SCORE_WEIGHTS: [f64; 3] = [1.0, 0.1, 0.05];
/// Most sources a facet filter resolves through one row.
const FACET_SOURCES_MAX: u32 = 200_000;
/// Longest hub or carrying-row text the ledger carries.
const LEDGER_TEXT_CHARS_MAX: usize = 120;

pub struct FinderPlugin {
    config: FinderConfig,
}

impl FinderPlugin {
    pub fn from_config(config: &toml::Table) -> Result<Self, PluginError> {
        let mut config: FinderConfig = parse_config(config)?;
        config.validate_query_bounds().map_err(PluginError)?;
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
        let service = FinderService::new(cx.get(&STORE)?, cx.get(&EMBEDDER)?, self.config.clone());
        cx.provide(&FINDER, Arc::new(service) as Arc<dyn Finder>, Facts::new())?;
        Ok(())
    }
}

pub struct FinderService {
    store: Arc<IndexStore>,
    embedder: Arc<dyn Embedder>,
    config: FinderConfig,
    clusters: SharedClusterCache,
}

impl FinderService {
    /// Construct outside the plugin tree (tests, embedding scenarios). The
    /// plugin pathway goes through [`FinderPlugin::apply`].
    pub fn new(store: Arc<IndexStore>, embedder: Arc<dyn Embedder>, config: FinderConfig) -> Self {
        Self {
            store,
            embedder,
            config,
            clusters: Arc::new(tokio::sync::Mutex::new(ClusterCache::default())),
        }
    }
}

/// The edges the mined rows among the seeds contribute, and the rows the
/// hub bound kept out.
#[derive(Default)]
struct Grounded {
    relations: Vec<Relation>,
    hubs: Vec<(FragmentId, u32)>,
}

/// The walk's output for one query: the graph, the per-column mass, and
/// its column sum — everything the rollup and the ledger read.
struct Walked {
    graph: Graph,
    columns: usize,
    mass: Vec<f64>,
    total: Vec<f64>,
    row_kinds: HashMap<FragmentId, RowKind>,
    hubs: Vec<(FragmentId, u32)>,
}

impl Walked {
    /// A fragment's walk mass, all columns.
    fn boost(&self, id: i64) -> f64 {
        self.graph.index_of(id).map_or(0.0, |i| self.total[i])
    }

    /// A fragment's walk mass per column.
    fn boost_by_column(&self, id: i64) -> Vec<f64> {
        match self.graph.index_of(id) {
            Some(i) => self.mass[i * self.columns..(i + 1) * self.columns].to_vec(),
            None => vec![0.0; self.columns],
        }
    }
}

#[async_trait::async_trait]
impl Finder for FinderService {
    async fn discover(&self, request: &FinderRequest) -> Result<Discovery, SeamError> {
        let config = self.config.with_overrides(&request.overrides)?;
        let seeds_started = Instant::now();
        let seeder = Seeder {
            store: &self.store,
            embedder: self.embedder.as_ref(),
            clusters: &self.clusters,
            config: &config,
        };
        let seeds = seeder.seeds(&request.text).await?;
        let mut trace = trace_of(&seeds, millis(seeds_started.elapsed()));
        if seeds.fused.is_empty() {
            return Ok(Discovery {
                ranked: Vec::new(),
                trace,
            });
        }
        let graph_started = Instant::now();
        let walked = self.walk(&config, &seeds, request.explain).await?;
        trace.graph_ms = millis(graph_started.elapsed());
        trace.hubs_excluded = self.excluded_hubs(&walked, request.explain).await?;
        let final_scores: HashMap<i64, f64> = walked
            .graph
            .ids()
            .filter(|id| walked.boost(*id) > 0.0 || seeds.fused.contains_key(id))
            .map(|id| {
                (
                    id,
                    walked.boost(id) + seeds.fused.get(&id).copied().unwrap_or(0.0),
                )
            })
            .collect();

        let rollup_started = Instant::now();
        let rollup = self
            .rollup(&config, request, final_scores, &seeds, &walked)
            .await?;
        trace.rollup_ms = millis(rollup_started.elapsed());
        trace.relations = count_u32(walked.graph.len());
        trace.candidate_sources = rollup.candidate_sources;
        trace.filtered_sources = rollup.filtered_sources;
        trace.evidence = rollup.evidence;
        tracing::info!(
            results = rollup.ranked.len(),
            elapsed_ms = trace.seeds_ms + trace.graph_ms + trace.rollup_ms,
            "finder query completed"
        );
        Ok(Discovery {
            ranked: rollup.ranked,
            trace,
        })
    }

    async fn expand(&self, source: &StoredSource) -> Result<Expansion, SeamError> {
        let fragments = self.store.fragments_of(source.id).await?;
        let ids: Vec<FragmentId> = fragments.iter().map(|f| f.id).collect();
        let relations = self.store.relations_touching(&ids).await?;
        let known: HashSet<i64> = ids.iter().map(|f| f.0).collect();
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

fn trace_of(seeds: &Seeds, seeds_ms: u64) -> QueryTrace {
    QueryTrace {
        seeds_ms,
        fts_hits: seeds.hits(SeedChannel::Prose),
        lexical_hits: seeds.hits(SeedChannel::Lexical),
        vector_hits: seeds.hits(SeedChannel::Vector),
        exact_hits: seeds.hits(SeedChannel::Exact),
        cluster_hits: seeds.hits(SeedChannel::Cluster),
        clusters_matched: seeds.clusters_matched,
        grounding_ms: seeds.grounding_ms,
        seeds: count_u32(seeds.fused.len()),
        ..QueryTrace::default()
    }
}

impl FinderService {
    /// Load the seed-local slice under the hub bound, weigh its edges, and
    /// run the walk — one restart column for the fused seeds, or one per
    /// channel under `explain`, which sum to the same walk.
    async fn walk(
        &self,
        config: &FinderConfig,
        seeds: &Seeds,
        explain: bool,
    ) -> Result<Walked, SeamError> {
        let seed_ids: Vec<FragmentId> = seeds.fused.keys().map(|id| FragmentId(*id)).collect();
        let mut neighborhood = self
            .store
            .relations_near_bounded(
                &seed_ids,
                config.graph_hops,
                config.graph_relation_limit,
                config.hub_bound(),
            )
            .await?;
        let grounded = self.ground_rows(config, seeds).await?;
        neighborhood.relations.extend(grounded.relations);
        neighborhood.hubs.extend(grounded.hubs);
        let mut vertex_ids: Vec<FragmentId> = neighborhood
            .relations
            .iter()
            .flat_map(|r| [r.from, r.to])
            .chain(seed_ids.iter().copied())
            .chain(neighborhood.hubs.iter().map(|(id, _)| *id))
            .collect();
        vertex_ids.sort();
        vertex_ids.dedup();
        let row_kinds = if config.weights.row_kinds_matter() || explain {
            self.store.row_kinds_of(&vertex_ids).await?
        } else {
            HashMap::new()
        };
        let graph = Graph::build(
            &neighborhood.relations,
            &config.weights,
            (!row_kinds.is_empty()).then_some(&row_kinds),
            seeds.fused.keys().copied(),
        );
        let columns = if explain { SeedChannel::ALL.len() } else { 1 };
        let restarts = restarts_of(&graph, seeds, columns);
        let mass = graph.walk(
            &restarts,
            columns,
            config.damping,
            config.iterations,
            config.epsilon,
        );
        let total = sum_columns(&mass, columns);
        Ok(Walked {
            graph,
            columns,
            mass,
            total,
            row_kinds,
            hubs: neighborhood.hubs,
        })
    }

    /// The edges of the mined rows among the seeds, read from the full-text
    /// index: a mined row stores no anchors (`design/vocabulary.md`,
    /// storage), so its `mentions` edges are its spelling's postings,
    /// synthesized here for the walk. A row past the hub bound is a hub
    /// like any other, by its document frequency. Best seeds first, at
    /// most `grounded_rows_max` rows, never past the relation limit.
    async fn ground_rows(
        &self,
        config: &FinderConfig,
        seeds: &Seeds,
    ) -> Result<Grounded, SeamError> {
        let mut grounded = Grounded::default();
        if config.grounded_rows_max == 0 || seeds.fused.is_empty() {
            return Ok(grounded);
        }
        let ids: Vec<FragmentId> = seeds.fused.keys().map(|id| FragmentId(*id)).collect();
        let mut rows: Vec<VocabularyRow> = self
            .store
            .vocabulary_rows_of(&ids)
            .await?
            .into_iter()
            .filter(|row| row.origin == VocabularyOrigin::Mined)
            .collect();
        rows.sort_by(|a, b| {
            let score =
                |row: &VocabularyRow| seeds.fused.get(&row.fragment.0).copied().unwrap_or(0.0);
            score(b)
                .total_cmp(&score(a))
                .then(a.fragment.cmp(&b.fragment))
        });
        rows.truncate(usize::try_from(config.grounded_rows_max).unwrap_or(usize::MAX));
        let mentions = RelationKind::new("mentions").expect("literal kind is valid");
        let relation_limit = usize::try_from(config.graph_relation_limit).unwrap_or(usize::MAX);
        for row in rows {
            if let Some(bound) = config.hub_bound()
                && row.document_frequency > bound
            {
                grounded.hubs.push((row.fragment, row.document_frequency));
                continue;
            }
            let remaining = relation_limit.saturating_sub(grounded.relations.len());
            if remaining == 0 {
                break;
            }
            let limit = config
                .hub_bound()
                .map_or(config.graph_relation_limit, |bound| bound.saturating_add(1))
                .min(u32::try_from(remaining).unwrap_or(u32::MAX))
                .max(1);
            let hits = self.store.fragments_spelling(&row.spelling, limit).await?;
            for hit in hits {
                grounded
                    .relations
                    .push(Relation::new(hit, mentions.clone(), row.fragment));
            }
        }
        Ok(grounded)
    }

    /// The hubs the bound kept out, named under `explain`.
    async fn excluded_hubs(
        &self,
        walked: &Walked,
        explain: bool,
    ) -> Result<Vec<ExcludedHub>, SeamError> {
        let mut hubs: Vec<ExcludedHub> = walked
            .hubs
            .iter()
            .map(|(id, degree)| ExcludedHub {
                fragment: *id,
                degree: *degree,
                kind: walked.row_kinds.get(id).copied(),
                text: None,
            })
            .collect();
        if explain && !hubs.is_empty() {
            let ids: Vec<FragmentId> = hubs.iter().map(|h| h.fragment).collect();
            let texts: HashMap<FragmentId, String> = self
                .store
                .fragments(&ids)
                .await?
                .into_iter()
                .filter_map(|f| f.text.map(|t| (f.id, preview(&t))))
                .collect();
            for hub in &mut hubs {
                hub.text = texts.get(&hub.fragment).cloned();
            }
        }
        Ok(hubs)
    }
}

/// The restart matrix: the fused seed distribution in one column, or each
/// channel's share of it in its own column, vertex-major.
fn restarts_of(graph: &Graph, seeds: &Seeds, columns: usize) -> Vec<f64> {
    let total = seeds.total();
    assert!(total > 0.0);
    let mut restarts = vec![0.0; graph.len() * columns];
    for (id, fused) in &seeds.fused {
        let Some(i) = graph.index_of(*id) else {
            continue;
        };
        if columns == 1 {
            restarts[i] = fused / total;
            continue;
        }
        let shares = seeds.by_channel.get(id).copied().unwrap_or([0.0; 5]);
        for (c, share) in shares.iter().enumerate() {
            restarts[i * columns + c] = share / total;
        }
    }
    restarts
}

/// Ranked sources plus what the rollup saw before the limit cut.
struct Rollup {
    evidence: Vec<SourceEvidence>,
    ranked: Vec<RankedSource>,
    candidate_sources: u32,
    filtered_sources: u32,
}

/// Wall-clock milliseconds for a trace field; a phase that runs longer than
/// `u64::MAX` ms is not a real phase.
fn millis(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// A collection length as a trace count. Counts are bounded by `seed_k`,
/// `graph_relation_limit`, and the catalog size, all far below `u32::MAX`.
fn count_u32(length: usize) -> u32 {
    u32::try_from(length).unwrap_or(u32::MAX)
}

fn preview(text: &str) -> String {
    inseam_seams::text::truncate_chars(
        &inseam_seams::text::collapse_ws(text),
        LEDGER_TEXT_CHARS_MAX,
    )
}

/// A fragment id with its final (seed + activated) score.
type ScoredFragment = (FragmentId, f64);

impl FinderService {
    /// Group fragment scores by source, filter, aggregate, and dress results
    /// with envelope, summary, hints, and evidence.
    async fn rollup(
        &self,
        config: &FinderConfig,
        request: &FinderRequest,
        final_scores: HashMap<i64, f64>,
        seeds: &Seeds,
        walked: &Walked,
    ) -> Result<Rollup, SeamError> {
        let ids: Vec<FragmentId> = final_scores.keys().map(|id| FragmentId(*id)).collect();
        let owners = self.store.sources_of_fragments(&ids).await?;
        let mut per_source: HashMap<SourceId, Vec<ScoredFragment>> = HashMap::new();
        for (fid, sid) in owners {
            let score = final_scores[&fid.0];
            per_source.entry(sid).or_default().push((fid, score));
        }
        let before_filter = per_source.len();
        let per_source = self.apply_filters(&request.filters, per_source).await?;
        let filtered_sources = count_u32(before_filter - per_source.len());

        let mut ranked: Vec<(SourceId, f64, Vec<ScoredFragment>)> = per_source
            .into_iter()
            .map(|(sid, mut frags)| {
                frags.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                (sid, source_score(&frags), frags)
            })
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.0.cmp(&b.0.0)));
        let candidate_sources = count_u32(ranked.len());
        ranked.truncate(request.limit);
        let top = ranked.first().map(|(_, s, _)| *s).unwrap_or(1.0);
        let norm = if top > 0.0 { top } else { 1.0 };

        let mut out = Vec::with_capacity(ranked.len());
        let mut evidence = Vec::with_capacity(ranked.len());
        for (sid, score, frags) in ranked {
            let Some(source) = self.store.source(sid).await? else {
                continue;
            };
            let ledger = if request.explain {
                Some(self.ledger(config, &frags, seeds, walked).await?)
            } else {
                None
            };
            evidence.push(rollup_evidence(
                &source, score, norm, &frags, seeds, walked, ledger,
            ));
            let summary = self.store.summary_of(sid).await?;
            let hints = self.rollup_hints(config, &frags, norm).await?;
            out.push(RankedSource {
                source,
                score: score / norm,
                summary,
                hints,
                replicas: Vec::new(),
            });
        }
        let ranked = collapse_by_digest(out);
        evidence.retain(|item| ranked.iter().any(|r| r.source.address == item.address));
        Ok(Rollup {
            evidence,
            ranked,
            candidate_sources,
            filtered_sources,
        })
    }

    /// Drop the candidate sources a request's filters exclude, before the
    /// rollup — the way boundary properties are applied
    /// (`design/vocabulary.md`, facets).
    async fn apply_filters(
        &self,
        filters: &QueryFilters,
        per_source: HashMap<SourceId, Vec<ScoredFragment>>,
    ) -> Result<HashMap<SourceId, Vec<ScoredFragment>>, SeamError> {
        if filters.is_empty() {
            return Ok(per_source);
        }
        let faceted = self.sources_with_facets(&filters.facets).await?;
        let mut kept = HashMap::with_capacity(per_source.len());
        for (sid, frags) in per_source {
            if let Some(allowed) = &faceted
                && !allowed.contains(&sid)
            {
                continue;
            }
            let Some(source) = self.store.source(sid).await? else {
                continue;
            };
            if envelope_passes(filters, &source) {
                kept.insert(sid, frags);
            }
        }
        Ok(kept)
    }

    /// The sources anchored to every named facet value (an intersection),
    /// or `None` when no facet was asked for.
    async fn sources_with_facets(
        &self,
        facets: &[String],
    ) -> Result<Option<HashSet<SourceId>>, SeamError> {
        if facets.is_empty() {
            return Ok(None);
        }
        let mut allowed: Option<HashSet<SourceId>> = None;
        for facet in facets {
            let normalized = inseam_kernel::store::normalize_spelling(facet);
            let rows = self.store.vocabulary_rows_spelled(&normalized).await?;
            let mut sources: HashSet<SourceId> = HashSet::new();
            for row in rows
                .iter()
                .filter(|r| matches!(r.kind, VocabularyKind::Facet | VocabularyKind::Entity))
            {
                sources.extend(
                    self.store
                        .sources_anchored_to(row.fragment, FACET_SOURCES_MAX)
                        .await?,
                );
            }
            allowed = Some(match allowed {
                None => sources,
                Some(previous) => previous.intersection(&sources).copied().collect(),
            });
        }
        Ok(allowed)
    }

    async fn rollup_hints(
        &self,
        config: &FinderConfig,
        fragments: &[ScoredFragment],
        normalization: f64,
    ) -> Result<Vec<RankedFragment>, SeamError> {
        assert!(normalization > 0.0);
        let mut hints = Vec::new();
        for (fid, fscore) in fragments {
            if hints.len() >= config.max_hints {
                break;
            }
            let Some(fragment) = self.store.fragment(*fid).await? else {
                continue;
            };
            // Summaries ride along separately, keywords have no place
            // to scan to, and text-less roots hint nothing.
            if fragment.mimetype.is_summary() || fragment.mimetype.is_keywords() {
                continue;
            }
            if fragment.text.is_none() {
                continue;
            }
            hints.push(RankedFragment {
                fragment,
                score: fscore / normalization,
            });
        }
        Ok(hints)
    }

    /// The exact decomposition of one source's raw score over its scoring
    /// fragments: per channel (seed and the walk it induced), per row kind
    /// the walk arrived through, and the neighbours that carried the most.
    async fn ledger(
        &self,
        config: &FinderConfig,
        fragments: &[ScoredFragment],
        seeds: &Seeds,
        walked: &Walked,
    ) -> Result<Ledger, SeamError> {
        assert_eq!(walked.columns, SeedChannel::ALL.len());
        let mut channel_seed = [0.0; 5];
        let mut channel_walk = [0.0; 5];
        let mut by_kind = std::collections::BTreeMap::new();
        let mut carriers: HashMap<i64, f64> = HashMap::new();
        for ((id, _), weight) in fragments.iter().zip(SOURCE_SCORE_WEIGHTS) {
            let shares = seeds.by_channel.get(&id.0).copied().unwrap_or([0.0; 5]);
            for (c, share) in shares.iter().enumerate() {
                channel_seed[c] += weight * share;
            }
            for (c, mass) in walked.boost_by_column(id.0).iter().enumerate() {
                channel_walk[c] += weight * mass;
            }
            let Some(vertex) = walked.graph.index_of(id.0) else {
                continue;
            };
            for (kind, mass) in walked.graph.arrivals_by_kind(
                &walked.total,
                vertex,
                config.damping,
                &walked.row_kinds,
            ) {
                *by_kind.entry(kind).or_insert(0.0) += weight * mass;
            }
            for (neighbour, mass) in walked.graph.arrivals(&walked.total, vertex, config.damping) {
                *carriers.entry(neighbour).or_insert(0.0) += weight * mass;
            }
        }
        let channels = SeedChannel::ALL
            .iter()
            .filter(|c| channel_seed[c.index()] > 0.0 || channel_walk[c.index()] > 0.0)
            .map(|c| ChannelLine {
                channel: *c,
                seed: channel_seed[c.index()],
                walk: channel_walk[c.index()],
            })
            .collect();
        let rows = self
            .carrying_rows(carriers, walked, config.explain_rows_max)
            .await?;
        Ok(Ledger {
            channels,
            walk_by_row_kind: by_kind,
            rows,
        })
    }

    /// The best carriers, named: text, row kind, document frequency and
    /// cluster when they are vocabulary rows.
    async fn carrying_rows(
        &self,
        carriers: HashMap<i64, f64>,
        walked: &Walked,
        limit: u32,
    ) -> Result<Vec<CarryingRow>, SeamError> {
        let mut best: Vec<(i64, f64)> = carriers.into_iter().collect();
        best.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        best.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let ids: Vec<FragmentId> = best.iter().map(|(id, _)| FragmentId(*id)).collect();
        let texts: HashMap<FragmentId, String> = self
            .store
            .fragments(&ids)
            .await?
            .into_iter()
            .map(|f| (f.id, f.text.as_deref().map(preview).unwrap_or_default()))
            .collect();
        let vocabulary: HashMap<FragmentId, (u32, Option<inseam_kernel::store::ClusterId>)> = self
            .store
            .vocabulary_rows_of(&ids)
            .await?
            .into_iter()
            .map(|row| (row.fragment, (row.document_frequency, row.cluster)))
            .collect();
        Ok(best
            .into_iter()
            .map(|(id, mass)| {
                let fragment = FragmentId(id);
                let (document_frequency, cluster) = match vocabulary.get(&fragment) {
                    Some((frequency, cluster)) => (Some(*frequency), *cluster),
                    None => (None, None),
                };
                CarryingRow {
                    fragment,
                    kind: walked
                        .row_kinds
                        .get(&fragment)
                        .copied()
                        .unwrap_or(RowKind::Other),
                    text: texts.get(&fragment).cloned().unwrap_or_default(),
                    document_frequency,
                    cluster,
                    mass,
                }
            })
            .collect())
    }
}

/// Whether a source's envelope passes the request's envelope filters.
fn envelope_passes(filters: &QueryFilters, source: &StoredSource) -> bool {
    if let Some(host) = &filters.host
        && source.address.host.as_str() != host
    {
        return false;
    }
    if let Some(source_type) = &filters.source_type
        && source.envelope.source_type != *source_type
    {
        return false;
    }
    let modified = source.envelope.modified;
    if let Some(after) = filters.modified_after
        && modified.is_none_or(|m| m < after)
    {
        return false;
    }
    if let Some(before) = filters.modified_before
        && modified.is_none_or(|m| m > before)
    {
        return false;
    }
    true
}

/// Preserve the actual rollup inputs so clients never reconstruct scores
/// from hints.
fn rollup_evidence(
    source: &StoredSource,
    score_raw: f64,
    normalization: f64,
    fragments: &[ScoredFragment],
    seeds: &Seeds,
    walked: &Walked,
    ledger: Option<Ledger>,
) -> SourceEvidence {
    assert!(normalization > 0.0);
    let fragments = fragments
        .iter()
        .take(3)
        .zip(SOURCE_SCORE_WEIGHTS)
        .map(|((id, _), weight)| FragmentEvidence {
            fragment: *id,
            prose_rank: seeds.rank_in(SeedChannel::Prose, id.0),
            lexical_rank: seeds.rank_in(SeedChannel::Lexical, id.0),
            vector_rank: seeds.rank_in(SeedChannel::Vector, id.0),
            exact_rank: seeds.rank_in(SeedChannel::Exact, id.0),
            cluster_rank: seeds.rank_in(SeedChannel::Cluster, id.0),
            seed: seeds.fused.get(&id.0).copied().unwrap_or(0.0),
            graph: walked.boost(id.0),
            weight,
        })
        .collect();
    SourceEvidence {
        address: source.address.clone(),
        score_raw,
        normalization,
        fragments,
        ledger,
    }
}

/// Merge collapses by content digest (`design/finder.md`): results whose
/// envelopes carry equal digests are one logical result — the same file
/// living on two hosts ranks once, not twice. The best-scoring copy (first,
/// since `ranked` arrives best-first) supplies score, summary, and hints;
/// the other copies become its replicas. Results without a digest never
/// collapse: best-effort dedup degrades to duplication, never a wrong merge.
fn collapse_by_digest(ranked: Vec<RankedSource>) -> Vec<RankedSource> {
    collapse_by_digest_by(
        ranked,
        |result| result.source.envelope.content_digest,
        |kept, duplicate| kept.replicas.push(duplicate.source.address),
    )
}

/// The digest collapse over any best-first list: `digest_of` names each
/// item's merge key, and `absorb` folds a later item into the earlier one
/// carrying the same digest. One function serves the finder's ranked
/// sources and the operations layer's cross-node merge, so the two can
/// never disagree about what "the same content" means.
pub(crate) fn collapse_by_digest_by<T>(
    ranked: Vec<T>,
    digest_of: impl Fn(&T) -> Option<ContentDigest>,
    mut absorb: impl FnMut(&mut T, T),
) -> Vec<T> {
    let incoming = ranked.len();
    let mut collapsed: Vec<T> = Vec::with_capacity(incoming);
    let mut index_by_digest: HashMap<ContentDigest, usize> = HashMap::new();
    for result in ranked {
        let Some(digest) = digest_of(&result) else {
            collapsed.push(result);
            continue;
        };
        match index_by_digest.get(&digest) {
            Some(&index) => absorb(&mut collapsed[index], result),
            None => {
                index_by_digest.insert(digest, collapsed.len());
                collapsed.push(result);
            }
        }
    }
    assert!(collapsed.len() <= incoming, "a collapse never adds results");
    collapsed
}

/// Max plus a tapered bonus for additional independent hits: sum invites
/// long-document bias, max alone ignores corroboration (`design/finder.md`).
fn source_score(sorted: &[ScoredFragment]) -> f64 {
    let mut score = 0.0;
    for ((_, fragment_score), weight) in sorted.iter().zip(SOURCE_SCORE_WEIGHTS) {
        score += weight * fragment_score;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::address::{Address, ContentLength, Envelope, Timestamp};
    use inseam_kernel::fragment::Mimetype;

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

    fn source(host: &str, source_type: &str, modified: Option<i64>) -> StoredSource {
        StoredSource {
            id: SourceId(1),
            address: format!("inseam://{host}/a/b")
                .parse::<Address>()
                .expect("valid"),
            envelope: Envelope {
                source_type: source_type.into(),
                content_type: Mimetype::text_plain(),
                length: ContentLength::Bytes(1),
                created: None,
                modified: modified.map(Timestamp),
                observed: Timestamp(0),
                properties: Vec::new(),
                facets: Vec::new(),
                hint: None,
                content_digest: None,
            },
            root_fragment: None,
            origin: None,
        }
    }

    #[test]
    fn envelope_filters_narrow_by_host_type_and_time() {
        let filters = QueryFilters {
            host: Some("fs-a".into()),
            source_type: Some("email".into()),
            facets: Vec::new(),
            modified_after: Some(Timestamp(10)),
            modified_before: Some(Timestamp(20)),
        };
        assert!(envelope_passes(
            &filters,
            &source("fs-a", "email", Some(15))
        ));
        assert!(!envelope_passes(
            &filters,
            &source("fs-b", "email", Some(15))
        ));
        assert!(!envelope_passes(
            &filters,
            &source("fs-a", "file", Some(15))
        ));
        assert!(!envelope_passes(
            &filters,
            &source("fs-a", "email", Some(5))
        ));
        assert!(!envelope_passes(
            &filters,
            &source("fs-a", "email", Some(25))
        ));
        assert!(!envelope_passes(&filters, &source("fs-a", "email", None)));
        assert!(envelope_passes(
            &QueryFilters::default(),
            &source("x", "y", None)
        ));
    }
}
