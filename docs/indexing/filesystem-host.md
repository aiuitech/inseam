# The Filesystem Host

The first `connection` provider: the `connection-fs` plugin, stewarding this machine's filesystem. Service hosts (Gmail, Slack, …) arrive later as loaded connection plugins on the same interface ([design/connections.md](../../design/connections.md)).

## Identity and addresses

The host id is `fs-<hostname>` (sanitized; `host_id` in the entry config overrides it). Locators are absolute paths with the leading `/` stripped, so addresses read like the paths they name:

```
/Users/greg/Data/Notes/reno.md  ->  inseam://fs-gregs-mba/Users/greg/Data/Notes/reno.md
```

Resolution refuses addresses for other hosts and any locator containing a parent-directory (`..`) component.

## Enumeration

`inseam index <dir>` walks the directory **read-only**:

- hidden files and directories (dot-named) are skipped — except the root the caller explicitly named;
- symlinks are not followed; empty files are skipped;
- every remaining regular file becomes a source with an envelope: mimetype by extension (with a text fallback for code files), byte length, created/modified/observed timestamps, and the filename as the discovery hint.

Text sources the indexer actually reads get their envelope length upgraded from bytes to lines — the unit `scan` and extents use. Binary and oversized sources stay byte-lengthed, with only their envelope recorded.

## Serving

`fetch` returns the full text content; `scan` returns 1-based inclusive line ranges (the end is clamped; a start past the end of the file is an error). Binary fetches over the JSON surface are refused for now.
