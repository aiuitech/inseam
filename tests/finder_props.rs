//! Property tests for the Finder's numeric core: fusion and propagation must
//! behave for any graph, not just the ones in example tests.

use std::collections::HashMap;

use inseam::finder::{personalized_pagerank, rrf_fuse};
use proptest::prelude::*;

fn arb_edges() -> impl Strategy<Value = Vec<(i64, i64, f64)>> {
    prop::collection::vec(
        (0i64..40, 0i64..40, 0.1f64..2.0).prop_map(|(a, b, w)| (a, b, w)),
        0..80,
    )
}

fn arb_seeds() -> impl Strategy<Value = HashMap<i64, f64>> {
    prop::collection::hash_map(0i64..40, 0.01f64..1.0, 1..10)
}

proptest! {
    /// The walk distribution is a distribution: non-negative, summing to ~1.
    #[test]
    fn ppr_yields_a_probability_distribution(
        seeds in arb_seeds(),
        edges in arb_edges(),
        damping in 0.05f64..0.95,
    ) {
        let edges: Vec<_> = edges.into_iter().filter(|(a, b, _)| a != b).collect();
        let p = personalized_pagerank(&seeds, &edges, damping, 30, 1e-9);
        for score in p.values() {
            prop_assert!(*score >= 0.0);
        }
        let total: f64 = p.values().sum();
        prop_assert!((total - 1.0).abs() < 1e-6, "sums to 1, got {total}");
    }

    /// Every seed keeps standing: propagation boosts, it never gates
    /// (`design/finder.md`). The restart term guarantees each seed at least
    /// (1 - damping) of its normalized seed mass.
    #[test]
    fn ppr_seed_floor_holds(
        seeds in arb_seeds(),
        edges in arb_edges(),
        damping in 0.05f64..0.95,
    ) {
        let edges: Vec<_> = edges.into_iter().filter(|(a, b, _)| a != b).collect();
        let total: f64 = seeds.values().sum();
        let p = personalized_pagerank(&seeds, &edges, damping, 30, 1e-12);
        for (id, seed) in &seeds {
            let normalized = seed / total;
            let floor = (1.0 - damping) * normalized;
            let got = p.get(id).copied().unwrap_or(0.0);
            prop_assert!(
                got >= floor - 1e-9,
                "seed {id}: got {got}, floor {floor}"
            );
        }
    }

    #[test]
    fn ppr_is_deterministic(
        seeds in arb_seeds(),
        edges in arb_edges(),
    ) {
        let edges: Vec<_> = edges.into_iter().filter(|(a, b, _)| a != b).collect();
        let a = personalized_pagerank(&seeds, &edges, 0.5, 25, 1e-9);
        let b = personalized_pagerank(&seeds, &edges, 0.5, 25, 1e-9);
        prop_assert_eq!(a, b);
    }

    /// RRF: presence in more lists never hurts, and scores stay positive and
    /// bounded by lists/k.
    #[test]
    fn rrf_scores_are_positive_and_bounded(
        list_a in prop::collection::vec(0i64..100, 0..30),
        list_b in prop::collection::vec(0i64..100, 0..30),
        k in 1.0f64..120.0,
    ) {
        let fused = rrf_fuse(&[&list_a, &list_b], k);
        for (id, score) in &fused {
            prop_assert!(*score > 0.0);
            prop_assert!(*score <= 2.0 / (k + 1.0) + 1e-9, "id {id} score {score}");
        }
        // An id present in both lists scores at least what it would from one.
        for id in list_a.iter().filter(|id| list_b.contains(id)) {
            let solo = rrf_fuse(&[&list_a], k);
            if let (Some(both), Some(one)) = (fused.get(id), solo.get(id)) {
                prop_assert!(both >= one);
            }
        }
    }
}
