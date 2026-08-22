# Services

The catalog of service seams: the typed interfaces plugins bind and consume ([kernel](kernel.md) defines the mechanism, [plugins](plugins.md) the provider/consumer discipline). This list is the system's real API surface — adding a capability to inseam means adding a seam here or a provider to one below, never a kernel feature.

Each seam names its definition, its expected providers, and the capability facts consumers may branch on.

## Kernel-provided (not plugins; listed because everything consumes them)

- **`store`** — catalog reads/writes, graph reads/writes, search primitives (FTS, vector, graph traversal), embedding-identity record. The only persistence in the system.
- **`state`** — per-plugin namespaced, versioned state ([kernel](kernel.md): mismatch = discard and rebuild).

## Data plane

- **`connections`** — the registry of configured edges to the hosts this node stewards ([connections](connections.md)): each registration is a host (id, kind, display name), its declared capabilities (enumerates, change feed, writable), and a `Connection` handle — enumerate sources, extract envelopes, fetch content. The registry is a linked provider (`connections`); connection plugins — filesystem and Google Workspace (linked; the latter one host per service), IMAP/Slack/REST (community, loaded) — register into it as effects, and consumers resolve by host.
- **`transforms`** — the registration door for [indexing](indexing.md): claims (mimetype + position) → apply (fragment in, sprouts + keyed sprouts out). Providers register transforms; the sweep consumes the registry. Registrations carry the shape fingerprint and LLM budget the sweep mediates; disposers returned by registration are the entire uninstall path, and registering as a fiber effect is one call on the seam (`register_as_effect`), shared by both tiers. Linked transforms: markdown, chunker, summarizer, entity extractor. Loaded transforms (the OCR reference plugin) claim roots or emitted mimetypes and recurse.
- **`embedder`** — text (later multimodal) → vector, with declared model identity + dimensions. Providers: endpoint-backed, hashed bag-of-words offline fallback, loaded custom. Capability facts: offline, dimensions, mimetypes embedded.
- **`llm`** — chat/completion (and vision) against a configured endpoint; the models consumers should use ride on the seam as capability facts. One seam, metered at the seam: transforms receive a narrowed **granted handle** whose every call is counted mechanically against the per-run budget and checked against the `LlmCall` guard event — monotonic-deny policy without the transform's cooperation.

## Retrieval plane

- **`finder`** — query → ranked sources with summaries and hints ([finder](finder.md)). Default provider implements RRF seeding + relevance propagation; alternative rankers (an LLM re-ranker on a big node) are provider swaps, not core changes.
- **`sweep`** — [index maintenance](index-maintenance.md): the reconciling sweep as a service so change feeds can schedule targeted runs and owner operations can invoke it. Consumes `connection`, `transforms`, `embedder`, `store`.

## Node plane

- **`operations`** — the registry behind the [node API](node-api.md): plugins register typed operations (scoped boundary or owner — the owner's grant operations among them); transport plugins (CLI, HTTP, MCP) consume the registry and stay logic-free. Boundary enforcement is a waterfall on operation dispatch: [access-control](access-control.md) property filtering and future rate limits/audit are listeners, and denial is monotonic — a later listener can never force-allow what one denied.
- **`oauth`** — the node's OAuth 2.0 grants ([connections](connections.md)), a registry: configured on the `oauth` entry or registered by the connection that knows the provider (endpoints, scopes, client-credential variables); authorized by the owner through the browser from any client — a loopback redirect for local transports, a transport-served redirect delivered back for remote ones — refreshed by the provider, announced on change (`GrantChanged`, carrying the signed-in account); host connections consume a grant by id and receive live access tokens. Linked provider: `oauth` (authorization code + PKCE, owner-private credential files).
- **`verification`** — property-verification methods ([access-control](access-control.md)): confirmation links, OAuth proofs, future attestations. Community-extensible, typically loaded.
- **`sync`** — catalog replication across node connections ([address-sync](address-sync.md)). Future.
- **`routing`** — resolving an address to a path toward its steward ([network](network.md)). Future.

## Open questions

- Seam granularity: whether `sweep` and `transforms` stay separate or fold (kept separate while change-feed scheduling is unbuilt).
- Whether the agent demo (today the engine behind `inseam agent`, in the CLI crate) grows into an `agent` seam or stays a consumer of `operations`.
- Which seams are projected into WIT first (transforms and connection force the design; `finder`/`sweep` likely never cross the boundary).
