//! The inseam CLI as a library: a **distribution** (`design/plugins.md`) is
//! an app crate that links a set of linked-tier plugins, ships a base
//! composition, and layers the node's own composition file on top
//! (`design/composition.md`). The stock `inseam` binary is
//! [`Distribution::first_party`]; a custom distribution (a cloud node with
//! its own performant linked plugins, built from a private repo) is a thin
//! binary crate that appends its factories and base entries and calls
//! [`run`]. Command handlers are a thin transport over the `operations`
//! seam; no command contains node logic.

mod registry;

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand, ValueEnum};

mod agent;
mod authoring;

use inseam_kernel::address::{Address, HostId};
use inseam_kernel::substrate::{
    Composition, CompositionEdits, FiberState, Kernel, SubstrateError,
};
use agent::{run_agent, AgentEvent};
use inseam_seams::llm::{self, ModelInfo, LLM};
use inseam_seams::oauth::{GrantId, GrantState, Redirect};
use inseam_seams::operations::{
    AuthorizeGrantRequest, AwaitAuthorizationRequest, CatalogFilter, CatalogRequest,
    CatalogResponse, ExpandRequest, FetchRequest, GrantView, IndexRequest, QueryRequest,
    QueryResponse, RevokeGrantRequest, ScanRequest, OPERATIONS,
};
use inseam_seams::sweep::DeepBudget;

pub use inseam_kernel::substrate::PluginFactory;

/// What makes the stock CLI the CLI: the plugins it mounts by default. A
/// user's composition file patches these entries by id or adds new ones
/// (loaded plugins included); `inseam config --resolved` prints the layered
/// result.
const BASE_COMPOSITION: &str = r#"
[[entry]]
id = "connections"
plugin = "connections"

[[entry]]
id = "fs"
plugin = "connection-fs"

[[entry]]
id = "oauth"
plugin = "oauth"

[[entry]]
id = "google"
plugin = "connection-google"

[[entry]]
id = "llm"
plugin = "llm-endpoint"

[[entry]]
id = "embedder"
plugin = "embedder"

[[entry]]
id = "transforms"
plugin = "transforms"

[[entry]]
id = "markdown"
plugin = "transform-markdown"

[[entry]]
id = "chunker"
plugin = "transform-chunker"

[[entry]]
id = "summarizer"
plugin = "transform-summarizer"

[[entry]]
id = "entities"
plugin = "transform-entities"

[[entry]]
id = "finder"
plugin = "finder"

[[entry]]
id = "sweep"
plugin = "sweep"

[[entry]]
id = "operations"
plugin = "operations"
"#;

/// What a distribution contributes to the node: the linked plugin factories
/// it ships and the base composition that activates them. Everything else —
/// commands, the wasm plugin host, composition layering — is identical
/// across distributions.
pub struct Distribution {
    pub factories: Vec<Arc<dyn PluginFactory>>,
    pub base_composition: String,
}

impl Distribution {
    /// The stock CLI: every first-party plugin, all active by default.
    pub fn first_party() -> Self {
        Self {
            factories: inseam_plugins::factories(),
            base_composition: BASE_COMPOSITION.to_string(),
        }
    }

    /// Link additional plugin factories into this distribution. Factories
    /// registered here are addressable from any composition by bare name,
    /// exactly like the first-party set.
    pub fn with_factories(
        mut self,
        extra: impl IntoIterator<Item = Arc<dyn PluginFactory>>,
    ) -> Self {
        self.factories.extend(extra);
        self
    }

    /// Append `[[entry]]` items (a TOML fragment) to the base composition,
    /// activating linked plugins by default. A node's composition file
    /// still patches these by id like any base entry.
    pub fn with_base_entries(mut self, entries: &str) -> Self {
        self.base_composition.push('\n');
        self.base_composition.push_str(entries);
        self
    }
}

#[derive(Parser)]
#[command(
    name = "inseam",
    version,
    about = "Personal data network: index locally, discover everywhere"
)]
struct Cli {
    /// Node data directory (index + catalog). Defaults to the platform data
    /// dir, e.g. ~/Library/Application Support/inseam.
    #[arg(long, global = true, env = "INSEAM_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Composition TOML layered over the distribution base. Defaults to
    /// <data-dir>/composition.toml when present.
    #[arg(long, global = true, env = "INSEAM_COMPOSITION")]
    composition: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Keep this node running and serve its authenticated owner web API.
    Serve {
        /// Address for the HTTP listener. The loopback default is safe for
        /// local use; a hosted node opts into a public listener explicitly.
        #[arg(long, env = "INSEAM_HTTP_BIND", default_value = "127.0.0.1:7337")]
        bind: SocketAddr,
        /// Owner token used only to create signed browser sessions. Prefer
        /// INSEAM_OWNER_TOKEN so the value does not appear in process lists.
        #[arg(long, env = "INSEAM_OWNER_TOKEN", hide_env_values = true)]
        owner_token: String,
        /// Approved indexing scope in `id=/absolute/path` form. The web API
        /// accepts the id and never accepts a raw filesystem path.
        #[arg(
            long = "index-root",
            env = "INSEAM_INDEX_ROOTS",
            value_delimiter = ',',
            value_name = "ID=PATH"
        )]
        index_roots: Vec<String>,
        /// Built Vite directory to serve. Without it, only the API is served.
        #[arg(long, env = "INSEAM_WEB_DIR")]
        web_dir: Option<PathBuf>,
        /// Cookie policy. Use local-http only for an HTTP development server.
        #[arg(
            long,
            env = "INSEAM_COOKIE_SECURITY",
            value_enum,
            default_value_t = CookieMode::Secure
        )]
        cookie: CookieMode,
        /// The origin owners reach this node at (https://node.example): what
        /// OAuth providers redirect back to when a grant is authorized from
        /// the web console. Derived from --bind when unset.
        #[arg(long, env = "INSEAM_PUBLIC_URL")]
        public_url: Option<String>,
    },
    /// Index a scope of one host this node stewards (read-only): a
    /// directory for the filesystem host, a label or folder for a service
    /// host. `root` may also be an address, `inseam://<host>/<root>`, which
    /// names the host itself.
    Index {
        root: String,
        /// The host to sweep; defaults to the only host mounted, and is
        /// required once several are.
        #[arg(long)]
        host: Option<String>,
        /// Re-index sources even when unchanged.
        #[arg(long)]
        rebuild: bool,
        /// Catalog every source (address + envelope) and deep-index none —
        /// the ingest run. Catalog-only sources stay pending and are
        /// deep-indexed by a later run with budget.
        #[arg(long, conflicts_with = "max_sources")]
        catalog_only: bool,
        /// Deep-index at most this many sources this run (the rest are
        /// cataloged); overrides the composition's `sweep.max_sources`.
        #[arg(long, value_name = "COUNT")]
        max_sources: Option<NonZeroU32>,
    },
    /// The catalog: every source this node knows about, deep-indexed or
    /// still pending, with counts.
    Catalog {
        /// Restrict to one host.
        #[arg(long)]
        host: Option<String>,
        /// Only sources whose subtree is built and searchable.
        #[arg(long, conflicts_with = "pending")]
        indexed: bool,
        /// Only sources cataloged but not yet deep-indexed.
        #[arg(long)]
        pending: bool,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        /// Emit the operation response as JSON.
        #[arg(long)]
        json: bool,
    },
    /// The hosts this node stewards and what each connection supports.
    Hosts,
    /// The OAuth grants this node holds and where each stands.
    Grants,
    /// Authorize an OAuth grant: prints the provider's sign-in URL and waits
    /// for the browser to land back on the node.
    Authorize { grant: String },
    /// Forget a grant's tokens; the hosts behind it withdraw until it is
    /// authorized again.
    Revoke { grant: String },
    /// Query the discovery index: ranked addresses with summaries and hints.
    Query {
        text: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Emit the operation response as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one source's fragments, relations, and connected keyed fragments (entities).
    Expand {
        address: String,
        #[arg(long)]
        json: bool,
    },
    /// Read a line range of a source without fetching all of it.
    Scan {
        address: String,
        #[arg(long)]
        start: u64,
        #[arg(long)]
        end: u64,
        #[arg(long)]
        json: bool,
    },
    /// Retrieve a source's full content.
    Fetch { address: String },
    /// Let a live LLM discover things through the operations seam
    /// (query/expand/scan/fetch as tools). Requires the endpoint API key.
    Agent {
        question: String,
        /// Override the llm provider's agent model.
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 12)]
        turns: usize,
    },
    /// List endpoint models suitable for a role, cheapest first.
    Models {
        /// Embedding models instead of tool-capable chat models.
        #[arg(long)]
        embeddings: bool,
    },
    /// Index and catalog statistics for this node.
    Status,
    /// The plugin tree: every fiber, its state, and its live effects.
    Plugins,
    /// The seams that accept loaded plugins and their contract; --wit prints
    /// the WIT world to generate bindings from.
    Seams {
        #[arg(long)]
        wit: bool,
    },
    /// What a plugin manifest may request, and whether this node can grant
    /// each right now (the live answer to "will my plugin degrade here?").
    Capabilities,
    /// Which transforms on this node claim a mimetype or a file — what a new
    /// plugin would sit beside, or duplicate.
    Claims { target: String },
    /// Loaded plugin artifacts: scaffold, validate, try, mount, install.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Print the composition. --resolved shows the layered result the node
    /// boots — what prints is what runs, by construction.
    Config {
        #[arg(long)]
        resolved: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CookieMode {
    Secure,
    LocalHttp,
}

impl From<CookieMode> for inseam_http::CookieSecurity {
    fn from(mode: CookieMode) -> Self {
        match mode {
            CookieMode::Secure => Self::Secure,
            CookieMode::LocalHttp => Self::LocalHttp,
        }
    }
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Scaffold a new loaded plugin: manifest, golden checks in the mandatory
    /// shape (red until the plugin does what it says), a degrading Rust
    /// stub, the WIT, and READMEs. Needs no network and no source tree.
    New {
        name: String,
        /// Mimetypes the plugin claims (essences or `type/*`).
        #[arg(long, value_delimiter = ',', required = true)]
        claims: Vec<String>,
        #[arg(long, default_value = "transform")]
        seam: String,
        /// Parent directory for the new `<name>/` folder.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Apply an artifact to one real file through the harness bridge (canned
    /// LLM, fake capabilities) and print what it emits; --as-check prints the
    /// observed output as a golden check to paste and tighten.
    Try {
        artifact: PathBuf,
        file: PathBuf,
        /// Override the detected mimetype.
        #[arg(long)]
        mimetype: Option<String>,
        /// The canned LLM reply; absent = the LLM refuses.
        #[arg(long)]
        llm_returns: Option<String>,
        /// Apply as a non-root fragment instead of a source root.
        #[arg(long)]
        not_root: bool,
        #[arg(long)]
        as_check: bool,
    },
    /// Append a local artifact to this node's composition (`wasm:<path>`),
    /// id defaulting to the artifact's stem.
    Mount {
        artifact: PathBuf,
        #[arg(long)]
        id: Option<String>,
    },
    /// Run the conformance harness against a .wasm artifact: static checks,
    /// a real bridge mount, the hostile-input contract battery, and the
    /// plugin's own golden checks (<artifact>.checks.toml). Exits nonzero
    /// on failure — the same verdict install-time admission enforces.
    Check { artifact: PathBuf },
    /// Fetch a plugin from a registry, verify its sha256 against the
    /// reviewed index, run the conformance harness, and mount it in this
    /// node's composition.
    Install {
        name: String,
        /// Registry root: an https URL or a local directory containing
        /// registry.toml. Defaults to the inseam repository's plugins tree.
        #[arg(long, env = "INSEAM_REGISTRY")]
        registry: Option<String>,
    },
}

/// Run the CLI for a distribution: parse args, boot the kernel with the
/// distribution's factories, dispatch the command. The whole `main` of any
/// distribution binary.
pub fn run(distribution: Distribution) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("INSEAM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("inseam=info")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let cli = Cli::parse();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?
        .block_on(run_command(cli, distribution))
}

async fn run_command(cli: Cli, distribution: Distribution) -> anyhow::Result<()> {
    let data_dir = match &cli.data_dir {
        Some(d) => d.clone(),
        None => dirs::data_local_dir()
            .context("no platform data directory; pass --data-dir")?
            .join("inseam"),
    };
    // `plugin check`/`plugin install` never boot the kernel: validating an
    // artifact is hermetic, and installing must work before the composition
    // it edits can settle.
    if let Command::Plugin { command } = &cli.command {
        match command {
            PluginCommand::Check { artifact } => {
                let report = inseam_wasm_host::check_artifact(artifact).await;
                print!("{}", report.render());
                if !report.passed() {
                    std::process::exit(1);
                }
            }
            PluginCommand::Install { name, registry } => {
                let composition_path = composition_path_of(&cli, &data_dir);
                registry::install(name, registry.as_deref(), &data_dir, &composition_path)
                    .await?;
            }
            PluginCommand::New { name, claims, seam, dir } => {
                let root = authoring::plugin_new(&authoring::Scaffold {
                    name,
                    seam,
                    claims,
                    dir,
                })?;
                authoring::print_scaffold_next_steps(&root, name);
            }
            PluginCommand::Try { artifact, file, mimetype, llm_returns, not_root, as_check } => {
                authoring::plugin_try(
                    &authoring::TryRequest {
                        artifact,
                        file,
                        mimetype: mimetype.as_deref(),
                        llm_returns: llm_returns.as_deref(),
                        not_root: *not_root,
                    },
                    *as_check,
                )
                .await?;
            }
            PluginCommand::Mount { artifact, id } => {
                let composition_path = composition_path_of(&cli, &data_dir);
                authoring::plugin_mount(artifact, id.as_deref(), &composition_path)?;
            }
        }
        return Ok(());
    }
    // `seams` is a question about the contract, not the node: no boot.
    if let Command::Seams { wit } = &cli.command {
        authoring::seams(*wit);
        return Ok(());
    }

    let composition = load_composition(&cli, &data_dir, &distribution.base_composition)?;

    // `config` never boots the kernel: printing the composition must work
    // even when the composition is broken enough that boot would not.
    if let Command::Config { resolved } = &cli.command {
        if *resolved {
            let mut flat = Composition::default();
            flat.entries = composition.resolved();
            println!("{}", flat.to_toml());
        } else {
            println!("{}", composition.to_toml());
        }
        return Ok(());
    }

    // Resolved before `cli.command` is taken apart below: the edit loop a
    // running node services needs the overlay path after that.
    let overlay_path = composition_path_of(&cli, &data_dir);
    let mut kernel = Kernel::boot(
        &data_dir,
        distribution.factories,
        vec![Arc::new(inseam_wasm_host::WasmSchemeFactory::new(&data_dir))],
    )
    .await?;
    match kernel.reconcile(&composition).await {
        Ok(()) => {}
        // A composition that cannot fully settle is loud but not fatal to
        // the process: commands touching the waiting seams fail with the
        // same message, and `inseam plugins` shows the tree.
        Err(e @ SubstrateError::Unsettled { .. }) => eprintln!("warning: {e}"),
        Err(e) => return Err(e.into()),
    }

    match cli.command {
        Command::Serve {
            bind,
            owner_token,
            index_roots,
            web_dir,
            cookie,
            public_url,
        } => {
            let roots = index_roots
                .iter()
                .map(|value| parse_http_index_root(value))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let operations = kernel.service(&OPERATIONS)?;
            let edits = kernel
                .take_composition_edits()
                .expect("a freshly booted kernel hands out its edits once");
            let base = base_composition(&distribution.base_composition)?;
            let transport = tokio::spawn(inseam_http::serve(
                inseam_http::ServerConfig {
                    bind,
                    owner_token,
                    cookie_security: cookie.into(),
                    index_roots: roots,
                    web_dir,
                    public_url,
                },
                operations,
                shutdown_signal(),
            ));
            serve_composition_edits(&mut kernel, &base, &overlay_path, edits, transport).await?;
        }
        Command::Index {
            root,
            host,
            rebuild,
            catalog_only,
            max_sources,
        } => {
            let ops = kernel.service(&OPERATIONS)?;
            let (host, root) = index_scope(&root, host.as_deref())?;
            let deep_budget = deep_budget_flag(catalog_only, max_sources);
            let report = ops
                .index(IndexRequest {
                    host,
                    root,
                    rebuild,
                    deep_budget,
                })
                .await?;
            println!("{report}");
        }
        Command::Catalog {
            host,
            indexed,
            pending,
            limit,
            json,
        } => {
            let ops = kernel.service(&OPERATIONS)?;
            let host = host.as_deref().map(HostId::new).transpose()?;
            let filter = catalog_filter_flag(indexed, pending);
            let response = ops
                .catalog(CatalogRequest {
                    host,
                    filter,
                    limit,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print_catalog(&response, filter);
            }
        }
        Command::Hosts => {
            let ops = kernel.service(&OPERATIONS)?;
            let hosts = ops.hosts().await?;
            if hosts.is_empty() {
                println!("no hosts: no connection plugin is mounted");
            }
            for h in &hosts {
                let c = h.capabilities;
                println!(
                    "{:28} {:8} {:14} {}{}{}  {}",
                    h.id,
                    h.kind,
                    h.entry,
                    if c.enumerates { "enumerates " } else { "" },
                    if c.change_feed { "change-feed " } else { "" },
                    if c.writable { "writable" } else { "read-only" },
                    h.display_name
                );
            }
        }
        Command::Grants => {
            let ops = kernel.service(&OPERATIONS)?;
            let grants = ops.grants().await?;
            if grants.is_empty() {
                println!("no grants: mount a connection that registers one (the `google` entry) or add `[[entry.config.grants]]` to the oauth entry (docs/plugins/oauth.md)");
            }
            for grant in &grants {
                println!("{:20} {:24} {}", grant.id, grant.provider, grant_status(grant));
            }
        }
        Command::Authorize { grant } => {
            let ops = kernel.service(&OPERATIONS)?;
            let id = GrantId::new(grant.as_str())?;
            let started = ops
                .authorize_grant(AuthorizeGrantRequest {
                    grant: id.clone(),
                    redirect: Redirect::Loopback,
                })
                .await?;
            println!("Open this URL in your browser and sign in:\n\n  {}\n", started.url);
            println!("Waiting for the browser to come back on {}…", started.redirect_uri);
            let view = ops
                .await_authorization(AwaitAuthorizationRequest {
                    state: started.state,
                })
                .await?;
            println!("grant `{id}` {}", grant_status(&view));
        }
        Command::Revoke { grant } => {
            let ops = kernel.service(&OPERATIONS)?;
            let id = GrantId::new(grant.as_str())?;
            let view = ops.revoke_grant(RevokeGrantRequest { grant: id.clone() }).await?;
            println!("grant `{id}` {}", grant_status(&view));
        }
        Command::Query { text, limit, json } => {
            let ops = kernel.service(&OPERATIONS)?;
            let response = ops.query(QueryRequest { text, limit }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print_results(&response);
            }
        }
        Command::Expand { address, json } => {
            let ops = kernel.service(&OPERATIONS)?;
            let response = ops
                .expand(ExpandRequest {
                    address: address.parse()?,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print_expansion(&response);
            }
        }
        Command::Scan {
            address,
            start,
            end,
            json,
        } => {
            let ops = kernel.service(&OPERATIONS)?;
            let response = ops
                .scan(ScanRequest {
                    address: address.parse()?,
                    start,
                    end,
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                if let Some(fragment) = response.served_from_fragment {
                    eprintln!("(served from text fragment {fragment})");
                }
                println!("{}", response.text);
            }
        }
        Command::Fetch { address } => {
            let ops = kernel.service(&OPERATIONS)?;
            let response = ops
                .fetch(FetchRequest {
                    address: address.parse()?,
                })
                .await?;
            println!("{}", response.text);
        }
        Command::Agent {
            question,
            model,
            turns,
        } => {
            let ops = kernel.service(&OPERATIONS)?;
            let Ok(client) = kernel.service(&LLM) else {
                bail!("`inseam agent` needs the llm entry active (set the endpoint API key)");
            };
            let model = model
                .or_else(|| {
                    kernel
                        .facts(&LLM)
                        .and_then(|f| f.str(llm::facts::AGENT_MODEL))
                        .map(str::to_string)
                })
                .context("no agent model configured")?;
            println!("· model {model}\n");
            let outcome = run_agent(ops.as_ref(), client.as_ref(), &model, &question, turns, |event| {
                match event {
                    AgentEvent::ToolCall { name, arguments } => {
                        println!("→ {name} {arguments}");
                    }
                    AgentEvent::ToolResult { name, brief } => {
                        println!("  ← {name}: {brief}");
                    }
                }
            })
            .await?;
            println!(
                "\n{}\n\n· {} turns, {} tool calls, ${:.4} spent",
                outcome.answer.trim(),
                outcome.turns,
                outcome.tool_calls,
                outcome.spent
            );
        }
        Command::Models { embeddings } => {
            let Ok(client) = kernel.service(&LLM) else {
                bail!("`inseam models` needs the llm entry active (set the endpoint API key)");
            };
            let mut models = client.models(embeddings).await?;
            if !embeddings {
                models.retain(|m| m.supports_tools());
            }
            models.sort_by(|a, b| {
                let price = |m: &ModelInfo| {
                    m.pricing
                        .as_ref()
                        .and_then(|p| p.prompt_per_million())
                        .unwrap_or(f64::MAX)
                };
                price(a).total_cmp(&price(b))
            });
            let role = if embeddings { "embedding" } else { "tool-capable chat" };
            println!("{} {role} models, cheapest prompt price first:\n", models.len());
            for m in models.iter().take(30) {
                let pricing = m
                    .pricing
                    .as_ref()
                    .map(|p| {
                        format!(
                            "${:.3}/M in, ${:.3}/M out",
                            p.prompt_per_million().unwrap_or(f64::NAN),
                            p.completion_per_million().unwrap_or(f64::NAN)
                        )
                    })
                    .unwrap_or_else(|| "unpriced".to_string());
                let ctx = m
                    .context_length
                    .map(|c| format!("{}k ctx", c / 1000))
                    .unwrap_or_default();
                println!("  {:52} {:28} {}", m.id, pricing, ctx);
            }
        }
        Command::Status => {
            let ops = kernel.service(&OPERATIONS)?;
            let status = ops.status().await?;
            println!("data dir       {}", data_dir.display());
            match &status.embedding_model {
                Some(model) => println!(
                    "embedding      {} ({} dims)",
                    model, status.embedding_dimensions
                ),
                None => println!("embedding      none (no embedder mounted)"),
            }
            if status.reembed_pending {
                println!("re-embed       pending — run `inseam index <dir>` to migrate");
            }
            println!(
                "sources        {} ({} indexed)",
                status.sources, status.indexed_sources
            );
            println!("fragments      {}", status.fragments);
            println!("relations      {}", status.relations);
            println!("keyed          {}", status.keyed_fragments);
            println!("search rows    {}", status.search_rows);
            println!(
                "vector index   {}",
                if status.vector_index_ready {
                    "ready (libSQL DiskANN)"
                } else {
                    "not present"
                }
            );
            println!(
                "store size     {} on disk",
                human_bytes(status.store_bytes)
            );
            println!(
                "content size   {} across cataloged sources",
                human_bytes(status.content_bytes)
            );
        }
        Command::Plugins => {
            for fiber in kernel.fibers() {
                let state = match &fiber.state {
                    FiberState::Active => "active".to_string(),
                    FiberState::Pending => format!("pending (missing: {})", fiber.missing.join(", ")),
                    FiberState::Failed(e) => format!("failed: {e}"),
                };
                println!("{:14} {:24} {}", fiber.id, fiber.plugin, state);
                for effect in &fiber.effects {
                    println!("{:14} · {}", "", effect);
                }
            }
        }
        Command::Capabilities => authoring::capabilities(&kernel),
        Command::Claims { target } => authoring::claims(&kernel, &target)?,
        Command::Config { .. } | Command::Plugin { .. } | Command::Seams { .. } => {
            unreachable!("handled before boot")
        }
    }
    kernel.shutdown().await;
    Ok(())
}

/// One line of where a grant stands, with the next step when there is one.
fn grant_status(grant: &GrantView) -> String {
    match &grant.state {
        GrantState::MissingSecret { env } => format!("missing secret: set {env}"),
        GrantState::Unauthorized => format!("unauthorized — run `inseam authorize {}`", grant.id),
        GrantState::Authorized {
            expires_at,
            scopes,
            account,
        } => format!(
            "authorized{} ({}) scopes: {}",
            account.as_deref().map(|a| format!(" as {a}")).unwrap_or_default(),
            expires_at.map_or("no expiry".to_string(), |t| format!("token until {}", inseam_seams::dates::ymd(t))),
            scopes.join(" ")
        ),
    }
}

/// What `inseam index` means by its arguments: an address names its host
/// and the locator is the scope; otherwise `--host` (if any) and the root
/// verbatim — except that a bare root naming an existing local path is made
/// absolute, so `inseam index .` keeps meaning this directory. That
/// convenience is the transport knowing it runs on a filesystem, nothing a
/// connection is told.
/// The `index` command's two budget flags, folded into one request value:
/// neither flag means "the composition's budget"; clap rejects both at once.
fn deep_budget_flag(catalog_only: bool, max_sources: Option<NonZeroU32>) -> Option<DeepBudget> {
    if catalog_only {
        assert!(max_sources.is_none(), "clap declares the flags mutually exclusive");
        return Some(DeepBudget::CatalogOnly);
    }
    max_sources.map(DeepBudget::Sources)
}

/// The `catalog` command's two filter flags; clap rejects both at once.
fn catalog_filter_flag(indexed: bool, pending: bool) -> CatalogFilter {
    if indexed {
        assert!(!pending, "clap declares the flags mutually exclusive");
        return CatalogFilter::Indexed;
    }
    if pending {
        return CatalogFilter::Pending;
    }
    CatalogFilter::All
}

fn print_catalog(response: &CatalogResponse, filter: CatalogFilter) {
    println!(
        "{} sources cataloged: {} indexed, {} pending",
        response.sources, response.indexed, response.pending
    );
    for entry in &response.entries {
        println!(
            "{:8} {:>10} {:10} {:28} {}",
            if entry.indexed { "indexed" } else { "pending" },
            human_bytes(entry.raw_bytes),
            entry.modified.as_deref().unwrap_or("-"),
            entry.content_type,
            entry.address
        );
    }
    let shown = u64::try_from(response.entries.len()).expect("a listing fits in u64");
    let matching = match filter {
        CatalogFilter::All => response.sources,
        CatalogFilter::Indexed => response.indexed,
        CatalogFilter::Pending => response.pending,
    };
    assert!(shown <= matching);
    if shown < matching {
        println!("({shown} of {matching} shown; raise --limit for more)");
    }
}

/// Bytes in the unit that keeps the number short, to one decimal place.
/// Integer arithmetic throughout: the tenths are computed exactly.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut divisor: u64 = 1;
    let mut unit = 0;
    while bytes / divisor >= 1000 && unit + 1 < UNITS.len() {
        divisor *= 1000;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }
    let tenths = bytes * 10 / divisor;
    format!("{}.{} {}", tenths / 10, tenths % 10, UNITS[unit])
}

fn index_scope(root: &str, host: Option<&str>) -> anyhow::Result<(Option<HostId>, String)> {
    if root.starts_with("inseam://") {
        if host.is_some() {
            bail!("pass either an address or --host, not both");
        }
        let address: Address = root.parse()?;
        return Ok((Some(address.host), address.locator.as_str().to_string()));
    }
    let host = host.map(HostId::new).transpose()?;
    let path = std::path::Path::new(root);
    let root = match (host.is_none(), path.exists()) {
        (true, true) => std::path::absolute(path)?.display().to_string(),
        _ => root.to_string(),
    };
    Ok((host, root))
}

/// The node's composition overlay: `--composition`, else `<data-dir>/composition.toml`.
fn composition_path_of(cli: &Cli, data_dir: &std::path::Path) -> PathBuf {
    cli.composition
        .clone()
        .unwrap_or_else(|| data_dir.join("composition.toml"))
}

fn parse_http_index_root(value: &str) -> anyhow::Result<inseam_http::IndexRoot> {
    let (id, path) = value
        .split_once('=')
        .with_context(|| format!("index root `{value}` must use ID=PATH"))?;
    if !Path::new(path).is_absolute() {
        bail!("index root `{id}` path must be absolute: `{path}`");
    }
    Ok(inseam_http::IndexRoot::new(id, path)?)
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "could not listen for the shutdown signal");
    }
}

/// Keep a running node editable: apply composition edits submitted through
/// the `composition` service (an owner installing a plugin) until the
/// transport finishes. The kernel stays here, on the node's own task, so a
/// reconcile never runs concurrently with itself; each edit is applied
/// whole, replied to, and the next one taken.
async fn serve_composition_edits(
    kernel: &mut Kernel,
    base: &Composition,
    overlay_path: &Path,
    mut edits: CompositionEdits,
    mut transport: tokio::task::JoinHandle<Result<(), inseam_http::ConfigError>>,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            served = &mut transport => {
                served.context("the owner HTTP server task")??;
                return Ok(());
            }
            pending = edits.next() => {
                let Some(pending) = pending else {
                    // The kernel holds the submitting end, so this cannot
                    // happen while it lives; fall back to the transport.
                    transport.await.context("the owner HTTP server task")??;
                    return Ok(());
                };
                tracing::info!(edit = ?pending.edit(), "applying a composition edit");
                let outcome = kernel
                    .apply_composition_edit(base, overlay_path, pending.edit())
                    .await;
                if let Err(error) = &outcome {
                    tracing::warn!("composition edit not applied: {error}");
                }
                pending.reply(outcome);
            }
        }
    }
}

fn base_composition(base_composition: &str) -> anyhow::Result<Composition> {
    Composition::parse(base_composition, "<distribution base>")
        .context("the distribution base composition must be valid")
}

fn load_composition(
    cli: &Cli,
    data_dir: &std::path::Path,
    base_composition: &str,
) -> anyhow::Result<Composition> {
    let base = self::base_composition(base_composition)?;
    let overlay_path = match &cli.composition {
        Some(path) => Some(path.clone()),
        None => {
            let default = data_dir.join("composition.toml");
            default.exists().then_some(default)
        }
    };
    match overlay_path {
        Some(path) => {
            let overlay = Composition::load(&path)?;
            Ok(base.layered(overlay)?)
        }
        None => Ok(base),
    }
}

fn print_results(response: &QueryResponse) {
    if response.results.is_empty() {
        println!("no results");
        return;
    }
    for (i, r) in response.results.iter().enumerate() {
        println!("{:2}. {}  ({:.3})", i + 1, r.address, r.score);
        let e = &r.envelope;
        let modified = e
            .modified
            .as_deref()
            .map(|m| format!(" · modified {m}"))
            .unwrap_or_default();
        println!("    {} · {}{}", e.content_type, e.length, modified);
        for replica in &r.replicas {
            println!("    = also at {replica}");
        }
        if let Some(summary) = &r.summary {
            println!("    {summary}");
        }
        for hint in &r.hints {
            let extent = hint
                .extent
                .as_deref()
                .map(|e| format!("[{e}] "))
                .unwrap_or_default();
            println!("    ▸ {extent}{}", hint.text);
        }
        println!();
    }
}

fn print_expansion(response: &inseam_seams::operations::ExpandResponse) {
    println!("{}", response.address);
    if let Some(summary) = &response.summary {
        println!("summary: {summary}\n");
    }
    println!("fragments:");
    for f in &response.fragments {
        let extent = f
            .extent
            .as_deref()
            .map(|e| format!(" [{e}]"))
            .unwrap_or_default();
        let text = f.text.as_deref().unwrap_or("");
        println!("  #{:<5} {}{extent}  {}", f.id, f.mimetype, text);
    }
    if !response.relations.is_empty() {
        println!("\nrelations:");
        for r in &response.relations {
            println!("  #{} -{}-> #{}", r.from, r.kind, r.to);
        }
    }
    if !response.neighbors.is_empty() {
        println!("\nconnected beyond this source:");
        for n in &response.neighbors {
            let text = n.text.as_deref().unwrap_or("");
            let source = n
                .source
                .as_ref()
                .map(|a| format!("  ({a})"))
                .unwrap_or_default();
            println!("  #{:<5} {}  {text}{source}", n.id, n.mimetype);
        }
    }
}
