# The `@inseam/brand` Package

`packages/brand` is the brand as code for Tailwind v4 and shadcn apps: one stylesheet, the assets, and the handful of components tuned to them. Two apps consume it today — the node console in `apps/web` and the hosted console in the separate `inseam-console` repo — and it exists so that they cannot drift apart.

It is a source package: TypeScript and CSS as written, no build step. Every consumer is a Vite app, and Vite compiles the package's `.tsx` the same way it compiles the app's own.

## What it exports

| Import | What |
| --- | --- |
| `@inseam/brand/theme.css` | Tokens, the `@theme` mapping, base styles, the stitch grid, and the utilities `eyebrow`, `label-caps`, `display`, `stitch`, plus the `brand-mark`, `cursor-block`, `presence-dot`, `metric-grid`/`metric`, and `rise` classes. Also loads JetBrains Mono, `tw-animate-css`, and shadcn's base variants. |
| `@inseam/brand/assets/*` | `mark.svg`, `stitch.svg`, `favicon.svg`, `favicon.ico`, and the macOS icon ([assets.md](assets.md)). |
| `@inseam/brand/components/ui/*` | `button`, `badge`, `input`, `card`, `label`, `alert`, `dialog`, `select` — shadcn `base-lyra` components on Base UI, restyled: square corners, small mono type, `warning` and `info` tones. |
| `@inseam/brand/components/brand-lockup` | The `▬●▬ inseam` wordmark. Pass `render` to make it a link. |
| `@inseam/brand/lib/utils` | `cn`. |

## Using it

The app's root stylesheet loads Tailwind, then the theme, then its own rules:

```css
@import "tailwindcss";
@import "@inseam/brand/theme.css";
```

Components import by path:

```tsx
import { Button } from "@inseam/brand/components/ui/button"
import { BrandLockup } from "@inseam/brand/components/brand-lockup"

<BrandLockup render={<Link to="/" />} />
```

The theme declares its own component directory as a Tailwind `@source`, so the classes those components use are generated even though they live outside the app's tree.

Peer dependencies are the libraries the components are built on — `react`, `@base-ui/react`, `class-variance-authority`, `clsx`, `tailwind-merge`, `lucide-react`, `tailwindcss` — and the app installs them itself, so there is exactly one React.

## How it is distributed

Inside this repo, `apps/web` depends on it as `workspace:*`; the repo root is a pnpm workspace containing `apps/web` and `packages/brand`, and `pnpm install` anywhere under either links them together. Edits to the package show up in the app's dev server immediately.

Outside this repo, it is a git dependency on this repo's `main`, scoped to the package directory:

```json
"@inseam/brand": "git+ssh://git@github.com/aiuitech/inseam.git#path:packages/brand"
```

pnpm records the exact commit in the consumer's lockfile; `pnpm update @inseam/brand` moves to the current `main`. The repo is private, so wherever that install runs needs SSH access to it. There is no npm publish step and no registry: the repo is the distribution.

## Adding a component

Run the shadcn CLI from `packages/brand` — its `components.json` targets the package's own paths — then restyle the result to the brand and import `cn` from `../../lib/utils` relatively, so the file resolves the same way whether the package is linked or fetched from git. Components that only one app needs stay in that app under its own `components/ui`; each app's `components.json` still points there.
