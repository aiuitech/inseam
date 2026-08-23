# Releases

How an `inseam` binary gets onto a machine and replaces itself. The intent is in [design/releases.md](../design/releases.md); this page is the mechanics.

## The origin and the manifest

A release **origin** is a base URL, or a local directory, that serves three kinds of file at relative paths:

```
manifest.json            # version per cohort, sha256 per target
manifest.json.minisig    # detached minisign signature over manifest.json
<path>/inseam-<target>.tar.gz   # the artifacts the manifest names
```

```json
{
  "schema": 1,
  "cohorts": {
    "stable": {
      "version": "0.2.0",
      "artifacts": {
        "x86_64-unknown-linux-gnu": { "path": "../../download/v0.2.0/inseam-x86_64-unknown-linux-gnu.tar.gz", "sha256": "…" }
      }
    }
  }
}
```

`path` resolves against the origin with URL-join semantics (so `..` works, but never above the host); for a directory origin `..` is refused. The stock distribution's origin is `https://github.com/aiuitech/inseam/releases/latest/download/`, so the manifest lives on the latest published release and names artifacts by per-tag path — which is how the same manifest can point at an older release's tarballs (a rollback).

## `inseam self update`

```sh
inseam self update                  # fetch, verify, and swap if the cohort names another version
inseam self update --check          # report only
inseam self update --cohort canary  # follow another cohort (INSEAM_RELEASE_COHORT)
inseam self update --origin https://mirror.example/inseam/   # a mirror (INSEAM_RELEASE_ORIGIN)
```

The verifying key is compiled into the binary by its distribution (`Distribution::first_party()` carries the open-source key; a custom distribution sets its own with `.with_update_channel(UpdateChannel { origin, public_key })`). Overriding the origin never changes the key, so a stock binary pointed at a mirror still runs only what inseam signed. The version comparison is inequality, not ordering: whatever the cohort names is what gets installed, which is what makes promoting an older version a rollback.

The swap writes the new executable beside the running one and renames it into place; the running process keeps serving from its old inode until restarted. On a hosted node a systemd timer runs the command inside the maintenance window and restarts the service.

## Cutting a stock release

1. Push a `v*` tag. `release.yml` calls the reusable `build-distribution.yml` for each target, assembles `checksums.txt`, and creates a **draft** GitHub release. Drafts are invisible to `releases/latest`, so neither `install.sh` nor `self update` can see an unpromoted build.
2. On your own machine, once: `cargo xtask release keygen` (writes `~/.config/inseam/release.key`, encrypted; prints the public key to paste into `UpdateChannel::first_party`).
3. `cargo xtask release promote v0.2.0` — builds `manifest.json` from the tag's `checksums.txt`, signs it, undrafts the release if it is the newest, and uploads the manifest and signature to whichever release is `latest`. Nodes see it on their next `self update`.
4. `cargo xtask release promote v0.1.0` is the rollback: the manifest on `latest` now names the older tarballs.

CI never holds the signing key. A compromised pipeline can add artifacts; it cannot make a node run them.

## A private distribution

A custom distribution ([plugins/distributions.md](plugins/distributions.md)) pins `inseam-cli` by git tag and calls the same reusable build workflow at that tag:

```yaml
jobs:
  build:
    uses: aiuitech/inseam/.github/workflows/build-distribution.yml@v0.2.0
    with:
      package: hosted-distribution
      targets: '[{"target":"x86_64-unknown-linux-gnu","runner":"ubuntu-22.04"}]'
```

Its artifacts go to its own origin (an R2 bucket, say), its manifest is signed with its own key, and its binary's `UpdateChannel` names both. `cargo xtask release` here is GitHub-specific; a private origin writes the same manifest shape — `schema`, `cohorts`, per-target `path` and `sha256` — and signs it with `minisign`.
