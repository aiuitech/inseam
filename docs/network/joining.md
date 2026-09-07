# Joining a Network

A walkthrough: one always-on node, one laptop, one query across both. Every step is `inseam network` in a shell; the HTTP owner routes do the same for a hosted node ([../architecture/hosted-node.md](../architecture/hosted-node.md#network)).

## 1. Run a backbone

Pick the node that will stay on — a Mac mini at home, a server, a hosted node ([../architecture/hosted-node.md](../architecture/hosted-node.md#backbone)). In its `composition.toml`:

```toml
[[entry]]
id = "node"
[entry.config]
display_name = "mini"
always_on = true

[[entry]]
id = "transport"
[entry.config]
bind_port = 7000                   # forwarded on the router, or opened in the firewall
# relay = "https://relay.example"  # the network's own iroh relay, once one runs; "n0" until then
```

Keep it running (`inseam serve`, or the app). The first boot mints the key under `<data-dir>/node/secret.key` and logs `node identity ready`; the roster publishes the node record, and republishes it once the transport reports its relay ([roster.md](roster.md#endpoint-rotation)).

## 2. Invite

On the backbone:

```sh
inseam network invite
```

prints the invitation on a line of its own — `inseam-invite:…`, holding the backbone's id and endpoints, a one-time token, and an expiry 24 hours out — followed by when it expires and the command to run on the other node. Carry the line to the laptop over any channel you would trust with a password. It admits one node, once.

## 3. Join

On the laptop:

```sh
inseam network join 'inseam-invite:…'
```

The laptop parses the invitation (an expired one is refused by name), dials the backbone, presents the token in the handshake, and runs one sync exchange. It answers with the network as the laptop now sees it: two nodes, the hosts each stewards, the size of the log. From here the sync loop keeps both converged every minute while a node process runs, and the laptop's standing connection is how the backbone reaches it behind NAT.

## 4. Verify

```sh
inseam network          # both nodes: live or not, last sync, the hosts each stewards; --json for the objects
inseam status           # remote_sources counts what arrived from the backbone's log
inseam catalog          # a remote row shows its origin
```

`live` beside a node means a session is open or the last exchange succeeded — what this node learned by trying, never a synced fact. A node listed with no `live` mark and a `last_error` is the one to go and look at.

## 5. Query and fetch across nodes

```sh
inseam query "kitchen renovation"
```

runs the laptop's index and the backbone's together; results from the backbone are marked `via <id>`, and the footer shows one line per node asked ([discovery.md](discovery.md)). Then the ladder works as it does locally:

```sh
inseam expand inseam://<host>/<locator>                   # served from the backbone's index
inseam scan inseam://<host>/<locator> --start 1 --end 40  # lines read through the backbone
inseam fetch inseam://<host>/<locator>                    # the source, through the backbone's connection to the host
```

A read of a host nobody can reach right now is `unreachable`, naming the nodes tried ([routing.md](routing.md#errors)).

## Expelling a lost device

A phone lost, a laptop sold: from any node that is still yours,

```sh
inseam network expel <node-id>
```

publishes the expulsion, disconnects the node now, and purges what it had published from every node as the record spreads. Its key is refused for good. A device that comes back joins as a new node — delete its `node/secret.key` so it mints a new key — through a fresh invitation ([roster.md](roster.md#expulsion)).
