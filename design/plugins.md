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

Tier is **provenance, not shape** — both tiers register into the same seams and are indistinguishable to consumers. The tiers are named for *when code enters the node*: **linked** at build, **loaded** at run. The earlier names — "native"/"sandboxed" — were retired because they read as a capability difference; both tiers have the full seam surface, and the sandbox is a property of the loaded tier's isolation boundary, not a smaller API.

### Linked plugins (trusted)

First-party and vetted code: Rust, statically linked into the binary, activated by composition. In-process trait calls, zero boundary cost — the tier the indexing hot path, the finder, the store-adjacent machinery live in. Trusted does not mean undisciplined: injections are still the only reach a plugin has, which keeps linked plugins auditable, swap-safe, and honest about their dependencies.

A **distribution** is an app crate that links a set of linked plugins and ships a base composition: the CLI, the macOS app, a future headless daemon are distributions of the same kernel ([runtime](runtime.md)). A **custom distribution** is the professional-configuration path for the linked tier — a private repo of trusted plugins compiled in from source (`docs/plugins/distributions.md`) — and inherits the same conformance battery through the shared harness crate.

### Loaded plugins (untrusted)

Community and machine-generated code: **WASM components against WIT interfaces**, loaded at runtime.

- **Sandboxed by construction** — a component sees only the capabilities the bridge hands it; critical when most third-party plugins hold credentials, read personal data, and were written by an agent.
- **Any language** — anything that compiles to a WASM component, so authors and AI agents work in what they know.
- **One artifact** — a `.wasm` + manifest runs identically on every node.

A loaded plugin is mounted by the **plugin-host bridge**, itself a linked plugin: it reads the manifest (which seams the component implements, which capabilities it requests — down to which external hosts it may call), instantiates the component, and adapts WIT ↔ the native service traits. Capability attenuation is interception at the bridge: budgets, host allowlists, and credential scoping are applied to the handles the component receives, not enforced inside it. Plugins get no raw sockets; the kernel side owns the network stack and the component describes requests — both the sandbox story and the workaround for WASI's immature networking.

**The WIT contract is generated from the service definitions**, not designed separately: the native seam is the source of truth, and the WIT world is its projection across the boundary. This retires the old two-registry risk — there is one seam, and the bridge is just another provider/consumer on it.

## Lifecycle

Uniform across tiers, owned by the substrate ([kernel](kernel.md)): reactive activation on injected services, restart on provider identity change, effect-unwind on unload, per-fiber failure containment, loud missing-dependency errors at composition settle. Loaded plugins additionally get *code* dynamism: install, upgrade, and reload of a `.wasm` artifact at runtime is an ordinary fiber replacement — dispose the old, mount the new — with no process restart. Long-running vs. per-call instantiation of a component is a bridge policy per seam (transforms are per-call; a connection holding a live change feed is long-running).

## Distribution

No inseam-specific registry to start. A loaded plugin is a `.wasm` + manifest distributed through ecosystems people already use — npm, PyPI, crates.io, OCI registries, or a plain URL. Install = fetch by ref, verify, mount into the composition.

A community registry follows once the WIT projection settles ([positioning](positioning.md)) — the intended path is an agent skill that authors plugins against the contract. Two commitments made now:

- **The contract is proven before the skill ships.** Exit criterion: every service-shaped feature in the shipping distributions reaches the kernel through the seams — the linked tier eats its own dog food before the loaded tier is invited.
- **Registry trust is a launch requirement, not hardening.** Plugins hold credentials and read personal data, and many will be machine-generated; the registry starts with signed manifests, declared capabilities, and an advisory/yank feed or it doesn't start.

### Release cooldown

New versions of loaded plugins do not activate immediately. A node holds each newly observed version in **cooldown** for a configurable window (per node, with a sane default; the whole mechanism applies to the loaded tier only — linked plugins ride deliberate binary updates). The point is to let the ecosystem's detection outrun the attacker's distribution: most registry supply-chain compromises are caught within days, by which time a cooled-down version has never run anywhere. Prior art: pnpm's `minimumReleaseAge`, adopted ecosystem-wide after the 2025 npm worm attacks.

The rules that make it hold:

- **The clock is locally unforgeable.** Cooldown counts from when *this node* first observed the version — a manifest's self-claimed release date is worthless. A registry-signed publish timestamp (transparency log) may shorten the wait for versions that are already old, never lengthen a claim into an exemption.
- **Cooldown ends with a check, not a timer.** At activation the node re-checks the registry's advisory feed; a version yanked or flagged during its window never activates. Delay without a detection channel is just hoping the owner reads the news.
- **Capability widening is its own gate.** A version whose manifest requests capabilities its predecessor didn't (new hosts, new seams) requires explicit owner approval regardless of soak time — the diff, not the clock, is the question.
- **The running version keeps running.** Cooldown delays upgrades and fresh installs; it never deactivates what is already active. Activation after cooldown is an ordinary fiber replacement.
- **The owner can override.** Sovereignty stands: an explicit per-install override activates immediately, with the prompt stating the version's age plainly. Overriding is a consent moment, not a config default.

Known trade-off: security *fixes* are delayed by the same window. An expedite path — a registry-signed advisory that marks a version as a vetted fix — is the likely answer and stays open below.

## Paths not taken

- **Native dynamic libraries.** No sandbox, ABI instability, per-platform builds. Unacceptable for untrusted/AI-generated code; unnecessary for trusted code, which links statically.
- **Sidecar processes over RPC.** Heavier per-plugin cost and larger attack surface; kept as a documented **escape hatch** for the rare integration WASM cannot express (native SDKs, device drivers), mounted through the same bridge pattern.
- **Embedded scripting language (Lua/JS only).** Single-language lock-in contradicts meeting authors where they are.
- **Plugins as config toggles on core features.** The previous architecture's drift: core modules with registries at the edges. Superseded by the substrate — a feature the core hosts is now structurally identical to a feature the community ships.

## Settled since

- **The substrate, both tiers, and the cooldown shipped.** Linked plugins are modules in `inseam-plugins` behind one factory registry; the wasm bridge (`inseam-wasm-host`) mounts `wasm:<artifact>` composition entries with manifest-attenuated capabilities, declared∩exported claims, per-call instantiation, fuel limits, and the release cooldown exactly as specified below (local first-seen clock in the kernel's state service, capability-widening gate, `allow_new` as the explicit consent moment). The wasip2 toolchain forces one nuance: components import core WASI interfaces, which the bridge satisfies with an **empty** context — no preopens, env, args, or sockets — so the effective surface stays the host interface.
- **The WIT projection is hand-maintained, not generated — yet.** `wit/transform.wit` mirrors `inseam-seams::transforms` and is kept in lockstep by review; generating it from the seam definitions remains the intent once more than one seam crosses the boundary. v1 also omits entity emission (loaded transforms emit fragments only); entity extraction stays linked until the WIT vocabulary earns it.
- **Wasmtime directly** (no framework layer), with our own WIT world — resolved from the open question below.
- **The authoring loop is skill-shaped already.** `skills/inseam-loaded-plugin` authors against the contract, and `plugins/ocr` (image OCR through the granted vision LLM) was produced by an agent running it — a working rehearsal of the community-registry path.
- **Plugin fitness is enforced, not reviewed.** One conformance harness (`inseam plugin check`, [registry.md](registry.md)) runs at authoring time, in registry CI, and at **install-time admission** on every node: the first sighting of an artifact runs static checks, a real bridge mount, a hostile-input battery (degrade, never trap — a guest panic is a trap), and the plugin's own **golden checks** — declarative input→expected-shape cases in `<name>.checks.toml`, executed with canned capabilities so they are data to run, not code to trust. Admission verdicts are cached by content hash in the node's state; failure refuses the mount with the failing check named (`admission = "warn" | "off"` per entry to override). Linked plugins get the mirror-image guarantee from the conformance harness crate (`inseam-conformance`), whose sweep over `factories()` runs in the workspace suite — and in any custom distribution's — so a plugin cannot be linked without inheriting it.
- **Golden checks are mandatory, and "has a checks file" is not the bar.** Everything is a plugin, so plugin fitness *is* system fitness — and the authors we design for are agents, for whom a test written first is both the specification and the guardrail. The harness therefore fails (not warns) a plugin that ships no checks, and enforces a small coverage rule on the ones it ships: at least one check that *proves the claim* (a substantive expectation about what came out) and one that *pins the degrade path* (input starved, output bounded). The rule is deliberately minimal: the built-in hostile battery already proves "doesn't crash", so demanding many checks would only invite padding; demanding two specific kinds closes the two ways a vacuous test passes. The schema, matcher, and rule live once (`inseam_conformance::golden`) and apply to both tiers — a linked transform cannot register without its own checks file either. Failure reports print what the plugin actually emitted next to what was expected, because the author reading them is mid-loop and should not need to add logging to see the gap. *Rejected:* code-based tests for loaded plugins (a node can't re-run what it can't trust; data can be re-run anywhere), and review-time test requirements (enforced-at-admission is the only rule every node can verify for itself).
- **Registry v0 is the repository itself** — `plugins/` with a sha256 index, an advisory feed, reproducible-build + harness + AI-review CI gates, and `inseam plugin install` verifying everything locally ([registry.md](registry.md)).
- **An OpenAI-compatible LLM endpoint is configuration, not a plugin implementation.** The `llm` seam's default provider speaks the OpenAI-compatible protocol to any base URL (OpenRouter, OpenAI, Ollama, vLLM); genuinely novel protocols or bundled models arrive as alternative providers.
- **Custom distributions are a supported path, not a fork.** `inseam-cli` is a library plus a three-line binary: `inseam_cli::run(Distribution::first_party())`. A private distribution — e.g. a commercial cloud node shipping its own performant linked plugins — is its own thin binary crate calling the same `run` with `.with_factories(...)` and `.with_base_entries(...)`, built from a cloned private repo with `cargo install --path .` (`docs/plugins/distributions.md`). The conformance battery moved into `inseam-conformance` so that path carries the linked tier's validation guarantee with it; the registry stays a loaded-tier mechanism, and trusted private code is distributed as source, never as registry artifacts.
- **The transform pathway was proven native-first.** The transform registry with claims/apply and capability-granted LLM handles ran natively before any WASM existed; that contract is what the WIT projection now derives from, rather than an invented one.

## Open questions

- Manifest signing/provenance: the registry's sha256 index covers integrity; per-publisher signatures arrive with the registry's graduation path ([registry.md](registry.md)).
- Cooldown expedite path: how a registry-signed advisory vouches a security fix past the window without becoming a bypass an attacker can earn.
- Wiring the cooldown's end-of-window check to the registry's advisory feed (`plugins/advisories.toml`) — install consults it today; the mount-time re-check does not yet. Plain-URL installs still have no feed to ask.
- Streaming large fetch results across the component boundary without copying whole payloads.
- How loaded plugins subscribe to events (project the bus into WIT, or keep components request/response-only and let bridges listen on their behalf).
