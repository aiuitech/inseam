# In-person meeting recording

The iOS app records ambient conversations through the built-in microphone. This is separate from cellular-call capture in [phone-call-context.md](phone-call-context.md). Start and stop are explicit; the user can name the meeting. Recording continues with the screen locked through the audio background mode. No automatic participant recognition is claimed.

## Preserve the evidence

Use AVAudioRecorder with lossless Apple Lossless audio in a CAF container, requesting 48 kHz and 16-bit source depth. Select a built-in data source supporting the stereo polar pattern when available. Fix the input orientation to the interface orientation at start and do not rotate channels while recording. Verify the active input and actual channel count. Mono is an explicit fallback when stereo is unsupported, never a two-channel copy of mono advertised as stereo.

Apple documents [built-in stereo capture](https://developer.apple.com/documentation/avfaudio/capturing-stereo-audio-from-built-in-microphones). The resulting channels can preserve useful relative level and timing cues. They are a system-provided stereo pair, not raw access to every physical microphone, absolute speaker bearings, or isolated tracks per participant. Voice matching and speaker labeling remain future processing; spatial cues alone do not identify names. The recording UI asks the user to keep the phone still.

Write a sidecar before recording starts and finalize it after stopping. Retain the recording ID, start/end time, actual file sample rate and channel count, selected input/data source/polar pattern, fixed input orientation, and stop reason. Keep the original audio unchanged when transcribing. Store meetings beside existing imports in the Call Recordings folder to reuse the host and indexing path without migrating existing addresses. An optional meeting metadata field distinguishes meeting sources and titles from call imports; older call sidecars remain readable.

## Lifecycle and bounds

Microphone denial or session setup failure produces an actionable error. Calls, input-route changes, and media-services reset stop the recording; do not silently switch microphone geometry or resume into a gap. A recording stops after four hours, matching the transcription ceiling. Save audio before transcription, so a model or indexing error cannot destroy it. Unique IDs prevent same-minute recordings from overwriting one another. A prewritten sidecar leaves provenance beside a partial file if the process terminates unexpectedly; such recordings are not represented as completed.

A 48 kHz, 16-bit stereo source produces 192,000 bytes/second before compression, about 691 MB/hour or 2.77 GB/four hours. Lossless compression is content-dependent. Require 3 GB available before starting to cover the four-hour budget with headroom; other processes can still consume space, so encoder errors must retain the file and be reported. AVAudioRecorder streams to disk; memory does not scale with meeting duration. Speech inference is deferred until capture ends, reducing competition for audio resources. This first version has no pause, live speech inference, or automatic speaker labeling.

## Verification

Build the iOS app with warnings treated as errors. Test sidecar compatibility and meeting-source metadata using a temporary host without microphone or network access. Physical-device checks remain necessary for microphone permission, actual stereo separation, mono fallback, screen locking, phone interruptions, input changes, and low storage. Simulator or compiler success cannot establish acoustic quality.
