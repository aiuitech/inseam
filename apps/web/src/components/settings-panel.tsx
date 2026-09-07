import { useState } from "react"
import { RotateCcw, Save, SlidersHorizontal } from "lucide-react"

import type { Settings } from "@/api"
import { Button } from "@inseam/brand/components/ui/button"
import { Input } from "@inseam/brand/components/ui/input"

type Props = {
  settings: Settings
  pending: boolean
  notice: { kind: "ok" | "error"; text: string } | null
  onSave: (settings: Settings) => void
}

/** How one config field is edited. The document is typed on the node
 * (`inseam_plugins::settings`); this schema only chooses a control per
 * field and never invents a field the node did not send. */
type Field =
  | { key: string; label: string; kind: "text"; hint?: string }
  | { key: string; label: string; kind: "text_or_null"; hint?: string }
  | { key: string; label: string; kind: "integer"; hint?: string }
  | { key: string; label: string; kind: "integer_or_null"; hint?: string }
  | { key: string; label: string; kind: "number"; hint?: string }
  | { key: string; label: string; kind: "boolean"; hint?: string }
  | {
      key: string
      label: string
      kind: "select"
      options: string[]
      hint?: string
    }
  | { key: string; label: string; kind: "lines"; hint?: string }
  | { key: string; label: string; kind: "json"; hint?: string }

type EntryForm = {
  id: string
  title: string
  fields: Field[]
}

type Group = { title: string; eyebrow: string; entries: EntryForm[] }

const LANE: string[] = ["interactive", "batch"]

/** The first-party entries as the macOS Configuration tab groups them:
 * Connections, Models, Indexing, Search. Toggle-only entries carry no
 * fields; every other field mirrors `docs/configuration.md`. */
const GROUPS: Group[] = [
  {
    title: "connections",
    eyebrow: "what this node reaches",
    entries: [
      { id: "connections", title: "connection registry", fields: [] },
      {
        id: "fs",
        title: "local filesystem",
        fields: [
          {
            key: "roots",
            label: "folders to index",
            kind: "lines",
            hint: "one absolute path per line; empty lets any folder be named per run, otherwise indexing is bounded to these",
          },
          {
            key: "host_id",
            label: "host id",
            kind: "text_or_null",
            hint: "blank: fs-<hostname>",
          },
          { key: "skip_hidden", label: "skip hidden files", kind: "boolean" },
          { key: "gitignore", label: "honor .gitignore", kind: "boolean" },
          {
            key: "ignore",
            label: "ignore patterns",
            kind: "lines",
            hint: "one gitignore-syntax pattern per line",
          },
        ],
      },
      {
        id: "google",
        title: "google workspace",
        fields: [
          { key: "grant", label: "grant", kind: "text" },
          { key: "client_id_env", label: "client id variable", kind: "text" },
          {
            key: "client_secret_env",
            label: "client secret variable",
            kind: "text",
            hint: "blank for none",
          },
          {
            key: "services",
            label: "services",
            kind: "lines",
            hint: "gmail, drive, calendar, contacts, tasks — one per line",
          },
          { key: "sources_max", label: "sources max", kind: "integer" },
        ],
      },
      {
        id: "oauth",
        title: "oauth",
        fields: [
          { key: "callback_port", label: "callback port", kind: "integer" },
          {
            key: "authorization_timeout_secs",
            label: "authorization timeout (s)",
            kind: "integer",
          },
          {
            key: "credentials_dir",
            label: "credentials dir",
            kind: "text_or_null",
            hint: "blank: the node's private oauth/ directory",
          },
          {
            key: "grants",
            label: "generic grants",
            kind: "json",
            hint: "JSON list of grants; connections register their own",
          },
        ],
      },
    ],
  },
  {
    title: "models",
    eyebrow: "language and embedding endpoints",
    entries: [
      {
        id: "llm",
        title: "llm endpoint",
        fields: [
          { key: "base_url", label: "base url", kind: "text" },
          {
            key: "api_key_env",
            label: "api key variable",
            kind: "text",
            hint: "blank for a keyless endpoint",
          },
          { key: "transform_model", label: "transform model", kind: "text" },
          {
            key: "transform_reasoning_effort",
            label: "transform reasoning effort",
            kind: "text_or_null",
            hint: 'blank: unset; "none" for thinking models',
          },
          { key: "agent_model", label: "agent model", kind: "text" },
          {
            key: "batches_url",
            label: "batches url",
            kind: "text_or_null",
            hint: "blank: derived",
          },
          {
            key: "transform_batch_model",
            label: "transform batch model",
            kind: "text_or_null",
            hint: "blank: derived",
          },
          {
            key: "batch_requests_max",
            label: "batch requests max",
            kind: "integer",
          },
        ],
      },
      {
        id: "embedder",
        title: "embedder",
        fields: [
          {
            key: "provider",
            label: "provider",
            kind: "select",
            options: ["endpoint", "hashed", "none"],
          },
          { key: "model", label: "model", kind: "text" },
          {
            key: "dimensions",
            label: "dimensions",
            kind: "integer_or_null",
            hint: "blank: the model's native width",
          },
          {
            key: "vectors",
            label: "vectors",
            kind: "select",
            options: ["all", "summaries"],
          },
        ],
      },
    ],
  },
  {
    title: "indexing",
    eyebrow: "transforms and the sweep",
    entries: [
      { id: "transforms", title: "transform registry", fields: [] },
      { id: "markdown", title: "markdown transform", fields: [] },
      {
        id: "chunker",
        title: "chunker",
        fields: [
          { key: "target_chars", label: "target chars", kind: "integer" },
        ],
      },
      {
        id: "summarizer",
        title: "summarizer",
        fields: [
          { key: "target_chars", label: "target chars", kind: "integer" },
          { key: "llm_call_budget", label: "llm call budget", kind: "integer" },
          { key: "llm_lane", label: "llm lane", kind: "select", options: LANE },
        ],
      },
      {
        id: "entities",
        title: "entities",
        fields: [
          { key: "max_per_source", label: "max per source", kind: "integer" },
          { key: "llm_call_budget", label: "llm call budget", kind: "integer" },
          { key: "llm_lane", label: "llm lane", kind: "select", options: LANE },
        ],
      },
      {
        id: "sweep",
        title: "sweep",
        fields: [
          {
            key: "max_sources",
            label: "max sources",
            kind: "integer",
            hint: "0: unlimited",
          },
          { key: "concurrency", label: "concurrency", kind: "integer" },
          {
            key: "batch_concurrency",
            label: "batch concurrency",
            kind: "integer",
          },
          {
            key: "source_reads_in_flight_max",
            label: "source reads in flight max",
            kind: "integer",
          },
          {
            key: "max_fragments_per_source",
            label: "max fragments per source",
            kind: "integer",
          },
          { key: "max_depth", label: "max depth", kind: "integer" },
          {
            key: "max_content_bytes",
            label: "max content bytes",
            kind: "integer",
          },
          {
            key: "max_reference_hops",
            label: "max reference hops",
            kind: "integer",
          },
          {
            key: "modified_after",
            label: "modified after",
            kind: "text_or_null",
            hint: "YYYY-MM-DD; blank for none",
          },
          {
            key: "ignore",
            label: "ignore rules",
            kind: "json",
            hint: "JSON list of rules over addresses and envelopes",
          },
        ],
      },
    ],
  },
  {
    title: "search",
    eyebrow: "ranking",
    entries: [
      {
        id: "finder",
        title: "finder",
        fields: [
          { key: "seed_k", label: "seed k", kind: "integer" },
          { key: "rrf_k", label: "rrf k", kind: "number" },
          { key: "damping", label: "damping", kind: "number" },
          { key: "iterations", label: "iterations", kind: "integer" },
          { key: "epsilon", label: "epsilon", kind: "number" },
          { key: "max_hints", label: "max hints", kind: "integer" },
          {
            key: "max_vector_distance",
            label: "max vector distance",
            kind: "number",
          },
          {
            key: "graph_hops",
            label: "graph hops",
            kind: "integer",
            hint: "at most 4",
          },
          {
            key: "graph_relation_limit",
            label: "graph relation limit",
            kind: "integer",
          },
          {
            key: "weights",
            label: "weights",
            kind: "json",
            hint: "{ default, by_kind: { <relation kind>: weight } }",
          },
        ],
      },
    ],
  },
]

/** The one entry the running node refuses to disable: it is the service
 * answering this console. */
const ALWAYS_ON = "operations"

type Config = Record<string, unknown>

function configOf(settings: Settings, id: string): Config {
  const entry = settings[id]
  return entry && typeof entry.config === "object" && entry.config !== null
    ? (entry.config as Config)
    : {}
}

/** Text shown for a field's current value. Lists and objects render as
 * lines or JSON so an edit round-trips through `parseField`. */
function renderField(field: Field, value: unknown): string {
  switch (field.kind) {
    case "lines":
      return Array.isArray(value) ? value.map(String).join("\n") : ""
    case "json":
      return JSON.stringify(value ?? null, null, 2)
    case "boolean":
      return value ? "true" : "false"
    default:
      return value === null || value === undefined ? "" : String(value)
  }
}

/** The typed value for what the owner typed, or a problem naming the
 * field. Structural checks only; the node validates meaning. */
function parseField(
  field: Field,
  text: string
): { value: unknown } | { problem: string } {
  const trimmed = text.trim()
  switch (field.kind) {
    case "text":
    case "select":
      return { value: text }
    case "text_or_null":
      return { value: trimmed === "" ? null : text }
    case "integer": {
      const value = Number(trimmed)
      return Number.isInteger(value) && trimmed !== ""
        ? { value }
        : { problem: `${field.label} must be a whole number` }
    }
    case "integer_or_null": {
      if (trimmed === "") return { value: null }
      const value = Number(trimmed)
      return Number.isInteger(value)
        ? { value }
        : { problem: `${field.label} must be a whole number or blank` }
    }
    case "number": {
      const value = Number(trimmed)
      return Number.isFinite(value) && trimmed !== ""
        ? { value }
        : { problem: `${field.label} must be a number` }
    }
    case "boolean":
      return { value: trimmed === "true" }
    case "lines":
      return {
        value: text
          .split("\n")
          .map((line) => line.trim())
          .filter((line) => line !== ""),
      }
    case "json":
      try {
        return { value: JSON.parse(trimmed === "" ? "null" : trimmed) }
      } catch {
        return { problem: `${field.label} must be valid JSON` }
      }
  }
}

function FieldControl({
  field,
  value,
  disabled,
  onChange,
}: {
  field: Field
  value: string
  disabled: boolean
  onChange: (text: string) => void
}) {
  switch (field.kind) {
    case "boolean":
      return (
        <input
          type="checkbox"
          checked={value === "true"}
          disabled={disabled}
          onChange={(event) =>
            onChange(event.target.checked ? "true" : "false")
          }
        />
      )
    case "select":
      return (
        <select
          value={value}
          disabled={disabled}
          onChange={(event) => onChange(event.target.value)}
        >
          {field.options.map((option) => (
            <option value={option} key={option}>
              {option}
            </option>
          ))}
        </select>
      )
    case "lines":
    case "json":
      return (
        <textarea
          value={value}
          disabled={disabled}
          rows={Math.min(8, Math.max(2, value.split("\n").length))}
          spellCheck={false}
          onChange={(event) => onChange(event.target.value)}
        />
      )
    default:
      return (
        <Input
          value={value}
          disabled={disabled}
          inputMode={
            field.kind === "text" || field.kind === "text_or_null"
              ? "text"
              : "decimal"
          }
          onChange={(event) => onChange(event.target.value)}
        />
      )
  }
}

/** Every field's text, keyed `<entry>.<field>`, from a settings document. */
function draftOf(settings: Settings): Record<string, string> {
  const draft: Record<string, string> = {}
  for (const group of GROUPS) {
    for (const entry of group.entries) {
      draft[`${entry.id}.enabled`] = settings[entry.id]?.enabled
        ? "true"
        : "false"
      const config = configOf(settings, entry.id)
      for (const field of entry.fields) {
        draft[`${entry.id}.${field.key}`] = renderField(
          field,
          config[field.key]
        )
      }
    }
  }
  return draft
}

/** The settings document to send: the one the node sent, with every
 * edited field parsed back in — so a field this form does not know stays
 * exactly as the node holds it. */
function documentOf(
  settings: Settings,
  draft: Record<string, string>
): { document: Settings } | { problem: string } {
  const document: Settings = JSON.parse(JSON.stringify(settings)) as Settings
  for (const group of GROUPS) {
    for (const entry of group.entries) {
      const target = document[entry.id]
      if (!target) return { problem: `the node has no \`${entry.id}\` entry` }
      target.enabled = draft[`${entry.id}.enabled`] === "true"
      if (entry.fields.length === 0) continue
      const config: Config = { ...configOf(document, entry.id) }
      for (const field of entry.fields) {
        const parsed = parseField(
          field,
          draft[`${entry.id}.${field.key}`] ?? ""
        )
        if ("problem" in parsed)
          return { problem: `${entry.id}.${parsed.problem}` }
        config[field.key] = parsed.value
      }
      target.config = config
    }
  }
  return { document }
}

/** The node's first-party configuration, editable in place. Save sends
 * the whole document; the node validates it, rewrites its composition
 * overlay, restarts only the entries that changed, and answers with what
 * it runs now — or refuses, naming the field. */
export function SettingsPanel({ settings, pending, notice, onSave }: Props) {
  const [draft, setDraft] = useState<Record<string, string>>(() =>
    draftOf(settings)
  )
  const [problem, setProblem] = useState<string | null>(null)
  // A fresh document from the node (after a save) replaces the draft: the
  // form always starts from what runs. Adjusted during render, the way
  // React resets state that derives from a changed prop.
  const [source, setSource] = useState(settings)
  if (source !== settings) {
    setSource(settings)
    setDraft(draftOf(settings))
    setProblem(null)
  }

  const pristine = draftOf(settings)
  const dirty = Object.keys(draft).some((key) => draft[key] !== pristine[key])

  function edit(key: string, text: string) {
    setDraft((current) => ({ ...current, [key]: text }))
    setProblem(null)
  }

  function save() {
    const built = documentOf(settings, draft)
    if ("problem" in built) {
      setProblem(built.problem)
      return
    }
    onSave(built.document)
  }

  return (
    <section className="settings-panel">
      <div className="settings-heading">
        <div>
          <p className="eyebrow">configuration / composition</p>
          <h2>
            <SlidersHorizontal /> tune this node
          </h2>
        </div>
        <div className="settings-actions">
          <Button
            type="button"
            variant="outline"
            disabled={pending || !dirty}
            onClick={() => {
              setDraft(pristine)
              setProblem(null)
            }}
          >
            <RotateCcw /> revert
          </Button>
          <Button type="button" disabled={pending || !dirty} onClick={save}>
            <Save className={pending ? "animate-spin" : ""} />
            {pending ? "applying" : "apply"}
          </Button>
        </div>
      </div>
      {notice ? (
        <p className={notice.kind === "error" ? "form-error" : "notice-ok"}>
          {notice.text}
        </p>
      ) : null}
      {problem ? <p className="form-error">{problem}</p> : null}
      {GROUPS.map((group) => (
        <details
          className="settings-group"
          key={group.title}
          open={group.title === "models"}
        >
          <summary>
            <strong>{group.title}</strong>
            <span>{group.eyebrow}</span>
          </summary>
          {group.entries.map((entry) => {
            const enabledKey = `${entry.id}.enabled`
            const enabled = draft[enabledKey] === "true"
            return (
              <fieldset className="settings-entry" key={entry.id}>
                <legend>
                  <label>
                    <input
                      type="checkbox"
                      checked={enabled}
                      disabled={pending || entry.id === ALWAYS_ON}
                      onChange={(event) =>
                        edit(
                          enabledKey,
                          event.target.checked ? "true" : "false"
                        )
                      }
                    />
                    <span>{entry.title}</span>
                    <code>{entry.id}</code>
                  </label>
                </legend>
                {entry.fields.length > 0 ? (
                  <div className="settings-fields">
                    {entry.fields.map((field) => {
                      const key = `${entry.id}.${field.key}`
                      return (
                        <label
                          className={`settings-field kind-${field.kind}`}
                          key={key}
                        >
                          <span>{field.label}</span>
                          <FieldControl
                            field={field}
                            value={draft[key] ?? ""}
                            disabled={pending || !enabled}
                            onChange={(text) => edit(key, text)}
                          />
                          {field.hint ? <small>{field.hint}</small> : null}
                        </label>
                      )
                    })}
                  </div>
                ) : null}
              </fieldset>
            )
          })}
        </details>
      ))}
      <p className="settings-footnote">
        Secrets never live here: model and OAuth entries name environment
        variables the node reads at start. Applying rewrites the node's
        composition.toml and restarts only the entries that changed.
      </p>
    </section>
  )
}
