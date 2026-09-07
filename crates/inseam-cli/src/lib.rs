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
mod release;

pub use release::UpdateChannel;

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};

mod agent;
mod authoring;

use agent::{AgentEvent, run_agent};
use inseam_kernel::address::{Address, HostId, Timestamp};
use inseam_kernel::network::{NodeId, NodeRecord};
use inseam_kernel::substrate::{Composition, CompositionEdits, FiberState, Kernel, SubstrateError};
use inseam_seams::dates::ymd;
use inseam_seams::llm::{self, LLM, LlmLane, ModelInfo};
use inseam_seams::oauth::{GrantId, GrantState, Redirect};
use inseam_seams::operations::{
    AuthorizeGrantRequest, AwaitAuthorizationRequest, CatalogFilter, CatalogRequest,
    CatalogResponse, ExpandRequest, ExpelRequest, FetchBytesRequest, FetchRequest, GrantView,
    IndexRequest, JoinRequest, NetworkView, OPERATIONS, Operations, QueryRequest, QueryResponse,
    RepairOutcome, RepairReport, RepairRequest, RevokeGrantRequest, ScanRequest,
};
use inseam_seams::roster::Invitation;
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
id = "directory"
plugin = "transform-directory"

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

[[entry]]
id = "node"
plugin = "node"

[[entry]]
id = "transport"
plugin = "transport-iroh"

[[entry]]
id = "roster"
plugin = "roster"

[[entry]]
id = "sync"
plugin = "sync"

[[entry]]
id = "routing"
plugin = "routing"
"#;
const REPAIR_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const REPAIR_PROGRESS_TICKS_MAX: u32 = 51_840;

/// What a distribution contributes to the node: the linked plugin factories
/// it ships and the base composition that activates them. Everything else —
/// commands, the wasm plugin host, composition layering — is identical
/// across distributions.
pub struct Distribution {
    pub factories: Vec<Arc<dyn PluginFactory>>,
    pub base_composition: String,
    /// Where `inseam self update` looks and which key it trusts
    /// (`design/releases.md`).
    pub update_channel: UpdateChannel,
}

impl Distribution {
    /// The stock CLI: every first-party plugin, all active by default.
    pub fn first_party() -> Self {
        Self {
            factories: inseam_plugins::factories(),
            base_composition: BASE_COMPOSITION.to_string(),
            update_channel: UpdateChannel::first_party(),
        }
    }

    /// Point `inseam self update` at this distribution's own origin and
    /// signing key. A private distribution sets both; a stock binary that
    /// only needs a mirror passes `--origin` at runtime instead.
    pub fn with_update_channel(mut self, channel: UpdateChannel) -> Self {
        self.update_channel = channel;
        self
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
    /// Maintain this binary itself: check for or install the release the
    /// distribution's origin names for a cohort.
    #[command(name = "self")]
    SelfMaintenance {
        #[command(subcommand)]
        command: SelfCommand,
    },
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
        /// Put every LLM-using transform on the endpoint's batch lane for
        /// this run: summaries collect into large batch-API jobs at the
        /// provider's discount instead of one request apiece. Minutes to
        /// hours of latency — for large, time-insensitive runs.
        #[arg(long)]
        batch: bool,
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
    /// Retrieve a source's full content: its text, or with `--output` its
    /// bytes (an image, a PDF, a linked file) written to a file.
    Fetch {
        address: String,
        /// Write the raw bytes here instead of printing text; `-` is stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
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
    /// This node's network: every node the roster admits with what the
    /// last sync learned about it, every host with its stewards, and the
    /// replicated log — or one of the ceremonies that change it.
    Network {
        #[command(subcommand)]
        command: Option<NetworkCommand>,
        /// Emit the network view as JSON.
        #[arg(long, global = true)]
        json: bool,
    },
    /// Repair the derived search index without fetching or re-embedding
    /// sources. Use --rebuild to reconstruct an existing DiskANN index.
    Repair {
        /// Reconstruct the DiskANN index even when it is already ready.
        #[arg(long)]
        rebuild: bool,
    },
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

#[derive(Subcommand)]
enum NetworkCommand {
    /// Mint an invitation for another node to join through this one: one
    /// string to carry across, good for one node, once, for a day.
    Invite,
    /// Join the network an invitation names: dial the inviter, present the
    /// token, sync once.
    Join {
        /// The invitation text (`inseam-invite:…`) as the inviting node printed it.
        invitation: String,
    },
    /// Expel a node: every node stops admitting it and drops its logs;
    /// this node disconnects it now.
    Expel {
        /// The node's full id (64 hex characters), as `inseam network` lists it.
        node: String,
    },
    /// One sync round with every dialable node now.
    Sync,
}

#[derive(Subcommand)]
enum SelfCommand {
    /// Fetch the signed release manifest, and if the cohort names a version
    /// other than the running one, swap this executable for it. Restart the
    /// process (or let the supervisor) to run the new version.
    Update {
        /// Base URL or directory serving manifest.json and its artifacts.
        /// Defaults to the distribution's origin; overriding it makes this
        /// binary a mirror client — the signing key does not change.
        #[arg(long, env = release::ORIGIN_ENV)]
        origin: Option<String>,
        /// Release cohort to follow.
        #[arg(long, env = "INSEAM_RELEASE_COHORT", default_value = "stable")]
        cohort: String,
        /// Report what would change without downloading or installing.
        #[arg(long)]
        check: bool,
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
    // `self update` never boots the kernel: it must work when the installed
    // binary cannot, which is exactly when it is needed.
    if let Command::SelfMaintenance { command } = &cli.command {
        match command {
            SelfCommand::Update {
                origin,
                cohort,
                check,
            } => {
                release::self_update(
                    &distribution.update_channel,
                    origin.as_deref(),
                    cohort,
                    *check,
                )
                .await?;
            }
        }
        return Ok(());
    }
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
                registry::install(name, registry.as_deref(), &data_dir, &composition_path).await?;
            }
            PluginCommand::New {
                name,
                claims,
                seam,
                dir,
            } => {
                let root = authoring::plugin_new(&authoring::Scaffold {
                    name,
                    seam,
                    claims,
                    dir,
                })?;
                authoring::print_scaffold_next_steps(&root, name);
            }
            PluginCommand::Try {
                artifact,
                file,
                mimetype,
                llm_returns,
                not_root,
                as_check,
            } => {
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
            let flat = Composition {
                entries: composition.resolved(),
            };
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
        vec![Arc::new(inseam_wasm_host::WasmSchemeFactory::new(
            &data_dir,
        ))],
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
            let operations = Arc::new(inseam_http::OperationsSlot::new(
                kernel.service(&OPERATIONS)?,
            ));
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
                Arc::clone(&operations),
                shutdown_signal(),
            ));
            serve_composition_edits(
                &mut kernel,
                &base,
                &overlay_path,
                edits,
                &operations,
                transport,
            )
            .await?;
        }
        Command::Index {
            root,
            host,
            rebuild,
            catalog_only,
            max_sources,
            batch,
        } => {
            let ops = kernel.service(&OPERATIONS)?;
            let (host, root) = index_scope(&root, host.as_deref())?;
            let deep_budget = deep_budget_flag(catalog_only, max_sources);
            let llm_lane = batch.then_some(LlmLane::Batch);
            let report = ops
                .index(IndexRequest {
                    host,
                    root,
                    rebuild,
                    deep_budget,
                    llm_lane,
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
                println!(
                    "no grants: mount a connection that registers one (the `google` entry) or add `[[entry.config.grants]]` to the oauth entry (docs/plugins/oauth.md)"
                );
            }
            for grant in &grants {
                println!(
                    "{:20} {:24} {}",
                    grant.id,
                    grant.provider,
                    grant_status(grant)
                );
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
            println!(
                "Open this URL in your browser and sign in:\n\n  {}\n",
                started.url
            );
            println!(
                "Waiting for the browser to come back on {}…",
                started.redirect_uri
            );
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
            let view = ops
                .revoke_grant(RevokeGrantRequest { grant: id.clone() })
                .await?;
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
                let total = response
                    .lines_total
                    .map(|n| format!(" of {n}"))
                    .unwrap_or_default();
                eprintln!("(lines {}-{}{total})", response.start, response.end);
                println!("{}", response.text);
            }
        }
        Command::Fetch { address, output } => {
            let ops = kernel.service(&OPERATIONS)?;
            let address: Address = address.parse()?;
            match output {
                Some(path) => {
                    let response = ops.fetch_bytes(FetchBytesRequest { address }).await?;
                    write_fetched_bytes(&path, &response.bytes.0)?;
                    eprintln!(
                        "wrote {} bytes of {} to {}",
                        response.bytes.0.len(),
                        response.content_type,
                        path.display()
                    );
                }
                None => match ops.fetch(FetchRequest { address }).await {
                    Ok(response) => println!("{}", response.text),
                    Err(inseam_seams::SeamError::BinaryFetch(address, content_type)) => bail!(
                        "{address} is {content_type}; save its bytes with \
                         `inseam fetch {address} --output <file>`"
                    ),
                    Err(error) => return Err(error.into()),
                },
            }
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
            let outcome = run_agent(
                ops.as_ref(),
                client.as_ref(),
                &model,
                &question,
                turns,
                |event| match event {
                    AgentEvent::ToolCall { name, arguments } => {
                        println!("→ {name} {arguments}");
                    }
                    AgentEvent::ToolResult { name, brief } => {
                        println!("  ← {name}: {brief}");
                    }
                },
            )
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
            let role = if embeddings {
                "embedding"
            } else {
                "tool-capable chat"
            };
            println!(
                "{} {role} models, cheapest prompt price first:\n",
                models.len()
            );
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
                    .map(|c| {
                        if c >= 1000 {
                            format!("{}k ctx", c / 1000)
                        } else {
                            format!("{c} ctx")
                        }
                    })
                    .unwrap_or_default();
                let dims = m
                    .embedding_dimensions
                    .map(|d| format!("{d} dims"))
                    .unwrap_or_default();
                println!("  {:52} {:28} {:10} {}", m.id, pricing, ctx, dims);
            }
        }
        Command::Status => {
            let ops = kernel.service(&OPERATIONS)?;
            let status = ops.status().await?;
            println!("data dir       {}", data_dir.display());
            match &status.embedding_model {
                Some(model) => println!(
                    "embedding      {} ({} dims, vectors: {})",
                    model,
                    status.embedding_dimensions,
                    status.embedding_vectors.as_str()
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
                    "not present (run `inseam repair`)"
                }
            );
            println!("store size     {} on disk", human_bytes(status.store_bytes));
            println!(
                "content size   {} across cataloged sources",
                human_bytes(status.content_bytes)
            );
            println!(
                "caches         {} embeddings, {} transform outputs",
                status.cached_embeddings, status.cached_transform_outputs
            );
            println!(
                "remote sources {} (learned from other nodes' logs, counted in sources)",
                status.remote_sources
            );
        }
        Command::Network { command, json } => {
            let ops = kernel.service(&OPERATIONS)?;
            run_network_command(ops.as_ref(), command, json).await?;
        }
        Command::Repair { rebuild } => {
            let ops = kernel.service(&OPERATIONS)?;
            let report = repair_with_progress(ops.as_ref(), rebuild).await?;
            print_repair_report(&report);
        }
        Command::Plugins => {
            for fiber in kernel.fibers() {
                let state = match &fiber.state {
                    FiberState::Active => "active".to_string(),
                    FiberState::Pending => {
                        format!("pending (missing: {})", fiber.missing.join(", "))
                    }
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
        Command::Config { .. }
        | Command::Plugin { .. }
        | Command::Seams { .. }
        | Command::SelfMaintenance { .. } => {
            unreachable!("handled before boot")
        }
    }
    kernel.shutdown().await;
    Ok(())
}

/// The `network` group: every ceremony answers with the network as it
/// looks afterwards, printed the same way as the bare listing; `invite`
/// prints the invitation instead, since that is what the owner carries.
async fn run_network_command(
    operations: &dyn Operations,
    command: Option<NetworkCommand>,
    json: bool,
) -> anyhow::Result<()> {
    let view = match command {
        None => operations.network().await?,
        Some(NetworkCommand::Invite) => {
            let invitation = operations.invite().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&invitation)?);
            } else {
                print_invitation(&invitation);
            }
            return Ok(());
        }
        Some(NetworkCommand::Join { invitation }) => {
            operations.join(JoinRequest { invitation }).await?
        }
        Some(NetworkCommand::Expel { node }) => {
            let node: NodeId = node
                .parse()
                .context("expel takes a node's full id, as `inseam network` lists it")?;
            operations.expel(ExpelRequest { node }).await?
        }
        Some(NetworkCommand::Sync) => operations.sync_now().await?,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        print_network(&view);
    }
    Ok(())
}

/// The invitation on a line of its own — so it copies cleanly — and what
/// it is good for.
fn print_invitation(invitation: &Invitation) {
    println!("{invitation}");
    println!();
    println!(
        "Expires {}; admits one node, once. On the joining node, run:",
        hm_utc(invitation.expires)
    );
    println!("  inseam network join <the line above>");
}

fn print_network(view: &NetworkView) {
    print_network_local(&view.local);
    print_network_nodes(view);
    print_network_hosts(view);
    println!(
        "log: {} entries from {} origins",
        view.log.entries, view.log.origins
    );
}

fn print_network_local(local: &NodeRecord) {
    println!("this node    {} ({})", local.id.short(), local.display_name);
    println!("id           {}", local.id);
    println!("capabilities {}", capability_flags(local));
    if local.endpoints.is_empty() {
        println!("endpoints    none (outbound-only: reached through sessions it opens)");
    }
    for endpoint in &local.endpoints {
        println!("endpoint     {endpoint}");
    }
    println!();
}

fn print_network_nodes(view: &NetworkView) {
    // The name column fits a hostname plus the "(this node)" marker.
    println!(
        "{:12} {:28} {:22} {:5} {:10} {:5} last error",
        "node", "name", "capabilities", "live", "last sync", "hosts"
    );
    for node in &view.nodes {
        let name = if node.is_local {
            format!("{} (this node)", node.record.display_name)
        } else {
            node.record.display_name.clone()
        };
        println!(
            "{:12} {:28} {:22} {:5} {:10} {:5} {}",
            node.record.id.short(),
            name,
            capability_flags(&node.record),
            if node.live { "yes" } else { "no" },
            node.last_sync.as_deref().unwrap_or("-"),
            node.hosts.len(),
            node.last_error.as_deref().unwrap_or("-"),
        );
    }
    println!();
}

fn print_network_hosts(view: &NetworkView) {
    if view.hosts.is_empty() {
        println!("hosts: none known");
        println!();
        return;
    }
    println!("{:28} {:8} {:24} stewards", "host", "kind", "name");
    for host in &view.hosts {
        let stewards: Vec<String> = host.stewards.iter().map(NodeId::short).collect();
        println!(
            "{:28} {:8} {:24} {}",
            host.host.id,
            host.host.kind,
            host.host.display_name,
            if stewards.is_empty() {
                "none (unreachable)".to_string()
            } else {
                stewards.join(" ")
            }
        );
    }
    println!();
}

/// The three capability flags as a short label list: what discovery
/// fan-out and routing branch on.
fn capability_flags(record: &NodeRecord) -> String {
    let c = record.capabilities;
    let flags = [
        (c.always_on, "always-on"),
        (c.deep_index, "deep-index"),
        (c.relays, "relays"),
    ];
    let named: Vec<&str> = flags
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect();
    if named.is_empty() {
        "-".to_string()
    } else {
        named.join(",")
    }
}

/// A timestamp to the minute, UTC: the date the shared helper renders,
/// plus the time of day an expiry needs.
fn hm_utc(timestamp: Timestamp) -> String {
    const SECS_PER_DAY: i64 = 86_400;
    let seconds_into_day = timestamp.0.rem_euclid(SECS_PER_DAY);
    let hours = seconds_into_day / 3600;
    let minutes = (seconds_into_day % 3600) / 60;
    assert!(hours < 24);
    assert!(minutes < 60);
    format!("{} {hours:02}:{minutes:02} UTC", ymd(timestamp))
}

async fn repair_with_progress(
    operations: &dyn Operations,
    rebuild: bool,
) -> anyhow::Result<RepairReport> {
    println!("Repairing derived search index...");
    let started = Instant::now();
    let repair = operations.repair(RepairRequest { rebuild });
    tokio::pin!(repair);
    let mut progress = tokio::time::interval(REPAIR_PROGRESS_INTERVAL);
    progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    progress.tick().await;
    for _ in 0..REPAIR_PROGRESS_TICKS_MAX {
        tokio::select! {
            result = &mut repair => return result.map_err(Into::into),
            _ = progress.tick() => {
                println!("Repairing derived search index: {} elapsed", format_duration(started.elapsed()));
            }
        }
    }
    bail!("search-index repair exceeded its 72 hour safety limit")
}

fn print_repair_report(report: &RepairReport) {
    let outcome = match report.outcome {
        RepairOutcome::Empty => "no vector rows to index",
        RepairOutcome::AlreadyReady => "already ready",
        RepairOutcome::Built => "built",
        RepairOutcome::Rebuilt => "rebuilt",
    };
    println!("search rows    {}", report.search_rows);
    println!("converted     {} legacy vectors", report.vectors_converted);
    println!("vector index  {outcome}");
    assert_eq!(
        report.vector_index_ready,
        matches!(
            report.outcome,
            RepairOutcome::AlreadyReady | RepairOutcome::Built | RepairOutcome::Rebuilt
        )
    );
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes < 60 {
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = minutes / 60;
    let minutes = minutes % 60;
    format!("{hours}h {minutes:02}m")
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
            account
                .as_deref()
                .map(|a| format!(" as {a}"))
                .unwrap_or_default(),
            expires_at.map_or("no expiry".to_string(), |t| format!(
                "token until {}",
                inseam_seams::dates::ymd(t)
            )),
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
        assert!(
            max_sources.is_none(),
            "clap declares the flags mutually exclusive"
        );
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
        // A row learned from a peer's log names its steward; this node's
        // own rows say so, so the two never read alike.
        let via = entry
            .origin
            .map_or("local".to_string(), |origin| origin.short());
        println!(
            "{:8} {:12} {:>10} {:10} {:28} {}",
            if entry.indexed { "indexed" } else { "pending" },
            via,
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
/// the `composition` service (an owner installing a plugin, a settings
/// write) until the transport finishes. The kernel stays here, on the
/// node's own task, so a reconcile never runs concurrently with itself;
/// each edit is applied whole, replied to, and the next one taken. An
/// edit may have restarted the `operations` provider, so the transport's
/// slot is refreshed after every one.
async fn serve_composition_edits(
    kernel: &mut Kernel,
    base: &Composition,
    overlay_path: &Path,
    mut edits: CompositionEdits,
    operations: &inseam_http::OperationsSlot,
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
                match kernel.service(&OPERATIONS) {
                    Ok(current) => operations.replace(current),
                    // The edit left `operations` waiting or failed: the
                    // transport keeps the last service and its calls say
                    // what is missing.
                    Err(error) => tracing::warn!("operations not rebound after edit: {error}"),
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
        print_query_meta(&response.meta);
        return;
    }
    for (i, r) in response.results.iter().enumerate() {
        let via = r
            .via
            .map(|node| format!("  via {}", node.short()))
            .unwrap_or_default();
        println!("{:2}. {}  ({:.3}){via}", i + 1, r.address, r.score);
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
            let extent = hint.extent.map(|e| format!("[{e}] ")).unwrap_or_default();
            println!("    ▸ {extent}{}", hint.text);
        }
        println!();
    }
    print_query_meta(&response.meta);
}

/// One footer line: how long the query took and what it worked through, so
/// a slow or thin answer can be read without reaching for `--json`.
fn print_query_meta(meta: &inseam_seams::operations::QueryMeta) {
    let t = &meta.trace;
    println!(
        "{} ms · seeds {} ms ({} fts + {} vector → {}) · graph {} ms ({} relations) · rollup {} ms ({} sources → limit {})",
        meta.elapsed_ms,
        t.seeds_ms,
        t.fts_hits,
        t.vector_hits,
        t.seeds,
        t.graph_ms,
        t.relations,
        t.rollup_ms,
        t.candidate_sources,
        meta.limit,
    );
    print_query_remote(meta);
}

/// One line per node the query fanned out to: what it contributed before
/// the merge, or why it contributed nothing. A node that failed never
/// failed the query, so this is where its failure shows.
fn print_query_remote(meta: &inseam_seams::operations::QueryMeta) {
    for summary in &meta.remote {
        match &summary.error {
            Some(error) => println!(
                "via {}: no results ({} ms): {error}",
                summary.node.short(),
                summary.elapsed_ms
            ),
            None => println!(
                "via {}: {} results ({} ms)",
                summary.node.short(),
                summary.results,
                summary.elapsed_ms
            ),
        }
    }
}

/// Write fetched bytes to `path`, or to stdout for `-`, so a binary fetch
/// never lands in a terminal by accident.
fn write_fetched_bytes(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if path == Path::new("-") {
        use std::io::Write as _;
        std::io::stdout().write_all(bytes)?;
        return Ok(());
    }
    std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))
}

fn print_expansion(response: &inseam_seams::operations::ExpandResponse) {
    println!("{}", response.address);
    if let Some(summary) = &response.summary {
        println!("summary: {summary}\n");
    }
    println!("fragments:");
    for f in &response.fragments {
        let extent = f.extent.map(|e| format!(" [{e}]")).unwrap_or_default();
        let text = f.text.as_deref().unwrap_or("");
        let reference = f
            .content_address
            .as_ref()
            .map(|a| format!("  @ {a}"))
            .unwrap_or_default();
        println!("  #{:<5} {}{extent}  {}{reference}", f.id, f.mimetype, text);
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
