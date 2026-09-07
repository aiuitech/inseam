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

Stewardship is published, not private: each steward publishes a stewardship record into the [roster](roster.md) — one per connection it registers, withdrawn when the connection goes — so the whole network knows which nodes can serve which hosts and with what capabilities. Host identity is an opaque stable id derived from the host's own identity material ([addressing](addressing.md)); two nodes stewarding the same host derive the same id and the network sees one host with two stewards, catalog entries deduping by host id + locator.

## Paths not taken

- **"Controlled / uncontrolled" hosts.** Rejected: described our relationship to the host and smuggled in a read-only assumption. Writability is a property of the connection, not the host category.
- **"Native host" (a host that *is* a node).** Rejected in favor of the strict separation above: hosts hold data, nodes network. Collapsing them made the co-located case a special case; keeping them separate makes it the ordinary one.

## Settled since

- **Stewardship is published by the roster plugin from the connections registry.** Every registration in `connections` becomes one host record and one stewardship record in this node's log, carrying the connection's capabilities and configured roots and nothing else; a registration that goes away earns a withdrawal, and an unchanged one costs no entry. The registry stays the truth about what this node serves: a stale claim this node once made for a host it no longer holds is never treated as a remote steward. "No live steward" is therefore knowable and reported — a read of a host every steward withdrew from is `UnknownHost`, and one whose stewards were all tried and none answered is `Unreachable`, naming every node tried ([network](network.md)).

## Open questions

- **Steward failover.** A host with two stewards is reached through whichever answers first; a host with one is unreachable while that node is down, and nothing today promotes a second steward. `Unreachable` names the nodes tried, so an owner sees which to bring up; automatic failover would need a second connection to the same host, which is a configuration, not a network act.
