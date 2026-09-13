# Phone call context

Research checked on 2026-09-13. These are options and proposed experiments, not a decision to build a dialer or a carrier service. The import flow in [ios-app.md](ios-app.md) remains; the callback conference route below has since been adopted as [call-capture](call-capture.md), with its experiment gate still open.

The goal is to remember conversations with minimal interruption to the user's existing setup. Keeping the existing carrier, number, and calling habits is a requirement. Carrier switching, number porting, and replacement SIM/eSIM service are excluded. Preserving the number alone does not satisfy this requirement.

## Recommendation

Keep Apple's recorder and improve ingestion first. Investigate whether user-authorized automation can remove the later export step. Test a Mac companion only for people who already answer calls there. A callback conference recorder is the leading alternative when automatic ingestion matters more than the answer-and-merge steps. Continuum documents this exact approach. It preserves the existing carrier and number, but still requires per-call interaction. Carrier migration is excluded; forwarding, VoIP, and accessories do not meet the preferred unchanged-setup experience.

There is no documented public iOS API in the material reviewed that lets an ordinary app tap both sides of an unrelated cellular or FaceTime call. That is a platform constraint, not evidence that seamless capture is impossible everywhere. Audio can also be captured by the endpoint handling a call, the carrier transporting it, or external hardware.

## What Continuum establishes

The supplied screenshot says Continuum captures system audio without a meeting bot and processes it locally. It does not identify iOS or demonstrate recording a cellular call in Apple's Phone app. The [linked page](https://oncontinuum.com/meetings-automation) was reachable, but its fetched content did not expose the expanded answer or establish the supported capture platforms. A subsequent user-supplied [mobile guide](https://help.oncontinuum.com/overview/mobile-app#starting-a-meeting) was successfully retrieved. It explicitly describes in-person meetings, capture through the phone's microphone, and placing the phone on a table near the conversation. It does not describe recording an active cellular call on that phone. The screenshot is vendor evidence, not an instruction or an independently verified architecture.

Desktop capture is technically credible. Apple publishes a [Core Audio tap sample](https://developer.apple.com/documentation/coreaudio/capturing-system-audio-with-core-audio-taps) for macOS 14.2 and later that captures process output after system-audio permission. The local microphone is a separate input. Inferring a desktop implementation is reasonable; claiming that this is Continuum's implementation would exceed the evidence.

The mobile guide establishes an ambient microphone workflow. Its separate [phone-call guide](https://help.oncontinuum.com/overview/phone-calls), subsequently supplied by the user, explicitly documents callback conferencing: Continuum calls the user, who answers and merges it with the contact call. It does not dial the contact. Capture begins after merging, and the resulting transcript appears as a meeting. This confirms the user's original external-number suggestion as a documented vendor workflow, not an iOS audio-capture exception. No vendor was contacted and the workflow was not device-tested.

## Options compared

| Option | Existing number | Native iPhone calling | Per-call effort | Main constraint |
| --- | --- | --- | --- | --- |
| Apple recording and import | Yes | Yes | Start recording, then share | Manual capture and ingestion |
| Apple recording, Mac ingestion | Yes | Yes | Start recording; ingestion automation unproven | Notes access and Mac availability |
| Call answered on Mac, capture there | Yes | Call handled on Mac | Permission/setup and capture control | Does not cover handset-only calls |
| Carrier recording with ported number | Subject to provider eligibility | Yes | Target is none after setup | Excluded: changes the user's carrier |
| Existing carrier integration | Yes, if carrier participates | Yes, if service supports it | Target is none | Requires actual carrier media integration |
| Forward incoming calls to a bridge | Yes externally | Different delivery route | Low after setup | Incoming only; separate destination required |
| VoIP with verified existing caller ID | Outgoing presentation only | System call UI, different transport | New outgoing calling flow | Incoming calls still bypass capture |
| Conference in a recorder | Yes | Yes | Add and merge during each call | Carrier support; misses pre-merge audio |
| External recorder | Yes | Yes | Hardware and recording control | Route and accessory limitations |
| Brief dictated recap | Yes | Yes | Speak after hanging up | Recollection, not a transcript |

## Apple recording with better ingestion

Apple's [call-recording guide](https://support.apple.com/guide/iphone/record-and-transcribe-a-call-iph57c6590e9/ios) documents a user-started recording, notice to both participants, storage in Notes, and audio sharing. Recording and transcription availability differ by region and language. Verify the launch markets rather than infer availability from the device language.

Inseam already accepts audio and text, asks for a participant, and transcribes missing text locally. Its call observer explicitly runs only while the app is running. A post-call notification is therefore a convenience, not an all-day capture trigger.

Proposed improvements:

- Make sharing audio or transcript to inseam one short import interaction. Preserve original transcript speaker labels when supplied; do not regenerate them unnecessarily.
- Offer a user-invoked Shortcut or Action Button entry to import or dictate a recap. Do not advertise it as starting Apple's call recorder until a documented action is demonstrated.
- Prototype a Mac importer for user-authorized Notes content. Test supported scripting and user-run Shortcuts first. Determine whether they expose the actual audio attachment or transcript, not just the enclosing note's title.
- Treat direct parsing of Notes' private database as a separate, fragile experiment. iCloud synchronization alone is not an API, and an iCloud entitlement does not expose another app's container.

The attractive hypothesis is "tap Record on iPhone; context appears through the Mac later." Automatic extraction of synchronized call attachments is unproven. Keep explicit sharing as the reliable fallback.

## Answer the existing phone number on the Mac

Apple supports [relaying iPhone calls to a Mac](https://support.apple.com/guide/mac-help/receive-calls-text-messages-mac-mchl1b152152/mac). Its [Mac Phone guide](https://support.apple.com/guide/phoneapp/while-on-a-call-phn4828a6/mac) also describes Apple's own call recorder.

A second route is inseam capturing the Mac call process output and microphone, with permissions and visible capture controls. This combines documented capabilities, but the combined pipeline needs testing. Do not assume a particular Phone or FaceTime process, protected audio behavior, or successful capture after an AirPods route switch.

This preserves the number and carrier for desk calls. A nearby Mac cannot passively hear calls that remain routed entirely through the iPhone. Moving the call back to the handset should stop capture and mark the missing segment.

## Carrier capture is excluded when it changes the user's setup

[1GLOBAL](https://www.1global.com/compliance) publicly offers in-network voice capture, eSIM/physical SIM provisioning, and SIPREC delivery to customer archives. This is evidence of a commercially deployed class of solution. It is not confirmation that inseam can buy a suitable consumer service in Canada or the US.

Excluded flow: the user ports an eligible existing mobile number to a participating voice carrier; ordinary Phone calls traverse that carrier; the carrier forks audio to an authorized ingest service. An existing carrier partnership could avoid porting. Either approach must explicitly support the user's native mobile voice service.

A second data eSIM cannot record calls on the original carrier's voice line. A second corporate voice line works only for calls using that line. Porting to a programmable VoIP provider also does not by itself preserve native cellular service.

If the scope ever changes, qualification would require answers on local number porting, voice coverage, inbound and outbound capture, roaming, Wi-Fi Calling, emergency calling, SMS/RCS, number-based iMessage and FaceTime activation, announcement controls, capture opt-out, archive retention, and delivery APIs. Ask for minimum commitments and per-minute pricing. Preserve these as purchasing gates, not assumed capabilities. FaceTime and WhatsApp calls do not become recordable merely because the carrier supplies data.

The business tradeoff is substantial: this is a communications service partnership with recurring support obligations. It fails the unchanged-setup requirement and is retained here only as background research. An integration with an existing carrier would need to preserve the current plan and service, and no such broadly available integration was established.

## Twilio without a new public-facing number

Three distinct products are possible. Combining them loosely creates misleading promises.

**Conference recorder.** Prefer a callback variant: the user requests capture from inseam, our service calls their existing number, and they answer and merge it with their contact call. This avoids manually dialing a recorder number. Continuum's phone-call guide documents this workflow and recommends saving its outbound numbers as a contact to reduce call screening. Its contact-saving advice is a mitigation, not our guarantee of carrier delivery. The alternative is dialing a recording-service contact and merging it. Apple's [conference instructions](https://support.apple.com/en-euro/111787) require user actions and carrier support. Inseam cannot assume it can programmatically merge an arbitrary existing call. Audio before joining is absent. A recorder joining a carrier-mixed conference may receive the humans already mixed together.

**Outgoing calls with the existing caller ID.** Twilio's [Call resource](https://www.twilio.com/docs/voice/api/call-resource) accepts a verified outgoing caller ID. Inseam could initiate VoIP or arrange a two-leg callback bridge. Recipients may see the familiar number, but return calls still reach the original carrier unless separately routed. Test caller-ID delivery in each destination market. A callback avoids carrying the user's leg over app VoIP, at the price of answering an extra call and paying for two legs.

**Incoming forwarding.** Route the public number to a private bridge destination, then deliver the call to an inseam VoIP endpoint or a different reachable telephone line. Forwarding it back to the same unconditionally forwarded number creates a loop. Conditional forwarding captures only busy, unanswered, or unreachable calls, not all answered calls. Forwarding does not capture ordinary outbound calls. Carrier charges and voicemail behavior need testing.

[Twilio BYOC](https://www.twilio.com/docs/voice/bring-your-own-carrier-byoc) can preserve an existing carrier relationship when that carrier can exchange SIP traffic with Twilio. It is a carrier integration, not a setting that taps an arbitrary retail mobile plan.

For calls actually traversing Twilio, [unidirectional Media Streams](https://www.twilio.com/docs/voice/media-streams) can provide inbound and outbound tracks; a bidirectional stream receives only the inbound track. [Dual-channel recording](https://www.twilio.com/docs/voice/tutorials/how-record-single-side-call) can separate the two sides of an appropriate two-party topology. Neither feature can unmix people whom an upstream carrier already mixed into one leg.

## New Apple APIs do not remove the transport boundary

The [default calling app](https://developer.apple.com/documentation/callkit/preparing-your-app-to-be-the-default-calling-app) capability routes calling intents to a VoIP app, with cellular fallback. This reduces dialer friction if we choose to own the call transport. It does not reroute every incoming carrier call to our audio pipeline.

The separate [default dialer capability](https://developer.apple.com/documentation/LiveCommunicationKit/preparing-your-app-to-be-the-default-dialer-app) can initiate cellular calls and access recent conversation history after becoming default. Apple's documentation requires an EU developer account and EU-located device for testing the relevant APIs. It does not document raw cellular-audio capture. That metadata could improve call association for an eligible product, but it should not become a universal iOS promise.

[Microphone injection](https://developer.apple.com/documentation/AVFAudio/Adding-synthesized-speech-to-calls) adds app-generated speech to calls for accessibility. It is not an API to read the remote speaker. [ReplayKit explicitly defines an active-call recording error](https://developer.apple.com/documentation/replaykit/rprecordingerrorcode/activephonecall), and ordinary [audio recording sessions can be interrupted by calls](https://developer.apple.com/documentation/avfaudio/avaudiosession/category-swift.struct/record). Do not fund a product around screen recording, background microphone tricks, or a VPN magically exposing call audio.

## Hardware and context without recording

Plaud illustrates the physical route: its [phone recorder uses speaker vibrations](https://support.plaud.ai/hc/en-us/articles/50837232018585-What-is-the-difference-between-Note-Recording-and-Phone-Call-Recording), while its [documented headphone limitation](https://support.plaud.ai/hc/en-us/articles/50837313885337-Can-I-use-headphones-earphones-when-I-record-a-phone-call-with-Plaud-Note) shows why this is not invisible system-audio access. An accessory integration can serve existing owners, but adds exactly the component this request hopes to avoid. A recording headset is another hardware hypothesis, requiring a partner and routing tests.

A cheaper fallback is a user-started, 20-second recap: who called, what changed, and what must happen next. Calendar context can suggest participants, subject to confirmation. Store this as the user's recollection and never fabricate quotations or claim it is a transcript. This can be useful even when the person forgot to record or intentionally declined recording.

## Fit with inseam and operating budget

Preserve the source model. Imports remain phone-owned sources; a carrier recording service is a separate host connected to an always-on node. Use provider call IDs for idempotent delivery. Correlate later imports without overwriting source provenance. Summaries should point to transcript spans or to the explicit user recap. The current phone observer's timestamps are observed metadata, not authoritative carrier records.

Live provider ingestion must not depend on an iPhone staying awake. An owner-controlled reachable service would receive it. Hosted ingestion and cross-node availability are proposed dependencies, not shipped behavior.

Illustrative capacity calculations, not measured performance or vendor quotes:

- Two mono 16 kHz, 16-bit PCM tracks consume 64,000 bytes/second, about 230 MB/hour. A 30-second ring buffer is 1.92 MB per call, excluding models and protocol overhead.
- Two 8 kHz, 8-bit tracks consume 128 kbit/second raw, about 171 kbit/second after base64 before JSON/TLS overhead. This matches the encoding described in [Twilio's stream messages](https://www.twilio.com/docs/voice/media-streams/websocket-messages).
- A combined compressed archive budget of 32 to 64 kbit/second is 14.4 to 28.8 MB/hour. Ten hours per week is roughly 0.62 to 1.25 GB per month using 4.33 weeks. Codec quality needs measurement.
- Speech inference is the likely CPU/energy bottleneck. Benchmark a one-hour sample on a target phone and Mac; measure elapsed time, peak memory, battery change, and recognition errors. Never infer throughput from API availability.
- Batch transcript indexing after the call. Bound concurrent ingestion, retries, spool bytes, and retention before implementation. The existing importer allows four-hour recordings; keep an explicit ceiling. If transcription falls behind, show a capture gap or use an explicitly enabled bounded spool rather than unbounded buffering.

Cost per call is the sum of transport legs, stream/recording charges, speech inference, and retention. Callback/forwarding designs can pay for two telephone legs. Carrier pricing remains quote-dependent. Measure call setup delay and added round-trip latency separately from time until searchable context; keep transcription failure outside the voice path wherever the transport allows it.

Transient audio processing still handles private conversation content, and a retained transcript is stored content. Apple's [review rule 2.5.14](https://developer.apple.com/app-store/review/guidelines/#software-requirements) requires consent and a clear capture indication. Design explicit capture state, participant notice, stop controls, and deletion of derivatives. Local processing or discarding audio alone does not establish legal compliance.

## Experiments before choosing a product

| Experiment | Bounded scope | Evidence required to proceed |
| --- | --- | --- |
| Apple import UX | One target iPhone, ten recordings across short and long calls | Exact taps; audio and transcript fidelity; duplicate handling; interruption recovery |
| Notes-to-Mac ingestion | Ten synchronized recordings, supported automation first | Actual attachment access, stable identifiers, deletion behavior, delayed-sync handling |
| Mac audio capture | Twelve calls across built-in audio, AirPods, USB headset, and handset handoff | Both speakers captured; route changes handled; gaps reported; no unrelated audio retained |
| Callback conference capture | Six calls across two intended carriers | Incoming callback can merge with the contact call, screening handled, notice reaches participants after merge, join delay measured, missing start marked |
| VoIP/forwarding prototype | Only if its UX tradeoff is accepted | Correct inbound/outbound identity, no forwarding loop, failure/voicemail behavior, per-call cost |

These experiments are proposed, not executed. The immediate implementation priority is reducing friction in the existing import path. The research priority is comparing Apple recording plus import with callback conferencing: the first minimizes in-call disruption, while the second can automate ingestion without changing phone service. The Mac experiment can establish useful automatic capture without committing inseam to being a telephone provider.
