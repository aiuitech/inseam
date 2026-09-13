# The Call Capture Host

The `connection-twilio-calls` plugin lets a node join a phone call you are already on and keep what was said ([design/call-capture.md](../../design/call-capture.md)). The node owns a phone number through a Twilio account. You press a button while on a call; the node dials your phone from that number; you answer the second call and tap **merge**. From then on the line records. When the call ends the recording and its transcript move into the node's archive and are indexed. Dialing the number yourself and merging works the same way.

One plugin does two jobs: it provides the `call-capture` seam (the button) and registers a host of kind `twilio-calls` (the archived calls).

## Configure

The entry is in the base composition, disabled. Enable it and put the credentials in the environment:

```toml
[[entry]]
id = "twilio-calls"
disabled = false
[entry.config]
# account_sid_env = "TWILIO_ACCOUNT_SID"                     # the (sub)account the key is scoped to
# api_key_sid_env = "TWILIO_API_KEY_SID"                     # an API key created inside that account
# api_key_secret_env = "TWILIO_API_KEY_SECRET"
# phone_number_env = "TWILIO_PHONE_NUMBER"                   # the account's number, E.164 (+14155550123)
# intelligence_service_sid_env = "TWILIO_INTELLIGENCE_SERVICE_SID"   # optional: a Conversation Intelligence service; unset = audio only
# notice = "This call is being recorded by inseam for the person who added this line."
# recording_secs_max = 7200        # the recording ceiling per call; Twilio allows up to 14400
# ring_secs = 30
# calls_per_hour_max = 6           # verification and capture calls together
# verification_ttl_secs = 600
# recordings_per_pull_max = 25     # per sweep
# transcript_polls_per_pull_max = 25
# recording_bytes_max = 67108864   # 64 MiB: MP3 at 32 kbit/s covers four hours
# timeout_ms = 60000
# retain_at_provider = false       # keep Twilio's copy after the archive has it
```

On a hosted node the control plane provisions all of this: a Twilio subaccount for your node alone, an API key scoped to it, a number, and a transcription service, handed to the node through its environment ([../architecture/hosted-node.md](../architecture/hosted-node.md)). Self-hosting, use your own Twilio account: create an API key, buy a voice-capable number, and point the number's voice URL at a TwiML document that says the notice and records (the console's `/api/twiml/capture` is one; a TwiML Bin with `<Say>` then `<Record maxLength="7200" timeout="120"/>` is another). Optionally create a Conversation Intelligence service and set its SID.

`inseam plugins` lists the four variables with the reason each is needed.

## Use

```sh
inseam capture                          # the capture number, your number's state, the last call, archived recordings
inseam capture number +14155550123      # the node calls you and speaks a six-digit code
inseam capture verify 123456            # confirm it; only a verified number is ever dialed on request
inseam capture start                    # during a call: answer the second call, then merge
inseam index --host <twilio-calls-…>    # pull finished recordings into the archive and index them
```

The same four are owner operations on every transport: `GET /api/v1/owner/capture`, `POST /api/v1/owner/capture/number` (`{"number": "+1…"}`), `POST …/capture/verify` (`{"code": "…"}`), `POST …/capture/start`. The iOS app's **capture this call** button drives them against the hosted node whose URL and owner token you enter under *settings › hosted node*.

Verification exists because anyone holding the owner token could otherwise make the node ring any phone. Calls are rate limited: at least a minute apart, at most `calls_per_hour_max` an hour.

## What the host serves

Each archived call is up to three sources sharing the recording's Twilio SID as a stem:

| Locator | Type | What |
| --- | --- | --- |
| `<RE…>/audio.mp3` | `audio/mpeg` | the recording, 32 kbit/s |
| `<RE…>/transcript.txt` | `text/plain` | one line per sentence: `[mm:ss] channel N: words` — once the transcript settles |
| `<RE…>/call.json` | `application/json` | the sidecar: call SID, from, to, start, duration, transcript state, whether Twilio's copy is gone |

Envelopes carry the call's start as `created`, a hint like `Call capture 2026-09-12 (12 min)`, and claimed properties `call.from`, `call.to`, `call.duration_secs`. The host has one scope, the whole host (`""`); `locator_prefix` says so, so vanished recordings reconcile.

## The pull

Every enumeration starts by pulling: list Twilio's completed recordings (one page, `recordings_per_pull_max`), and for each not yet archived, fetch the call's from/to, download the MP3 under `recording_bytes_max`, write the sidecar, write the audio, and request a transcript when a service is configured. Then, for archived calls whose transcript is pending (up to `transcript_polls_per_pull_max` per run), ask Twilio; a completed one is rendered and written, a failed one is marked. Finally, any call whose transcript has settled and whose Twilio copy still exists is **deleted at Twilio**, unless `retain_at_provider` is set — the provider account belongs to whoever provisioned it, and your calls should not rest where they can read them. A pull that fails (Twilio unreachable) is logged, and the archive is listed as it stands, so an outage never makes archived calls vanish from the catalog.

The archive lives at `<data-dir>/<entry id>/recordings/<RE…>/`. This is the one host whose content the node stores rather than references, because after deletion the archive holds the only copy.

## What it cannot do

- Capture audio before the merge. The recording starts when you merge.
- Separate the people on your side of the merge: the carrier hands the node one mixed channel. The transcript attributes by channel; the node's own leg is silent.
- Play the notice to people who join after the merge; the notice is spoken to whoever answers the second call, and its text is configuration.
