# GitHub check fixtures

Canned GitHub API answers referenced by `../github.checks.toml`
(`body_file` entries) and served to the plugin through the harness's
canned network — the same move `llm_returns` makes for a transform's LLM.
See `plugins/README.md` for the conventions and `docs/plugins/validation.md`
for how the harness uses them.

| File | What | Why |
| --- | --- | --- |
| `tree.json` | A `git/trees/<ref>?recursive=1` answer: four blobs (`README.md`, `docs/guide.md`, `src/lib.rs`, `logo.png`) and two tree entries, not truncated. | The checks assert that blobs become sources, trees do not, scopes filter by path prefix, and content types follow extensions. Small enough to read; shaped exactly as GitHub answers. |
| `contents.json` | A `contents/<path>` answer for `README.md` with its name and size. | The `describe` check asserts the envelope is built from the contents API without reading the file. |

Fixtures are data handed to a **sandboxed** component; no network is
contacted during checks. Keep them tiny, well-formed, and committed —
`inseam plugin install` downloads every fixture a checks file references so
the same checks can run on the installing node.
