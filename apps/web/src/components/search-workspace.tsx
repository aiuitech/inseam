import { type FormEvent, useState } from "react"
import { ArrowUpRight, FileText, Search, X } from "lucide-react"

import type { ExpandResponse, FetchResponse, QueryResult } from "@/api"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

type Props = {
  detail: ExpandResponse | null
  fetched: FetchResponse | null
  loading: boolean
  results: QueryResult[]
  onCloseDetail: () => void
  onFetch: (address: string) => void
  onSearch: (text: string) => void
  onSelect: (address: string) => void
}

export function SearchWorkspace(props: Props) {
  const [text, setText] = useState("")

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    const query = text.trim()
    if (query.length > 0) props.onSearch(query)
  }

  return (
    <section className="search-workspace">
      <header className="workspace-heading">
        <p className="eyebrow">discovery / local index</p>
        <h1>find a thread worth pulling.</h1>
      </header>
      <form className="search-box" onSubmit={submit}>
        <Search aria-hidden="true" />
        <Input
          value={text}
          onChange={(event) => setText(event.target.value)}
          placeholder="contracts mentioning renewal terms"
          aria-label="Search your sources"
        />
        <Button
          type="submit"
          disabled={props.loading || text.trim().length === 0}
        >
          {props.loading ? "searching" : "search"}
        </Button>
      </form>
      <div className="result-count">
        <span>{props.results.length.toString().padStart(2, "0")} results</span>
        <span>ranked by this node</span>
      </div>
      <div className="result-list">
        {props.results.map((result, index) => (
          <ResultRow
            key={result.address}
            index={index}
            result={result}
            onSelect={props.onSelect}
          />
        ))}
        {!props.loading && props.results.length === 0 ? (
          <div className="empty-results">
            <span aria-hidden="true">●▬▬●▬▬●</span>
            <p>Ask the index what you need. Results stay on this node.</p>
          </div>
        ) : null}
      </div>
      {props.detail ? <DetailPanel {...props} detail={props.detail} /> : null}
    </section>
  )
}

function ResultRow({
  index,
  result,
  onSelect,
}: {
  index: number
  result: QueryResult
  onSelect: (address: string) => void
}) {
  return (
    <button className="result-row" onClick={() => onSelect(result.address)}>
      <span className="result-rank">{String(index + 1).padStart(2, "0")}</span>
      <span className="result-copy">
        <strong>
          {result.envelope.title ?? result.address.split("/").at(-1)}
        </strong>
        <span>
          {result.summary ?? result.hints[0]?.text ?? "No summary indexed."}
        </span>
        <small>{result.address}</small>
      </span>
      <span className="result-meta">
        <Badge variant="outline">{result.envelope.content_type}</Badge>
        <span>{result.score.toFixed(3)}</span>
        <ArrowUpRight aria-hidden="true" />
      </span>
    </button>
  )
}

function DetailPanel({
  detail,
  fetched,
  loading,
  onCloseDetail,
  onFetch,
}: Props & {
  detail: ExpandResponse
}) {
  return (
    <aside className="detail-panel">
      <header>
        <div>
          <p className="eyebrow">source / expanded</p>
          <h2>{detail.address.split("/").at(-1)}</h2>
        </div>
        <Button variant="ghost" size="icon" onClick={onCloseDetail}>
          <X />
          <span className="sr-only">Close source detail</span>
        </Button>
      </header>
      <p className="detail-address">{detail.address}</p>
      <p className="detail-summary">
        {detail.summary ?? "No source summary is indexed."}
      </p>
      <div className="fragment-list">
        {detail.fragments.slice(0, 8).map((fragment) => (
          <article key={fragment.id}>
            <span>{fragment.extent ?? `fragment ${fragment.id}`}</span>
            <Badge variant="outline">{fragment.mimetype}</Badge>
            {fragment.text ? <p>{fragment.text}</p> : null}
          </article>
        ))}
      </div>
      {fetched ? (
        <pre className="fetched-text">{fetched.text}</pre>
      ) : (
        <Button onClick={() => onFetch(detail.address)} disabled={loading}>
          <FileText /> fetch full source
        </Button>
      )}
    </aside>
  )
}
