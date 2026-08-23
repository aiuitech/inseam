# Releases

How a built `inseam` binary reaches a machine and replaces the one running there. The concept covers the stock open-source binary, a self-hoster's mirror, and the [hosted](hosted-service.md) distribution, because they are one mechanism with different inputs — a release is a signed manifest at an origin, and `inseam self update` is the only consumer.

## One command, one format, any origin

`inseam self update` resolves the current version at an **origin**, verifies a signature, downloads the tarball for the running target, verifies its checksum, renames it over the running executable, and exits so the supervisor restarts it. Nothing about that path knows where the bytes came from.

An origin is a base URL that serves `manifest.json`, its detached signature, and the artifacts the manifest names, at paths relative to the base. GitHub Releases is not a special case with its own API: the stock distribution's origin is `https://github.com/aiuitech/inseam/releases/latest/download/`, which GitHub redirects to the latest release's assets, so "latest" costs no API call and no rate-limit budget. The release workflow uploads `manifest.json` as one more asset beside the tarballs and `checksums.txt` it already publishes. A mirror is a plain copy of those files; an air-gapped fleet points at its own.

The format is fixed so the origin can vary:

```json
{
  "schema": 1,
  "cohorts": {
    "stable": {
      "version": "1.4.0",
      "artifacts": {
        "x86_64-unknown-linux-gnu": { "path": "1.4.0/inseam-x86_64-unknown-linux-gnu.tar.gz", "sha256": "…" }
      }
    }
  }
}
```

The manifest names a version **per cohort**, with the checksum of every artifact under it. Artifacts carry no signature of their own; the manifest's signature binds them. The stock distribution only ever publishes `stable`; the hosted distribution publishes `canary` and `stable` and moves tenants between them ([hosted-service](hosted-service.md)).

## The key is the distribution's, the origin is an option

The verifying public key is compiled into the binary through the distribution — `Distribution::first_party()` carries the inseam open-source key, a custom distribution supplies its own through `with_update_channel(origin, public_key)`. The origin alone is overridable at runtime, by `--origin` or `INSEAM_RELEASE_ORIGIN`, and that is the whole of the surface.

Separating the two is what makes the override safe to expose. Pointing a stock binary at a different origin produces a *mirror*: it will still run only what inseam signed, so the worst a hostile origin can do is serve nothing. Changing the key is a build-time act, which is exactly the custom-distribution path ([plugins](plugins.md)) — a private binary trusts a private key and a private origin, decided by whoever compiled it. Resolution order is flag, then environment, then the distribution's default. The origin is deliberately not a composition entry: the updater runs as a one-shot process before the node is up, and should not need to parse the composition to find its own upstream.

## Signing happens off the build machine

CI builds, tests, and uploads artifacts and checksums; CI does not sign. Promotion — rewriting the manifest to name a version for a cohort and signing it — is an operator action with a minisign key that never leaves the operator's machine. A compromised pipeline can add artifacts to an origin; it cannot make any node run them. Rollback is promoting the previous version again, which is the same action.

This mirrors the registry's stance for the loaded tier ([registry](registry.md)): the channel that carries bytes and the authority that says "run these" are distinct, and only the second holds a key worth stealing.

## A private distribution is two crates and a tag

A custom distribution lives in its own repository as the two crates `docs/plugins/distributions.md` already prescribes — the linked plugins and the three-line binary — depending on `inseam-cli` by git **tag**, never branch, so the resulting binary is reproducible from one commit of each repository. Its test suite enrolls the conformance battery, so a private plugin cannot ship without passing what every linked plugin passes.

The build pipeline is not a second pipeline. The public release workflow's build job is a reusable workflow the private repository calls at the same tag it depends on — same toolchain pin, same `--locked`, same SHA-pinned actions — so the two cannot drift. Updating the core or updating a private plugin is then the same motion: bump or commit, tag, build, promote to canary, promote to stable.

## Paths not taken

- **A self-update library with its own release-provider abstraction.** The providers differ only in URL shape, and the one that tempted special-casing (GitHub) already serves a plain asset path. A fixed manifest format over a base URL is less code and one verification path.
- **Signing in CI.** Convenient, and it hands the run-this authority to the same credential that builds; an origin compromise would then be a fleet compromise. Build and promote are separated so that they fail separately.
- **Per-artifact signatures.** One signed manifest that carries checksums binds everything with one key operation; per-artifact signing multiplies the ceremony without adding what the manifest does not already prove.
- **The origin as a composition entry.** The updater must work before the node does, and the composition is the node's.
- **Polling from inside the serving process.** A supervisor timer running `inseam self update` in the maintenance window keeps the updater a process that can replace the binary and exit, and keeps the window a supervisor setting rather than node state.

## Open questions

- Whether the stock distribution should also publish a `canary` cohort for self-hosters who opt in, or leave early versions to building from source.
- Key rotation: how a node that trusts key A learns to trust key B without a push, presumably a manifest signed by A that names B.
