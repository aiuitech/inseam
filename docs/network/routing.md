# Routing

How a node reads a source that another node stewards. The `routing` plugin (`crates/inseam-plugins/src/routing/`) provides the `routing` seam (`inseam-seams::routing`) and serves the `inseam/route/1` protocol on the transport, so the same code is the requester on one node and the steward — or the relay — on another. Query fan-out rides the same seam and protocol and has its own page, [discovery.md](discovery.md). The reasoning is in [design/network.md](../../design/network.md).

## Locating a host

`routing.locate(host)` reads the connections registry and the roster without dialing anyone:

| Answer | Meaning |
| --- | --- |
| `Local` | this node stewards the host; reads go to its own connection and never touch the network |
| `Remote(stewards)` | other nodes claim the host in the roster, in roster order; whether any is live is learned by trying |
| `Unknown` | nothing in the roster claims the host |

A stewardship claim by this node itself for a host its registry no longer holds is stale, not remote: the registry is the truth about what this node serves.

## A routed read

`read_text`, `read_lines`, `read_bytes`, `describe`, and `expand` all follow one path for a `Remote` host:

1. Dial the roster's stewards directly, in roster order, at most four (`STEWARDS_TRIED_MAX`). The first answer wins.
2. When none answers, ask the peers this node holds a live session with to relay, at most four (`RELAY_PEERS_MAX`), skipping anyone already tried.
3. When nobody answers, the error is `Unreachable`, naming the host and every node tried — stewards and relays — so an owner sees which node to bring up.

A relay that receives a request for a host it does not steward forwards it the same way, with two rules that keep a request from wandering: it decrements the request's hop budget (`HOPS_MAX`, four, the requester's own hop included, so A→B→C is two) and appends itself to the request's `visited` list, and it never forwards to a node already in that list. The `visited` list is therefore bounded at five entries and never names a node twice; a request that breaks either bound is refused by name before it is served.

A steward's own typed answer — no such source, a range past the end, a fetch too large — crosses the wire as that error, distinct from a relay's "unreachable": a client never retries a refusal. `read_lines` checks its range before dialing and the steward checks it again on receipt. Each routed read is bounded end to end by `request_timeout_secs`; a candidate that runs past it is the next candidate's turn.

## What is never forwarded

A relay answers "unreachable through me" — and the requester moves on to its next candidate — instead of forwarding when:

- the request has no hop budget left;
- the relay is itself in the request's `visited` list (a loop);
- the relay's own `node` entry says `relays = false` ([identity.md](identity.md)).

Two things never cross a second hop at all. A malformed request — a hop budget past four, a `visited` list past five names or naming a node twice, an empty query, a bad line range, a body past 64 KiB — is refused by name before anything is served. And a query is never relayed: a fan-out reaches every target directly, and a relayed query would answer from the relay's own index and be counted twice.

## The wire: `inseam/route/1`

One request, one reply, per transport exchange. The request is JSON:

```json
{ "hops_remaining": 4, "visited": ["<node id>"], "body": { "op": "read_lines", "address": "inseam://…", "start": 5, "end": 9 } }
```

| `op` | Arguments | Served by |
| --- | --- | --- |
| `read_text` | `address` | the steward's connection |
| `read_lines` | `address`, `start`, `end` (1-based, inclusive) | the steward's connection, reading no further than `end` |
| `read_bytes` | `address` | the steward's connection, bounded at 32 MiB |
| `describe` | `address` | the steward's connection |
| `expand` | `address` | the steward's index, through the same rung the operations layer serves |
| `scan` | `address`, `start`, `end` | the steward's index and connection — the same checks, clamp, and media fallback as a local scan |
| `query` | `text`, `limit` (1..=50) | the receiving node's own index; never forwarded ([discovery.md](discovery.md)) |

The reply's first byte is a framing tag: `0` and the rest is a JSON `RouteResponse` — `{"kind": "text" \| "envelope" \| "expand" \| "scan" \| "query" \| "error", "value": …}`; `1` and the rest is the raw bytes of a successful `read_bytes`. Bytes ride raw because base64 inside JSON would inflate a maximal fetch by a third and past the transport's 40 MiB message bound ([transport.md](transport.md)). An `error` value carries `kind` (the `SeamError` variant in snake case: `unknown_source`, `nothing_to_scan`, `refused`, `unavailable`, `unreachable`, …) and `message`; the requester rebuilds the typed variants whose data it already holds (the address, the host) and carries the rest as a failure with the steward's sentence.

## Errors

What an owner sees when a read of a remote source fails, at the CLI or as the HTTP error kind:

| Error | Meaning |
| --- | --- |
| `no connection on this node stewards host …` (`unknown_host`) | the address is cataloged but no roster node claims to steward its host — every steward withdrew, or the node was expelled. Without the `routing` entry, every host this node does not steward reads this way |
| `no steward of host … answered (tried …)` (`unreachable`) | stewards and relays were tried and none answered; the sentence names the host and every node tried, so the owner sees which node to bring up |
| `node … is not admitted to this network` (`not_admitted`) | a dial was refused at the door: this node is not in the peer's roster yet (the peer has not synced since this node joined), or was expelled |
| the steward's own error | `no source at …`, a range past the end, a fetch past 32 MiB — the host's answer, carried across the wire typed and never retried |

## Configuration

The `routing` entry (`[routing]`, `deny_unknown_fields`):

| Field | Default | Meaning |
| --- | --- | --- |
| `request_timeout_secs` | `30` | how long one routed read may take end to end before the requester tries its next candidate; must be greater than zero |
| `fan_out` | `true` | whether queries fan out at all |
| `fan_out_timeout_ms` | `3000` | how long a fan-out waits for each node before reporting it as a straggler; must be greater than zero |
| `fan_out_nodes_max` | `8` | most nodes one query fans out to; held to the seam's bound of eight, and zero is refused (set `fan_out = false` to keep queries local) |

The entry injects `store`, `node`, `transport`, `roster`, `connections`, and `finder`, all required; the operations plugin injects `routing` optionally and keeps every local operation on a node composed without it.
