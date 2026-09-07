import { Link2, Link2Off, PlugZap } from "lucide-react"

import type { Grant, Host } from "@/api"
import { Badge } from "@inseam/brand/components/ui/badge"
import { Button } from "@inseam/brand/components/ui/button"

type Props = {
  grants: Grant[]
  hosts: Host[]
  pending: boolean
  callbackUrl: string
  notice: { kind: "ok" | "error"; text: string } | null
  onConnect: (grant: string) => void
  onDisconnect: (grant: string) => void
}

function stateLabel(grant: Grant): string {
  switch (grant.state.state) {
    case "authorized":
      return grant.state.account
        ? `connected as ${grant.state.account}`
        : "connected"
    case "missing_secret":
      return `set ${grant.state.env} on the server`
    case "unauthorized":
      return "not connected"
  }
}

/** The accounts this node can hold a grant to, and the hosts each one
 * stewards once authorized. Connecting sends this tab to the provider; the
 * node's callback route brings it back here. */
export function ConnectionsPanel({
  grants,
  hosts,
  pending,
  callbackUrl,
  notice,
  onConnect,
  onDisconnect,
}: Props) {
  return (
    <section className="connections-panel">
      <div>
        <p className="eyebrow">connections / accounts</p>
        <h2>connect an account</h2>
      </div>
      {notice ? (
        <p className={notice.kind === "error" ? "form-error" : "notice-ok"}>
          {notice.text}
        </p>
      ) : null}
      {grants.length === 0 ? (
        <p className="index-empty">
          No grants on this node. Enable the google entry in the composition.
        </p>
      ) : null}
      {grants.map((grant) => {
        const connected = grant.state.state === "authorized"
        const blocked = grant.state.state === "missing_secret"
        const stewarded = hosts.filter((host) =>
          host.display_name.endsWith(
            grant.state.state === "authorized" && grant.state.account
              ? grant.state.account
              : "\u0000"
          )
        )
        return (
          <div className="grant-row" key={grant.id}>
            <div className="grant-copy">
              <strong>{grant.id}</strong>
              <span>{grant.provider}</span>
              <small>{stateLabel(grant)}</small>
              {stewarded.length > 0 ? (
                <div className="grant-hosts">
                  {stewarded.map((host) => (
                    <Badge variant="outline" key={host.id}>
                      {host.kind}
                    </Badge>
                  ))}
                </div>
              ) : null}
            </div>
            <div className="grant-actions">
              {connected ? (
                <Button
                  variant="outline"
                  disabled={pending}
                  onClick={() => onDisconnect(grant.id)}
                >
                  <Link2Off /> disconnect
                </Button>
              ) : null}
              <Button
                disabled={pending || blocked}
                onClick={() => onConnect(grant.id)}
                title={
                  blocked
                    ? "the server needs the client id first"
                    : `sign in at ${grant.provider}`
                }
              >
                {connected ? <PlugZap /> : <Link2 />}
                {connected ? "reconnect" : "connect"}
              </Button>
            </div>
          </div>
        )
      })}
      <p className="grant-footnote">
        register <code>{callbackUrl}</code> as a redirect URI on the OAuth client
      </p>
    </section>
  )
}
