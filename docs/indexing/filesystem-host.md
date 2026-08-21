# The Filesystem Host

The first connection plugin: `connection-fs`, registering this machine's filesystem into the `connections` registry ([connections.md](connections.md)) as a host of kind `fs` with read-only capabilities (enumerates; no change feed yet; not writable). Service hosts (Gmail, Slack, …) register the same way on the same seam ([design/connections.md](../../design/connections.md)).

## Identity and addresses

The host id is `fs-<hostname>` (sanitized; `host_id` in the entry config overrides it; `skip_hidden`, `gitignore`, and `ignore` configure enumeration — [ignore.md](ignore.md)). Locators are absolute paths with the leading `/` stripped, so addresses read like the paths they name:

```
/Users/greg/Data/Notes/reno.md  ->  inseam://fs-gregs-mba/Users/greg/Data/Notes/reno.md
```

Resolution refuses addresses for other hosts and any locator containing a parent-directory (`..`) component.

## Enumeration

`inseam index <dir>` walks the directory **read-only** (a scope may also be given as a locator — `inseam index inseam://<host>/Users/greg/Notes` is the same scope as `/Users/greg/Notes`):

- hidden files and directories (dot-named) are skipped by default, `.gitignore` files found in the tree are honored by default, `.inseamignore` files are always honored, and the entry's `ignore` patterns prune the walk — all in gitignore syntax ([ignore.md](ignore.md)). The root the caller explicitly named is never skipped;
- symlinks are not followed; empty files are skipped;
- every remaining regular file becomes a source with an envelope: mimetype by extension (with a text fallback for code files), byte length, created/modified/observed timestamps, and the filename as the discovery hint.

Text sources the indexer actually reads get their envelope length upgraded from bytes to lines — the unit `scan` and extents use. Binary and oversized sources stay byte-lengthed, with only their envelope recorded.

## Serving

`fetch` returns the full text content; `scan` returns 1-based inclusive line ranges (the end is clamped; a start past the end of the file is an error). Binary fetches over the JSON surface are refused for now.
