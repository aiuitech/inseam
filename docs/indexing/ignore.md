# Ignoring Sources

Ignoring keeps sources out of a node entirely: an ignored source is never cataloged, never indexed, and never synced. It is the answer for build output, dependencies, credentials, spam, and mailing lists — things that are not yours to index ([design/ignore.md](../../design/ignore.md)). It works in two layers.

## Host-native: the connection's own vocabulary

Each connection plugin exposes its host's natural way of excluding things and applies it while listing, so the walk never touches what is ignored. For the filesystem connection (`fs` entry, [configuration.md](../configuration.md)):

```toml
[[entry]]
id = "fs"
[entry.config]
skip_hidden = true          # dot-named files and directories (default true)
gitignore = true            # honor .gitignore files and .git/info/exclude (default true)
ignore = [
  "node_modules/",          # gitignore syntax: no slash = this name at any depth
  "*.log",
  "!important.log",         # `!` re-includes
  "/Users/greg/Library/",   # leading slash = an absolute path
]
```

- `.gitignore` files are honored whether or not the directory is a git repository; turn `gitignore` off to index what git would not track.
- A `.inseamignore` file in any directory is always honored, with the same syntax, scoped to that directory — use it to keep a subtree out of inseam without touching git's view of it.
- The directory you name in `inseam index <dir>` is never skipped by its own rules, even if it is hidden or matched by a pattern.

Service connections (Gmail, Slack, …) expose their own equivalents — labels, queries, channels — in their own entry config.

## Host-agnostic: rules over addresses and envelopes

Locators are paths on a filesystem but opaque ids in a mailbox, so the core's rules match what every source has: its address and its envelope. Configure them on the `sweep` entry; each rule is a set of globs that must **all** match, and a source is ignored when **any** rule matches:

```toml
[[entry]]
id = "sweep"
[[entry.config.ignore]]
locator = "**/node_modules/**"            # path-wise glob: `*` stops at `/`, `**` spans segments
[[entry.config.ignore]]
address = "inseam://fs-*/tmp/**"          # the full address, same path-wise rules
[[entry.config.ignore]]
host = "gmail-*"
property = "email:*@newsletters.example"  # any correspondent at that domain
[[entry.config.ignore]]
source_type = "email"
hint = "[SPAM]*"                          # an email's hint is its subject
[[entry.config.ignore]]
content_type = "image/*"
```

| field | matched against | glob style |
| --- | --- | --- |
| `address` | `inseam://<host>/<locator>` | path-wise |
| `host` | the host id | plain (`*` matches anything) |
| `locator` | the locator | path-wise |
| `source_type` | the envelope's source type (`file`, `email`) | plain |
| `content_type` | the mimetype essence (`text/markdown`, `image/jpeg`) | plain |
| `hint` | the discovery hint (filename, subject); sources with no hint never match | plain |
| `property` | each trust property as `key:value`; matches if any does | plain |

A rule with no field, or a malformed glob, is a configuration error: the `sweep` entry fails with the rule and field named (visible in `inseam plugins`), and nothing is indexed until it is fixed.

## What ignoring does to an existing index

Ignoring is membership, not scope — unlike the cutoff and size dials, it removes data. A source that was indexed before a rule covered it is deleted on the next sweep, exactly like a file that was removed from disk (`inseam index` reports it under `removed`, and the run's `ignored` count says how many sources the rules kept out). Lifting the rule brings it back as a new source; re-indexing it is cheap because embeddings and summaries for unchanged content are cached.
