# The Filesystem Host

The one connection shipped in core: a node stewarding its own machine's filesystem. Service hosts (Gmail, Slack, …) arrive later as WASM connection plugins ([design/plugins.md](../../design/plugins.md)).

## Identity and addresses

The host id is `fs-<hostname>` (sanitized). Locators are absolute paths with the leading `/` stripped, so addresses round-trip textually:

```
/Users/greg/Data/Notes/reno.md  ->  inseam://fs-gregs-mba/Users/greg/Data/Notes/reno.md
```

Resolution refuses foreign hosts and any locator containing a parent-directory component.

## Enumeration

`inseam index <dir>` walks the directory **read-only**:

- hidden files and directories (dot-named) are skipped — except the root the caller explicitly named;
- symlinks are not followed; empty files are skipped;
- every remaining regular file becomes a source with an envelope: mimetype by extension (with a text-extension fallback for code files), byte length, created/modified/observed timestamps, filename as the discovery hint.

Text sources the indexer actually reads get their envelope length upgraded to lines — the unit `scan` and extents speak. Binary and oversized sources stay byte-lengthed and envelope-only.

## Serving

`fetch` returns full text content; `scan` returns 1-based inclusive line ranges (end clamped, start past EOF is an error). Binary fetches over the JSON surface are refused for now.
