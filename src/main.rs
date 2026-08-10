//! The inseam CLI: the same binary invoking operations directly against the
//! local node — one of the transport adapters `design/node-api.md` promises.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};

use inseam::agent::{run_agent, AgentEvent};
use inseam::llm::LlmClient;
use inseam::ops::{
    ExpandRequest, FetchRequest, Node, QueryRequest, QueryResponse, ScanRequest,
};
use inseam::profile::IndexProfile;

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
    /// Index profile TOML. Defaults to <data-dir>/profile.toml when present.
    #[arg(long, global = true, env = "INSEAM_PROFILE")]
    profile: Option<PathBuf>,
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
    /// Let a live LLM discover things through the finder API (query/expand/
    /// scan/fetch as tools). Requires the endpoint API key.
    Agent {
        question: String,
        /// Override the profile's agent model.
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 12)]
        turns: usize,
    },
    /// List OpenRouter models suitable for a role, cheapest first.
    Models {
        /// Embedding models instead of tool-capable chat models.
        #[arg(long)]
        embeddings: bool,
    },
    /// Index and catalog statistics for this node.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
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
    let profile = load_profile(&cli, &data_dir)?;
    let key_env = profile.endpoint.api_key_env.clone();
    let llm = LlmClient::from_config(&profile.endpoint).ok().map(Arc::new);

    match cli.command {
        Command::Index { root, rebuild } => {
            let node = Node::open(&data_dir, profile, llm).await?;
            let report = node.index_dir(&root, rebuild).await?;
            println!("{report}");
        }
        Command::Query { text, limit, json } => {
            let node = Node::open(&data_dir, profile, llm).await?;
            let response = node.query(QueryRequest { text, limit }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print_results(&response);
            }
        }
        Command::Expand { address, json } => {
            let node = Node::open(&data_dir, profile, llm).await?;
            let response = node.expand(ExpandRequest {
                address: address.parse()?,
            })?;
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
            let node = Node::open(&data_dir, profile, llm).await?;
            let response = node
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
            let node = Node::open(&data_dir, profile, llm).await?;
            let response = node
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
            let Some(client) = llm else {
                bail!("`inseam agent` needs {key_env} set (see .env.example)");
            };
            let model = model.unwrap_or_else(|| profile.llm.agent_model.clone());
            let node = Node::open(&data_dir, profile, Some(client.clone())).await?;
            println!("· model {model}\n");
            let outcome = run_agent(&node, &client, &model, &question, turns, |event| {
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
            let Some(client) = llm else {
                bail!("`inseam models` needs {key_env} set (see .env.example)");
            };
            let mut models = client.models(embeddings).await?;
            if !embeddings {
                models.retain(|m| m.supports_tools());
            }
            models.sort_by(|a, b| {
                let price = |m: &inseam::llm::ModelInfo| {
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
            let node = Node::open(&data_dir, profile, llm).await?;
            let stats = node.store().stats()?;
            let rows = node.store().search_rows_count().await?;
            println!("data dir       {}", data_dir.display());
            println!(
                "embedding      {} ({} dims)",
                node.profile().embedding.model,
                node.store().dimensions()
            );
            if node.store().reembed_pending() {
                println!("re-embed       pending — run `inseam index <dir>` to migrate");
            }
            println!("sources        {} ({} indexed)", stats.sources, stats.indexed_sources);
            println!("fragments      {}", stats.fragments);
            println!("relations      {}", stats.relations);
            println!("entities       {}", stats.entities);
            println!("search rows    {rows}");
        }
    }
    Ok(())
}

fn load_profile(cli: &Cli, data_dir: &std::path::Path) -> anyhow::Result<IndexProfile> {
    if let Some(path) = &cli.profile {
        return Ok(IndexProfile::load(path)?);
    }
    let default_path = data_dir.join("profile.toml");
    if default_path.exists() {
        return Ok(IndexProfile::load(&default_path)?);
    }
    Ok(IndexProfile::default())
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

fn print_expansion(response: &inseam::ops::ExpandResponse) {
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
