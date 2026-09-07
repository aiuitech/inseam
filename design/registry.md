# Plugin Registry

The registry problem in one sentence: loaded plugins hold credentials and
read personal data, many are machine-generated, and the distribution channel
must therefore prove **integrity** (you got the bytes the publisher
published), **fitness** (they work), and **review** (someone — or something
— looked) without a human review board in the loop. The WordPress model
(human reviewers backing a central registry) is the explicit anti-goal: it
does not scale to AI-authored software, and its trust is social rather than
mechanical.

## Registry v0: the repository is the registry

The `plugins/` tree of the inseam repository **is** the registry. No proxy,
no server, no separate infrastructure:

- **`plugins/registry.toml`** — the index: name, version, publisher,
  artifact path, sha256. The committed hash is one integrity anchor.
- **`plugins/publishers.toml`** — the publisher roster: ids and minisign
  public keys. The other anchor's trust root.
- **`plugins/advisories.toml`** — the yank/flag feed, appended by PR.
- **`plugins/<name>/`** — source, committed artifact, manifest, golden
  checks, fixtures, and the publisher-signed release record. One directory
  per plugin; the layout is documented in `plugins/README.md`.

`inseam plugin install <name>` fetches the index from GitHub raw (or any
checkout via `--registry <path>` — one code path, so tests never need the
network), refuses anything in the advisory feed, verifies the publisher's
signature over the release record against the roster, requires the record
and the index to agree, downloads every signed file — artifact, manifest,
golden checks, fixtures — checking each against its digest, runs the
conformance harness locally, and appends the composition entry.
Mount-time gates (admission, release cooldown, capability widening) still
apply — install is convenience, not trust.

### Why no proxy

A checksum/validation proxy between the node and GitHub was considered and
rejected: it adds an operator to trust and a single point of failure while
providing nothing the design doesn't already have. Integrity comes from
hashes committed in a reviewed tree — the serving channel (GitHub raw, a
mirror, a local checkout) never needs to be trusted, because the fetched
bytes are verified against the index and the index itself entered through
review. If the index's own channel is the concern, the answer is pinning
install to a commit hash or signing — cryptography, not middleware; the
per-publisher signature below is that answer, shipped.

### Per-publisher signing: two anchors that must agree

A release is signed by whoever publishes it, and the signature is over a
**release record** — `<name>.release.toml`, the sha256 of every file the
release is made of: the artifact, the manifest, the golden checks, and
each fixture the checks name. The publisher's minisign public key is
enrolled in `publishers.toml`, which enters the tree only through review
and is a trust root under CODEOWNERS like the index. An installing node
then holds two independent anchors — the index's digest (the reviewed
tree) and the signed record's (the publisher) — and refuses unless both
agree with each other and with the bytes. Either catches what the other
missed: a mis-reviewed index entry cannot install without the publisher's
signature, and a leaked publisher key cannot install without an index
entry a human reviewed. Signing every file rather than the artifact alone
matters because the checks are re-run on-node at admission: a fixture or
a check swapped under an unsigned sidecar could steer the verdict.

The key ceremony is the release key's (`releases.md`): `cargo xtask
plugin keygen` makes an encrypted key on the publisher's machine and
enrolls its public half; `cargo xtask plugin sign <name>` writes the
record, its signature, and the index entry in one step so the two anchors
cannot disagree by accident. CI verifies (`inseam plugin verify`) and
never holds a key. *Rejected:* signing the index in CI (a CI key is a key
the pipeline holds, and the pipeline is the thing this posture assumes
compromised); a self-claimed publish timestamp in the record (the
publisher is exactly who cooldown does not trust about time — the
registry-signed timestamp remains open below); and per-artifact
signatures without a record (one key operation binding every file is what
keeps the checks trustworthy).

### Trust is mechanical: four gates, one harness

Every version enters through a PR, and the merge gate
(`.github/workflows/plugins.yml`) runs:

1. **Reproducibility** — the committed `.wasm` must match a `--locked`
   rebuild from the PR's source on a pinned toolchain. The artifact cannot
   diverge from the code a reviewer sees.
1. **Provenance** — `inseam plugin verify --registry plugins`: every
   publisher signature holds, every signed file hashes as recorded, and
   the index agrees with each record — the same function a node runs at
   install.
1. **The conformance harness** — `inseam plugin check`: static manifest
   coherence, a real bridge mount, the hostile-input contract battery, and
   the plugin's own golden checks — mandatory, and held to a coverage rule
   ([plugins.md](plugins.md)). The *same harness* runs at authoring
   time (the skill's loop), in CI, and on every installing node at
   admission — passing once means passing everywhere, and a node never
   takes CI's word for it.
1. **AI security review** — a model reviews every plugin diff for
   capability overreach (the `hosts` allow list above all), exfiltration
   through LLM prompts, fetch requests, or emitted fragments,
   prompt-injection staging, and obfuscation; a scheduled sweep
   re-audits the whole tree weekly and files issues that become advisory
   entries. This is the WordPress reviewer, made mechanical and continuous.

The advisory feed closes the loop with the release cooldown
([plugins.md](plugins.md)): a version flagged during its soak window never
activates anywhere.

## Supply-chain posture

Using GitHub as the registry makes GitHub part of the trust surface, so
the CI and repo configuration are treated as attack targets in their own
right:

- **Fork PRs never see secrets.** All triggers are `pull_request` — never
  `pull_request_target` — so third-party PRs run with no secrets and a
  read-only token by construction. The AI-review job additionally guards
  on same-repo head; fork submissions reach review by a maintainer
  retargeting the branch after reading it, not by loosening triggers (the
  classic pwn-request).
- **Untrusted code executes only in a disarmed job.** The conformance job
  runs PR code by design (`cargo build` executes build scripts and proc
  macros), so it holds no secrets, gets `contents: read` only, and checks
  out with `persist-credentials: false`. GitHub's cache scoping keeps a
  PR's poisoned caches out of `main`'s.
- **Actions are pinned to commit SHAs**, with the version as a comment —
  mutable tags are how the 2025 `tj-actions/changed-files` compromise
  spread. Bumps change the SHA and comment together, through review.
- **A green check name proves nothing.** A PR can edit the workflow that
  emits its own required check, so the merge gate that cannot be forged is
  **human CODEOWNERS review** (`.github/CODEOWNERS`), with `.github/**`,
  `registry.toml`, `publishers.toml`, and `advisories.toml` called out as
  trust roots.
  Branch protection must require it; the AI review is explicitly
  advisory, instructed never to approve, and must never be a required
  approver — a malicious plugin could try to prompt-inject its own
  reviewer, so both AI jobs treat repository content as data to analyze
  and flag reviewer-addressed text as a finding in itself.
- **Repo settings are part of the design** (they live outside the tree, so
  they are recorded here). Applied now: read-only default `GITHUB_TOKEN`,
  workflows barred from approving PRs
  (`can_approve_pull_request_reviews = false`), SHA pinning required for
  all actions (`sha_pinning_required = true`), secrets limited to
  `ANTHROPIC_API_KEY`. **Flipped the day the repo goes public or gains
  collaborators** (today it is private and solo, so required-review
  protection would only block the owner's own pushes): branch protection
  on `main` requiring CODEOWNERS review + the conformance check, no
  direct or force pushes, and Actions approval required for all outside
  collaborators' workflow runs (a setting GitHub only exposes on public
  repos; the default covers only first-time contributors).
- **The node is the last line, and it holds without CI.** Even a fully
  subverted pipeline changes nothing a node trusts: install verifies the
  publisher's signature against the human-reviewed roster and the sha256
  against the human-reviewed index, re-runs the harness locally, and
  mount-time admission, cooldown, and capability gates run on-node.
  Corrupting what nodes install requires merging an index change *and*
  holding a publisher's key — the human-reviewed act the rest of the
  posture protects, plus a secret that never enters the pipeline.

## Graduation path (not yet)

Signals that v0 has outgrown the repo: third-party submissions arriving
faster than repo review tolerates, or plugins needing their own release
cadence. Then: a separate registry repository with the same layout (the client
already takes `--registry`, and per-publisher signing already ships),
per-publisher namespacing of plugin names, and a transparency expectation
that the index is append-only in git history. The registry-signed publish
timestamps that cooldown can honor ([plugins.md](plugins.md)) would come
from a registry-held key that countersigns records on merge — distinct
from the publisher's, and still open.

## Paths not taken

- **A checksum/validation proxy service.** See above — middleware adds
  trust surface; hashes in a reviewed tree remove it.
- **Human review board (the WordPress model).** Does not scale to
  AI-authored plugins; replaced by the reproducibility gate, the shared
  harness, and continuous AI review.
- **A registry server / package-manager infrastructure now.** npm/PyPI/OCI
  distribution remains possible for third parties (install by URL), but
  the first-party channel needs none of it while the tree is small.
- **Trusting CI verdicts at install time.** Nodes re-run the harness and
  re-verify hashes locally; CI passing is a publishing precondition, not a
  substitute for local verification.
