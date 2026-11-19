//! orch — a local workbench for orchestrating coding agents.
//!
//! One task is one git worktree plus one live ACP agent. The daemon serves a
//! single-page UI and a JSON API on localhost, and can hold several
//! repositories open at once.

mod acp;
mod receipt;
mod server;
mod settings;
mod task;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use task::{Config, Orch};
use tokio::net::TcpListener;

const DEFAULT_PORT: u16 = 7717;
const DEFAULT_AGENT: &str = "opencode acp";

fn help() {
    println!(
        "orch {version}

usage:
  orch [--repo PATH] [--port N] [--agent CMD] [--no-browser]

options:
  --repo PATH    repository to open and select first (default: restore the
                 previous selection, or the current directory on first run)
  --port N       port to listen on (default: {port})
  --agent CMD    ACP agent command (default: {agent})
  --title-model MODEL
                 model used to name a task from its first message
                 (default: a small model from the agent's catalogue)
  --no-browser   do not open the UI in a browser
  -h, --help     show this help

More repositories can be opened from the UI.

environment:
  ORCH_HOME      where worktrees, state, and the repository list live
                 (default: ~/.orch)
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
    let mut repo_given = false;
    let mut agent = DEFAULT_AGENT.to_string();
    let mut title_model: Option<String> = None;
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
            "--repo" => {
                repo = PathBuf::from(args.next().context("--repo needs a value")?);
                repo_given = true;
            }
            "--agent" => agent = args.next().context("--agent needs a value")?,
            "--title-model" => {
                title_model = Some(args.next().context("--title-model needs a value")?)
            }
            "--no-browser" => browser = false,
            other => anyhow::bail!("unknown argument `{other}` (try --help)"),
        }
    }

    let mut parts = agent.split_whitespace();
    let program = parts.next().context("--agent is empty")?.to_string();
    let agent_args: Vec<String> = parts.map(str::to_string).collect();

    let root = task::state_dir();
    std::fs::create_dir_all(&root)?;
    let orch = Orch::new(
        Config {
            program,
            args: agent_args,
            title_model,
        },
        root,
    );

    // The repository is optional: the current directory is opened when it is a
    // git repository, but it only becomes the active one when `--repo` says so
    // or when there is no previously selected repository to restore. An
    // explicit `--repo` that is not a repository is a mistake worth failing on.
    let first = match std::fs::canonicalize(&repo) {
        Ok(path) if inside_git(&path) => Some(path),
        Ok(path) if repo_given => anyhow::bail!("{} is not a git repository", path.display()),
        Ok(_) => None,
        Err(error) if repo_given => {
            return Err(error).with_context(|| format!("cannot open {}", repo.display()));
        }
        Err(_) => None,
    };
    orch.restore(first.as_deref(), repo_given).await?;

    let app = server::router(Arc::clone(&orch));
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("cannot bind 127.0.0.1:{port}"))?;
    let url = format!("http://127.0.0.1:{port}");

    println!("orch {}", env!("CARGO_PKG_VERSION"));
    match orch.current().await {
        Some(ws) => {
            println!("  repo   {}", ws.repo.path);
            println!("  branch {}", ws.repo.branch);
            println!("  tasks  {}", ws.task_count().await);
        }
        None => println!("  repo   (none open)"),
    }
    println!("  url    {url}");

    if browser {
        open_browser(&url);
    }

    axum::serve(listener, app).await?;
    Ok(())
}

/// Cheap ancestor walk: no subprocess, and it works for linked worktrees.
fn inside_git(path: &std::path::Path) -> bool {
    let mut current = Some(path);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return true;
        }
        current = dir.parent();
    }
    false
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
