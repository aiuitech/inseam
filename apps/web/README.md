# inseam web console

The shared React and Vite owner interface for local and hosted nodes. It
speaks only the authenticated `/api/v1/owner/*` HTTP transport. Node logic
stays in Rust.

## Develop

After a fresh clone, with `inseam` on PATH (`cargo install --path
crates/inseam-cli` from the repo root):

```sh
cd apps/web
pnpm install
pnpm serve
```

`pnpm install` here installs the whole pnpm workspace at the repo root, which
links in `@inseam/brand` from `packages/brand` — the theme, assets, and shared
components ([docs/brand/package.md](../../docs/brand/package.md)). Everything in
`src/index.css` is this console's own layout; brand changes go in the package.

`pnpm serve` starts a node on `127.0.0.1:7337` and Vite on
`http://localhost:5173`, and prints the owner token to paste into the login
screen. The defaults are made for a throwaway local setup: the hosted
composition, the repo's own `docs/` as the index root (`docs`), a data dir at
`apps/web/.inseam`, and a fixed development token. Each is an environment
variable when you want something else:

```sh
INSEAM_INDEX_ROOTS=notes=/absolute/path INSEAM_OWNER_TOKEN=... pnpm serve
```

`pnpm dev` runs Vite alone against a node you started yourself:

```sh
INSEAM_OWNER_TOKEN=0123456789abcdef0123456789abcdef \
  inseam --composition ../../deploy/hosted/composition.toml serve \
  --cookie local-http --index-root documents=/absolute/path

pnpm dev
```

Vite proxies `/api` to `127.0.0.1:7337`. `pnpm build` writes `dist/`, which
`inseam serve --web-dir` can serve from the same origin.

The project began with the required preset:

```sh
pnpm dlx shadcn@latest init --preset b3DpieXUt6 --template vite --pointer
```
