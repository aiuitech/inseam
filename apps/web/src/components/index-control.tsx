import { type FormEvent, useState } from "react"
import { RefreshCw } from "lucide-react"

import type { Host, IndexReport, IndexRoot } from "@/api"
import { Button } from "@/components/ui/button"

type Props = {
  hosts: Host[]
  roots: IndexRoot[]
  pending: boolean
  report: IndexReport | null
  onIndex: (root: string, host?: string) => void
}

export function IndexControl({
  hosts,
  roots,
  pending,
  report,
  onIndex,
}: Props) {
  const [root, setRoot] = useState(roots[0]?.id ?? "")
  const [host, setHost] = useState(hosts.length === 1 ? hosts[0].id : "")

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    if (root) onIndex(root, host || undefined)
  }

  return (
    <section className="index-control">
      <div>
        <p className="eyebrow">maintenance / bounded roots</p>
        <h2>reconcile a host</h2>
      </div>
      {roots.length > 0 ? (
        <form onSubmit={submit}>
          <label>
            root
            <select
              value={root}
              onChange={(event) => setRoot(event.target.value)}
            >
              {roots.map((entry) => (
                <option value={entry.id} key={entry.id}>
                  {entry.id}
                </option>
              ))}
            </select>
          </label>
          {hosts.length > 1 ? (
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
          Start the server with --index-root id=/path.
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
