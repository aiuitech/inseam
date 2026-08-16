# Brand Assets

`assets/` holds the brand sources and their rendered outputs. The brand itself — mark geometry, colors, type, voice — is defined in `docs/brand/README.md`.

## Sources (SVG, hand-edited)

| File | What |
| --- | --- |
| `mark.svg` | The `▬●▬` stitch mark, transparent background |
| `stitch.svg` | The `●▬▬●▬▬●` strip for rules/banners, transparent background |
| `favicon.svg` | Mark on a square ground tile |
| `icon-macos.svg` | Mark on a rounded ground tile with Apple-template margins (824px tile in a 1024px canvas) |

## Generated (committed, rebuilt by script)

`assets/build-icons.sh` renders the sources into `favicon.ico` (16/32/48), `icon-macos.icns` (all macOS iconset sizes), and reference PNGs under `assets/png/`. It needs ImageMagick (`brew install imagemagick`) and macOS `iconutil` for the `.icns`. Outputs are committed so app builds never need those tools — re-run the script after editing any source SVG.

`apps/macos/build.sh` copies `icon-macos.icns` into the app bundle as `Inseam.icns` (referenced by `CFBundleIconFile`).
