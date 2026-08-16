# Plugin Registry

The registry problem in one sentence: sandboxed plugins hold credentials and
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

- **`plugins/registry.toml`** — the index: name, version, artifact path,
  sha256. The committed hash is the integrity anchor.
- **`plugins/advisories.toml`** — the yank/flag feed, appended by PR.
- **`plugins/<name>/`** — source, committed artifact, manifest, golden
  checks, fixtures. One directory per plugin; the layout is documented in
  `plugins/README.md`.

`inseam plugin install <name>` fetches the index and artifact from GitHub
raw (or any checkout via `--registry <path>` — one code path, so tests
never need the network), verifies the artifact's sha256 against the index,
refuses anything in the advisory feed, downloads the golden checks and
their fixtures, runs the conformance harness locally, and appends the
composition entry. Mount-time gates (admission, release cooldown,
capability widening) still apply — install is convenience, not trust.

### Why no proxy

A checksum/validation proxy between the node and GitHub was considered and
rejected: it adds an operator to trust and a single point of failure while
providing nothing the design doesn't already have. Integrity comes from
hashes committed in a reviewed tree — the serving channel (GitHub raw, a
mirror, a local checkout) never needs to be trusted, because the fetched
bytes are verified against the index and the index itself entered through
review. If the index's own channel is the concern, the answer is pinning
install to a commit hash or signing the index in CI — cryptography, not
middleware.

### Trust is mechanical: three gates, one harness

Every version enters through a PR, and the merge gate
(`.github/workflows/plugins.yml`) runs:

1. **Reproducibility** — the committed `.wasm` must match a `--locked`
   rebuild from the PR's source on a pinned toolchain. The artifact cannot
   diverge from the code a reviewer sees.
2. **The conformance harness** — `inseam plugin check`: static manifest
   coherence, a real bridge mount, the hostile-input contract battery, and
   the plugin's own golden checks. The *same harness* runs at authoring
   time (the skill's loop), in CI, and on every installing node at
   admission — passing once means passing everywhere, and a node never
   takes CI's word for it.
3. **AI security review** — a model reviews every plugin diff for
   capability overreach, exfiltration through LLM prompts or emitted
   fragments, prompt-injection staging, and obfuscation; a scheduled sweep
   re-audits the whole tree weekly and files issues that become advisory
   entries. This is the WordPress reviewer, made mechanical and continuous.

The advisory feed closes the loop with the release cooldown
([plugins.md](plugins.md)): a version flagged during its soak window never
activates anywhere.

## Graduation path (not yet)

Signals that v0 has outgrown the repo: third-party submissions arriving
faster than repo review tolerates, or plugins needing their own release
cadence. Then: a separate registry repository with the same layout (the
client already takes `--registry`), per-publisher namespacing, an index
signed in CI (minisign), and a transparency expectation that the index is
append-only in git history. The registry-signed publish timestamps that
cooldown can honor ([plugins.md](plugins.md)) fall out of that signing
step.

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
