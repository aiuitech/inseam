# Connections

A **connection** is how the node reaches one host it stewards ([design/connections.md](../../design/connections.md)). A node holds many — the local filesystem, a mailbox, a chat workspace — so the `connections` seam is a **registry**: the `connections` plugin provides it, and every connection plugin registers the hosts it reaches into it. The [filesystem host](filesystem-host.md) is the first; [Google Workspace](google-workspace.md) registers one host per service the same way; loaded WASM connections will too.

## What a registration is

One call per host, from the connection plugin's `apply`:

```rust
register_as_effect(cx, Registration {
    entry_id: cx.entry_id().to_string(),
    host: HostDescription { id, kind, display_name },   // HostKind: `fs`, `gmail`, …
    capabilities: Capabilities { enumerates: true, change_feed: false, writable: false },
    connection: Arc::new(my_connection),                 // enumerate / locator_prefix / read_*
})?;
```

- **Host description** — the stable host id addresses carry, the kind (the locator-schema family, also the domain separator in id derivation), and a display name. This is the roster's host record, held locally.
- **Capabilities** — what the edge supports, stated in full: whether it can *enumerate* (a fetch-only edge is never swept), whether it offers a *change feed*, whether it is *writable*. These are the facts consumers branch on.
- **The connection** — `enumerate(root)` lists the sources under a connection-interpreted scope (a directory, a label); `locator_prefix(root)` says which locators that scope covers so vanished sources can be reconciled (`Some("")` means the whole host — what a flat id space answers for its everything-scope; `None` skips reconciliation rather than guessing); `read_text` / `read_lines` / `read_bytes` serve content.

Registration is an effect: unmounting the plugin unregisters the host, and nothing downstream has to be told. One node holds **one connection per host** — a second entry registering a host already stewarded fails that entry alone, naming both.

## How consumers find a connection

Consumers never bind a connection; they inject `connections` and resolve by host:

- the **sweep** resolves the host a `SweepRequest` names at the start of every run, so mounting a mailbox never restarts the sweep;
- **operations** resolve `scan`/`fetch` by the host in the address, and `index` by the host the request names — or the only host mounted. With two or more hosts, a scope must name one: `inseam index --host <id> <root>`, or an address, `inseam index inseam://<host>/<root>`. `inseam hosts` lists what is mounted.

## Writing one

Copy `crates/inseam-plugins/src/connection_fs/` for a linked connection to a local host: its config is the host's own exclusion vocabulary and identity override, its `apply` builds the connection and registers it. Copy `connection_google/` for a remote service: it registers its own OAuth grant, derives each host id from the signed-in account with `derive_host_id(kind, principal)`, registers hosts when the grant is authorized (at apply, or on the `GrantChanged` event), and shares one authenticated API client across its services ([../plugins/oauth.md](../plugins/oauth.md)). Loaded (WASM) connections wait on the WIT projection of this seam; the transform seam crossed first.
