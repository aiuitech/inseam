import { Database, LogOut, Network, ScanSearch } from "lucide-react"

import type { Host, OwnerInfo, StatusReport } from "@/api"
import { Button } from "@/components/ui/button"

type Props = {
  hosts: Host[]
  info: OwnerInfo
  status: StatusReport
  onLogout: () => void
}

function Metric({ label, value }: { label: string; value: number | string }) {
  return (
    <div className="metric">
      <span>{label}</span>
      <strong>{value}</strong>
    </div>
  )
}

export function NodeSidebar({ hosts, info, status, onLogout }: Props) {
  return (
    <aside className="node-sidebar">
      <header className="brand-lockup">
        <span className="brand-mark">▬●▬</span>
        <span>inseam</span>
      </header>
      <div className="node-presence">
        <span className="presence-dot" />
        <span>node online</span>
        <small>v{info.version}</small>
      </div>
      <section className="metric-grid" aria-label="Node statistics">
        <Metric label="sources" value={status.sources.toLocaleString()} />
        <Metric
          label="indexed"
          value={status.indexed_sources.toLocaleString()}
        />
        <Metric label="fragments" value={status.fragments.toLocaleString()} />
        <Metric label="relations" value={status.relations.toLocaleString()} />
      </section>
      <section className="side-section">
        <h2>
          <Network /> hosts / {hosts.length}
        </h2>
        <div className="host-list">
          {hosts.map((host) => (
            <div className="host-row" key={host.id}>
              <span>{host.display_name}</span>
              <small>{host.kind}</small>
            </div>
          ))}
          {hosts.length === 0 ? <p>no hosts mounted</p> : null}
        </div>
      </section>
      <section className="side-section engine-state">
        <h2>
          <Database /> index engine
        </h2>
        <p>{status.embedding_model ?? "no embedder mounted"}</p>
        <small>
          {status.embedding_dimensions > 0
            ? `${status.embedding_dimensions} dimensions`
            : `${status.search_rows} search rows`}
        </small>
      </section>
      <div className="sidebar-spacer" />
      <div className="sidebar-footer">
        <ScanSearch /> {info.index_roots.length} approved index roots
        <Button
          variant="ghost"
          size="icon-sm"
          onClick={onLogout}
          title="Sign out"
        >
          <LogOut />
          <span className="sr-only">Sign out</span>
        </Button>
      </div>
    </aside>
  )
}
