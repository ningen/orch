//! Tasks: one git worktree plus one live ACP agent session.

use crate::acp::{self, Acp, Incoming};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

#[derive(Clone)]
pub struct Config {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoInfo {
    pub path: String,
    pub name: String,
    pub branch: String,
}

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
}

#[derive(Serialize, Deserialize)]
struct Stored {
    task: TaskMeta,
    items: Vec<Value>,
}

pub struct Hub {
    pub repo: RepoInfo,
    pub dir: PathBuf,
    pub cfg: Config,
    tx: broadcast::Sender<String>,
    pub tasks: RwLock<BTreeMap<String, Arc<Task>>>,
    models: RwLock<Vec<Value>>,
    default_model: RwLock<Option<String>>,
}

pub struct Task {
    pub meta: RwLock<TaskMeta>,
    pub items: RwLock<Vec<Value>>,
    hub: Weak<Hub>,
    agent: Mutex<Option<Arc<Acp>>>,
    session: RwLock<Option<String>>,
    spawn_lock: Mutex<()>,
    busy: AtomicBool,
    restored: AtomicBool,
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

fn dir_key(path: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    format!("{:016x}", h.finish())
}

async fn git(cwd: &Path, args: &[&str]) -> Result<(bool, String, String)> {
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

impl Hub {
    pub async fn open(repo_path: &Path, dir: PathBuf, cfg: Config) -> Result<Arc<Hub>> {
        let path = std::fs::canonicalize(repo_path)?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo")
            .to_string();
        let branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(&path)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "HEAD".into());

        let (tx, _) = broadcast::channel(1024);
        let hub = Arc::new(Hub {
            repo: RepoInfo {
                path: path.to_string_lossy().to_string(),
                name,
                branch,
            },
            dir,
            cfg,
            tx,
            tasks: RwLock::new(BTreeMap::new()),
            models: RwLock::new(Vec::new()),
            default_model: RwLock::new(None),
        });
        hub.reload().await?;
        Ok(hub)
    }

    async fn reload(self: &Arc<Self>) -> Result<()> {
        let tasks_dir = self.dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir)?;
        let mut map = BTreeMap::new();
        for entry in std::fs::read_dir(&tasks_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(mut stored) = serde_json::from_str::<Stored>(&text) else {
                continue;
            };
            // Agent processes do not survive a restart.
            stored.task.status = "idle".into();
            for item in stored.items.iter_mut() {
                if item.get("streaming").and_then(Value::as_bool) == Some(true) {
                    item["streaming"] = json!(false);
                }
            }
            let task = Arc::new(Task {
                meta: RwLock::new(stored.task.clone()),
                items: RwLock::new(stored.items),
                hub: Arc::downgrade(self),
                agent: Mutex::new(None),
                session: RwLock::new(None),
                spawn_lock: Mutex::new(()),
                busy: AtomicBool::new(false),
                restored: AtomicBool::new(true),
            });
            map.insert(stored.task.id.clone(), task);
        }
        *self.tasks.write().await = map;

        // Seed the model catalogue from whatever the last session saw.
        let mut models = Vec::new();
        let mut default = None;
        for task in self.tasks.read().await.values() {
            let meta = task.meta.read().await;
            if models.is_empty() && !meta.models.is_empty() {
                models = meta.models.clone();
            }
            if default.is_none() {
                default = meta.model.clone();
            }
        }
        *self.models.write().await = models;
        *self.default_model.write().await = default;
        Ok(())
    }

    pub fn emit(&self, value: Value) {
        let _ = self.tx.send(value.to_string());
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    pub async fn list(&self) -> Vec<TaskMeta> {
        let tasks = self.tasks.read().await;
        let mut out: Vec<TaskMeta> = Vec::with_capacity(tasks.len());
        for t in tasks.values() {
            out.push(t.meta.read().await.clone());
        }
        out.sort_by_key(|m| m.created_at);
        out
    }

    pub async fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.read().await.get(id).cloned()
    }

    pub async fn create(
        self: &Arc<Self>,
        title: String,
        model: Option<String>,
    ) -> Result<Arc<Task>> {
        let id = short_id();
        let branch = format!("orch/{}-{}", slug(&title), &id[2..]);
        let worktree = self.dir.join("worktrees").join(&id);

        let repo = PathBuf::from(&self.repo.path);
        let (ok, base, err) = git(&repo, &["rev-parse", "HEAD"]).await?;
        if !ok {
            return Err(anyhow!("cannot read HEAD: {err}"));
        }
        if let Some(parent) = worktree.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let wt = worktree.to_string_lossy().to_string();
        let (ok, _, err) = git(&repo, &["worktree", "add", "-b", &branch, &wt, &base]).await?;
        if !ok {
            return Err(anyhow!("git worktree add failed: {err}"));
        }

        let ts = now_ms();
        let known_models = self.models.read().await.clone();
        let known_default = self.default_model.read().await.clone();
        let model = model.or(known_default);
        let task = Arc::new(Task {
            meta: RwLock::new(TaskMeta {
                id: id.clone(),
                title,
                branch,
                worktree: wt,
                base,
                status: "idle".into(),
                model,
                models: known_models,
                usage: None,
                created_at: ts,
                updated_at: ts,
            }),
            items: RwLock::new(Vec::new()),
            hub: Arc::downgrade(self),
            agent: Mutex::new(None),
            session: RwLock::new(None),
            spawn_lock: Mutex::new(()),
            busy: AtomicBool::new(false),
            restored: AtomicBool::new(false),
        });

        self.tasks.write().await.insert(id.clone(), task.clone());
        task.persist().await;
        self.emit(json!({ "type": "task", "task": *task.meta.read().await }));
        Ok(task)
    }
}

impl Task {
    /// Broadcast an event to every connected client.
    pub fn emit(&self, value: Value) {
        if let Some(hub) = self.hub.upgrade() {
            hub.emit(value);
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

    async fn persist(&self) {
        let (task, items) = self.snapshot().await;
        let Some(hub) = self.hub.upgrade() else {
            return;
        };
        let dir = hub.dir.join("tasks");
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

    async fn push_item(&self, item: Value) {
        self.items.write().await.push(item.clone());
        self.emit(json!({
            "type": "item",
            "taskId": self.id_str().await,
            "item": item
        }));
    }

    async fn id_str(&self) -> String {
        self.meta.read().await.id.clone()
    }

    async fn error(&self, message: &str) {
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
            if let Err(e) = task.run_turn(text).await {
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

        let Some(hub) = self.hub.upgrade() else {
            return Err(anyhow!("orch is shutting down"));
        };
        let (meta, _) = self.snapshot().await;
        let (tx, rx) = mpsc::channel(512);
        let cwd = PathBuf::from(&meta.worktree);
        let acp = Acp::spawn(&hub.cfg.program, &hub.cfg.args, &cwd, tx)?;

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

        let session = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            acp.request(
                "session/new",
                json!({ "cwd": meta.worktree, "mcpServers": [] }),
            ),
        )
        .await
        .map_err(|_| anyhow!("the agent did not create a session"))??;

        let sid = session
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("agent returned no sessionId"))?
            .to_string();

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
            if let Some(hub) = self.hub.upgrade() {
                *hub.models.write().await = models.clone();
                let mut default = hub.default_model.write().await;
                if default.is_none() {
                    *default = self.meta.read().await.model.clone();
                }
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
        *self.session.write().await = Some(sid);

        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            pump(weak, rx).await;
        });

        let _ = init;
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
            if let Some(opts) = reply.get("configOptions").and_then(Value::as_array) {
                if let Some(list) = opts
                    .iter()
                    .find(|o| o.get("category").and_then(Value::as_str) == Some("model"))
                    .and_then(|o| o.get("options"))
                    .and_then(Value::as_array)
                {
                    let mut m = self.meta.write().await;
                    m.models = list.clone();
                    let snapshot = m.clone();
                    drop(m);
                    self.emit(json!({ "type": "task", "task": snapshot }));
                }
            }
        }
        self.persist().await;
        Ok(())
    }

    async fn on_update(self: &Arc<Self>, params: Value) {
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
}

async fn pump(task: Weak<Task>, mut rx: mpsc::Receiver<Incoming>) {
    while let Some(msg) = rx.recv().await {
        let Some(task) = task.upgrade() else { break };
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
        if let Some(v) = update.get(key) {
            if !v.is_null() {
                item[key] = v.clone();
            }
        }
    }
    if let Some(kind) = update.get("kind") {
        if !kind.is_null() {
            item["toolKind"] = kind.clone();
        }
    }
    if let Some(input) = update.get("rawInput") {
        if input.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            item["input"] = input.clone();
        }
    }
    if let Some(locs) = update.get("locations").and_then(Value::as_array) {
        let paths: Vec<Value> = locs.iter().filter_map(|l| l.get("path").cloned()).collect();
        if !paths.is_empty() {
            item["locations"] = json!(paths);
        }
    }
    let mut out = item["output"].as_str().unwrap_or("").to_string();
    if let Some(raw) = update.get("rawOutput").and_then(|r| r.get("output")) {
        if let Some(s) = raw.as_str() {
            out = s.to_string();
        }
    }
    if out.is_empty() {
        if let Some(content) = update.get("content").and_then(Value::as_array) {
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
    }
    item["output"] = json!(out);
}

impl Task {
    pub async fn diff(&self) -> Result<Value> {
        let (meta, _) = self.snapshot().await;
        let wt = PathBuf::from(&meta.worktree);
        let _ = git(&wt, &["add", "-N", "-A"]).await;
        let (_, stat, _) = git(&wt, &["diff", "--stat", &meta.base]).await?;
        let (_, diff, _) = git(&wt, &["diff", &meta.base]).await?;
        Ok(json!({ "stat": stat, "diff": diff }))
    }

    pub async fn commit(&self) -> Result<Value> {
        let (meta, _) = self.snapshot().await;
        let wt = PathBuf::from(&meta.worktree);
        let _ = git(&wt, &["add", "-A"]).await;
        let (ok, _, err) = git(&wt, &["commit", "-m", &format!("orch: {}", meta.title)]).await?;
        let (_, status, _) = git(&wt, &["status", "--porcelain"]).await?;
        if !ok && !status.is_empty() {
            return Err(anyhow!("commit failed: {err}"));
        }
        let repo = self
            .hub
            .upgrade()
            .map(|h| h.repo.path.clone())
            .unwrap_or_else(|| meta.worktree.clone());
        let message = format!(
            "branch `{}` is ready. Merge it with:\n\n  git -C {} merge --no-ff {}",
            meta.branch, repo, meta.branch
        );
        Ok(json!({ "ok": true, "message": message }))
    }

    pub async fn remove(self: &Arc<Self>) -> Result<()> {
        let (meta, _) = self.snapshot().await;
        if let Some(hub) = self.hub.upgrade() {
            hub.tasks.write().await.remove(&meta.id);
            let repo = PathBuf::from(&hub.repo.path);
            let wt = PathBuf::from(&meta.worktree);
            let _ = git(&repo, &["worktree", "remove", "--force", &meta.worktree]).await;
            let _ = std::fs::remove_dir_all(&wt);
            let _ = std::fs::remove_file(hub.dir.join("tasks").join(format!("{}.json", meta.id)));
            hub.emit(json!({ "type": "task", "task": null, "removed": meta.id }));
        }
        *self.agent.lock().await = None;
        Ok(())
    }
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
