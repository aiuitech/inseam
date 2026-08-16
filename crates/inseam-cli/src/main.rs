//! The inseam CLI: a **distribution** (`design/plugins.md`) — an app crate
//! that links the first-party native plugins plus the wasm plugin host,
//! ships a base composition, and layers the node's own composition file on
//! top (`design/composition.md`). Command handlers are a thin transport over
//! the `operations` seam; no command contains node logic.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};

use inseam_kernel::substrate::{Composition, FiberState, Kernel, SubstrateError};
use inseam_plugins::agent::{run_agent, AgentEvent};
use inseam_seams::llm::{self, ModelInfo, LLM};
use inseam_seams::operations::{
    ExpandRequest, FetchRequest, IndexRequest, QueryRequest, QueryResponse, ScanRequest,
    OPERATIONS,
};

/// What makes the CLI the CLI: the plugins it mounts by default. A user's
/// composition file patches these entries by id or adds new ones (sandboxed
/// plugins included); `inseam config --resolved` prints the layered result.
const BASE_COMPOSITION: &str = r#"
[[entry]]
id = "fs"
plugin = "connection-fs"

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
    /// Index a directory of the local filesystem host (read-only).
    Index {
        root: PathBuf,
        /// Re-index sources even when unchanged.
        #[arg(long)]
        rebuild: bool,
    },
    /// Query the discovery index: ranked addresses with summaries and hints.
    Query {
        text: String,
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Emit the operation response as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show one source's fragments, relations, and connected entities.
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
    /// Print the composition. --resolved shows the layered result the node
    /// boots — what prints is what runs, by construction.
    Config {
        #[arg(long)]
        resolved: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("INSEAM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("inseam=info")),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let data_dir = match &cli.data_dir {
        Some(d) => d.clone(),
        None => dirs::data_local_dir()
            .context("no platform data directory; pass --data-dir")?
            .join("inseam"),
    };
    let composition = load_composition(&cli, &data_dir)?;

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

    let mut kernel = Kernel::boot(
        &data_dir,
        inseam_plugins::factories(),
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
        Command::Index { root, rebuild } => {
            let ops = kernel.service(&OPERATIONS)?;
            let report = ops
                .index(IndexRequest {
                    root: root.display().to_string(),
                    rebuild,
                })
                .await?;
            println!("{report}");
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
                        .facts("llm")
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
            println!("entities       {}", status.entities);
            println!("search rows    {}", status.search_rows);
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
        Command::Config { .. } => unreachable!("handled before boot"),
    }
    kernel.shutdown().await;
    Ok(())
}

fn load_composition(cli: &Cli, data_dir: &std::path::Path) -> anyhow::Result<Composition> {
    let base = Composition::parse(BASE_COMPOSITION, "<distribution base>")
        .expect("the distribution base composition is valid");
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
