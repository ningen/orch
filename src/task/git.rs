//! Git operations on a task's worktree: fingerprints, rebase, merge, and the
//! gate that decides whether committing is allowed.

use super::{Task, TaskMeta, now_ms};
use crate::receipt::{ReceiptState, classify};
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Run git in `cwd`, returning (ok, stdout, stderr).
pub(crate) async fn git(cwd: &Path, args: &[&str]) -> Result<(bool, String, String)> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("git {}", args.join(" ")))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

impl Task {
    /// Mark new files as intent-to-add.
    ///
    /// Anything that reads the git index instead of the working tree cannot
    /// see a fresh file otherwise: flake evaluation in particular reports the
    /// path as missing, so an agent that adds a `.nix` file and immediately
    /// runs `nix build` fails for a reason that looks like its own mistake.
    pub async fn stage_intent(&self) -> Result<()> {
        let (meta, _) = self.snapshot().await;
        if meta.worktree.is_empty() {
            return Ok(());
        }
        let _ = git(&PathBuf::from(&meta.worktree), &["add", "-N", "-A"]).await;
        Ok(())
    }

    pub async fn diff(&self) -> Result<Value> {
        let (meta, _) = self.snapshot().await;
        if meta.worktree.is_empty() {
            return Ok(json!({ "stat": "", "diff": "" }));
        }
        self.stage_intent().await.ok();
        let (meta, _) = self.snapshot().await;
        let wt = PathBuf::from(&meta.worktree);
        let (_, stat, _) = git(&wt, &["diff", "--stat", &meta.base]).await?;
        let (_, diff, _) = git(&wt, &["diff", &meta.base]).await?;
        Ok(json!({ "stat": stat, "diff": diff }))
    }

    /// Stable fingerprint of the worktree contents, used to tell whether a
    /// passing check still describes what would be committed.
    pub async fn fingerprint(&self) -> String {
        let (meta, _) = self.snapshot().await;
        if meta.worktree.is_empty() {
            return String::new();
        }
        let wt = PathBuf::from(&meta.worktree);
        let (_, head, _) = git(&wt, &["rev-parse", "HEAD"]).await.unwrap_or_default();
        let (_, status, _) = git(&wt, &["status", "--porcelain"])
            .await
            .unwrap_or_default();
        let (_, diff, _) = git(&wt, &["diff", "HEAD"]).await.unwrap_or_default();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        head.hash(&mut hasher);
        status.hash(&mut hasher);
        diff.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    /// How many commits the base branch has that this task does not.
    pub async fn behind(&self) -> u64 {
        let (meta, _) = self.snapshot().await;
        if meta.base_ref.is_empty() {
            return 0;
        }
        let wt = PathBuf::from(&meta.worktree);
        let range = format!("HEAD..{}", meta.base_ref);
        let (ok, out, _) = git(&wt, &["rev-list", "--count", &range]).await.unwrap_or((
            false,
            String::new(),
            String::new(),
        ));
        if ok {
            out.trim().parse().unwrap_or(0)
        } else {
            0
        }
    }

    /// Record that the task landed on the base branch.
    async fn mark_merged(&self) {
        self.set_status("merged").await;
    }

    /// Replay the task branch onto the current base branch. The endpoint
    /// entry refuses while the agent is busy; the autopilot calls the core
    /// directly, inside a turn it already owns.
    pub async fn rebase(self: &Arc<Self>) -> Result<Value> {
        if self.busy.load(Ordering::SeqCst) {
            return Err(anyhow!("the agent is still working on this task"));
        }
        self.rebase_core().await
    }

    pub(super) async fn rebase_core(&self) -> Result<Value> {
        let (meta, _) = self.snapshot().await;
        if meta.base_ref.is_empty() || meta.worktree.is_empty() {
            return Err(anyhow!("no base branch was recorded for this task"));
        }
        let wt = PathBuf::from(&meta.worktree);
        let (ok, _, err) = git(&wt, &["rebase", &meta.base_ref]).await?;
        if !ok {
            let _ = git(&wt, &["rebase", "--abort"]).await;
            return Err(anyhow!(
                "rebase onto `{}` failed and was aborted:\n{err}",
                meta.base_ref
            ));
        }
        self.invalidate_verify().await;
        Ok(json!({
            "ok": true,
            "message": format!("rebased onto `{}` — run verify again", meta.base_ref)
        }))
    }

    /// Throw away every change in the worktree.
    pub async fn reset(self: &Arc<Self>) -> Result<Value> {
        if self.busy.load(Ordering::SeqCst) {
            return Err(anyhow!("the agent is still working on this task"));
        }
        let (meta, _) = self.snapshot().await;
        if meta.worktree.is_empty() {
            return Err(anyhow!("this task has not started yet"));
        }
        let wt = PathBuf::from(&meta.worktree);
        let (ok, _, err) = git(&wt, &["reset", "--hard", &meta.base]).await?;
        if !ok {
            return Err(anyhow!("reset failed: {err}"));
        }
        let _ = git(&wt, &["clean", "-fd"]).await;
        self.invalidate_verify().await;
        Ok(json!({ "ok": true, "message": "worktree reset to the base commit" }))
    }

    async fn invalidate_verify(&self) {
        let mut meta = self.meta.write().await;
        meta.verify = None;
        meta.updated_at = now_ms();
        let snapshot = meta.clone();
        drop(meta);
        self.emit(json!({ "type": "task", "task": snapshot }));
        self.persist().await;
    }

    /// Commit the worktree, refusing when checks are configured but have not
    /// passed against exactly these contents. The decision comes from the
    /// same state machine the UI sees. `force` overrides a `needs_review`
    /// receipt (policy findings a human chose to accept); it never overrides
    /// a failed check or a stale fingerprint.
    pub(super) async fn commit_worktree(&self, force: bool) -> Result<TaskMeta> {
        let (meta, _) = self.snapshot().await;
        if meta.worktree.is_empty() {
            return Err(anyhow!("this task has not started yet"));
        }
        let has_checks = self
            .ws
            .upgrade()
            .map(|ws| {
                ws.settings
                    .try_read()
                    .map(|s| !s.checks.is_empty())
                    .unwrap_or(true)
            })
            .unwrap_or(false);
        if has_checks {
            let fingerprint = self.fingerprint().await;
            let state = classify(meta.verify.as_ref(), 0, true);
            let current = meta
                .verify
                .as_ref()
                .map(|v| !v.fingerprint.is_empty() && v.fingerprint == fingerprint)
                .unwrap_or(false);
            match (state, current) {
                (ReceiptState::Passed, true) => {}
                (ReceiptState::Passed, false) => {
                    return Err(anyhow!(
                        "the worktree changed since the last passing check — verify again"
                    ));
                }
                (ReceiptState::NeedsReview, true) => {
                    if !force {
                        let findings = meta
                            .verify
                            .as_ref()
                            .map(|v| v.policy.as_slice())
                            .unwrap_or_default()
                            .iter()
                            .map(|f| {
                                format!(
                                    "  {} `{}` in {}",
                                    f.get("kind").and_then(Value::as_str).unwrap_or("?"),
                                    f.get("pattern").and_then(Value::as_str).unwrap_or("?"),
                                    f.get("path").and_then(Value::as_str).unwrap_or("?"),
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        return Err(anyhow!(
                            "the diff policy flagged this change:\n{findings}\n\
                             resolve it, verify again, or merge with an explicit override"
                        ));
                    }
                }
                _ => return Err(anyhow!("this task has not passed its checks — run verify")),
            }
        }

        let wt = PathBuf::from(&meta.worktree);
        let _ = git(&wt, &["add", "-A"]).await;
        let (ok, _, err) = git(&wt, &["commit", "-m", &format!("orch: {}", meta.title)]).await?;
        if !ok {
            let (_, status, _) = git(&wt, &["status", "--porcelain"]).await?;
            if !status.is_empty() {
                return Err(anyhow!("commit failed: {err}"));
            }
        }
        Ok(meta)
    }

    /// Commit, then fast-forward the repository onto the task branch when it
    /// is safe to do so.
    pub async fn merge(self: &Arc<Self>, force: bool) -> Result<Value> {
        // A receipt describes one base. If the base moved, the receipt no
        // longer describes what would land, so merge refuses and names the
        // way back: rebase, then verify against the new base.
        let behind = self.behind().await;
        if behind > 0 {
            return Err(anyhow!(
                "the base branch moved ahead by {behind} commits — rebase and verify again before merging"
            ));
        }
        let meta = self.commit_worktree(force).await?;
        let repo = self
            .ws
            .upgrade()
            .map(|ws| ws.repo.path.clone())
            .unwrap_or_else(|| meta.worktree.clone());
        let root = PathBuf::from(&repo);

        let (_, dirty, _) = git(&root, &["status", "--porcelain"]).await?;
        if !dirty.is_empty() {
            return Ok(json!({
                "ok": true,
                "merged": false,
                "message": format!(
                    "branch `{}` is committed. The repository has uncommitted \
                     changes, so merge it by hand:\n\n  git -C {} merge --no-ff {}",
                    meta.branch, repo, meta.branch
                )
            }));
        }

        let message = format!("orch: {}", meta.title);
        let (ok, _, err) = git(&root, &["merge", "--no-ff", "-m", &message, &meta.branch]).await?;
        if !ok {
            let _ = git(&root, &["merge", "--abort"]).await;
            return Err(anyhow!("merge failed and was aborted:\n{err}"));
        }
        if force {
            self.push_item(json!({
                "kind": "note",
                "id": format!("n_{}", uuid::Uuid::new_v4().simple()),
                "at": now_ms(),
                "text": "merged with a policy override — the diff policy findings were accepted by a human",
                "tone": "warn",
            }))
            .await;
        }
        self.mark_merged().await;
        self.persist().await;
        Ok(json!({
            "ok": true,
            "merged": true,
            "message": format!("merged `{}` into `{}`", meta.branch, meta.base_ref)
        }))
    }
}
