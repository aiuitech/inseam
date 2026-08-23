# Indexing

How a node turns sources into a searchable **semantic graph**. The index is derived, local, and rebuildable ([discovery](discovery.md) states this); this doc is how it gets built.

## Fragments, not nodes

The graph's unit is a **fragment**: a piece of understanding derived from a source — a markdown section, a transcript line, a summary, an extracted entity. We deliberately don't call them "nodes": *node* already means an inseam instance ([nodes-and-hosts](nodes-and-hosts.md)), and overloading it inside the index would poison every conversation about either.

Every fragment carries:

- a **mimetype** — so the pipeline knows how to treat it next. `text/markdown`, `text/html`, `image/jpeg`, or inseam-defined types for derived things (a summary, an entity).
- **relations** — typed edges to other fragments, every one read **input → output** (the fragment a transform was applied to, to what it produced). Every emitted fragment relates to its input, and the kind says why. The kernel defines exactly two kinds — the **transform relation**: `contains` (structural decomposition) and `derives` (derived understanding, a summary). Every other kind is the emitting plugin's vocabulary: `links-to` (the markdown transform: a section to a URL it links), `mentions` (the entity extractor: a fragment to an entity), `transcribes` (the OCR plugin: an image to its transcript). Relation kinds are first-class — the [Finder](finder.md) weights them by name, and a kind it has never seen still conducts at a default weight.
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

- **Summarizer** — attaches a summary fragment (the input `derives` it) to whatever it's pointed at, to a configured length. This is the one **mandatory** transform: the [Finder](finder.md) serves summaries in every response so an AI client can decide whether to keep digging, so every indexed source must have one. Profiles configure the length, not the existence. A small device's whole index can be summaries and nothing else.
- **Entity extractor** — pulls out people, places, projects, dates as **entity fragments**, deduplicated per index (one fragment per entity, however many sources mention it), with `mentions` relations back to every fragment that referenced it. Entities are the graph's connective tissue: they are how two unrelated sources end up one hop apart. Everything entity-shaped is this plugin's own vocabulary; what the kernel provides is the generic mechanism beneath it — **keyed fragments** (below).

Transforms are why unreadable data becomes findable: registering a handler for a mimetype teaches the whole network's indexes what that data means.

## Keyed fragments: the one index-wide fragment shape

A fragment normally belongs to exactly one source. The single exception the kernel's store supports is a **keyed fragment**: a fragment that belongs to no source and is deduplicated across the whole index under a plugin-namespaced key (`entity:person:greg`). A transform emits one as a **keyed sprout** — key, fragment, relation kind, and an **anchor** saying where in the emitting source the edge lands (the input fragment, or every source-content fragment whose text contains a needle, falling back to the input). The sweep resolves it: get-or-create the fragment under its key, then anchor it with the emitter's relation kind. A keyed fragment left with no relation is collected at the end of the sweep ([index-maintenance](index-maintenance.md)).

This is deliberately generic. Entities are the first vocabulary built on it; a topics plugin, a correspondent plugin over mail, a citation plugin over papers each bring their own key namespace, mimetype, and relation kind without the kernel learning any of them. Because keyed fragments belong to no source they carry no source address: they conduct relevance and appear in `expand`, but never rank as results themselves — boundary exposure stays source-level.

## Digests dedupe compute, not the graph

The expensive parts of indexing recur per fragment, not per source: embeddings, and the LLM transforms (summaries, entities). Fragment content digests turn those into **digest-keyed caches**:

- an embedding is stored once per `(content digest, embedding identity)` — the embedder model, its dimensions, and the vector scope; fragment rows reference it.
- an LLM transform's output is stored once per `(input content digest, that transform's shape-relevant identity and config)` — the same ingredients the shape stamp digests ([index-maintenance](index-maintenance.md)), so a shape change never needs a cache flush: stale entries simply stop matching.

The dedup is deliberately at the **artifact** layer, not the graph layer. Fragments stay per-source — every fragment carries exactly one source address, the invariant boundary filtering leans on — so the same file indexed from two hosts, or the same boilerplate section in fifty documents, still produces its own cheap fragment rows, but pays for its vectors and its LLM calls exactly once. The vectors are the storage bulk, so this is also where the index actually gets lean.

The same caches are what make the delete-and-rebuild maintenance model affordable: a rebuilt subtree re-runs decomposition (cheap) and hits the caches for every fragment whose content didn't change, so a rebuild costs only what actually changed. Cache entries left unreferenced by a rebuild or deletion are kept — content that comes back re-hits for free, the same tighten-then-loosen stance [index-maintenance](index-maintenance.md) takes on scope — and reclaimed only by the explicit `vacuum`.

## Configuration is composition

Everything above is configured per node by its [composition](composition.md), including what is kept out of the index altogether ([ignore](ignore.md)): which transform plugins are mounted and each one's config (summary lengths, entity budgets, recursion depth), which embedder provider runs, the sweep's cutoffs and budgets. A phone's composition: summarizer only, short summaries, vectors for summaries only, envelope-derived text, tight cutoff. A cloud node's: every transform, link-following, entities, long summaries, a vector on every fragment. Same machinery, different entries — the asymmetry [discovery](discovery.md) promises, with no profile mechanism separate from ordinary plugin config.

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

- **Where relations live physically**: the catalog tables hold the graph (fragments, relations, the keyed-fragment registry); the search tables in the same libSQL database hold vectors + FTS over text-bearing fragments and are strictly derived.
- **Incremental re-indexing, first cut**: change detection is `modified` timestamp + raw byte size, with an `indexed` completion mark so interrupted runs re-index; a changed source's whole fragment subtree is deleted (relations cascade) and rebuilt. The timestamp + size heuristic stays the *detector* — it needs no fetch — and the digest-keyed artifact caches (above) make its false positives cheap: a touched-but-unchanged file rebuilds structure and re-spends nothing.
- **Maintenance**: how the index stays true to sources and composition after the first build — the reconciling sweep, invalidation tiers, deletion handling, entity GC — is its own concept: [index-maintenance](index-maintenance.md).
- **Entity fragments and source-level authorization**: a deduplicated entity belongs to no single source, so entity fragments carry no source address. They conduct relevance and appear in `expand`, but never rank as results themselves — boundary exposure remains source-level. (Generalized since to every keyed fragment, above.)
- **Relation kinds are an open vocabulary, and entities are a plugin's.** The first cut had a closed `RelationKind` enum in the kernel (`contains`, `links-to`, `derived-from`, `mentions`, `transcribes`) and an entity registry the store and sweep knew by name. Both were plugin knowledge in the kernel: only the transform relation is a core truth of indexing — a transform always produces a child or a derivation — so the kernel now defines exactly `contains` and `derives` and validates every other kind as a name; the entity registry became the keyed-fragment facility and `EntityKind`, `ExtractedEntity`, `text/x-inseam-entity`, and `mentions` moved into the entity plugin. The one direction rule (input → output) replaced per-kind direction logic: `derived-from` (child → parent) became `derives` (parent → child) so the rule has no exceptions. *Rejected:* a relation-kind registry plugins declare into — kinds are data flowing through the fixed structure ([kernel](kernel.md)), and a registry would be a second source of truth for names that only the emitter and the finder's weight table care about.

- **The vector width belongs to the model; the config can only agree with it.** The embedder's `dimensions` is optional — unset takes the model's native width — and when set it is checked at activation against what the model produces: a built-in catalog of known embedding models first, the endpoint's own introspection second (a local ollama reports each installed model's capabilities and width through its native API; its OpenAI-compatible `/v1/models` lists ids alone), and explicit config last, taken on trust with the first embedding as the check. A smaller width is accepted only for Matryoshka-trained models and is then requested as the API's `dimensions`; every OpenAI-compatible server will truncate any model's vectors on request, and a truncated non-Matryoshka vector is noise wearing the right shape. *Rejected:* probing the model with a throwaway embedding on every boot (a network call per process start to learn a constant), and trusting the configured width until the first mismatch (the previous behavior — a whole sweep's transforms spent before the first batch failed).

- **Which fragments get vectors is an embedder dial, and part of the identity.** Vectors are the storage bulk, so `vectors = all | summaries` chooses between a vector on every text fragment and one bounded vector per source; every text fragment enters the full-text index regardless, so exact-term recall into sections survives the lean setting and only vector seeding narrows to summaries. The scope rides the embedding identity the store records, so flipping it is an in-place re-embed ([index-maintenance](index-maintenance.md)) — no transforms re-run — rather than a shape change that would rebuild subtrees. *Rejected:* a sweep-side row filter keyed into the shape stamp (re-runs LLM transforms to change what is derived from their output), and dropping non-summary rows from full-text too (saves little — text is already in the catalog — and loses exact-term search for a storage win that vectors, not text, deliver).

- **Local models are configuration, and the endpoint is asked what it knows.** A local ollama is `base_url = http://localhost:11434/v1` with an empty `api_key_env`; the `llm` provider reads its native discovery API beside the OpenAI-compatible one — installed models, which embed and which chat, each embedder's width — for `inseam models` and for width validation, and falls back to the plain `/models` catalog elsewhere. Thinking models are told how much to think per call (`transform_reasoning_effort`, sent as `reasoning_effort` only when configured): transform-grade calls want the cheapest direct answer, and a thinking model left to itself spends its reply on thought and returns an empty summary. *Rejected:* an `ollama` provider variant (the seam contract is the OpenAI-compatible wire; the native API is discovery only, best-effort, and the plugin stays config-driven — `design/plugins.md`).

- **Embeddings cross the wire as base64.** The embeddings request asks for `encoding_format: "base64"` — one packed little-endian `f32` array per input instead of thousands of decimal literals: less than half the bytes and a byte copy instead of float parsing on the hot path of indexing. Servers that ignore the parameter answer in floats and both shapes are read, so no endpoint is cut off by the optimization.

- **Indexing is a plan-then-land pipeline, parallel where the time goes.** Transforms — LLM calls above all — are the cost of indexing, so they are what runs concurrently: a configurable number of sources are *planned* at once (content read, transforms applied recursively, every claimant of a fragment in flight together), and each source's result is a pure-data **subtree plan** — fragments and relations by plan position, keyed sprouts with their anchors resolved — that the store *lands* in one transaction. Landing happens in enumeration order whatever order planning finishes, so the built index is independent of the concurrency dial; the embedding stage then embeds batches concurrently and lands them in order, carrying each source's `indexed` mark with the batch that completes it. Per-transform LLM budgets are shared meters with atomic reservation, so concurrency cannot overspend them. *Rejected:* per-fragment store writes as they happen (the first cut: one autocommit per fragment and relation, serialized with the transforms it waited on — a subtree rebuild scanned the vector table per source, and no amount of transform parallelism could hide the write path); per-transform or per-fragment parallelism inside one store transaction (the store is one connection, one writer — concurrency belongs in planning, not persistence); a work-stealing unordered landing (faster in the tail, but fragment ids would depend on timing, which golden checks and reproducible rebuilds are not worth trading for).

## Open questions

- Transform budgets and cycle prevention when link-following transforms recurse into the open web (v1 records `text/uri-list` fragments but does not fetch them).
- Entity deduplication/resolution quality ("Greg" vs "Greg Hunt" vs an email address) — and whether entity resolution is itself a pluggable transform. Keyed sprouts make the plumbing for that pluggable already; the resolution logic is not written.
- Keyed sprouts across the WIT boundary: loaded transforms emit child fragments only today. The entity extractor's shape (key + anchor) is the vocabulary the projection would need. (Embedding-model migration was open here; settled as in-place re-embed in [index-maintenance](index-maintenance.md).)
