# inseam

**All your context, accessible anywhere.** Every email, file, chat thread, ticket, and record you have — wherever it lives — behind one discovery surface your AI can climb from any device, without the content ever leaving where it sits.

`inseam` indexes your data where it sits — a laptop's filesystem, a Gmail account, a service's API — and gives agents, apps, and people one way to find and fetch all of it. It stores addresses and a discovery index, never the content: the network syncs *where data is*, and source material is fetched on demand from where it already lives. The outside world reads only what its verified identity is granted.

What makes "all" and "anywhere" honest is the architecture underneath: everything inseam does is a **plugin** — safe by construction, flexible by contract, and authorable by AI. When your context lives somewhere inseam doesn't reach yet, the answer isn't waiting on a vendor's connector roadmap; it's your own agent writing the plugin and your node mounting it safely, minutes later.

## How it works

Three nouns carry the model:

- A **source** is a unit of data: an email, a file, a chat thread, a database row.
- A **host** is where sources live. Hosts hold data and nothing more — a host doesn't know inseam exists.
- A **node** is a running inseam instance. Nodes steward hosts: publish their sources' addresses, index them, and serve fetches on their behalf.

Nodes connect into a network — yours, a single trust domain. Addresses and their envelopes (small metadata records) sync to every node, so every node knows what exists everywhere; content stays put until something fetches it. Each node is local-first: offline it discovers against its own index, and when connected it can fan queries out to nodes with stronger indexes. A phone keeps a cheap envelope-only index and borrows quality from a bigger node; that asymmetry is the point.

Discovery is a ladder, not a dump. `query` returns ranked addresses with summaries and hints; `expand` and `scan` pull fragments and line ranges from promising results; `fetch` retrieves a full source only when it earns it. An agent climbing this ladder gets progressively closer to source material instead of receiving twenty chunks up front.

Exposure to the outside is by verified property: an external requester proves claims like `email:user@example.com` and can discover and fetch exactly the sources carrying that property — no user database anywhere.

## Plugins all the way down

The plugin model isn't an extension mechanism bolted onto a product — it *is* the product's philosophy. The kernel is deliberately tiny: it runs plugins and owns the store, nothing else. Every capability — connections, transforms, embeddings, ranking, the sweep, the node API, and eventually sync and transports — is a plugin on a typed service seam, composed by a declarative `composition.toml`. Three properties follow:

- **Safe.** Community and AI-written plugins are sandboxed WASM components mounted at runtime: capabilities attenuated to what the manifest declares, fuel-limited, freshly instantiated per call, validated by a conformance harness at authoring, at publish, and again at install. New versions soak through a release cooldown; registry installs are hash-verified against a reviewed index with advisories. A plugin written minutes ago is safe to mount because the *bridge* bounds what it can do — trust is enforced, not assumed.
- **Flexible.** Tier is provenance, not shape: a sandboxed plugin registers into the same seam as one compiled into the binary, indistinguishable to the rest of the node. Swap the embedder, disable a transform, mount a new connection — the composition file is the whole configuration surface, and changing it dirties exactly the sources it touches.
- **AI-authored, self-modifying.** The contracts are small and typed precisely so agents can write against them. The authoring skill (`skills/inseam-loaded-plugin`) walks an agent from golden checks to a validated artifact; the first shipped plugin (`plugins/ocr`) was written by an agent running it. A node built [from source](docs/get-started.md) closes the loop: the agent using inseam is the agent extending inseam — noticing the missing connector, writing it, validating it with `inseam plugin check`, and mounting it into its own composition.

This is what backs the value prop. "All your context" can't be capped by anyone's connector list when the long tail of connectors is written by the community and by your own agents — and "accessible anywhere" holds because those plugins run wherever your nodes do, under the same sandbox everywhere. The full design intent lives in [design/plugins.md](design/plugins.md); the practical docs in [docs/plugins/](docs/plugins/).

## Who it's for

Developers and tech-forward users, in two postures:

- **Wire an agent to everything you have.** Your agents get one permission-aware discovery surface over email, files, chats, tickets — wherever your data already is — instead of a bag of per-service integrations.
- **Build on the network.** The same boundary that serves your agents serves products: a support widget that retrieves a visitor's own correspondence, an app that reads a customer's records where they live.

## Why this and not —

- **Sync-everything search** (recorders, "ingest your data" vector pipelines): those copy your content into another silo you now have to trust and pay to store. inseam replicates addresses and envelopes only; the content never moves.
- **A pile of per-service integrations**: access without discovery. Each connector answers questions about its own service; none can rank an email against a file against a call transcript. inseam's job is the cross-source index over all of it.
- **Hosted enterprise search**: your content on a vendor's hardware, on a vendor's terms. Every inseam node is yours; a hosted node is an option, not the product.

## Open source, and the hosted node

The core is open source, and stays small — the kernel-plus-plugins architecture above is the entire footprint. The long tail of connectors comes from the community and from agents, shared through a public registry whose merge gate is the same conformance harness every node runs at install.

The commercial offering is a hosted, well-provisioned cloud node: a strong always-on index for your network. Architecturally it's an ordinary node with no special powers — the same software this repo builds — so what it sells is index quality and uptime, not lock-in. Nothing in inseam requires it.

## Status

Early, and described as early. The kernel and both plugin tiers are real: everything the node does — the filesystem connection, transforms, embeddings, the Finder, the sweep, the node API — is a plugin on typed service seams over a small kernel that runs plugins and owns the store, composed by a declarative `composition.toml`. Loaded WASM plugins mount at runtime with manifest-attenuated capabilities and a release cooldown; the first (`plugins/ocr`, image OCR through a granted vision-LLM capability) was written by an agent against the authoring skill. The discovery ladder — `query` → `expand`/`scan` → `fetch` — is driveable by a person (`inseam query`), a live LLM (`inseam agent`), or the authenticated web console (`inseam serve`). Multi-node sync and the external property-filtered boundary are designed but not yet built. Start at [docs/get-started.md](docs/get-started.md) (install, first run, self-modification), then [docs/cli.md](docs/cli.md) and [docs/architecture/kernel.md](docs/architecture/kernel.md); intent lives in [design/](design/README.md).
