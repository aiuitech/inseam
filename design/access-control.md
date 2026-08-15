# Access Control

## Inside the network: trusted by construction

An inseam network is an **intranet** — a single trust domain. All source data within the network is declared trusted: if you can set up a node and its connections, you have by that act declared access. There are no internal permissions, no per-node visibility rules, and no user accounts. Every node sees the full catalog and may fetch anything ([address-sync](address-sync.md) relies on this).

Overlapping or federated networks (connecting two people's networks) would break this assumption and are **explicitly out of scope for now** — inseam is a personal network.

## The boundary: exposure by verified property

Access control exists for one purpose: deciding what the network exposes to the **outside**. An external requester — an app, a service, a person's session — is not a node and never participates in sync. It reaches the network through a node and presents **verified properties**: `key:value` claims like `email:user@example.com`, proven to some node's satisfaction.

### Properties attach at two levels

- **Host level**: a property on a [host](nodes-and-hosts.md) covers all sources it holds.
- **Source level**: a property on an individual source's [envelope](addressing.md) — e.g. an email source carries `email:user@example.com` because that address was a correspondent.

### Trust levels

Every attached property records *how* it came to be trusted:

- **Verified**: proven by a defined method — OAuth against the provider, a confirmation-link flow, (later) cryptographic attestation. A verification records the method, the verifier, and an **expiry**; verified properties age out and must be re-proven.
- **Claimed**: asserted without proof, typically by the owner. Explicitly weaker; boundary policy can distinguish the two.

Verification methods are extensible via [plugins](plugins.md).

### The matching rule

> An external requester holding verified property P may discover and fetch sources that carry property P (on their envelope or via their host).

Additionally, a host or source may be marked **public**: exposed to any external requester with no property match required. Public is an explicit, owner-declared exposure — nothing is public by default. A boundary query therefore sees the network's public data plus whatever its verified properties unlock.

The property filter travels with the request: wherever a boundary request routes ([network](network.md)), the serving node applies the same rule, and [discovery](discovery.md) filters results before addresses are ever revealed. Internal node↔node traffic carries no such filter.

Mechanically, enforcement is a guard chain on the `operations` seam ([node-api](node-api.md), [services](services.md)): boundary policy listeners run on every dispatch, any listener may deny, and **no listener can force-allow what another denied** — the rule composes monotonically as plugins add policy.

## Worked example

A third-party chat widget on my website verifies visitors by email, and its backend reaches my inseam network through one of my nodes. A visitor's session arrives as an external requester with verified `email:user@example.com`. My Gmail is a host in the network, and every Gmail source where that address was a correspondent carries `email:user@example.com` on its envelope. Result: the chat session can discover and fetch exactly that visitor's correspondence with me — nothing else — with no user database anywhere.

## Paths not taken

- **User accounts / identity records.** Identity is an emergent view — the set of properties a requester has verified — not a stored object. This is what lets unrelated services interoperate without shared registration.
- **ACLs naming principals.** Same problem in different clothes; property matching composes across services, principal lists don't.
- **Internal access control between nodes.** Rejected: the intranet assumption makes it dead weight, and the setup act itself is the grant. Revisit only if federated networks ever land.

## Open questions

- Which nodes may act as boundary verifiers, and whether one node honors a verification performed by another (within a personal network: presumably yes, since all nodes share an owner).
- Revocation before expiry, and how revocations propagate.
- Property namespace conventions (`email:`, `domain:`, `device:` …) and wildcard/pattern grants.
- Whether exposure can require *combinations* of properties (AND/OR policies) rather than single-property match.
