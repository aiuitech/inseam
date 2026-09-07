import { type FormEvent, useState } from "react"
import { ArrowRight, LockKeyhole } from "lucide-react"

import { Button } from "@inseam/brand/components/ui/button"
import { Input } from "@inseam/brand/components/ui/input"

type LoginScreenProps = {
  error: string | null
  pending: boolean
  onLogin: (token: string) => Promise<void>
}

export function LoginScreen({ error, pending, onLogin }: LoginScreenProps) {
  const [token, setToken] = useState("")

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault()
    void onLogin(token)
  }

  return (
    <main className="login-shell">
      <div className="login-stitch" aria-hidden="true">
        ●▬▬●▬▬●
      </div>
      <section className="login-panel">
        <p className="eyebrow">owner channel / private</p>
        <h1>
          enter the node
          <span className="cursor-block" aria-hidden="true" />
        </h1>
        <p className="login-copy">
          This console can search, fetch, and index every host stewarded by this
          node. Use the owner token configured on the server.
        </p>
        <form onSubmit={submit} className="login-form">
          <label htmlFor="owner-token">owner token</label>
          <div className="token-row">
            <LockKeyhole aria-hidden="true" />
            <Input
              id="owner-token"
              type="password"
              autoComplete="current-password"
              value={token}
              onChange={(event) => setToken(event.target.value)}
              minLength={32}
              required
              autoFocus
            />
          </div>
          {error ? <p className="form-error">{error}</p> : null}
          <Button type="submit" disabled={pending || token.length < 32}>
            {pending ? "opening..." : "open console"}
            <ArrowRight aria-hidden="true" />
          </Button>
        </form>
      </section>
      <p className="login-footnote">signed session / expires in 12 hours</p>
    </main>
  )
}
