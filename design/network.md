# Network

An inseam network is a graph of [nodes](nodes-and-hosts.md). There is no central authority; our own hosted service is just a well-provisioned node like any other.

The network is an **intranet**: all nodes belong to one owner and form a single trust domain ([access-control](access-control.md)). Node↔node traffic is unrestricted; restriction exists only at the boundary with the outside.

## Topology

Each node holds [connections](connections.md) to one or more other nodes (and to the hosts it stewards). The graph is arbitrary: mesh, hub-and-spoke, chain — whatever connections the user establishes.

## Transitive reachability

Nodes do not need direct connections to reach each other. If A connects to B and B connects to C, then A can discover and fetch from C **by way of B**. Nodes route requests for hosts they cannot reach directly through connected nodes that can.

Three kinds of knowledge with different scope:

- **Addresses are global.** [Address sync](address-sync.md) converges every node on the full catalog, so every node knows what exists everywhere.
- **Reachability facts are global.** The [roster](roster.md) converges every node on who exists, their dialable endpoints, and who stewards what — so any node can *attempt* to reach any other, and knows which node a fetch must ultimately land on.
- **Sessions are local.** Which connections are actually live right now is known only to the nodes holding them. Reaching a host means dialing roster endpoints and routing hop-by-hop toward a live steward; liveness is learned by trying, never read from a synced record.

## The backbone convention

Because unstable nodes (rotating IPs, NAT, intermittent power) only converge while some mutual peer is reachable, every network is encouraged to include at least one **always-on node with a stable endpoint** — typically hosted. It acts as the reliable meeting point through which roster and catalog updates flow. This is a deployment convention, not architecture: it is an ordinary node ([roster](roster.md) covers the mechanics), and the network functions without it, just with slower convergence.

## Boundary requests route too

External requesters enter the network through whichever node they reach, but the data they're after may live anywhere. Their requests route like any other — with the requester's verified properties carried along, and the [access-control](access-control.md) filter applied by the serving node wherever the request lands. Internal requests between nodes carry no filter.

## Paths not taken

- **Central relay/store.** Rejected: contradicts local-first and "data doesn't move"; a hosted node may *act* as a popular hub, but nothing in the architecture requires one.
- **DHT-style partial knowledge (for now).** Personal networks are small (tens of nodes, not millions); full catalog replication is simpler and enables offline discovery. Revisit if network sizes demand it.

## Open questions

- Route selection when multiple paths to a host exist (the roster names the candidate stewards; prefer a live session? shortest? healthiest?).
- Loop prevention and hop limits for forwarded requests.
- Semantics when a host is unreachable (stale catalog entries, error surfaces).
