# Hosted Service

The commercial offering: inseam provisions and keeps alive a well-provisioned [node](nodes-and-hosts.md) for someone who does not want to operate one. The node is ordinary — same kernel, same seams, same [composition](composition.md) mechanics — and the customer's [network](network.md) treats it as the always-on backbone the topology already wants.

What is sold is three things, in order of how hard they are to copy: a **tuned composition** that indexes deeply, a set of **inseam-authored plugins** not published as open source, and **operation** — always-on, backed up, patched, provisioned in under a minute. There is no free tier; the open-source core is the free tier ([positioning](positioning.md)).

The control plane, tenant lifecycle, callback router, and metering described here are built in their own repository, [inseam-console](https://github.com/aiuitech/inseam-console), which depends on this one and never the other way round. This repository carries only what a node itself needs to be hostable — `inseam serve`, `inseam self update`, the `deploy/hosted/` self-hosting profile — so that nothing about the commercial service leaks into an ordinary node.

## One tenant, one virtual machine

The tenant boundary is a machine boundary, not a namespace. A hosted node holds live OAuth refresh tokens for a person's mail and files, their whole index, and executes community- and agent-authored WASM. The loaded tier's sandbox answers *intra*-node risk ([plugins](plugins.md)); it says nothing about tenant-to-tenant, and a shared-kernel escape would surrender every customer's grants at once. Containers on a shared kernel are therefore not a candidate, and the audience — developers who know what namespace isolation is worth — is the last one to sell it to.

The upside is that the hosted node becomes literally the thing a self-hoster runs, which is what makes "an ordinary node" a mechanism rather than a claim.

It also settles how a tenant extends their node: not by rebuilding the image — that is the operator's lever, for releases — but by uploading a loaded plugin through the console, which the node mounts into itself at runtime ([composition](composition.md), [plugins](plugins.md)). The control plane is not involved, which is the point: the plugin, like the index, is the tenant's.

## The volume is the tenant; the server is cattle

The node's data directory lives on an attached block volume, never on the server's boot disk. Every operation an operator needs is then one primitive over that split:

| Operation | Mechanism |
| --- | --- |
| release | the node swaps its own binary from the signed manifest ([releases](releases.md)); the server is untouched |
| rebuild | recreate the server from the base image, reattach the volume — for OS base changes and repair, never for releases |
| suspend | detach the volume, delete the server |
| archive | snapshot the volume, delete the volume |
| resize | change the server type, reattach |
| restore | create a server from the image, attach the volume |
| patch the OS | does not exist — servers are rebuilt from a fresh base image, never edited |

Nothing is configured in place, so nothing drifts, and there is no configuration-management layer to own. The base image contains no `inseam` binary: first boot fetches the version the release manifest names for the node's cohort, so a freshly rebuilt server converges to the same version a long-lived one updated itself to, through one mechanism. A node that misbehaves is deleted and recreated against the same volume. The cost model follows the same split: a *powered-off* server still bills (its resources stay reserved), so suspension means deleting the server and keeping the volume, and archival means keeping only a snapshot.

Hetzner Cloud is the first provider — its API covers server, volume, image, and firewall lifecycle, and its per-node cost leaves the margin a flat rate needs. Nothing above is Hetzner-specific; the provider is one adapter in the control plane.

## The control plane cannot read the nodes

The control plane provisions, meters, and bills. It must not be able to read what it provisions, and this is an architectural invariant rather than an operational preference: the whole positioning rests on the customer's data being theirs, and a claim that can be quietly violated by an operator's SSH key is not backed by mechanism ([access-control](access-control.md) makes the same move at the boundary).

Concretely: no standing key into a tenant machine; the volume is encrypted with a per-tenant key the control plane does not hold in the clear; the owner token is minted by the node at first boot and claimed once by the customer, never stored in a form the control plane can use; and support access is a customer-initiated, expiring grant. The control plane knows a node's size, version, health, and bill. It does not know its contents.

## Updates are pulled, not pushed

Push-based rollout requires exactly the standing credential the invariant above forbids, so releases go the other way. Inseam publishes a **signed release manifest** at the hosted origin naming a version per cohort ([releases](releases.md)). A supervisor timer on each node runs `inseam self update` inside the tenant's maintenance window: it verifies the signature, swaps the binary, and restarts. The node reports its running version to the control plane's status endpoint, and the **response carries the node's cohort** — the one piece of release state the control plane owns. Staged rollout is the control plane moving tenants between cohorts; rollback is promoting the previous version again. The control plane can choose *which signed version* a node runs and nothing else, which is precisely its legitimate power.

Two things are deliberately separate here. A **release** is a binary swap the node performs on itself; a **rebuild** is the control plane recreating the server from the base image against the same volume, reserved for base-image changes and repair. Only the first happens on inseam's release cadence, and the server is never touched for it. A node cannot rebuild itself — a Hetzner token is project-scoped, and a tenant machine holding one could delete every other tenant's server — so the split is forced by the credential model, not chosen for tidiness.

This costs a delay between publish and convergence and gains three things: the invariant holds, there is no orchestrator, and the mechanism does not care whether there are ten nodes or ten thousand. It is the same shape the loaded tier's release cooldown already takes — the node decides when code activates, using a clock it owns.

## The OAuth callback router

A hosted node lives at `<tenant>.inseam.io`, and Google matches redirect URIs exactly, so per-tenant registration on one Web client does not scale. The answer is a fixed callback at `auth.inseam.io/callback` that reads the tenant out of the CSRF `state` it issued and **302s the browser** to that node's own `/api/v1/oauth/callback` with the code intact.

This revisits a decision [connections](connections.md) recorded as not taken — "a central callback broker relaying codes to nodes" — and it is a narrower thing than what was rejected there. The router never receives the code server-side: it redirects a browser that is already carrying it, the same way the provider did. It holds no tokens, performs no exchange, and cannot replay anything. What it does add is a name-resolution service in the sign-in path, which is a real availability dependency and is accepted as one — sign-in for hosted tenants fails while it is down, existing grants keep working. Self-hosted nodes are unaffected; they register their own redirect URI directly, as today.

## Telephony is a subaccount per tenant

Call capture ([call-capture](call-capture.md)) gives every hosted node a phone number. The credential that reaches it is the same shape as the volume key: **one Twilio subaccount per tenant**, created by the control plane at provision time, with the number bought inside it, a transcription service created with the tenant's own key, and an **API key scoped to the subaccount** handed to the node through cloud-init. The node reaches one account; the master credentials never leave the control plane. The key's secret is returned once and never stored — a rebuild mints a new one and deletes the old, like the node token — so the control plane holds SIDs and nothing that opens a tenant's calls.

Recordings rest in Twilio under an account the master credentials can open, which is the one place the invariant above would leak. The node closes it: a recording is deleted from Twilio the moment the node's archive has the audio and the transcript has settled, so the window is minutes and the only durable copy is on the tenant's volume. Suspend and archive suspend the subaccount (calls stop; the number is kept and still billed); destroy releases the number and closes the subaccount, which is permanent. The number's inbound instructions are one static TwiML document the console serves for every tenant alike, content-free by construction. *Rejected:* the master credentials on every node (one compromised node reads every tenant's calls — the Hetzner-token argument again), and a separate Twilio account per tenant (a billing relationship each; subaccounts isolate under one bill).

## Private plugins are the moat

Inseam authors plugins it does not publish, and ships them to the hosted distribution. This reverses [positioning](positioning.md)'s earlier rejection of proprietary connectors, and the reasons are recorded there. The mechanism is not new: a **custom distribution** — a private repo of linked plugins compiled from source, inheriting the conformance battery through the shared harness — is already the professional-configuration path for the linked tier ([plugins](plugins.md)). The hosted distribution is one.

One rule keeps this from rotting the plugin story:

> The moat is *which plugins exist*, never *what the platform lets a plugin do*.

Private implementations, public seams. There is no seam, capability, or kernel affordance available to an inseam-authored plugin and not to a community one; a private plugin passes the same conformance harness as any other; and anyone can write a competing plugin against the same published contract and get identical behavior from the same node. The design error [positioning](positioning.md) names is a *privileged seam*, not a proprietary implementation — a distinction worth stating precisely, because the first would make every community plugin a second-class citizen and the second does not.

The hosted distribution is its own repository — the private linked plugins and a three-line binary pinning `inseam-cli` by tag — built by the public release pipeline's reusable build job and promoted to cohorts by an operator who holds the signing key off the build machine ([releases](releases.md)). Updating the core and updating a private plugin are the same motion: tag, build, promote.

Which tier a private plugin uses is decided by the existing rule, not by commercial preference: **linked** where the boundary cost bites (the indexing hot path, embedders, finder internals), **loaded** everywhere else. Preferring loaded matters more than it looks — a loaded private plugin is an artifact delivered through an entitled channel, so one binary runs everywhere and entitlement stays a licensing fact rather than a code fork. Linked private plugins mean the hosted binary genuinely differs from the open-source one, which is honest open core but must be *said* rather than implied away, and which forecloses ever selling a plugin pack to a self-hosted node.

The credibility risk is real and is managed rather than denied: a platform whose best plugins are reserved invites community authors to ask why they should contribute. The answers are that inseam keeps publishing plugins openly, that the contract is public enough to compete against, and that the reserved ones are the ones inseam operates and supports.

## Metering

Flat rate including an index-volume allowance, then per-gigabyte above it.

The meter is **gigabytes of index volume** because it is a number the customer can verify for themselves (`inseam status` reports store and content sizes) and it maps one-to-one onto a line item inseam actually pays. Counting sources is the more native unit and tracks cost worse: the expensive resource is indexing work, which scales with churn rather than with resident size.

Churn is therefore not metered but *bounded*, using a knob that already exists: the sweep's deep budget (catalog-only, per-run source caps — [index-maintenance](index-maintenance.md)) is a tier property. A plan buys an allowance and a budget, and the composition asymmetry that [positioning](positioning.md) calls the product is the same dial.

Volumes grow but do not shrink, so a customer who deletes half their catalog cannot shrink their bill without a volume migration. Metering *provisioned* gigabytes is simpler and makes that visible; metering *used* gigabytes is kinder and leaves inseam holding the gap. Open below.

## Paths not taken

- **Containers on a shared kernel (Kubernetes, a container PaaS).** Denser and cheaper, and the isolation boundary is a kernel shared with every other tenant's OAuth grants and untrusted plugin code. Density does not pay for itself at this price point.
- **Scale-to-zero / suspend-on-idle (Fly Machines and similar).** The product *is* the always-on backbone through which roster and catalog updates converge ([network](network.md)); a node that sleeps is not one. It would also have put the fleet's reachability on a platform whose UDP path is its weakest, and node↔node transport is QUIC ([connections](connections.md)).
- **A free tier.** Rejected: the open-source core is the free tier, and a permanently-provisioned machine per free user has no cost floor. This is what makes always-on virtual machines affordable at all.
- **Push-based updates.** Requires a standing credential into every tenant machine, contradicting the control-plane invariant above.
- **Nodes rebuilding their own servers.** Would require each tenant machine to hold a project-scoped provider token, which is a credential over every other tenant. Nodes swap binaries; the control plane rebuilds servers.
- **A Cloudflare-proxied tenant hostname.** Proxying `<tenant>.inseam.io` would terminate the tenant's TLS on infrastructure the operator controls, which is the invariant violated by another name, and would not carry node↔node QUIC. Tenant records are DNS-only and the node holds its own certificate.
- **A private fork of the kernel or private seams.** Rejected as the actual design error [positioning](positioning.md) warns about; the custom-distribution path gets the same commercial result without it.

## Open questions

- Provisioned versus used gigabytes as the billed quantity, and whether volume shrink is worth a migration path.
- Metering call minutes: the number, the two legs, recording, and transcription are per-use costs the flat rate does not cover; a pass-through line or a monthly minute allowance both fit the existing meter shape ([call-capture](call-capture.md)).
- Where the per-tenant volume key lives such that the control plane can attach a volume it cannot decrypt, and what recovery looks like when a customer loses their owner token.
- Whether `auth.inseam.io` should also serve self-hosted nodes as an opt-in convenience, which would make it a shared dependency the design currently avoids.
- Cohort assignment policy for releases: how long canary runs, and what health signal promotes it.
- Whether entitled loaded plugins ever become purchasable for self-hosted nodes, which would need an entitlement check the registry does not have.
