//! Workspaces, tasks, and agent sessions.
//!
//! A workspace is one git repository. A task is one isolated git worktree plus
//! one live ACP agent session inside that workspace. Several workspaces can be
//! open at once, and their tasks keep running while you switch between them.
//!
//! The module is split by responsibility: this file holds the types and the
//! conversation core, [`workspace`] the registry, [`git`] the worktree
//! operations, [`verify`] the gate, and [`autopilot`] what the system decides
//! on its own after a turn ends.

mod autopilot;
mod git;
mod verify;
mod workspace;

pub use workspace::{Config, Orch, Workspace};

use crate::acp::{self, Acp, Incoming};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock, mpsc};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskMeta {
    pub id: String,
    pub title: String,
    pub branch: String,
    pub worktree: String,
    pub base: String,
    pub status: String,
    pub model: Option<String>,
    pub models: Vec<Value>,
    pub usage: Option<Value>,
    pub created_at: u64,
    pub updated_at: u64,
    /// Branch this task was cut from, used for "behind" and rebase.
    #[serde(default)]
    pub base_ref: String,
    /// Agent session to resume after a restart.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Result of the last verification run.
    #[serde(default)]
    pub verify: Option<Verify>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Verify {
    /// "passed", "failed", or "needs_review" (checks green, policy flagged).
    pub status: String,
    pub at: u64,
    /// Fingerprint of the working tree that produced this result.
    pub fingerprint: String,
    /// The commit the task branch was at when this ran.
    #[serde(default)]
    pub head_sha: String,
    /// The base branch commit it was evaluated against.
    #[serde(default)]
    pub base_sha: String,
    pub commands: Vec<Value>,
    /// Diff-policy findings: what a human should look at before merging.
    #[serde(default)]
    pub policy: Vec<Value>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Stored {
    task: TaskMeta,
    items: Vec<Value>,
}

pub struct Task {
    pub meta: RwLock<TaskMeta>,
    pub items: RwLock<Vec<Value>>,
    pub(crate) ws: Weak<Workspace>,
    agent: Mutex<Option<Arc<Acp>>>,
    pub(crate) session: RwLock<Option<String>>,
    /// Session used only to name the task from its first message. Updates for
    /// it are diverted into `title_buffer` instead of the transcript.
    title_session: RwLock<Option<String>>,
    title_buffer: Mutex<String>,
    spawn_lock: Mutex<()>,
    pub(crate) busy: AtomicBool,
    restored: AtomicBool,
    /// True while an ACP session is being resumed, when replayed history must
    /// not be appended to the transcript again.
    replaying: AtomicBool,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn short_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    format!("t_{}", &u[..6])
}

fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 24 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "task".into() } else { out }
}

/// A short, single-line title from whatever the naming model replied with.
fn clean_title(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let line = line
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '#' || c.is_whitespace())
        .trim_start_matches("Title:")
        .trim();
    let mut out: String = line.chars().take(72).collect();
    if line.chars().count() > 72 {
        out = out.trim_end().to_string();
    }
    out.trim_matches(|c: char| c == '.' || c == '。')
        .to_string()
}

fn fallback_title(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("task");
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = collapsed.chars().take(60).collect();
    if short.trim().is_empty() {
        "task".into()
    } else {
        short
    }
}

/// Pick a small model from the agent's catalogue for naming tasks.
fn pick_title_model(options: &[Value]) -> Option<String> {
    let hints = [
        "flash",
        "haiku",
        "mini",
        "small",
        "lightning",
        "lite",
        "nano",
        "free",
    ];
    for hint in hints {
        for option in options {
            let value = option.get("value").and_then(Value::as_str).unwrap_or("");
            let name = option.get("name").and_then(Value::as_str).unwrap_or("");
            if value.to_lowercase().contains(hint) || name.to_lowercase().contains(hint) {
                return Some(value.to_string());
            }
        }
    }
    options
        .first()
        .and_then(|o| o.get("value"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn dir_key(path: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    format!("{:016x}", h.finish())
}

/* ---------------------------------------------------------------- Task core */

impl Task {
    /// Assemble a task. `restored` marks one reloaded from disk, whose next
    /// message must warn that agent processes do not survive a restart.
    pub(crate) fn new(
        meta: TaskMeta,
        items: Vec<Value>,
        ws: &Arc<Workspace>,
        restored: bool,
    ) -> Task {
        Task {
            meta: RwLock::new(meta),
            items: RwLock::new(items),
            ws: Arc::downgrade(ws),
            agent: Mutex::new(None),
            session: RwLock::new(None),
            title_session: RwLock::new(None),
            title_buffer: Mutex::new(String::new()),
            spawn_lock: Mutex::new(()),
            busy: AtomicBool::new(false),
            restored: AtomicBool::new(restored),
            replaying: AtomicBool::new(false),
        }
    }

    /// Broadcast an event to every connected client.
    pub fn emit(&self, value: Value) {
        if let Some(ws) = self.ws.upgrade() {
            ws.emit(value);
        }
    }

    pub async fn set_status(&self, status: &str) {
        let mut m = self.meta.write().await;
        if m.status == status {
            return;
        }
        m.status = status.to_string();
        m.updated_at = now_ms();
        let snapshot = m.clone();
        drop(m);
        self.emit(json!({ "type": "task", "task": snapshot }));
    }

    pub async fn snapshot(&self) -> (TaskMeta, Vec<Value>) {
        let meta = self.meta.read().await.clone();
        let items = self.items.read().await.clone();
        (meta, items)
    }

    pub(crate) async fn persist(&self) {
        let (task, items) = self.snapshot().await;
        let Some(ws) = self.ws.upgrade() else {
            return;
        };
        let dir = ws.dir.join("tasks");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let stored = Stored {
            task: task.clone(),
            items,
        };
        if let Ok(text) = serde_json::to_string(&stored) {
            let path = dir.join(format!("{}.json", task.id));
            let tmp = dir.join(format!("{}.json.tmp", task.id));
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    pub(crate) async fn push_item(&self, item: Value) {
        self.items.write().await.push(item.clone());
        self.emit(json!({
            "type": "item",
            "taskId": self.id_str().await,
            "item": item
        }));
    }

    pub(crate) async fn id_str(&self) -> String {
        self.meta.read().await.id.clone()
    }

    pub(crate) async fn error(&self, message: &str) {
        let id = self.id_str().await;
        let item = json!({
            "kind": "note",
            "id": format!("n_{}", uuid::Uuid::new_v4().simple()),
            "at": now_ms(),
            "text": message,
            "tone": "error",
        });
        self.items.write().await.push(item.clone());
        self.emit(json!({ "type": "item", "taskId": id, "item": item }));
        self.set_status("error").await;
        self.persist().await;
    }

    /// Create the worktree and, when the task has no title yet, name it from
    /// the first message using a small model. Safe to call more than once.
    async fn initialize(self: &Arc<Self>, first_prompt: &str) -> Result<()> {
        let Some(ws) = self.ws.upgrade() else {
            return Err(anyhow!("this workspace is closed"));
        };
        let meta = self.meta.read().await.clone();
        let known_title = meta.title.trim().to_string();
        if !meta.worktree.is_empty() && !known_title.is_empty() {
            return Ok(());
        }

        let id = meta.id.clone();
        let short = id.strip_prefix("t_").unwrap_or(&id).to_string();

        let (branch, wt) = if meta.worktree.is_empty() {
            let branch = if known_title.is_empty() {
                format!("orch/task-{short}")
            } else {
                format!("orch/{}-{short}", slug(&known_title))
            };
            let repo = PathBuf::from(&ws.repo.path);
            let (ok, base, err) = git::git(&repo, &["rev-parse", "HEAD"]).await?;
            if !ok {
                return Err(anyhow!("cannot read HEAD: {err}"));
            }
            let worktree = ws.dir.join("worktrees").join(&id);
            if let Some(parent) = worktree.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let wt = worktree.to_string_lossy().to_string();
            let (ok, _, err) =
                git::git(&repo, &["worktree", "add", "-b", &branch, &wt, &base]).await?;
            if !ok {
                return Err(anyhow!("git worktree add failed: {err}"));
            }
            {
                let mut m = self.meta.write().await;
                m.branch = branch.clone();
                m.worktree = wt.clone();
                m.base = base;
                m.base_ref = ws.repo.branch.clone();
                m.updated_at = now_ms();
            }
            self.persist().await;
            self.emit(json!({ "type": "task", "task": *self.meta.read().await }));
            if !known_title.is_empty() {
                return Ok(());
            }
            (branch, wt)
        } else {
            (meta.branch.clone(), meta.worktree.clone())
        };

        // Name the task from the first message. Any failure falls back to the
        // message itself so the task can still start.
        let generated = async {
            let agent = self.ensure_agent().await?;
            self.title_from_agent(&agent, first_prompt).await
        }
        .await
        .ok()
        .filter(|title| !title.is_empty());
        let title = generated.unwrap_or_else(|| fallback_title(first_prompt));

        let renamed = format!("orch/{}-{short}", slug(&title));
        let mut final_branch = branch;
        if renamed != final_branch {
            let (ok, _, err) = git::git(Path::new(&wt), &["branch", "-m", &renamed]).await?;
            if ok {
                final_branch = renamed;
            } else {
                eprintln!("could not rename branch to {renamed}: {err}");
            }
        }
        {
            let mut m = self.meta.write().await;
            m.title = title;
            m.branch = final_branch;
            m.updated_at = now_ms();
        }
        self.persist().await;
        self.emit(json!({ "type": "task", "task": *self.meta.read().await }));
        Ok(())
    }

    /// Ask a small model for a title, using a throwaway session so the main
    /// conversation stays clean.
    async fn title_from_agent(self: &Arc<Self>, agent: &Arc<Acp>, prompt: &str) -> Result<String> {
        let meta = self.meta.read().await.clone();
        let session = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            agent.request(
                "session/new",
                json!({ "cwd": meta.worktree, "mcpServers": [] }),
            ),
        )
        .await
        .map_err(|_| anyhow!("the naming session timed out"))??;
        let sid = session
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("the agent returned no sessionId"))?
            .to_string();

        let options = session
            .get("configOptions")
            .and_then(Value::as_array)
            .and_then(|opts| {
                opts.iter()
                    .find(|o| o.get("category").and_then(Value::as_str) == Some("model"))
                    .and_then(|o| o.get("options"))
                    .and_then(Value::as_array)
            })
            .map(|list| list.as_slice())
            .unwrap_or_default();
        let model = self
            .ws
            .upgrade()
            .and_then(|ws| ws.orch.upgrade())
            .and_then(|orch| orch.cfg.title_model.clone())
            .or_else(|| pick_title_model(options));
        if let Some(model) = model {
            let _ = agent
                .request(
                    "session/set_config_option",
                    json!({ "sessionId": sid, "configId": "model", "value": model }),
                )
                .await;
        }

        *self.title_buffer.lock().await = String::new();
        *self.title_session.write().await = Some(sid.clone());

        let instruction = format!(
            "Name this coding task. Read the request and reply with a concise title \
             of 3 to 6 words in the same language as the request. Reply with the title \
             only: no quotes, no punctuation, no explanation, and do not use tools.\n\n\
             Request:\n{prompt}"
        );
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            agent.request(
                "session/prompt",
                json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": instruction }] }),
            ),
        )
        .await;
        result.map_err(|_| anyhow!("naming the task timed out"))??;

        // The reply has arrived, but the chunks that produced it travel a
        // different path (the pump task) and may still be in its queue.
        // Give them a moment to land in the buffer; otherwise the title is
        // read as empty and a later chunk leaks into the main transcript.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let text;
        loop {
            let taken = std::mem::take(&mut *self.title_buffer.lock().await);
            if !taken.is_empty() || tokio::time::Instant::now() >= deadline {
                text = taken;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Keep `title_session` set: session ids are unique, so any straggler
        // update for this session must keep flowing into the buffer rather
        // than into the transcript.
        let _ = agent
            .request("session/close", json!({ "sessionId": sid }))
            .await;
        Ok(clean_title(&text))
    }

    pub async fn prompt(self: &Arc<Self>, text: String) -> Result<()> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err(anyhow!("the agent is still working on this task"));
        }
        if self.restored.swap(false, Ordering::SeqCst) {
            self.push_item(json!({
                "kind": "note",
                "id": format!("n_{}", uuid::Uuid::new_v4().simple()),
                "at": now_ms(),
                "text": "agent processes do not survive a restart — this message starts a fresh session",
                "tone": "warn",
            }))
            .await;
        }
        self.push_item(json!({
            "kind": "user",
            "id": format!("u_{}", uuid::Uuid::new_v4().simple()),
            "at": now_ms(),
            "text": text,
        }))
        .await;
        self.set_status("running").await;
        self.persist().await;

        let task = self.clone();
        tokio::spawn(async move {
            let result = match task.initialize(&text).await {
                Ok(()) => task.run_turn(text).await,
                Err(error) => Err(error),
            };
            if let Err(e) = result {
                task.error(&format!("{e:#}")).await;
            }
            task.busy.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    async fn run_turn(self: &Arc<Self>, text: String) -> Result<()> {
        let agent = self.ensure_agent().await?;
        let sid = self
            .session
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("no session"))?;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60 * 60),
            agent.request(
                "session/prompt",
                json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": text }] }),
            ),
        )
        .await
        .map_err(|_| anyhow!("the turn timed out"))??;

        if let Some(usage) = result.get("usage") {
            let mut m = self.meta.write().await;
            m.usage = Some(usage.clone());
            let snapshot = m.clone();
            drop(m);
            self.emit(json!({ "type": "task", "task": snapshot }));
        }

        self.finish_streaming().await;
        let stop = result
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or("end_turn");
        match stop {
            "refusal" => self.error("the agent refused this turn").await,
            "cancelled" => self.set_status("idle").await,
            _ => self.set_status("done").await,
        }
        self.persist().await;

        // Now decide what still needs doing — and do whatever does not need
        // a human judgment: rebase, verify, and (with permission) merge.
        if stop != "refusal" && stop != "cancelled" {
            self.autopilot().await;
        }
        Ok(())
    }

    async fn finish_streaming(&self) {
        let id = self.id_str().await;
        let mut items = self.items.write().await;
        let mut changed = Vec::new();
        for item in items.iter_mut() {
            if item.get("streaming").and_then(Value::as_bool) == Some(true) {
                item["streaming"] = json!(false);
                changed.push(item.clone());
            }
        }
        drop(items);
        for item in changed {
            self.emit(json!({ "type": "item", "taskId": id, "item": item }));
        }
    }

    /// Spawn the agent for this task if it is not already running.
    async fn ensure_agent(self: &Arc<Self>) -> Result<Arc<Acp>> {
        if let Some(a) = self.agent.lock().await.as_ref() {
            return Ok(a.clone());
        }
        let _guard = self.spawn_lock.lock().await;
        if let Some(a) = self.agent.lock().await.as_ref() {
            return Ok(a.clone());
        }

        let Some(ws) = self.ws.upgrade() else {
            return Err(anyhow!("this workspace is closed"));
        };
        let Some(orch) = ws.orch.upgrade() else {
            return Err(anyhow!("orch is shutting down"));
        };

        let (meta, _) = self.snapshot().await;
        let (tx, rx) = mpsc::channel(512);
        let cwd = PathBuf::from(&meta.worktree);
        let acp = Acp::spawn(&orch.cfg.program, &orch.cfg.args, &cwd, tx)?;

        let init = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            acp.request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientCapabilities": acp::capabilities(),
                    "clientInfo": { "name": "orch", "version": env!("CARGO_PKG_VERSION") },
                }),
            ),
        )
        .await
        .map_err(|_| anyhow!("the agent did not answer initialize"))??;

        let can_load = init
            .get("agentCapabilities")
            .and_then(|c| c.get("loadSession"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Prefer resuming the previous session so a restart does not throw
        // away the conversation. Replayed history is dropped by the guard in
        // `on_update`.
        let mut session = Value::Null;
        let mut sid = String::new();
        if can_load && let Some(previous) = meta.session_id.clone() {
            self.replaying.store(true, Ordering::SeqCst);
            let loaded = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                acp.request(
                    "session/load",
                    json!({ "sessionId": previous, "cwd": meta.worktree, "mcpServers": [] }),
                ),
            )
            .await;
            self.replaying.store(false, Ordering::SeqCst);
            if let Ok(Ok(value)) = loaded {
                session = value;
                sid = previous;
            }
        }

        if sid.is_empty() {
            session = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                acp.request(
                    "session/new",
                    json!({ "cwd": meta.worktree, "mcpServers": [] }),
                ),
            )
            .await
            .map_err(|_| anyhow!("the agent did not create a session"))??;
            sid = session
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("agent returned no sessionId"))?
                .to_string();
        }

        let models = session
            .get("configOptions")
            .and_then(Value::as_array)
            .and_then(|opts| {
                opts.iter()
                    .find(|o| o.get("category").and_then(Value::as_str) == Some("model"))
                    .and_then(|o| o.get("options"))
                    .and_then(Value::as_array)
                    .cloned()
            })
            .unwrap_or_default();

        if !models.is_empty() {
            *ws.models.write().await = models.clone();
            let mut default = ws.default_model.write().await;
            if default.is_none() {
                *default = self.meta.read().await.model.clone();
            }
        }
        {
            let mut m = self.meta.write().await;
            if !models.is_empty() {
                m.models = models;
                if m.model.is_none() {
                    m.model = session
                        .get("configOptions")
                        .and_then(Value::as_array)
                        .and_then(|opts| {
                            opts.iter()
                                .find(|o| {
                                    o.get("category").and_then(Value::as_str) == Some("model")
                                })
                                .and_then(|o| o.get("currentValue"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        });
                }
            }
            m.updated_at = now_ms();
            let snapshot = m.clone();
            drop(m);
            self.emit(json!({ "type": "task", "task": snapshot }));
        }

        if let Some(model) = self.meta.read().await.model.clone() {
            let _ = acp
                .request(
                    "session/set_config_option",
                    json!({ "sessionId": sid, "configId": "model", "value": model }),
                )
                .await;
        }

        *self.agent.lock().await = Some(acp.clone());
        *self.session.write().await = Some(sid.clone());

        {
            let mut m = self.meta.write().await;
            if m.session_id.as_deref() != Some(sid.as_str()) {
                m.session_id = Some(sid);
                m.updated_at = now_ms();
                let snapshot = m.clone();
                drop(m);
                self.emit(json!({ "type": "task", "task": snapshot }));
            }
        }

        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            pump(weak, rx).await;
        });

        Ok(acp)
    }

    pub async fn cancel(&self) -> Result<()> {
        let sid = self.session.read().await.clone();
        let agent = self.agent.lock().await.clone();
        if let (Some(sid), Some(agent)) = (sid, agent) {
            agent
                .notify("session/cancel", json!({ "sessionId": sid }))
                .await?;
        }
        self.set_status("idle").await;
        Ok(())
    }

    pub async fn set_model(self: &Arc<Self>, value: String) -> Result<()> {
        // Remember the choice so the next task in this repository starts there.
        if let Some(ws) = self.ws.upgrade() {
            *ws.default_model.write().await = Some(value.clone());
        }
        {
            let mut m = self.meta.write().await;
            m.model = Some(value.clone());
            m.updated_at = now_ms();
            let snapshot = m.clone();
            drop(m);
            self.emit(json!({ "type": "task", "task": snapshot }));
        }
        let sid = self.session.read().await.clone();
        let agent = self.agent.lock().await.clone();
        if let (Some(sid), Some(agent)) = (sid, agent) {
            let reply = agent
                .request(
                    "session/set_config_option",
                    json!({ "sessionId": sid, "configId": "model", "value": value }),
                )
                .await?;
            if let Some(list) = reply
                .get("configOptions")
                .and_then(Value::as_array)
                .and_then(|opts| {
                    opts.iter()
                        .find(|o| o.get("category").and_then(Value::as_str) == Some("model"))
                        .and_then(|o| o.get("options"))
                        .and_then(Value::as_array)
                })
            {
                let mut m = self.meta.write().await;
                m.models = list.clone();
                let snapshot = m.clone();
                drop(m);
                self.emit(json!({ "type": "task", "task": snapshot }));
            }
        }
        self.persist().await;
        Ok(())
    }

    async fn on_update(self: &Arc<Self>, params: Value) {
        if self.replaying.load(Ordering::SeqCst) {
            return;
        }
        let title_sid = self.title_session.read().await.clone();
        if let Some(title_sid) = title_sid
            && params.get("sessionId").and_then(Value::as_str) == Some(title_sid.as_str())
        {
            if let Some(update) = params.get("update")
                && update.get("sessionUpdate").and_then(Value::as_str)
                    == Some("agent_message_chunk")
            {
                let text = chunk_text(update);
                if !text.is_empty() {
                    self.title_buffer.lock().await.push_str(&text);
                }
            }
            return;
        }
        let Some(update) = params.get("update") else {
            return;
        };
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("agent_message_chunk") => {
                let text = chunk_text(update);
                if !text.is_empty() {
                    self.append_chunk(update.get("messageId"), "agent", &text)
                        .await;
                }
            }
            Some("agent_thought_chunk") => {
                let text = chunk_text(update);
                if !text.is_empty() {
                    self.append_chunk(update.get("messageId"), "thought", &text)
                        .await;
                }
            }
            Some("tool_call") => self.upsert_tool(update, true).await,
            Some("tool_call_update") => self.upsert_tool(update, false).await,
            Some("usage_update") => {
                let mut m = self.meta.write().await;
                m.usage = Some(json!({
                    "used": update.get("used").cloned().unwrap_or(json!(0)),
                    "size": update.get("size").cloned().unwrap_or(json!(0)),
                    "cost": update.get("cost").and_then(|c| c.get("amount")).cloned().unwrap_or(json!(0)),
                }));
                let snapshot = m.clone();
                drop(m);
                self.emit(json!({ "type": "task", "task": snapshot }));
            }
            _ => {}
        }
    }

    async fn append_chunk(&self, message_id: Option<&Value>, kind: &str, text: &str) {
        let id = format!(
            "{}_{}",
            if kind == "agent" { "a" } else { "th" },
            message_id.and_then(Value::as_str).unwrap_or("0000")
        );
        let task_id = self.id_str().await;
        let created;
        {
            let mut items = self.items.write().await;
            if let Some(idx) = items.iter().position(|i| i["id"] == json!(id)) {
                let current = items[idx]["text"].as_str().unwrap_or("").to_string();
                items[idx]["text"] = json!(format!("{current}{text}"));
                created = false;
            } else {
                let item = if kind == "agent" {
                    json!({ "kind": "agent", "id": id, "at": now_ms(), "text": text, "streaming": true })
                } else {
                    json!({ "kind": "thought", "id": id, "at": now_ms(), "text": text })
                };
                items.push(item);
                created = true;
            }
        }
        if created {
            let item = self
                .items
                .read()
                .await
                .iter()
                .find(|i| i["id"] == json!(id))
                .cloned();
            if let Some(item) = item {
                self.emit(json!({ "type": "item", "taskId": task_id, "item": item }));
            }
        } else {
            self.emit(json!({
                "type": "delta",
                "taskId": task_id,
                "itemId": id,
                "text": text
            }));
        }
    }

    async fn upsert_tool(&self, update: &Value, create: bool) {
        let Some(call_id) = update.get("toolCallId").and_then(Value::as_str) else {
            return;
        };
        let task_id = self.id_str().await;
        let mut items = self.items.write().await;
        let idx = items.iter().position(|i| i["id"] == json!(call_id));
        let item = match idx {
            Some(idx) => {
                let item = &mut items[idx];
                merge_tool(item, update);
                item.clone()
            }
            None => {
                if !create {
                    return;
                }
                let mut item = json!({
                    "kind": "tool",
                    "id": call_id,
                    "at": now_ms(),
                    "title": update.get("title").cloned().unwrap_or(json!("tool")),
                    "toolKind": update.get("kind").cloned().unwrap_or(json!("other")),
                    "status": update.get("status").cloned().unwrap_or(json!("pending")),
                    "locations": [],
                    "input": update.get("rawInput").cloned().unwrap_or(json!({})),
                    "output": "",
                });
                merge_tool(&mut item, update);
                items.push(item.clone());
                item
            }
        };
        drop(items);
        self.emit(json!({ "type": "item", "taskId": task_id, "item": item }));
    }

    async fn on_request(self: &Arc<Self>, method: &str, params: Value) -> Value {
        match method {
            "session/request_permission" => {
                let options = params
                    .get("options")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let choice = options
                    .iter()
                    .find(|o| {
                        o.get("optionId")
                            .and_then(Value::as_str)
                            .map(|id| id.contains("allow"))
                            .unwrap_or(false)
                    })
                    .or_else(|| options.first());
                let title = params
                    .get("toolCall")
                    .and_then(|t| t.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("a tool call");
                match choice.and_then(|o| o.get("optionId").cloned()) {
                    Some(option_id) => {
                        self.push_item(json!({
                            "kind": "note",
                            "id": format!("n_{}", uuid::Uuid::new_v4().simple()),
                            "at": now_ms(),
                            "text": format!("approved automatically: {title}"),
                            "tone": "warn",
                        }))
                        .await;
                        json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
                    }
                    None => json!({ "outcome": { "outcome": "cancelled" } }),
                }
            }
            _ => Value::Null,
        }
    }

    async fn on_exit(&self) {
        *self.agent.lock().await = None;
        *self.session.write().await = None;
        if self.busy.load(Ordering::SeqCst) {
            self.error("the agent process exited").await;
        }
    }

    pub async fn remove(self: &Arc<Self>) -> Result<()> {
        let (meta, _) = self.snapshot().await;
        let Some(ws) = self.ws.upgrade() else {
            return Ok(());
        };
        ws.tasks.write().await.remove(&meta.id);
        let repo = PathBuf::from(&ws.repo.path);
        if !meta.worktree.is_empty() {
            let wt = PathBuf::from(&meta.worktree);
            let _ = git::git(&repo, &["worktree", "remove", "--force", &meta.worktree]).await;
            let _ = std::fs::remove_dir_all(&wt);
        }
        let _ = std::fs::remove_file(ws.dir.join("tasks").join(format!("{}.json", meta.id)));
        ws.emit(json!({ "type": "removed", "taskId": meta.id }));
        *self.agent.lock().await = None;
        Ok(())
    }
}

async fn pump(task: Weak<Task>, mut rx: mpsc::Receiver<Incoming>) {
    while let Some(msg) = rx.recv().await {
        let Some(task) = task.upgrade() else {
            break;
        };
        match msg {
            Incoming::Notification { method, params } => {
                if method == "session/update" {
                    task.on_update(params).await;
                }
            }
            Incoming::Request {
                method,
                params,
                reply,
            } => {
                let result = task.on_request(&method, params).await;
                let _ = reply.send(result);
            }
            Incoming::Stderr(line) => {
                if std::env::var_os("ORCH_DEBUG").is_some() {
                    eprintln!("[agent] {line}");
                }
            }
            Incoming::Exited => {
                task.on_exit().await;
                break;
            }
        }
    }
}

fn chunk_text(update: &Value) -> String {
    update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn merge_tool(item: &mut Value, update: &Value) {
    for key in ["title", "status"] {
        if let Some(v) = update.get(key).filter(|v| !v.is_null()) {
            item[key] = v.clone();
        }
    }
    if let Some(kind) = update.get("kind").filter(|k| !k.is_null()) {
        item["toolKind"] = kind.clone();
    }
    if let Some(input) = update
        .get("rawInput")
        .filter(|i| i.as_object().map(|o| !o.is_empty()).unwrap_or(false))
    {
        item["input"] = input.clone();
    }
    if let Some(paths) = update
        .get("locations")
        .and_then(Value::as_array)
        .map(|locs| {
            locs.iter()
                .filter_map(|l| l.get("path").cloned())
                .collect::<Vec<Value>>()
        })
        .filter(|paths: &Vec<Value>| !paths.is_empty())
    {
        item["locations"] = json!(paths);
    }
    let mut out = item["output"].as_str().unwrap_or("").to_string();
    if let Some(s) = update
        .get("rawOutput")
        .and_then(|r| r.get("output"))
        .and_then(Value::as_str)
    {
        out = s.to_string();
    }
    if out.is_empty()
        && let Some(content) = update.get("content").and_then(Value::as_array)
    {
        let mut buf = String::new();
        for block in content {
            if let Some(text) = block
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
            {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(text);
            }
        }
        out = buf;
    }
    item["output"] = json!(out);
}

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ORCH_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join(".orch")
}

pub fn project_dir(repo_path: &str) -> PathBuf {
    state_dir().join("projects").join(dir_key(repo_path))
}
