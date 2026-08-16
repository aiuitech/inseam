# Transforms

Transforms take a fragment of a mimetype they claim and emit child fragments with typed relations ([design/indexing.md](../../design/indexing.md)). Each transform is a **plugin**; the registry it registers into is the `transforms` seam ([../kernel.md](../architecture/kernel.md)).

## One registration pathway, two tiers

Linked transform plugins and loaded WASM transforms register identically: `claims(mimetype, is_root)` + `apply(ctx) -> output`, plus a per-run LLM call budget and a **shape fingerprint** (config digest; artifact version for loaded ones) that feeds the shape stamp ([maintenance.md](maintenance.md)). Registration returns a disposer held as a fiber effect — unmounting the plugin unwinds it, and the next sweep discovers the divergence.

The sweep applies claimants recursively: registered transforms over the source root, then over every emitted fragment, until nothing claims the output — a transform claiming another's emitted mimetype (the loaded tier's normal shape, e.g. OCR emitting `text/plain` from images) chains in the same rebuild. Application order is structural before enrichment, then entry id: deterministic regardless of activation order.

Capabilities are handed in, never grabbed: the context carries the text, optionally the raw bytes (only for transforms that declare `wants_bytes`, at the root), and optionally a **granted LLM** (`GrantedLlm`) — metered mechanically against the transform's per-run budget and checked against the `LlmCall` guard on every call. Budget spent or guard denied → the capability refuses and the transform degrades.

Output is uniform: `sprouts` (child fragments with relation kinds — a summary is a sprout related `derived-from`) plus `entities`, which the sweep deduplicates index-wide and wires `mentions` edges for, since a transform cannot know fragment ids.

## The first-party transforms

- **Markdown** (structural, `text/markdown` roots) — decomposes by the document's own heading outline; sections nest as authored, `http(s)` links become `text/uri-list` children (`links-to`). Headingless documents fall back to chunking.
- **Chunker** (structural, other indexable text roots) — paragraph-boundary chunks aimed at `target_chars` (hard break at twice it), line extents.
- **Summarizer** (enrichment, every root) — **mandatory**: every indexed source gets a summary fragment (`text/x-inseam-summary`, `derived-from` the root). Three qualities by circumstance: LLM (capability granted), extractive (text otherwise), envelope-derived (content never read). Provenance rides the mimetype as `via=llm|extractive|envelope`.
- **Entity extractor** (enrichment, every root) — LLM-extracts up to `max_per_source` named entities; each becomes one `text/x-inseam-entity;kind=…` fragment per index with `mentions` edges from the fragments whose text contains the name.
- **OCR** (`plugins/ocr`, loaded, image roots) — the reference community-tier transform: transcribes image text through the granted vision LLM, emitting `text/plain;via=ocr` related `transcribes` ([../plugins/loaded.md](../plugins/loaded.md)).

Decomposition budgets from the sweep entry prune every transform's output: depth cap, per-source fragment cap.

## Mimetypes the index defines

| Mimetype | Meaning |
| --- | --- |
| `text/x-inseam-summary;via=<provenance>` | the mandatory summary fragment |
| `text/x-inseam-entity;kind=<kind>` | a deduplicated entity |
| `text/uri-list` | a link found inside a fragment |

inseam-defined types are derived understanding: no transform may claim them, and the wasm bridge refuses components that emit them.
