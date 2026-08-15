# Positioning

Who inseam is for, what pays for it, and the sequencing that follows. These are product decisions, recorded here because they constrain the architecture as hard as any technical one.

## Audience

Developers and tech-forward users first; infrastructure for products second. The first buyer of the discovery ladder is someone wiring an agent to their own data; the second is someone building a product on the [boundary](access-control.md). Consumer packaging (the macOS app) follows the developer surface, not the other way around.

Consequence: adapters that meet agents where they are (CLI today, HTTP and MCP next — [node-api](node-api.md)) rank above end-user UI.

## Differentiation

Three refusals define the product against the landscape:

- **No content silo.** Sync-everything search products copy content into storage you must trust and fund. inseam replicates addresses + envelopes only ([addressing](addressing.md)).
- **Discovery, not just access.** Per-service integrations answer questions one service at a time; inseam's value is the cross-source [index](discovery.md) that ranks an email against a file against a call.
- **No privileged vendor position.** The hosted offering is an ordinary [node](nodes-and-hosts.md). Anything that only works on the hosted node is a design error.

## Commercialization: the hosted node

Open-source core; revenue from hosting well-provisioned cloud nodes — a strong, always-on index for a user's network. The sale is index quality and uptime. This only stays honest under two architectural invariants:

- The hosted node runs the same open-source software with no private capabilities.
- The per-node [composition](composition.md) asymmetry is the product: weak devices borrow quality from the paid node, so the upgrade is felt on every device without any device being locked in.

## Community plugins are the connector strategy

The long tail of integrations will not be built in-house. The plan: a plugin surface small and typed enough for agents to target ([plugins](plugins.md)), an agent skill that writes plugins for users who need one, and a public registry so plugins written once circulate. Two obligations fall out:

- **Boundary proof before skill.** The plugin contract counts as real only when every service-shaped feature in the shipping distributions reaches the [kernel](kernel.md) through the service seams — the native tier proves the contract before the sandboxed tier is invited. Shipping the plugin-authoring skill before that invites a generation of plugins against a leaky contract.
- **Registry trust from day one.** Machine-generated connectors holding OAuth grants are a supply-chain risk by default; signing and declared capabilities are launch requirements of the registry, not hardening added later ([plugins](plugins.md)).

## Sequencing

1. **Two nodes, honestly.** A local filesystem node and a cloud node, each with its own connections and index, syncing addresses and fanning out discovery to each other. This is the smallest configuration that exercises the novel claims (sync, remote fan-out, index asymmetry). Routing generality — transitive reachability, path selection, steward failover — stays in [network](network.md) as design until this configuration is boringly reliable.
2. **Plugin boundary + first connection plugin**, which forces the real WIT contract ([plugins](plugins.md)).
3. **MCP adapter**, so any agent can climb the discovery ladder without adopting the CLI.

## Paths not taken

- **Consumer-first packaging.** Rejected for now: the app's bar (a flawless Gmail connector on day one) is higher than the developer surface's, and the developer wedge funds the connectors the app needs.
- **Proprietary connectors as the moat.** Rejected: contradicts the community strategy and puts the company on the connector treadmill the plugin system exists to escape.
