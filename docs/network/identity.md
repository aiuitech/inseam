# Node Identity

A node is its Ed25519 keypair. The public key is the node id: the dial target for the transport, the origin of every record the node publishes, and what a peer authenticates on the wire. The `node` plugin (`crates/inseam-plugins/src/node/`) provides the `node` seam (`inseam-seams::node`), which the transport, the roster, and sync all read, so there is one answer to "which node is this" in the process ([design/roster.md](../../design/roster.md)).

## The key file

The secret lives at `<data-dir>/node/secret.key`: 64 lowercase hex characters and a newline, in a directory the plugin sets to mode `0700`, the file to `0600`. It is minted on the first apply — 32 random bytes from the OS generator, written to a temporary file and renamed into place so a crash mid-write never leaves half a key — and read back on every boot after that. The boot log says which happened (`source = Minted` or `Kept`).

The key is never configured, never synced, and never leaves the process except into this file. A file that exists but is not a key (wrong length, uppercase, not hex) is refused by name rather than replaced, because replacing it would mint a new node under an old data directory; the error says to restore it from a backup or move it aside on purpose. A key readable by other accounts on the machine earns a warning at boot (`chmod 600` it), not a refusal. Copying a data directory copies the identity: two processes with one key are one node to the network, so a cloned machine needs its key file removed before its first boot.

The id is derived through iroh's own key type, so the hex the node announces is byte for byte the id the transport authenticates. `inseam network` prints it in full; listings show the first twelve characters as a reading aid, never as an identifier.

## What the node advertises

The rest of the node record is presentation and capabilities, from the `node` entry's config. Changing any of it restarts the entry, and the roster republishes the record.

| Field | Default | Meaning |
| --- | --- | --- |
| `display_name` | the machine's hostname | what owners see in listings; at most 128 characters, whitespace trimmed, empty refused |
| `always_on` | `false` | the node intends to be reachable at all times at a stable endpoint — the backbone convention. Sync rounds and fan-outs put `always_on` nodes first |
| `deep_index` | `true` | the node indexes the content of the hosts it stewards, so a query fanned out to it answers from content, not envelopes alone. Only `deep_index` nodes are fan-out targets ([discovery.md](discovery.md)) |
| `relays` | `true` | the node forwards routed requests for hosts it does not steward toward nodes that do. With `relays = false` it answers "unreachable through me" instead ([routing.md](routing.md)) |

A machine with no hostname is named `inseam node`. The entry injects nothing and provides `node`, declaring `id` and `display_name` as facts.
