# Google Workspace

Gmail envelopes carry facets ([vocabulary.md](vocabulary.md)): `author` from the `From` header — the display name when there is one, else the address — and one `label` per Gmail label, so a message's author becomes an entity row related `authored` from its root and each label a facet row a query can filter on (`inseam query … --facet label:INBOX`).

The `google` entry (plugin `connection-google`, in the base composition) connects one Google account and stewards each of its services as its own host: **Gmail**, **Google Drive**, **Google Calendar**, **Google Contacts**, **Google Tasks** — read-only. One sign-in covers all of them: the entry registers a single OAuth grant whose scopes are the union of the enabled services' read-only scopes plus the OpenID scopes that name the account ([../plugins/oauth.md](../plugins/oauth.md)).

## Set up once

Today you bring your own OAuth client — the one-time console step below. An OAuth client is the *app's* identity with Google, not yours: the tokens it yields live on your node and never reach whoever created the client, and authorizing gives that creator nothing in return. When inseam is commercialized a verified inseam client will ship bundled and be used when you have set none, so this step disappears for the CLI and the macOS app; a node you authorize from the web console will still need its own client, because Google requires that node's exact callback URL to be registered. The reasoning — Google's verification requirements for the Gmail and Drive scopes, and the blast radius of one shared client — is in [design/connections.md](../../design/connections.md).

1. In the [Google Cloud console](https://console.cloud.google.com/apis/credentials), create an OAuth client and enable the APIs you want: Gmail, Drive, Calendar, People, Tasks. Choose the type by how you will sign in: **Desktop app** if only from the CLI or the macOS app (any loopback port is accepted, nothing to register); **Web application** if also from the web console, with both redirect URIs registered — `http://127.0.0.1:47781/callback` and your node's `https://<your node>/api/v1/oauth/callback`. Add your account as a test user while the app is unverified, and publish the app to production once it works — a client left in *Testing* status expires its refresh tokens after seven days, which looks like the connection dropping weekly.
2. Give the node the client identity — environment variables, like every secret: `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET` in your shell for the CLI; the Secrets tab of the macOS app (it offers **Add Client ID…** when they are missing); the container environment for a hosted node.
3. Connect, from whichever client is at hand:

```sh
inseam grants                 # google   accounts.google.com   unauthorized — run `inseam authorize google`
inseam authorize google       # sign in; the browser lands on the loopback port
inseam hosts                  # the Google hosts appear, one per service
```

The macOS app: Settings → Connections → Google Workspace → **Connect Google…**. The web console: **connect** in the Connections panel. The grant is the same one, held once in `<data-dir>/oauth/google.json`, so connecting from any client connects them all.

## The hosts

Once the grant is authorized the entry registers a host per enabled service, each with an id derived from the signed-in account — `gmail-3f9a…`, `google-drive-…` — so two nodes connecting the same account mint the same ids ([../../design/addressing.md](../../design/addressing.md)). `inseam hosts` lists them with the kind and display name (`Gmail · you@example.com`). Revoking the grant (`inseam revoke google`, **Disconnect**, **disconnect**) withdraws them; authorizing again brings them back — no restart, the connection follows the grant.

| Kind | Locator | Scope of `inseam index --host <id> <root>` | Served as |
| --- | --- | --- | --- |
| `gmail` | message id | a Gmail search (`label:INBOX`, `newer_than:30d`); empty for all mail | headers + body text (`text/plain`) |
| `google-drive` | file id | a folder id, swept with its subfolders; empty for every file | the file's bytes; Docs/Sheets/Slides exported as `text/plain` / `text/csv` |
| `google-calendar` | `<calendar>/<event>` | a calendar id (`primary`); empty for every calendar | the event as text |
| `google-contacts` | person id | empty (the account's contacts) | the contact card as text |
| `google-tasks` | `<list>/<task>` | a task-list id; empty for every list | the task as text |

```sh
inseam index --host gmail-3f9a… "newer_than:90d"
inseam index --host google-drive-… ""
inseam query "renewal terms" ; inseam fetch inseam://gmail-3f9a…/18f2ab…
```

An empty scope reconciles deletions for the whole host; a Gmail search or a Drive folder is not a locator prefix, so those scopes add and update but never remove.

## Config

```toml
[[entry]]
id = "google"
[entry.config]
grant = "google"                          # the grant it registers; also the credential file's name
client_id_env = "GOOGLE_CLIENT_ID"
client_secret_env = "GOOGLE_CLIENT_SECRET"  # "" for a client without a secret
services = ["gmail", "drive", "calendar", "contacts", "tasks"]
sources_max = 5000                        # sources one enumeration collects per service per run
```

A second Google account is a second entry with its own `grant` id and its own client variables. Two entries for the *same* account collide on host ids and the second fails alone, named. Changing `services` changes the scopes: authorize again.

## Bounds and behavior

Enumeration walks at most 200 pages per list and `sources_max` sources per service per run; Gmail costs one metadata call per message (eight in flight). Drive folder scopes descend at most 512 folders. Every call is read-only; change feeds (Gmail history, Drive changes) are declared capabilities the sweep does not schedule yet ([maintenance.md](maintenance.md)). A rejected token is an `unauthorized` error naming the grant; a refused call quotes Google; a rate limit is `unavailable`.
