// One command for a fresh clone: start a local inseam node on 127.0.0.1:7337
// and the Vite dev server that proxies `/api` to it. Everything the node needs
// (token, composition, data dir, index root) has a development default so the
// only prerequisite is an `inseam` binary on PATH.
import { spawn, spawnSync } from "node:child_process"
import { existsSync } from "node:fs"
import { dirname, resolve } from "node:path"
import { fileURLToPath } from "node:url"

const webDir = resolve(dirname(fileURLToPath(import.meta.url)), "..")
const repoDir = resolve(webDir, "..", "..")

// A fixed token is acceptable here: the node binds to loopback and the cookie
// policy is local-http, so this only ever guards a developer's own machine.
// Override with INSEAM_OWNER_TOKEN for anything else.
const ownerTokenDefault = "inseam-local-development-owner-token-0000"
const ownerToken = process.env.INSEAM_OWNER_TOKEN ?? ownerTokenDefault

// The repo's own docs are the default corpus: they exist after every clone and
// contain nothing personal. Override with INSEAM_INDEX_ROOTS=id=/abs/path.
const indexRoots =
  process.env.INSEAM_INDEX_ROOTS ?? `docs=${resolve(repoDir, "docs")}`

// A data dir inside apps/web keeps this throwaway index apart from any real
// node in the platform data directory.
const dataDir = process.env.INSEAM_DATA_DIR ?? resolve(webDir, ".inseam")
const composition =
  process.env.INSEAM_COMPOSITION ??
  resolve(repoDir, "deploy", "hosted", "composition.toml")

if (ownerToken.length < 32) {
  console.error("INSEAM_OWNER_TOKEN must be at least 32 bytes.")
  process.exit(1)
}
if (!existsSync(composition)) {
  console.error(`Composition not found: ${composition}`)
  process.exit(1)
}
const probe = spawnSync("inseam", ["--version"], { stdio: "ignore" })
if (probe.error !== undefined || probe.status !== 0) {
  console.error(
    "`inseam` is not on PATH. From the repo root run:\n\n" +
      "  cargo install --path crates/inseam-cli\n"
  )
  process.exit(1)
}

console.log(`inseam node   http://127.0.0.1:7337`)
console.log(`owner token   ${ownerToken}`)
console.log(`index roots   ${indexRoots}`)
console.log(`data dir      ${dataDir}`)
console.log(`composition   ${composition}\n`)

// Each child leads its own process group so shutdown can signal the whole
// tree (pnpm wrappers included) rather than only the direct child.
const node = spawn(
  "inseam",
  ["--composition", composition, "serve", "--cookie", "local-http"],
  {
    stdio: "inherit",
    detached: true,
    env: {
      ...process.env,
      INSEAM_OWNER_TOKEN: ownerToken,
      INSEAM_INDEX_ROOTS: indexRoots,
      INSEAM_DATA_DIR: dataDir,
    },
  }
)
const vite = spawn(
  process.execPath,
  [resolve(webDir, "node_modules", "vite", "bin", "vite.js")],
  { cwd: webDir, stdio: "inherit", detached: true }
)

// Either process exiting ends the session: a half-running console is worse
// than none, and the developer restarts with one command anyway.
let shuttingDown = false
function shutdown(code) {
  if (shuttingDown) return
  shuttingDown = true
  for (const child of [node, vite]) {
    if (child.exitCode === null) {
      try {
        process.kill(-child.pid, "SIGTERM")
      } catch {
        // The group already exited between the check and the signal.
      }
    }
  }
  process.exit(code)
}
node.on("exit", (code) => shutdown(code ?? 1))
vite.on("exit", (code) => shutdown(code ?? 1))
process.on("SIGINT", () => shutdown(0))
process.on("SIGTERM", () => shutdown(0))
process.on("SIGHUP", () => shutdown(0))
