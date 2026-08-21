# Ignore

How an owner keeps sources out of a node: **ignore** is a statement of membership — "this is not mine to index" — distinct from the [index-maintenance](index-maintenance.md) *scope* dials, which only say how much of what is mine to build.

## Two layers, because hosts differ

Ignoring happens in two places, and the split follows what each party can know.

**Host-native ignore lives in the connection.** Only the filesystem connection can read a `.gitignore`; only a Gmail connection knows what a label or a search query means. So every [connection](connections.md) plugin exposes its host's natural exclusion vocabulary in its own config, and applies it while enumerating — an ignored source never becomes an address. This is also the fast path: a walk that never descends into `node_modules` or `Library` does no work there, where a filter applied afterwards would have stat-ed every file first. For the filesystem connection the vocabulary is gitignore syntax — configured patterns anchored at the filesystem root, `.gitignore` files found in the tree (honored by default, git repository or not), and a per-directory `.inseamignore` for excluding a subtree from inseam without changing git's view of it. A mail connection's equivalent is labels and queries; a chat connection's is channels. Each is a host fact expressed where that host is understood.

**Host-agnostic ignore lives in the sweep, over addresses and envelopes.** Locators on a filesystem are paths, but a Gmail locator is an opaque message id — a path glob says nothing about it. What *every* source has is an [address](addressing.md) (host id + locator) and an envelope: source type, content type, hint, trust properties. So the core rule vocabulary is exactly those fields, each a glob, AND-ed within a rule and OR-ed across rules:

```toml
[[entry]]
id = "sweep"
[[entry.config.ignore]]
locator = "**/node_modules/**"           # path-wise: `*` stops at `/`, `**` spans
[[entry.config.ignore]]
host = "gmail-*"
property = "email:*@newsletters.example"  # every correspondent with that domain
[[entry.config.ignore]]
source_type = "email"
hint = "[SPAM]*"                           # the subject line is the hint
[[entry.config.ignore]]
content_type = "image/*"
```

"Ignore some emails from Gmail" is therefore answered by the envelope, not by a path: an email source already carries `email:<correspondent>` trust properties ([access-control](access-control.md)) and its subject as the discovery hint, so sender, domain, and subject are all matchable without teaching the core anything about mail. The same rules apply unchanged to sources that arrive by address sync from another steward, which a connection-level rule never sees.

Rules are parsed once, at the composition boundary, and a malformed glob or an empty rule (which would ignore everything) parks the sweep entry with the rule and field named — never a silent non-match. The rule set is a kernel type (`inseam_kernel::ignore`), so a later query-time ignore can reuse the vocabulary rather than invent a second one.

## Ignored means absent — and that evicts

An ignored source is **not cataloged**: no address, no envelope, nothing to sync. Ignore exists for exactly the things an owner does not want anywhere on the network — build output, credentials, spam, a mailing list — and a catalog-only row would still replicate the address and envelope to every node. Absence is the only safe reading.

It follows that ignoring is the one configuration change that *removes* index data: a source that was indexed before a rule covered it is deleted on the next sweep, through the same vanished-source path a deleted file takes, because to the sweep it has vanished from enumeration. This is deliberately the opposite of the [index-maintenance](index-maintenance.md) rule that scope shrinkage never evicts. The two are not in tension: tightening a cutoff says *build less of my data* and must be free to reverse, so paid-for fragments stay; ignoring says *this is not my data* and the owner's intent is exactly that it be gone. Lifting a rule readmits the source as new, and the digest-keyed artifact caches make that cheap — so ignoring, too, is free to reverse.

## Paths not taken

- **One layer only, in the core.** Rejected: the core cannot read `.gitignore`, cannot prune a walk it does not perform, and cannot know a mail label. Host-native exclusion is a connection fact.
- **One layer only, in the connections.** Rejected: it would give every host a different ignore vocabulary, and could never cover sources learned by address sync.
- **Ignored sources as catalog-only rows.** Rejected: it leaks the address and envelope network-wide, defeating the purpose; and it would make "vanished" and "ignored" two states the sweep has to tell apart.
- **A separate ignore seam or plugin.** Rejected for now: one consumer (the sweep) and one rule type; the type lives in the kernel so a second consumer can share it without a seam.
- **Ordered rules with negation in the core set.** Rejected for now: a set of OR-ed rules has no order to reason about, and the host-native layer already has gitignore's `!` where people expect it.

## Open questions

- Handing the host-agnostic rule set down to connections as a pruning hint, so a locator glob can cut a walk short too.
- A query-time ignore (hide without un-indexing) reusing the same rule type.
- Whether ignore rules should travel with a host's roster record, so every steward of a host applies the owner's exclusions.
