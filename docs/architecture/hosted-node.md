# Hosted Node

`inseam serve` keeps an ordinary node running behind an authenticated HTTP
owner API. It can also serve the compiled Vite console from `--web-dir`, so
local and hosted nodes present the same client and operation messages.

## HTTP owner transport

`crates/inseam-http` translates JSON to calls on the `operations` seam. It
never calls the store, Finder, sweep, or connections directly. Routes live
under `/api/v1/owner`; `/api/v1/health` is the one unauthenticated route.

The server requires `INSEAM_OWNER_TOKEN` or `--owner-token` with at least 32
bytes. Login exchanges it for a signed, 12-hour `HttpOnly` session cookie.
Cookies are `Secure` by default and `SameSite=Strict`. `--cookie local-http`
or `INSEAM_COOKIE_SECURITY=local-http` exists only for an HTTP development
server.

## OAuth from the console

The owner's browser is not where the node runs, so a grant authorized from
the console cannot come back to a loopback port. The transport serves the
redirect itself: `POST /api/v1/owner/grants/authorize` starts an attempt whose
redirect URI is `<public url>/api/v1/oauth/callback`, the console sends the
tab to the provider, and the provider returns it to that route — the second
unauthenticated route, by necessity (a `SameSite=Strict` cookie never rides a
cross-site redirect) and safely (the operation accepts nothing but an
unguessable `state` it issued). The node exchanges the code, stores the
tokens, and sends the tab back to `/?authorized=<grant>` (or
`/?authorization_error=…`); the console shows the outcome and the new hosts.
`--public-url` / `INSEAM_PUBLIC_URL` names the public origin; `/api/v1/owner/info`
reports the resulting callback URL, which must be registered on the OAuth
client. `GET /owner/grants` and `POST /owner/grants/revoke` complete the set
([../plugins/oauth.md](../plugins/oauth.md)).

`GET /api/v1/owner/raw?address=<address>` is the one route whose answer is
not JSON: the `fetch_bytes` operation unwrapped to a body under its own
`Content-Type`, so the console's `<img>` or any HTTP client with the owner
cookie reads an indexed image as a plain URL. `POST /api/v1/owner/fetch_bytes`
is the same operation in JSON, bytes as base64. What may be served and how
much is the operation's decision ([../finder/operations.md](../finder/operations.md));
the route decides only how a browser may treat the body. The index holds
whatever the hosts do — an HTML page, an SVG with a script — and rendering
that on the owner origin would run it with the session cookie, so every
`raw` response carries `Content-Security-Policy: sandbox` and `nosniff`,
and only raster images (`image/png`, `image/jpeg`, `image/gif`,
`image/webp`) are `Content-Disposition: inline`; everything else is an
`attachment`.

Requests are limited to 64 KiB, 64 concurrent calls, and 60 seconds — except
the one route that carries files, `POST /api/v1/owner/plugins/install`, which
takes up to 44 MiB (a 32 MiB plugin directory, base64 in JSON). The server
binds to `127.0.0.1:7337` unless the operator chooses another address.
API responses disable caching, and the server applies a restrictive content
security policy and browser capability policy to the console.

## Installing plugins into the running node

`GET /api/v1/owner/plugins` lists every composition entry as the kernel runs
it (id, plugin ref, `active` | `pending` | `failed` with the reason, live
effects, missing services and secrets). `POST /api/v1/owner/plugins/install`
takes `{ "id", "files": [{ "path", "bytes" }], "config"? }` — the plugin
directory as the registry lays it out (`<name>.wasm`, its manifest and
checks, any fixtures), each file's bytes in standard base64 — and mounts it
**without restarting the node**: the files land under
`<data-dir>/plugins/<id>/`, the entry is appended to the node's
`composition.toml`, and the kernel reconciles; the response is the new
entry's state. An entry that fails to activate (admission, a bad manifest)
is rolled back — file, tree, and uploaded files — and the failure is the
error. The same mount-time gates as any `wasm:` entry apply
([../plugins/loaded.md](../plugins/loaded.md)); `serve` is the transport
that applies such edits, because it owns the running kernel.

## Filesystem scopes

The web API never accepts an arbitrary filesystem path. Each `--index-root`
or `INSEAM_INDEX_ROOTS` entry maps a short ID to one absolute directory:

```sh
inseam serve --index-root documents=/srv/inseam/hosts/documents
```

The client sends `documents`; the transport resolves the configured path
before it calls `operations.index`. At most 64 unique root IDs may be active.
The same request may carry `deep_budget` (`"catalog_only"`, `{"sources": N}`,
or `"unlimited"`) to override the composition's `sweep.max_sources` for that
run; `POST /api/v1/owner/catalog` lists the catalog (`host`, `filter` =
`all` | `indexed` | `pending`, `limit`).

## Web console

`apps/web` is a React, TypeScript, Vite, and shadcn client. It shows node
statistics and mounted hosts, searches the index, expands and fetches a
source, connects and disconnects accounts (the Connections panel lists every
grant and the hosts it stewards), installs a loaded plugin from a chosen
plugin directory and lists what the node runs (the Plugins panel), and
triggers a sweep over an approved root. Vite proxies `/api` to a
local node during development. A production build can be served by the node
from the same origin.

## Container package

`deploy/hosted` builds the Rust binary and web client into one container. The
node data directory is `/var/lib/inseam`; source mounts are separate under
`/srv/inseam/hosts`. The sample Compose file exposes the container only on
host loopback and expects a TLS reverse proxy for remote access.

One running process owns one data directory. A service with several owners
runs one process and persistent volume per personal trust domain.
