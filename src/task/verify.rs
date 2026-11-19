//! The gate: verify commands, the diff-policy scan, and arbitrary commands
//! streamed in the task worktree.

use super::{Task, now_ms};
use crate::settings::{Settings, glob_match};
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;

impl Task {
    pub async fn settings(&self) -> Settings {
        match self.ws.upgrade() {
            Some(ws) => ws.settings.read().await.clone(),
            None => Settings::default(),
        }
    }

    /// Run a shell command in the worktree, streaming its output.
    ///
    /// Returns the exit code and a bounded tail of everything printed, which
    /// is what the verification cards keep.
    pub async fn exec(self: &Arc<Self>, command: &str, run_id: &str) -> Result<(i32, String)> {
        let (meta, _) = self.snapshot().await;
        if meta.worktree.is_empty() {
            return Err(anyhow!("this task has not started yet"));
        }
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&meta.worktree)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.emit(json!({
                    "type": "run", "taskId": meta.id, "runId": run_id,
                    "stream": "stderr", "text": format!("{error}\n")
                }));
                self.emit(json!({
                    "type": "run", "taskId": meta.id, "runId": run_id,
                    "stream": "exit", "text": "", "code": -1
                }));
                return Err(error).with_context(|| format!("cannot run `{command}`"));
            }
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (tx, mut rx) = mpsc::channel::<(String, String)>(256);
        if let Some(out) = stdout {
            let tx = tx.clone();
            tokio::spawn(pump_pipe(out, "stdout", tx));
        }
        if let Some(err) = stderr {
            let tx = tx.clone();
            tokio::spawn(pump_pipe(err, "stderr", tx));
        }
        drop(tx);

        let task = self.clone();
        let task_id = meta.id.clone();
        let run = run_id.to_string();
        let drain = tokio::spawn(async move {
            let mut tail = String::new();
            while let Some((stream, text)) = rx.recv().await {
                task.emit(json!({
                    "type": "run", "taskId": task_id, "runId": run,
                    "stream": stream, "text": text
                }));
                if tail.len() < 64 * 1024 {
                    tail.push_str(&text);
                }
            }
            tail
        });

        let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
        let tail = drain.await.unwrap_or_default();
        self.emit(json!({
            "type": "run", "taskId": meta.id, "runId": run_id,
            "stream": "exit", "text": "", "code": code
        }));
        Ok((code, tail))
    }

    async fn patch_item<F: FnOnce(&mut Value)>(&self, id: &str, patch: F) {
        let task_id = self.id_str().await;
        let mut items = self.items.write().await;
        if let Some(index) = items.iter().position(|i| i["id"] == json!(id)) {
            patch(&mut items[index]);
            let item = items[index].clone();
            drop(items);
            self.emit(json!({ "type": "item", "taskId": task_id, "item": item }));
        }
    }

    /// Scan the worktree diff against the repository's diff policy.
    ///
    /// Policy is evidence for a human, not a verdict: findings are attached
    /// to the verify receipt and make it say `needs_review` even when every
    /// check passed. Merging then needs an explicit override.
    async fn policy_scan(&self, settings: &Settings) -> Result<Vec<Value>> {
        if settings.policy_paths.is_empty() && settings.policy_lines.is_empty() {
            return Ok(Vec::new());
        }
        let (meta, _) = self.snapshot().await;
        let wt = PathBuf::from(&meta.worktree);
        let base = &meta.base;

        let (_, paths_out, _) = super::git::git(&wt, &["diff", "--name-only", "-z", base]).await?;
        let mut findings = Vec::new();
        for path in paths_out.split('\0').filter(|p| !p.is_empty()) {
            for pattern in &settings.policy_paths {
                if glob_match(pattern, path) {
                    findings.push(json!({
                        "kind": "path",
                        "pattern": pattern,
                        "path": path,
                    }));
                }
            }
        }

        if !settings.policy_lines.is_empty() {
            let (_, diff, _) = super::git::git(&wt, &["diff", "-U0", base]).await?;
            let mut file = String::new();
            let mut hits: std::collections::BTreeMap<String, Vec<String>> =
                std::collections::BTreeMap::new();
            for line in diff.lines() {
                if let Some(rest) = line.strip_prefix("+++ b/") {
                    file = rest.to_string();
                } else if line.starts_with('+') && !line.starts_with("+++") {
                    for pattern in &settings.policy_lines {
                        if line.contains(pattern.as_str()) {
                            hits.entry(pattern.clone()).or_default().push(file.clone());
                        }
                    }
                }
            }
            for (pattern, files) in hits {
                let mut unique = files;
                unique.sort();
                unique.dedup();
                findings.push(json!({
                    "kind": "line",
                    "pattern": pattern,
                    "path": unique.join(", "),
                }));
            }
        }
        Ok(findings)
    }

    /// Run every configured check, in order, against the current worktree.
    /// The endpoint entry; the autopilot calls [`Task::run_verify`] directly.
    pub async fn verify(self: &Arc<Self>) -> Result<()> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(anyhow!("the agent is still working on this task"));
        }
        let task = self.clone();
        tokio::spawn(async move {
            if let Err(error) = task.run_verify().await {
                task.push_item(json!({
                    "kind": "note",
                    "id": format!("n_{}", uuid::Uuid::new_v4().simple()),
                    "at": now_ms(),
                    "text": format!("verify: {error:#}"),
                    "tone": "error",
                }))
                .await;
            }
            task.busy.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    pub(super) async fn run_verify(self: &Arc<Self>) -> Result<()> {
        let Some(ws) = self.ws.upgrade() else {
            return Err(anyhow!("this workspace is closed"));
        };
        let settings = ws.settings.read().await.clone();
        if settings.checks.is_empty() {
            return Err(anyhow!(
                "no checks are configured for this repository — add one in the verify panel"
            ));
        }
        for check in &settings.checks {
            settings.check_command(check)?;
        }

        // Flake evaluation reads the index, so make new files visible first.
        self.stage_intent().await.ok();
        let fingerprint = self.fingerprint().await;
        let wt = PathBuf::from(&self.meta.read().await.worktree);
        let (_, head_sha, _) = super::git::git(&wt, &["rev-parse", "HEAD"]).await?;
        let meta_base = self.meta.read().await.base_ref.clone();
        let base_sha = match super::git::git(&wt, &["rev-parse", &meta_base]).await {
            Ok((true, out, _)) => out,
            _ => String::new(),
        };
        let findings = self.policy_scan(&settings).await?;

        let id = format!("v_{}", uuid::Uuid::new_v4().simple());
        self.push_item(json!({
            "kind": "tool",
            "id": id,
            "at": now_ms(),
            "title": "verify",
            "toolKind": "verify",
            "status": "in_progress",
            "locations": [],
            "input": {
                "checks": settings.checks,
                "policyPaths": settings.policy_paths,
                "policyLines": settings.policy_lines,
            },
            "output": "",
        }))
        .await;

        if !findings.is_empty() {
            let mut head = String::from("diff policy — needs a human look:\n");
            for finding in &findings {
                head.push_str(&format!(
                    "  {} (pattern `{}`, path {})\n",
                    finding["kind"].as_str().unwrap_or("?"),
                    finding["pattern"].as_str().unwrap_or("?"),
                    finding["path"].as_str().unwrap_or("?"),
                ));
            }
            let snapshot = head.clone();
            let item_id = id.clone();
            self.patch_item(&item_id, move |item| {
                item["output"] = json!(snapshot);
            })
            .await;
        }

        let mut results = Vec::new();
        let mut transcript = String::new();
        let mut failed = false;

        for (index, command) in settings.checks.iter().enumerate() {
            let run_id = format!("{id}-{index}");
            let (code, output) = self.exec(command, &run_id).await?;
            if !transcript.is_empty() {
                transcript.push('\n');
            }
            transcript.push_str(&format!("$ {command}\n"));
            transcript.push_str(&output);
            if code != 0 {
                transcript.push_str(&format!("[exit {code}]\n"));
                failed = true;
            }
            results.push(json!({ "command": command, "code": code }));

            let status = if failed { "failed" } else { "in_progress" };
            let snapshot = transcript.clone();
            let label = status.to_string();
            let item_id = id.clone();
            self.patch_item(&item_id, move |item| {
                item["status"] = json!(label);
                item["output"] = json!(snapshot);
            })
            .await;

            if failed {
                break;
            }
        }

        let status = if failed {
            "failed"
        } else if !findings.is_empty() {
            "needs_review"
        } else {
            "passed"
        };
        let label = status.to_string();
        let item_id = id.clone();
        self.patch_item(&item_id, move |item| {
            item["status"] = json!(label);
        })
        .await;

        {
            let mut meta = self.meta.write().await;
            meta.verify = Some(super::Verify {
                status: status.to_string(),
                at: now_ms(),
                fingerprint: if failed { String::new() } else { fingerprint },
                head_sha,
                base_sha,
                commands: results,
                policy: findings,
            });
            meta.updated_at = now_ms();
            let snapshot = meta.clone();
            drop(meta);
            self.emit(json!({ "type": "task", "task": snapshot }));
        }
        self.persist().await;

        if failed {
            return Err(anyhow!("a check failed"));
        }
        Ok(())
    }
}

async fn pump_pipe<R>(mut pipe: R, name: &'static str, tx: mpsc::Sender<(String, String)>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    let mut pending = String::new();
    loop {
        match pipe.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                pending.push_str(&String::from_utf8_lossy(&buf[..n]));
                while let Some(idx) = pending.find('\n') {
                    let line: String = pending.drain(..=idx).collect();
                    if tx.send((name.to_string(), line)).await.is_err() {
                        return;
                    }
                }
            }
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        let _ = tx.send((name.to_string(), pending)).await;
    }
}
