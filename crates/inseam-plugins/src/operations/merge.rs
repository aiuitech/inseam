//! Merging a query's local results with what the fan-out brought back
//! (`design/discovery.md`, `design/finder.md`). Scores from differently
//! profiled indexes do not compare, so the merge fuses by rank —
//! reciprocal rank fusion with the finder's own `k` — across the local
//! list and each remote list, keeps one copy per address (the local one
//! when this node has it, else the best-ranked remote), and then collapses
//! digest-equal copies into `replicas` exactly as one node's finder does.
//! A remote node that failed contributes an empty list and a summary
//! saying why; it never fails the query.

use std::collections::HashMap;

use inseam_kernel::address::Address;
use inseam_seams::operations::{FanOutSummary, QueryResult};
use inseam_seams::routing::{FAN_OUT_NODES_MAX, FanOutReply};

use super::ladder::{QUERY_LIMIT_MAX, round3};
use crate::finder::{RRF_K_DEFAULT, collapse_by_digest_by, rrf_fuse};

/// Most ranked lists one merge fuses: the local list and one per fanned-out
/// node. Bounds the interning below.
const LISTS_MAX: usize = FAN_OUT_NODES_MAX + 1;
/// Most distinct addresses one merge can see: every list full of distinct
/// addresses.
const CANDIDATES_MAX: usize = LISTS_MAX * QUERY_LIMIT_MAX;

/// The merged results and one summary per fanned-out node.
pub(crate) struct Merged {
    pub results: Vec<QueryResult>,
    pub remote: Vec<FanOutSummary>,
}

/// One address's representative among the lists it appeared in.
struct Candidate {
    result: QueryResult,
    /// The rank the representative held in its list; a lower rank from
    /// another remote list replaces it. Local copies are never replaced.
    rank: usize,
    is_local: bool,
}

/// Merge the local list with every remote list. With nothing to merge —
/// no remote node answered with results — the local list stands
/// untouched, scores and all, so a node with no network answers exactly as
/// it always has.
pub(crate) fn merge(
    mut local: Vec<QueryResult>,
    replies: Vec<FanOutReply>,
    limit: usize,
) -> Merged {
    assert!(limit >= 1);
    assert!(
        replies.len() <= FAN_OUT_NODES_MAX,
        "fan-out is bounded before the merge"
    );
    let remote: Vec<FanOutSummary> = replies.iter().map(summary_of).collect();
    let any_remote_results = replies.iter().any(|reply| !reply.results.is_empty());
    if !any_remote_results {
        local.truncate(limit);
        return Merged {
            results: local,
            remote,
        };
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut ids: HashMap<Address, i64> = HashMap::new();
    let mut lists: Vec<Vec<i64>> = Vec::with_capacity(LISTS_MAX);
    lists.push(intern_list(&mut candidates, &mut ids, local, None));
    for reply in replies {
        let mut results = reply.results;
        // A peer's list is bounded by the limit it was asked for; a peer
        // that sends more is held to the same bound, never trusted past it.
        results.truncate(QUERY_LIMIT_MAX);
        for result in &mut results {
            result.via = Some(reply.node);
        }
        lists.push(intern_list(
            &mut candidates,
            &mut ids,
            results,
            Some(reply.node),
        ));
    }
    assert!(lists.len() <= LISTS_MAX);
    assert!(candidates.len() <= CANDIDATES_MAX);
    let ranked = fuse(candidates, &lists);
    let mut results = collapse_by_digest_by(
        ranked,
        |result| result.envelope.content_digest,
        absorb_replica,
    );
    results.truncate(limit);
    assert!(results.len() <= limit);
    Merged { results, remote }
}

fn summary_of(reply: &FanOutReply) -> FanOutSummary {
    FanOutSummary {
        node: reply.node,
        results: u32::try_from(reply.results.len()).unwrap_or(u32::MAX),
        elapsed_ms: reply.elapsed_ms,
        error: reply.error.clone(),
    }
}

/// Intern one best-first list: each address gets a stable id on first
/// sight, and its representative is replaced only by a better-ranked
/// remote copy — never when the representative is local. `via` is `None`
/// exactly for the local list.
fn intern_list(
    candidates: &mut Vec<Candidate>,
    ids: &mut HashMap<Address, i64>,
    results: Vec<QueryResult>,
    via: Option<inseam_kernel::network::NodeId>,
) -> Vec<i64> {
    let is_local = via.is_none();
    let mut list: Vec<i64> = Vec::with_capacity(results.len());
    for (rank, result) in results.into_iter().enumerate() {
        assert_eq!(result.via, via, "a list carries one provenance");
        let id = match ids.get(&result.address) {
            Some(&id) => {
                let index = usize::try_from(id).expect("ids are candidate indexes");
                let held = &mut candidates[index];
                if !held.is_local && (is_local || rank < held.rank) {
                    held.result = result;
                    held.rank = rank;
                    held.is_local = is_local;
                }
                id
            }
            None => {
                let id = i64::try_from(candidates.len()).expect("candidates are bounded");
                ids.insert(result.address.clone(), id);
                candidates.push(Candidate {
                    result,
                    rank,
                    is_local,
                });
                id
            }
        };
        list.push(id);
    }
    list
}

/// Reciprocal rank fusion over the interned lists, best-first, scores
/// renormalized so the top result is 1.0 like any query's. Ties break by
/// id, and ids are assigned local list first, so a local copy outranks a
/// remote one that fused to the same score.
fn fuse(candidates: Vec<Candidate>, lists: &[Vec<i64>]) -> Vec<QueryResult> {
    let borrowed: Vec<&[i64]> = lists.iter().map(Vec::as_slice).collect();
    let fused = rrf_fuse(&borrowed, RRF_K_DEFAULT);
    let mut order: Vec<(i64, f64)> = fused.into_iter().collect();
    order.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let top = order.first().map_or(1.0, |(_, score)| *score);
    assert!(top > 0.0, "a fused score is a sum of positive terms");
    let mut slots: Vec<Option<QueryResult>> = candidates
        .into_iter()
        .map(|candidate| Some(candidate.result))
        .collect();
    let mut ranked = Vec::with_capacity(order.len());
    for (id, score) in order {
        let index = usize::try_from(id).expect("ids are candidate indexes");
        let Some(mut result) = slots[index].take() else {
            continue;
        };
        result.score = round3(score / top);
        ranked.push(result);
    }
    assert!(
        slots.iter().all(Option::is_none),
        "every candidate was ranked once"
    );
    ranked
}

/// Fold a digest-equal copy into the result that outranked it: its address
/// and its own replicas join the kept result's, each address once.
fn absorb_replica(kept: &mut QueryResult, duplicate: QueryResult) {
    let mut incoming = Vec::with_capacity(1 + duplicate.replicas.len());
    incoming.push(duplicate.address);
    incoming.extend(duplicate.replicas);
    for address in incoming {
        if address == kept.address || kept.replicas.contains(&address) {
            continue;
        }
        kept.replicas.push(address);
    }
}

#[cfg(test)]
mod tests {
    use inseam_kernel::address::{ContentDigest, ContentLength};
    use inseam_kernel::network::NodeId;
    use inseam_seams::operations::EnvelopeView;

    use super::*;

    fn node(byte: u8) -> NodeId {
        NodeId::from_bytes([byte; 32])
    }

    fn result(address: &str, digest: Option<&[u8]>) -> QueryResult {
        QueryResult {
            address: address.parse().expect("valid address"),
            score: 1.0,
            summary: None,
            envelope: EnvelopeView {
                source_type: "file".to_string(),
                content_type: "text/plain".to_string(),
                length: ContentLength::Lines(1),
                created: None,
                modified: None,
                title: None,
                content_digest: digest.map(ContentDigest::of_bytes),
            },
            hints: Vec::new(),
            replicas: Vec::new(),
            via: None,
        }
    }

    fn reply(byte: u8, results: Vec<QueryResult>, error: Option<&str>) -> FanOutReply {
        FanOutReply {
            node: node(byte),
            results,
            elapsed_ms: 5,
            error: error.map(str::to_string),
        }
    }

    fn addresses(results: &[QueryResult]) -> Vec<String> {
        results.iter().map(|r| r.address.to_string()).collect()
    }

    #[test]
    fn without_remote_results_the_local_list_stands_untouched() {
        let mut local = vec![
            result("inseam://fs-a/one", None),
            result("inseam://fs-a/two", None),
        ];
        local[1].score = 0.4;
        let merged = merge(local, vec![reply(2, Vec::new(), Some("timed out"))], 8);
        assert_eq!(
            addresses(&merged.results),
            ["inseam://fs-a/one", "inseam://fs-a/two"]
        );
        assert_eq!(
            merged.results[1].score, 0.4,
            "no fusion rescored the local list"
        );
        assert_eq!(merged.remote.len(), 1);
        assert_eq!(merged.remote[0].error.as_deref(), Some("timed out"));
        assert_eq!(merged.remote[0].results, 0);
    }

    #[test]
    fn rank_fusion_rewards_an_address_both_lists_rank() {
        let local = vec![
            result("inseam://fs-a/only-local", None),
            result("inseam://fs-a/shared", None),
        ];
        let remote = vec![
            result("inseam://fs-a/shared", None),
            result("inseam://fs-b/only-remote", None),
        ];
        let merged = merge(local, vec![reply(2, remote, None)], 8);
        assert_eq!(
            addresses(&merged.results),
            [
                "inseam://fs-a/shared",
                "inseam://fs-a/only-local",
                "inseam://fs-b/only-remote"
            ],
            "rank 2 + rank 1 beats a lone rank 1, and the local lone rank 1 beats the remote one by id"
        );
        assert_eq!(
            merged.results[0].score, 1.0,
            "the top result is renormalized to 1.0"
        );
        assert!(merged.results[1].score < 1.0);
        assert_eq!(merged.remote[0].results, 2);
    }

    #[test]
    fn a_shared_address_keeps_its_local_copy() {
        let mut local = vec![result("inseam://fs-a/shared", None)];
        local[0].summary = Some("local summary".to_string());
        let mut remote = vec![result("inseam://fs-a/shared", None)];
        remote[0].summary = Some("remote summary".to_string());
        let merged = merge(local, vec![reply(2, remote, None)], 8);
        assert_eq!(merged.results.len(), 1);
        assert_eq!(merged.results[0].summary.as_deref(), Some("local summary"));
        assert_eq!(merged.results[0].via, None);
    }

    #[test]
    fn an_address_only_remotes_hold_keeps_its_best_ranked_copy() {
        let local = Vec::new();
        let mut second_place = result("inseam://fs-b/doc", None);
        second_place.summary = Some("ranked second on node 2".to_string());
        let mut first_place = result("inseam://fs-b/doc", None);
        first_place.summary = Some("ranked first on node 3".to_string());
        let merged = merge(
            local,
            vec![
                reply(
                    2,
                    vec![result("inseam://fs-b/other", None), second_place],
                    None,
                ),
                reply(3, vec![first_place], None),
            ],
            8,
        );
        let doc = merged
            .results
            .iter()
            .find(|r| r.address.to_string() == "inseam://fs-b/doc")
            .expect("merged");
        assert_eq!(doc.summary.as_deref(), Some("ranked first on node 3"));
        assert_eq!(doc.via, Some(node(3)));
    }

    #[test]
    fn digest_equal_copies_collapse_into_replicas_across_nodes() {
        let local = vec![result("inseam://fs-a/notes.md", Some(b"same bytes"))];
        let remote = vec![
            result("inseam://drive-x/notes.md", Some(b"same bytes")),
            result("inseam://drive-x/other.md", Some(b"different")),
        ];
        let merged = merge(local, vec![reply(2, remote, None)], 8);
        assert_eq!(
            addresses(&merged.results),
            ["inseam://fs-a/notes.md", "inseam://drive-x/other.md"]
        );
        assert_eq!(
            merged.results[0]
                .replicas
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["inseam://drive-x/notes.md"]
        );
        assert!(merged.results[1].replicas.is_empty());
    }

    #[test]
    fn results_without_a_digest_never_collapse() {
        let local = vec![result("inseam://fs-a/a", None)];
        let remote = vec![result("inseam://fs-b/b", None)];
        let merged = merge(local, vec![reply(2, remote, None)], 8);
        assert_eq!(merged.results.len(), 2);
    }

    #[test]
    fn a_failed_node_is_summarized_while_the_rest_merge() {
        let local = vec![result("inseam://fs-a/a", None)];
        let merged = merge(
            local,
            vec![
                reply(2, Vec::new(), Some("timed out after 3000 ms")),
                reply(3, vec![result("inseam://fs-c/c", None)], None),
            ],
            8,
        );
        assert_eq!(merged.results.len(), 2);
        assert_eq!(merged.remote.len(), 2);
        assert_eq!(merged.remote[0].node, node(2));
        assert_eq!(
            merged.remote[0].error.as_deref(),
            Some("timed out after 3000 ms")
        );
        assert_eq!(merged.remote[1].results, 1);
        assert_eq!(merged.remote[1].error, None);
    }

    #[test]
    fn the_merge_is_truncated_to_the_limit() {
        let local = vec![
            result("inseam://fs-a/a", None),
            result("inseam://fs-a/b", None),
        ];
        let remote = vec![
            result("inseam://fs-b/c", None),
            result("inseam://fs-b/d", None),
        ];
        let merged = merge(local, vec![reply(2, remote, None)], 3);
        assert_eq!(merged.results.len(), 3);
    }
}
