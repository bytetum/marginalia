//! `marginalia` binary: REST + MCP server, stdio MCP, and small CLI helpers. All logic lives in `App`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use marginalia::blob::FsBlobStore;
use marginalia::domain::LibraryExport;
use marginalia::repo::new_id;
use marginalia::service::{App, AppConfig, LlmConfig};
use marginalia::surreal::SurrealRepo;
use marginalia::{api, mcp};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Parser)]
#[command(name = "marginalia", version, about = "Personal reading library with an agent, REST and MCP")]
struct Cli {
    /// Directory holding `db/` (SurrealDB) and `blobs/` (raw pages).
    #[arg(long, global = true, env = "MARGINALIA_DATA", default_value = "./data")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve REST and MCP (at /mcp) on one listener.
    Serve {
        #[arg(long, env = "MARGINALIA_BIND", default_value = "127.0.0.1:7878")]
        bind: String,
        /// Static bearer token required on everything except GET /health.
        #[arg(long, env = "MARGINALIA_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Serve MCP over stdio (for harnesses that spawn the server).
    Mcp,
    /// Save an article by URL.
    Add { url: String },
    /// Terminal chat with the built-in agent.
    Chat,
    /// Write a brief across the library, optionally steered by a direction.
    Brief {
        #[arg(trailing_var_arg = true)]
        direction: Vec<String>,
    },
    /// Restore a library snapshot (from `GET /export`) into an empty data dir. Offline: stop `serve` first.
    Import {
        /// JSON file written by `GET /export` (or the backup script).
        file: PathBuf,
    },
}

/// Minimal .env loader: KEY=VALUE lines, # comments; variables already set win.
fn load_dotenv() {
    let Ok(raw) = std::fs::read_to_string(".env") else { return };
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let (key, value) = (key.trim(), value.trim().trim_matches('"'));
            if std::env::var_os(key).is_none() {
                // SAFETY: called once at startup, before the runtime starts any other thread.
                unsafe { std::env::set_var(key, value) };
            }
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    env_opt(key).unwrap_or_else(|| default.to_string())
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn app_config() -> AppConfig {
    let llm = env_opt("LLM_API_KEY").map(|api_key| LlmConfig {
        base_url: env_or("LLM_BASE_URL", "https://api.deepseek.com"),
        api_key,
        model: env_or("LLM_MODEL", "deepseek-chat"),
    });
    AppConfig { llm, firecrawl_key: env_opt("FIRECRAWL_API_KEY") }
}

async fn build_app(data_dir: &Path) -> Result<Arc<App>> {
    let db = data_dir.join("db");
    std::fs::create_dir_all(&db).with_context(|| format!("creating {}", db.display()))?;
    let repo = SurrealRepo::open(&db).await.with_context(|| {
        format!(
            "opening database in {} (only one process may use it; if `marginalia serve` is running, use its REST API or the HTTP MCP at /mcp)",
            db.display()
        )
    })?;
    let blobs = FsBlobStore::new(data_dir.join("blobs"));
    Ok(Arc::new(App::new(Arc::new(repo), Arc::new(blobs), app_config())))
}

fn main() -> Result<()> {
    load_dotenv();
    let cli = Cli::parse();
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    // Read the file before opening the database, so a bad path does not touch the data dir.
    let snapshot = match &cli.command {
        Command::Import { file } => Some(read_export(file)?),
        _ => None,
    };
    let app = build_app(&cli.data_dir).await?;
    match cli.command {
        Command::Serve { bind, token } => serve(app, &bind, token.filter(|t| !t.is_empty())).await,
        Command::Mcp => {
            // stdout carries the protocol; anything human-readable goes to stderr.
            eprintln!("marginalia mcp: serving on stdio (data in {})", cli.data_dir.display());
            mcp::serve_stdio(app).await?;
            Ok(())
        }
        Command::Add { url } => {
            let a = app.save_url(&url).await?;
            println!("Saved \"{}\" ({} words, via {}) as {}", a.title, a.words, a.via, a.id);
            Ok(())
        }
        Command::Chat => chat(app).await,
        Command::Import { file } => {
            let export = snapshot.context("no snapshot was read")?;
            let (articles, highlights, syntheses) = (export.articles.len(), export.highlights.len(), export.syntheses.len());
            app.import(export).await.with_context(|| {
                format!("importing {} into {} (the library there must be empty; move data/db aside first)", file.display(), cli.data_dir.display())
            })?;
            println!("Imported {articles} article(s), {highlights} highlight(s), {syntheses} synthesis(es).");
            println!("Blobs are not in the JSON: copy the backup's blobs/ into {}.", cli.data_dir.join("blobs").display());
            Ok(())
        }
        Command::Brief { direction } => {
            let direction = direction.join(" ");
            let s = app.brief(Some(direction.as_str()).filter(|d| !d.trim().is_empty())).await?;
            println!("{}\n\n{}\n\n(saved as {})", s.title, s.body, s.id);
            Ok(())
        }
    }
}

fn read_export(file: &Path) -> Result<LibraryExport> {
    let raw = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    serde_json::from_slice(&raw).with_context(|| format!("{} is not a Marginalia export", file.display()))
}

async fn serve(app: Arc<App>, bind: &str, token: Option<String>) -> Result<()> {
    let loopback = bind.parse::<SocketAddr>().map(|a| a.ip().is_loopback()).unwrap_or_else(|_| bind.starts_with("localhost"));
    if token.is_none() && !loopback {
        eprintln!("WARNING: binding to {bind} (not loopback) with NO bearer token: anyone who can reach it can read and write the library.");
        eprintln!("WARNING: set MARGINALIA_TOKEN or pass --token.");
    }
    let llm = app.has_llm();
    let router = api::router(app, token.clone());
    let listener = tokio::net::TcpListener::bind(bind).await.with_context(|| format!("binding {bind}"))?;
    eprintln!(
        "marginalia listening on http://{} (REST, MCP at /mcp) · auth {} · LLM {}",
        listener.local_addr()?,
        if token.is_some() { "bearer token" } else { "off" },
        if llm { "on" } else { "off" },
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

const HELP: &str = "Commands:
  /add <url>                 save an article
  /list                      list saved articles
  /analyse <id> [direction]  deep read of one article, optionally steered
  /brief [direction]         synthesise the library, optionally steered
  /new                       start a fresh conversation
  /quit
Anything else is a question for the agent about your library.";

async fn prompt() {
    let mut out = tokio::io::stdout();
    let _ = out.write_all(b"\n> ").await;
    let _ = out.flush().await;
}

async fn chat(app: Arc<App>) -> Result<()> {
    let count = app.repo().list_articles(u32::MAX, 0).await?.len();
    println!("Marginalia · {count} article(s){}", if app.has_llm() { "" } else { " · no LLM_API_KEY: chat, /analyse and /brief are unavailable" });
    println!("{HELP}");
    let mut session = new_id();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    prompt().await;
    while let Some(raw) = lines.next_line().await? {
        let line = raw.trim();
        if line.is_empty() {
            prompt().await;
            continue;
        }
        let (cmd, arg) = line.split_once(char::is_whitespace).map(|(c, a)| (c, a.trim())).unwrap_or((line, ""));
        let result: Result<()> = async {
            match cmd {
                "/quit" | "/exit" => return Ok(()),
                "/help" => println!("{HELP}"),
                "/add" if arg.is_empty() => println!("Usage: /add <url>"),
                "/add" => {
                    let a = app.save_url(arg).await?;
                    println!("Saved \"{}\" ({} words, via {}) as {}", a.title, a.words, a.via, a.id);
                }
                "/list" => {
                    for a in app.repo().list_articles(u32::MAX, 0).await? {
                        println!("  {}  {}  ({} words, {} highlights)", a.id, a.title, a.words, a.highlight_count);
                    }
                }
                "/new" => {
                    app.reset_chat(&session);
                    session = new_id();
                    println!("Fresh conversation.");
                }
                "/analyse" if arg.is_empty() => println!("Usage: /analyse <id> [direction]"),
                "/analyse" => {
                    let (id, direction) = arg.split_once(char::is_whitespace).map(|(i, d)| (i, d.trim())).unwrap_or((arg, ""));
                    let s = app.analyse(id, Some(direction).filter(|d| !d.is_empty())).await?;
                    println!("\n{}\n\n{}\n\n(saved as {})", s.title, s.body, s.id);
                }
                "/brief" => {
                    let s = app.brief(Some(arg).filter(|d| !d.is_empty())).await?;
                    println!("\n{}\n\n{}\n\n(saved as {})", s.title, s.body, s.id);
                }
                _ => println!("\n{}", app.chat(&session, line).await?),
            }
            Ok(())
        }
        .await;
        if matches!(cmd, "/quit" | "/exit") {
            break;
        }
        if let Err(e) = result {
            println!("Error: {e:#}");
        }
        prompt().await;
    }
    Ok(())
}
