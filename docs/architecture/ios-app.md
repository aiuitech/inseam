# iOS App

The SwiftUI shell in `apps/ios` embeds the node core through `crates/inseam-ffi`, like the [macOS app](macos-app.md), and shares its model layer through `apps/inseamkit`. It is a leaf steward: it indexes the hosts only the phone can reach, using the phone's own models, and answers queries while it is in front. The reasoning is in [design/ios-app.md](../../design/ios-app.md).

## What it does

- **Opens the node** under the app's Application Support with Apple's on-device sentence embedder mounted as the `embedder` provider (`embedder-app`), so no API key is needed for vectors. When the model's assets are not on the device yet it requests them and opens with the base embedder, which needs an endpoint key.
- **Stewards two bridged hosts**, both listed under *this device stewards*: the photo library (`photos`, images only, full access required) and the Call Recordings folder (`call-recordings`). *index photos* and *index call recordings* run the sweep over each.
- **Attaches call recordings.** Inseam does not capture live cellular-call audio. Its recording import flow uses the Phone app's own recorder (iOS 18.1+), which saves into Notes. Share it from Notes to inseam (the app is an "open in" target for audio and plain text) or save it into *Files › On My iPhone › inseam › Call Recordings*. The attach sheet names the participant — typed or from the contact picker — writes a sidecar with the participant and the call's times, transcribes the audio on device (iOS 26 `SpeechAnalyzer`) when no transcript came along, and indexes. The participant becomes a claimed `participant` property on the recording's envelope.
- **Records in-person meetings.** Tap *record a meeting*, optionally name it, then start and stop. The app requests microphone access and uses the built-in microphone. Supported inputs retain stereo in a lossless CAF file; other devices record mono. The UI shows which was selected. Recording continues with the screen locked and stops at four hours, on an audio interruption, or when the input configuration changes. Keep the phone still to preserve useful stereo cues. Participants are not identified automatically.
- **Saves before transcribing.** Meetings use unique filenames in *Files › On My iPhone › inseam › Call Recordings*. A JSON sidecar retains the name, timing, microphone configuration, channel orientation, and stop reason. The original survives transcription errors. At least 3 GB free space is required before starting. After saving, inseam transcribes on device and indexes through the recordings host. Interrupted capture requires starting a new recording.
- **Captures a call through the hosted node.** *capture this call* opens a sheet against the always-on node whose URL and owner token are saved under *settings › hosted node*. The first time, enter your phone number: the node calls it and speaks a code, which you type back. After that the button makes the node ring you; answer and tap *merge calls*, and the recording and transcript land in that node's index ([../indexing/call-capture-host.md](../indexing/call-capture-host.md)). The phone places nothing itself.
- **Observes calls.** With the app running, a call ending posts a local notification reminding you to share the recording; the call's start and end are kept for the attach sheet. CallKit exposes no phone number for calls the app did not place.
- **Searches** through the same finder as every client, with the shared result row.
- **Shows each indexing run** as it enumerates, catalogs, indexes, and finalizes. The process view reports source counts and the current address and can pause, resume, or stop the Rust sweep. A stopped run keeps completed sources and the next run resumes.
- **Configures the node** from a Settings sheet. Connections, model endpoints, indexing limits, ignore rules, search ranking, Keychain secrets, and raw composition use the same validated settings bridge as macOS. Visual saves retain the Apple on-device embedder; raw composition can replace it deliberately.

Configuration rows keep a caption visible above each editable value. Placeholder text only describes an empty value; it never carries the field's identity.

Further capture options and their evidence are in [phone-call-context.md](../../design/phone-call-context.md). Carrier recording, Mac capture, and automated Notes ingestion are research proposals, not current app capabilities.

## Layout

- `project.yml` — the XcodeGen spec; `xcodegen generate` writes `Inseam.xcodeproj`. Deployment target iOS 26, Swift 5 language mode, iPhone portrait orientation, all iPad orientations, and `LIBRARY_SEARCH_PATHS` pointing at `Core/<platform>/`.
- `build-core.sh` — builds `inseam-ffi` for `aarch64-apple-ios` and `aarch64-apple-ios-sim` with `--no-default-features` (the loaded-plugin tier is off on iOS: no JIT) and copies the static libraries under `Core/`.
- `Sources/Inseam/InseamApp.swift` — the app; opens the node on launch and routes "open in inseam" files to the attach sheet.
- `Sources/Inseam/NodeModel.swift` — owns the node handle, registers the bridged hosts after every open, runs blocking core calls off the main actor.
- `Sources/Inseam/SettingsView.swift` — first-party configuration, Keychain secrets, and raw composition editors for the phone's node.
- `Sources/Inseam/AppleEmbedder.swift` — `NLContextualEmbedding` (Latin script, 512 dimensions, mean-pooled) as a `ShellEmbedding`.
- `Sources/Inseam/PhotosHost.swift` — PhotoKit as a `BridgedHostSource`: `photos/<local identifier>` locators; date, place, favorite, and screenshot properties; original bytes on demand.
- `Sources/Inseam/CallRecordingsHost.swift` — the Documents › Call Recordings folder as a `BridgedHostSource`, the sidecar format, and the import.
- `Sources/Inseam/CallObserver.swift` — `CXCallObserver` plus the end-of-call notification.
- `Sources/Inseam/MeetingRecorder.swift`, `MeetingAudioSession.swift` — recording lifecycle and stereo input configuration.
- `Sources/Inseam/MeetingMetadata.swift` — capture geometry and termination metadata in the backward-compatible sidecar.
- `Sources/Inseam/MeetingRecordingView.swift` — start/stop, capture status, and saved-audio sharing.
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

Beyond the macOS app's surface ([macos-app.md](macos-app.md)): `inseam_node_open_with_shell` (open with the app's embedder), `inseam_node_register_host` / `inseam_node_unregister_host` (bridge a host in and out), and `inseam_node_index_host` (sweep a stewarded host by id). Controlled directory and host calls take a borrowed callback for `IndexProgress` JSON; it returns continue, pause, or stop. `inseam_settings_write_preserving_embedder` gives the visual iOS editor the ordinary validated write path without masking the shell's provider. The Swift faces live in `InseamKit` as `CoreNode`, `IndexController`, and the bridge protocols.

## Recording verification

After generating the Xcode project and building the simulator core, run the file/metadata tests on an available ARM iOS simulator:

```sh
xcodebuild -project apps/ios/Inseam.xcodeproj -scheme Inseam \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
  CODE_SIGNING_ALLOWED=NO SWIFT_TREAT_WARNINGS_AS_ERRORS=YES \
  GCC_TREAT_WARNINGS_AS_ERRORS=YES test
```

These tests exercise lossless channel preservation and host compatibility without opening the microphone or the node. Real microphone geometry, lock-screen capture, permission denial, incoming calls, and route changes require a physical-device pass. The test target compiles the host and metadata files directly, without launching the application or downloading model assets.

Validation on 2026-09-13: five simulator tests passed, including distinct-channel lossless round-trip, legacy call metadata, meeting metadata, incomplete capture, and invalid locators. The unsigned device build passed with compiler warnings treated as errors. The recorder entry point and sheet were visually checked in Simulator. The unsigned simulator app could not open the node's Keychain, so live transcription/indexing was not verified there. Physical capture checks remain outstanding.
