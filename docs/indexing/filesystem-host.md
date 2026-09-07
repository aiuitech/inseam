# The Filesystem Host

The first connection plugin: `connection-fs`, registering this machine's filesystem into the `connections` registry ([connections.md](connections.md)) as a host of kind `fs` with read-only capabilities (enumerates; no change feed yet; not writable). Service hosts (Gmail, Slack, …) register the same way on the same seam ([design/connections.md](../../design/connections.md)).

## Identity and addresses

The host id is derived from the **machine's identity, never its hostname** ([design/addressing.md](../../design/addressing.md)): `fs-` followed by sixteen hex characters of a BLAKE3 fingerprint over the kind and the machine id, the same derivation every connection uses. The machine id is the platform's own — the IOPlatformUUID on macOS, `/etc/machine-id` on Linux, the registry's `MachineGuid` on Windows — so renaming the machine changes nothing and two nodes on one machine mint one host. Where the platform has none (a container image without systemd, a phone), the entry mints a random identity once and keeps it at `<data-dir>/<entry>/machine-id`, so the id follows the data volume. `machine_id` in the entry config replaces the identity material (a tenant name, a test fixture); it is never the id itself. `skip_hidden`, `gitignore`, and `ignore` configure enumeration ([ignore.md](ignore.md)).

Locators are absolute paths with the leading `/` stripped, so addresses read like the paths they name:

```
/Users/greg/Data/Notes/reno.md  ->  inseam://fs-3f9a1b2c4d5e6f70/Users/greg/Data/Notes/reno.md
```

An index built while the id was still the hostname names a host no steward serves; the store's schema version was bumped with the change, so such an index is dropped on open and rebuilt by the next sweep ([storage.md](storage.md)).

Resolution refuses addresses for other hosts and any locator containing a parent-directory (`..`) component.

## Configured folders

The entry's `roots` lists the folders this host indexes, as absolute paths (at most 64, none twice, no `..`). Empty — the default — means the owner names a scope per run and any directory is one, which is what `inseam index <dir>` has always done. Once folders are configured, every scope on this host must lie inside one of them: a scope outside is refused by name, listing the configured folders, from the CLI as much as from a remote transport. A configured folder need not exist at boot (a volume mounted later is still the owner's configuration); it is checked when it is indexed.

The connection reports its folders with its registration, so owner surfaces offer them as the roots to index: `inseam hosts` and the `hosts` operation carry them, the web console lists them in its index control, and the console's configuration panel (or the macOS app's Configuration tab) is where they are chosen ([../architecture/hosted-node.md](../architecture/hosted-node.md#filesystem-scopes)).

## Enumeration

`inseam index <dir>` walks the directory **read-only** (a scope may also be given as a locator — `inseam index inseam://<host>/Users/greg/Notes` is the same scope as `/Users/greg/Notes`):

- hidden files and directories (dot-named) are skipped by default, `.gitignore` files found in the tree are honored by default, `.inseamignore` files are always honored, and the entry's `ignore` patterns prune the walk — all in gitignore syntax ([ignore.md](ignore.md)). The root the caller explicitly named is never skipped;
- symlinks are not followed; empty files are skipped;
- every remaining regular file becomes a source with an envelope: mimetype by extension (with a text fallback for code files), byte length, created/modified/observed timestamps, and the filename as the discovery hint;
- every directory between an admitted file and the scope root — the root included — becomes a **folder source** (`inode/directory`, `source_type = "directory"`, zero bytes, the directory's name as the hint), listed after the files. A folder exists exactly when something under it does: an empty or wholly ignored directory is no source, and the filesystem root `/` never is. The sweep composes a folder's content from its children ([transforms.md](transforms.md#folders)).

Text sources the indexer actually reads get their envelope length upgraded from bytes to lines — the unit `scan` and extents use, and the same read fills the envelope's content digest (BLAKE3 over the raw bytes — the finder's cross-host dedup key). Binary and oversized sources stay byte-lengthed, with only their envelope recorded; a source whose content is never read carries no digest.

## Serving

`fetch` returns the full text content — for a folder, its entries' names one per line in name order, directories marked with a trailing `/`, hidden names following `skip_hidden`; `scan` returns 1-based inclusive line ranges, read through a buffered reader that stops at the range's last line rather than loading the file (the end is clamped; a start past the end of the file is a typed error naming the line count); `fetch_bytes` returns any file's raw bytes with its content type — the rung for images and other binaries, which `fetch` refuses by name ([../finder/operations.md](../finder/operations.md)). Every read, whole or ranged, holds one of the host's bounded read permits.

Content reads run on the runtime's blocking pool and pass through a gate of 64 reads in flight per host, shared by every handle to it. The sweep can resume thousands of parked planners at once when a batch-lane job lands; without the gate each would open its file together and exhaust a default 256-descriptor macOS shell. Readers past the gate wait rather than fail.
