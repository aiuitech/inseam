# Plugins

Everything in inseam is a plugin. Two tiers: [linked](linked.md) (first-party Rust, compiled into a distribution) and [loaded](loaded.md) (WASM components mounted at runtime against the [WIT contract](transform-plugin-wit.md)). [Validation](validation.md) is the shared quality gate, the [authoring CLI](authoring-cli.md) is the author's companion (ask the node, scaffold, try, check, mount), the [registry](registry.md) distributes loaded plugins, and [distributions](distributions.md) covers building custom binaries from source.
