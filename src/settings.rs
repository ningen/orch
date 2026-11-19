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
    /// Glob patterns for changed paths a verify result should flag for
    /// review. Matching does not fail the checks by itself; it makes the
    /// receipt say `needs_review` so a human looks before merging.
    #[serde(default, alias = "policyPaths")]
    pub policy_paths: Vec<String>,
    /// Substrings for added lines that should flag the change for review.
    #[serde(default, alias = "policyLines")]
    pub policy_lines: Vec<String>,
    /// How much the system decides on its own after a turn ends:
    /// "off", "keep-fresh" (rebase + re-verify), or "auto-merge".
    /// See `crate::receipt::AutopilotMode`.
    #[serde(default = "default_autopilot")]
    pub autopilot: String,
}

fn default_autopilot() -> String {
    crate::receipt::AutopilotMode::KeepFresh
        .as_str()
        .to_string()
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

    /// Patterns that make an agent's own work weaker: skipping tests,
    /// disabling type checks, editing CI. Suggested, never forced.
    pub fn suggested_policy(root: &Path) -> (Vec<String>, Vec<String>) {
        let mut paths = Vec::new();
        let mut lines = Vec::new();
        if root.join(".github").exists() {
            paths.push(".github/workflows/*".to_string());
        }
        if root.join("Cargo.toml").exists() {
            lines.push("#[ignore]".to_string());
        }
        if root.join("package.json").exists() {
            lines.push("it.skip".to_string());
            lines.push("describe.skip".to_string());
            lines.push("test.skip".to_string());
            lines.push("@ts-nocheck".to_string());
        }
        if root.join("flake.nix").exists() {
            paths.push("flake.lock".to_string());
        }
        (paths, lines)
    }
}

/// Minimal glob: `*` matches any run of characters (including `/`), `?`
/// matches one. Everything else is literal.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn inner(p: &[u8], s: &[u8]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        match p[0] {
            b'*' => {
                // Collapse consecutive stars, then try every split.
                let rest = &p[1..];
                if rest.first() == Some(&b'*') {
                    return inner(rest, s);
                }
                for i in 0..=s.len() {
                    if inner(rest, &s[i..]) {
                        return true;
                    }
                }
                false
            }
            b'?' => !s.is_empty() && inner(&p[1..], &s[1..]),
            literal => !s.is_empty() && s[0] == literal && inner(&p[1..], &s[1..]),
        }
    }
    inner(pattern.as_bytes(), path.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_within_segments() {
        assert!(glob_match("*.nix", "flake.nix"));
        assert!(glob_match("*.nix", "hosts/default.nix"));
        assert!(!glob_match("*.nix", "flake.toml"));
        assert!(glob_match("test/*", "test/util.rs"));
        assert!(glob_match("test/*", "test/a/b.rs")); // `*` crosses `/`
        assert!(!glob_match("test/*", "src/main.rs"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("exact.rs", "exact.rs"));
        assert!(!glob_match("exact.rs", "other.rs"));
    }

    #[test]
    fn glob_edge_cases() {
        assert!(glob_match("*", "anything/at/all"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
        assert!(glob_match("**", "deep/path"));
        // `*` crosses `/`, so a workflow guard also covers nested jobs.
        assert!(glob_match(
            ".github/workflows/*",
            ".github/workflows/ci.yml"
        ));
        assert!(glob_match(
            ".github/workflows/*",
            ".github/workflows/jobs/ci.yml"
        ));
    }

    #[test]
    fn settings_roundtrip_keeps_policy() {
        let dir = std::env::temp_dir().join(format!("orch-test-{}", std::process::id()));
        let settings = Settings {
            checks: vec!["cargo test".into()],
            deny: default_deny(),
            policy_paths: vec![".github/workflows/*".into()],
            policy_lines: vec!["#[ignore]".into()],
            autopilot: "auto-merge".into(),
        };
        settings.save(&dir).unwrap();
        let loaded = Settings::load(&dir);
        assert_eq!(loaded.policy_paths, settings.policy_paths);
        assert_eq!(loaded.policy_lines, settings.policy_lines);
        assert_eq!(loaded.autopilot, "auto-merge");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_settings_files_still_load() {
        let dir = std::env::temp_dir().join(format!("orch-test-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            r#"{"checks":["cargo test"],"deny":["git push"]}"#,
        )
        .unwrap();
        let loaded = Settings::load(&dir);
        assert_eq!(loaded.checks, vec!["cargo test".to_string()]);
        assert!(loaded.policy_paths.is_empty());
        assert!(loaded.policy_lines.is_empty());
        // Missing fields fall back to their defaults, never to "off".
        assert_eq!(loaded.autopilot, "keep-fresh");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
