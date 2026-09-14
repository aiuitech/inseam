//! Clusters (`design/vocabulary.md`): sets of vocabulary rows that co-occur
//! across sources. Formation is by co-occurrence, not by embedding — a new
//! row joins the cluster whose members share the most of its sources, and
//! founds one when none does — which is exact, free, and defined for a row
//! that appears in a single document. The whole decision runs in memory
//! over the sources the pass matched, most frequent row first, and is
//! written to the store afterwards in batches: clusters founded this pass
//! carry provisional ids until then. The vector is embedded afterwards,
//! from the members' spellings and glosses in a stable order, for the one
//! use a discrete match cannot serve: grounding a paraphrased query.

use std::collections::{HashMap, HashSet};

use inseam_kernel::address::ContentDigest;
use inseam_kernel::store::{ClusterId, SourceId, StoredCluster, VocabularyRow};

/// Most members named in a cluster's embedded text.
pub const EMBED_MEMBERS_MAX: usize = 64;
/// Longest gloss carried into the embedded text.
const EMBED_GLOSS_CHARS_MAX: usize = 120;
/// Clusters remembered per source: past this a source is about everything
/// and says nothing about one topic.
pub const CLUSTERS_PER_SOURCE_MAX: usize = 32;

/// A cluster as the in-memory decision names it: one the store holds, or
/// one founded this pass, by its index in [`Clustering::founded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Home {
    Stored(ClusterId),
    Founded(usize),
}

/// The co-occurrence view one pass keeps in memory: for every source, the
/// clusters present in it — the store's assignments plus the ones made
/// earlier in the same pass, so a row founded a moment ago is a candidate
/// home for the next row.
#[derive(Default)]
pub struct Clustering {
    by_source: HashMap<SourceId, Vec<Home>>,
    members: HashMap<Home, u32>,
    /// Clusters founded this pass: label and members, in founding order.
    pub founded: Vec<(String, Vec<usize>)>,
    /// Rows joining clusters the store already holds.
    pub joins: HashMap<ClusterId, Vec<usize>>,
    pub joined: usize,
}

impl Clustering {
    /// Record a stored cluster's member count.
    pub fn size(&mut self, cluster: ClusterId, members: u32) {
        self.members.insert(Home::Stored(cluster), members);
    }

    /// Record a row already assigned to a stored cluster: its sources
    /// carry that cluster.
    pub fn place(&mut self, cluster: ClusterId, sources: &[SourceId]) {
        self.mark(Home::Stored(cluster), sources);
    }

    fn mark(&mut self, home: Home, sources: &[SourceId]) {
        for source in sources {
            let present = self.by_source.entry(*source).or_default();
            if present.len() >= CLUSTERS_PER_SOURCE_MAX || present.contains(&home) {
                continue;
            }
            present.push(home);
        }
    }

    /// Decide and record a row's home: the best cluster present in at
    /// least `join_min` of its sources with room under `members_max`,
    /// else a cluster of its own when `clusters_max` allows. A row with no
    /// source gets nothing. Returns the home taken, if any.
    pub fn assign(
        &mut self,
        row: usize,
        label: &str,
        sources: &[SourceId],
        join_min: f64,
        members_max: u32,
        clusters_max: usize,
    ) -> Option<Home> {
        if sources.is_empty() {
            return None;
        }
        match self.decide(sources, join_min, members_max) {
            Some(home) => {
                match home {
                    Home::Stored(cluster) => self.joins.entry(cluster).or_default().push(row),
                    Home::Founded(index) => self.founded[index].1.push(row),
                }
                self.joined += 1;
                *self.members.entry(home).or_insert(0) += 1;
                self.mark(home, sources);
                Some(home)
            }
            None => {
                if self.members.len() >= clusters_max {
                    return None;
                }
                let home = Home::Founded(self.founded.len());
                self.founded.push((label.to_string(), vec![row]));
                self.members.insert(home, 1);
                self.mark(home, sources);
                Some(home)
            }
        }
    }

    /// The best cluster present in at least `join_min` of the sources
    /// with room under `members_max`; ties go to the earlier cluster.
    fn decide(&self, sources: &[SourceId], join_min: f64, members_max: u32) -> Option<Home> {
        let mut shared: HashMap<Home, u32> = HashMap::new();
        for source in sources {
            if let Some(homes) = self.by_source.get(source) {
                for home in homes {
                    *shared.entry(*home).or_insert(0) += 1;
                }
            }
        }
        let total = u32::try_from(sources.len()).unwrap_or(u32::MAX);
        let mut best: Option<(Home, u32)> = None;
        for (home, count) in shared {
            let fraction = f64::from(count) / f64::from(total);
            let full = self.members.get(&home).copied().unwrap_or(0) >= members_max;
            if fraction < join_min || full {
                continue;
            }
            let better = match best {
                None => true,
                Some((best_home, best_count)) => {
                    count > best_count || (count == best_count && home < best_home)
                }
            };
            if better {
                best = Some((home, count));
            }
        }
        best.map(|(home, _)| home)
    }
}

/// The text a cluster's vector embeds: the members most frequent first,
/// each with its gloss when it has one, in a stable order so the digest
/// is stable and a reembed happens only when membership or a gloss moved.
pub fn embed_text(members: &[VocabularyRow]) -> String {
    let mut sorted: Vec<&VocabularyRow> = members.iter().collect();
    sorted.sort_by(|a, b| {
        b.document_frequency
            .cmp(&a.document_frequency)
            .then(a.normalized.cmp(&b.normalized))
    });
    sorted
        .iter()
        .take(EMBED_MEMBERS_MAX)
        .map(|row| match &row.gloss {
            Some(gloss) => format!(
                "{}: {}",
                row.spelling,
                inseam_seams::text::truncate_chars(gloss, EMBED_GLOSS_CHARS_MAX)
            ),
            None => row.spelling.clone(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn digest_of(text: &str) -> String {
    ContentDigest::of_bytes(text.as_bytes()).to_hex()
}

/// Pairs to merge: for each changed cluster, the nearest other cluster
/// by cosine when it clears `merge_cosine`. Each changed cluster merges at
/// most once per pass, smaller into larger, and a cluster already chosen
/// as a loser is not a survivor for anyone else.
pub fn merges(
    changed: &[StoredCluster],
    all: &[StoredCluster],
    merge_cosine: f64,
) -> Vec<(ClusterId, ClusterId)> {
    let mut taken: HashSet<ClusterId> = HashSet::new();
    let mut out: Vec<(ClusterId, ClusterId)> = Vec::new();
    for cluster in changed {
        let Some(vector) = &cluster.vector else {
            continue;
        };
        if taken.contains(&cluster.id) {
            continue;
        }
        let nearest = all
            .iter()
            .filter(|other| other.id != cluster.id && !taken.contains(&other.id))
            .filter_map(|other| {
                other
                    .vector
                    .as_ref()
                    .filter(|v| v.len() == vector.len())
                    .map(|v| (other, crate::finder::cosine(vector, v)))
            })
            .filter(|(_, similarity)| *similarity >= merge_cosine)
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.id.cmp(&a.0.id)));
        let Some((other, _)) = nearest else {
            continue;
        };
        let (survivor, loser) = if other.member_count >= cluster.member_count {
            (other.id, cluster.id)
        } else {
            (cluster.id, other.id)
        };
        taken.insert(survivor);
        taken.insert(loser);
        out.push((survivor, loser));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::fragment::{FragmentId, FragmentKey};
    use inseam_kernel::store::{VocabularyKind, VocabularyOrigin};

    fn row(spelling: &str, frequency: u32, gloss: Option<&str>) -> VocabularyRow {
        VocabularyRow {
            fragment: FragmentId(1),
            key: FragmentKey::new(format!("term:{spelling}")).expect("valid"),
            kind: VocabularyKind::Term,
            origin: VocabularyOrigin::Mined,
            spelling: spelling.to_string(),
            normalized: spelling.to_lowercase(),
            document_frequency: frequency,
            cluster: None,
            gloss: gloss.map(str::to_string),
        }
    }

    fn cluster(id: i64, members: u32, vector: Vec<f32>) -> StoredCluster {
        StoredCluster {
            id: ClusterId(id),
            label: String::new(),
            text_digest: String::new(),
            vector: Some(vector),
            member_count: members,
            document_frequency: 0,
            changed_sweep: 0,
        }
    }

    fn sources(ids: &[i64]) -> Vec<SourceId> {
        ids.iter().map(|id| SourceId(*id)).collect()
    }

    #[test]
    fn rows_join_the_cluster_sharing_most_of_their_sources() {
        let mut clustering = Clustering::default();
        clustering.size(ClusterId(7), 2);
        clustering.size(ClusterId(9), 2);
        clustering.place(ClusterId(7), &sources(&[1, 2]));
        clustering.place(ClusterId(9), &sources(&[2, 3]));
        let row_sources = sources(&[1, 2, 4]);
        assert_eq!(
            clustering.assign(0, "a", &row_sources, 0.5, 64, 100),
            Some(Home::Stored(ClusterId(7)))
        );
        assert_eq!(clustering.joins[&ClusterId(7)], vec![0]);
        assert_eq!(
            clustering.assign(1, "b", &sources(&[4, 5, 6]), 0.9, 64, 100),
            Some(Home::Founded(0)),
            "under the join floor a row founds"
        );
        assert_eq!(clustering.founded[0], ("b".to_string(), vec![1]));
        assert_eq!(
            clustering.assign(2, "c", &sources(&[5, 6]), 0.5, 64, 100),
            Some(Home::Founded(0)),
            "a cluster founded this pass is a home for the next row"
        );
        assert_eq!(clustering.founded[0].1, vec![1, 2]);
        assert_eq!(clustering.assign(3, "d", &[], 0.5, 64, 100), None);
        assert_eq!(clustering.joined, 2);
    }

    #[test]
    fn full_clusters_and_the_cluster_cap_hold() {
        let mut clustering = Clustering::default();
        clustering.size(ClusterId(7), 2);
        clustering.place(ClusterId(7), &sources(&[1, 2]));
        assert_eq!(
            clustering.assign(0, "a", &sources(&[1, 2]), 0.5, 2, 100),
            Some(Home::Founded(0)),
            "a full cluster takes nobody"
        );
        assert_eq!(
            clustering.assign(1, "b", &sources(&[9]), 0.5, 64, 2),
            None,
            "the stored cluster and the founded one fill the cap"
        );
    }

    #[test]
    fn embed_text_is_stable_and_frequency_ordered() {
        let members = vec![
            row("beta", 2, None),
            row("Alpha", 5, Some("the first")),
            row("gamma", 2, None),
        ];
        assert_eq!(embed_text(&members), "Alpha: the first; beta; gamma");
        assert_eq!(digest_of("x"), digest_of("x"));
        assert_ne!(digest_of("x"), digest_of("y"));
    }

    #[test]
    fn merges_pair_each_changed_cluster_once_smaller_into_larger() {
        let all = vec![
            cluster(1, 3, vec![1.0, 0.0]),
            cluster(2, 5, vec![0.99, 0.1]),
            cluster(3, 1, vec![0.0, 1.0]),
        ];
        let pairs = merges(&all[..1], &all, 0.9);
        assert_eq!(pairs, vec![(ClusterId(2), ClusterId(1))]);
        assert!(merges(&all[2..], &all, 0.9).is_empty());
        let both = merges(&all[..2], &all, 0.9);
        assert_eq!(both.len(), 1, "a cluster merges at most once per pass");
    }
}
