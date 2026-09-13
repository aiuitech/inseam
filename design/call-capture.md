# Call capture

How a conversation the owner is having on their ordinary phone becomes a source, without changing their carrier, number, or dialer. [Phone-call-context](phone-call-context.md) compared the routes and named callback conferencing the leading one when automatic ingestion matters more than the merge step; this is that route, adopted, with the trust model that makes it fit a node the owner does not operate themselves.

## The shape

The owner's always-on node owns a telephone number through a programmable-voice provider (Twilio first). While the owner is on a call, they press one button. The node dials the owner's phone from that number; the owner answers the second call and merges it into the first with the phone's own conference control. From the merge on, the node's leg hears the conversation and records it. When the call ends, the recording and its transcript move into the node's archive and are indexed like any other source. Dialing the number directly and merging works the same way, for the day the callback is screened.

Three seams carry it, and nothing else in the node knows telephony exists:

- **`call-capture`** — a small seam a button drives: status, set the owner's number, verify it, start a call. Its provider is the telephony plugin. The `operations` seam exposes those four as owner operations, so every transport — the CLI, the HTTP owner API the phone speaks, the console — is the same thin skin.
- **`connections`** — the same plugin registers one host of kind `twilio-calls`, principal the provider account, whose sources are the archived calls: `<recording>/audio.mp3`, `<recording>/transcript.txt`, `<recording>/call.json`, sharing a stem exactly as the iOS call-recordings host does. The sweep, the finder, and operations see a host.
- **`state`** — the owner's verified number, a pending code, and the hour's call starts, none of which is a credential.

The node never serves a webhook and never opens a port. Both calls it places carry their instructions inline: a spoken code for verification, a spoken notice then a long recording for capture. The number's inbound instructions are one static document the console serves for every tenant alike, carrying no tenant data. Recordings are pulled, not pushed: each enumeration first moves finished recordings from the provider into the archive, bounded per run, then lists the archive.

## Why the owner verifies their own phone

Anyone holding the owner token could otherwise make the node ring an arbitrary number at the owner's expense, or worse, at someone else's. So setting the number places a call to it that speaks a six-digit code, and only a number whose code was read back may be dialed on request. Verification, capture, and their combination are rate limited: a minimum gap between calls and a ceiling per hour, both configuration.

## The provider account is the tenant's, not the operator's

A hosted node must not hold a credential that reaches another tenant, and the operator must not be able to read a tenant's calls ([hosted-service](hosted-service.md)). Both fall out of one choice: **one Twilio subaccount per tenant**, created by the control plane at provision time, with the number bought inside it and an **API key scoped to it** handed to the node. The node reaches its own subaccount's numbers and recordings and nothing else — the same blast radius the per-tenant volume key gives. The key's secret is returned by the provider once, at creation; the control plane renders it into cloud-init and never stores it, and a rebuild mints a fresh key and deletes the old, exactly like the node token. A self-hoster's own Twilio account plays the subaccount's role with no change to the plugin.

The recordings rest in the provider's storage under an account the operator's master credentials can open. That is the one place the "cannot read the node" invariant would leak, and it is closed by the node: **a recording is deleted from the provider the moment the archive has its audio and its transcript has settled**. The archive therefore stores bytes — unlike every other remote host, whose content the index only references ([addressing](addressing.md)) — because the copy is the only one. Retaining the provider's copy is an explicit opt-in (`retain_at_provider`) for a self-hoster who wants Twilio's console to show their calls.

Transcription is the provider's Conversation Intelligence, one service per subaccount created with the tenant's own key and data logging off. It exists because the always-on node has no speech model of its own today; the phone transcribes on device, the desktop node asks the provider. The transcript is a source in its own right, one timestamped line per sentence with the media channel as the only speaker attribution a merged call can honestly carry.

## The tier is forced, and it is honest

The plugin is **linked**, in the open repository: registering a service the `operations` provider injects, reading environment variables, and attaching a non-OAuth credential are three things the loaded contract cannot express today ([plugins](plugins.md), open questions). It is open source because the moat rule holds — what is private is the console's provisioning, not the plugin or its seam — and because a self-hoster with a Twilio account gets the identical feature. When operations and static credentials cross into WIT, this is the first candidate to move.

## What is lost, stated plainly

- Audio before the merge. Capture begins when the owner merges, and the design does not pretend otherwise: the hint and the sidecar carry the recording's own start.
- Speaker separation on the owner's side. The carrier hands the node's leg the humans already mixed; the transcript attributes by channel, and the node's own leg is silent.
- The consent notice is heard by whoever answers the second call, before the merge. Participants added afterward hear only what the owner tells them; jurisdictions differ, and the notice text is configuration for that reason.
- A screened callback. Saving the capture number as a contact is the mitigation; dialing it directly is the fallback.

## Cost sketch

Two legs per capture (the provider's call to the owner's phone, and the recording), a per-minute recording charge, a per-minute transcription charge, and a monthly number. An hour-long call is a few dollars at list price, dominated by transcription; the flat hosted rate does not cover it today (open below). The archive grows at MP3's 32 kbit/s — about 14 MiB per hour — well inside a plan's index allowance, and each recording download is capped at 64 MiB, which covers the four-hour ceiling with headroom.

## Paths not taken

- **Media Streams / live transcription.** A WebSocket the node would have to serve, for audio that is transcribed after the call anyway; the loaded tier could never express it, and the linked tier gains nothing from it today.
- **The master Twilio credentials on every node.** One compromised node would read every tenant's calls; the design already rejects the equivalent Hetzner token for the same reason.
- **A per-tenant Twilio account (not subaccount).** Separate billing relationships for each tenant; subaccounts give the isolation and keep one bill.
- **SMS verification codes.** US A2P registration for every tenant number is a compliance process the voice code sidesteps, and the node already knows how to place a call.
- **Verifying by Twilio Verify.** Another service per subaccount for what one call with inline TwiML already does.
- **A loaded plugin now.** The three contract gaps above; noted as the first plugin to migrate when they close.
- **Provisioning the number from the node.** Spending money as a side effect of mount, with a master credential on the node. The console provisions, once, with consent already given at checkout.

## Open questions

- **Metering call minutes.** The hosted plan meters index gigabytes; minutes are a new line item that scales with use, not with volume. A per-minute pass-through or a monthly allowance both fit the existing meter shape.
- **Recording encryption at the provider.** Twilio can encrypt recordings with a customer public key so even the master account reads ciphertext; with delete-after-ingest the window is minutes, but a key the node minted would close it entirely. Unverified against the current API and left for the experiment below.
- **Speaker attribution.** Dual-channel recording separates only the node's leg from everyone else; whether the provider's diarization on the mixed channel is worth its cost is a measurement.
- **The experiment.** Phone-call-context's callback gate stands: six calls across two carriers, measuring merge success, screening, notice audibility after the merge, join delay, and the marked missing start. This document adopts the architecture; the gate decides the product claim.
