// Everything the server needs to start, parsed once from argv and the
// environment into one typed value. Parsing is pure so the tests can cover
// every refusal without spawning a process; `main.ts` is the only caller
// that reads `process`.

export const NODE_URL_DEFAULT = "http://127.0.0.1:7337"
export const BIND_DEFAULT = "127.0.0.1:7338"
/** The node refuses shorter tokens (`inseam serve`); refuse here too so a
 * misconfigured server fails at start, not at the first tool call. */
export const OWNER_TOKEN_BYTES_MIN = 32
/** Longest argv this parser walks. A launcher never passes more than a
 * handful of flags; anything longer is a mistake, not a use case. */
const ARGS_MAX = 32

export type Transport = "stdio" | "http"

export type Options = {
  transport: Transport
  /** The node's origin, without a path. */
  nodeUrl: string
  /** The node's owner token; sent once at login for a session cookie. */
  ownerToken: string
  /** `host:port` for the streamable HTTP listener; unused over stdio. */
  bind: string
  /** Hostnames the HTTP listener accepts in `Host` and `Origin`, beyond
   * the loopback names it always accepts. */
  allowedHosts: string[]
  /** Bearer token every HTTP request must carry; `undefined` only on a
   * loopback bind. */
  bearerToken: string | undefined
  help: boolean
}

export class OptionsError extends Error {
  constructor(message: string) {
    super(message)
    this.name = "OptionsError"
  }
}

export const USAGE = `inseam-mcp — the inseam node as MCP tools

Usage: inseam-mcp [--transport stdio|http] [--node-url <origin>]
                  [--bind <host:port>] [--allowed-host <hostname>]...

  --transport      stdio (default) for a local agent host; http for a
                   streamable HTTP endpoint at <bind>/mcp
  --node-url       the node's owner API origin (INSEAM_NODE_URL,
                   default ${NODE_URL_DEFAULT})
  --bind           where the http transport listens (INSEAM_MCP_BIND,
                   default ${BIND_DEFAULT})
  --allowed-host   an extra hostname the http transport accepts in Host
                   and Origin headers; loopback names are always accepted
  --help           this text

Environment:
  INSEAM_OWNER_TOKEN       required; the node's owner token (>= ${OWNER_TOKEN_BYTES_MIN} bytes)
  INSEAM_MCP_BEARER_TOKEN  required when --bind is not loopback; every
                           HTTP request must carry it as a Bearer token
`

type Environment = Record<string, string | undefined>

/** Parse argv (without the runtime and script) and the environment. */
export function parseOptions(args: string[], env: Environment): Options {
  if (args.length > ARGS_MAX) {
    throw new OptionsError(`too many arguments (${args.length} > ${ARGS_MAX})`)
  }
  const flags = parseFlags(args)
  if (flags.help) {
    return { ...defaults(env), help: true }
  }
  const options = withFlags(defaults(env), flags)
  validate(options)
  return options
}

type Flags = {
  transport?: string
  nodeUrl?: string
  bind?: string
  allowedHosts: string[]
  help: boolean
}

function parseFlags(args: string[]): Flags {
  const flags: Flags = { allowedHosts: [], help: false }
  let index = 0
  while (index < args.length) {
    const flag = args[index]
    if (flag === "--help" || flag === "-h") {
      flags.help = true
      index += 1
      continue
    }
    const value = args[index + 1]
    if (value === undefined) {
      throw new OptionsError(`${flag} needs a value`)
    }
    switch (flag) {
      case "--transport":
        flags.transport = value
        break
      case "--node-url":
        flags.nodeUrl = value
        break
      case "--bind":
        flags.bind = value
        break
      case "--allowed-host":
        flags.allowedHosts.push(value)
        break
      default:
        throw new OptionsError(`unknown flag ${flag}`)
    }
    index += 2
  }
  return flags
}

function defaults(env: Environment): Options {
  return {
    transport: "stdio",
    nodeUrl: env.INSEAM_NODE_URL ?? NODE_URL_DEFAULT,
    ownerToken: env.INSEAM_OWNER_TOKEN ?? "",
    bind: env.INSEAM_MCP_BIND ?? BIND_DEFAULT,
    allowedHosts: [],
    bearerToken: env.INSEAM_MCP_BEARER_TOKEN,
    help: false,
  }
}

function withFlags(options: Options, flags: Flags): Options {
  return {
    ...options,
    transport: parseTransport(flags.transport ?? options.transport),
    nodeUrl: flags.nodeUrl ?? options.nodeUrl,
    bind: flags.bind ?? options.bind,
    allowedHosts: flags.allowedHosts,
  }
}

function parseTransport(value: string): Transport {
  if (value === "stdio" || value === "http") {
    return value
  }
  throw new OptionsError(`--transport must be stdio or http, not ${value}`)
}

function validate(options: Options): void {
  if (Buffer.byteLength(options.ownerToken) < OWNER_TOKEN_BYTES_MIN) {
    throw new OptionsError(
      `INSEAM_OWNER_TOKEN must contain at least ${OWNER_TOKEN_BYTES_MIN} bytes`
    )
  }
  validateNodeUrl(options.nodeUrl)
  if (options.transport === "http") {
    validateBind(options)
  }
}

function validateNodeUrl(nodeUrl: string): void {
  let url: URL
  try {
    url = new URL(nodeUrl)
  } catch {
    throw new OptionsError(`--node-url ${nodeUrl} is not a URL`)
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new OptionsError(`--node-url ${nodeUrl} must be http or https`)
  }
  if (url.pathname !== "/" || url.search !== "" || url.hash !== "") {
    throw new OptionsError(
      `--node-url ${nodeUrl} must be an origin with no path or query`
    )
  }
}

function validateBind(options: Options): void {
  const address = parseBind(options.bind)
  const loopback = isLoopback(address.host)
  if (loopback) {
    return
  }
  if (options.bearerToken === undefined) {
    throw new OptionsError(
      `--bind ${options.bind} is not loopback; set INSEAM_MCP_BEARER_TOKEN so callers must authenticate`
    )
  }
  if (Buffer.byteLength(options.bearerToken) < OWNER_TOKEN_BYTES_MIN) {
    throw new OptionsError(
      `INSEAM_MCP_BEARER_TOKEN must contain at least ${OWNER_TOKEN_BYTES_MIN} bytes`
    )
  }
}

export type BindAddress = { host: string; port: number }

/** `host:port`, with IPv6 in brackets (`[::1]:7338`). */
export function parseBind(bind: string): BindAddress {
  const separator = bind.lastIndexOf(":")
  if (separator <= 0) {
    throw new OptionsError(`--bind ${bind} must be host:port`)
  }
  const host = bind.slice(0, separator)
  const port = Number(bind.slice(separator + 1))
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new OptionsError(`--bind ${bind} has no valid port`)
  }
  const bracketed = host.startsWith("[") && host.endsWith("]")
  return { host: bracketed ? host.slice(1, -1) : host, port }
}

export function isLoopback(host: string): boolean {
  if (host === "localhost" || host === "::1") {
    return true
  }
  return host.startsWith("127.")
}
