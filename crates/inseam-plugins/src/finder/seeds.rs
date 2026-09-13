//! Seeding a query: the five ranked lists and their fusion
//! (`design/finder.md`, `design/vocabulary.md`). Prose and lexical BM25 and
//! the vector neighbours are the hybrid seed; **exact grounding** adds the
//! vocabulary rows the question spells outright, and **cluster grounding**
//! adds the rows and aliases of the clusters the query's vector lands on —
//! the offline bridge from a paraphrased question to the corpus's word.
//! Fusion is reciprocal rank with a weight and a rank gate per list, and it
//! keeps every list's contribution apart so the ledger can name it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use inseam_kernel::fragment::{FragmentId, RelationKind};
use inseam_kernel::store::{ClusterId, IndexStore, VocabularyRow};
use inseam_seams::SeamError;
use inseam_seams::embedder::Embedder;
use inseam_seams::finder::SeedChannel;

use super::config::FinderConfig;

/// Longest phrase exact grounding looks for, in query tokens.
pub const GRAM_TOKENS_MAX: usize = 4;
/// Most query tokens exact grounding reads; a question is not a document.
pub const QUERY_TOKENS_MAX: usize = 48;
/// Most members and aliases one cluster contributes to the seed list.
pub const CLUSTER_ROWS_MAX: usize = 128;
/// Neighbourhood chunk for alias lookups.
const ALIAS_LOOKUP_MAX: u32 = 4_096;

/// The seeds of one query, list by list, and their fusion. Every array is
/// indexed by [`SeedChannel::index`].
pub struct Seeds {
    /// Each list best-first, ungated, so evidence can report full ranks.
    pub lists: [Vec<i64>; 5],
    /// Fused seed score per fragment.
    pub fused: HashMap<i64, f64>,
    /// Each channel's share of the fused score per fragment.
    pub by_channel: HashMap<i64, [f64; 5]>,
    pub clusters_matched: u32,
    pub grounding_ms: u64,
}

impl Seeds {
    pub fn hits(&self, channel: SeedChannel) -> u32 {
        u32::try_from(self.lists[channel.index()].len()).unwrap_or(u32::MAX)
    }

    /// One-based rank of a fragment in a channel's list.
    pub fn rank_in(&self, channel: SeedChannel, id: i64) -> Option<u32> {
        self.lists[channel.index()]
            .iter()
            .position(|candidate| *candidate == id)
            .map(|index| u32::try_from(index + 1).unwrap_or(u32::MAX))
    }

    pub fn total(&self) -> f64 {
        self.fused.values().sum()
    }
}

/// The clusters' vectors, loaded once per vocabulary generation: ten
/// thousand dot products per query is well under a millisecond, and a
/// reload only when the pass changed something.
#[derive(Default)]
pub struct ClusterCache {
    generation: u64,
    clusters: Vec<(ClusterId, Vec<f32>)>,
}

impl ClusterCache {
    /// The cached clusters at the store's current generation, reloading
    /// when the generation moved.
    pub async fn current(
        cache: &tokio::sync::Mutex<ClusterCache>,
        store: &IndexStore,
    ) -> Result<Vec<(ClusterId, Vec<f32>)>, SeamError> {
        let generation = store.vocabulary_generation().await?;
        let mut guard = cache.lock().await;
        if guard.generation != generation || guard.clusters.is_empty() {
            let loaded: Vec<(ClusterId, Vec<f32>)> = store
                .clusters()
                .await?
                .into_iter()
                .filter_map(|cluster| cluster.vector.map(|vector| (cluster.id, vector)))
                .collect();
            guard.generation = generation;
            guard.clusters = loaded;
        }
        Ok(guard.clusters.clone())
    }
}

/// What seeding needs from the service.
pub struct Seeder<'a> {
    pub store: &'a IndexStore,
    pub embedder: &'a dyn Embedder,
    pub clusters: &'a tokio::sync::Mutex<ClusterCache>,
    pub config: &'a FinderConfig,
}

impl Seeder<'_> {
    /// Run every enabled list and fuse them.
    pub async fn seeds(&self, text: &str) -> Result<Seeds, SeamError> {
        let (prose, lexical) = self.text_lists(text).await?;
        let query_vector = self.query_vector(text).await?;
        let vector = self.vector_list(query_vector.as_deref()).await?;
        let grounding_started = Instant::now();
        let exact = if self.config.list(SeedChannel::Exact).enabled {
            self.exact_list(text).await?
        } else {
            Vec::new()
        };
        let (cluster, clusters_matched) = if self.config.list(SeedChannel::Cluster).enabled {
            self.cluster_list(query_vector.as_deref()).await?
        } else {
            (Vec::new(), 0)
        };
        let grounding_ms = millis(grounding_started.elapsed());
        let lists = [prose, lexical, vector, exact, cluster];
        let (fused, by_channel) = fuse(&lists, self.config);
        tracing::info!(
            prose = lists[0].len(),
            lexical = lists[1].len(),
            vector = lists[2].len(),
            exact = lists[3].len(),
            cluster = lists[4].len(),
            fused = fused.len(),
            "finder seed retrieval completed"
        );
        Ok(Seeds {
            lists,
            fused,
            by_channel,
            clusters_matched,
            grounding_ms,
        })
    }

    /// Function words go: a query that ORs "the" matches every row of a
    /// large index and BM25 scores them all, for rows that rank last.
    /// Prose rows and lexical rows are two lists, each ranked by its own
    /// table's statistics: a one-line term row and a whole document are
    /// not comparable by BM25 score.
    async fn text_lists(&self, text: &str) -> Result<(Vec<i64>, Vec<i64>), SeamError> {
        let prose_on = self.config.list(SeedChannel::Prose).enabled;
        let lexical_on = self.config.list(SeedChannel::Lexical).enabled;
        if !prose_on && !lexical_on {
            return Ok((Vec::new(), Vec::new()));
        }
        let query = inseam_seams::extract::strip_stopwords(text);
        let prose = if prose_on {
            ids_of(self.store.search_fts(&query, self.config.seed_k).await?)
        } else {
            Vec::new()
        };
        let lexical = if lexical_on {
            ids_of(
                self.store
                    .search_fts_lexical(&query, self.config.seed_k)
                    .await?,
            )
        } else {
            Vec::new()
        };
        Ok((prose, lexical))
    }

    /// The query's vector, when the node embeds and any list wants it.
    async fn query_vector(&self, text: &str) -> Result<Option<Vec<f32>>, SeamError> {
        let wanted = self.config.list(SeedChannel::Vector).enabled
            || self.config.list(SeedChannel::Cluster).enabled;
        if self.embedder.dimensions().is_none() || !wanted {
            return Ok(None);
        }
        let mut vectors = self.embedder.embed(&[text]).await?;
        Ok(vectors.pop())
    }

    /// Nearest-k returns the k nearest whatever the distance; beyond the
    /// floor a "neighbour" is noise and must not seed the walk.
    async fn vector_list(&self, query_vector: Option<&[f32]>) -> Result<Vec<i64>, SeamError> {
        let Some(vector) = query_vector else {
            return Ok(Vec::new());
        };
        if !self.config.list(SeedChannel::Vector).enabled {
            return Ok(Vec::new());
        }
        let mut hits = self.store.search_vector(vector, self.config.seed_k).await?;
        hits.retain(|(_, distance)| f64::from(*distance) <= self.config.max_vector_distance);
        Ok(ids_of(hits))
    }

    /// Exact grounding: every n-gram of the question that spells a
    /// vocabulary row, longest phrases first, rarer rows first within a
    /// length. One index probe per n-gram, bounded by the token cap.
    async fn exact_list(&self, text: &str) -> Result<Vec<i64>, SeamError> {
        let tokens = query_tokens(text);
        let mut found: Vec<(usize, VocabularyRow)> = Vec::new();
        let mut seen: HashSet<i64> = HashSet::new();
        for n in (1..=GRAM_TOKENS_MAX).rev() {
            if tokens.len() < n {
                continue;
            }
            for window in tokens.windows(n) {
                if n == 1 && is_stopword(&window[0]) {
                    continue;
                }
                let gram = window.join(" ");
                for row in self.store.vocabulary_rows_spelled(&gram).await? {
                    if seen.insert(row.fragment.0) {
                        found.push((n, row));
                    }
                }
            }
        }
        found.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(a.1.document_frequency.cmp(&b.1.document_frequency))
                .then(a.1.fragment.cmp(&b.1.fragment))
        });
        Ok(found.into_iter().map(|(_, row)| row.fragment.0).collect())
    }

    /// Cluster grounding: the clusters whose vector clears the cosine
    /// floor, best first, each contributing its members (rarest first) and
    /// their aliases. Returns the list and how many clusters matched.
    async fn cluster_list(
        &self,
        query_vector: Option<&[f32]>,
    ) -> Result<(Vec<i64>, u32), SeamError> {
        let Some(vector) = query_vector else {
            return Ok((Vec::new(), 0));
        };
        let clusters = ClusterCache::current(self.clusters, self.store).await?;
        let matched = nearest_clusters(
            vector,
            &clusters,
            self.config.cluster_query_cosine,
            self.config.clusters_per_query_max,
        );
        let mut list: Vec<i64> = Vec::new();
        let mut seen: HashSet<i64> = HashSet::new();
        for (cluster, _) in &matched {
            let mut members = self.store.cluster_members(*cluster).await?;
            members.sort_by(|a, b| {
                a.document_frequency
                    .cmp(&b.document_frequency)
                    .then(a.fragment.cmp(&b.fragment))
            });
            members.truncate(CLUSTER_ROWS_MAX);
            let member_ids: Vec<FragmentId> = members.iter().map(|m| m.fragment).collect();
            let aliases = self.aliases_of(&member_ids).await?;
            for id in member_ids.iter().chain(aliases.iter()) {
                if seen.insert(id.0) {
                    list.push(id.0);
                }
            }
        }
        Ok((list, u32::try_from(matched.len()).unwrap_or(u32::MAX)))
    }

    /// The alias rows related `aliases` into these rows.
    async fn aliases_of(&self, rows: &[FragmentId]) -> Result<Vec<FragmentId>, SeamError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let aliases_kind = RelationKind::new("aliases").expect("literal kind is valid");
        let relations = self.store.relations_touching(rows).await?;
        let members: HashSet<FragmentId> = rows.iter().copied().collect();
        let mut aliases: Vec<FragmentId> = relations
            .into_iter()
            .filter(|r| r.kind == aliases_kind && members.contains(&r.to))
            .map(|r| r.from)
            .take(usize::try_from(ALIAS_LOOKUP_MAX).unwrap_or(usize::MAX))
            .collect();
        aliases.sort();
        aliases.dedup();
        Ok(aliases)
    }
}

/// Reciprocal rank fusion with a weight and a rank gate per list
/// (`design/vocabulary.md`): `score(d) = Σ weight(list) / (k + rank)` over
/// the ranks under each list's gate. Only an id's best rank per list
/// counts. Returns the fused scores and each channel's share.
pub fn fuse(
    lists: &[Vec<i64>; 5],
    config: &FinderConfig,
) -> (HashMap<i64, f64>, HashMap<i64, [f64; 5]>) {
    let mut fused: HashMap<i64, f64> = HashMap::new();
    let mut by_channel: HashMap<i64, [f64; 5]> = HashMap::new();
    for channel in SeedChannel::ALL {
        let list = config.list(channel);
        if !list.enabled || list.weight <= 0.0 {
            continue;
        }
        let gate = usize::try_from(list.ranks_max).unwrap_or(usize::MAX);
        let mut seen: HashSet<i64> = HashSet::new();
        for (rank, id) in lists[channel.index()].iter().take(gate).enumerate() {
            if !seen.insert(*id) {
                continue;
            }
            let score = list.weight / (config.rrf_k + rank as f64 + 1.0);
            *fused.entry(*id).or_insert(0.0) += score;
            by_channel.entry(*id).or_insert([0.0; 5])[channel.index()] += score;
        }
    }
    (fused, by_channel)
}

/// Reciprocal rank fusion over best-first id lists: score(d) = Σ 1/(k + rank).
/// Rank-based, so BM25 scores and cosine distances never need to be made
/// comparable — the reason RRF is the working fusion (`design/finder.md`).
/// Only an id's best rank in each list counts. The cross-node merge fuses
/// with this.
pub fn rrf_fuse(lists: &[&[i64]], k: f64) -> HashMap<i64, f64> {
    let mut fused: HashMap<i64, f64> = HashMap::new();
    for list in lists {
        let mut seen = HashSet::new();
        for (rank, id) in list.iter().enumerate() {
            if seen.insert(*id) {
                *fused.entry(*id).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
            }
        }
    }
    fused
}

/// The clusters whose cosine with the query clears `floor`, best first,
/// at most `limit`.
pub fn nearest_clusters(
    query: &[f32],
    clusters: &[(ClusterId, Vec<f32>)],
    floor: f64,
    limit: u32,
) -> Vec<(ClusterId, f64)> {
    let mut scored: Vec<(ClusterId, f64)> = clusters
        .iter()
        .filter(|(_, vector)| vector.len() == query.len())
        .map(|(id, vector)| (*id, cosine(query, vector)))
        .filter(|(_, similarity)| *similarity >= floor)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    scored
}

/// Cosine similarity; zero for a zero vector.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        norm_a += f64::from(*x) * f64::from(*x);
        norm_b += f64::from(*y) * f64::from(*y);
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// The question's tokens as exact grounding matches them: lowercase, edge
/// punctuation trimmed, inner `-_./` kept so identifiers survive, at most
/// [`QUERY_TOKENS_MAX`] of them.
pub fn query_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|token| !token.is_empty())
        .take(QUERY_TOKENS_MAX)
        .collect()
}

/// A one-token gram worth probing: not a function word, not one letter.
fn is_stopword(token: &str) -> bool {
    token.chars().count() < 2 || inseam_seams::extract::is_stopword(token)
}

fn ids_of(hits: Vec<(FragmentId, f32)>) -> Vec<i64> {
    hits.into_iter().map(|(id, _)| id.0).collect()
}

fn millis(elapsed: std::time::Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// The finder's cluster cache handle, shared by the service.
pub type SharedClusterCache = Arc<tokio::sync::Mutex<ClusterCache>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finder::config::SeedList;

    #[test]
    fn rrf_rewards_presence_in_both_lists() {
        let fused = rrf_fuse(&[&[1, 2, 3], &[2, 9]], 60.0);
        assert!(fused[&2] > fused[&1], "rank-2+rank-1 beats a single rank-1");
        assert!(fused[&1] > fused[&3]);
        assert!(fused.contains_key(&9));
        assert!(rrf_fuse(&[&[], &[]], 60.0).is_empty());
    }

    #[test]
    fn fusion_honors_weights_gates_and_keeps_channels_apart() {
        let mut config = FinderConfig::default();
        config.seed_lists.cluster = SeedList {
            enabled: true,
            weight: 0.5,
            ranks_max: 1,
        };
        let lists = [vec![1, 2], vec![], vec![], vec![], vec![3, 4]];
        let (fused, by_channel) = fuse(&lists, &config);
        assert!(fused.contains_key(&3), "rank one passes the gate");
        assert!(!fused.contains_key(&4), "rank two is gated out");
        let prose_top = by_channel[&1][SeedChannel::Prose.index()];
        let cluster_top = by_channel[&3][SeedChannel::Cluster.index()];
        assert!((cluster_top - prose_top * 0.5).abs() < 1e-12);
        assert_eq!(by_channel[&1][SeedChannel::Cluster.index()], 0.0);
        config.seed_lists.cluster.enabled = false;
        let (fused, _) = fuse(&lists, &config);
        assert!(!fused.contains_key(&3));
    }

    #[test]
    fn query_tokens_keep_identifiers_and_trim_punctuation() {
        assert_eq!(
            query_tokens("What's the H200 (eu-central-1) status, SUP-100432?"),
            vec![
                "what's",
                "the",
                "h200",
                "eu-central-1",
                "status",
                "sup-100432"
            ]
        );
        assert!(query_tokens("  ").is_empty());
    }

    #[test]
    fn stopwords_are_skipped_as_single_grams() {
        assert!(is_stopword("the"));
        assert!(!is_stopword("h200"));
        assert!(is_stopword("a"));
    }

    #[test]
    fn nearest_clusters_apply_the_floor_and_the_limit() {
        let clusters = vec![
            (ClusterId(1), vec![1.0, 0.0]),
            (ClusterId(2), vec![0.7, 0.7]),
            (ClusterId(3), vec![0.0, 1.0]),
            (ClusterId(4), vec![1.0]),
        ];
        let near = nearest_clusters(&[1.0, 0.0], &clusters, 0.5, 5);
        assert_eq!(
            near.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![ClusterId(1), ClusterId(2)]
        );
        assert_eq!(nearest_clusters(&[1.0, 0.0], &clusters, 0.5, 1).len(), 1);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
