# Sync

Sync is how every node converges on the same catalog and the same roster. Each node keeps an append-only log of the records it originates, every node holds a copy of every log it has seen, and an exchange between two nodes ships each side the entries the other lacks. The log lives in the kernel store beside the catalog (`crates/inseam-kernel/src/store/replication.rs`); the `sync` plugin (`crates/inseam-plugins/src/sync/`) provides the `sync` seam (`inseam-seams::sync`) and moves entries over the transport as `inseam/sync/1` ([design/address-sync.md](../../design/address-sync.md)).

## The log

One table, `sync_log`, holds every entry this node has: the **origin** (the node that wrote it), an **epoch**, a **sequence**, the record's **key**, a tombstone flag, and the record as JSON ([../indexing/storage.md](../indexing/storage.md)). The store never learns this node's own id — identity lives above the kernel, in the `node` plugin — so the local log is filed under an empty origin and the sync seam maps it to the node id at the boundary.

- **Sequence.** Each origin numbers its entries from one and never reuses a number within an epoch. The high-water mark is kept in `meta` (`log_seq`), not derived from the table, because compaction may remove the highest row.
- **Epoch.** A log is named by its origin *and* an epoch (`meta.log_epoch`). The store mints one when it is created and again whenever it is rebuilt — a schema-version bump, a replaced data directory — from the clock in nanoseconds, or the superseded epoch plus one if the clock went backwards. Without it, a node whose store was rebuilt would restart at sequence one, and every peer holding its old, higher sequences would ignore the new log forever. A higher epoch from one origin supersedes the older one wholesale: on arrival, the peer purges everything it held from that origin and starts over.
- **Version vector.** A node's whole knowledge is the highest (epoch, sequence) it holds per origin. That is what one node tells another so the other can ship exactly the missing suffix.
- **Keys.** `source:<address>`, `node:<id>`, `host:<id>`, `stewardship:<node>/<host>`, `expulsion:<node>`. Within one origin's log the latest entry under a key is the whole truth: a source and its removal share a key, as do a stewardship and its withdrawal.
- **Compaction on append.** Appending an entry deletes every earlier entry under the same key from the same origin and epoch — for a local write and for a peer's entry alike. The log holds one live entry per key, and a **tombstone** (`SourceGone`, `StewardshipWithdrawn`) stays until a later entry under its key replaces it, so a removal keeps reaching nodes that have not seen it. Sequence numbers survive compaction, so a peer holding an older vector still receives exactly the entries that supersede what it missed.

## What gets logged

The catalog writes log themselves. A sweep re-upserts every source it sees each run, so the store logs a `Source` entry only when it is news: a new row, a row taken over from a remote steward, a changed byte size, or an envelope that changed in anything but `observed`. Deleting a source this node stewards logs a `SourceGone`. Roster records enter through `publish` ([roster.md](roster.md)). Seeing a source again logs nothing.

## Applying a peer's entries

A batch from a peer lands in one transaction, entry by entry:

- an echo of this node's own log, or an entry already held, is **skipped**;
- an entry out of bounds — a display name past 128 characters, more than 16 endpoints, an epoch or sequence the column cannot hold — is **refused** and not held;
- a node record about another node, or a stewardship (or withdrawal) from anyone but the steward, is held, so the vector stays honest, but never applied;
- everything else is held (compacting its key) and applied: source entries into `sources` with `origin` set to the steward, roster entries into `roster_nodes`, `roster_hosts`, `roster_stewardships`, and `roster_expulsions`, the last applied winning.

A remote source entry updates only a row that is itself remote: **a row this node stewards is never overwritten by a peer's copy**, and a remote tombstone deletes only the row its own origin stewards. When this node starts stewarding an address a peer had published, the local write takes the row over and logs it. Remote rows carry no fragments and are not in the search index ([discovery.md](discovery.md#what-a-remote-row-is)).

An expulsion applied from a peer purges the expelled node's log, rows, and roster rows — except when the expelled node is this node (the store keeps serving; the network refuses it) or the entry's own origin (purging it would erase the order, and the peer would ship it again forever).

## The exchange

`inseam/sync/1` is one round trip that moves knowledge both ways. The request carries the requester's version vector and the entries it knows the responder lacks — none on the first round; the responder applies them and answers with its vector afterwards and the entries the requester's vector lacked. Each side then knows the other's vector, so the next round ships the next suffix with no second negotiation. An exchange runs rounds until neither side has news, or the responder made no progress (its vector stood still and it shipped nothing), or 64 rounds are spent — at 2,000 entries per batch per direction, 128,000 entries each way, after which the next scheduled round continues it. Bodies are JSON. Entries of origins neither side has met directly ride along too, which is what makes propagation transitive.

Every request runs under `request_timeout_secs`, once at the transport and once around it. A batch that applied a roster record fires `RosterChanged`, so the roster refreshes its admission view.

## The loop

After `initial_delay_secs`, and every `interval_secs` after, the plugin runs one **round**: an exchange with every roster node it can dial (a node record with endpoints, not this node, not expelled) plus every peer holding a live session with it that it could not dial — the backbone reaching an outbound-only laptop through the laptop's own connection. `always_on` nodes go first, so a round cut short by the bound still reaches the backbone; at most 8 exchanges run at once; a failed exchange lands on that peer's status, never on the round. `sync_with` is the other entry point: one exchange with one named peer, which is how `inseam network join` presents its invitation.

| Field | Default | Meaning |
| --- | --- | --- |
| `interval_secs` | `60` | seconds between rounds; at least 1 |
| `initial_delay_secs` | `2` | seconds after mount before the first round, so the transport and the roster settle |
| `peers_per_round_max` | `32` | most peers one round exchanges with; at least 1, clamped to 32 |
| `request_timeout_secs` | `30` | how long one request may take before the peer is given up on; at least 1 |

The entry injects `store`, `node`, `transport`, and `roster`, all required, and provides `sync` with `interval_secs` as a fact.

## What the owner sees

`inseam network sync` runs a round now and prints the network afterwards ([../cli.md](../cli.md)). What it shows per peer is what the last attempt learned — `live` (the last exchange succeeded, or a session is open), the date of the last success, the last error, and entries received and sent in all — never a synced fact. At most 1,024 peers are remembered, the one tried longest ago forgotten first. `inseam status` counts `remote_sources`; `inseam catalog` shows a remote row's `origin`.

## What leaves a host

Everything in the log replicates to every node in the network, a hosted node included: addresses and envelopes (types, length, timestamps, the title hint, trust properties, the content digest) with each source's byte size; node records (display name, endpoints, capabilities); host records (kind, display name); stewardships (capabilities, and the steward's configured folder paths). Content never does — no bytes, no fragments, no summaries, no vectors. Neither do credentials, the node key, sessions, or open invitations. Entries are not signed: the transport authenticates both ends and the network is one trust domain, so a node relaying another's log is trusted to relay it unchanged.

## Bounds

| Bound | Value |
| --- | --- |
| entries per batch | 2,000 |
| rounds per exchange | 64 |
| peers per round | 32 |
| exchanges at once | 8 |
| rows per roster listing | 10,000 |
| peers remembered | 1,024 |
