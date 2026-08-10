# Addressing

The founding constraint: **source data never moves; only addresses do.** The network maintains and syncs *where data is*, not the data itself. Content is fetched on demand from where it already lives.

## Source

A **source** is the unit of addressable data: an email, a file, a chat thread, a database row — whatever a plugin decides is a sensible retrieval unit for its host.

## Address

An address names a source globally:

```
host identity  +  locator within that host
```

The host component is what makes routing work: resolving an address means finding a path to that host's steward node ([network](network.md)). The locator is opaque to everyone but the steward's connection plugin — the core never interprets it.

## Envelope

Each address carries an **envelope**: a small, structured metadata record that syncs along with it. The envelope holds:

- source type and content type
- content length (lines for text, bytes otherwise) — what lets callers [scan](finder.md) a range instead of fetching whole sources
- timestamps (created, modified, observed)
- trust properties attached at the source level ([access-control](access-control.md))
- discovery hints (title/summary-grade text the [index](discovery.md) can use without fetching)

The envelope is the only content-derived thing that leaves a host. It is deliberately size-bounded: envelopes are replicated to the whole network, so every byte is paid for N times.

## Paths not taken

- **Content-addressing (hashes) as primary identity.** Sources mutate in place on hosts we don't control; location-addressing matches reality. Content hashes may appear *in* envelopes for change detection.
- **Syncing content previews beyond the envelope.** Tempting for search quality, rejected as a default: it recreates data movement. Nodes that want richer indexes fetch content through the normal, access-controlled fetch path.

## Open questions

- Concrete host-ID scheme (also raised in [nodes-and-hosts](nodes-and-hosts.md)).
- Address stability when a host renames/moves a source; whether stewards emit tombstones + new addresses or stable synthetic IDs.
- Versioning: is an address the latest version, or can it name a revision?
- Envelope size ceiling and schema evolution rules.
