# Network

An inseam network is a set of nodes that belong to one owner and form one trust domain. Inside it nothing is filtered: every node holds the whole catalog and the whole roster, and any node can read any host through the node that stewards it. There is no central authority; a hosted node is an ordinary node that happens to stay on ([design/network.md](../../design/network.md)).

Three kinds of knowledge, three scopes:

- **Addresses are global.** The catalog — every address with its envelope — replicates to every node through [sync](sync.md). Content never does.
- **Reachability is global.** The [roster](roster.md) — which nodes exist, how each is dialed, which hosts exist, who stewards what — replicates the same way, as more record kinds in the same log.
- **Sessions are local.** Which peers are live right now is known only to the node holding the connection. Nothing synced says "online"; liveness is learned by dialing.

Five composition entries, enabled by default in the stock CLI and the apps:

| Page | Entry | Plugin | What it does |
| --- | --- | --- | --- |
| [identity.md](identity.md) | `node` | `node` | the keypair that is the node's identity, its display name, and the capabilities it advertises |
| [transport.md](transport.md) | `transport` | `transport-iroh` | QUIC dialed by public key, relays, the admission handshake, sessions |
| [roster.md](roster.md) | `roster` | `roster` | the node, host, and stewardship records this node publishes; invitations and expulsions |
| [sync.md](sync.md) | `sync` | `sync` | the replicated log and the exchange that converges it |
| [routing.md](routing.md) | `routing` | `routing` | reading a source another node stewards, directly or through a relay |
| [discovery.md](discovery.md) | `routing` | `routing` | fanning a query out to other nodes' indexes and merging the answers |

[joining.md](joining.md) walks through setting one up: a backbone, an invitation, a laptop that joins, a device that is expelled. The owner's commands are `inseam network` and its subcommands ([../cli.md](../cli.md)); the HTTP owner API carries the same operations ([../architecture/hosted-node.md](../architecture/hosted-node.md#network)); the configuration is five entries ([../configuration.md](../configuration.md)). The reasoning is in [design/network.md](../../design/network.md), [design/roster.md](../../design/roster.md), [design/address-sync.md](../../design/address-sync.md), [design/connections.md](../../design/connections.md), and [design/discovery.md](../../design/discovery.md).
