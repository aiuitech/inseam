# Indexing

How a node turns sources into a searchable **semantic graph**. The index is derived, local, and rebuildable ([discovery](discovery.md) states this); this doc is how it gets built.

## Fragments, not nodes

The graph's unit is a **fragment**: a piece of understanding derived from a source — a markdown section, a transcript line, a summary, an extracted entity. We deliberately don't call them "nodes": *node* already means an inseam instance ([nodes-and-hosts](nodes-and-hosts.md)), and overloading it inside the index would poison every conversation about either.

Every fragment carries:

- a **mimetype** — so the pipeline knows how to treat it next. `text/markdown`, `text/html`, `image/jpeg`, or inseam-defined types for derived things (a summary, an entity).
- **relations** — typed edges to other fragments. Every derived fragment relates to its parent; the type says why: `contains` (structural decomposition), `links-to` (a markdown link to a URL), `derived-from` (a summary of its parent), `mentions` (an entity reference), `transcribes` (an srt for a video). Relation kinds are first-class — the [Finder](finder.md) leans on them.
- an **embedding** — every fragment gets one. A multimodal embedding model embeds whatever mimetypes it supports; text is the common case.
- an **extent** — where the fragment sits within its parent and how long it is (lines for text, bytes or timestamps otherwise). Extents are what let the [Finder](finder.md)'s `scan` operation peek into a slice of a source instead of fetching all of it.
- its source's **address** — and nothing more for access control: **authorization lives at the source level only.** A fragment is never more or less exposed than its source; boundary filtering resolves against the source's trust properties, and every fragment (summaries included) follows its source's answer.

Text fragments additionally enter the full-text index. The result per source is a tree rooted at the source, cross-linked into a graph by entity and link relations.

Fragments are index-local. They have no addresses, never sync, and two nodes indexing the same source will hold different fragments — that is the per-node asymmetry working as intended.

## Transforms

A **transform** is a registered handler: it takes a fragment of a mimetype it claims and emits child fragments with typed relations. Indexing is just recursive transform application until nothing claims the output (or budget runs out).

- A `text/markdown` transform decomposes by the document's own semantic structure: first-level headings with the text under them, second-level headings nested below, each `contains`-related to its parent. A link inside becomes a `text/uri-list` child.
- A URL fragment's transform may fetch the target — yielding a `text/html` child — whose own transform summarizes the page. Recursion, mimetype by mimetype.
- A `video/mp4` transform emits an `application/x-subrip` child, whose transform emits `timestamp:text` line fragments.

Transforms come from two places: **core** ships the universal ones, and [plugins](plugins.md) supply the long tail — the same WASM/WIT surface as connection plugins, with fetches going through capability-mediated I/O (a transform that follows links declares which hosts it may call; no raw sockets).

Two core transforms matter enough to name:

- **Summarizer** — attaches a summary fragment (`derived-from`) to whatever it's pointed at, to a configured length. This is the one **mandatory** transform: the [Finder](finder.md) serves summaries in every response so an AI client can decide whether to keep digging, so every indexed source must have one. Profiles configure the length, not the existence. A small device's whole index can be summaries and nothing else.
- **Entity extractor** — pulls out people, places, projects, dates as **entity fragments**, deduplicated per index (one fragment per entity, however many sources mention it), with `mentions` relations back to every fragment that referenced it. Entities are the graph's connective tissue: they are how two unrelated sources end up one hop apart.

Transforms are why unreadable data becomes findable: registering a handler for a mimetype teaches the whole network's indexes what that data means.

## Index profiles

Everything above is configured per node in its **index profile**:

- embedding model (and thereby which mimetypes embed)
- source date cutoff — don't index past a horizon
- which transforms run, and recursion depth / budget
- summary lengths
- entity extraction on/off and entity budget
- storage backend for the index

A phone's profile: summarizer only, short summaries, envelope-derived text, tight cutoff. A cloud node's profile: every transform, link-following, entities, long summaries. Same machinery, different dial positions — the asymmetry [discovery](discovery.md) promises.

## Storage

The index lives in LanceDB ([runtime](runtime.md)), whose object-store backend makes "where" a profile setting: local disk or S3-compatible storage.

**Remote object storage is for co-located nodes, not for small devices.** Vector + FTS queries against S3 pay object-store round trips per probe; run from a laptop against a distant bucket, interactive queries degrade badly. But inseam already has a better answer for "limitless index off device": that is exactly query fan-out — the small node keeps its lean local index and forwards queries to a big node whose index is deep. The big node may itself back its Lance data with S3 — it runs in the same region as the bucket and can cache hot data locally, so the latency argument disappears. S3 as a *backend for a nearby node*: yes. S3 as a *remote index for a far-away device*: no — use the network.

## Paths not taken

- **"Transformer" / "transformation" as the term.** Rejected: *transformer* is unusably overloaded (the ML architecture — in an AI-native project, worst possible collision) and *transformation* names the event, not the registered thing. **Transform** (noun) keeps the intuition without the collision. Also considered: deriver, refiner, distiller — all narrower than what the mechanism does.
- **"Node" for graph vertices.** Rejected for the terminology clash above.
- **Fixed-size chunking.** Rejected as the primary decomposition: semantic structure (headings, subtitles, threads) produces fragments that mean something, which the relation graph and Finder depend on. A dumb chunker can still exist as a fallback transform for structureless text.
- **Entities as a separate store.** Rejected: entities are fragments in the same graph. One graph, one retrieval algorithm.

## Settled since

- **Where relations live physically**: the SQLite catalog store holds the graph (fragments, relations, entity registry); Lance holds vectors + FTS over text-bearing fragments and is strictly derived.
- **Incremental re-indexing, first cut**: change detection is `modified` timestamp + raw byte size, with an `indexed` completion mark so interrupted runs re-index; a changed source's whole fragment subtree is deleted (relations cascade) and rebuilt. Content-hash detection can replace the heuristic later without structural change.
- **Maintenance**: how the index stays true to sources and profile after the first build — the reconciling sweep, profile-change invalidation tiers, deletion handling, entity GC — is its own concept: [index-maintenance](index-maintenance.md).
- **Entity fragments and source-level authorization**: a deduplicated entity belongs to no single source, so entity fragments carry no source address. They conduct relevance and appear in `expand`, but never rank as results themselves — boundary exposure remains source-level.

## Open questions

- Transform budgets and cycle prevention when link-following transforms recurse into the open web (v1 records `text/uri-list` fragments but does not fetch them).
- Entity deduplication/resolution quality ("Greg" vs "Greg Hunt" vs an email address) — and whether entity resolution is itself a pluggable transform. (Embedding-model migration was open here; settled as in-place re-embed in [index-maintenance](index-maintenance.md).)
