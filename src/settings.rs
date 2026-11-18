//! Per-repository settings.
//!
//! These live in orch's own state directory, never in the repository itself:
//! a checked-in config would leak into every clone and, worse, let an agent
//! rewrite the gate that is supposed to judge its work.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Settings {
    /// Commands run in order by "verify". Empty means no gate is configured.
    #[serde(default)]
    pub checks: Vec<String>,
    /// Substrings that orch refuses to execute on the user's behalf.
    #[serde(default = "default_deny")]
    pub deny: Vec<String>,
}

fn default_deny() -> Vec<String> {
    vec![
        ".#switch".to_string(),
        "git push".to_string(),
        "git reset --hard".to_string(),
        "sudo".to_string(),
    ]
}

impl Settings {
    pub fn load(dir: &Path) -> Settings {
        let path = dir.join("settings.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let text = serde_json::to_string_pretty(self)?;
        let path = dir.join("settings.json");
        let tmp = dir.join("settings.json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Refuse commands that would escape the worktree.
    pub fn check_command(&self, command: &str) -> Result<()> {
        for pattern in &self.deny {
            if !pattern.is_empty() && command.contains(pattern.as_str()) {
                anyhow::bail!(
                    "refused to run `{command}`: it matches the deny pattern `{pattern}` (edit the repository settings to allow it)"
                );
            }
        }
        Ok(())
    }

    /// A first guess at how to check a repository, so the gate is one click
    /// away instead of a blank textarea.
    pub fn suggested(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        if root.join("flake.nix").exists() {
            out.push("nix flake check".to_string());
            out.push("nix fmt -- --check .".to_string());
        }
        if root.join("Cargo.toml").exists() {
            out.push("cargo test".to_string());
            out.push("cargo clippy --all-targets -- -D warnings".to_string());
        }
        if root.join("package.json").exists() {
            out.push("npm test".to_string());
        }
        if root.join("pyproject.toml").exists() {
            out.push("uv run pytest".to_string());
        }
        out
    }
}
