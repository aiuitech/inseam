# inseam web console

The shared React and Vite owner interface for local and hosted nodes. It
speaks only the authenticated `/api/v1/owner/*` HTTP transport. Node logic
stays in Rust.

## Develop

Start an HTTP node on port 7337 with a local cookie, then run Vite:

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
