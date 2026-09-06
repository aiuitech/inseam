# iOS App

The SwiftUI shell in `apps/ios` embeds the node core through `crates/inseam-ffi`, like the [macOS app](macos-app.md), and shares its model layer through `apps/inseamkit`. It is a leaf steward: it indexes the hosts only the phone can reach, using the phone's own models, and answers queries while it is in front. The reasoning is in [design/ios-app.md](../../design/ios-app.md).

## What it does

- **Opens the node** under the app's Application Support with Apple's on-device sentence embedder mounted as the `embedder` provider (`embedder-app`), so no API key is needed for vectors. When the model's assets are not on the device yet it requests them and opens with the base embedder, which needs an endpoint key.
- **Stewards two bridged hosts**, both listed under *this device stewards*: the photo library (`photos`, images only, full access required) and the Call Recordings folder (`call-recordings`). *index photos* and *index call recordings* run the sweep over each.
- **Attaches call recordings.** iOS lets no third-party app record a call, so the recording comes from the Phone app's own recorder (iOS 18.1+), which saves into Notes. Share it from Notes to inseam (the app is an "open in" target for audio and plain text) or save it into *Files › On My iPhone › inseam › Call Recordings*. The attach sheet names the participant — typed or from the contact picker — writes a sidecar with the participant and the call's times, transcribes the audio on device (iOS 26 `SpeechAnalyzer`) when no transcript came along, and indexes. The participant becomes a claimed `participant` property on the recording's envelope.
- **Observes calls.** With the app running, a call ending posts a local notification reminding you to share the recording; the call's start and end are kept for the attach sheet. CallKit exposes no phone number for calls the app did not place.
- **Searches** through the same finder as every client, with the shared result row.

## Layout

- `project.yml` — the XcodeGen spec; `xcodegen generate` writes `Inseam.xcodeproj`. Deployment target iOS 26, Swift 5 language mode, iPhone portrait orientation, all iPad orientations, and `LIBRARY_SEARCH_PATHS` pointing at `Core/<platform>/`.
- `build-core.sh` — builds `inseam-ffi` for `aarch64-apple-ios` and `aarch64-apple-ios-sim` with `--no-default-features` (the loaded-plugin tier is off on iOS: no JIT) and copies the static libraries under `Core/`.
- `Sources/Inseam/InseamApp.swift` — the app; opens the node on launch and routes "open in inseam" files to the attach sheet.
- `Sources/Inseam/NodeModel.swift` — owns the node handle, registers the bridged hosts after every open, runs blocking core calls off the main actor.
- `Sources/Inseam/AppleEmbedder.swift` — `NLContextualEmbedding` (Latin script, 512 dimensions, mean-pooled) as a `ShellEmbedding`.
- `Sources/Inseam/PhotosHost.swift` — PhotoKit as a `BridgedHostSource`: `photos/<local identifier>` locators; date, place, favorite, and screenshot properties; original bytes on demand.
- `Sources/Inseam/CallRecordingsHost.swift` — the Documents › Call Recordings folder as a `BridgedHostSource`, the sidecar format, and the import.
- `Sources/Inseam/CallObserver.swift` — `CXCallObserver` plus the end-of-call notification.
- `Sources/Inseam/Transcription.swift` — on-device transcription of an imported recording into a `.txt` beside it.
- `Sources/Inseam/ContentView.swift`, `ContactPicker.swift` — the search view, the hosts section, the attach sheet, and the contact picker wrapper.

## Building and running on your iPhone

Requires Xcode 26 (the iOS SDK; the Command Line Tools alone are not enough), XcodeGen (`brew install xcodegen`), and the Rust targets:

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
apps/ios/build-core.sh
cd apps/ios && xcodegen generate && open Inseam.xcodeproj
```

In Xcode, pick your team under Signing & Capabilities (a free Apple ID personal team is enough), connect the phone, and Run. Free-team builds expire after seven days and must be reinstalled from Xcode; iCloud, push, App Groups, and TestFlight need the paid Developer Program. Apple Intelligence features need an iPhone 15 Pro or later.

The shared kit's bridges are tested on macOS by `apps/inseamkit/test.sh`, which exercises a bridged host and a Swift embedder end to end through the core. Xcode builds the iOS shell and checks its platform APIs.

## The FFI it uses

Beyond the macOS app's surface ([macos-app.md](macos-app.md)): `inseam_node_open_with_shell` (open with the app's embedder), `inseam_node_register_host` / `inseam_node_unregister_host` (bridge a host in and out), and `inseam_node_index_host` (sweep a stewarded host by id). The Swift face is `CoreNode.init(dataDir:embedder:)`, `registerHost`, `unregisterHost`, and `indexHost` in `apps/inseamkit/Sources/InseamKit/Bridges.swift`.
