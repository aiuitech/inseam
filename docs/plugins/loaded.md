# Loaded Plugins

The untrusted tier ([design/plugins.md](../../design/plugins.md)), named for how the code arrives: **loaded** from an artifact at runtime, where [linked plugins](linked.md) are compiled in at build. They are WASM components mounted by the bridge in `crates/inseam-wasm-host`, sandboxed by construction. The tier is about where the code came from, not what it does — a WASM transform registers into the same `transforms` seam as a linked one, a WASM connection into the same `connections` registry as the filesystem host, and neither the sweep nor the finder can tell them apart.

## Two seams cross the boundary

| Seam | WIT world | Instance policy | Reference plugin |
| --- | --- | --- | --- |
| `transform` | [`transform-plugin`](transform-plugin-wit.md) | fresh instance per call | `plugins/ocr` — image OCR through the granted vision LLM |
| `connection` | [`connection-plugin`](connection-plugin-wit.md) | one long-running instance per entry | `plugins/github` — one repository as a host ([../indexing/github-host.md](../indexing/github-host.md)) |

Both worlds import the same two capability interfaces, `host` and `fetch`; `inseam seams` prints the summary and `inseam seams --wit` the whole package.

## Mounting

A composition entry whose plugin ref uses the `wasm:` scheme:

```toml
[[entry]]
id = "ocr"
plugin = "wasm:plugins/ocr/ocr.wasm"
[entry.config]
cooldown_days = 7    # wait period for new versions (see below); 0 by default for local dev
# allow_new = true   # explicit consent for a version still inside its wait period
# fuel = 2000000000  # per-call execution budget

[[entry]]
id = "github-hello"
plugin = "wasm:plugins/github/github.wasm"
[entry.config]
roots = [""]                  # a connection: the scopes the owner indexes; "" is the whole host
# grant = "github"            # the oauth grant whose bearer `authorize` attaches
# fetch_bytes_max = 16777216  # dials on the network grant: body cap, timeout, redirects, calls per call
# fetch_timeout_ms = 10000
# fetch_redirects_max = 3
# fetch_calls_max = 64
[entry.config.plugin]         # the plugin's own config, handed to `configure` as TOML
repository = "octo/hello"
ref = "main"
```

Next to the artifact sits its manifest (`<name>.manifest.toml`): identity, version, seam, what it declares for that seam, and the capabilities it requests. The manifest is what an owner reviews; the bridge guarantees the component gets nothing beyond it.

```toml
name = "github"
version = "0.1.0"
seam = "connection"             # or "transform"
host_kind = "github"            # connection: identity — describe-host() must export exactly this
[connection]                    # connection: effective = declared AND exported
enumerates = true
change_feed = false
writable = false
# claims = ["image/png"]        # transform: effective claims = declared ∩ exported
# roots_only = false
# kind = "enrichment"
[capabilities]
hosts = ["api.github.com", "raw.githubusercontent.com"]   # every host `fetch` may contact
grant = true                    # `authorize` may attach the entry's oauth grant bearer
# llm = true                    # transform: llm-complete / llm-describe-image
# source_bytes = true           # transform: the claimed fragment's bytes
# llm_call_budget = 25
```

Three ways the entry gets there: by hand, as above; `inseam plugin mount` / `plugin install` ([authoring-cli.md](authoring-cli.md), [registry.md](registry.md)), which append it before the node boots; and, on a **running** node, the `install_plugin` owner operation — the web console's Plugins panel, `POST /api/v1/owner/plugins/install` ([../architecture/hosted-node.md](../architecture/hosted-node.md)) — which uploads the plugin directory, writes it under `<data-dir>/plugins/<id>/`, appends the entry, and reconciles the kernel in place, no restart; the macOS app does the same through `inseam_node_install_plugin` ([../architecture/macos-app.md](../architecture/macos-app.md)). A mount that fails is rolled back and named; the gates below apply to all three identically.

## The capabilities

Imports are capabilities: a component only ever receives what the bridge hands it, attenuated per its manifest. Besides core WASI — given an **empty** environment: no files, env vars, args, or sockets — the imports are:

- `log` — into the node's tracing output; always granted.
- `llm-complete`, `llm-describe-image` — the same metered, guarded LLM handle linked transforms get; if the manifest didn't request `llm`, every call errors.
- `source-bytes` — the claimed fragment's raw bytes (transforms; `source_bytes`): the source's at the root, or the content a non-root fragment references ([../indexing/transforms.md](../indexing/transforms.md#content-references)).
- `fetch` — **the network as a described request**. The component names a method (GET, HEAD, POST, PUT, PATCH, DELETE), a URL, headers, and a body; the node performs it through the same SSRF guard the [web host](../indexing/web-host.md) uses, and only to hosts the manifest's `hosts` list names (exact hosts or `*.domain` patterns; empty means no network at all). Every redirect hop is re-checked against the list, the body is capped, the request is timed out, and transport-owned headers (`host`, `content-length`, …) are refused. A non-success status comes back as an answer, not an error — a plugin speaking a host API reads its own 404s. Calls are budgeted per transform application or per connection call (`fetch_calls_max`).
- `fetch` with `authorize = true` — the node attaches the bearer token of the OAuth grant the entry names (`grant = "…"`, [oauth.md](oauth.md)) before sending. The token is read by the bridge and never crosses into the component; it rides only on hops to the host the component named, never on a redirect elsewhere. Requires the manifest's `grant` capability; a request that asks without one is refused before anything leaves the node.

## What the bridge enforces

- **Effective claims = manifest ∩ exported** — a transform can't quietly claim more than its manifest promised; zero overlap refuses the mount.
- **Host identity is the bridge's** — a connection names its host by kind and principal; the bridge derives the id (`derive_host_id`), refuses a kind other than the manifest's `host_kind`, and computes the effective edge capabilities as declared AND exported. A component cannot forge another host's identity or declare itself writable past its manifest.
- **Output hygiene** — emitted fragments are checked (inseam-defined mimetypes are dropped, unknown relations become `contains`, malformed parent references are treated as roots); enumerated sources with malformed locators are dropped, unparseable content types become `application/octet-stream`, and every envelope's `observed` stamp, trust properties, and digest are the node's to set, never the plugin's.
- **A fresh instance per transform call** — component state is thrown away between runs; nothing leaks between sources. A connection keeps its one instance (pagination cursors, small caches survive), but every call gets its own fuel and fetch budget, and an instance that traps is discarded and rebuilt on the next call — a fault costs one call, never the host.
- **Fuel limits** — a component stuck in a loop is stopped instead of hanging the sweep; a trap or error is logged and produces nothing (indexing is enrichment — a broken transform never blocks a source; a broken connection call is the offline answer the sweep reports).
- **Release cooldown** — a newly seen artifact hash waits `cooldown_days` before activating, timed from when this node first saw it (recorded in the node's own plugin state, so it can't be forged). `allow_new = true` on the entry is the explicit consent.
- **Capability widening is its own gate** — requesting different capabilities than the approved version (a new host in `hosts` included) refuses the mount until the owner sets `allow_new`, regardless of the wait; the refusal names the diff.
- **Install-time admission** — the first time a node sees an artifact it runs the full validation harness ([validation.md](validation.md)); a plugin that crashes on hostile input, ships no golden checks, or fails its own refuses to mount, naming the failing check. `admission = "enforce" | "warn" | "off"` per entry; verdicts are cached by content hash.
- **Shape stamps carry the artifact** — the name, version, and content hash go into a transform registration's shape fingerprint, so upgrading a plugin re-indexes exactly the sources it built ([indexing/maintenance.md](../indexing/maintenance.md)).

## Authoring and validating

The agent skill at `skills/inseam-loaded-plugin/SKILL.md` is the authoring guide for both seams (scaffold, golden checks first, manifest, build to `wasm32-wasip2`, validate). The validation gate:

```sh
inseam plugin check plugins/<name>/<name>.wasm
```

the same harness CI runs at publish and every node runs at admission ([validation.md](validation.md)). `plugins/ocr` is the reference transform and `plugins/github` the reference connection; `crates/inseam-wasm-host/tests/ocr_e2e.rs` and `github_e2e.rs` exercise them end to end (with a fake LLM, a fake GitHub on a loopback port, and a fake OAuth grant — proving a component only ever sees the granted capability, and that the bearer is the bridge's to attach). Distribution and installation: [registry.md](registry.md).
