# Positioning

Who inseam is for, what pays for it, and the sequencing that follows. These are product decisions, recorded here because they constrain the architecture as hard as any technical one.

## Audience

Developers and tech-forward users first; infrastructure for products second. The first buyer of the discovery ladder is someone wiring an agent to their own data; the second is someone building a product on the [boundary](access-control.md). Consumer packaging (the macOS app) follows the developer surface, not the other way around.

Consequence: adapters that meet agents where they are (CLI today, HTTP and MCP next — [node-api](node-api.md)) rank above end-user UI.

## Differentiation

Three refusals define the product against the landscape:

- **No content silo.** Sync-everything search products copy content into storage you must trust and fund. inseam replicates addresses + envelopes only ([addressing](addressing.md)).
- **Discovery, not just access.** Per-service integrations answer questions one service at a time; inseam's value is the cross-source [index](discovery.md) that ranks an email against a file against a call.
- **No privileged vendor position.** The hosted offering is an ordinary [node](nodes-and-hosts.md). Any *platform capability* that only works on the hosted node is a design error — a seam, a kernel affordance, or a plugin power reserved to inseam's own code. Inseam-authored plugins that are not published are not that; they run on the same contract everyone else targets ([hosted-service](hosted-service.md)).

## Commercialization: the hosted node

Open-source core; revenue from hosting well-provisioned cloud nodes — a strong, always-on index for a user's network. The sale is index quality, reserved plugins, and uptime, on a flat rate with an index-volume allowance and no free tier ([hosted-service](hosted-service.md) has the shape). This only stays honest under three architectural invariants:

- The hosted node runs the open-source kernel and seams, unmodified. What it adds is composition and plugins, never platform.
- Private plugins are implementations, not privileges: the same conformance harness, the same published contract, no capability a community plugin cannot reach.
- The per-node [composition](composition.md) asymmetry is the product: weak devices borrow quality from the paid node, so the upgrade is felt on every device without any device being locked in.

The phrasing that matters for surfaces: the hosted node has no special *powers*, and it does have plugins we wrote and did not publish. Both halves get said. Claiming it runs nothing but published code would be false ([PRODUCT.md](../PRODUCT.md) Principle 1).

## Community plugins are the connector strategy

The long tail of integrations will not be built in-house. The plan: a plugin surface small and typed enough for agents to target ([plugins](plugins.md)), an agent skill that writes plugins for users who need one, and a public registry so plugins written once circulate. Two obligations fall out:

- **Boundary proof before skill.** The plugin contract counts as real only when every service-shaped feature in the shipping distributions reaches the [kernel](kernel.md) through the service seams — the linked tier proves the contract before the loaded tier is invited. Shipping the plugin-authoring skill before that invites a generation of plugins against a leaky contract.
- **Registry trust from day one.** Machine-generated connectors holding OAuth grants are a supply-chain risk by default; signing and declared capabilities are launch requirements of the registry, not hardening added later ([plugins](plugins.md)).

## Sequencing

1. **Two nodes, honestly.** A local filesystem node and a cloud node, each with its own connections and index, syncing addresses and fanning out discovery to each other. This is the smallest configuration that exercises the novel claims (sync, remote fan-out, index asymmetry). Routing generality — transitive reachability, path selection, steward failover — stays in [network](network.md) as design until this configuration is boringly reliable.
2. **Plugin boundary + first connection plugin**, which forces the real WIT contract ([plugins](plugins.md)).
3. **MCP adapter**, so any agent can climb the discovery ladder without adopting the CLI.

## Paths not taken

- **Consumer-first packaging.** Rejected for now: the app's bar (a flawless Gmail connector on day one) is higher than the developer surface's, and the developer wedge funds the connectors the app needs.
- **Proprietary connectors as the *whole* moat.** Still rejected in its original form — a company whose integrations are all private is back on the connector treadmill the plugin system exists to escape, and has nothing to offer the community authors it depends on. Superseded in part; see below.
- **Private seams or a private kernel fork.** Rejected: this is the actual privileged-vendor failure. A reserved plugin competes on its implementation; a reserved seam makes every community plugin second-class and quietly ends the ecosystem.

## Settled since

- **Reserved plugins are a moat; reserved platform is not.** The original rejection of proprietary connectors was one decision doing two jobs, and only one of them survives. Keeping the *contract* open is what the community strategy needs — authors target a published seam, agents generate against it, plugins circulate. Keeping every *implementation* open was never load-bearing for that, and it left the hosted node selling operations alone, which is thin and copyable. So: inseam publishes plugins openly and also authors plugins it reserves for the hosted distribution, through the custom-distribution path the linked tier already defines ([plugins](plugins.md)) — no new mechanism, and the conformance harness applies unchanged. The treadmill worry is answered by the split rather than by abstinence: the long tail stays community-written because the contract is public, while the depth inseam builds itself is what a hosted plan buys. The cost is accepted and stated plainly — the hosted binary is not the published binary, "open core" is the accurate word, and surfaces say so ([hosted-service](hosted-service.md)).
