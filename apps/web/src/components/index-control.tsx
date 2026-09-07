import { type FormEvent, useState } from "react"
import { RefreshCw } from "lucide-react"

import type { Host, IndexReport, IndexRoot } from "@/api"
import { Button } from "@inseam/brand/components/ui/button"

type Props = {
  hosts: Host[]
  roots: IndexRoot[]
  pending: boolean
  report: IndexReport | null
  onIndex: (root: string, host?: string) => void
}

/** One thing the owner may index: a root the server approved at start
 * (`--index-root id=/path`, sent by id) or a folder configured on a host
 * (sent verbatim with its host). */
type Scope = {
  key: string
  label: string
  root: string
  host?: string
}

function scopesOf(hosts: Host[], roots: IndexRoot[]): Scope[] {
  const approved = roots.map((entry) => ({
    key: `approved:${entry.id}`,
    label: entry.id,
    root: entry.id,
  }))
  const configured = hosts.flatMap((host) =>
    host.roots.map((root) => ({
      key: `host:${host.id}:${root}`,
      label: hosts.length > 1 ? `${host.display_name} · ${root}` : root,
      root,
      host: host.id,
    }))
  )
  return [...approved, ...configured]
}

export function IndexControl({
  hosts,
  roots,
  pending,
  report,
  onIndex,
}: Props) {
  const scopes = scopesOf(hosts, roots)
  const [key, setKey] = useState(scopes[0]?.key ?? "")
  const [host, setHost] = useState(hosts.length === 1 ? hosts[0].id : "")
  const chosen = scopes.find((scope) => scope.key === key) ?? scopes[0]
  // An approved root still needs a host once several are mounted; a
  // configured folder already names its own.
  const needsHost =
    chosen !== undefined && chosen.host === undefined && hosts.length > 1

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    if (!chosen) return
    onIndex(chosen.root, chosen.host ?? (host || undefined))
  }

  return (
    <section className="index-control">
      <div>
        <p className="eyebrow">maintenance / bounded roots</p>
        <h2>reconcile a host</h2>
      </div>
      {scopes.length > 0 ? (
        <form onSubmit={submit}>
          <label>
            root
            <select
              value={chosen?.key ?? ""}
              onChange={(event) => setKey(event.target.value)}
            >
              {scopes.map((scope) => (
                <option value={scope.key} key={scope.key}>
                  {scope.label}
                </option>
              ))}
            </select>
          </label>
          {needsHost ? (
            <label>
              host
              <select
                value={host}
                onChange={(event) => setHost(event.target.value)}
                required
              >
                <option value="" disabled>
                  choose host
                </option>
                {hosts.map((entry) => (
                  <option value={entry.id} key={entry.id}>
                    {entry.display_name}
                  </option>
                ))}
              </select>
            </label>
          ) : null}
          <Button type="submit" variant="outline" disabled={pending}>
            <RefreshCw className={pending ? "animate-spin" : ""} />
            {pending ? "indexing" : "run index"}
          </Button>
        </form>
      ) : (
        <p className="index-empty">
          Add folders to index under configuration → connections → local
          filesystem, or start the server with --index-root id=/path.
        </p>
      )}
      {report ? (
        <p className="index-report">
          {report.sources_seen} seen / {report.indexed} indexed /{" "}
          {report.fragments} fragments
        </p>
      ) : null}
    </section>
  )
}
