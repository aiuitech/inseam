# Transforms

A transform takes a fragment of a mimetype it claims and emits child fragments with typed relations ([design/indexing.md](../../design/indexing.md)). Each transform is a **plugin**; the registry they sign up with is the `transforms` seam ([../architecture/kernel.md](../architecture/kernel.md)).

## One registration pathway, two tiers

Linked (Rust) transforms and loaded (WASM) transforms register the same way: `claims(mimetype, is_root)` + `apply(ctx) -> output`, plus a per-run LLM call budget and a **shape fingerprint** (a config digest; the artifact version for loaded ones) that feeds the shape stamp ([maintenance.md](maintenance.md)). Registration returns an undo function held as a fiber effect — unmounting the plugin runs it, and the next sweep notices the difference.

The sweep applies transforms recursively: over the source root, then over every fragment they emit, until nothing claims the output. A transform claiming another's emitted mimetype (the loaded tier's usual shape — e.g. OCR emitting `text/plain` from images) chains in the same rebuild. Application order is structural transforms before enrichment ones, then by entry id — deterministic no matter what order plugins started in.

Capabilities are handed in, never grabbed: the context carries the text, optionally the raw bytes (only for transforms that declare `wants_bytes`, and only at the root), and optionally a **granted LLM** (`GrantedLlm`) — metered against the transform's per-run budget and checked against the `LlmCall` guard on every call. Budget spent, or guard says no → the LLM handle refuses, and the transform falls back gracefully.

Output is uniform: `sprouts` (child fragments, each with the relation kind from the input to it — a summary is a sprout related `derives`) plus `keyed` sprouts: fragments shared index-wide under a plugin-namespaced key, with an **anchor** rule (the input fragment, or every source-content fragment whose text contains a needle) the sweep resolves, since a transform can't know fragment ids. Relation kinds are an open vocabulary: the kernel defines `contains` and `derives`; a plugin names any other kind it needs (kebab-case) and the finder weights it by name.

## The first-party transforms

- **Markdown** (structural, `text/markdown` roots) — splits the document by its own heading outline; sections nest as written, `http(s)` links become `text/uri-list` children (`links-to`). Documents with no headings fall back to chunking.
- **Chunker** (structural, other indexable text roots) — paragraph-boundary chunks aimed at `target_chars` (hard break at twice that), with line extents.
- **Summarizer** (enrichment, every root) — **mandatory**: every indexed source gets a summary fragment (`text/x-inseam-summary`; the root `derives` it). Three qualities depending on circumstance: LLM (when the capability is granted), extractive (when there's text but no LLM), envelope-derived (when the content was never read). The provenance rides the mimetype as `via=llm|extractive|envelope`.
- **Entity extractor** (enrichment, every root) — uses the LLM to pull out up to `max_per_source` named entities; each is emitted as a keyed sprout (`entity:<kind>:<name>`) that becomes one `text/x-inseam-entity;kind=…` fragment per index, anchored by `mentions` edges from every fragment whose text contains the name. The whole entity vocabulary is this plugin's.
- **OCR** (`plugins/ocr`, loaded, image roots) — the reference community-tier transform: reads image text through the granted vision LLM, emitting `text/plain;via=ocr` related `transcribes` ([../plugins/loaded.md](../plugins/loaded.md)).

The sweep entry's limits prune every transform's output: a depth cap and a per-source fragment cap.

## Mimetypes the index defines

| Mimetype | Meaning |
| --- | --- |
| `text/x-inseam-summary;via=<provenance>` | the mandatory summary fragment |
| `text/x-inseam-entity;kind=<kind>` | a deduplicated entity (the entity plugin's type; a keyed fragment) |
| `text/uri-list` | a link found inside a fragment |

These are inseam's own derived types: no transform may claim them, and the wasm bridge refuses components that try to emit them.
