# Transforms

Transforms take a fragment of a mimetype they claim and emit child fragments with typed relations ([design/indexing.md](../../design/indexing.md)).

## One registration pathway

Every transform — core today, plugin tomorrow — registers into the `TransformRegistry` with the same contract: a `claims(mimetype, is_root)` predicate and an `apply(ctx) -> output` implementation. The indexer consults the registry per source root, in registration order (structural before enrichment), and mediates every capability: a transform never performs I/O itself. The LLM handle arrives in the context, and is withheld once the transform's per-run call budget (declared at registration) is spent — budget enforcement *is* capability withholding, the same story the WASM sandbox will enforce mechanically. WASM transforms will join as additional registrants behind a host-side adapter; the registry and the indexer's pathway don't change.

Output is uniform: `sprouts` (child fragments with their relation kinds — a summary is just a sprout related `derived-from`) plus `entities`, which the core deduplicates index-wide and wires `mentions` edges for, since a transform cannot know fragment ids.

They come in two shapes in the current core:

## Structural (claim source roots, emit whole trees)

- **Markdown** (`text/markdown`) — decomposes by the document's own heading outline: each section is a fragment whose text is the section's *own* content, whose extent (in lines) spans the section *including* subsections, and whose children are its subsections. Preamble text before the first heading becomes a section. `http(s)` links become `text/uri-list` child fragments (`links-to`) of the section containing them. Documents without headings fall back to the chunker.
- **Chunker** (any other indexable text) — paragraph-boundary chunks aimed at ~1,600 characters (hard break at twice that), line extents, parent's mimetype preserved.

Both emit their full subtree in one application, so recursive transform application bottoms out immediately; plugin transforms ([design/plugins.md](../../design/plugins.md)) will re-enter the recursion by claiming the emitted mimetypes.

Budgets from the profile prune decomposition: depth cap, per-source fragment cap.

## Enrichment (registered after the structural transforms)

- **Summarizer — mandatory.** It claims every source root, so every indexed source gets a summary fragment (`text/x-inseam-summary`, `derived-from` the root). Three qualities, chosen by circumstance: LLM (capability granted and within the run's call budget), extractive (text sources otherwise: first words, markdown stripped), envelope-derived (content never read — binary, oversized, or envelope-only profiles). Provenance is recorded on the fragment's mimetype as `via=llm|extractive|envelope`; the index report counts by it.
- **Entity extractor — optional.** LLM-extracts up to `max_per_source` named entities (person, place, org, project, date). Each becomes one `text/x-inseam-entity;kind=…` fragment per index — deduplicated through the registry — with `mentions` edges from every fragment of the source whose text contains the name (root as fallback). Entities carry no source of their own; they are the graph's connective tissue.

## Mimetypes the index defines

| Mimetype | Meaning |
| --- | --- |
| `text/x-inseam-summary;via=<provenance>` | the mandatory summary fragment |
| `text/x-inseam-entity;kind=<kind>` | a deduplicated entity |
| `text/uri-list` | a link found inside a fragment |

inseam-defined types are derived understanding: structural transforms never decompose them.
