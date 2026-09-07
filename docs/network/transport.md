# Transport

The `transport-iroh` plugin (`crates/inseam-plugins/src/transport_iroh/`) provides the `transport` seam (`inseam-seams::transport`): one request/response exchange with a peer named by node id, over QUIC dialed by that public key. iroh supplies the dialing, NAT hole punching, and the relay fallback; the plugin supplies the shape inseam needs on top — one ALPN, a protocol name per stream, an admission handshake, and a session table ([design/connections.md](../../design/connections.md)).

## Dialing by key

A peer is dialed from its roster node record: the id and the `endpoints` the record carries. iroh authenticates the dialed key in the TLS handshake, so a connection to anyone but the named node cannot exist. A peer with no readable endpoint is not dialed — there is no address-lookup service to fall back on, by design — and the error says so; such a node is reached only through a session it opened itself (below). A node never dials itself.

## Endpoints

A node record's `endpoints` are dialing hints in the transport's vocabulary:

- `relay:<url>` — the relay the node is reachable through (`relay:https://relay.example/`), listed first because it is the reliable path;
- `ip:<socket address>` — a direct address (`ip:203.0.113.5:7000`, `ip:[2001:db8::1]:7000`).

The transport renders them from iroh's view of itself. Loopback and link-local addresses are kept only when they are all there is — two nodes on one machine, a LAN with no route out — and never beside routable ones, where they would be noise replicated to every node. At most 16 endpoints of at most 256 characters each. Parsing a peer's endpoints skips any kind it cannot read with a warning, so a newer transport publishing a hint an older one does not know still dials on the rest.

## Relays

| `relay` | Meaning |
| --- | --- |
| `"n0"` (default) | iroh's public relays: NAT traversal works before anyone runs infrastructure |
| `"none"` | direct connections only; two nodes behind different NATs may never reach each other |
| `https://…` | the network's own relay, one URL |

A relay forwards encrypted QUIC packets when no direct path exists; nothing is readable or stored on it. The shape to run is a backbone node with an iroh relay server beside it and every node in the network naming that URL, so no traffic depends on public relay infrastructure ([joining.md](joining.md), [../architecture/hosted-node.md](../architecture/hosted-node.md#backbone)). Relay readiness is learned in the background: `apply` returns once the sockets are bound, and the roster republishes the node record when the relay endpoint appears ([roster.md](roster.md#endpoint-rotation)).

## Configuration

The `transport` entry (`[transport]`, plugin `transport-iroh`, `deny_unknown_fields`):

| Field | Default | Meaning |
| --- | --- | --- |
| `relay` | `"n0"` | as above; anything but `n0`, `none`, or an `https://` URL is refused |
| `bind_port` | `0` | UDP port to listen on, IPv4 and IPv6 (IPv6 welcome, not required); `0` takes an ephemeral one. A backbone with a forwarded port names it here |
| `request_timeout_secs` | `30` | how long the transport's own exchanges may take: the admission handshake, and serving one inbound stream end to end; at least 1 |
| `idle_timeout_secs` | `120` | QUIC idle timeout; a keep-alive goes out at a third of it, so at least 3 |

The entry injects `node` (required) and provides `transport`, declaring `relay` and `id` as facts. `apply` binds the sockets and returns; nothing waits on the network at boot.

## One ALPN, a protocol per stream

Every connection carries the ALPN `inseam/1`. Each request is one QUIC bi-stream: a JSON header line naming the protocol (`{"protocol":"inseam/sync/1"}`), a big-endian `u32` body length, and the body; the response is a status byte (ok or error), a `u32` length, and the bytes — an error's bytes are its message. Bodies travel raw, never base64: a routed `fetch_bytes` ships up to 32 MiB, and the message bound is that plus 8 MiB of framing, **40 MiB**, checked on both sides before a buffer is allocated. The header is bounded at 1 KiB.

Plugins register one handler per protocol name (`inseam_seams::transport::register_as_effect`); a second registration for a name is refused by name, and unmounting the plugin withdraws it. A name is lowercase ASCII letters, digits, `/`, `.`, `-`, at most 64 characters, and carries its version (`inseam/route/1`): a breaking change is a new name. The protocols today are `inseam/sync/1` ([sync.md](sync.md)) and `inseam/route/1` ([routing.md](routing.md)). `inseam/hello/1` is the transport's own; it can be neither registered nor requested.

## Admission

The first stream a dialer opens is `inseam/hello/1`, carrying `{"invitation": <token or null>}`. The acceptor asks the admission policy the roster installed — expelled peers refused, roster members admitted, strangers admitted only with an open invitation token ([roster.md](roster.md#admission)) — and with no policy installed refuses everyone, so a node whose roster has not come up is closed rather than open. A refused peer learns only that it was refused: the reply says `refused`, the connection closes with application code 1, and it never enters the session table; the dialer sees `NotAdmitted`. The dialer runs no check of its own, because the TLS handshake already proved the peer is the key it dialed. The handshake must finish within `request_timeout_secs`; at most 64 are in progress at once, and past that incoming connections are refused at the door rather than queued.

## Sessions

An admitted connection is a session with that peer, in the direction it was opened, and a node holds one session per peer. QUIC is symmetric, so an accepted inbound connection serves this node's outbound requests too. That is how a backbone reaches a laptop behind NAT: the laptop keeps a standing connection open (the keep-alive at a third of the idle timeout keeps the NAT binding warm), and the backbone opens streams on it. A request to a peer reuses its live session in either direction and dials only when there is none.

When both sides dial each other at once, both keep the connection dialed by the lower node id and close the other, so they converge on one. When the same dialer connects twice, the newer connection wins. A session found closed is evicted and the request fails with `retry to redial`; the caller retries, and the retry dials — there is no retry loop inside the transport. `inseam network` shows a session as `live`; expulsion's local half, `disconnect`, closes one.

## Bounds

| Bound | Value |
| --- | --- |
| message body, either direction | 40 MiB |
| request header | 1 KiB |
| streams served at once, per connection | 64 (a 65th waits in QUIC flow control; no uni-streams at all) |
| admission handshakes in flight | 64 |
| protocol handlers | 64 |
| live sessions, both directions together | 256 — the least recently used is evicted |
| endpoints per record | 16, each 256 characters |
