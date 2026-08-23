# macOS App

The SwiftUI shell in `apps/macos` embeds the node core through `crates/inseam-ffi`. It proves the integration end to end: open the node, index a chosen folder, and query — all business logic in Rust, only UI in Swift.

## The FFI boundary (`crates/inseam-ffi`)

A C ABI static library (`libinseam_ffi.a`) with a hand-maintained header at `crates/inseam-ffi/include/inseam_ffi.h` (keep header and `src/lib.rs` in sync). Surface:

| Function | Does |
| --- | --- |
| `inseam_version` | Core version string |
| `inseam_settings_read` | Effective first-party settings as JSON, with plugin defaults filled in |
| `inseam_settings_write` | Validate settings JSON and atomically update the composition while retaining custom entries |
| `inseam_node_open` | Open the node under a data dir; optional composition path, otherwise `<data_dir>/composition.toml` layered over the built-in base. Opens even when entries are parked (a missing API key, say) — operation calls then error with the explanation |
| `inseam_node_health` | Per-entry fiber health JSON: `{id, plugin, state, error, missing, missing_secrets}` per composition entry — "what is parked and why", with each missing secret's variable name and owner-facing purpose |
| `inseam_node_query` | Finder query → `QueryResponse` JSON |
| `inseam_node_index_dir` | Index a local directory → `IndexReport` JSON |
| `inseam_node_hosts` | The hosts the node stewards → `HostView[]` JSON |
| `inseam_node_grants` | The OAuth grants the node holds → `GrantView[]` JSON |
| `inseam_node_authorize_begin` / `inseam_node_authorize_await` | Start a loopback authorization (returns the provider URL the app opens) and block until the browser comes back |
| `inseam_node_revoke_grant` | Forget a grant's tokens |
| `inseam_node_plugins` | Every composition entry as the kernel runs it → `PluginView[]` JSON (the `plugins` owner operation) |
| `inseam_node_install_plugin` | Mount a loaded plugin into the open node from an `InstallPluginRequest` JSON (`{id, files: [{path, bytes(base64)}], config?}`) → the new entry's `PluginView` JSON. Files land under `<data-dir>/plugins/<id>/`, the entry is appended to `composition.toml`, the kernel reconciles in place — no reopen; a failed mount is rolled back and named |
| `inseam_node_free` / `inseam_string_free` | Release handles/strings the library allocated |

Conventions: calls that can fail take `char **error_out` (null return + owned message on failure); every returned string is freed with `inseam_string_free`. The handle owns a booted kernel and its tokio runtime, so calls block — app shells run them off the main thread. Responses are the same serde types `ops` serializes everywhere else.

The handle is a distribution like the CLI: it boots the WASM plugin host, so `wasm:` entries in `composition.toml` mount, and it is what applies composition edits ([../../design/composition.md](../../design/composition.md)) — `inseam_node_install_plugin` runs the operation on the runtime and services the `composition` edit channel until it returns, the same loop `inseam serve` runs beside its transport. Overlapping calls on one handle serialize on the kernel rather than racing.

## The Swift side (`apps/macos`)

A SwiftPM package, no Xcode project:

- `Sources/CInseamFFI/module.modulemap` — system-library target exposing the FFI header and linking `inseam_ffi`.
- `Sources/Inseam/InseamCore.swift` — `CoreNode`: RAII wrapper over the handle (with explicit `close()` so Settings can reopen), JSON decoding into Swift structs (snake_case converted).
- `Sources/Inseam/ContentView.swift` — the UI: open node on launch (data dir `~/Library/Application Support/inseam`, shared with the CLI), Index Folder… button, query field, results list.
- `Sources/Inseam/Configuration.swift` — Codable mirrors of the first-party plugin config types used by the settings bridge.
- `Sources/Inseam/SettingsView.swift` — the Settings scene (⌘,): visual Configuration, Secrets, and Advanced tabs.
- `Sources/Inseam/Secrets.swift` — `SecretStore`, the Keychain wrapper behind the Secrets tab.

## Plugins

A loaded plugin is installed from the app the way the web console does it: choose the plugin's directory (`<name>.wasm`, its manifest and checks, any fixtures), confirm the entry id, and the node mounts it in place through `inseam_node_install_plugin`; `inseam_node_plugins` lists what runs. Unmounting is editing `composition.toml` (the Advanced tab); upgrading is remove-then-install.

## Configuration and secrets

The Configuration tab is the ordinary editing path. It groups the first-party entries into Connections, Models, Indexing, and Search, with native controls for every field and enable switch. Connections includes the local filesystem, the Google Workspace connection — its entry config plus, live from the open node, where its grant stands and the one button that fits (**Add Client ID…** steering to Secrets, **Connect Google…** which opens the browser and waits off the main thread, **Disconnect**), and the hosts it stewards — and generic OAuth grants. The Rust bridge reads the effective composition through the real plugin config types, fills in their defaults, validates edits, and atomically writes complete config tables back to `<data-dir>/composition.toml`. This matters because a config table replaces the base entry's table wholesale rather than merging field by field.

Form saves retain custom and loaded-plugin entries. The serializer normalizes the TOML and does not retain comments. The Advanced tab keeps a raw editor for custom plugin fields and hand-authored structure the first-party form cannot represent. A malformed file prevents the visual form from loading and points the user to Advanced. Saving either editor reopens the node.

Secrets follow the composition design ([design/composition.md](../../design/composition.md)): the file never holds them; plugin configs name environment variables (`api_key_env`-style). The CLI gets those variables from your shell, but a GUI app launched from Finder has no shell environment — so the Secrets tab stores name→value pairs in your login Keychain (service `app.inseam.secrets`), and the app exports each one with `setenv` just before every node open. Adding or removing a secret reopens the node so plugins re-resolve their keys.

Before showing a missing-secret prompt, startup also checks for one unambiguous generic-password item whose Keychain service exactly matches each plugin-declared environment variable, such as `OPENROUTER_API_KEY`, and retries the node once when it finds one. The lookup is capped at 64 declared names, never enumerates the general Keychain, and disables authentication UI. An inaccessible item is treated as absent instead of producing a run of permission dialogs.

A missing secret is not fatal: the entry that needs it fails contained, everything depending on it parks pending, and the node still opens. Plugins declare their secret needs with a reason (`Plugin::secrets()`, [../plugins/linked.md](../plugins/linked.md)), so instead of raw errors the main window shows the variable name, the plugin's own plain-language justification, an "Add API Key…" button that opens the Secrets tab pre-filled, and a quiet line naming what stays paused meanwhile. Entering the key reopens the node and the parked entries activate. Parked entries with no declared secret fall back to the raw per-entry error lines. With the stock composition the llm entry gates more than summaries — the default embedder embeds through the llm endpoint, so finder, sweep, and operations park with it until the key arrives or the composition switches the embedder off `endpoint` mode.

## Building

```sh
apps/macos/build.sh            # → /Applications/Inseam.app
apps/macos/build.sh /tmp/out   # → any other destination dir
```

The script builds `inseam-ffi` in release, runs `swift build -c release` with `-L target/release`, assembles the `.app` bundle from `apps/macos/Info.plist` (including the app icon from `assets/icon-macos.icns` — see `docs/assets.md`), ad-hoc codesigns, and copies to the destination — default `/Applications`, so the finished app is directly launchable. Requires only the Xcode Command Line Tools (`swift`).

Known cosmetic warning: the Rust objects target the host macOS version while the app declares `LSMinimumSystemVersion` 14.0, so `ld` prints version-mismatch warnings. Harmless for local builds; set `MACOSX_DEPLOYMENT_TARGET=14.0` when building the Rust side for distribution.
