# ocr

Install: `inseam plugin install ocr` (verifies, checks, and mounts — see
`docs/plugins/registry.md`), or mount a local build as below. Golden checks
live in `ocr.checks.toml` (fixtures documented in `fixtures/README.md`);
validate any change with `inseam plugin check plugins/ocr/ocr.wasm`.

Extracts text from images encountered during indexing. Claims `image/png`,
`image/jpeg`, and `image/webp` wherever they appear — image files at the
root, and images a document links to (fragments carrying a content
reference; `docs/indexing/transforms.md`) — reads the bytes the host hands
it, and makes one metered vision call asking the model to transcribe all
visible text verbatim. Non-empty transcripts (capped at 20,000 characters) are
emitted as a single `text/plain;via=ocr` fragment with the `transcribes`
relation; images with no text, or runs where a capability is withheld or the
LLM budget is spent, produce no fragments. Requires the `llm` and
`source_bytes` capabilities with an LLM call budget of 25 per index run.

## Mounting

Add to your node's `composition.toml`:

```toml
[[entry]]
id = "ocr"
plugin = "wasm:plugins/ocr/ocr.wasm"
[entry.config]
# cooldown_days = 7    # release cooldown for newly observed versions
# allow_new = true     # explicit consent to skip the cooldown
```
