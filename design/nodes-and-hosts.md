# Sources, Hosts, and Nodes

The three load-bearing nouns in inseam.

## Source

A **source** is a piece of data: an email, a file, a chat thread, a database row. Sources are what addresses point at and what discovery finds. Full treatment in [addressing](addressing.md).

## Host

A **host** is where sources live: a machine's filesystem, a Gmail account, a SaaS API, a website's database. Hosts hold data and nothing more — a host has no inseam machinery and does not need to know inseam exists. Every address names the host its source lives on.

## Node

A **node** is a running inseam instance: a participant in the network. Nodes hold [connections](connections.md), sync the address catalog, maintain a [discovery index](discovery.md), route queries and fetches, guard the network [boundary](access-control.md), and run [plugins](plugins.md).

## Hosts and nodes are different, even on the same machine

A node often runs on the same machine as a host, but they remain distinct things. An inseam node on a macOS desktop shares the machine's address with the filesystem host it serves — but the host's job is holding sources on the filesystem, while the node's job is networking: it reads that host through a local connection and gives it presence on the inseam network (WebSockets, HTTP) that the host itself never had.

## Every host joins through a node

Hosts never speak for themselves; a node does it for them. The node holding the connection to a host is its **steward**: it publishes the host's addresses into the catalog, indexes it, and serves fetches from it to the network.

A node may steward several hosts — the macOS node above might serve the local filesystem *and* a Gmail account it holds an OAuth grant for. The only difference between the two is the connection protocol:

- a **local host** is reached through local protocols (filesystem, OS APIs);
- a **remote host** is reached through a service's own protocol (OAuth + REST, IMAP, …).

The model is uniform: one concept of host, one stewarding relationship, different connection plugins.

## Paths not taken

- **"Controlled / uncontrolled" hosts.** Rejected: described our relationship to the host and smuggled in a read-only assumption. Writability is a property of the connection, not the host category.
- **"Native host" (a host that *is* a node).** Rejected in favor of the strict separation above: hosts hold data, nodes network. Collapsing them made the co-located case a special case; keeping them separate makes it the ordinary one.

## Open questions

- **Host identity.** A stable host ID scheme is needed, especially so two nodes that both connect to the same Gmail account resolve to *one* host, not two.
- **Multiple stewards.** If two nodes steward the same host, who publishes? Likely: both may, catalog entries dedupe by host ID + locator, and routing may prefer any steward.
- **Steward failover.** A host is only reachable while some steward is; unreachability semantics belong in [network](network.md).
