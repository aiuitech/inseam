# Indexing

How a node turns sources into a searchable **semantic graph**. The index is derived, local, and rebuildable ([discovery](discovery.md) states this); this doc is how it gets built.

## Fragments, not nodes

The graph's unit is a **fragment**: a piece of understanding derived from a source — a markdown section, a transcript line, a summary, an extracted entity. We deliberately don't call them "nodes": *node* already means an inseam instance ([nodes-and-hosts](nodes-and-hosts.md)), and overloading it inside the index would poison every conversation about either.

Every fragment carries:

- a **mimetype** — so the pipeline knows how to treat it next. `text/markdown`, `text/html`, `image/jpeg`, or inseam-defined types for derived things (a summary, an entity).
- **relations** — typed edges to other fragments. Every derived fragment relates to its parent; the type says why: `contains` (structural decomposition), `links-to` (a markdown link to a URL), `derived-from` (a summary of its parent), `mentions` (an entity reference), `transcribes` (an srt for a video). Relation kinds are first-class — the [Finder](finder.md) leans on them.
- an **embedding** — every fragment gets one. A multimodal embedding model embeds whatever mimetypes it supports; text is the common case.
- a **content digest** — BLAKE3 over the fragment's content bytes, the key into the derived-artifact caches (below).
- an **extent** — where the fragment sits within its parent and how long it is (lines for text, bytes or timestamps otherwise). Extents are what let the [Finder](finder.md)'s `scan` operation peek into a slice of a source instead of fetching all of it.
- its source's **address** — and nothing more for access control: **authorization lives at the source level only.** A fragment is never more or less exposed than its source; boundary filtering resolves against the source's trust properties, and every fragment (summaries included) follows its source's answer.

Text fragments additionally enter the full-text index. The result per source is a tree rooted at the source, cross-linked into a graph by entity and link relations.

Fragments are index-local. They have no addresses, never sync, and two nodes indexing the same source will hold different fragments — that is the per-node asymmetry working as intended.

## Transforms

A **transform** is a registered handler: it takes a fragment of a mimetype it claims and emits child fragments with typed relations. Indexing is just recursive transform application until nothing claims the output (or budget runs out).

- A `text/markdown` transform decomposes by the document's own semantic structure: first-level headings with the text under them, second-level headings nested below, each `contains`-related to its parent. A link inside becomes a `text/uri-list` child.
- A URL fragment's transform may fetch the target — yielding a `text/html` child — whose own transform summarizes the page. Recursion, mimetype by mimetype.
- A `video/mp4` transform emits an `application/x-subrip` child, whose transform emits `timestamp:text` line fragments.

Every transform is a [plugin](plugins.md) registering into the `transforms` [seam](services.md): the universal ones ship as linked plugins in every distribution, and the long tail arrives as loaded plugins claiming emitted mimetypes — same registry, same claims/apply contract, with fetches going through capability-mediated I/O (a transform that follows links declares which hosts it may call; no raw sockets) and LLM budgets metered at the `llm` seam.

Two core transforms matter enough to name:

- **Summarizer** — attaches a summary fragment (`derived-from`) to whatever it's pointed at, to a configured length. This is the one **mandatory** transform: the [Finder](finder.md) serves summaries in every response so an AI client can decide whether to keep digging, so every indexed source must have one. Profiles configure the length, not the existence. A small device's whole index can be summaries and nothing else.
- **Entity extractor** — pulls out people, places, projects, dates as **entity fragments**, deduplicated per index (one fragment per entity, however many sources mention it), with `mentions` relations back to every fragment that referenced it. Entities are the graph's connective tissue: they are how two unrelated sources end up one hop apart.

Transforms are why unreadable data becomes findable: registering a handler for a mimetype teaches the whole network's indexes what that data means.

## Digests dedupe compute, not the graph

The expensive parts of indexing recur per fragment, not per source: embeddings, and the LLM transforms (summaries, entities). Fragment content digests turn those into **digest-keyed caches**:

- an embedding is stored once per `(content digest, embedder model + dimensions)`; fragment rows reference it.
- an LLM transform's output is stored once per `(input content digest, that transform's shape-relevant identity and config)` — the same ingredients the shape stamp digests ([index-maintenance](index-maintenance.md)), so a shape change never needs a cache flush: stale entries simply stop matching.

The dedup is deliberately at the **artifact** layer, not the graph layer. Fragments stay per-source — every fragment carries exactly one source address, the invariant boundary filtering leans on — so the same file indexed from two hosts, or the same boilerplate section in fifty documents, still produces its own cheap fragment rows, but pays for its vectors and its LLM calls exactly once. The vectors are the storage bulk, so this is also where the index actually gets lean.

The same caches are what make the delete-and-rebuild maintenance model affordable: a rebuilt subtree re-runs decomposition (cheap) and hits the caches for every fragment whose content didn't change, so a rebuild costs only what actually changed. Cache entries left unreferenced by a rebuild or deletion are kept — content that comes back re-hits for free, the same tighten-then-loosen stance [index-maintenance](index-maintenance.md) takes on scope — and reclaimed only by the explicit `vacuum`.

## Configuration is composition

Everything above is configured per node by its [composition](composition.md), including what is kept out of the index altogether ([ignore](ignore.md)): which transform plugins are mounted and each one's config (summary lengths, entity budgets, recursion depth), which embedder provider runs, the sweep's cutoffs and budgets. A phone's composition: summarizer only, short summaries, envelope-derived text, tight cutoff. A cloud node's: every transform, link-following, entities, long summaries. Same machinery, different entries — the asymmetry [discovery](discovery.md) promises, with no profile mechanism separate from ordinary plugin config.

## Storage

The index lives in libSQL ([runtime](runtime.md)) as a single database file on the node's local disk — catalog tables and derived search tables in one file.

**The index is local storage; reach comes from the network.** An earlier cut of this design (the LanceDB era) kept an object-store backend open so a node could put its index on S3. That door is closed: remote storage pays round trips per probe — run from a laptop against a distant bucket, interactive queries degrade badly — and inseam already has a better answer for "limitless index off device": query fan-out. The small node keeps its lean local index and forwards queries to a big node whose index is deep; the big node keeps *its* index on its own fast local disk. Big index → big node's disk, reached over the network — never a far-away device mounting remote index storage.

## Paths not taken

- **"Transformer" / "transformation" as the term.** Rejected: *transformer* is unusably overloaded (the ML architecture — in an AI-native project, worst possible collision) and *transformation* names the event, not the registered thing. **Transform** (noun) keeps the intuition without the collision. Also considered: deriver, refiner, distiller — all narrower than what the mechanism does.
- **"Node" for graph vertices.** Rejected for the terminology clash above.
- **Fixed-size chunking.** Rejected as the primary decomposition: semantic structure (headings, subtitles, threads) produces fragments that mean something, which the relation graph and Finder depend on. A dumb chunker can still exist as a fallback transform for structureless text.
- **Entities as a separate store.** Rejected: entities are fragments in the same graph. One graph, one retrieval algorithm.
- **Shared fragment rows across sources.** Rejected: deduping identical content by making sources share subtrees would turn fragment→source into one-to-many and hand boundary filtering a new edge case, to save only the cheap rows. Dedupe the expensive derived artifacts by digest instead; keep the one-fragment-one-source invariant.

## Settled since

- **Where relations live physically**: the catalog tables hold the graph (fragments, relations, entity registry); the search tables in the same libSQL database hold vectors + FTS over text-bearing fragments and are strictly derived.
- **Incremental re-indexing, first cut**: change detection is `modified` timestamp + raw byte size, with an `indexed` completion mark so interrupted runs re-index; a changed source's whole fragment subtree is deleted (relations cascade) and rebuilt. The timestamp + size heuristic stays the *detector* — it needs no fetch — and the digest-keyed artifact caches (above) make its false positives cheap: a touched-but-unchanged file rebuilds structure and re-spends nothing.
- **Maintenance**: how the index stays true to sources and composition after the first build — the reconciling sweep, invalidation tiers, deletion handling, entity GC — is its own concept: [index-maintenance](index-maintenance.md).
- **Entity fragments and source-level authorization**: a deduplicated entity belongs to no single source, so entity fragments carry no source address. They conduct relevance and appear in `expand`, but never rank as results themselves — boundary exposure remains source-level.

## Open questions

- Transform budgets and cycle prevention when link-following transforms recurse into the open web (v1 records `text/uri-list` fragments but does not fetch them).
- Entity deduplication/resolution quality ("Greg" vs "Greg Hunt" vs an email address) — and whether entity resolution is itself a pluggable transform. (Embedding-model migration was open here; settled as in-place re-embed in [index-maintenance](index-maintenance.md).)
