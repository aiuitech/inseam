# iOS App

The phone as a node. This settles what an iPhone can steward, how its app shell reaches hosts and models the Rust core cannot, why it needs no API keys, what "record a call" can honestly mean on iOS, and how the app gets onto a phone. It also answers the open question in [runtime](runtime.md): mobile is a node platform, with a specific shape.

The platform facts below were checked against Apple's documentation and current coverage in September 2026 (iOS 26 shipping, iOS 27 in beta). They are constraints, not preferences; the decisions follow from them.

## The phone is a leaf steward

A node is one binary plus a composition plus a data directory ([runtime](runtime.md)); the iOS app embeds the same `inseam-ffi` distribution the macOS app does and is a node in every sense but one: it is not always on. iOS suspends an app shortly after it leaves the foreground, allows no long-running server, and grants background time only in bounded slices (a nightly `BGProcessingTask` while charging; a user-initiated `BGContinuedProcessingTask` with a visible progress bar). So the phone is a **leaf steward**: it stewards the hosts only it can reach, indexes them while it is in front (and in background slices), answers queries while in front, and relies on the network's always-on node ([network](network.md)) for reach. What a leaf cannot do is serve fetches at arbitrary times; the [roster](roster.md) will carry that as a stewardship fact, and [address sync](address-sync.md) is how the phone's *catalog* — addresses and envelopes, never content — reaches the rest of the network so a photo is discoverable from the desk even when the phone is asleep. That sync is future work; the shape here is what it syncs.

The index is the phone's own ([indexing](indexing.md)): derived, local, never shipped. The data directory lives in the app's Application Support; nothing in it needs iCloud.

## The shell is a plugin: bridged hosts and providers

Some hosts exist only behind a platform framework in the app's process — the Photos library through PhotoKit, a folder the user picked through a security-scoped bookmark, on macOS a Notes folder or the Messages database. Some capabilities do too: Apple's on-device sentence embedder, the on-device speech transcriber, the Foundation Models LLM. None of these can be reached from Rust, and none should be copied into files just to be reached.

So the app shell is a plugin in its own right, in both directions, through the FFI:

- **A bridged host** is an ordinary registration in the `connections` seam ([connections](connections.md)) whose `Connection` is a set of C callbacks the shell supplies: enumerate a root into the same `EnumeratedSource` shape every connection produces (locator, envelope fields, byte size, claimed properties), and read one locator's bytes. The host id is derived from kind and principal exactly as every connection derives it ([addressing](addressing.md)), so two devices bridging the same library would agree. The registration's entry id is `app:<kind>`: the shell's code is its composition. The sweep, the finder, and operations see nothing special — a Photos host and a Gmail host are indistinguishable to them, which is the whole point of the registry being the seam.
- **A shell provider** is a linked plugin the distribution carries whose implementation is the shell's callbacks. The first is `embedder-app`: the shell declares a model identity and width, and the node's `embedder` entry is re-pointed at it beneath the node's own composition, which still wins — an owner who prefers an endpoint names one. Same shape later for `llm-app` (Foundation Models) and a transcriber.

Both bridges share one contract: callbacks may run on any thread, concurrently, from the core's blocking pool; every buffer the shell returns is freed through the shell's own free callback; `release` runs exactly once, after the last in-flight use, so the shell frees its context there and nowhere else. A registration the node refuses never takes the context — the shell keeps it. Everything is bounded: sources per enumeration, bytes per read, properties per source, texts per embed call, bridged hosts per node.

*Rejected:* **exporting to files** (materialize photos and recordings into a directory the filesystem connection indexes). It copies content the index is supposed to reference ([addressing](addressing.md)), doubles storage on the most storage-constrained device, and loses the host's own metadata at the door. *Rejected:* **a Swift plugin runtime** (writing connection plugins in Swift against a Swift seam). Seams are Rust; the linked tier is Rust; a second plugin language for one platform is a second system. The bridge is one C struct per direction and the plugin stays where plugins live. *Rejected:* **making the shell call operations to insert catalog rows directly.** The sweep owns reconciliation, change detection, shape stamps, and re-indexing; a host that bypasses it would need all of that rewritten in Swift.

## Loaded plugins are off on iOS

The loaded tier runs WASM components through wasmtime's Cranelift JIT ([plugins](plugins.md)), which needs executable memory that iOS denies to third-party apps. `inseam-ffi` therefore has a `loaded-plugins` feature, on by default and off for the iOS build: the iOS node mounts linked plugins and bridged hosts and providers, and a `wasm:` entry in its composition fails contained, by name. Wasmtime's Pulley interpreter (no native code generation, roughly an order of magnitude slower) is the path to turning the tier back on; it is not proven on iOS yet and the OCR reference plugin is the first candidate to prove it with.

## No API keys: models on the device

The question was whether an installed ChatGPT, Claude, or Gemini app, or the user's subscription to one, could do the node's inference. The answer is no, on every axis:

- **Another vendor's app cannot be a backend.** Keychain items are shared only within one developer's Team ID; the ChatGPT app's Shortcuts action ("Ask ChatGPT") is callable only by system surfaces, and the only cross-app path (`shortcuts://run-shortcut`) foregrounds the Shortcuts app and cannot run in the background. None of it exposes embeddings.
- **No provider sanctions "bring your subscription."** Anthropic forbids third parties routing through Claude.ai credentials and enforces it server-side. Google forbids third-party use of Gemini CLI's OAuth and has removed the consumer path. OpenAI's public "Sign in with ChatGPT" (August 2026) is identity only — name, email, avatar — with no model access; the Codex-client token path is informally tolerated for personal use, chat-only, embedding-free, and has no iOS-shaped redirect. A product cannot stand on any of these.
- **Apple's iOS 27 provider protocol is bring-your-own-backend**, not bring-your-subscription: `LanguageModel` conformers wrap the developer's own API keys.

What the device *does* have, without keys or network:

- **Embeddings: `NLContextualEmbedding`** (iOS 17+), a BERT-style sentence model per script family, 512 dimensions on iOS, Apple-shipped assets downloaded once. This is `embedder-app`'s first backend, mean-pooled over tokens, with the identity `apple/nlcontextualembedding-latin-mean1` so a pooling change re-embeds. Its retrieval quality against the open models the benchmarks use is unmeasured; [benchmarking](benchmarking.md) decides whether it stays the default or a Core ML port of a small open embedder (MiniLM, e5-small — 15–25 ms per text on recent iPhones) replaces it. Foundation Models exposes no embeddings.
- **Chat: the Foundation Models framework** (iOS 26+; iPhone 15 Pro and later), a ~3B-parameter on-device model with guided generation and tool calling, a 4,096-token window shared by prompt and output, image input from iOS 27, and rate limiting in the background. That is the summarizer's and the entity extractor's future `llm-app` backend, with the sweep's budgets and the 4k window as its ceilings. Private Cloud Compute (iOS 27, 32k window) needs a managed entitlement gated on the App Store Small Business Program — usable for a small commercial distribution, not assumable for every node.
- **Speech: `SpeechAnalyzer`** (iOS 26+), on-device, no duration cap, file input, timestamps, no diarization. This is how an imported call recording becomes searchable text.
- **Vision:** OCR, classification, and `VNGenerateImageFeaturePrintRequest` (a 768-float image fingerprint, not text-aligned) are on-device; MobileCLIP via Core ML gives text-aligned image vectors at a few milliseconds each. Photos' own captions, people, and semantic search are not exposed to third-party apps. Image understanding is the next provider bridge, not this one.

The node's first-party endpoint plugins still work on iOS for owners who set a key in the app's Keychain, as on macOS.

## What the phone stewards

**Photos** — a bridged host of kind `photos`. PhotoKit with full read access (limited access is refused by name rather than indexing a silent subset) exposes dates, place, favorite, kind, and album membership without touching pixels; the original bytes are read on demand, from iCloud when offloaded. Locators are `photos/<local identifier>`. `PHPhotoLibraryChangeObserver` is the change feed for a later targeted sweep ([index-maintenance](index-maintenance.md)); today the sweep's own reconciliation finds what changed. Images only until a video transform exists.

**Call recordings** — a bridged host of kind `call-recordings` over the app's Documents › Call Recordings folder, which the Files app shows as *On My iPhone › inseam*. A recording is up to three sources sharing a stem: the audio, its transcript, and a JSON sidecar whose participant and call times become claimed envelope properties on the other two ([access-control](access-control.md): claimed, never verified — the owner said who it was).

**Messages — not on iOS.** No API reads iMessage or SMS history; iMessage app extensions see only their own payloads, and SMS filter extensions see unknown-sender SMS only and cannot store what they see. The Mac is where Messages is indexed: with Messages in iCloud, the full history is in `~/Library/Messages/chat.db`, readable with Full Disk Access — a macOS bridged host or linked connection, to be built there.

## Recording calls, honestly

An ordinary third-party iOS app has no documented public API to capture the audio of a cellular or FaceTime call handled by another app. Calls transported by the app itself or a participating carrier are different; see [phone-call-context.md](phone-call-context.md) for the researched options and proposed experiments. During a call the system's audio session is non-mixable and higher priority: the app's session is interrupted, its microphone input stops, and the remote party's audio never reaches any third-party process. Microphone permission changes nothing; Apple's own screen recording captures no call audio for the same reason. `CXCallObserver` reports that a call started, connected, and ended — and *not* the number. The public APIs reviewed for [phone-call-context.md](phone-call-context.md) do not establish access to that audio.

Apple's own call recorder (iOS 18.1+) is the route: it announces itself to both parties, saves the audio and an on-device transcript with speaker labels into Notes' *Call Recordings* folder, and lets the user *Share Audio* or *Save Audio to Files* from there. Treat imported audio as mixed unless inspection establishes separate tracks; do not infer channel separation from transcript speaker labels. A provider-controlled call transport can expose separate inbound and outbound tracks, as covered in [phone-call-context.md](phone-call-context.md).

So the feature is shaped around what exists:

1. While running, the app observes calls. iOS does not wake it for arbitrary call events. When an observed call ends, it posts a local notification: recorded it in Phone? Share the recording to inseam. The call's start and end are kept (bounded, the last 32) so the attach step can fill them in.
2. The user shares the recording (and, optionally, Notes' transcript) to inseam — the app is an "open in" target for audio and plain text, which needs no entitlement — or saves it into the Call Recordings folder.
3. The attach sheet takes the participant (typed, or from the system contact picker, which runs out of process and needs no Contacts permission), writes the sidecar, transcribes the audio on device when no transcript came along, and indexes.

Everything a phone number would have carried automatically is carried by the sidecar, claimed by the owner.

*Rejected:* **inseam as a VoIP dialer.** A CallKit VoIP call the app itself places owns its audio pipeline and could record both parties on separate channels. That is a phone product, not a context product, and it records only calls made through inseam. *Rejected:* **reading Notes' container.** It is sandboxed and end-to-end encrypted; there is no API.

## Storage

Recordings live in the app's Documents folder because that is visible in Files and reachable by "open in place" with no entitlement at all — it works on a free personal team. An iCloud Drive container (`NSUbiquitousContainers`) would show the same folder on every device and is where this moves once distribution is paid anyway; the node's data directory stays local on every platform because the index is per node ([discovery](discovery.md)).

## The owner can configure the leaf

The phone owns a composition and Keychain just as the Mac does, so it exposes the same first-party configuration groups: connections, models, indexing, search, secrets, and the raw composition. Saving the visual form validates and atomically writes through `inseam-ffi`, then reopens the node. Secrets remain out of the composition and use the shared `app.inseam.secrets` Keychain namespace.

The one deliberate difference is the embedder. The iOS shell mounts `embedder-app` beneath the owner's overlay. A visual settings save preserves any `embedder` entry already in that overlay instead of writing the desktop endpoint defaults over the on-device model. The Models page therefore reports the Apple provider as read-only. An owner can still replace it deliberately in the raw composition editor. Configuration saves are disabled while an operation holds the node, so the app never closes a handle beneath a blocking FFI call.

## Building and testing on a phone

Xcode (the iOS SDK) is required; the Command Line Tools alone build the macOS app but cannot cross-compile the Rust core for `aarch64-apple-ios` — every C build script asks `xcrun` for the SDK. The project is an XcodeGen spec (`apps/ios/project.yml`) so the `.xcodeproj` is generated, never merged; `apps/ios/build-core.sh` builds the core for device and simulator with the loaded tier off.

A free Apple ID personal team installs the app on your own iPhone from Xcode: the profile expires after seven days (reinstall from Xcode), ten App IDs and three devices a week, and no iCloud, push, App Groups, or TestFlight. Everything the app does today fits inside that. The paid program ($99/year) removes the expiry, unlocks the iCloud container and TestFlight for handing builds to other people. Apple Intelligence features need an iPhone 15 Pro or later.

## Open questions

- **Catalog sync from a leaf.** The phone's envelopes reaching the always-on node is what makes photos discoverable from anywhere; [address-sync](address-sync.md) is unbuilt, and a leaf's stewardship record needs a "serves while awake" fact in the [roster](roster.md).
- **`llm-app` over Foundation Models**, and whether summaries and entities are worth the 4k window and the background rate limits on a phone, or whether a leaf should ship catalog-only and let a bigger node deepen what it can fetch ([index-maintenance](index-maintenance.md), deep budget).
- **The embedder to standardize on** for small devices: `NLContextualEmbedding` versus a Core ML port of the benchmark models. One embedding identity per network is the simplest story for cross-node ranking; the benchmarks decide.
- **Image understanding as a bridge**: MobileCLIP or Foundation Models' image input feeding a transform, so a photo is searchable by what is in it, not only when and where.
- **Pulley** for the loaded tier on iOS.
- **The macOS Messages host**, and whether Apple's call recordings on a Mac (Notes syncs them) can be picked up there without the share step.
