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
#[command(name = "marginalia", version = marginalia::VERSION, about = "Personal reading library with an agent, REST and MCP")]
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
        /// Listen address. Default: $HOST:$PORT when either is set, else 127.0.0.1:7878.
        #[arg(long, env = "MARGINALIA_BIND")]
        bind: Option<String>,
        /// Static bearer token required on everything except GET /health. Or set MARGINALIA_TOKEN_FILE.
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

/// A secret given either directly or as the path of a file holding it (a trailing newline is dropped).
/// Giving both is an error, and so is an empty file: a blank token would silently turn auth off.
fn resolve_secret(name: &str, direct: Option<String>, file: Option<String>) -> Result<Option<String>> {
    let direct = direct.filter(|v| !v.trim().is_empty());
    let Some(path) = file.filter(|v| !v.trim().is_empty()) else { return Ok(direct) };
    anyhow::ensure!(direct.is_none(), "set either {name} or {name}_FILE, not both");
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {name}_FILE {path}"))?;
    let secret = raw.trim_end_matches(['\n', '\r']);
    anyhow::ensure!(!secret.trim().is_empty(), "{name}_FILE {path} is empty");
    Ok(Some(secret.to_string()))
}

/// `resolve_secret` for a secret that comes only from the environment: `$NAME` or the file at `$NAME_FILE`.
fn env_secret(name: &str) -> Result<Option<String>> {
    resolve_secret(name, std::env::var(name).ok(), std::env::var(format!("{name}_FILE")).ok())
}

/// Listen address: `--bind`/`MARGINALIA_BIND` wins, then `$HOST:$PORT` (either may be missing; this is how a
/// service manager passes it), then 127.0.0.1:7878. A bare IPv6 host gets its brackets.
fn resolve_bind(bind: Option<String>, host: Option<String>, port: Option<String>) -> String {
    let nonempty = |v: Option<String>| v.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if let Some(bind) = nonempty(bind) {
        return bind;
    }
    let (host, port) = (nonempty(host), nonempty(port));
    if host.is_none() && port.is_none() {
        return "127.0.0.1:7878".to_string();
    }
    let host = host.unwrap_or_else(|| "127.0.0.1".to_string());
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host };
    format!("{host}:{}", port.unwrap_or_else(|| "7878".to_string()))
}

fn app_config() -> Result<AppConfig> {
    let llm = env_secret("LLM_API_KEY")?.map(|api_key| LlmConfig {
        base_url: env_or("LLM_BASE_URL", "https://api.deepseek.com"),
        api_key,
        model: env_or("LLM_MODEL", "deepseek-chat"),
    });
    Ok(AppConfig { llm, firecrawl_key: env_secret("FIRECRAWL_API_KEY")? })
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
    Ok(Arc::new(App::new(Arc::new(repo), Arc::new(blobs), app_config()?)))
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
        Command::Serve { bind, token } => {
            let token = resolve_secret("MARGINALIA_TOKEN", token, std::env::var("MARGINALIA_TOKEN_FILE").ok())?;
            let bind = resolve_bind(bind, std::env::var("HOST").ok(), std::env::var("PORT").ok());
            serve(app, &bind, token).await
        }
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
    // Stop accepting on the signal and let in-flight requests finish. Long-lived MCP event streams never finish on
    // their own, so after SHUTDOWN_GRACE the remaining connections are dropped instead of hanging the stop.
    let (signalled, on_signal) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        shutdown_signal().await;
        eprintln!("marginalia: shutting down");
        let _ = signalled.send(());
    });
    tokio::select! {
        result = server => result?,
        _ = async {
            let _ = on_signal.await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => eprintln!("marginalia: closing connections still open after {}s", SHUTDOWN_GRACE.as_secs()),
    }
    eprintln!("marginalia: stopped");
    Ok(())
}

const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Resolves on Ctrl-C (SIGINT) or, on Unix, SIGTERM (what systemd and containers send).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn bind_precedence() {
        assert_eq!(resolve_bind(None, None, None), "127.0.0.1:7878");
        assert_eq!(resolve_bind(s("0.0.0.0:1"), s("h"), s("2")), "0.0.0.0:1");
        assert_eq!(resolve_bind(None, s("0.0.0.0"), s("9000")), "0.0.0.0:9000");
        assert_eq!(resolve_bind(None, None, s("9000")), "127.0.0.1:9000");
        assert_eq!(resolve_bind(None, s("10.0.0.5"), None), "10.0.0.5:7878");
        assert_eq!(resolve_bind(None, s("::1"), s("80")), "[::1]:80");
        assert_eq!(resolve_bind(s("  "), s(""), s("9")), "127.0.0.1:9");
    }

    #[test]
    fn secret_from_value_or_file() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tok");
        let p = path.to_string_lossy().to_string();
        assert_eq!(resolve_secret("X", None, None)?, None);
        assert_eq!(resolve_secret("X", s("v"), None)?, s("v"));
        assert_eq!(resolve_secret("X", s(""), None)?, None);

        std::fs::write(&path, "from-file\n")?;
        assert_eq!(resolve_secret("X", None, Some(p.clone()))?, s("from-file"));
        let both = resolve_secret("X", s("v"), Some(p.clone())).unwrap_err().to_string();
        assert!(both.contains("X_FILE") && both.contains("not both"), "{both}");

        std::fs::write(&path, "\n")?;
        assert!(resolve_secret("X", None, Some(p)).is_err(), "empty file");
        let missing = dir.path().join("nope").to_string_lossy().to_string();
        assert!(resolve_secret("X", None, Some(missing)).is_err(), "missing file");
        Ok(())
    }
}
