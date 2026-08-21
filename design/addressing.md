# Addressing

The founding constraint: **source data never moves; only addresses do.** The network maintains and syncs *where data is*, not the data itself. Content is fetched on demand from where it already lives.

## Source

A **source** is the unit of addressable data: an email, a file, a chat thread, a database row — whatever a plugin decides is a sensible retrieval unit for its host.

## Address

An address names a source globally:

```
inseam://<host-id>/<locator>
```

The host component is what makes routing work: resolving an address means looking the host up in the [roster](roster.md), finding its steward, and finding a path to that node ([network](network.md)). The locator is opaque to everyone but the steward's connection plugin — the core never interprets it.

## Host identity

The **host id is an opaque, stable identifier** — a fingerprint of identity material appropriate to the host's kind, with the kind as domain separator in the derivation:

- filesystem host → machine identity (hardware/platform UUID), *not* the mutable hostname;
- service host → the account principal (the Gmail address, the Slack workspace + user), *not* the API used to reach it.

Because derivation is deterministic per kind, any steward connecting to the same underlying host mints the same id independently — two nodes holding grants to one Gmail account converge on *one* host with no coordination. The kind acting as domain separator means ids from different kinds can never collide.

Everything else about a host — its kind, its display name, who stewards it, what protocols reach it — lives in roster records, not in the address. Addresses are pure identity, so they survive machine renames, IP rotation, protocol changes, and steward handoff: **the name of the data never changes because the way to reach it changed.**

## Envelope

Each address carries an **envelope**: a small, structured metadata record that syncs along with it. The envelope holds:

- source type and content type
- content length (lines for text, bytes otherwise) — what lets callers [scan](finder.md) a range instead of fetching whole sources
- timestamps (created, modified, observed)
- trust properties attached at the source level ([access-control](access-control.md))
- discovery hints (title/summary-grade text the [index](discovery.md) can use without fetching)
- a **content digest**, when the steward has one (below) — the cross-host dedup key

The envelope is the only content-derived thing that leaves a host. It is deliberately size-bounded: envelopes are replicated to the whole network, so every byte is paid for N times.

## Paths not taken

- **Content-addressing (hashes) as primary identity.** Sources mutate in place on hosts we don't control; location-addressing matches reality. Two copies of one file really are two sources — different trust properties, different availability, different lifecycles — and identity must have somewhere coherent to hang those. Content digests exist in the envelope (settled below), but as a dedup key, never a name.
- **Protocol or kind in the host name** (the early `fs-<hostname>` scheme). Rejected: it conflates reachability with identity. Protocol is a property of a steward's [connection](connections.md) — per-edge, plural, and mutable — while an address must be one and stable; a host reached by two protocols would mint two identities and break multi-steward dedup. The kind participates in id *derivation* (as domain separator) but is a roster fact, not a name component.
- **Mutable names (hostnames) as host identity.** Rejected: renaming a machine must not invalidate every address on it.
- **Syncing content previews beyond the envelope.** Tempting for search quality, rejected as a default: it recreates data movement. Nodes that want richer indexes fetch content through the normal, access-controlled fetch path.

## Settled since

- **Content digest in the envelope.** An optional envelope field: BLAKE3 over the source's raw bytes, computed by the steward while it reads content it already reads for hints and length, or taken from service metadata only when the service provides that exact algorithm. In practice that read is the index sweep's: filesystem enumeration is metadata-only, so the digest is filled the first time indexing reads the source's bytes — a source whose content is never read (oversized, binary with no claiming transform) simply carries none. Its one job is to be a **merge key**: the same file living on two hosts (a local file synced to Google Drive) is two sources whose envelopes carry equal digests, and the [Finder](finder.md) collapses them into one result at merge time. The envelope is the right layer because it is the only content-derived artifact that reaches every node — an envelope-only index never fetches content and could not compute a digest itself. Dedup is **best-effort by construction**: a service with no comparable hash, or a source with no stable byte form (a Google-native doc), simply yields no digest and never collapses — degradation is duplication, never a correctness problem. Fragment-level digests inside the index are a separate mechanism with a separate job ([indexing](indexing.md)).

## Open questions

- Address stability when a host renames/moves a source; whether stewards emit tombstones + new addresses or stable synthetic IDs.
- Versioning: is an address the latest version, or can it name a revision?
- Envelope size ceiling and schema evolution rules.
