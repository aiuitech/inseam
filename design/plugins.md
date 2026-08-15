# Plugins

Everything in inseam is a plugin. The [kernel](kernel.md) runs plugins and owns the store; every feature — hosts and connections, transforms, embedders, the finder, the LLM endpoint, transports, the boundary, sync — is a plugin binding or consuming [service seams](services.md). Two audiences drive the surface: the community, and **AI-authored plugins** — non-developers must be able to have an agent write the plugin they need, so the contract must be small, typed, and generatable.

## Anatomy

Every plugin, regardless of tier, is the same five declarations:

- **name** — identity in the [composition](composition.md).
- **config** — a typed, schema-validated struct; validated before the plugin runs, surfaced to UIs and docs from the same schema. No hardcoded tunables: anything deployment-varying is config.
- **inject** — the service keys it requires (plus optional ones, marked). This is the plugin's capability manifest: the substrate makes undeclared access unrepresentable, so what a plugin *can* touch is readable off its declaration.
- **provide** — the service keys it binds, if it is a provider.
- **apply** — the body: runs when injections are satisfied, makes all its registrations as [effects](kernel.md), and thereby needs no uninstall path. Failure lands the fiber alone; unload unwinds every effect in reverse.

The provider/consumer split is a design rule, not just a mechanism: a **service definition** (trait + vocabulary + capability facts) lives apart from its providers, and consumers bind to the definition only. The test of a good seam is that swapping providers — local filesystem for remote sandbox, OpenRouter for Ollama, native summarizer for a WASM one — restarts consumers without editing them.

## Two trust tiers, one plugin model

Tier is **provenance, not shape** — both tiers register into the same seams and are indistinguishable to consumers.

### Native plugins (trusted)

First-party and vetted code: Rust, statically linked into the binary, activated by composition. In-process trait calls, zero boundary cost — the tier the indexing hot path, the finder, the store-adjacent machinery live in. Trusted does not mean undisciplined: injections are still the only reach a plugin has, which keeps native plugins auditable, swap-safe, and honest about their dependencies.

A **distribution** is an app crate that links a set of native plugins and ships a base composition: the CLI, the macOS app, a future headless daemon are distributions of the same kernel ([runtime](runtime.md)).

### Sandboxed plugins (untrusted)

Community and machine-generated code: **WASM components against WIT interfaces**, loaded at runtime.

- **Sandboxed by construction** — a component sees only the capabilities the bridge hands it; critical when most third-party plugins hold credentials, read personal data, and were written by an agent.
- **Any language** — anything that compiles to a WASM component, so authors and AI agents work in what they know.
- **One artifact** — a `.wasm` + manifest runs identically on every node.

A sandboxed plugin is mounted by the **plugin-host bridge**, itself a native plugin: it reads the manifest (which seams the component implements, which capabilities it requests — down to which external hosts it may call), instantiates the component, and adapts WIT ↔ the native service traits. Capability attenuation is interception at the bridge: budgets, host allowlists, and credential scoping are applied to the handles the component receives, not enforced inside it. Plugins get no raw sockets; the kernel side owns the network stack and the component describes requests — both the sandbox story and the workaround for WASI's immature networking.

**The WIT contract is generated from the service definitions**, not designed separately: the native seam is the source of truth, and the WIT world is its projection across the boundary. This retires the old two-registry risk — there is one seam, and the bridge is just another provider/consumer on it.

## Lifecycle

Uniform across tiers, owned by the substrate ([kernel](kernel.md)): reactive activation on injected services, restart on provider identity change, effect-unwind on unload, per-fiber failure containment, loud missing-dependency errors at composition settle. Sandboxed plugins additionally get *code* dynamism: install, upgrade, and reload of a `.wasm` artifact at runtime is an ordinary fiber replacement — dispose the old, mount the new — with no process restart. Long-running vs. per-call instantiation of a component is a bridge policy per seam (transforms are per-call; a connection holding a live change feed is long-running).

## Distribution

No inseam-specific registry to start. A sandboxed plugin is a `.wasm` + manifest distributed through ecosystems people already use — npm, PyPI, crates.io, OCI registries, or a plain URL. Install = fetch by ref, verify, mount into the composition.

A community registry follows once the WIT projection settles ([positioning](positioning.md)) — the intended path is an agent skill that authors plugins against the contract. Two commitments made now:

- **The contract is proven before the skill ships.** Exit criterion: every service-shaped feature in the shipping distributions reaches the kernel through the seams — the native tier eats its own dog food before the sandboxed tier is invited.
- **Registry trust is a launch requirement, not hardening.** Plugins hold credentials and read personal data, and many will be machine-generated; the registry starts with signed manifests and declared capabilities or it doesn't start.

## Paths not taken

- **Native dynamic libraries.** No sandbox, ABI instability, per-platform builds. Unacceptable for untrusted/AI-generated code; unnecessary for trusted code, which links statically.
- **Sidecar processes over RPC.** Heavier per-plugin cost and larger attack surface; kept as a documented **escape hatch** for the rare integration WASM cannot express (native SDKs, device drivers), mounted through the same bridge pattern.
- **Embedded scripting language (Lua/JS only).** Single-language lock-in contradicts meeting authors where they are.
- **Plugins as config toggles on core features.** The previous architecture's drift: core modules with registries at the edges. Superseded by the substrate — a feature the core hosts is now structurally identical to a feature the community ships.

## Settled since

- **An OpenAI-compatible LLM endpoint is configuration, not a plugin implementation.** The `llm` seam's default provider speaks the OpenAI-compatible protocol to any base URL (OpenRouter, OpenAI, Ollama, vLLM); genuinely novel protocols or bundled models arrive as alternative providers.
- **The transform pathway was proven native-first.** The transform registry with claims/apply and capability-granted LLM handles ran natively before any WASM existed; that contract is what the WIT projection now derives from, rather than an invented one.

## Open questions

- Wasmtime directly vs. a framework layer on top; leaning wasmtime + our own generated WIT world for full contract control.
- Manifest schema: capability grants, version pinning, signing/provenance (matters more as AI-generated plugins circulate).
- Streaming large fetch results across the component boundary without copying whole payloads.
- How sandboxed plugins subscribe to events (project the bus into WIT, or keep components request/response-only and let bridges listen on their behalf).
