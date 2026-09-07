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
//!   red for the right reason, on either seam;
//! - `inseam plugin try` — apply a transform to one real file through the
//!   harness bridge, or make one live call on a connection, and see what
//!   comes out;
//! - `inseam plugin mount` — put a local artifact into the composition.
//!
//! None of these boot the kernel except `capabilities` and `claims`, which
//! are questions about the live node by definition.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use inseam_kernel::fragment::Mimetype;
use inseam_kernel::substrate::Kernel;
use inseam_plugins::connection_fs::detect_mimetype;
use inseam_seams::connection::{CONNECTIONS, HostKind};
use inseam_seams::llm::{self, LLM};
use inseam_seams::oauth::OAUTH;
use inseam_seams::transforms::TRANSFORMS;
use inseam_wasm_host::{
    TryCall, TryConnectionOutcome, TryInput, TryOutcome, plugin_wit, try_artifact, try_connection,
};

/// Inputs longer than this are not inlined into a generated check — a
/// golden check is a small, readable example, not a corpus.
const CHECK_INLINE_TEXT_CHARS_MAX: usize = 2_000;

/// Config pairs one `plugin try` may carry.
const TRY_CONFIG_PAIRS_MAX: usize = 64;

// ---------------------------------------------------------------------------
// seams
// ---------------------------------------------------------------------------

pub fn seams(wit: bool) {
    if wit {
        print!("{}", plugin_wit());
        return;
    }
    println!(
        "Seams that accept loaded plugins (manifest `seam = ...`), package inseam:plugin@0.1.0:\n"
    );
    println!("  transform   WIT world `transform-plugin` — per-call instantiation");
    println!(
        "              exports  claims() -> claim-spec; apply(env, mimetype, is-root, text) -> output"
    );
    println!("              imports  log                 always");
    println!("                       llm-complete        needs [capabilities] llm = true");
    println!("                       llm-describe-image  needs [capabilities] llm = true");
    println!(
        "                       source-bytes        needs [capabilities] source_bytes = true (root only)"
    );
    println!(
        "                       fetch               needs [capabilities] hosts = [\"api.example.com\", \"*.cdn.example\"]"
    );
    println!(
        "              output   child fragments only (no keyed sprouts); parent indexes an EARLIER fragment;"
    );
    println!(
        "                       relation: contains | derives (kernel) or your own kebab-case kind;"
    );
    println!("                       inseam-defined mimetypes (text/x-inseam-*) are refused");
    println!("              rules    degrade, never trap; fresh instance per call; fuel-metered");
    println!();
    println!("  connection  WIT world `connection-plugin` — one long-running instance per entry");
    println!(
        "              exports  configure(toml); describe-host() -> kind + principal + display-name;"
    );
    println!(
        "                       capabilities(); enumerate(root) -> sources; locator-prefix(root);"
    );
    println!("                       read-bytes(locator); describe(locator) -> envelope");
    println!("              imports  log                 always");
    println!(
        "                       fetch               needs [capabilities] hosts = [...]; the node performs it"
    );
    println!(
        "                       fetch.authorize     needs [capabilities] grant = true and `grant = \"<id>\"` on the entry:"
    );
    println!(
        "                                           the node attaches the bearer; the token never crosses"
    );
    println!(
        "              manifest host_kind = \"<kind>\" (identity: must equal what describe-host exports);"
    );
    println!(
        "                       [connection] enumerates / change_feed / writable (effective = declared AND exported)"
    );
    println!(
        "              rules    the bridge derives the host id from kind + principal; locators are yours;"
    );
    println!(
        "                       an error is the offline answer (never a trap); a trap discards the instance"
    );
    println!();
    println!("`inseam seams --wit > wit/plugin.wit` prints the package to generate bindings from.");
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
            println!(
                "                      transform model  {}",
                fact(llm::facts::TRANSFORM_MODEL)
            );
            println!(
                "                      agent model      {}",
                fact(llm::facts::AGENT_MODEL)
            );
            println!(
                "                      grants llm-complete and llm-describe-image, metered per run"
            );
        }
        Err(e) => {
            println!("  llm = true          NOT GRANTABLE on this node — {e}");
            println!(
                "                      a plugin requesting it mounts, but every call refuses;"
            );
            println!(
                "                      it runs its degrade path here (what your starved check pins)"
            );
        }
    }
    println!(
        "  llm_call_budget     LLM calls per index run charged to the plugin (0 = unlimited by it;"
    );
    println!("                      the node's own guards still apply). Inert without llm = true.");
    println!("  source_bytes = true GRANTABLE — the filesystem host serves raw bytes at the root");
    println!("                      (a fragment below the root never gets bytes)");
    println!(
        "  hosts = [...]       GRANTABLE — `fetch` reaches exactly these hosts (exact or `*.domain`),"
    );
    println!(
        "                      through the node's guard: public addresses only unless named, every"
    );
    println!(
        "                      redirect hop re-checked, body capped, timed out. Empty = no network."
    );
    println!("                      Adding a host later is capability widening (needs allow_new).");
    match kernel.service(&OAUTH) {
        Ok(oauth) => {
            let ids: Vec<String> = oauth.grants().iter().map(|g| g.id().to_string()).collect();
            println!(
                "  grant = true        GRANTABLE — oauth seam bound; grants this node holds: {}",
                if ids.is_empty() {
                    "(none yet; docs/plugins/oauth.md)".to_string()
                } else {
                    ids.join(", ")
                }
            );
            println!(
                "                      the entry names one (`grant = \"<id>\"`); `authorize` attaches its bearer"
            );
        }
        Err(e) => {
            println!("  grant = true        NOT GRANTABLE on this node — {e}");
            println!(
                "                      `authorize` requests refuse; unauthenticated fetches still work"
            );
        }
    }
    match kernel.service(&TRANSFORMS) {
        Ok(registry) => {
            let count = registry.snapshot().len();
            println!(
                "\n  transforms seam     bound; {count} transform(s) registered — `inseam claims <mimetype>` lists who claims what"
            );
        }
        Err(e) => {
            println!("\n  transforms seam     not bound — {e}; a loaded transform cannot register")
        }
    }
    match kernel.service(&CONNECTIONS) {
        Ok(registry) => {
            let hosts: Vec<String> = registry
                .snapshot()
                .iter()
                .map(|r| format!("{} ({})", r.host.id, r.host.kind))
                .collect();
            println!(
                "  connections seam    bound; {} host(s) stewarded: {}",
                hosts.len(),
                hosts.join(", ")
            );
            println!(
                "                      a loaded connection registers one more; one connection per host per node"
            );
        }
        Err(e) => {
            println!("  connections seam    not bound — {e}; a loaded connection cannot register")
        }
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
    /// Transform seam: the claims.
    pub claims: &'a [String],
    /// Connection seam: the host kind.
    pub kind: Option<&'a str>,
    pub dir: &'a Path,
}

fn scaffold_validate(s: &Scaffold<'_>) -> anyhow::Result<()> {
    let name_ok = !s.name.is_empty()
        && s.name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !name_ok {
        bail!(
            "plugin name `{}` must be lowercase ascii, digits, and dashes",
            s.name
        );
    }
    match s.seam {
        "transform" => scaffold_validate_transform(s),
        "connection" => scaffold_validate_connection(s),
        other => {
            bail!("seam `{other}` accepts no loaded plugins; `inseam seams` lists those that do")
        }
    }
}

fn scaffold_validate_transform(s: &Scaffold<'_>) -> anyhow::Result<()> {
    if s.claims.is_empty() {
        bail!("pass at least one --claims mimetype (e.g. --claims image/png,image/jpeg)");
    }
    if s.kind.is_some() {
        bail!("--kind is for the connection seam; a transform names --claims");
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

fn scaffold_validate_connection(s: &Scaffold<'_>) -> anyhow::Result<()> {
    let Some(kind) = s.kind else {
        bail!("pass --kind, the host kind this connection stewards (e.g. --kind github)");
    };
    HostKind::new(kind).with_context(|| format!("--kind `{kind}`"))?;
    if !s.claims.is_empty() {
        bail!("--claims is for the transform seam; a connection names --kind");
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
    match s.seam {
        "connection" => format!(
            "# Reviewed by the node owner; enforced by the bridge (docs/plugins/loaded.md).\n\
             name = {name:?}\n\
             version = \"0.1.0\"\n\
             seam = \"connection\"\n\
             host_kind = {kind:?}       # identity: describe-host() must export exactly this kind\n\
             \n\
             [connection]              # effective = this AND what capabilities() exports\n\
             enumerates = true\n\
             change_feed = false\n\
             writable = false\n\
             \n\
             [capabilities]            # request the MINIMUM you use; widening later re-gates approval\n\
             hosts = [\"REPLACE.example.com\"]   # every host `fetch` may contact; empty = no network\n\
             grant = false             # `authorize` attaches the entry's oauth grant bearer\n",
            name = s.name,
            kind = s.kind.unwrap_or_default()
        ),
        _ => {
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
                 llm_call_budget = 0       # LLM calls per index run, when llm = true\n\
                 hosts = []                # hosts `fetch` may contact; empty = no network\n",
                name = s.name
            )
        }
    }
}

fn scaffold_checks(s: &Scaffold<'_>) -> String {
    match s.seam {
        "connection" => scaffold_checks_connection(s),
        _ => scaffold_checks_transform(s),
    }
}

fn scaffold_checks_transform(s: &Scaffold<'_>) -> String {
    let first = &s.claims[0];
    let example_mimetype = first
        .strip_suffix("/*")
        .map(|t| format!("{t}/example"))
        .unwrap_or(first.clone());
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
         # [[check.fetch]]                       # canned network, needs hosts = [...]\n\
         # url = \"https://REPLACE.example.com/x\"\n\
         # body = \"...\"\n\
         \n\
         [check.expect]\n\
         min_fragments = 1\n\
         fragment_contains = \"REPLACE WITH TEXT YOUR PLUGIN EMITS\"\n\
         relation = \"REPLACE: contains | derives | your-own-kind\"\n\
         # mimetype = \"text/plain\"   # prefix of an emitted fragment's mimetype\n\
         \n\
         # The degrade path: no content, no LLM, no network. Decide what happens —\n\
         # nothing is the usual answer — and pin it.\n\
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

fn scaffold_checks_connection(s: &Scaffold<'_>) -> String {
    format!(
        "# Golden checks for `{name}` (docs/plugins/validation.md): the network as data —\n\
         # each check names the replies the host would give for the URLs the plugin\n\
         # asks, and the sources, bytes, or envelope it promises to make of them.\n\
         # Mandatory coverage, enforced by `inseam plugin check`: one check that PROVES\n\
         # THE CLAIM (locator_contains / content_type / hint_contains / text_contains)\n\
         # and one STARVED check (no [[check.fetch]]) that pins the offline answer\n\
         # (error = true, or max_sources). Canned URLs must be on hosts the manifest\n\
         # names; nothing is contacted.\n\
         \n\
         [config]                                  # [entry.config.plugin] every check configures with\n\
         example = \"REPLACE\"\n\
         \n\
         [[check]]\n\
         name = \"REPLACE: what enumerate lists, in one sentence\"\n\
         call = \"enumerate\"\n\
         root = \"\"\n\
         [[check.fetch]]\n\
         url = \"https://REPLACE.example.com/list\"\n\
         body = \"REPLACE with the host's reply\"           # or body_file = \"fixtures/list.json\"\n\
         # status = 200\n\
         # authorized = true                        # the reply needs the grant bearer; 401 otherwise\n\
         [check.expect]\n\
         min_sources = 1\n\
         locator_contains = \"REPLACE\"\n\
         # content_type = \"text/\"\n\
         \n\
         # The offline path: no network at all. An error the sweep reports is the\n\
         # right answer — never a trap, never an empty listing that reconciles every\n\
         # source away.\n\
         [[check]]\n\
         name = \"errors offline\"\n\
         call = \"enumerate\"\n\
         [check.expect]\n\
         error = true\n",
        name = s.name,
    )
}

fn scaffold_cargo_toml(s: &Scaffold<'_>) -> String {
    let extra = if s.seam == "connection" {
        "serde = { version = \"1\", features = [\"derive\"] }\ntoml = { version = \"0.8\", default-features = false, features = [\"parse\"] }\n"
    } else {
        ""
    };
    format!(
        "[package]\nname = {:?}\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         # Standalone: not a member of any surrounding workspace.\n[workspace]\n\n\
         [lib]\ncrate-type = [\"cdylib\"]\n\n\
         [dependencies]\nwit-bindgen = \"0.60\"\n{extra}\n\
         [profile.release]\nopt-level = \"s\"\nlto = true\nstrip = true\n",
        s.name
    )
}

fn scaffold_lib_rs(s: &Scaffold<'_>) -> String {
    match s.seam {
        "connection" => scaffold_lib_rs_connection(s),
        _ => scaffold_lib_rs_transform(s),
    }
}

fn scaffold_lib_rs_transform(s: &Scaffold<'_>) -> String {
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
         #[allow(unused_imports)]\nuse inseam::plugin::{{fetch, host}};\n\
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
         \x20       //   let Ok(answer) = fetch::fetch(&fetch::Request {{ method: \"GET\".into(), url, headers: vec![], body: None, authorize: false }}) else {{ return Ok(empty) }};\n\
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

fn scaffold_lib_rs_connection(s: &Scaffold<'_>) -> String {
    format!(
        "//! `{name}`: REPLACE with the host this connection stewards and how it reaches\n\
         //! it (one paragraph). Every failure is an error naming what failed — the\n\
         //! offline answer the sweep reports — never a panic.\n\
         \n\
         wit_bindgen::generate!({{\n    path: \"wit\",\n    world: \"connection-plugin\",\n}});\n\
         \n\
         use std::cell::RefCell;\n\
         \n\
         use exports::inseam::plugin::connection::{{\n\
         \x20   ContentLength, EdgeCapabilities, Envelope, Guest, HostDescription, Source,\n\
         }};\n\
         #[allow(unused_imports)]\nuse inseam::plugin::{{fetch, host}};\n\
         use serde::Deserialize;\n\
         \n\
         /// The entry's `[entry.config.plugin]`, parsed once by `configure`.\n\
         #[derive(Debug, Clone, Deserialize)]\n\
         #[serde(deny_unknown_fields)]\n\
         struct Config {{\n\
         \x20   example: String,\n\
         }}\n\
         \n\
         thread_local! {{\n\
         \x20   static CONFIG: RefCell<Option<Config>> = const {{ RefCell::new(None) }};\n\
         }}\n\
         \n\
         fn configured() -> Result<Config, String> {{\n\
         \x20   CONFIG.with(|c| c.borrow().clone()).ok_or_else(|| \"not configured\".to_string())\n\
         }}\n\
         \n\
         struct Plugin;\n\
         \n\
         impl Guest for Plugin {{\n\
         \x20   fn configure(config: String) -> Result<(), String> {{\n\
         \x20       let parsed: Config = toml::from_str(&config).map_err(|e| format!(\"plugin config: {{e}}\"))?;\n\
         \x20       CONFIG.with(|c| *c.borrow_mut() = Some(parsed));\n\
         \x20       Ok(())\n\
         \x20   }}\n\
         \n\
         \x20   /// The bridge derives the host id from kind + principal; the kind must\n\
         \x20   /// equal the manifest's `host_kind`.\n\
         \x20   fn describe_host() -> Result<HostDescription, String> {{\n\
         \x20       let config = configured()?;\n\
         \x20       Ok(HostDescription {{\n\
         \x20           kind: {kind:?}.to_string(),\n\
         \x20           principal: config.example.to_ascii_lowercase(),\n\
         \x20           display_name: format!(\"REPLACE · {{}}\", config.example),\n\
         \x20       }})\n\
         \x20   }}\n\
         \n\
         \x20   fn capabilities() -> EdgeCapabilities {{\n\
         \x20       EdgeCapabilities {{ enumerates: true, change_feed: false, writable: false }}\n\
         \x20   }}\n\
         \n\
         \x20   fn enumerate(_root: String) -> Result<Vec<Source>, String> {{\n\
         \x20       let _config = configured()?;\n\
         \x20       // Describe the request; the node performs it under the manifest's hosts:\n\
         \x20       //   let answer = fetch::fetch(&fetch::Request {{ method: \"GET\".into(), url, headers: vec![], body: None, authorize: false }})?;\n\
         \x20       //   if !(200..300).contains(&answer.status) {{ return Err(format!(\"status {{}}\", answer.status)); }}\n\
         \x20       // Then one Source per item: locator (your vocabulary), envelope, raw_bytes.\n\
         \x20       let _ = (Source {{ locator: String::new(), envelope: Envelope {{ source_type: String::new(), content_type: String::new(), length: ContentLength::Bytes(0), created: None, modified: None, hint: None }}, raw_bytes: 0 }},);\n\
         \x20       Err(\"REPLACE: not implemented\".to_string())\n\
         \x20   }}\n\
         \n\
         \x20   fn locator_prefix(_root: String) -> Option<String> {{\n\
         \x20       Some(String::new())\n\
         \x20   }}\n\
         \n\
         \x20   fn read_bytes(_locator: String) -> Result<Vec<u8>, String> {{\n\
         \x20       Err(\"REPLACE: not implemented\".to_string())\n\
         \x20   }}\n\
         \n\
         \x20   fn describe(_locator: String) -> Result<Envelope, String> {{\n\
         \x20       Err(\"this host describes sources only by enumeration\".to_string())\n\
         \x20   }}\n\
         }}\n\
         \n\
         export!(Plugin);\n",
        name = s.name,
        kind = s.kind.unwrap_or_default(),
    )
}

fn scaffold_readme(s: &Scaffold<'_>) -> String {
    let try_line = if s.seam == "connection" {
        format!(
            "inseam plugin try {name}.wasm --config example=REPLACE --enumerate \"\"   # one live call",
            name = s.name
        )
    } else {
        format!(
            "inseam plugin try {name}.wasm <file>       # what it emits for one real file",
            name = s.name
        )
    };
    format!(
        "# {name}\n\nREPLACE: what it does, what it needs (capabilities and why), what it emits.\n\n\
         ## Develop\n\n```sh\n\
         cargo build --release --target wasm32-wasip2 && cp target/wasm32-wasip2/release/{crate}.wasm {name}.wasm\n\
         inseam plugin check {name}.wasm            # the gate: must end PASS\n\
         {try_line}\n\
         ```\n\n## Mount\n\n```sh\ninseam plugin mount $PWD/{name}.wasm      # appends the entry below to the composition\n```\n\n\
         ```toml\n[[entry]]\nid = {name:?}\nplugin = \"wasm:<path>/{name}.wasm\"\n```\n",
        name = s.name,
        crate = s.name.replace('-', "_"),
    )
}

const FIXTURES_README: &str = "# Fixtures\n\nByte fixtures referenced by the checks file (`bytes_file` on a transform\ncheck, `body_file` on a canned reply), handed to the plugin during\n`inseam plugin check`. Keep them tiny and well-formed; the harness's LLM and\nnetwork are canned, so content never matters — only that bytes reach the\nplugin and come back shaped right. Every installing node downloads them.\n\n| File | What | Why |\n| --- | --- | --- |\n";

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
        (
            root.join(format!("{}.manifest.toml", s.name)),
            scaffold_manifest(s),
        ),
        (
            root.join(format!("{}.checks.toml", s.name)),
            scaffold_checks(s),
        ),
        (root.join("Cargo.toml"), scaffold_cargo_toml(s)),
        (root.join("src/lib.rs"), scaffold_lib_rs(s)),
        (root.join("wit/plugin.wit"), plugin_wit()),
        (root.join("README.md"), scaffold_readme(s)),
        (root.join("fixtures/README.md"), FIXTURES_README.to_string()),
    ];
    for (path, content) in &files {
        std::fs::write(path, content).with_context(|| path.display().to_string())?;
    }
    Ok(root)
}

pub fn print_scaffold_next_steps(root: &Path, name: &str, seam: &str) {
    println!("scaffolded {}\n", root.display());
    println!("next — the loop (skills/inseam-loaded-plugin):");
    println!("  1. edit {name}.checks.toml: replace every REPLACE with the behavior you promise");
    println!("  2. edit {name}.manifest.toml: request only the capabilities you will call");
    println!("  3. rustup target add wasm32-wasip2   # once; then:");
    println!("     cd {name} && cargo build --release --target wasm32-wasip2 \\");
    println!(
        "       && cp target/wasm32-wasip2/release/{}.wasm {name}.wasm",
        name.replace('-', "_")
    );
    println!(
        "  4. inseam plugin check {name}.wasm      # red on your first check — that is the to-do"
    );
    if seam == "connection" {
        println!(
            "  5. implement src/lib.rs until it ends PASS; `inseam plugin try {name}.wasm --config k=v --enumerate \"\"` makes one live call"
        );
        println!(
            "  6. inseam plugin mount $PWD/{name}.wasm, add [entry.config.plugin] to the entry, then `inseam hosts` and `inseam index --host <id> \"\"`"
        );
    } else {
        println!(
            "  5. implement src/lib.rs until it ends PASS; `inseam plugin try {name}.wasm <file>` shows output"
        );
        println!(
            "  6. inseam plugin mount $PWD/{name}.wasm && inseam plugins && inseam index <dir>"
        );
    }
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
        input.text.as_ref().map_or("none".to_string(), |t| format!(
            "{} chars",
            t.chars().count()
        )),
        input.bytes.as_ref().map_or(0, Vec::len),
        if input.llm_returns.is_some() {
            "canned"
        } else {
            "refuses"
        }
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
        println!(
            "  #{i:<3} {:<28} {:<13} parent {parent}",
            f.mimetype, f.relation
        );
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
            let fixture = request
                .file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
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
            println!(
                "fragment_contains = {word:?}   # REPLACE with the text that proves the claim"
            );
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
// plugin try — connection seam
// ---------------------------------------------------------------------------

/// Exactly one of the three connection calls, or none (a transform try).
pub fn try_call(
    enumerate: Option<&str>,
    read: Option<&str>,
    describe: Option<&str>,
) -> anyhow::Result<Option<TryCall>> {
    let named = [enumerate.is_some(), read.is_some(), describe.is_some()]
        .iter()
        .filter(|set| **set)
        .count();
    if named > 1 {
        bail!("pass one of --enumerate, --read, --describe");
    }
    Ok(match (enumerate, read, describe) {
        (Some(root), _, _) => Some(TryCall::Enumerate {
            root: root.to_string(),
        }),
        (_, Some(locator), _) => Some(TryCall::Read {
            locator: locator.to_string(),
        }),
        (_, _, Some(locator)) => Some(TryCall::Describe {
            locator: locator.to_string(),
        }),
        (None, None, None) => None,
    })
}

/// `key=value` pairs as the TOML a component's `configure` receives. Values
/// are typed by shape — `true`/`false`, integers — and strings otherwise,
/// which is what a composition author would write.
pub fn config_toml(pairs: &[String]) -> anyhow::Result<String> {
    if pairs.len() > TRY_CONFIG_PAIRS_MAX {
        bail!("at most {TRY_CONFIG_PAIRS_MAX} --config pairs");
    }
    let mut table = toml::Table::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=') else {
            bail!("--config `{pair}` is not key=value");
        };
        let key = key.trim();
        if key.is_empty() {
            bail!("--config `{pair}` has an empty key");
        }
        let typed = match value.trim() {
            "true" => toml::Value::Boolean(true),
            "false" => toml::Value::Boolean(false),
            other => match other.parse::<i64>() {
                Ok(n) => toml::Value::Integer(n),
                Err(_) => toml::Value::String(other.to_string()),
            },
        };
        table.insert(key.to_string(), typed);
    }
    Ok(toml::to_string(&table)?)
}

fn print_connection_outcome(outcome: &TryConnectionOutcome) {
    println!(
        "host    kind={}  principal={:?}  display={:?}",
        outcome.host_kind, outcome.host_principal, outcome.host_display_name
    );
    for note in &outcome.notes {
        println!("note    {note}");
    }
    if let Some(e) = &outcome.plugin_error {
        println!("plugin returned Err({e:?})");
    }
    if !outcome.sources.is_empty()
        || outcome.bytes.is_none() && outcome.envelope.is_none() && outcome.plugin_error.is_none()
    {
        println!("output  {} source(s)", outcome.sources.len());
        for (i, source) in outcome.sources.iter().enumerate().take(200) {
            println!(
                "  #{i:<4} {:<40} {:<24} {:>9} bytes  {}",
                source.locator,
                source.content_type,
                source.raw_bytes,
                source.hint.as_deref().unwrap_or("")
            );
        }
        if outcome.sources.len() > 200 {
            println!("  … {} more", outcome.sources.len() - 200);
        }
    }
    if let Some(bytes) = &outcome.bytes {
        println!("output  {} byte(s)", bytes.len());
        let text = String::from_utf8_lossy(bytes);
        for line in text.lines().take(12) {
            println!("       │ {line}");
        }
    }
    if let Some((content_type, hint)) = &outcome.envelope {
        println!("output  envelope  content-type={content_type}  hint={hint:?}");
    }
}

pub async fn plugin_try_connection(
    artifact: &Path,
    config: &[String],
    call: TryCall,
) -> anyhow::Result<()> {
    let config = config_toml(config)?;
    let outcome = try_connection(artifact, &config, call)
        .await
        .map_err(|e| anyhow::anyhow!("{}: {e}", artifact.display()))?;
    print_connection_outcome(&outcome);
    Ok(())
}

// ---------------------------------------------------------------------------
// plugin mount
// ---------------------------------------------------------------------------

pub fn plugin_mount(
    artifact: &Path,
    id: Option<&str>,
    composition_path: &Path,
) -> anyhow::Result<()> {
    let artifact = artifact
        .canonicalize()
        .with_context(|| format!("{} does not exist", artifact.display()))?;
    let stem = artifact
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .context("artifact has no file name")?;
    let id = id.unwrap_or(&stem);
    crate::registry::mount(id, &artifact, composition_path)?;
    println!(
        "mounted `{id}` → {}\nin {}",
        artifact.display(),
        composition_path.display()
    );
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
            kind: None,
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
        s.seam = "connection";
        assert!(scaffold_validate(&s).is_err(), "a connection needs --kind");
        s.kind = Some("Git Hub");
        assert!(scaffold_validate(&s).is_err());
        s.kind = Some("github");
        assert!(scaffold_validate(&s).is_ok());
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
            "wit/plugin.wit",
            "README.md",
            "fixtures/README.md",
        ] {
            assert!(root.join(file).exists(), "{file}");
        }
        let checks = std::fs::read_to_string(root.join("demo-plugin.checks.toml")).expect("reads");
        let parsed =
            inseam_conformance::ChecksFile::parse(&checks).expect("scaffolded checks parse");
        assert!(
            parsed.required_coverage().is_ok(),
            "the scaffold ships the mandatory coverage shape"
        );
        assert!(
            checks.contains("text = \"REPLACE"),
            "textual claims get an inline text input"
        );
        let manifest: inseam_wasm_host::ArtifactManifest = toml::from_str(
            &std::fs::read_to_string(root.join("demo-plugin.manifest.toml")).expect("reads"),
        )
        .expect("scaffolded manifest parses");
        assert_eq!(manifest.claims, claims);
        assert!(
            plugin_new(&scaffold(&claims, dir.path())).is_err(),
            "refuses to overwrite"
        );
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
    fn a_connection_scaffold_parses_on_the_connection_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let none: Vec<String> = Vec::new();
        let root = plugin_new(&Scaffold {
            name: "demo-host",
            seam: "connection",
            claims: &none,
            kind: Some("demo"),
            dir: dir.path(),
        })
        .expect("scaffolds");
        let checks = std::fs::read_to_string(root.join("demo-host.checks.toml")).expect("reads");
        let parsed = inseam_conformance::ConnectionChecksFile::parse(&checks)
            .expect("connection checks parse");
        assert!(parsed.required_coverage().is_ok());
        let manifest: inseam_wasm_host::ArtifactManifest = toml::from_str(
            &std::fs::read_to_string(root.join("demo-host.manifest.toml")).expect("reads"),
        )
        .expect("manifest parses");
        assert_eq!(manifest.seam, "connection");
        assert_eq!(manifest.host_kind.as_deref(), Some("demo"));
        assert!(
            std::fs::read_to_string(root.join("src/lib.rs"))
                .expect("reads")
                .contains("connection-plugin")
        );
    }

    #[test]
    fn try_config_pairs_are_typed_by_shape() {
        let toml = config_toml(&[
            "repository=octo/hello".into(),
            "authorize=true".into(),
            "files_max=10".into(),
        ])
        .expect("builds");
        let table: toml::Table = toml::from_str(&toml).expect("parses");
        assert_eq!(table["repository"].as_str(), Some("octo/hello"));
        assert_eq!(table["authorize"].as_bool(), Some(true));
        assert_eq!(table["files_max"].as_integer(), Some(10));
        assert!(config_toml(&["novalue".into()]).is_err());
        assert!(config_toml(&["=x".into()]).is_err());
    }

    #[test]
    fn exactly_one_connection_call_is_accepted() {
        assert!(try_call(None, None, None).expect("none").is_none());
        assert!(matches!(
            try_call(Some(""), None, None).expect("one"),
            Some(TryCall::Enumerate { .. })
        ));
        assert!(try_call(Some(""), Some("x"), None).is_err());
    }

    #[test]
    fn toml_strings_go_multiline_only_when_needed() {
        assert_eq!(toml_string("one line"), "\"one line\"");
        assert!(toml_string("two\nlines").starts_with("\"\"\"\n"));
    }
}
