//! Clusters (`design/vocabulary.md`): sets of vocabulary rows that co-occur
//! across sources. Formation is by co-occurrence, not by embedding — a new
//! row joins the cluster whose members share the most of its sources, and
//! founds one when none does — which is exact, free, and defined for a row
//! that appears in a single document. The vector is embedded afterwards,
//! from the members' spellings and glosses in a stable order, for the one
//! use a discrete match cannot serve: grounding a paraphrased query.

use std::collections::{HashMap, HashSet};

use inseam_kernel::address::ContentDigest;
use inseam_kernel::store::{ClusterId, SourceId, StoredCluster, VocabularyRow};

/// Most members named in a cluster's embedded text.
pub const EMBED_MEMBERS_MAX: usize = 64;
/// Longest gloss carried into the embedded text.
const EMBED_GLOSS_CHARS_MAX: usize = 120;

/// Where a row should go: an existing cluster, a cluster founded in this
/// pass, or a cluster of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assignment {
    Join(ClusterId),
    Found,
}

/// The co-occurrence view one pass keeps in memory: for every source the
/// pass has looked at, the clusters present in it — the store's answer
/// plus the assignments made earlier in the same pass, so a row founded
/// a moment ago is a candidate home for the next row.
#[derive(Default)]
pub struct Presence {
    by_source: HashMap<SourceId, HashSet<ClusterId>>,
    members: HashMap<ClusterId, u32>,
}

impl Presence {
    /// Record what the store knows for these sources.
    pub fn learn(&mut self, stored: HashMap<SourceId, Vec<ClusterId>>) {
        for (source, clusters) in stored {
            self.by_source.entry(source).or_default().extend(clusters);
        }
    }

    pub fn knows(&self, source: SourceId) -> bool {
        self.by_source.contains_key(&source)
    }

    /// Record a cluster's member count as the store reports it.
    pub fn size(&mut self, cluster: ClusterId, members: u32) {
        self.members.insert(cluster, members);
    }

    /// Record an assignment made in this pass.
    pub fn assign(&mut self, cluster: ClusterId, sources: &[SourceId]) {
        for source in sources {
            self.by_source.entry(*source).or_default().insert(cluster);
        }
        *self.members.entry(cluster).or_insert(0) += 1;
    }

    /// Decide a row's home from the fraction of its sources each cluster
    /// is present in: the best cluster at or over `join_min` with room
    /// under `members_max`, else a cluster of its own. A row with no
    /// source founds nothing.
    pub fn decide(
        &self,
        sources: &[SourceId],
        join_min: f64,
        members_max: u32,
    ) -> Option<Assignment> {
        if sources.is_empty() {
            return None;
        }
        let mut shared: HashMap<ClusterId, u32> = HashMap::new();
        for source in sources {
            if let Some(clusters) = self.by_source.get(source) {
                for cluster in clusters {
                    *shared.entry(*cluster).or_insert(0) += 1;
                }
            }
        }
        let total = sources.len() as f64;
        let mut best: Option<(ClusterId, f64)> = None;
        for (cluster, count) in shared {
            let fraction = f64::from(count) / total;
            let full = self.members.get(&cluster).copied().unwrap_or(0) >= members_max;
            if fraction < join_min || full {
                continue;
            }
            let better = match best {
                None => true,
                Some((best_id, best_fraction)) => {
                    fraction > best_fraction || (fraction == best_fraction && cluster < best_id)
                }
            };
            if better {
                best = Some((cluster, fraction));
            }
        }
        Some(match best {
            Some((cluster, _)) => Assignment::Join(cluster),
            None => Assignment::Found,
        })
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

    #[test]
    fn rows_join_the_cluster_sharing_most_of_their_sources() {
        let mut presence = Presence::default();
        presence.learn(HashMap::from([
            (SourceId(1), vec![ClusterId(7)]),
            (SourceId(2), vec![ClusterId(7), ClusterId(9)]),
            (SourceId(3), vec![ClusterId(9)]),
        ]));
        presence.size(ClusterId(7), 2);
        presence.size(ClusterId(9), 2);
        let sources = [SourceId(1), SourceId(2), SourceId(4)];
        assert_eq!(
            presence.decide(&sources, 0.5, 64),
            Some(Assignment::Join(ClusterId(7)))
        );
        assert_eq!(presence.decide(&sources, 0.9, 64), Some(Assignment::Found));
        assert_eq!(
            presence.decide(&sources, 0.5, 2),
            Some(Assignment::Found),
            "a full cluster takes nobody"
        );
        assert_eq!(presence.decide(&[], 0.5, 64), None);
        presence.assign(ClusterId(11), &[SourceId(4), SourceId(5)]);
        assert_eq!(
            presence.decide(&[SourceId(4), SourceId(5)], 0.5, 64),
            Some(Assignment::Join(ClusterId(11)))
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
