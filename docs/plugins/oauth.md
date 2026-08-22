# OAuth Grants

Many remote hosts are reached the same way: an OAuth 2.0 authorization-code grant that yields a refreshable token. The `oauth` seam holds that once — a **grant** per provider account, authorized in the browser, consumed by any connection plugin (linked or loaded) for live access tokens ([design/connections.md](../../design/connections.md)). The linked `oauth` plugin is the provider; it is in the base composition with no grants of its own.

## Two ways a grant arrives

- **A connection registers it.** A connection plugin that knows its provider brings the grant with it: the [Google Workspace connection](../indexing/google-workspace.md) registers `google` — Google's endpoints, the read-only scopes for the services it stewards, the OpenID scopes that name the account — and you supply only the client identity through the environment variables its entry names. Nothing to configure on the `oauth` entry.
- **You configure it.** For a provider no connection knows yet, define a generic grant on the `oauth` entry:

```toml
[[entry]]
id = "oauth"
[entry.config]
# callback_port = 47781          # the loopback redirect: http://127.0.0.1:<port>/callback
# authorization_timeout_secs = 300
# credentials_dir = "…"          # default <data-dir>/oauth

[[entry.config.grants]]
id = "slack"                                            # what consumers name: grant = "slack"
authorization_url = "https://slack.com/oauth/v2/authorize"
token_url = "https://slack.com/api/oauth.v2.access"
scopes = ["channels:history"]
client_id_env = "SLACK_CLIENT_ID"                       # the app identity, from your environment
client_secret_env = "SLACK_CLIENT_SECRET"               # omit for public (PKCE-only) clients
[entry.config.grants.authorization_params]              # provider extras, if any
```

Either way a grant is one record — id, endpoints, scopes, client variables — and one grant per id: a plugin cannot register an id the `oauth` entry already configures.

The client id and secret are environment variables, like every secret the node reads; a grant whose variables are unset shows as `missing secret` in `inseam grants` and the other grants keep working. A client identifies the *app*, not the account — the tokens stay on your node; today every grant is bring-your-own, and a bundled inseam client for the Google grant is planned for when inseam is commercialized ([../indexing/google-workspace.md](../indexing/google-workspace.md)). `inseam plugins` lists the variables with the reason each is needed; the macOS app's Secrets tab stores them in the Keychain.

## Authorize — from any client

The flow is RFC 6749 authorization code with PKCE (S256): a random `state` guards against forged redirects, the code is exchanged at `token_url`, and the tokens land in `<data-dir>/oauth/<grant>.json` (directory 0700, file 0600). What differs per client is only where the browser comes back:

| Client | Redirect | How |
| --- | --- | --- |
| CLI | loopback (RFC 8252) | `inseam authorize <grant>` prints the sign-in URL and waits on `127.0.0.1:<callback_port>/callback` |
| macOS app | loopback | Settings → Connections → **Connect Google…** opens the browser and waits off the main thread |
| web console | the node's own URL | **connect** sends the tab to the provider, which returns it to `<public url>/api/v1/oauth/callback`; the node exchanges the code and sends the tab back to the console ([../architecture/hosted-node.md](../architecture/hosted-node.md)) |

Register both redirect URIs on the OAuth client — the loopback one and, for a node you authorize from the web, its callback URL (`inseam serve` prints it in `/api/v1/owner/info`, and the console shows it). `callback_port = 0` lets the OS pick a port per attempt — only for providers that accept any loopback port.

```sh
inseam grants              # each grant: missing secret / unauthorized / authorized as <account> (token until …, scopes)
inseam authorize google    # prints the sign-in URL; waits for the browser to land on the loopback port
inseam revoke google       # forget the tokens; hosts behind the grant withdraw
```

Access tokens are refreshed a minute before they expire; a rotated refresh token replaces the old one in place. When the provider returns an OpenID `id_token` the signed-in account (its `email`) is kept with the tokens — that is what a connection derives its host ids from, so nothing about a grant depends on a further API call. Nothing about a grant ever enters the store or the composition.

## What happens when a grant changes

Authorizing or revoking a grant fires a `GrantChanged` event on the node's bus. Connections behind the grant listen: the Google connection registers its hosts the moment the browser comes back and withdraws them on revoke, without a restart — `inseam serve` and the macOS app see new hosts live.

## Consume a grant from a plugin

A connection that knows its provider registers the grant and keeps the handle:

```rust
let grant = inseam_seams::oauth::register_as_effect(cx, spec).await?;   // spec: GrantSpec — endpoints, scopes, client envs
```

A connection consuming a grant configured elsewhere declares `Inject::required("oauth")`, names the grant in its config, and at apply:

```rust
let oauth = cx.get(&OAUTH)?;
let grant = oauth.grant(&config.grant).ok_or_else(|| PluginError(format!("no grant `{}`", config.grant)))?;
require_scopes(grant.as_ref(), &["https://www.googleapis.com/auth/gmail.readonly"])
    .map_err(|e| PluginError(e.to_string()))?;            // a scope gap is named now, not as a 403 mid-sweep
```

Either way, per request: `let token = grant.access_token().await?;` (refreshed if needed; `Unauthorized` until the owner authorizes) and `request.header("Authorization", token.authorization_header())`. `grant.state()` is `missing_secret` / `unauthorized` / `authorized { account, expires_at, scopes }`; `grant.revoke()` forgets the stored tokens. Subscribe to `GrantChanged` on `cx.bus()` to register hosts when the grant becomes usable.

## Owner operations

The `operations` seam carries the owner's side so every transport is the same thin skin: `grants`, `authorize_grant { grant, redirect: loopback | external { redirect_uri } }`, `await_authorization { state }`, `complete_authorization { state, code, error, … }`, `revoke_grant { grant }` ([../finder/operations.md](../finder/operations.md)).
