# Hosted node

This package runs one always-on node and its owner web console. The node owns
one embedded database under `/var/lib/inseam`. Source files live on separate
host mounts under `/srv/inseam/hosts`; the example mounts them read-only.

## Run

Create a strong token, make the example source directory, and start the node:

```sh
cd deploy/hosted
mkdir -p documents
export INSEAM_OWNER_TOKEN="$(openssl rand -hex 32)"
export INSEAM_COOKIE_SECURITY=local-http
docker compose up --build
```

Open `http://127.0.0.1:7337` for local testing. Remove the
`INSEAM_COOKIE_SECURITY` override in a remote deployment so the container uses
secure cookies, and put HTTPS in front of it. The Compose port binds to host
loopback on purpose. Point a TLS reverse proxy at that address instead of
publishing the container port directly.

`INSEAM_INDEX_ROOTS` maps browser-visible IDs to approved server directories.
The API never accepts a raw path. The comma-separated form supports up to 64
entries:

```sh
INSEAM_INDEX_ROOTS=documents=/srv/inseam/hosts/documents,notes=/srv/inseam/hosts/notes
```

To connect a Google account from the console, set `GOOGLE_CLIENT_ID` and
`GOOGLE_CLIENT_SECRET` in the environment and `INSEAM_PUBLIC_URL` to the
origin owners use (`https://node.example`); register
`https://node.example/api/v1/oauth/callback` as a redirect URI on the OAuth
client. The console's Connections panel then signs in and the Google hosts
appear without a restart (`docs/indexing/google-workspace.md`).

Run one container per personal trust domain. Do not share `/var/lib/inseam`
between replicas or customers.
