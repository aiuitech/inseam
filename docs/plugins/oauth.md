# OAuth Grants

Many remote hosts are reached the same way: an OAuth 2.0 authorization-code grant that yields a refreshable token. The `oauth` seam holds that once — configure a **grant** per provider account, authorize it in the browser, and any connection plugin (linked or loaded) consumes it by id for live access tokens ([design/connections.md](../../design/connections.md)). The linked `oauth` plugin is the provider; it is in the base composition with no grants, so adding one is a config patch.

## Configure a grant

```toml
[[entry]]
id = "oauth"
[entry.config]
# callback_port = 47781          # the loopback redirect: http://127.0.0.1:<port>/callback
# authorization_timeout_secs = 300
# credentials_dir = "…"          # default <data-dir>/oauth

[[entry.config.grants]]
id = "google"                                           # what consumers name: grant = "google"
authorization_url = "https://accounts.google.com/o/oauth2/v2/auth"
token_url = "https://oauth2.googleapis.com/token"
scopes = ["https://www.googleapis.com/auth/gmail.readonly"]
client_id_env = "GOOGLE_CLIENT_ID"                      # the app identity, from your environment
client_secret_env = "GOOGLE_CLIENT_SECRET"              # omit for public (PKCE-only) clients
[entry.config.grants.authorization_params]              # provider extras; Google needs these for a refresh token
access_type = "offline"
prompt = "consent"
```

Register `http://127.0.0.1:47781/callback` as a redirect URI when you create the client in the provider's console. `callback_port = 0` lets the OS pick a port per attempt — only for providers that accept any loopback port.

The client id and secret are environment variables, like every secret the node reads; a grant whose variables are unset shows as `missing secret` in `inseam grants` and the other grants keep working. `inseam plugins` lists the variables with the reason each is needed.

## Authorize

```sh
inseam grants              # each grant: missing secret / unauthorized / authorized (token until …, scopes)
inseam authorize google    # prints the sign-in URL; waits for the browser to land on the loopback port
```

The flow is RFC 6749 authorization code with PKCE (S256) over a loopback redirect (RFC 8252): a random `state` guards against forged redirects, the code is exchanged at `token_url`, and the tokens land in `<data-dir>/oauth/<grant>.json` (directory 0700, file 0600). Access tokens are refreshed a minute before they expire; a rotated refresh token replaces the old one in place. Nothing about a grant ever enters the store or the composition.

## Consume a grant from a plugin

A host connection declares `Inject::required("oauth")`, names the grant in its config, and at apply:

```rust
let oauth = cx.get(&OAUTH)?;
let grant = oauth.grant(&config.grant).ok_or_else(|| PluginError(format!("no grant `{}`", config.grant)))?;
require_scopes(grant.as_ref(), &["https://www.googleapis.com/auth/gmail.readonly"])
    .map_err(|e| PluginError(e.to_string()))?;            // a scope gap is named now, not as a 403 mid-sweep
// later, per request:
let token = grant.access_token().await?;                  // refreshed if needed; Unauthorized until `inseam authorize`
request.header("Authorization", token.authorization_header())
```

`GrantState` (`missing_secret` / `unauthorized` / `authorized`) is what a connection reports while it cannot work; `revoke()` forgets the stored tokens.
