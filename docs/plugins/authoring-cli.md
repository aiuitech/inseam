# The Authoring CLI

The `inseam` binary doubles as the plugin author's companion: the node you are extending describes its own contract, capabilities, and current plugins, scaffolds a plugin, runs it against real files, validates it, and mounts it — no source checkout, no network. It exists because the authors inseam designs for are agents, and an agent that can *ask the node* needs to guess nothing ([design/plugins.md](../../design/plugins.md)). The loop itself is the skill at `skills/inseam-loaded-plugin/SKILL.md` ([../skills/inseam-loaded-plugin.md](../skills/inseam-loaded-plugin.md)); this page is the command reference.

| Command | Boots the node? | What it answers |
| --- | --- | --- |
| `inseam seams [--wit]` | no | Which seams accept loaded plugins, their exports, imports and the manifest key gating each, output rules. `--wit` prints the embedded WIT world — redirect it to `wit/transform.wit` to generate bindings. |
| `inseam capabilities` | yes | Each capability a manifest may request (`llm`, `source_bytes`, `llm_call_budget`), what it grants, and whether *this* node can grant it right now (LLM bound? which transform/agent model?). A plugin whose capability the node cannot grant still mounts — it runs its degrade path, which is what its starved golden check pins. |
| `inseam claims <mimetype\|path>` | yes | Every transform registered on this node that claims the input (a path is read for its detected mimetype), with kind, root/fragment scope, and entry id — so a new plugin complements rather than duplicates. |
| `inseam plugin new <name> --claims a/b,c/*` | no | Scaffolds `<name>/`: manifest, golden checks already in the mandatory-coverage shape (the positive check is red until the plugin does what it says; the starved check passes on the stub), a degrading Rust stub, the WIT, and READMEs. Any language works from here; the Rust stub is a template. |
| `inseam plugin try <artifact> <file> [--mimetype] [--llm-returns "…"] [--not-root] [--as-check]` | no | Applies the artifact once to one real file through the harness bridge — detected mimetype, text when the type is text, bytes on offer, canned LLM — and prints every fragment plus notes on what the bridge withheld and why. `--as-check` prints the observed output as a `[[check]]` to paste and tighten. |
| `inseam plugin check <artifact>` | no | The conformance harness ([validation.md](validation.md)). |
| `inseam plugin mount <artifact> [--id]` | no | Appends `[[entry]] plugin = "wasm:<path>"` to the node's composition (the same write `plugin install` does). |
| `inseam plugins`, `index`, `status`, `query`, `expand` | yes | The live proof: the fiber's state and effects, then the fragments the plugin actually hangs off sources. |

`try` and `check` share one bridge and one fake-capability set, so what `try` shows is what a golden check would see; `check` and admission share one harness, so what passes locally passes on every node.
