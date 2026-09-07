# Discovery Across Nodes

A query runs against this node's index and, when the `routing` entry is mounted with `fan_out = true`, against other nodes' indexes at the same time; the answers merge into one ranked list. The fan-out is the `routing` seam's `fan_out` (`crates/inseam-plugins/src/routing/`); the merge belongs to the operations plugin (`crates/inseam-plugins/src/operations/merge.rs`) ([design/discovery.md](../../design/discovery.md), [design/finder.md](../../design/finder.md)).

## Targets

Every other roster node advertising `deep_index`, ordered live-session first, then `always_on`, then roster order, and cut to `fan_out_nodes_max` (default and ceiling 8). Each target gets one `inseam/route/1` `query` request — the text and the limit, clamped to 1..=50 — with no hop budget, because a query is never forwarded: the target answers from its own index or not at all. A node that is not `deep_index` (a phone with an envelope-only index) is never asked, and a node with a live session is asked first because it is known to be reachable right now.

## Timeouts

Each exchange is bounded by `fan_out_timeout_ms` (default 3000) twice: the timeout is handed to the transport, and a timer of the plugin's own enforces it again, so a query answers on time even if a transport misbehaves. A node that times out, refuses, or answers badly is one reply carrying an `error`; the query still succeeds. Nothing is retried, and nothing is dialed with `fan_out = false`.

## The merge

With no remote results the local list stands untouched, scores and all: a node with no network answers exactly as it always has. Otherwise:

1. **Rank fusion.** The local list and each remote list are fused by reciprocal rank with the finder's own `k = 60` — each address scores the sum of `1 / (k + rank)` over the lists it appears in. Scores from differently profiled indexes never compare directly; ranks do.
2. **One copy per address.** The local copy wins when this node ranked the address; otherwise the copy that ranked best on any remote node, and its summary and hints come with it. A tie between a local and a remote copy at the same fused score goes to the local one.
3. **Replicas.** Digest-equal copies collapse into one result listing the rest under `replicas`, exactly as one node's results do — the local file and its Drive twin, indexed on two nodes, rank once. Results without a digest never collapse.
4. The list is cut to the limit and the top result renormalized to `1.0`.

## What a result carries

- `via` — the node whose index produced a result, when a fan-out did; absent for this node's own results. `inseam query` marks such a result `via <node>`.
- `meta.remote` — one entry per node the query fanned out to: `node`, `results` it contributed before the merge, `elapsed_ms`, and `error` when it contributed none; absent when the query fanned out to nobody. `inseam query` prints one footer line per node; `--json` carries the objects ([../finder/operations.md](../finder/operations.md#query-meta)).

A result served `via` another node is fetched, expanded, and scanned through routing like any remote source ([routing.md](routing.md)); `expand` is answered from the steward's index, since that is where the fragments are.

## What a remote row is

The catalog holds every address in the network, but a row learned from a peer's log is envelope-only: it has no fragments, no summary, no vectors, and it is not in this node's search index. A query finds remote content only through fan-out. A node with `fan_out = false`, or one whose peers are all offline, finds only what it indexed itself, even though `inseam catalog` lists everything. Indexing the envelopes of remote rows locally, so an offline node could still find them by title and hints, is the next step ([design/discovery.md](../../design/discovery.md)).
