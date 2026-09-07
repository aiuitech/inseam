# Roster

The roster is the network's description of itself: which nodes exist and how to dial them, which hosts exist, and who stewards what. The `roster` plugin (`crates/inseam-plugins/src/roster/`) provides the `roster` seam (`inseam-seams::roster`): it is the only writer of this node's own roster records, the typed view over the roster tables the store materializes ([sync.md](sync.md)), and the transport's admission policy. The records replicate exactly as catalog entries do — same log, same exchange ([design/roster.md](../../design/roster.md)).

## Three record kinds

| Record | Key | Authored by | Carries |
| --- | --- | --- | --- |
| node | `node:<id>` | the node it describes, only | display name, endpoints, capabilities (`always_on`, `deep_index`, `relays`) |
| host | `host:<host id>` | any steward of the host | kind (`fs`, `gmail`, …) and display name |
| stewardship | `stewardship:<node>/<host>` | the steward, only | the connection's capabilities (`enumerates`, `change_feed`, `writable`) and its configured `roots`; never credentials |

A node record from any origin but the node it describes is held but never applied; so is a stewardship, or its withdrawal, from anyone but the steward. Host records collide by design: two stewards of one Gmail account derive the same host id and both publish a host record for it, the later entry wins, and the network sees one host with two stewards. A stewardship withdrawal is a tombstone under the stewardship key. A host whose every steward withdrew stays known with no stewards, and a read of it is `unreachable`, not `unknown host` ([routing.md](routing.md#errors)).

Two more records ride the same log: an **expulsion** (`expulsion:<node>`, below) and the catalog's own source entries ([sync.md](sync.md)).

## What this node publishes

At apply, before the seam is provided, the plugin publishes the node record with the transport's current endpoints, then one host record and one stewardship record per registration in the connections registry ([../indexing/connections.md](../indexing/connections.md)). A reconciler task then keeps the log matching reality. It wakes on `ConnectionsChanged` (a Google grant authorized, a host withdrawn), on `RosterChanged`, and on a timer; each pass publishes what the registry holds that the log does not, withdraws what the registry no longer holds, and refreshes the admission view. An unchanged host costs no entry. What was last published is remembered in memory only, so a restart republishes each record once; the log compacts per key, so that is one entry per record, not growth. At most 1,024 hosts per node.

## Endpoint rotation

Every pass compares the transport's endpoints with the last published record and republishes the node record when they differ — the relay coming up after boot, a direct address changing, a laptop moving networks. The poll interval is the entry's one setting:

| Field | Default | Meaning |
| --- | --- | --- |
| `endpoint_poll_secs` | `30` | seconds between timer-driven passes; a rotation is republished within this long; at least 1 |

The new record spreads on the next sync exchange over whatever connection the node can still make — outbound dialing works even when the inbound address has just died. `republish` on the seam forces one now.

## Admission

Who may connect is the roster's answer, given synchronously on the transport's accept path from an in-memory view refreshed after every pass and every `RosterChanged`. In order:

1. a node's own key is never admitted as a peer;
2. an **expelled** node is refused, before anything else, so a stale record can never readmit it;
3. a **roster member** — a node record held and not expelled — is admitted;
4. a peer admitted by invitation whose own node record has not arrived yet is admitted (the record arrives in the first exchange);
5. a stranger presenting an **open invitation token** is admitted as the token is redeemed;
6. everyone else is refused.

The peer learns only that it was refused; the reason goes to this node's log. `is_admitted` on the seam answers from the store, the authoritative pair to the cache the policy answers from.

## Invitations

`inseam network invite` mints one: this node's id and current endpoints, a fresh one-time token (32 random bytes, base64url), and an expiry **24 hours** out, rendered as one line, `inseam-invite:<base64url JSON>`. The owner carries that line to the joining node ([joining.md](joining.md)). At most 32 invitations are open at once — a 33rd is refused until one is redeemed or expires, and expired ones are pruned first, so stale tokens never block a new one — and an invitation's text is at most 8 KiB with at most 16 endpoints, checked when minted and again when parsed.

A token is redeemed exactly once: the joining node presents it in the admission handshake, the roster consumes it and remembers the peer as invited, and the first sync exchange delivers the newcomer's node record, after which it is a roster member like any other. Token comparison is constant-time, and the token never appears in a log or an error. Open invitations live in memory only: a restart forgets them, and the owner mints a fresh one. A node with no dialable endpoint mints an invitation that names none, and nobody can join through it — invite from the backbone.

## Expulsion

`inseam network expel <node-id>` publishes an expulsion record and disconnects the node now. An expulsion is the one record an origin publishes about another node: any node may author it, because the owner runs it on whichever node is at hand. On every node that applies it, the expelled node's log, the catalog rows it stewarded, and the roster rows it produced or that describe it are purged, and the admission policy refuses it from then on. Expulsions the expelled node itself had authored stay, so losing a node later never readmits what the owner already expelled. A node cannot expel itself; run it from another node. There is no un-expel: an expelled key is refused even with an invitation, so a device that comes back needs a new identity (remove its key file, [identity.md](identity.md)) and a fresh invitation.

The entry injects `store`, `node`, `transport`, and `connections`, all required, and provides `roster` with the node's `id` as a fact.
