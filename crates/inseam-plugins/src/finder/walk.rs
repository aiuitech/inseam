//! The relevance walk: personalized PageRank over the seed-local relation
//! slice (`design/finder.md`), with edges weighted by relation kind and by
//! the row kind of the fragment mass flows into (`design/vocabulary.md`).
//! The walk is linear in its restart vector, so it runs over a matrix of
//! restart columns — one per seed channel under `explain` — and the
//! columns sum to the walk a single fused restart would produce; that is
//! what makes the ledger an exact decomposition rather than an estimate.

use std::collections::{BTreeMap, HashMap};

use inseam_kernel::fragment::{FragmentId, Relation};
use inseam_kernel::store::RowKind;

use super::config::RelationWeights;

/// Most restart columns one walk carries: one per seed channel.
pub const COLUMNS_MAX: usize = 8;

/// The slice as an adjacency structure: interned vertices, directed
/// weighted neighbours, and each vertex's total outgoing weight.
pub struct Graph {
    nodes: Vec<i64>,
    index: HashMap<i64, usize>,
    /// Per vertex, `(neighbour, weight of the edge out of this vertex)`.
    adjacency: Vec<Vec<(usize, f64)>>,
    out_weight: Vec<f64>,
}

impl Graph {
    /// Build the slice's graph. Every seed is a vertex even with no edge,
    /// so the restart vector has somewhere to land. `row_kinds` weighs the
    /// far end of each edge; absent, every row kind conducts at one.
    pub fn build(
        relations: &[Relation],
        weights: &RelationWeights,
        row_kinds: Option<&HashMap<FragmentId, RowKind>>,
        seeds: impl Iterator<Item = i64>,
    ) -> Self {
        let mut graph = Self {
            nodes: Vec::new(),
            index: HashMap::new(),
            adjacency: Vec::new(),
            out_weight: Vec::new(),
        };
        for id in seeds {
            graph.intern(id);
        }
        let mut merged: HashMap<(usize, usize), f64> = HashMap::new();
        for relation in relations {
            if relation.from == relation.to {
                continue;
            }
            let base = weights.weight(&relation.kind);
            if base <= 0.0 {
                continue;
            }
            let from = graph.intern(relation.from.0);
            let to = graph.intern(relation.to.0);
            let into_to = base * row_weight(weights, row_kinds, relation.to);
            let into_from = base * row_weight(weights, row_kinds, relation.from);
            *merged.entry((from, to)).or_insert(0.0) += into_to;
            *merged.entry((to, from)).or_insert(0.0) += into_from;
        }
        for ((from, to), weight) in merged {
            if weight <= 0.0 {
                continue;
            }
            graph.adjacency[from].push((to, weight));
            graph.out_weight[from] += weight;
        }
        graph
    }

    fn intern(&mut self, id: i64) -> usize {
        if let Some(index) = self.index.get(&id) {
            return *index;
        }
        let index = self.nodes.len();
        self.nodes.push(id);
        self.index.insert(id, index);
        self.adjacency.push(Vec::new());
        self.out_weight.push(0.0);
        index
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn index_of(&self, id: i64) -> Option<usize> {
        self.index.get(&id).copied()
    }

    pub fn id_at(&self, index: usize) -> i64 {
        self.nodes[index]
    }

    pub fn ids(&self) -> impl Iterator<Item = i64> + '_ {
        self.nodes.iter().copied()
    }

    /// Run the walk from `restarts` — `columns` restart vectors laid out
    /// vertex-major (`[vertex * columns + column]`), each summing to its
    /// share of one — for at most `iterations` rounds with an ε early exit.
    /// Returns the stationary mass in the same layout.
    pub fn walk(
        &self,
        restarts: &[f64],
        columns: usize,
        damping: f64,
        iterations: usize,
        epsilon: f64,
    ) -> Vec<f64> {
        assert!(columns >= 1);
        assert!(columns <= COLUMNS_MAX);
        assert_eq!(restarts.len(), self.len() * columns);
        assert!((0.0..1.0).contains(&damping));
        let n = self.len();
        let mut p = restarts.to_vec();
        let mut next = vec![0.0; n * columns];
        for _ in 0..iterations {
            next.iter_mut().for_each(|value| *value = 0.0);
            let mut dangling = vec![0.0; columns];
            for i in 0..n {
                let mass = &p[i * columns..(i + 1) * columns];
                if mass.iter().all(|m| *m == 0.0) {
                    continue;
                }
                if self.out_weight[i] == 0.0 {
                    for (c, m) in mass.iter().enumerate() {
                        dangling[c] += m;
                    }
                    continue;
                }
                for (j, weight) in &self.adjacency[i] {
                    let share = weight / self.out_weight[i];
                    for c in 0..columns {
                        next[j * columns + c] += share * mass[c];
                    }
                }
            }
            let mut delta = 0.0;
            for i in 0..n * columns {
                let c = i % columns;
                let value =
                    (1.0 - damping) * restarts[i] + damping * (next[i] + dangling[c] * restarts[i]);
                delta += (value - p[i]).abs();
                p[i] = value;
            }
            if delta < epsilon {
                break;
            }
        }
        p
    }

    /// The walk mass that arrives at `vertex` from each neighbour in one
    /// more step from `total` (the summed columns): `damping · p[j] ·
    /// w_ji / out[j]`, best first. This is what carried the mass, not a
    /// path — and it is the ledger's "by row" line.
    pub fn arrivals(&self, total: &[f64], vertex: usize, damping: f64) -> Vec<(i64, f64)> {
        assert_eq!(total.len(), self.len());
        let mut arrivals: Vec<(i64, f64)> = Vec::new();
        for (j, neighbours) in self.adjacency.iter().enumerate() {
            if total[j] == 0.0 || self.out_weight[j] == 0.0 {
                continue;
            }
            for (target, weight) in neighbours {
                if *target != vertex {
                    continue;
                }
                let mass = damping * total[j] * weight / self.out_weight[j];
                if mass > 0.0 {
                    arrivals.push((self.nodes[j], mass));
                }
            }
        }
        arrivals.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        arrivals
    }

    /// [`Self::arrivals`] summed by the neighbours' row kinds.
    pub fn arrivals_by_kind(
        &self,
        total: &[f64],
        vertex: usize,
        damping: f64,
        row_kinds: &HashMap<FragmentId, RowKind>,
    ) -> BTreeMap<RowKind, f64> {
        let mut by_kind = BTreeMap::new();
        for (id, mass) in self.arrivals(total, vertex, damping) {
            let kind = row_kinds
                .get(&FragmentId(id))
                .copied()
                .unwrap_or(RowKind::Other);
            *by_kind.entry(kind).or_insert(0.0) += mass;
        }
        by_kind
    }
}

fn row_weight(
    weights: &RelationWeights,
    row_kinds: Option<&HashMap<FragmentId, RowKind>>,
    id: FragmentId,
) -> f64 {
    match row_kinds {
        None => 1.0,
        Some(kinds) => weights.row_weight(kinds.get(&id).copied().unwrap_or(RowKind::Other)),
    }
}

/// Sum the columns of a vertex-major matrix into one value per vertex.
pub fn sum_columns(matrix: &[f64], columns: usize) -> Vec<f64> {
    assert!(columns >= 1);
    assert_eq!(matrix.len() % columns, 0);
    matrix
        .chunks_exact(columns)
        .map(|row| row.iter().sum())
        .collect()
}

/// Undirected weighted edges from the relation graph, weights by kind, a
/// pair's parallel edges summed. The shape the property tests and the
/// older single-restart walk use.
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

/// Personalized PageRank by power iteration over undirected weighted
/// edges: the one-column walk. Seeds are the restart distribution
/// (normalized inside); walks continue with probability `damping`;
/// dangling mass restarts at the seeds. Returns a distribution over every
/// node walks can reach.
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
    let mut graph = Graph {
        nodes: Vec::new(),
        index: HashMap::new(),
        adjacency: Vec::new(),
        out_weight: Vec::new(),
    };
    for id in seeds.keys() {
        graph.intern(*id);
    }
    for (a, b, w) in edges {
        let ia = graph.intern(*a);
        let ib = graph.intern(*b);
        graph.adjacency[ia].push((ib, *w));
        graph.adjacency[ib].push((ia, *w));
        graph.out_weight[ia] += *w;
        graph.out_weight[ib] += *w;
    }
    let mut restart = vec![0.0; graph.len()];
    for (id, score) in seeds {
        restart[graph.index[id]] = score / total;
    }
    let p = graph.walk(&restart, 1, damping, iterations, epsilon);
    graph
        .ids()
        .enumerate()
        .filter(|(i, _)| p[*i] > 0.0)
        .map(|(i, id)| (id, p[i]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use inseam_kernel::fragment::RelationKind;

    fn kind(name: &str) -> RelationKind {
        RelationKind::new(name).expect("valid kind")
    }

    #[test]
    fn columns_sum_to_the_single_restart_walk() {
        let relations = vec![
            Relation::new(FragmentId(1), kind("contains"), FragmentId(2)),
            Relation::new(FragmentId(2), kind("mentions"), FragmentId(3)),
            Relation::new(FragmentId(4), kind("contains"), FragmentId(3)),
        ];
        let weights = RelationWeights::default();
        let graph = Graph::build(&relations, &weights, None, [1, 4].into_iter());
        let n = graph.len();
        let one = graph.index_of(1).expect("seed");
        let four = graph.index_of(4).expect("seed");
        let mut single = vec![0.0; n];
        single[one] = 0.75;
        single[four] = 0.25;
        let mut split = vec![0.0; n * 2];
        split[one * 2] = 0.75;
        split[four * 2 + 1] = 0.25;
        let p_single = graph.walk(&single, 1, 0.5, 30, 1e-12);
        let p_split = sum_columns(&graph.walk(&split, 2, 0.5, 30, 1e-12), 2);
        for (a, b) in p_single.iter().zip(&p_split) {
            assert!((a - b).abs() < 1e-9, "{a} vs {b}");
        }
    }

    #[test]
    fn row_kind_weights_steer_mass_toward_stronger_kinds() {
        let relations = vec![
            Relation::new(FragmentId(1), kind("mentions"), FragmentId(2)),
            Relation::new(FragmentId(1), kind("mentions"), FragmentId(3)),
        ];
        let weights = RelationWeights::default();
        let kinds: HashMap<FragmentId, RowKind> = HashMap::from([
            (FragmentId(1), RowKind::Prose),
            (FragmentId(2), RowKind::Identifier),
            (FragmentId(3), RowKind::Alias),
        ]);
        let graph = Graph::build(&relations, &weights, Some(&kinds), [1].into_iter());
        let mut restart = vec![0.0; graph.len()];
        restart[graph.index_of(1).expect("seed")] = 1.0;
        let p = graph.walk(&restart, 1, 0.5, 30, 1e-12);
        let identifier = p[graph.index_of(2).expect("present")];
        let alias = p[graph.index_of(3).expect("present")];
        assert!(
            identifier > alias,
            "identifier {identifier} vs alias {alias}"
        );
        let arrivals = graph.arrivals(&p, graph.index_of(1).expect("seed"), 0.5);
        assert_eq!(arrivals.len(), 2, "both neighbours hand mass back");
        let by_kind = graph.arrivals_by_kind(&p, graph.index_of(1).expect("seed"), 0.5, &kinds);
        assert!(by_kind[&RowKind::Identifier] > 0.0);
        assert!(by_kind[&RowKind::Alias] > 0.0);
    }

    #[test]
    fn the_legacy_walk_keeps_its_shape() {
        let seeds = HashMap::from([(1, 1.0)]);
        let edges = vec![(1, 2, 1.0), (2, 3, 1.0), (4, 5, 1.0)];
        let p = personalized_pagerank(&seeds, &edges, 0.5, 20, 1e-9);
        assert!(p[&2] > p[&3], "one hop beats two hops");
        assert!(p[&3] > 0.0, "two hops still reached");
        assert!(!p.contains_key(&4), "disconnected components get nothing");
        let none = personalized_pagerank(&seeds, &[], 0.5, 20, 1e-9);
        assert!((none[&1] - 1.0).abs() < 1e-9);
        assert!(personalized_pagerank(&HashMap::new(), &edges, 0.5, 10, 1e-9).is_empty());
    }

    #[test]
    fn weighted_edges_merge_parallel_and_drop_self_loops() {
        let relations = vec![
            Relation::new(FragmentId(1), kind("contains"), FragmentId(2)),
            Relation::new(FragmentId(2), kind("mentions"), FragmentId(1)),
            Relation::new(FragmentId(3), kind("contains"), FragmentId(3)),
        ];
        let edges = weighted_edges(&relations, &RelationWeights::default());
        assert_eq!(edges.len(), 1);
        let (a, b, w) = edges[0];
        assert_eq!((a, b), (1, 2));
        assert!((w - 1.8).abs() < 1e-9, "contains 1.0 + mentions 0.8");
    }
}
