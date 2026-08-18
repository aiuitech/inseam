# macOS App

The SwiftUI shell in `apps/macos` embeds the node core through `crates/inseam-ffi`. It proves the integration end to end: open the node, index a chosen folder, and query — all business logic in Rust, only UI in Swift.

## The FFI boundary (`crates/inseam-ffi`)

A C ABI static library (`libinseam_ffi.a`) with a hand-maintained header at `crates/inseam-ffi/include/inseam_ffi.h` (keep header and `src/lib.rs` in sync). Surface:

| Function | Does |
| --- | --- |
| `inseam_version` | Core version string |
| `inseam_node_open` | Open the node under a data dir; optional composition path, otherwise `<data_dir>/composition.toml` layered over the built-in base. Opens even when entries are parked (a missing API key, say) — operation calls then error with the explanation |
| `inseam_node_health` | Per-entry fiber health JSON: `{id, plugin, state, error, missing}` per composition entry — "what is parked and why" |
| `inseam_node_query` | Finder query → `QueryResponse` JSON |
| `inseam_node_index_dir` | Index a local directory → `IndexReport` JSON |
| `inseam_node_free` / `inseam_string_free` | Release handles/strings the library allocated |

Conventions: calls that can fail take `char **error_out` (null return + owned message on failure); every returned string is freed with `inseam_string_free`. The handle owns a booted kernel and its tokio runtime, so calls block — app shells run them off the main thread. Responses are the same serde types `ops` serializes everywhere else.

## The Swift side (`apps/macos`)

A SwiftPM package, no Xcode project:

- `Sources/CInseamFFI/module.modulemap` — system-library target exposing the FFI header and linking `inseam_ffi`.
- `Sources/Inseam/InseamCore.swift` — `CoreNode`: RAII wrapper over the handle (with explicit `close()` so Settings can reopen), JSON decoding into Swift structs (snake_case converted).
- `Sources/Inseam/ContentView.swift` — the UI: open node on launch (data dir `~/Library/Application Support/inseam`, shared with the CLI), Index Folder… button, query field, results list.
- `Sources/Inseam/SettingsView.swift` — the Settings scene (⌘,): a Composition tab and a Secrets tab.
- `Sources/Inseam/Secrets.swift` — `SecretStore`, the Keychain wrapper behind the Secrets tab.

## Configuration and secrets

Settings is file-first, like VS Code: the Composition tab is a plain TOML editor over `<data-dir>/composition.toml` — the same file the CLI layers, documented in [configuration.md](../configuration.md) — so hand edits and GUI edits are the same thing. Saving writes the file and reopens the node.

Secrets follow the composition design ([design/composition.md](../../design/composition.md)): the file never holds them; plugin configs name environment variables (`api_key_env`-style). The CLI gets those variables from your shell, but a GUI app launched from Finder has no shell environment — so the Secrets tab stores name→value pairs in your login Keychain (service `app.inseam.secrets`), and the app exports each one with `setenv` just before every node open. Adding or removing a secret reopens the node so plugins re-resolve their keys.

A missing secret is not fatal: the entry that needs it fails contained, everything depending on it parks pending, and the node still opens. The main window reads `inseam_node_health` and lists what's parked and why (for example `llm: environment variable OPENROUTER_API_KEY is not set`), pointing at Settings; entering the key reopens the node and the parked entries activate. With the stock composition the llm entry gates more than summaries — the default embedder embeds through the llm endpoint, so finder, sweep, and operations park with it until the key arrives or the composition switches the embedder off `endpoint` mode.

## Building

```sh
apps/macos/build.sh            # → /Applications/Inseam.app
apps/macos/build.sh /tmp/out   # → any other destination dir
```

The script builds `inseam-ffi` in release, runs `swift build -c release` with `-L target/release`, assembles the `.app` bundle from `apps/macos/Info.plist` (including the app icon from `assets/icon-macos.icns` — see `docs/assets.md`), ad-hoc codesigns, and copies to the destination — default `/Applications`, so the finished app is directly launchable. Requires only the Xcode Command Line Tools (`swift`).

Known cosmetic warning: the Rust objects target the host macOS version while the app declares `LSMinimumSystemVersion` 14.0, so `ld` prints version-mismatch warnings. Harmless for local builds; set `MACOSX_DEPLOYMENT_TARGET=14.0` when building the Rust side for distribution.
