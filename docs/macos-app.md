# macOS App

The SwiftUI shell in `apps/macos` embeds the node core through `crates/inseam-ffi`. It proves the integration end to end: open the node, index a chosen folder, and query — all business logic in Rust, only UI in Swift.

## The FFI boundary (`crates/inseam-ffi`)

A C ABI staticlib (`libinseam_ffi.a`) with a hand-maintained header at `crates/inseam-ffi/include/inseam_ffi.h` (keep header and `src/lib.rs` in sync). Surface:

| Function | Does |
| --- | --- |
| `inseam_version` | Core version string |
| `inseam_node_open` | Open the node under a data dir; optional profile path, else `<data_dir>/profile.toml`, else defaults |
| `inseam_node_query` | Finder query → `QueryResponse` JSON |
| `inseam_node_index_dir` | Index a local directory → `IndexReport` JSON |
| `inseam_node_free` / `inseam_string_free` | Release handles/strings the library allocated |

Conventions: fallible calls take `char **error_out` (null return + owned message on failure); every returned string is freed with `inseam_string_free`. The handle owns a tokio runtime, so calls block — app shells run them off the main thread. Responses are the same serde views `ops` serializes everywhere else.

## The Swift side (`apps/macos`)

A SwiftPM package, no Xcode project:

- `Sources/CInseamFFI/module.modulemap` — system-library target exposing the FFI header and linking `inseam_ffi`.
- `Sources/Inseam/InseamCore.swift` — `CoreNode`: RAII wrapper over the handle, JSON decoding into Swift structs (snake_case converted).
- `Sources/Inseam/ContentView.swift` — the UI: open node on launch (data dir `~/Library/Application Support/inseam`, shared with the CLI), Index Folder… button, query field, results list.

## Building

```sh
apps/macos/build.sh            # → /Applications/Inseam.app
apps/macos/build.sh /tmp/out   # → any other destination dir
```

The script builds `inseam-ffi` in release, `swift build -c release` with `-L target/release`, assembles the `.app` bundle from `apps/macos/Info.plist`, ad-hoc codesigns, and copies to the destination — default `/Applications`, so the finished app is directly launchable. Requires the Xcode Command Line Tools (`swift`) only.

Known cosmetic warning: the Rust objects target the host macOS version while the app declares `LSMinimumSystemVersion` 14.0, so `ld` prints version-mismatch warnings. Harmless for local builds; set `MACOSX_DEPLOYMENT_TARGET=14.0` when building the Rust side for distribution.
