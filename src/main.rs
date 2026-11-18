//! orch — a local workbench for orchestrating coding agents.
//!
//! One task is one git worktree plus one live ACP agent. The daemon serves a
//! single-page UI and a JSON API on localhost.

mod acp;
mod server;
mod task;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use task::{Config, Hub};
use tokio::net::TcpListener;

const DEFAULT_PORT: u16 = 7717;
const DEFAULT_AGENT: &str = "opencode acp";

fn help() {
    println!(
        "orch {version}

usage:
  orch [--repo PATH] [--port N] [--agent CMD] [--no-browser]

options:
  --repo PATH    git repository to work in (default: current directory)
  --port N       port to listen on (default: {port})
  --agent CMD    ACP agent command (default: {agent})
  --no-browser   do not open the UI in a browser
  -h, --help     show this help

environment:
  ORCH_HOME      where worktrees and task state live (default: ~/.orch)
  ORCH_DEBUG     set to 1 to print agent stderr",
        version = env!("CARGO_PKG_VERSION"),
        port = DEFAULT_PORT,
        agent = DEFAULT_AGENT,
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut port = DEFAULT_PORT;
    let mut repo = PathBuf::from(".");
    let mut agent = DEFAULT_AGENT.to_string();
    let mut browser = true;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                help();
                return Ok(());
            }
            "-V" | "--version" => {
                println!("orch {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--port" => port = args.next().context("--port needs a value")?.parse()?,
            "--repo" => repo = PathBuf::from(args.next().context("--repo needs a value")?),
            "--agent" => agent = args.next().context("--agent needs a value")?,
            "--no-browser" => browser = false,
            other => anyhow::bail!("unknown argument `{other}` (try --help)"),
        }
    }

    let repo = std::fs::canonicalize(&repo)
        .with_context(|| format!("cannot open repository {}", repo.display()))?;
    let project = task::project_dir(&repo.to_string_lossy());
    std::fs::create_dir_all(project.join("tasks"))?;
    std::fs::create_dir_all(project.join("worktrees"))?;

    let mut parts = agent.split_whitespace();
    let program = parts.next().context("--agent is empty")?.to_string();
    let agent_args: Vec<String> = parts.map(str::to_string).collect();

    let hub = Hub::open(
        &repo,
        project,
        Config {
            program,
            args: agent_args,
        },
    )
    .await?;

    let app = server::router(Arc::clone(&hub));
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("cannot bind 127.0.0.1:{port}"))?;
    let url = format!("http://127.0.0.1:{port}");

    println!("orch {}", env!("CARGO_PKG_VERSION"));
    println!("  repo   {}", hub.repo.path);
    println!("  branch {}", hub.repo.branch);
    println!("  url    {url}");
    println!("  tasks  {}", hub.list().await.len());

    if browser {
        open_browser(&url);
    }

    axum::serve(listener, app).await?;
    Ok(())
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
