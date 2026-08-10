//! The indexing pipeline: enumerate sources, decompose them into the
//! semantic graph via transforms, attach the mandatory summary and entity
//! fragments, embed everything with text, and land it all in the store
//! (`design/indexing.md`). Every run is a reconciling sweep
//! (`design/index-maintenance.md`): unchanged sources are skipped; changed,
//! interrupted, and profile-stale ones have their fragment subtree rebuilt;
//! vanished ones are removed; orphaned entities are collected; and a pending
//! embedding migration is performed before anything else.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

use thiserror::Error;

use crate::address::ContentLength;
use crate::embed::{EmbedError, Embedder};
use crate::fragment::{Extent, FragmentId, Mimetype, NewFragment, RelationKind, Sprout};
use crate::host_fs::{self, EnumeratedSource, FsHost, FsHostError};
use crate::llm::LlmClient;
use crate::profile::IndexProfile;
use crate::store::{Freshness, IndexStore, SearchRow, SourceId, StoreError};
use crate::transform::{
    self, entities::ExtractedEntity, DecomposeBudget, TransformCtx, TransformRegistry,
};

/// Search rows buffered before an embed+write flush.
const FLUSH_AT: usize = 128;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Host(#[from] FsHostError),
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error("profile: {0}")]
    Profile(#[from] crate::dates::DateError),
}

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct IndexReport {
    pub sources_seen: usize,
    pub indexed: usize,
    pub unchanged: usize,
    pub catalog_only: usize,
    pub skipped_cutoff: usize,
    pub fragments: usize,
    pub relations: usize,
    pub entities_seen: usize,
    pub entity_calls: usize,
    /// Sources removed because enumeration no longer sees them.
    pub removed: usize,
    /// Entity fragments collected because no relation touches them anymore.
    pub entities_removed: usize,
    /// Search rows re-embedded by a pending embedding migration.
    pub reembedded: usize,
    pub llm_summaries: usize,
    pub extractive_summaries: usize,
    pub envelope_summaries: usize,
    pub embedded: usize,
    /// Dollars reported by OpenRouter across the run's calls.
    pub spent: f64,
}

impl fmt::Display for IndexReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{} sources seen: {} indexed, {} unchanged, {} catalog-only, {} past cutoff",
            self.sources_seen, self.indexed, self.unchanged, self.catalog_only, self.skipped_cutoff
        )?;
        writeln!(
            f,
            "{} fragments, {} relations, {} entity mentions",
            self.fragments, self.relations, self.entities_seen
        )?;
        if self.removed + self.entities_removed + self.reembedded > 0 {
            writeln!(
                f,
                "maintenance: {} sources removed, {} entities collected, {} rows re-embedded",
                self.removed, self.entities_removed, self.reembedded
            )?;
        }
        write!(
            f,
            "summaries: {} llm, {} extractive, {} envelope · {} embedded · ${:.4} spent",
            self.llm_summaries,
            self.extractive_summaries,
            self.envelope_summaries,
            self.embedded,
            self.spent
        )
    }
}

pub struct Indexer<'a> {
    pub store: &'a IndexStore,
    pub fs: &'a FsHost,
    pub profile: &'a IndexProfile,
    pub embedder: &'a Embedder,
    pub llm: Option<&'a LlmClient>,
}

impl Indexer<'_> {
    /// Reconcile the index with every enumerable source under `dir` and with
    /// the current profile. `rebuild` forces re-indexing of unchanged sources.
    pub async fn index_dir(&self, dir: &Path, rebuild: bool) -> Result<IndexReport, IndexError> {
        let cutoff = self.profile.cutoff.modified_after_epoch()?;
        let stamp = self.profile.shape_stamp();
        let mut report = IndexReport::default();
        // A pending embedding migration blocks search: resolve it before the
        // sweep so even a zero-change run leaves the index queryable.
        if self.store.reembed_pending() {
            self.reembed(&mut report).await?;
        }
        let sources = self.fs.enumerate(dir)?;
        report.sources_seen = sources.len();
        let registry = TransformRegistry::from_profile(self.profile);
        // LLM calls charged per transform this run, for capability mediation.
        let mut llm_calls: HashMap<&'static str, usize> = HashMap::new();
        let mut pending: Vec<PendingRow> = Vec::new();

        for source in &sources {
            let fresh = self.store.freshness(
                &source.address,
                source.envelope.modified,
                source.raw_bytes,
                &stamp,
            )?;
            if let (Some(cutoff), Some(modified)) = (cutoff, source.envelope.modified)
                && modified < cutoff {
                    // Catalog newly seen out-of-horizon sources so the map is
                    // complete; leave known ones alone — scope shrinkage
                    // never evicts what a looser profile already built.
                    if fresh == Freshness::New {
                        self.store.upsert_source(
                            &source.address,
                            &source.envelope,
                            source.raw_bytes,
                        )?;
                    }
                    report.skipped_cutoff += 1;
                    continue;
                }
            if fresh == Freshness::Unchanged && !rebuild {
                report.unchanged += 1;
                continue;
            }
            let deep_budget_left = self.profile.budget.max_sources == 0
                || report.indexed < self.profile.budget.max_sources;
            if !deep_budget_left {
                // Catalog-only: no stamp is recorded, so the source stays
                // dirty and is deep-indexed once a later run has budget.
                let sid = self
                    .store
                    .upsert_source(&source.address, &source.envelope, source.raw_bytes)?;
                self.store.mark_indexed(sid, None)?;
                report.catalog_only += 1;
                continue;
            }
            self.index_source(source, &registry, &stamp, &mut llm_calls, &mut report, &mut pending)
                .await?;
            report.indexed += 1;
            if pending.len() >= FLUSH_AT {
                self.flush(&mut pending, &mut report).await?;
            }
        }

        self.flush(&mut pending, &mut report).await?;
        self.reconcile_vanished(dir, &sources, &mut report).await?;
        let orphaned = self.store.gc_entities()?;
        self.store.delete_search_rows(&orphaned).await?;
        report.entities_removed = orphaned.len();
        self.store.rebuild_fts().await?;
        if let Some(llm) = self.llm {
            report.spent = llm.spent();
        }
        Ok(report)
    }

    /// Remove cataloged sources under the swept directory that enumeration no
    /// longer sees. No tombstones: the index is derived, and a source that
    /// reappears is simply new.
    async fn reconcile_vanished(
        &self,
        dir: &Path,
        seen: &[EnumeratedSource],
        report: &mut IndexReport,
    ) -> Result<(), IndexError> {
        let root = dir.canonicalize().map_err(|source| FsHostError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let prefix = match self.fs.address_for(&root) {
            Ok(address) => address.locator.as_str().to_string(),
            // The filesystem root itself has no locator; a sweep of `/`
            // reconciles nothing rather than failing the run.
            Err(_) => return Ok(()),
        };
        let seen: HashSet<&str> = seen.iter().map(|s| s.address.locator.as_str()).collect();
        let child_prefix = format!("{prefix}/");
        for (sid, locator) in self.store.sources_of_host(self.fs.id())? {
            let under_root = locator == prefix || locator.starts_with(&child_prefix);
            if !under_root || seen.contains(locator.as_str()) {
                continue;
            }
            let old = self.store.delete_fragments_of(sid)?;
            self.store.delete_search_rows(&old).await?;
            self.store.delete_source(sid)?;
            report.removed += 1;
            tracing::debug!(locator, "removed vanished source");
        }
        Ok(())
    }

    /// Re-populate the search table from the catalog under the profile's
    /// embedding config: text comes from SQLite, so no transform re-runs and
    /// no LLM spend — vectors are the only thing rebuilt.
    async fn reembed(&self, report: &mut IndexReport) -> Result<(), IndexError> {
        let targets = self.store.reembed_targets()?;
        report.reembedded = targets.len();
        self.store.begin_reembed().await?;
        let mut pending: Vec<PendingRow> = Vec::new();
        for (fragment, source, text) in targets {
            pending.push(PendingRow {
                fragment,
                source,
                text,
            });
            if pending.len() >= FLUSH_AT {
                self.flush(&mut pending, report).await?;
            }
        }
        self.flush(&mut pending, report).await?;
        self.store.finish_reembed()?;
        tracing::info!(rows = report.reembedded, "re-embedded search index");
        Ok(())
    }

    /// Build one source's subtree by applying every registered transform that
    /// claims its root, in registration order (structural before enrichment).
    /// The source is marked indexed only once everything is stored.
    async fn index_source(
        &self,
        source: &EnumeratedSource,
        registry: &TransformRegistry,
        stamp: &str,
        llm_calls: &mut HashMap<&'static str, usize>,
        report: &mut IndexReport,
        pending: &mut Vec<PendingRow>,
    ) -> Result<(), IndexError> {
        let content = self.read_content(source)?;
        let mut envelope = source.envelope.clone();
        if let Some(text) = &content {
            envelope.length = ContentLength::Lines(host_fs::count_lines(text));
        }

        let sid = self
            .store
            .upsert_source(&source.address, &envelope, source.raw_bytes)?;
        let old = self.store.delete_fragments_of(sid)?;
        self.store.delete_search_rows(&old).await?;

        let root_extent = match envelope.length {
            ContentLength::Lines(n) => Extent::Lines { start: 1, end: n.max(1) },
            ContentLength::Bytes(n) => Extent::Bytes { start: 0, end: n },
        };
        let root = self.store.insert_fragment(
            Some(sid),
            &NewFragment {
                mimetype: envelope.content_type.clone(),
                text: None,
                extent: Some(root_extent),
            },
        )?;
        self.store.set_root_fragment(sid, root)?;
        report.fragments += 1;

        let mut fragment_budget = self.profile.budget.max_fragments_per_source;
        let mut texted: Vec<(FragmentId, String)> = Vec::new();

        for registered in registry.claimants(&envelope.content_type, true) {
            let name = registered.transform.name();
            // Capability mediation: the LLM handle is granted only while the
            // transform's per-run call budget lasts; withheld, the transform
            // falls back or emits nothing.
            let calls_made = llm_calls.entry(name).or_insert(0);
            let ctx = TransformCtx {
                envelope: &envelope,
                text: content.as_deref(),
                llm: self.llm.filter(|_| *calls_made < registered.llm_call_budget),
                model: &self.profile.llm.transform_model,
            };
            let out = registered.transform.apply(&ctx).await;
            *calls_made += out.llm_calls;
            if name == "entity-extractor" {
                report.entity_calls += out.llm_calls;
            }
            for sprout in &out.sprouts {
                if sprout.fragment.mimetype.is_summary() {
                    match sprout.fragment.mimetype.param("via") {
                        Some("llm") => report.llm_summaries += 1,
                        Some("envelope") => report.envelope_summaries += 1,
                        _ => report.extractive_summaries += 1,
                    }
                }
            }
            let sprouts = transform::prune(
                out.sprouts,
                DecomposeBudget {
                    max_depth: self.profile.budget.max_depth,
                    max_fragments: fragment_budget,
                },
            );
            let planted: usize = sprouts.iter().map(Sprout::count).sum();
            fragment_budget = fragment_budget.saturating_sub(planted);
            self.plant(sid, root, sprouts, report, pending, &mut texted)?;
            if !out.entities.is_empty() {
                self.wire_entities(root, out.entities, &texted, report, pending)?;
            }
        }

        self.store.mark_indexed(sid, Some(stamp))?;
        tracing::debug!(address = %source.address, "indexed");
        Ok(())
    }

    fn read_content(&self, source: &EnumeratedSource) -> Result<Option<String>, IndexError> {
        if !host_fs::is_texty(&source.envelope.content_type)
            || source.raw_bytes > self.profile.budget.max_content_bytes
        {
            return Ok(None);
        }
        Ok(Some(self.fs.read_text(&source.address)?))
    }

    /// Persist a sprout forest under `parent`, collecting text-bearing
    /// fragments for embedding and entity attachment.
    fn plant(
        &self,
        sid: SourceId,
        parent: FragmentId,
        sprouts: Vec<Sprout>,
        report: &mut IndexReport,
        pending: &mut Vec<PendingRow>,
        texted: &mut Vec<(FragmentId, String)>,
    ) -> Result<(), IndexError> {
        for sprout in sprouts {
            let id = self.store.insert_fragment(Some(sid), &sprout.fragment)?;
            self.store
                .insert_relation(&sprout.relation.edge(parent, id))?;
            report.fragments += 1;
            report.relations += 1;
            if let Some(text) = &sprout.fragment.text
                && !text.trim().is_empty() {
                    pending.push(PendingRow {
                        fragment: id,
                        source: Some(sid),
                        text: text.clone(),
                    });
                    // Derived understanding (summaries) is searchable but not
                    // a mention site: entities wire to source content only.
                    if !sprout.fragment.mimetype.is_inseam_defined() {
                        texted.push((id, text.clone()));
                    }
                }
            self.plant(sid, id, sprout.children, report, pending, texted)?;
        }
        Ok(())
    }

    /// Deduplicate extracted entities through the index-wide registry and
    /// wire `mentions` relations to the fragments whose text references them.
    /// This stays core-side: a transform cannot know fragment ids.
    fn wire_entities(
        &self,
        root: FragmentId,
        extracted: Vec<ExtractedEntity>,
        texted: &[(FragmentId, String)],
        report: &mut IndexReport,
        pending: &mut Vec<PendingRow>,
    ) -> Result<(), IndexError> {
        for entity in extracted {
            let key = entity.key();
            let fragment = match self.store.entity_fragment(&key)? {
                Some(f) => f,
                None => {
                    let f = self.store.insert_fragment(
                        None,
                        &NewFragment {
                            mimetype: Mimetype::entity()
                                .with_param("kind", entity.kind.as_str()),
                            text: Some(entity.name.clone()),
                            extent: None,
                        },
                    )?;
                    self.store.register_entity(&key, f)?;
                    report.fragments += 1;
                    pending.push(PendingRow {
                        fragment: f,
                        source: None,
                        text: entity.name.clone(),
                    });
                    f
                }
            };
            // Relate the entity to the fragments that actually mention it,
            // falling back to the source root.
            let needle = entity.name.to_lowercase();
            let mut mentioned = false;
            for (fid, ftext) in texted {
                if ftext.to_lowercase().contains(&needle) {
                    self.store
                        .insert_relation(&RelationKind::Mentions.edge(*fid, fragment))?;
                    report.relations += 1;
                    mentioned = true;
                }
            }
            if !mentioned {
                self.store
                    .insert_relation(&RelationKind::Mentions.edge(root, fragment))?;
                report.relations += 1;
            }
            report.entities_seen += 1;
        }
        Ok(())
    }

    /// Embed buffered rows and land them in the search table.
    async fn flush(
        &self,
        pending: &mut Vec<PendingRow>,
        report: &mut IndexReport,
    ) -> Result<(), IndexError> {
        if pending.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(pending);
        let vectors: Vec<Option<Vec<f32>>> = if self.embedder.dimensions().is_some() {
            let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
            match self.embedder.embed(&texts).await {
                Ok(vs) => {
                    report.embedded += vs.len();
                    vs.into_iter().map(Some).collect()
                }
                Err(e) => {
                    tracing::warn!("embedding failed; rows stay text-searchable only: {e}");
                    vec![None; rows.len()]
                }
            }
        } else {
            vec![None; rows.len()]
        };
        let search_rows: Vec<SearchRow> = rows
            .into_iter()
            .zip(vectors)
            .map(|(row, vector)| SearchRow {
                fragment: row.fragment,
                source: row.source,
                text: row.text,
                vector,
            })
            .collect();
        self.store.add_search_rows(&search_rows).await?;
        Ok(())
    }
}

struct PendingRow {
    fragment: FragmentId,
    source: Option<SourceId>,
    text: String,
}

#[cfg(test)]
mod tests {
    // The indexer's behavior is covered end-to-end in `tests/index_and_find.rs`
    // with a temp corpus and the hashed embedder; unit surface here would
    // only restate that setup.
}
