import { useCallback, useEffect, useState } from "react"

import {
  api,
  ApiError,
  type ExpandResponse,
  type FetchResponse,
  type Host,
  type IndexReport,
  type OwnerInfo,
  type QueryResult,
  type StatusReport,
} from "@/api"
import { IndexControl } from "@/components/index-control"
import { LoginScreen } from "@/components/login-screen"
import { NodeSidebar } from "@/components/node-sidebar"
import { SearchWorkspace } from "@/components/search-workspace"

type NodeSnapshot = { hosts: Host[]; info: OwnerInfo; status: StatusReport }

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message
  return "the node did not return a readable error"
}

export function App() {
  const [authenticated, setAuthenticated] = useState<boolean | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [pending, setPending] = useState(false)

  useEffect(() => {
    api
      .session()
      .then((session) => setAuthenticated(session.authenticated))
      .catch((reason) => {
        setError(errorMessage(reason))
        setAuthenticated(false)
      })
  }, [])

  async function login(token: string) {
    setPending(true)
    setError(null)
    try {
      await api.login(token)
      setAuthenticated(true)
    } catch (reason) {
      setError(errorMessage(reason))
    } finally {
      setPending(false)
    }
  }

  if (authenticated === null) return <div className="boot-screen">●▬▬●▬▬●</div>
  if (!authenticated)
    return <LoginScreen error={error} pending={pending} onLogin={login} />
  return <OwnerConsole onExpired={() => setAuthenticated(false)} />
}

function OwnerConsole({ onExpired }: { onExpired: () => void }) {
  const [snapshot, setSnapshot] = useState<NodeSnapshot | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [results, setResults] = useState<QueryResult[]>([])
  const [detail, setDetail] = useState<ExpandResponse | null>(null)
  const [fetched, setFetched] = useState<FetchResponse | null>(null)
  const [report, setReport] = useState<IndexReport | null>(null)
  const [pending, setPending] = useState(true)

  const run = useCallback(
    async <T,>(operation: () => Promise<T>): Promise<T | null> => {
      setPending(true)
      setError(null)
      try {
        return await operation()
      } catch (reason) {
        if (reason instanceof ApiError && reason.status === 401) onExpired()
        setError(errorMessage(reason))
        return null
      } finally {
        setPending(false)
      }
    },
    [onExpired]
  )

  useEffect(() => {
    Promise.all([api.info(), api.status(), api.hosts()])
      .then(([info, status, hosts]) => setSnapshot({ info, status, hosts }))
      .catch((reason: unknown) => {
        if (reason instanceof ApiError && reason.status === 401) onExpired()
        setError(errorMessage(reason))
      })
      .finally(() => setPending(false))
  }, [onExpired])

  if (!snapshot) {
    return <div className="boot-screen">{error ?? "reading node state..."}</div>
  }

  return (
    <main className="console-shell">
      <NodeSidebar
        {...snapshot}
        onLogout={() =>
          void api
            .logout()
            .then(onExpired)
            .catch((reason) => {
              setError(errorMessage(reason))
            })
        }
      />
      <div className="console-main">
        {error ? <div className="error-banner">{error}</div> : null}
        <SearchWorkspace
          detail={detail}
          fetched={fetched}
          loading={pending}
          results={results}
          onCloseDetail={() => {
            setDetail(null)
            setFetched(null)
          }}
          onSearch={(text) =>
            void run(() => api.query(text)).then((value) => {
              if (value) setResults(value.results)
            })
          }
          onSelect={(address) =>
            void run(() => api.expand(address)).then((value) => {
              if (value) {
                setDetail(value)
                setFetched(null)
              }
            })
          }
          onFetch={(address) =>
            void run(() => api.fetch(address)).then((value) => {
              if (value) setFetched(value)
            })
          }
        />
        <IndexControl
          hosts={snapshot.hosts}
          roots={snapshot.info.index_roots}
          pending={pending}
          report={report}
          onIndex={(root, host) =>
            void run(() => api.index(root, host)).then((value) => {
              if (!value) return
              setReport(value)
              void api
                .status()
                .then((status) => setSnapshot({ ...snapshot, status }))
                .catch((reason) => setError(errorMessage(reason)))
            })
          }
        />
      </div>
    </main>
  )
}

export default App
