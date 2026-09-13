# Inseam Design

This directory is the architectural design docs for inseam. Design docs are the **intent** driving the architecture. Each file documents design decisions made. They're considered full and up to date at all times. Old ideas and retired decisions continue to live in the git history  and/or as explicit decisions on paths NOT taken and why.

One file per concept.

Current concepts include [network](network.md), which defines the graph of nodes as one owner's trust domain where addresses and reachability are global and sessions are local, [connections](connections.md), which defines the two kinds of edge — node→host through the connections registry, node↔node through the iroh transport — [roster](roster.md), which defines the synced self-description of the network — nodes, hosts, stewards, admission, invitations, expulsion — [address-sync](address-sync.md), which defines the per-origin log, its epochs and compaction, and the exchange that converges every node, [discovery](discovery.md), which defines per-node indexes and the fan-out that merges them, [ios-app](ios-app.md), which defines the phone as a leaf steward for the hosts only it can reach (Photos, Apple's call recordings) with on-device models in place of API keys, [benchmarking](benchmarking.md), which defines how external corpora, fresh indexes, model assignments, and recorded runs produce comparable results, [hosted-service](hosted-service.md), which defines how the commercial offering provisions, isolates, updates, and meters tenant nodes without gaining any power over them, and [releases](releases.md), which defines how a signed manifest at any origin delivers a binary to the stock CLI, a mirror, or a private distribution through one `inseam self update`.

Research supporting the iOS design: [phone-call-context](phone-call-context.md) compares Apple recording imports, Mac capture, carrier recording, and programmable calling. Its alternatives are proposals with explicit validation gates, not adopted architecture.

[In-person meeting recording](meeting-recording.md) defines lossless stereo capture, mono fallback, capture metadata, and interruption handling in the iOS app.

[Vocabulary](vocabulary.md) is a proposal with validation gates: the corpus's own terms mined from the index's statistics, grouped into clusters the model grounds once each, so documents and questions are read in the corpus's words and hub terms are bounded rather than damped.
