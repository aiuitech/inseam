# The Web Host

The `connection-web` plugin registers the public web as one **fetch-only host** of kind `web` ([connections.md](connections.md)). It enumerates nothing and is never swept; it exists so a link found in an indexed document can be followed: the `links` transform turns a link to an image into a fragment that references the image's web address ([transforms.md](transforms.md#the-first-party-transforms)), the planner reads those bytes for byte-wanting transforms such as OCR, and clients fetch the same address like any other.

Mounting the entry is your consent for the node to contact linked sites. It is not in the base composition; add it, and the links transform, to `composition.toml`:

```toml
[[entry]]
id = "web"
plugin = "connection-web"
[entry.config]
allow_hosts = ["*.wikimedia.org", "imgs.example.com"]   # empty: any host with a public address
# content_bytes_max = 16777216   # 16 MiB; larger resources are refused
# timeout_ms = 10000
# redirects_max = 3              # each hop re-guarded; at most 10
# user_agent = "inseam/<version>"

[[entry]]
id = "links"
plugin = "transform-links"
[entry.config]
# follow = ["image/*"]           # what a link may resolve to; anything else stays a link
# probe = true                   # confirm types through the web host when it is mounted
```

## Addresses

Every node derives the same host id for the one public web, so a reference minted on one node names the same thing on another. The locator is the URL itself:

```
https://example.com/flyers/garage-sale.png  ->  inseam://web-76de5c824c9abf81/https://example.com/flyers/garage-sale.png
```

`inseam hosts` lists it with `enumerates: false`. A bare `inseam index <dir>` still means the filesystem — a fetch-only host is never a candidate for a sweep's scope, so mounting it makes nothing ambiguous — and naming it (`inseam index --host web-… …`) is refused by name.

## The guard

Every request, and every redirect hop, passes the same guard before a socket opens ([design/connections.md](../../design/connections.md)):

- `http` and `https` only;
- the host is in `allow_hosts` when the list is set (`example.com` exactly, or `*.example.com` for the domain and everything under it);
- the host's addresses are resolved first and every one must be **public** — loopback, private, carrier-grade NAT, link-local, multicast, and reserved ranges are refused — unless the owner named the host in `allow_hosts`, which is how a LAN image server (or a test fixture on `127.0.0.1`) is reached;
- the connection is pinned to the addresses it checked, so a DNS answer cannot change between the check and the connect;
- redirects are followed by hand, at most `redirects_max` hops, each re-guarded;
- the body is refused before it is read when the declared length passes `content_bytes_max`, and the moment it passes the cap otherwise;
- the whole request, headers and body, is bounded by `timeout_ms`.

A refused fetch is an error the caller sees (`fetch_bytes`) or a reference left without bytes (the planner), never a silent empty answer.

How far into the web a sweep may go is not the host's dial but the sweep's: `sweep.max_reference_hops` (default 1) bounds how many references a chain follows away from a source, so a page follower mounted beside the links transform can never crawl unbounded ([transforms.md](transforms.md#content-references)). The host bounds each request; the sweep bounds the chain.

## Serving

`fetch_bytes` on a web address returns the resource under the content type its headers declare; `fetch` and `scan` read it as text. A `scan` streams the body only until its last requested line has arrived and then drops the connection, and it is not refused by a declared `Content-Length` past the byte cap — only the bytes actually read count — so the first lines of a resource too large to fetch whole are still scannable. There is no way to skip the bytes before a line (the index records extents in lines, not byte offsets, and lines are found by reading), so an HTTP range request would not help here; a sequential read that stops early is the whole optimization. `describe` answers the `links` transform with the content type and declared length from a `HEAD` request (falling back to a body-less `GET` when a server refuses `HEAD`). Nothing is cached: the index holds references, never copies ([../../design/addressing.md](../../design/addressing.md)).
