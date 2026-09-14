//! The vocabulary pass (`design/vocabulary.md`): a sweep phase that runs
//! after every file has landed and before folders, reads the store and
//! never a host, and does what needs the whole corpus in view — mine the
//! candidates the statistics name, match them across all the text, cluster
//! the rows by co-occurrence, embed the clusters, and ground the changed
//! ones with the model once each. Per-source planners stay pure functions
//! of their text; this is the corpus-level step.
//!
//! The pass writes rows and clusters, never an edge per match: a matched
//! row's anchors are the full-text index's postings for its spelling, read
//! back at query time, and its document frequency and its cluster are
//! settled in memory from one walk over the text. Two walks over the
//! corpus, a few hundred thousand row writes, and no relation table growth
//! — that is the whole storage and time budget of the pass before the
//! model is asked anything (`design/vocabulary.md`, storage).

mod cluster;
mod facets;
mod ground;
mod mine;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use inseam_kernel::fragment::{FragmentId, FragmentKey, Mimetype, NewFragment, RelationKind};
use inseam_kernel::store::{
    ClusterId, IndexStore, NewVocabularyRow, SearchRole, SearchRow, SourceId, StoredCluster,
    VOCABULARY_LIST_MAX, VocabularyKind, VocabularyOrigin, VocabularyRow, normalize_spelling,
};
use inseam_seams::SeamError;
use inseam_seams::embedder::Embedder;
use inseam_seams::llm::LlmLane;
use inseam_seams::sweep::VocabularyReport;

use super::grant::Grantor;
use cluster::Clustering;
use mine::{Candidates, Matcher, Shape, keyword_phrases, tokenize};

/// The meter the cluster grounding calls are charged to.
pub const LLM_CONSUMER: &str = "vocabulary";
/// Content fragments read per store page.
const TEXT_PAGE_ROWS: u32 = 2_000;
/// Pages one scan may read: bounds the loop at two hundred million rows.
const PAGES_MAX: u32 = 100_000;
/// Rows planted, frequencies written, or clusters founded per transaction.
const WRITE_BATCH: usize = 2_000;
/// Cluster texts embedded per request.
const EMBED_BATCH: usize = 64;
/// Sources remembered per row for the cluster decision: a sample past
/// this, and a row past the hub bound says nothing about one topic anyway.
const CLUSTER_SOURCES_MAX: usize = 256;
/// Grounding calls in flight at once: the model's latency, not the pass's
/// bookkeeping, is what the grounding step waits on.
const GROUND_CONCURRENCY: usize = 8;
/// Excerpts handed to the model per cluster, one per member.
const EXCERPT_MEMBERS: usize = 3;
/// Hubs the report names.
const HUBS_REPORTED: u32 = 20;

/// The pass's dials, under `[sweep.vocabulary]`. The mining dials ride the
/// pass's own digest — changing one re-runs the pass in full, never a
/// per-source transform (`design/vocabulary.md`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VocabularyConfig {
    pub enabled: bool,
    /// A candidate must recur in at least this many sources.
    pub term_df_min: u32,
    /// ... and in at most this percentage of the content sources.
    pub term_df_max_percent: u32,
    /// The percentage is never taken below this count, so a small corpus
    /// still has a band.
    pub term_df_max_floor: u32,
    /// Distinct candidate spellings the mining table keeps (tokens that
    /// recurred with a shape mark, and derived phrases).
    pub candidates_max: u32,
    /// Distinct recurring tokens the mining table counts, marked or not:
    /// about sixteen bytes each.
    pub recurring_tokens_max: u32,
    pub clusters_max: u32,
    pub cluster_terms_max: u32,
    /// The fraction of a row's sources a cluster must be present in for
    /// the row to join it.
    pub cluster_join_min: f64,
    /// Two clusters at or over this cosine merge, smaller into larger.
    pub cluster_merge_cosine: f64,
    /// Rows assigned to clusters per pass; the rest wait.
    pub cluster_assignments_per_sweep_max: u32,
    /// Grounding calls per run; `0` grounds nothing.
    pub cluster_llm_budget: usize,
    pub llm_lane: LlmLane,
    pub aliases_per_row_max: u32,
}

impl Default for VocabularyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            term_df_min: 2,
            term_df_max_percent: 2,
            term_df_max_floor: 50,
            candidates_max: 200_000,
            recurring_tokens_max: 8_000_000,
            clusters_max: 10_000,
            cluster_terms_max: 64,
            cluster_join_min: 0.5,
            cluster_merge_cosine: 0.92,
            cluster_assignments_per_sweep_max: 200_000,
            cluster_llm_budget: 500,
            llm_lane: LlmLane::Interactive,
            aliases_per_row_max: 4,
        }
    }
}

impl VocabularyConfig {
    /// The dials whose change re-runs the pass in full. The version
    /// prefix moves when the pass's own shape does, so an index built by
    /// an earlier pass is re-mined on its next sweep.
    pub fn digest(&self) -> String {
        format!(
            "vocabulary-v2|df_min={}|df_max_percent={}|df_max_floor={}|candidates={}|recurring={}|phrase_tokens={}",
            self.term_df_min,
            self.term_df_max_percent,
            self.term_df_max_floor,
            self.candidates_max,
            self.recurring_tokens_max,
            mine::PHRASE_TOKENS_MAX
        )
    }

    /// The top of the band for a corpus of `sources` content sources.
    pub fn df_max(&self, sources: u64) -> u32 {
        let percent = sources
            .saturating_mul(u64::from(self.term_df_max_percent))
            .checked_div(100)
            .unwrap_or(0);
        let percent = u32::try_from(percent).unwrap_or(u32::MAX);
        percent.max(self.term_df_max_floor).max(self.term_df_min)
    }
}

/// One pass over the store.
pub(super) struct VocabularyPass<'a> {
    pub store: &'a IndexStore,
    pub embedder: &'a dyn Embedder,
    pub grantor: &'a Arc<Grantor>,
    pub config: &'a VocabularyConfig,
}

/// A row the store holds, as the pass needs it before matching.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExistingRow {
    fragment: FragmentId,
    kind: VocabularyKind,
    origin: VocabularyOrigin,
    cluster: Option<ClusterId>,
}

/// One spelling the matching walk counts: a candidate to plant, or a row
/// the store has whose frequency and sources are measured again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MatchedRow {
    normalized: String,
    spelling: String,
    shape: Shape,
    existing: Option<ExistingRow>,
    /// Whether this spelling is a band candidate (mined or derived) rather
    /// than only an existing row of another origin.
    candidate: bool,
    documents: u32,
    sources: Vec<SourceId>,
    /// Filled once the row is planted or known.
    fragment: Option<FragmentId>,
}

impl MatchedRow {
    fn kind(&self) -> VocabularyKind {
        match &self.existing {
            Some(existing) => existing.kind,
            None => match self.shape {
                Shape::Identifier => VocabularyKind::Identifier,
                Shape::Term | Shape::None => VocabularyKind::Term,
            },
        }
    }

    fn clusterable(&self) -> bool {
        matches!(
            self.kind(),
            VocabularyKind::Term | VocabularyKind::Identifier | VocabularyKind::Entity
        )
    }
}

impl VocabularyPass<'_> {
    /// Run the pass. `changed` says whether this run landed or removed any
    /// source; a run that changed nothing under an unchanged configuration
    /// has nothing to mine.
    pub async fn run(&self, changed: bool) -> Result<VocabularyReport, SeamError> {
        let mut report = VocabularyReport::default();
        if let Some(reason) = self.skip_reason(changed).await? {
            report.skipped = Some(reason);
            return Ok(report);
        }
        let generation = self.store.vocabulary_generation().await?.saturating_add(1);
        let dropped = self.store.drop_mined_anchors().await?;
        if dropped > 0 {
            tracing::info!(dropped, "dropped stored anchors of mined rows");
        }
        report.rows_adopted =
            usize::try_from(self.store.adopt_keyed_fragments().await?).unwrap_or(usize::MAX);

        let started = Instant::now();
        let candidates = self.mine(&mut report).await?;
        report.mine_ms = millis(started.elapsed());

        let started = Instant::now();
        let mut rows = self.table(candidates, &mut report).await?;
        self.match_all(&mut rows, &mut report).await?;
        self.plant_all(&mut rows, &mut report).await?;
        let planted = facets::plant_facets(self.store).await?;
        report.facets_planted = planted.rows_created;
        report.facet_anchors = planted.anchors_written;
        report.anchors_added += planted.anchors_written;
        self.store.recount_envelope_document_frequencies().await?;
        report.match_ms = millis(started.elapsed());

        let started = Instant::now();
        self.cluster(&rows, generation, &mut report).await?;
        drop(rows);
        self.embed_clusters(generation, &mut report).await?;
        self.merge_clusters(generation, &mut report).await?;
        report.cluster_ms = millis(started.elapsed());

        let started = Instant::now();
        self.ground_clusters(generation, &mut report).await?;
        self.store.recount_clusters().await?;
        self.embed_clusters(generation, &mut report).await?;
        report.ground_ms = millis(started.elapsed());

        self.store.bump_vocabulary_generation().await?;
        self.store
            .set_vocabulary_config_digest(&self.config.digest())
            .await?;
        report.hubs = self.hubs().await?;
        Ok(report)
    }

    async fn skip_reason(&self, changed: bool) -> Result<Option<String>, SeamError> {
        if changed {
            return Ok(None);
        }
        let digest = self.store.vocabulary_config_digest().await?;
        let generation = self.store.vocabulary_generation().await?;
        if generation > 0 && digest.as_deref() == Some(self.config.digest().as_str()) {
            return Ok(Some("unchanged".to_string()));
        }
        Ok(None)
    }

    /// Scan every derived keyword row for phrase candidates and every
    /// content fragment for tokens, counting each once per source.
    async fn mine(&self, report: &mut VocabularyReport) -> Result<Candidates, SeamError> {
        let mut candidates = Candidates::new(
            usize::try_from(self.config.recurring_tokens_max).unwrap_or(usize::MAX),
            usize::try_from(self.config.candidates_max).unwrap_or(usize::MAX),
            self.config.term_df_min,
        );
        let mut after = FragmentId(0);
        for _ in 0..PAGES_MAX {
            let page = self
                .store
                .derived_texts(Mimetype::keywords().essence(), after, TEXT_PAGE_ROWS)
                .await?;
            for (_, text) in &page {
                for phrase in keyword_phrases(text) {
                    candidates.propose_phrase(phrase);
                }
            }
            match page.last() {
                Some((id, _)) if page.len() >= usize::try_from(TEXT_PAGE_ROWS).unwrap_or(0) => {
                    after = *id;
                }
                _ => break,
            }
        }
        let mut current: Option<SourceId> = None;
        let mut sources_walked: usize = 0;
        let mut after = FragmentId(0);
        for _ in 0..PAGES_MAX {
            let page = self.store.content_texts(after, TEXT_PAGE_ROWS).await?;
            for text in &page {
                // A source's fragments land in one transaction, so their ids
                // are contiguous and a source change is a new source.
                if current != Some(text.source) {
                    current = Some(text.source);
                    sources_walked += 1;
                    candidates.begin_source();
                }
                let tokens = tokenize(&text.text);
                candidates.count(&tokens);
                candidates.count_phrases(&tokens);
            }
            match page.last() {
                Some(last) if page.len() >= usize::try_from(TEXT_PAGE_ROWS).unwrap_or(0) => {
                    after = last.fragment;
                }
                _ => break,
            }
        }
        report.sources_walked = sources_walked;
        report.candidates_mined = candidates.len();
        if candidates.dropped_at_cap > 0 {
            tracing::warn!(
                dropped = candidates.dropped_at_cap,
                "vocabulary candidate table hit a cap; raise sweep.vocabulary.candidates_max or recurring_tokens_max"
            );
        }
        Ok(candidates)
    }

    /// The table the matching walk counts: the band's candidates plus
    /// every row the store has (facets aside), with mined rows the band no
    /// longer names retracted first.
    async fn table(
        &self,
        candidates: Candidates,
        report: &mut VocabularyReport,
    ) -> Result<Vec<MatchedRow>, SeamError> {
        let sources = self.store.content_source_count().await?;
        let df_max = self.config.df_max(sources);
        let band = candidates.in_band(self.config.term_df_min, df_max);
        report.candidates_kept = band.len();
        report.candidates_derived = band.iter().filter(|(_, t)| t.derived).count();
        let band_set: HashSet<&str> = band.iter().map(|(n, _)| n.as_str()).collect();

        let mut existing = self.existing_rows().await?;
        let retract: Vec<FragmentId> = existing
            .iter()
            .filter(|(normalized, row)| {
                row.origin == VocabularyOrigin::Mined && !band_set.contains(normalized.as_str())
            })
            .map(|(_, row)| row.fragment)
            .collect();
        self.store.retract_vocabulary_rows(&retract).await?;
        report.rows_retracted = retract.len();
        let retracted: HashSet<FragmentId> = retract.into_iter().collect();
        existing.retain(|_, row| !retracted.contains(&row.fragment));

        let mut rows: Vec<MatchedRow> = Vec::with_capacity(band.len() + existing.len());
        for (normalized, tally) in band {
            let existing = existing.remove(&normalized);
            rows.push(MatchedRow {
                fragment: existing.as_ref().map(|e| e.fragment),
                normalized,
                spelling: tally.spelling.clone(),
                shape: tally.shape,
                existing,
                candidate: true,
                documents: 0,
                sources: Vec::new(),
            });
        }
        for (normalized, row) in existing {
            rows.push(MatchedRow {
                fragment: Some(row.fragment),
                spelling: String::new(),
                normalized,
                shape: Shape::None,
                existing: Some(row),
                candidate: false,
                documents: 0,
                sources: Vec::new(),
            });
        }
        Ok(rows)
    }

    /// Every non-facet row's normalized spelling → what the store holds.
    async fn existing_rows(&self) -> Result<HashMap<String, ExistingRow>, SeamError> {
        let mut rows: HashMap<String, ExistingRow> = HashMap::new();
        let mut offset: u32 = 0;
        for _ in 0..PAGES_MAX {
            let page = self
                .store
                .vocabulary_rows_page(None, VOCABULARY_LIST_MAX, offset)
                .await?;
            let count = page.len();
            for row in page {
                if row.kind == VocabularyKind::Facet {
                    continue;
                }
                rows.entry(row.normalized).or_insert(ExistingRow {
                    fragment: row.fragment,
                    kind: row.kind,
                    origin: row.origin,
                    cluster: row.cluster,
                });
            }
            if count < usize::try_from(VOCABULARY_LIST_MAX).unwrap_or(0) {
                break;
            }
            offset = offset.saturating_add(VOCABULARY_LIST_MAX);
        }
        Ok(rows)
    }

    /// Walk every content fragment once more and count, for every row,
    /// the sources that spell it — once per source, with a bounded sample
    /// of those sources kept for the cluster decision.
    async fn match_all(
        &self,
        rows: &mut [MatchedRow],
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        let table: HashMap<String, usize> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.normalized.clone(), index))
            .collect();
        let matcher = Matcher::new(table);
        if matcher.is_empty() {
            return Ok(());
        }
        let mut current: Option<SourceId> = None;
        let mut seen_in_source: HashSet<usize> = HashSet::new();
        let mut matches: usize = 0;
        let mut after = FragmentId(0);
        for _ in 0..PAGES_MAX {
            let page = self.store.content_texts(after, TEXT_PAGE_ROWS).await?;
            for text in &page {
                if current != Some(text.source) {
                    current = Some(text.source);
                    seen_in_source.clear();
                }
                for index in matcher.matches(&tokenize(&text.text)) {
                    if !seen_in_source.insert(index) {
                        continue;
                    }
                    matches += 1;
                    let row = &mut rows[index];
                    row.documents = row.documents.saturating_add(1);
                    if row.sources.len() < CLUSTER_SOURCES_MAX {
                        row.sources.push(text.source);
                    }
                }
            }
            match page.last() {
                Some(last) if page.len() >= usize::try_from(TEXT_PAGE_ROWS).unwrap_or(0) => {
                    after = last.fragment;
                }
                _ => break,
            }
        }
        report.matches_counted = matches;
        Ok(())
    }

    /// Plant the candidates the walk confirmed in the band — a phrase
    /// counts only here, and a seen-filter false positive is a singleton
    /// the walk finds out — and record every known row's frequency.
    async fn plant_all(
        &self,
        rows: &mut [MatchedRow],
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        let sources = self.store.content_source_count().await?;
        let df_max = self.config.df_max(sources);
        let in_band =
            |row: &MatchedRow| row.documents >= self.config.term_df_min && row.documents <= df_max;
        let new_indexes: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.existing.is_none() && row.candidate && in_band(row))
            .map(|(index, _)| index)
            .collect();
        let surface = self.store.has_search_surface().await?;
        for batch in new_indexes.chunks(WRITE_BATCH) {
            let new_rows: Vec<NewVocabularyRow> =
                batch.iter().map(|index| mined_row(&rows[*index])).collect();
            let results = self.store.plant_vocabulary_rows(&new_rows).await?;
            let mut search_rows: Vec<SearchRow> = Vec::new();
            for ((index, row), result) in batch.iter().zip(&new_rows).zip(&results) {
                rows[*index].fragment = Some(result.fragment);
                if result.created {
                    report.rows_planted += 1;
                    if let Some(text) = &row.fragment.text {
                        search_rows.push(SearchRow {
                            fragment: result.fragment,
                            source: None,
                            text: text.clone(),
                            vector: None,
                            role: SearchRole::Lexical,
                        });
                    }
                }
            }
            if surface {
                self.store.add_search_rows(&search_rows).await?;
            }
        }
        let known: Vec<(FragmentId, u32)> = rows
            .iter()
            .filter(|row| row.existing.is_some())
            .filter_map(|row| row.fragment.map(|fragment| (fragment, row.documents)))
            .collect();
        for batch in known.chunks(WRITE_BATCH) {
            self.store.set_document_frequencies(batch).await?;
        }
        Ok(())
    }

    /// Decide every unclustered row's home in memory, most frequent first,
    /// then write the founded clusters and the joins in batches.
    async fn cluster(
        &self,
        rows: &[MatchedRow],
        generation: u64,
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        let mut clustering = Clustering::default();
        let mut stored_count: usize = 0;
        for cluster in self.store.clusters().await? {
            clustering.size(cluster.id, cluster.member_count);
            stored_count += 1;
        }
        let mut order: Vec<usize> = (0..rows.len())
            .filter(|index| rows[*index].fragment.is_some() && rows[*index].clusterable())
            .collect();
        order.sort_by(|a, b| {
            rows[*b]
                .documents
                .cmp(&rows[*a].documents)
                .then(rows[*a].normalized.cmp(&rows[*b].normalized))
        });
        for index in &order {
            if let Some(cluster) = rows[*index].existing.as_ref().and_then(|e| e.cluster) {
                clustering.place(cluster, &rows[*index].sources);
            }
        }
        let clusters_max = usize::try_from(self.config.clusters_max).unwrap_or(usize::MAX);
        let assignments_max =
            usize::try_from(self.config.cluster_assignments_per_sweep_max).unwrap_or(usize::MAX);
        let unclustered = order
            .iter()
            .filter(|index| {
                rows[**index]
                    .existing
                    .as_ref()
                    .is_none_or(|e| e.cluster.is_none())
            })
            .take(assignments_max);
        for index in unclustered {
            let row = &rows[*index];
            let label = if row.spelling.is_empty() {
                row.normalized.as_str()
            } else {
                row.spelling.as_str()
            };
            clustering.assign(
                *index,
                label,
                &row.sources,
                self.config.cluster_join_min,
                self.config.cluster_terms_max,
                clusters_max.max(stored_count),
            );
        }
        report.clusters_joined = clustering.joined;
        self.write_clusters(rows, clustering, generation, report)
            .await?;
        self.store.recount_clusters().await?;
        Ok(())
    }

    async fn write_clusters(
        &self,
        rows: &[MatchedRow],
        clustering: Clustering,
        generation: u64,
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        let fragment_of = |index: &usize| rows[*index].fragment;
        let founded: Vec<(String, Vec<FragmentId>)> = clustering
            .founded
            .into_iter()
            .map(|(label, members)| (label, members.iter().filter_map(fragment_of).collect()))
            .filter(|(_, members): &(String, Vec<FragmentId>)| !members.is_empty())
            .collect();
        for batch in founded.chunks(WRITE_BATCH) {
            let ids = self.store.found_clusters(batch, generation).await?;
            report.clusters_founded += ids.iter().filter(|id| id.is_some()).count();
        }
        for (cluster, members) in clustering.joins {
            let fragments: Vec<FragmentId> = members.iter().filter_map(fragment_of).collect();
            for batch in fragments.chunks(WRITE_BATCH) {
                self.store.join_cluster(cluster, batch, generation).await?;
            }
        }
        Ok(())
    }

    /// Embed every cluster changed this pass whose text moved, in batches.
    async fn embed_clusters(
        &self,
        generation: u64,
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        if self.embedder.dimensions().is_none() {
            return Ok(());
        }
        let changed = self.store.clusters_changed_since(generation).await?;
        let mut work: Vec<(ClusterId, String, String)> = Vec::new();
        for cluster in &changed {
            let members = self.store.cluster_members(cluster.id).await?;
            if members.is_empty() {
                continue;
            }
            let text = cluster::embed_text(&members);
            let digest = cluster::digest_of(&text);
            if digest == cluster.text_digest && cluster.vector.is_some() {
                continue;
            }
            work.push((cluster.id, digest, text));
        }
        for batch in work.chunks(EMBED_BATCH) {
            let texts: Vec<&str> = batch.iter().map(|(_, _, text)| text.as_str()).collect();
            let vectors = self.embedder.embed(&texts).await?;
            for ((id, digest, _), vector) in batch.iter().zip(vectors) {
                self.store.set_cluster_vector(*id, digest, &vector).await?;
                report.clusters_embedded += 1;
            }
        }
        Ok(())
    }

    /// Fold clusters whose vectors say they are one topic.
    async fn merge_clusters(
        &self,
        generation: u64,
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        let all = self.store.clusters().await?;
        let changed: Vec<StoredCluster> = all
            .iter()
            .filter(|c| c.changed_sweep >= generation)
            .cloned()
            .collect();
        let pairs = cluster::merges(&changed, &all, self.config.cluster_merge_cosine);
        for (survivor, loser) in pairs {
            self.store
                .merge_clusters(survivor, loser, generation)
                .await?;
            report.clusters_merged += 1;
        }
        if report.clusters_merged > 0 {
            self.store.recount_clusters().await?;
            self.embed_clusters(generation, report).await?;
        }
        Ok(())
    }

    /// One model call per changed cluster while the budget lasts, with
    /// [`GROUND_CONCURRENCY`] calls in flight; each answer is applied as it
    /// arrives, on this task, so the store sees one writer.
    async fn ground_clusters(
        &self,
        generation: u64,
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        let Some(llm) = self.grantor.grant_named(LLM_CONSUMER, self.config.llm_lane) else {
            return Ok(());
        };
        let changed = self.store.clusters_changed_since(generation).await?;
        let aliases_max = usize::try_from(self.config.aliases_per_row_max).unwrap_or(4);
        let llm = llm.as_ref();
        let mut answers = stream::iter(changed.into_iter().take(self.config.cluster_llm_budget))
            .map(|cluster| async move {
                let members = self.store.cluster_members(cluster.id).await?;
                if members.is_empty() {
                    return Ok::<_, SeamError>(None);
                }
                let excerpts = self.excerpts_for(&members).await?;
                let grounding = ground::ground(llm, &members, &excerpts, aliases_max).await;
                Ok(Some((members, grounding)))
            })
            .buffer_unordered(GROUND_CONCURRENCY);
        while let Some(answer) = answers.next().await {
            let Some((members, grounding)) = answer? else {
                continue;
            };
            let grounding = match grounding {
                Ok(grounding) => grounding,
                Err(SeamError::Refused(reason)) => {
                    tracing::info!("cluster grounding stopped: {reason}");
                    break;
                }
                Err(error) => {
                    tracing::warn!("cluster grounding failed, continuing: {error}");
                    continue;
                }
            };
            report.llm_calls += 1;
            report.clusters_grounded += 1;
            self.apply_grounding(&members, grounding, generation, report)
                .await?;
        }
        Ok(())
    }

    /// Short passages where a cluster's most frequent members appear: one
    /// content fragment per member from the full-text index, or from the
    /// row's relations when it is anchored that way.
    async fn excerpts_for(&self, members: &[VocabularyRow]) -> Result<Vec<String>, SeamError> {
        let mut excerpts = Vec::new();
        for member in members.iter().take(EXCERPT_MEMBERS) {
            let mut anchors = self.store.fragments_spelling(&member.spelling, 1).await?;
            if anchors.is_empty() {
                anchors = self
                    .store
                    .relations_touching(&[member.fragment])
                    .await?
                    .into_iter()
                    .filter(|r| r.to == member.fragment)
                    .map(|r| r.from)
                    .take(1)
                    .collect();
            }
            for fragment in self.store.fragments(&anchors).await? {
                if let Some(text) = fragment.text {
                    excerpts.push(text);
                }
            }
        }
        Ok(excerpts)
    }

    /// Apply what the model settled: merges demote a row to an alias,
    /// glosses are written once, aliases become rows related `aliases`
    /// into their word.
    async fn apply_grounding(
        &self,
        members: &[VocabularyRow],
        grounding: ground::Grounding,
        generation: u64,
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        let by_spelling: HashMap<&str, &VocabularyRow> =
            members.iter().map(|m| (m.normalized.as_str(), m)).collect();
        for (keep, drop) in &grounding.merges {
            if let (Some(survivor), Some(loser)) = (
                by_spelling.get(keep.as_str()),
                by_spelling.get(drop.as_str()),
            ) && survivor.fragment != loser.fragment
            {
                self.store
                    .merge_vocabulary_rows(survivor.fragment, loser.fragment)
                    .await?;
                report.rows_merged += 1;
                report.anchors_added += 1;
            }
        }
        for (term, gloss) in &grounding.glosses {
            if let Some(row) = by_spelling.get(term.as_str()) {
                self.store.set_vocabulary_gloss(row.fragment, gloss).await?;
                report.glosses_written += 1;
            }
        }
        let aliases_kind = RelationKind::new("aliases").expect("literal kind is valid");
        let surface = self.store.has_search_surface().await?;
        for (term, aliases) in &grounding.aliases {
            let Some(row) = by_spelling.get(term.as_str()) else {
                continue;
            };
            let new_rows: Vec<NewVocabularyRow> = aliases.iter().map(|a| alias_row(a)).collect();
            let planted = self.store.plant_vocabulary_rows(&new_rows).await?;
            let anchors: Vec<(FragmentId, FragmentId)> =
                planted.iter().map(|p| (p.fragment, row.fragment)).collect();
            self.store
                .anchor_vocabulary(&aliases_kind, &anchors)
                .await?;
            report.anchors_added += anchors.len();
            let search_rows: Vec<SearchRow> = new_rows
                .iter()
                .zip(&planted)
                .filter(|(_, p)| p.created)
                .filter_map(|(r, p)| {
                    r.fragment.text.as_ref().map(|text| SearchRow {
                        fragment: p.fragment,
                        source: None,
                        text: text.clone(),
                        vector: None,
                        role: SearchRole::Lexical,
                    })
                })
                .collect();
            if surface {
                self.store.add_search_rows(&search_rows).await?;
            }
            report.aliases_planted += planted.iter().filter(|p| p.created).count();
        }
        if let Some(cluster) = members.first().and_then(|m| m.cluster) {
            self.store.join_cluster(cluster, &[], generation).await?;
        }
        Ok(())
    }

    /// The highest-frequency rows, named: the first thing to read after a
    /// pass.
    async fn hubs(&self) -> Result<Vec<(String, u32)>, SeamError> {
        Ok(self
            .store
            .vocabulary_rows_page(None, HUBS_REPORTED, 0)
            .await?
            .into_iter()
            .map(|row| (row.spelling, row.document_frequency))
            .collect())
    }
}

/// A mined candidate as a row: identifiers under `identifier:`, terms
/// under `term:` — the namespaces the hints transform already uses, so a
/// term it extracted and a term the pass mined are one keyed fragment.
fn mined_row(row: &MatchedRow) -> NewVocabularyRow {
    assert!(row.existing.is_none());
    let (kind, prefix, mimetype) = match row.shape {
        Shape::Identifier => (
            VocabularyKind::Identifier,
            "identifier",
            "text/x-inseam-identifier",
        ),
        Shape::Term | Shape::None => (VocabularyKind::Term, "term", "text/x-inseam-term"),
    };
    NewVocabularyRow {
        key: FragmentKey::new(format!("{prefix}:{}", row.normalized))
            .expect("a bounded normalized spelling under a literal prefix is a valid key"),
        fragment: NewFragment {
            mimetype: Mimetype::parse(mimetype).expect("literal mimetype is valid"),
            text: Some(row.spelling.clone()),
            extent: None,
            content_address: None,
        },
        kind,
        origin: VocabularyOrigin::Mined,
        normalized: row.normalized.clone(),
        document_frequency: row.documents,
    }
}

fn alias_row(alias: &str) -> NewVocabularyRow {
    let normalized = normalize_spelling(alias);
    NewVocabularyRow {
        key: FragmentKey::new(format!("alias:{normalized}"))
            .expect("a bounded alias under a literal prefix is a valid key"),
        fragment: NewFragment {
            mimetype: Mimetype::parse("text/x-inseam-alias").expect("literal mimetype is valid"),
            text: Some(alias.to_string()),
            extent: None,
            content_address: None,
        },
        kind: VocabularyKind::Alias,
        origin: VocabularyOrigin::Grounded,
        normalized,
        document_frequency: 0,
    }
}

fn millis(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matched(normalized: &str, spelling: &str, shape: Shape, documents: u32) -> MatchedRow {
        MatchedRow {
            normalized: normalized.into(),
            spelling: spelling.into(),
            shape,
            existing: None,
            candidate: true,
            documents,
            sources: Vec::new(),
            fragment: None,
        }
    }

    #[test]
    fn the_band_top_is_a_percentage_with_a_floor() {
        let config = VocabularyConfig::default();
        assert_eq!(config.df_max(40), 50, "the floor holds on a small corpus");
        assert_eq!(config.df_max(25_000), 500);
        assert_eq!(config.df_max(512_000), 10_240);
        let strict = VocabularyConfig {
            term_df_max_floor: 1,
            term_df_min: 3,
            ..VocabularyConfig::default()
        };
        assert_eq!(strict.df_max(10), 3, "never below the band's bottom");
    }

    #[test]
    fn the_digest_names_only_the_mining_dials() {
        let a = VocabularyConfig::default().digest();
        let b = VocabularyConfig {
            cluster_llm_budget: 1,
            ..VocabularyConfig::default()
        }
        .digest();
        let c = VocabularyConfig {
            term_df_min: 3,
            ..VocabularyConfig::default()
        }
        .digest();
        assert_eq!(a, b, "a budget change never re-mines");
        assert_ne!(a, c);
    }

    #[test]
    fn mined_rows_take_the_extractors_namespaces() {
        let identifier = mined_row(&matched("sup-100432", "SUP-100432", Shape::Identifier, 3));
        assert_eq!(identifier.key.as_str(), "identifier:sup-100432");
        assert_eq!(identifier.kind, VocabularyKind::Identifier);
        assert_eq!(identifier.document_frequency, 3);
        let term = mined_row(&matched(
            "residency stamp",
            "residency stamp",
            Shape::Term,
            3,
        ));
        assert_eq!(term.key.as_str(), "term:residency stamp");
        assert_eq!(
            alias_row("The 80GB accelerator").key.as_str(),
            "alias:the 80gb accelerator"
        );
    }
}
