import { type FormEvent, useRef, useState } from "react"
import { Package, Upload } from "lucide-react"

import type { Plugin, PluginFile } from "@/api"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

type Props = {
  plugins: Plugin[]
  pending: boolean
  notice: { kind: "ok" | "error"; text: string } | null
  onInstall: (id: string, files: PluginFile[]) => void
}

/** Most files one upload may carry; the node enforces the same bound. */
const FILES_MAX = 64

function stateLabel(plugin: Plugin): string {
  switch (plugin.state.state) {
    case "active":
      return "active"
    case "pending":
      return plugin.missing.length > 0
        ? `waiting for ${plugin.missing.join(", ")}`
        : "pending"
    case "failed":
      return `failed: ${plugin.state.reason}`
  }
}

/** A chosen file's path inside the plugin directory: the browser's
 * directory-relative path minus the directory itself, else the bare name. */
function relativePath(file: File): string {
  const relative = file.webkitRelativePath
  if (!relative) return file.name
  const parts = relative.split("/")
  return parts.length > 1 ? parts.slice(1).join("/") : relative
}

/** The stem of the one `.wasm` among the chosen files — the id to offer. */
function suggestedId(files: File[]): string {
  const artifact = files.find((file) => file.name.endsWith(".wasm"))
  if (!artifact) return ""
  return artifact.name.slice(0, -".wasm".length).toLowerCase()
}

function readAsBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onerror = () => reject(reader.error)
    reader.onload = () => {
      const url = String(reader.result)
      resolve(url.slice(url.indexOf(",") + 1))
    }
    reader.readAsDataURL(file)
  })
}

/** The entries this node runs and an upload form that mounts a loaded
 * plugin into it without a restart: pick the plugin's directory (artifact,
 * manifest, checks, fixtures), confirm the entry id, install. */
export function PluginsPanel({ plugins, pending, notice, onInstall }: Props) {
  const [files, setFiles] = useState<File[]>([])
  const [id, setId] = useState("")
  const [reading, setReading] = useState(false)
  const [problem, setProblem] = useState<string | null>(null)
  const picker = useRef<HTMLInputElement>(null)

  function choose(list: FileList | null) {
    const chosen = list ? Array.from(list) : []
    setFiles(chosen)
    setId(suggestedId(chosen))
    setProblem(
      chosen.length > FILES_MAX
        ? `choose at most ${FILES_MAX} files`
        : chosen.length > 0 && !suggestedId(chosen)
          ? "the directory has no .wasm artifact"
          : null
    )
  }

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    if (problem || files.length === 0 || !id) return
    setReading(true)
    try {
      const encoded: PluginFile[] = []
      for (const file of files) {
        encoded.push({ path: relativePath(file), bytes: await readAsBase64(file) })
      }
      onInstall(id, encoded)
      setFiles([])
      setId("")
      if (picker.current) picker.current.value = ""
    } catch (reason) {
      setProblem(reason instanceof Error ? reason.message : "could not read the files")
    } finally {
      setReading(false)
    }
  }

  const loaded = plugins.filter((plugin) => plugin.plugin.startsWith("wasm:"))
  const linked = plugins.filter((plugin) => !plugin.plugin.startsWith("wasm:"))
  const busy = pending || reading

  return (
    <section className="plugins-panel">
      <div>
        <p className="eyebrow">plugins / loaded tier</p>
        <h2>extend this node</h2>
      </div>
      {notice ? (
        <p className={notice.kind === "error" ? "form-error" : "notice-ok"}>
          {notice.text}
        </p>
      ) : null}
      <form className="plugin-upload" onSubmit={(event) => void submit(event)}>
        <label>
          plugin directory
          <input
            ref={picker}
            type="file"
            multiple
            // Non-standard but universal: pick the whole plugin directory so
            // the manifest, checks and fixtures ride along with the artifact.
            {...{ webkitdirectory: "", directory: "" }}
            onChange={(event) => choose(event.target.files)}
            disabled={busy}
          />
        </label>
        <label>
          entry id
          <Input
            value={id}
            onChange={(event) => setId(event.target.value)}
            placeholder="ocr"
            pattern="[a-z0-9][a-z0-9_-]*"
            maxLength={64}
            disabled={busy}
          />
        </label>
        <Button
          type="submit"
          disabled={busy || files.length === 0 || !id || problem !== null}
        >
          <Upload className={reading ? "animate-spin" : ""} />
          {reading ? "reading" : pending ? "installing" : "install"}
        </Button>
        <p className="plugin-footnote">
          {files.length > 0
            ? `${files.length} file${files.length === 1 ? "" : "s"} chosen`
            : "the directory holds <name>.wasm, its manifest and checks, and any fixtures"}
        </p>
        {problem ? <p className="form-error">{problem}</p> : null}
      </form>
      {loaded.length === 0 ? (
        <p className="index-empty">No loaded plugins yet.</p>
      ) : null}
      {loaded.map((plugin) => (
        <div className="grant-row" key={plugin.id}>
          <div className="grant-copy">
            <strong>{plugin.id}</strong>
            <span>{plugin.plugin}</span>
            <small>{stateLabel(plugin)}</small>
          </div>
          <Badge variant="outline">
            <Package /> loaded
          </Badge>
        </div>
      ))}
      <details className="plugin-linked">
        <summary>{linked.length} linked entries</summary>
        {linked.map((plugin) => (
          <div className="plugin-linked-row" key={plugin.id}>
            <strong>{plugin.id}</strong>
            <span>{plugin.plugin}</span>
            <small>{stateLabel(plugin)}</small>
          </div>
        ))}
      </details>
    </section>
  )
}
