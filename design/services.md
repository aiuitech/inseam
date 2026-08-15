# Services

The catalog of service seams: the typed interfaces plugins bind and consume ([kernel](kernel.md) defines the mechanism, [plugins](plugins.md) the provider/consumer discipline). This list is the system's real API surface — adding a capability to inseam means adding a seam here or a provider to one below, never a kernel feature.

Each seam names its definition, its expected providers, and the capability facts consumers may branch on.

## Kernel-provided (not plugins; listed because everything consumes them)

- **`store`** — catalog reads/writes, graph reads/writes, search primitives (FTS, vector, graph traversal), embedding-identity record. The only persistence in the system.
- **`state`** — per-plugin namespaced, versioned state ([kernel](kernel.md): mismatch = discard and rebuild).

## Data plane

- **`connection`** — a configured edge to one host: enumerate sources, extract envelopes, fetch content, optionally stream a change feed ([connections](connections.md)). Providers: filesystem (first), Gmail/IMAP/Slack/REST (community, sandboxed). Capability facts: change feed offered, writability, enumeration cost class.
- **`transforms`** — the registration door for [indexing](indexing.md): claims (mimetype + position) → apply (fragment in, sprouts + entities out). Providers register transforms; the sweep consumes the registry. Native transforms: markdown, chunker, summarizer, entity extractor. Sandboxed transforms claim emitted mimetypes and recurse. Capability facts per transform: structural vs. enrichment, LLM-hungry or not.
- **`embedder`** — text (later multimodal) → vector, with declared model identity + dimensions. Providers: endpoint-backed, hashed bag-of-words offline fallback, sandboxed custom. Capability facts: offline, dimensions, mimetypes embedded.
- **`llm`** — chat/completion against a configured endpoint. One seam, metered at the seam: per-consumer budgets are waterfall interception on `llm` calls, which is how a transform's LLM allowance is enforced without the transform's cooperation.

## Retrieval plane

- **`finder`** — query → ranked sources with summaries and hints ([finder](finder.md)). Default provider implements RRF seeding + relevance propagation; alternative rankers (an LLM re-ranker on a big node) are provider swaps, not core changes.
- **`sweep`** — [index maintenance](index-maintenance.md): the reconciling sweep as a service so change feeds can schedule targeted runs and owner operations can invoke it. Consumes `connection`, `transforms`, `embedder`, `store`.

## Node plane

- **`operations`** — the registry behind the [node API](node-api.md): plugins register typed operations (scoped boundary or owner); transport plugins (CLI, HTTP, MCP) consume the registry and stay logic-free. Boundary enforcement is a waterfall on operation dispatch: [access-control](access-control.md) property filtering and future rate limits/audit are listeners, and denial is monotonic — a later listener can never force-allow what one denied.
- **`verification`** — property-verification methods ([access-control](access-control.md)): confirmation links, OAuth proofs, future attestations. Community-extensible, typically sandboxed.
- **`sync`** — catalog replication across node connections ([address-sync](address-sync.md)). Future.
- **`routing`** — resolving an address to a path toward its steward ([network](network.md)). Future.

## Open questions

- Seam granularity: whether `sweep` and `transforms` stay separate or fold (kept separate while change-feed scheduling is unbuilt).
- Whether the agent demo grows into an `agent` seam or stays a consumer of `operations`.
- Which seams are projected into WIT first (transforms and connection force the design; `finder`/`sweep` likely never cross the boundary).
