# OCR check fixtures

Byte fixtures referenced by `../ocr.checks.toml` (`bytes_file` entries) and
fed to the plugin as `source-bytes` during conformance checks. See
`plugins/README.md` for the conventions and `docs/plugins/validation.md`
for how the harness uses them.

| File | What | Why |
| --- | --- | --- |
| `pixel.png` | A minimal valid PNG: 1×1 transparent RGBA, 67 bytes. | The checks assert *plumbing* (bytes reach the plugin, the transcript comes back shaped right), not OCR quality — the harness's LLM is canned, so the image content never matters. The smallest well-formed PNG keeps the registry payload and the install download honest. |

Fixtures are inputs handed to a **sandboxed** component with a **fake** LLM:
they are never sent anywhere and nothing is asserted about their visual
content. Keep them tiny, well-formed, and committed — `inseam plugin
install` downloads every fixture a checks file references so the same
checks can run on the installing node.
