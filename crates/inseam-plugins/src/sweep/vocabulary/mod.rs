//! The vocabulary pass (`design/vocabulary.md`): a sweep phase that runs
//! after every file has landed and before folders, reads the store and
//! never a host, and does what needs the whole corpus in view — mine the
//! candidates the statistics name, match and anchor them across all the
//! text, cluster the rows by co-occurrence, embed the clusters, and ground
//! the changed ones with the model once each. Per-source planners stay
//! pure functions of their text; this is the corpus-level step.

mod cluster;
mod facets;
mod ground;
mod mine;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use inseam_kernel::address::HostId;
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
use cluster::{Assignment, Presence};
use mine::{Candidates, Matcher, Shape, keyword_phrases, tokenize};

/// The meter the cluster grounding calls are charged to.
pub const LLM_CONSUMER: &str = "vocabulary";
/// Content fragments read per store page.
const TEXT_PAGE_ROWS: u32 = 2_000;
/// Pages one scan may read: bounds the loop at two hundred million rows.
const PAGES_MAX: u32 = 100_000;
/// Rows planted, anchors written, or joins applied per transaction.
const WRITE_BATCH: usize = 2_000;
/// Cluster texts embedded per request.
const EMBED_BATCH: usize = 64;
/// Sources read per row when deciding its cluster: past this a row is a
/// hub whose sources say nothing about one topic.
const CLUSTER_SOURCES_MAX: u32 = 2_000;
/// Excerpts handed to the model per cluster, and anchors read per member.
const EXCERPT_MEMBERS: usize = 3;
const EXCERPTS_PER_MEMBER: usize = 1;
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
    /// Distinct candidates the mining table keeps (soft cap for plain
    /// words; shape-marked tokens may double it).
    pub candidates_max: u32,
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
            clusters_max: 10_000,
            cluster_terms_max: 64,
            cluster_join_min: 0.5,
            cluster_merge_cosine: 0.92,
            cluster_assignments_per_sweep_max: 50_000,
            cluster_llm_budget: 500,
            llm_lane: LlmLane::Interactive,
            aliases_per_row_max: 4,
        }
    }
}

impl VocabularyConfig {
    /// The dials whose change re-runs the pass in full.
    pub fn digest(&self) -> String {
        format!(
            "vocabulary-v1|df_min={}|df_max_percent={}|df_max_floor={}|candidates={}|phrase_tokens={}",
            self.term_df_min,
            self.term_df_max_percent,
            self.term_df_max_floor,
            self.candidates_max,
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
    /// The swept host and its kind (`filesystem`, `gmail`): the host facet
    /// every rooted source of the sweep is anchored to.
    pub host: &'a HostId,
    pub host_kind: &'a str,
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
        report.rows_adopted =
            usize::try_from(self.store.adopt_keyed_fragments().await?).unwrap_or(usize::MAX);

        let started = Instant::now();
        let candidates = self.mine(&mut report).await?;
        report.mine_ms = millis(started.elapsed());

        let started = Instant::now();
        self.plant_and_match(candidates, &mut report).await?;
        let planted = facets::plant_facets(self.store, self.host, self.host_kind).await?;
        report.facets_planted = planted.rows_created;
        report.facet_anchors = planted.anchors_written;
        self.store.recount_document_frequencies().await?;
        report.match_ms = millis(started.elapsed());

        let started = Instant::now();
        self.cluster(generation, &mut report).await?;
        self.embed_clusters(generation, &mut report).await?;
        self.merge_clusters(generation, &mut report).await?;
        report.cluster_ms = millis(started.elapsed());

        let started = Instant::now();
        self.ground_clusters(generation, &mut report).await?;
        self.store.recount_document_frequencies().await?;
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
        let mut candidates =
            Candidates::new(usize::try_from(self.config.candidates_max).unwrap_or(usize::MAX));
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
                "vocabulary candidate table hit its cap; raise sweep.vocabulary.candidates_max"
            );
        }
        Ok(candidates)
    }

    /// Keep the band, retract mined rows the band no longer names, plant
    /// the new ones, then anchor every row — new and existing — across
    /// all the text.
    async fn plant_and_match(
        &self,
        candidates: Candidates,
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        let sources = self.store.content_source_count().await?;
        let df_max = self.config.df_max(sources);
        let band = candidates.in_band(self.config.term_df_min, df_max);
        report.candidates_kept = band.len();
        report.candidates_derived = band.iter().filter(|(_, t)| t.derived).count();
        let band_set: HashSet<&str> = band.iter().map(|(n, _)| *n).collect();

        let mined = self
            .store
            .vocabulary_rows_of_origin(VocabularyOrigin::Mined)
            .await?;
        let retract: Vec<FragmentId> = mined
            .iter()
            .filter(|(_, normalized)| !band_set.contains(normalized.as_str()))
            .map(|(id, _)| *id)
            .collect();
        self.store.unanchor_vocabulary(&retract).await?;
        report.rows_retracted = retract.len();

        let mut spellings = self.all_spellings().await?;
        for id in &retract {
            spellings.retain(|_, row| row != id);
        }
        let new_rows: Vec<NewVocabularyRow> = band
            .iter()
            .filter(|(normalized, _)| !spellings.contains_key(*normalized))
            .map(|(normalized, tally)| mined_row(normalized, tally))
            .collect();
        report.rows_planted = self.plant(&new_rows, &mut spellings).await?;

        let relations_before = self.store.stats().await?.relations;
        self.anchor_all(Matcher::new(spellings)).await?;
        let relations_after = self.store.stats().await?.relations;
        report.anchors_added =
            usize::try_from(relations_after.saturating_sub(relations_before)).unwrap_or(0);
        Ok(())
    }

    /// Every row's normalized spelling → its fragment, by pages.
    async fn all_spellings(&self) -> Result<HashMap<String, FragmentId>, SeamError> {
        let mut spellings: HashMap<String, FragmentId> = HashMap::new();
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
                spellings.entry(row.normalized).or_insert(row.fragment);
            }
            if count < usize::try_from(VOCABULARY_LIST_MAX).unwrap_or(0) {
                break;
            }
            offset = offset.saturating_add(VOCABULARY_LIST_MAX);
        }
        Ok(spellings)
    }

    /// Plant rows in batches, file lexical search rows for the created
    /// ones, and add every planted spelling to the matcher's table.
    async fn plant(
        &self,
        rows: &[NewVocabularyRow],
        spellings: &mut HashMap<String, FragmentId>,
    ) -> Result<usize, SeamError> {
        let surface = self.store.has_search_surface().await?;
        let mut planted: usize = 0;
        for batch in rows.chunks(WRITE_BATCH) {
            let results = self.store.plant_vocabulary_rows(batch).await?;
            let mut search_rows: Vec<SearchRow> = Vec::new();
            for (row, result) in batch.iter().zip(&results) {
                spellings.insert(row.normalized.clone(), result.fragment);
                if result.created {
                    planted += 1;
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
        Ok(planted)
    }

    /// Walk every content fragment once more and anchor the rows it names.
    async fn anchor_all(&self, matcher: Matcher<FragmentId>) -> Result<(), SeamError> {
        if matcher.is_empty() {
            return Ok(());
        }
        let mentions = RelationKind::new("mentions").expect("literal kind is valid");
        let mut pending: Vec<(FragmentId, FragmentId)> = Vec::new();
        let mut after = FragmentId(0);
        for _ in 0..PAGES_MAX {
            let page = self.store.content_texts(after, TEXT_PAGE_ROWS).await?;
            for text in &page {
                for row in matcher.matches(&tokenize(&text.text)) {
                    pending.push((text.fragment, row));
                }
                if pending.len() >= WRITE_BATCH {
                    self.store.anchor_vocabulary(&mentions, &pending).await?;
                    pending.clear();
                }
            }
            match page.last() {
                Some(last) if page.len() >= usize::try_from(TEXT_PAGE_ROWS).unwrap_or(0) => {
                    after = last.fragment;
                }
                _ => break,
            }
        }
        self.store.anchor_vocabulary(&mentions, &pending).await?;
        Ok(())
    }

    /// Assign every unclustered row a home by co-occurrence.
    async fn cluster(
        &self,
        generation: u64,
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        let rows = self
            .store
            .vocabulary_rows_unclustered(self.config.cluster_assignments_per_sweep_max)
            .await?;
        let mut presence = Presence::default();
        let mut cluster_count = 0_u32;
        for cluster in self.store.clusters().await? {
            presence.size(cluster.id, cluster.member_count);
            cluster_count = cluster_count.saturating_add(1);
        }
        let mut joins: HashMap<ClusterId, Vec<FragmentId>> = HashMap::new();
        let mut pending_joins: usize = 0;
        for row in &rows {
            let sources = self
                .store
                .sources_anchored_to(row.fragment, CLUSTER_SOURCES_MAX)
                .await?;
            self.learn_presence(&mut presence, &sources).await?;
            let decision = presence.decide(
                &sources,
                self.config.cluster_join_min,
                self.config.cluster_terms_max,
            );
            match decision {
                None => {}
                Some(Assignment::Join(cluster)) => {
                    presence.assign(cluster, &sources);
                    joins.entry(cluster).or_default().push(row.fragment);
                    pending_joins += 1;
                    report.clusters_joined += 1;
                }
                Some(Assignment::Found) => {
                    if cluster_count >= self.config.clusters_max {
                        continue;
                    }
                    let founded = self
                        .store
                        .found_cluster(&row.spelling, &[row.fragment], generation)
                        .await?;
                    if let Some(cluster) = founded {
                        presence.assign(cluster, &sources);
                        cluster_count += 1;
                        report.clusters_founded += 1;
                    }
                }
            }
            if pending_joins >= WRITE_BATCH {
                self.apply_joins(&mut joins, generation).await?;
                pending_joins = 0;
            }
        }
        self.apply_joins(&mut joins, generation).await?;
        self.store.recount_clusters().await?;
        Ok(())
    }

    async fn learn_presence(
        &self,
        presence: &mut Presence,
        sources: &[SourceId],
    ) -> Result<(), SeamError> {
        let unknown: Vec<SourceId> = sources
            .iter()
            .copied()
            .filter(|s| !presence.knows(*s))
            .collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let mut stored = self.store.clusters_in_sources(&unknown).await?;
        for source in &unknown {
            stored.entry(*source).or_default();
        }
        presence.learn(stored);
        Ok(())
    }

    async fn apply_joins(
        &self,
        joins: &mut HashMap<ClusterId, Vec<FragmentId>>,
        generation: u64,
    ) -> Result<(), SeamError> {
        for (cluster, members) in joins.drain() {
            self.store
                .join_cluster(cluster, &members, generation)
                .await?;
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

    /// One model call per changed cluster while the budget lasts.
    async fn ground_clusters(
        &self,
        generation: u64,
        report: &mut VocabularyReport,
    ) -> Result<(), SeamError> {
        let Some(llm) = self.grantor.grant_named(LLM_CONSUMER, self.config.llm_lane) else {
            return Ok(());
        };
        let changed = self.store.clusters_changed_since(generation).await?;
        for cluster in changed.iter().take(self.config.cluster_llm_budget) {
            let members = self.store.cluster_members(cluster.id).await?;
            if members.is_empty() {
                continue;
            }
            let excerpts = self.excerpts_for(&members).await?;
            let aliases_max = usize::try_from(self.config.aliases_per_row_max).unwrap_or(4);
            let grounding =
                match ground::ground(llm.as_ref(), &members, &excerpts, aliases_max).await {
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

    /// Short passages where a cluster's most frequent members appear.
    async fn excerpts_for(&self, members: &[VocabularyRow]) -> Result<Vec<String>, SeamError> {
        let mut excerpts = Vec::new();
        for member in members.iter().take(EXCERPT_MEMBERS) {
            let anchors: Vec<FragmentId> = self
                .store
                .relations_touching(&[member.fragment])
                .await?
                .into_iter()
                .filter(|r| r.to == member.fragment)
                .map(|r| r.from)
                .take(EXCERPTS_PER_MEMBER)
                .collect();
            for fragment in self.store.fragments(&anchors).await? {
                if let Some(text) = fragment.text {
                    excerpts.push(text);
                }
            }
        }
        Ok(excerpts)
    }

    /// Apply what the model settled: merges re-key anchors, glosses are
    /// written once, aliases become rows related `aliases` into their word.
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

    /// The highest-degree rows, named: the first thing to read after a
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
fn mined_row(normalized: &str, tally: &mine::Tally) -> NewVocabularyRow {
    let (kind, prefix, mimetype) = match tally.shape {
        Shape::Identifier => (
            VocabularyKind::Identifier,
            "identifier",
            "text/x-inseam-identifier",
        ),
        Shape::Term | Shape::None => (VocabularyKind::Term, "term", "text/x-inseam-term"),
    };
    NewVocabularyRow {
        key: FragmentKey::new(format!("{prefix}:{normalized}"))
            .expect("a bounded normalized spelling under a literal prefix is a valid key"),
        fragment: NewFragment {
            mimetype: Mimetype::parse(mimetype).expect("literal mimetype is valid"),
            text: Some(tally.spelling.clone()),
            extent: None,
            content_address: None,
        },
        kind,
        origin: VocabularyOrigin::Mined,
        normalized: normalized.to_string(),
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
    }
}

fn millis(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let identifier = mined_row(
            "sup-100432",
            &mine::Tally {
                shape: Shape::Identifier,
                spelling: "SUP-100432".into(),
                documents: 3,
                derived: false,
            },
        );
        assert_eq!(identifier.key.as_str(), "identifier:sup-100432");
        assert_eq!(identifier.kind, VocabularyKind::Identifier);
        let term = mined_row(
            "residency stamp",
            &mine::Tally {
                shape: Shape::Term,
                spelling: "residency stamp".into(),
                documents: 3,
                derived: true,
            },
        );
        assert_eq!(term.key.as_str(), "term:residency stamp");
        assert_eq!(
            alias_row("The 80GB accelerator").key.as_str(),
            "alias:the 80gb accelerator"
        );
    }
}
