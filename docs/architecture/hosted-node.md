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

Requests are limited to 64 KiB, 64 concurrent calls, and 60 seconds. The
server binds to `127.0.0.1:7337` unless the operator chooses another address.
API responses disable caching, and the server applies a restrictive content
security policy and browser capability policy to the console.

## Filesystem scopes

The web API never accepts an arbitrary filesystem path. Each `--index-root`
or `INSEAM_INDEX_ROOTS` entry maps a short ID to one absolute directory:

```sh
inseam serve --index-root documents=/srv/inseam/hosts/documents
```

The client sends `documents`; the transport resolves the configured path
before it calls `operations.index`. At most 64 unique root IDs may be active.

## Web console

`apps/web` is a React, TypeScript, Vite, and shadcn client. It shows node
statistics and mounted hosts, searches the index, expands and fetches a
source, connects and disconnects accounts (the Connections panel lists every
grant and the hosts it stewards), and triggers a sweep over an approved root. Vite proxies `/api` to a
local node during development. A production build can be served by the node
from the same origin.

## Container package

`deploy/hosted` builds the Rust binary and web client into one container. The
node data directory is `/var/lib/inseam`; source mounts are separate under
`/srv/inseam/hosts`. The sample Compose file exposes the container only on
host loopback and expects a TLS reverse proxy for remote access.

One running process owns one data directory. A service with several owners
runs one process and persistent volume per personal trust domain.
