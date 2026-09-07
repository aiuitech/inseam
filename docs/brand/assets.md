# Brand Assets

`packages/brand/assets/` holds the brand sources and their rendered outputs. They ship inside the [`@inseam/brand` package](package.md), so every app that installs the package has them. The brand itself — mark geometry, colors, type, voice — is defined in `docs/brand/README.md`.

## Sources (SVG, hand-edited)

| File | What |
| --- | --- |
| `mark.svg` | The `▬●▬` stitch mark, transparent background |
| `stitch.svg` | The `●▬▬●▬▬●` strip for rules/banners, transparent background |
| `favicon.svg` | Mark on a square ground tile |
| `icon-macos.svg` | Mark on a rounded ground tile with Apple-template margins (824px tile in a 1024px canvas) |

## Generated (committed, rebuilt by script)

`packages/brand/assets/build-icons.sh` renders the sources into `favicon.ico` (16/32/48), `icon-macos.icns` (all macOS iconset sizes), and reference PNGs under `png/`. It needs ImageMagick (`brew install imagemagick`) and macOS `iconutil` for the `.icns`. Outputs are committed so app builds never need those tools — re-run the script after editing any source SVG.

## Who reads them

- Web apps import them through the package: `import favicon from "@inseam/brand/assets/favicon.svg"` gives Vite a URL it hashes into the build like any other asset.
- `apps/macos/build.sh` copies `icon-macos.icns` into the app bundle as `Inseam.icns` (referenced by `CFBundleIconFile`).
- The Astro sites (`apps/www.inseam.io`, `apps/docs.inseam.io`) still carry their own copies under `public/` and `src/assets/`; they are not in the pnpm workspace yet.
