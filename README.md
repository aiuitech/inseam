# inseam

`inseam` is an extensible, plugin-driven framework for unifying and discovering data across devices, platforms, and services. The aim is to make any data you have accessible from anywhere.

Architected as a distributed file system for AI, it gives agents and applications a consistent, permission-aware way to reach the context they need — wherever that data lives. `inseam` is fast and lean, storing only addresses of data and a lightweight discovery index over them, never the source content itself; original material is fetched on demand from where it already lives.

An inseam host is simply somewhere data lives — a laptop's filesystem, a Gmail account, a service's API. Hosts join your inseam network through nodes: running inseam instances that publish their hosts' data addresses and serve fetches on their behalf. Each node is local first so it can operate offline and on-device. When connected, the network syncs all data addresses so every node is aware of what data is where and who may access it.

Discovery runs against the index first and progressively retrieves source material only when it improves the answer, making inseam a scalable personal context engine for AI systems that need awareness of everything.
