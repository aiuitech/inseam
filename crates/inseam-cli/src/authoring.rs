//! The plugin author's companion commands (`docs/plugins/authoring-cli.md`):
//! what the node already knows, handed to whoever is writing a plugin
//! against it — an agent in the usual case. Each answers one question in the
//! loop without a source checkout or a network:
//!
//! - `inseam seams [--wit]` — which seams accept loaded plugins and under
//!   what contract (the embedded WIT, printable for binding generation);
//! - `inseam capabilities` — what a manifest may request, and whether *this*
//!   node can honor each right now;
//! - `inseam claims <mimetype|path>` — who on this node already claims an
//!   input, so a new plugin can complement instead of duplicate;
//! - `inseam plugin new` — a scaffold whose first `inseam plugin check` is
//!   red for the right reason;
//! - `inseam plugin try` — apply an artifact to one real file through the
//!   harness bridge and see what comes out, optionally as a golden check;
//! - `inseam plugin mount` — put a local artifact into the composition.
//!
//! None of these boot the kernel except `capabilities` and `claims`, which
//! are questions about the live node by definition.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

use inseam_kernel::fragment::Mimetype;
use inseam_kernel::substrate::Kernel;
use inseam_plugins::connection_fs::detect_mimetype;
use inseam_seams::llm::{self, LLM};
use inseam_seams::transforms::TRANSFORMS;
use inseam_wasm_host::{try_artifact, TryInput, TryOutcome, TRANSFORM_WIT};

/// Inputs longer than this are not inlined into a generated check — a
/// golden check is a small, readable example, not a corpus.
const CHECK_INLINE_TEXT_CHARS_MAX: usize = 2_000;

// ---------------------------------------------------------------------------
// seams
// ---------------------------------------------------------------------------

pub fn seams(wit: bool) {
    if wit {
        print!("{TRANSFORM_WIT}");
        return;
    }
    println!("Seams that accept loaded plugins (manifest `seam = ...`):\n");
    println!("  transform   WIT world `transform-plugin` (inseam:plugin@0.1.0)");
    println!("              exports  claims() -> claim-spec; apply(env, mimetype, is-root, text) -> output");
    println!("              imports  log                 always");
    println!("                       llm-complete        needs [capabilities] llm = true");
    println!("                       llm-describe-image  needs [capabilities] llm = true");
    println!("                       source-bytes        needs [capabilities] source_bytes = true (root only)");
    println!("              output   child fragments only (no keyed sprouts); parent indexes an EARLIER fragment;");
    println!("                       relation: contains | derives (kernel) or your own kebab-case kind;");
    println!("                       inseam-defined mimetypes (text/x-inseam-*) are refused");
    println!("              rules    degrade, never trap; fresh instance per call; fuel-metered");
    println!();
    println!("`inseam seams --wit` prints the WIT to generate bindings from.");
    println!("`inseam capabilities` says what this node can grant right now.");
}

// ---------------------------------------------------------------------------
// capabilities
// ---------------------------------------------------------------------------

pub fn capabilities(kernel: &Kernel) {
    println!("Capabilities a manifest may request, and this node's status for each:\n");
    match kernel.service(&LLM) {
        Ok(_) => {
            let facts = kernel.facts(&LLM);
            let fact = |key: &str| -> String {
                facts
                    .and_then(|f| f.get(key))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| "(unset)".into())
            };
            println!("  llm = true          GRANTABLE — llm seam bound");
            println!("                      transform model  {}", fact(llm::facts::TRANSFORM_MODEL));
            println!("                      agent model      {}", fact(llm::facts::AGENT_MODEL));
            println!("                      grants llm-complete and llm-describe-image, metered per run");
        }
        Err(e) => {
            println!("  llm = true          NOT GRANTABLE on this node — {e}");
            println!("                      a plugin requesting it mounts, but every call refuses;");
            println!("                      it runs its degrade path here (what your starved check pins)");
        }
    }
    println!("  llm_call_budget     LLM calls per index run charged to the plugin (0 = unlimited by it;");
    println!("                      the node's own guards still apply). Inert without llm = true.");
    println!("  source_bytes = true GRANTABLE — the filesystem host serves raw bytes at the root");
    println!("                      (a fragment below the root never gets bytes)");
    match kernel.service(&TRANSFORMS) {
        Ok(registry) => {
            let count = registry.snapshot().len();
            println!("\n  transforms seam     bound; {count} transform(s) registered — `inseam claims <mimetype>` lists who claims what");
        }
        Err(e) => println!("\n  transforms seam     not bound — {e}; a loaded transform cannot register"),
    }
}

// ---------------------------------------------------------------------------
// claims
// ---------------------------------------------------------------------------

/// A path is read for its detected mimetype; anything else must parse as one.
fn mimetype_of(target: &str) -> anyhow::Result<Mimetype> {
    let path = Path::new(target);
    if path.exists() {
        Ok(detect_mimetype(path))
    } else {
        Mimetype::parse(target)
            .with_context(|| format!("`{target}` is neither an existing file nor a mimetype"))
    }
}

pub fn claims(kernel: &Kernel, target: &str) -> anyhow::Result<()> {
    let mimetype = mimetype_of(target)?;
    let registry = kernel.service(&TRANSFORMS)?;
    println!("{target} → {mimetype}\n");
    let mut any = false;
    for registration in registry.snapshot() {
        let at_root = registration.transform.claims(&mimetype, true);
        let below_root = registration.transform.claims(&mimetype, false);
        if !at_root && !below_root {
            continue;
        }
        any = true;
        let scope = match (at_root, below_root) {
            (true, true) => "root + fragments",
            (true, false) => "root only",
            (false, true) => "fragments only",
            (false, false) => unreachable!("filtered above"),
        };
        println!(
            "  {:18} {:<11} {:<17} entry `{}`",
            registration.name,
            format!("{:?}", registration.transform.kind()).to_lowercase(),
            scope,
            registration.entry_id
        );
    }
    if !any {
        println!("  nothing on this node claims it — a plugin claiming it would be the first");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// plugin new
// ---------------------------------------------------------------------------

pub struct Scaffold<'a> {
    pub name: &'a str,
    pub seam: &'a str,
    pub claims: &'a [String],
    pub dir: &'a Path,
}

fn scaffold_validate(s: &Scaffold<'_>) -> anyhow::Result<()> {
    if s.seam != "transform" {
        bail!("seam `{}` accepts no loaded plugins; `inseam seams` lists those that do", s.seam);
    }
    let name_ok = !s.name.is_empty()
        && s
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !name_ok {
        bail!("plugin name `{}` must be lowercase ascii, digits, and dashes", s.name);
    }
    if s.claims.is_empty() {
        bail!("pass at least one --claims mimetype (e.g. --claims image/png,image/jpeg)");
    }
    for claim in s.claims {
        let well_formed = match claim.strip_suffix("/*") {
            Some(kind) => !kind.is_empty() && !kind.contains('/'),
            None => claim
                .split_once('/')
                .is_some_and(|(kind, sub)| !kind.is_empty() && !sub.is_empty()),
        };
        if !well_formed {
            bail!("claim `{claim}` is not a mimetype essence or `type/*` pattern");
        }
    }
    Ok(())
}

/// Whether any claim is text-shaped, which decides whether the scaffolded
/// positive check hands in `text` or a `bytes_file`.
fn claims_are_textual(claims: &[String]) -> bool {
    claims.iter().any(|c| {
        Mimetype::parse(c.strip_suffix("/*").unwrap_or(c))
            .map(|m| inseam_seams::text::is_indexable_text(&m))
            .unwrap_or(c.starts_with("text/"))
    })
}

fn scaffold_manifest(s: &Scaffold<'_>) -> String {
    let claims = s
        .claims
        .iter()
        .map(|c| format!("{c:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# Reviewed by the node owner; enforced by the bridge (docs/plugins/loaded.md).\n\
         name = {name:?}\n\
         version = \"0.1.0\"\n\
         seam = \"transform\"\n\
         claims = [{claims}]      # effective claims = this ∩ what claims() exports\n\
         roots_only = true\n\
         kind = \"enrichment\"     # `structural` if you decompose the source itself\n\
         \n\
         [capabilities]            # request the MINIMUM you use; widening later re-gates approval\n\
         llm = false               # llm-complete / llm-describe-image\n\
         source_bytes = false      # source-bytes (raw bytes at the root)\n\
         llm_call_budget = 0       # LLM calls per index run, when llm = true\n",
        name = s.name
    )
}

fn scaffold_checks(s: &Scaffold<'_>) -> String {
    let first = &s.claims[0];
    let example_mimetype = first.strip_suffix("/*").map(|t| format!("{t}/example")).unwrap_or(first.clone());
    let input = if claims_are_textual(s.claims) {
        "text = \"REPLACE WITH A SMALL EXAMPLE INPUT\"".to_string()
    } else {
        format!(
            "bytes_file = \"fixtures/example.bin\"   # add a tiny, well-formed fixture and a row in fixtures/README.md\n\
             # (needs [capabilities] source_bytes = true in {}.manifest.toml)",
            s.name
        )
    };
    format!(
        "# Golden checks for `{name}` (docs/plugins/validation.md): example inputs and the\n\
         # output shape this plugin promises. Mandatory coverage, enforced by\n\
         # `inseam plugin check`: one check that PROVES THE CLAIM (a substantive\n\
         # expectation) and one STARVED check (no text, no llm_returns) that pins the\n\
         # degrade path with max_fragments. Write these before the code; they are the\n\
         # spec. The first `inseam plugin check` is red on the first check until the\n\
         # plugin does what it says.\n\
         \n\
         [[check]]\n\
         name = \"REPLACE: what this plugin does, in one sentence\"\n\
         mimetype = {mimetype:?}\n\
         {input}\n\
         # llm_returns = \"REPLACE with the kind of reply your prompt asks for\"   # needs llm = true\n\
         \n\
         [check.expect]\n\
         min_fragments = 1\n\
         fragment_contains = \"REPLACE WITH TEXT YOUR PLUGIN EMITS\"\n\
         relation = \"REPLACE: contains | derives | your-own-kind\"\n\
         # mimetype = \"text/plain\"   # prefix of an emitted fragment's mimetype\n\
         \n\
         # The degrade path: no content, no LLM. Decide what happens — nothing is the\n\
         # usual answer — and pin it.\n\
         [[check]]\n\
         name = \"emits nothing when starved\"\n\
         mimetype = {mimetype:?}\n\
         \n\
         [check.expect]\n\
         min_fragments = 0\n\
         max_fragments = 0\n",
        name = s.name,
        mimetype = example_mimetype,
    )
}

fn scaffold_cargo_toml(s: &Scaffold<'_>) -> String {
    format!(
        "[package]\nname = {:?}\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         # Standalone: not a member of any surrounding workspace.\n[workspace]\n\n\
         [lib]\ncrate-type = [\"cdylib\"]\n\n\
         [dependencies]\nwit-bindgen = \"0.60\"\n\n\
         [profile.release]\nopt-level = \"s\"\nlto = true\nstrip = true\n",
        s.name
    )
}

fn scaffold_lib_rs(s: &Scaffold<'_>) -> String {
    let claims = s
        .claims
        .iter()
        .map(|c| format!("                {c:?}.into(),"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "//! `{name}`: REPLACE with what this plugin does and why (one paragraph).\n\
         //! Degrades to empty output whenever a capability is withheld or the\n\
         //! input is unusable — never an error, never a panic.\n\
         \n\
         wit_bindgen::generate!({{\n    path: \"wit\",\n    world: \"transform-plugin\",\n}});\n\
         \n\
         use exports::inseam::plugin::transform::{{ClaimSpec, Envelope, Fragment, Guest, Output}};\n\
         #[allow(unused_imports)]\nuse inseam::plugin::host;\n\
         \n\
         struct Plugin;\n\
         \n\
         impl Guest for Plugin {{\n\
         \x20   /// Must overlap the manifest's `claims` — the bridge runs the intersection.\n\
         \x20   fn claims() -> ClaimSpec {{\n\
         \x20       ClaimSpec {{\n\
         \x20           mimetypes: vec![\n{claims}\n            ],\n\
         \x20           roots_only: true,\n\
         \x20       }}\n\
         \x20   }}\n\
         \n\
         \x20   fn apply(\n\
         \x20       _env: Envelope,\n\
         \x20       _mimetype: String,\n\
         \x20       _is_root: bool,\n\
         \x20       _text: Option<String>,\n\
         \x20   ) -> Result<Output, String> {{\n\
         \x20       let empty = Output {{ fragments: vec![] }};\n\
         \x20       // Host imports return Err when the manifest did not request them or\n\
         \x20       // the budget is spent — treat every Err as \"degrade\":\n\
         \x20       //   let Ok(bytes) = host::source_bytes() else {{ return Ok(empty) }};\n\
         \x20       //   let Ok(reply) = host::llm_complete(SYSTEM, &user) else {{ return Ok(empty) }};\n\
         \x20       // Emit fragments; `parent` indexes an EARLIER fragment in this list,\n\
         \x20       // None hangs it off the claimed source:\n\
         \x20       //   Ok(Output {{ fragments: vec![Fragment {{ parent: None,\n\
         \x20       //       mimetype: \"text/plain;via={name}\".into(), relation: \"derived-from\".into(),\n\
         \x20       //       text: Some(reply) }}] }})\n\
         \x20       let _ = Fragment {{ parent: None, mimetype: String::new(), relation: String::new(), text: None }};\n\
         \x20       Ok(empty)\n\
         \x20   }}\n\
         }}\n\
         \n\
         export!(Plugin);\n",
        name = s.name,
    )
}

fn scaffold_readme(s: &Scaffold<'_>) -> String {
    format!(
        "# {name}\n\nREPLACE: what it does, what it needs (capabilities and why), what it emits.\n\n\
         ## Develop\n\n```sh\n\
         cargo build --release --target wasm32-wasip2 && cp target/wasm32-wasip2/release/{crate}.wasm {name}.wasm\n\
         inseam plugin check {name}.wasm            # the gate: must end PASS\n\
         inseam plugin try {name}.wasm <file>       # what it emits for one real file\n\
         ```\n\n## Mount\n\n```sh\ninseam plugin mount $PWD/{name}.wasm      # appends the entry below to the composition\n```\n\n\
         ```toml\n[[entry]]\nid = {name:?}\nplugin = \"wasm:<path>/{name}.wasm\"\n```\n",
        name = s.name,
        crate = s.name.replace('-', "_"),
    )
}

const FIXTURES_README: &str = "# Fixtures\n\nByte fixtures referenced by the checks file (`bytes_file`), handed to the\nplugin as `source-bytes` during `inseam plugin check`. Keep them tiny and\nwell-formed; the harness's LLM is canned, so content never matters — only\nthat bytes reach the plugin and come back shaped right. Every installing\nnode downloads them.\n\n| File | What | Why |\n| --- | --- | --- |\n";

pub fn plugin_new(s: &Scaffold<'_>) -> anyhow::Result<PathBuf> {
    scaffold_validate(s)?;
    let root = s.dir.join(s.name);
    if root.exists() {
        bail!("{} already exists; refusing to overwrite", root.display());
    }
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::create_dir_all(root.join("wit"))?;
    std::fs::create_dir_all(root.join("fixtures"))?;
    let files: [(PathBuf, String); 7] = [
        (root.join(format!("{}.manifest.toml", s.name)), scaffold_manifest(s)),
        (root.join(format!("{}.checks.toml", s.name)), scaffold_checks(s)),
        (root.join("Cargo.toml"), scaffold_cargo_toml(s)),
        (root.join("src/lib.rs"), scaffold_lib_rs(s)),
        (root.join("wit/transform.wit"), TRANSFORM_WIT.to_string()),
        (root.join("README.md"), scaffold_readme(s)),
        (root.join("fixtures/README.md"), FIXTURES_README.to_string()),
    ];
    for (path, content) in &files {
        std::fs::write(path, content).with_context(|| path.display().to_string())?;
    }
    Ok(root)
}

pub fn print_scaffold_next_steps(root: &Path, name: &str) {
    println!("scaffolded {}\n", root.display());
    println!("next — the loop (skills/inseam-loaded-plugin):");
    println!("  1. edit {name}.checks.toml: replace every REPLACE with the behavior you promise");
    println!("  2. edit {name}.manifest.toml: request only the capabilities you will call");
    println!("  3. rustup target add wasm32-wasip2   # once; then:");
    println!("     cd {name} && cargo build --release --target wasm32-wasip2 \\");
    println!("       && cp target/wasm32-wasip2/release/{}.wasm {name}.wasm", name.replace('-', "_"));
    println!("  4. inseam plugin check {name}.wasm      # red on your first check — that is the to-do");
    println!("  5. implement src/lib.rs until it ends PASS; `inseam plugin try {name}.wasm <file>` shows output");
    println!("  6. inseam plugin mount $PWD/{name}.wasm && inseam plugins && inseam index <dir>");
}

// ---------------------------------------------------------------------------
// plugin try
// ---------------------------------------------------------------------------

pub struct TryRequest<'a> {
    pub artifact: &'a Path,
    pub file: &'a Path,
    pub mimetype: Option<&'a str>,
    pub llm_returns: Option<&'a str>,
    pub not_root: bool,
}

/// Build the input exactly as the sweep would: detected mimetype, text only
/// when the type is indexable text, bytes always on offer (the bridge
/// withholds them unless the manifest asks).
fn try_input(request: &TryRequest<'_>) -> anyhow::Result<TryInput> {
    let mimetype = match request.mimetype {
        Some(m) => Mimetype::parse(m).with_context(|| format!("--mimetype `{m}`"))?,
        None => detect_mimetype(request.file),
    };
    let bytes = std::fs::read(request.file).with_context(|| request.file.display().to_string())?;
    let text = inseam_seams::text::is_indexable_text(&mimetype)
        .then(|| String::from_utf8_lossy(&bytes).into_owned());
    Ok(TryInput {
        mimetype: mimetype.to_string(),
        is_root: !request.not_root,
        text,
        bytes: Some(bytes),
        llm_returns: request.llm_returns.map(str::to_string),
    })
}

fn print_try_outcome(input: &TryInput, outcome: &TryOutcome) {
    println!(
        "input   {}  root={}  text={}  bytes={}  llm={}",
        input.mimetype,
        input.is_root,
        input.text.as_ref().map_or("none".to_string(), |t| format!("{} chars", t.chars().count())),
        input.bytes.as_ref().map_or(0, Vec::len),
        if input.llm_returns.is_some() { "canned" } else { "refuses" }
    );
    for note in &outcome.notes {
        println!("note    {note}");
    }
    if let Some(e) = &outcome.plugin_error {
        println!("plugin returned Err({e:?}) — tolerated, but prefer Ok with no fragments");
    }
    println!("output  {} fragment(s)", outcome.fragments.len());
    for (i, f) in outcome.fragments.iter().enumerate() {
        let parent = f.parent.map_or("source".to_string(), |p| format!("#{p}"));
        println!("  #{i:<3} {:<28} {:<13} parent {parent}", f.mimetype, f.relation);
        if let Some(text) = &f.text {
            for line in text.lines().take(8) {
                println!("       │ {line}");
            }
            if text.lines().count() > 8 {
                println!("       │ … ({} chars total)", text.chars().count());
            }
        }
    }
}

/// Snapshot the observed output as a golden check the author can paste and
/// then tighten — the fastest route from "I saw it work" to "it is tested".
fn print_as_check(request: &TryRequest<'_>, input: &TryInput, outcome: &TryOutcome) {
    println!("\n# --- paste into <name>.checks.toml, then tighten the expectations ---");
    println!("[[check]]");
    println!("name = \"REPLACE: what this proves\"");
    println!("mimetype = {:?}", input.mimetype);
    if !input.is_root {
        println!("is_root = false");
    }
    match &input.text {
        Some(t) if t.chars().count() <= CHECK_INLINE_TEXT_CHARS_MAX => {
            println!("text = {}", toml_string(t));
        }
        Some(_) => println!(
            "# text = ...   # {} is too large to inline; cut a small example that shows the same behavior",
            request.file.display()
        ),
        None => {
            let fixture = request.file.file_name().map(|n| n.to_string_lossy().into_owned());
            println!(
                "bytes_file = \"fixtures/{}\"   # copy the file into fixtures/ (keep it tiny) and document it in fixtures/README.md",
                fixture.unwrap_or_else(|| "example.bin".into())
            );
        }
    }
    if let Some(reply) = &input.llm_returns {
        println!("llm_returns = {}", toml_string(reply));
    }
    println!("\n[check.expect]");
    println!("min_fragments = {}", outcome.fragments.len().min(1));
    println!("max_fragments = {}", outcome.fragments.len());
    if let Some(first) = outcome.fragments.first() {
        println!("relation = {:?}", first.relation);
        println!("mimetype = {:?}", first.mimetype);
        if let Some(word) = first
            .text
            .as_deref()
            .and_then(|t| t.split_whitespace().find(|w| w.chars().count() >= 4))
        {
            println!("fragment_contains = {word:?}   # REPLACE with the text that proves the claim");
        }
    }
}

/// A TOML string literal: basic for one line, multi-line basic otherwise.
fn toml_string(value: &str) -> String {
    if value.contains('\n') {
        format!("\"\"\"\n{}\"\"\"", value.replace("\"\"\"", "\\\"\"\""))
    } else {
        format!("{value:?}")
    }
}

pub async fn plugin_try(request: &TryRequest<'_>, as_check: bool) -> anyhow::Result<()> {
    let input = try_input(request)?;
    let outcome = try_artifact(request.artifact, input.clone())
        .await
        .map_err(|e| anyhow::anyhow!("{}: {e}", request.artifact.display()))?;
    print_try_outcome(&input, &outcome);
    if as_check {
        print_as_check(request, &input, &outcome);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// plugin mount
// ---------------------------------------------------------------------------

pub fn plugin_mount(artifact: &Path, id: Option<&str>, composition_path: &Path) -> anyhow::Result<()> {
    let artifact = artifact
        .canonicalize()
        .with_context(|| format!("{} does not exist", artifact.display()))?;
    let stem = artifact
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .context("artifact has no file name")?;
    let id = id.unwrap_or(&stem);
    crate::registry::mount(id, &artifact, composition_path)?;
    println!("mounted `{id}` → {}\nin {}", artifact.display(), composition_path.display());
    println!("next: `inseam plugins` (fiber `{id}` should be active), then `inseam index <dir>`");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scaffold<'a>(claims: &'a [String], dir: &'a Path) -> Scaffold<'a> {
        Scaffold {
            name: "demo-plugin",
            seam: "transform",
            claims,
            dir,
        }
    }

    #[test]
    fn scaffold_rejects_bad_names_seams_and_claims() {
        let dir = tempfile::tempdir().expect("tempdir");
        let claims = vec!["image/png".to_string()];
        let mut s = scaffold(&claims, dir.path());
        s.name = "Demo Plugin";
        assert!(scaffold_validate(&s).is_err());
        s.name = "demo";
        s.seam = "finder";
        assert!(scaffold_validate(&s).is_err());
        s.seam = "transform";
        let bad = vec!["png".to_string()];
        s.claims = &bad;
        assert!(scaffold_validate(&s).is_err());
        let none: Vec<String> = Vec::new();
        s.claims = &none;
        assert!(scaffold_validate(&s).is_err());
    }

    #[test]
    fn scaffold_writes_every_file_and_its_checks_meet_the_coverage_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let claims = vec!["text/plain".to_string()];
        let root = plugin_new(&scaffold(&claims, dir.path())).expect("scaffolds");
        for file in [
            "demo-plugin.manifest.toml",
            "demo-plugin.checks.toml",
            "Cargo.toml",
            "src/lib.rs",
            "wit/transform.wit",
            "README.md",
            "fixtures/README.md",
        ] {
            assert!(root.join(file).exists(), "{file}");
        }
        let checks = std::fs::read_to_string(root.join("demo-plugin.checks.toml")).expect("reads");
        let parsed = inseam_conformance::ChecksFile::parse(&checks).expect("scaffolded checks parse");
        assert!(parsed.required_coverage().is_ok(), "the scaffold ships the mandatory coverage shape");
        assert!(checks.contains("text = \"REPLACE"), "textual claims get an inline text input");
        let manifest: inseam_wasm_host::ArtifactManifest =
            toml::from_str(&std::fs::read_to_string(root.join("demo-plugin.manifest.toml")).expect("reads"))
                .expect("scaffolded manifest parses");
        assert_eq!(manifest.claims, claims);
        assert!(plugin_new(&scaffold(&claims, dir.path())).is_err(), "refuses to overwrite");
    }

    #[test]
    fn scaffold_offers_a_fixture_for_binary_claims() {
        let dir = tempfile::tempdir().expect("tempdir");
        let claims = vec!["image/*".to_string()];
        let root = plugin_new(&scaffold(&claims, dir.path())).expect("scaffolds");
        let checks = std::fs::read_to_string(root.join("demo-plugin.checks.toml")).expect("reads");
        assert!(checks.contains("bytes_file = \"fixtures/example.bin\""));
        assert!(checks.contains("mimetype = \"image/example\""));
    }

    #[test]
    fn toml_strings_go_multiline_only_when_needed() {
        assert_eq!(toml_string("one line"), "\"one line\"");
        assert!(toml_string("two\nlines").starts_with("\"\"\"\n"));
    }
}
