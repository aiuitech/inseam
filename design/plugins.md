# Plugins

The core stays small and does the invariant things well: catalog + [sync](address-sync.md), routing, [discovery](discovery.md), [access control](access-control.md), plugin execution. Everything service- and format-specific is a plugin. The long tail of integrations will come from the community and from **AI-authored plugins** — non-developers must be able to have an agent write the plugin they need, so the surface must be small, typed, and generatable.

## What plugins provide

- **Connection types**: how to reach and authenticate against a service ([connections](connections.md)) — Gmail, Slack, a filesystem, an arbitrary REST API.
- **Source handling**: enumerating a host's sources, extracting [envelopes](addressing.md), fetching content on demand.
- **Verification methods**: new ways to verify trust properties ([access-control](access-control.md)).
- Later, most likely: index enrichment, content transformers.

## Runtime: WASM components (WASI)

Plugins are WebAssembly components against WIT-defined interfaces.

- **Sandboxed by construction** — a plugin sees only the capabilities the core hands it, critical when most plugins are third-party or machine-generated.
- **Any language** — anything that compiles to a WASM component (Rust, Go, Python, JS, …), so authors and AI agents work in whatever they know.
- **Typed contract** — WIT interfaces are the API: machine-readable, versionable, and exactly the kind of narrow schema an agent can reliably target.
- **One artifact** — a `.wasm` file runs identically on every node the core runs on.

### Capability-mediated I/O

Plugins do not get raw sockets. The core exposes host functions (HTTP requests, credential access, storage) and the plugin's manifest declares what it needs — down to which external hosts it may call. The core mediates every call. This is both the sandbox story and the workaround for WASI's still-maturing native networking: the core owns the network stack; plugins just describe requests.

## Distribution

No inseam-specific registry to start. A plugin is a `.wasm` + manifest, distributed through ecosystems people already use — npm, PyPI, crates.io, OCI registries, or a plain URL. Install = fetch by package ref or URL, verify, load.

A community registry follows once the WIT contract settles ([positioning](positioning.md)) — the intended path is an agent skill that authors plugins against the contract, publishing to a registry the community shares. Two commitments made now:

- **The contract is proven before the skill ships.** Exit criterion: the core carries zero service-specific dependencies — everything service-shaped reaches core through the plugin pathway. The OpenRouter extraction is the current test of this.
- **Registry trust is a launch requirement, not hardening.** Plugins hold credentials and read personal data, and many will be machine-generated; the registry starts with signed manifests and declared capabilities (which host functions, which external hosts) or it doesn't start.

## Paths not taken

- **Native dynamic libraries.** No sandbox, ABI instability, per-platform builds. Unacceptable for untrusted/AI-generated code.
- **Sidecar processes over RPC.** Heavier per-plugin cost and a much larger attack surface, but kept as a documented **escape hatch** for the rare integration WASM cannot express (native SDK dependencies, device drivers).
- **Embedded scripting language (Lua/JS only).** Single-language lock-in contradicts meeting authors where they are.

## Settled since

- **Core transforms go through the plugin pathway already.** The index's `TransformRegistry` is the registration door: core transforms register with the same claims/apply contract and capability-mediated I/O (the LLM handle is granted per call and withheld when budgets are spent) that WASM transforms will use behind a host-side adapter. One registry, two backends; the WIT world can be extracted from this contract rather than invented.
- **An OpenAI-compatible LLM endpoint is configuration, not a plugin.** The profile's `[endpoint]` (base URL + key env var) covers OpenRouter, OpenAI, Ollama, vLLM and kin; plugin-supplied embedders remain for genuinely custom implementations (bundled models, novel protocols).
- **The WIT contract waits for the first connection plugin.** Connections (Gmail, IMAP) cannot be faked natively, so they force the real design; transforms and verification methods ride the same world then.

## Open questions

- Wasmtime directly vs. a plugin-framework layer (e.g. Extism) on top; leaning wasmtime + our own WIT world for full control of the contract.
- Manifest schema: capability grants, version pinning, signing/provenance (matters more as AI-generated plugins circulate).
- Long-running vs. per-call plugin instantiation; state a plugin may keep.
- Streaming large fetch results across the component boundary without copying whole payloads.
