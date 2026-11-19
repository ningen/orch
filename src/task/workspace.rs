//! The daemon registry: every open repository, and how they persist.

use super::{Stored, Task, TaskMeta, git, now_ms, project_dir, short_id};
use crate::settings::Settings;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use tokio::sync::{RwLock, broadcast};

#[derive(Clone)]
pub struct Config {
    pub program: String,
    pub args: Vec<String>,
    /// Model used to name a task from its first message. When unset, a small
    /// model is picked from the agent's catalogue.
    pub title_model: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoInfo {
    pub path: String,
    pub name: String,
    pub branch: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Registry {
    #[serde(default)]
    repos: Vec<String>,
    #[serde(default)]
    current: Option<String>,
}

/// The daemon: every open repository plus one event stream for the UI.
pub struct Orch {
    pub cfg: Config,
    pub root: PathBuf,
    pub workspaces: RwLock<BTreeMap<String, Arc<Workspace>>>,
    current: RwLock<Option<String>>,
    tx: broadcast::Sender<String>,
}

/// One open git repository.
pub struct Workspace {
    pub repo: RepoInfo,
    pub dir: PathBuf,
    pub tasks: RwLock<BTreeMap<String, Arc<Task>>>,
    pub(crate) orch: Weak<Orch>,
    pub(crate) models: RwLock<Vec<Value>>,
    pub(crate) default_model: RwLock<Option<String>>,
    pub settings: RwLock<Settings>,
}

impl Orch {
    pub fn new(cfg: Config, root: PathBuf) -> Arc<Orch> {
        let (tx, _) = broadcast::channel(2048);
        Arc::new(Orch {
            cfg,
            root,
            workspaces: RwLock::new(BTreeMap::new()),
            current: RwLock::new(None),
            tx,
        })
    }

    pub fn emit(&self, value: Value) {
        let _ = self.tx.send(value.to_string());
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join("workspaces.json")
    }

    /// Reopen every repository the daemon saw last time.
    ///
    /// `first` is the directory orch was started in; it is opened when it is a
    /// repository. It only becomes the active one when `prefer_first` is set
    /// (an explicit `--repo`) or when there is no saved selection to restore.
    pub async fn restore(self: &Arc<Self>, first: Option<&Path>, prefer_first: bool) -> Result<()> {
        let registry: Registry = std::fs::read_to_string(self.registry_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();

        let mut order: Vec<String> = Vec::new();
        if let Some(path) = first {
            let canonical = std::fs::canonicalize(path)?;
            order.push(canonical.to_string_lossy().to_string());
        }
        for path in registry.repos {
            if !order.contains(&path) {
                order.push(path);
            }
        }

        for path in &order {
            if let Err(error) = self.open_workspace(Path::new(path)).await {
                eprintln!("skipping {path}: {error:#}");
            }
        }

        // Keep the user's last selection across restarts. Starting orch inside
        // some repository must not yank the UI away from the one they chose.
        let open: Vec<String> = self.workspaces.read().await.keys().cloned().collect();
        let saved = registry.current.filter(|path| open.contains(path));
        let fallback = open.first().cloned();
        let current = if prefer_first {
            first
                .and_then(|path| std::fs::canonicalize(path).ok())
                .map(|path| path.to_string_lossy().to_string())
                .filter(|path| open.contains(path))
        } else {
            saved
        }
        .or(fallback);
        if let Some(path) = current {
            self.set_current(&path).await;
        }
        Ok(())
    }

    /// Open a repository (or return the one already open).
    pub async fn open_workspace(self: &Arc<Self>, path: &Path) -> Result<Arc<Workspace>> {
        let canonical = std::fs::canonicalize(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        let (ok, top, err) = git::git(&canonical, &["rev-parse", "--show-toplevel"]).await?;
        if !ok || top.is_empty() {
            return Err(anyhow!(
                "{} is not a git repository ({err})",
                path.display()
            ));
        }
        let key = top.clone();
        if let Some(ws) = self.workspaces.read().await.get(&key) {
            return Ok(ws.clone());
        }

        let ws = Workspace::load(self, PathBuf::from(&top)).await?;
        self.workspaces.write().await.insert(key, ws.clone());
        self.save_registry().await;
        Ok(ws)
    }

    pub async fn workspace(&self, path: &str) -> Option<Arc<Workspace>> {
        self.workspaces.read().await.get(path).cloned()
    }

    pub async fn current(&self) -> Option<Arc<Workspace>> {
        let key = self.current.read().await.clone()?;
        self.workspace(&key).await
    }

    pub async fn current_path(&self) -> Option<String> {
        self.current.read().await.clone()
    }

    pub async fn set_current(&self, path: &str) {
        if self.workspaces.read().await.contains_key(path) {
            *self.current.write().await = Some(path.to_string());
            self.save_registry().await;
        }
    }

    /// Every workspace in a shape the UI can render directly.
    ///
    /// Tasks carry `behind` (how far the base branch has moved) and
    /// `receiptState` (what the receipt says about the work) so a client can
    /// classify and recommend without re-deriving the state machine.
    pub async fn summary(&self) -> Vec<Value> {
        let workspaces = self.workspaces.read().await;
        let mut out = Vec::with_capacity(workspaces.len());
        for ws in workspaces.values() {
            let mut tasks: Vec<Value> = Vec::new();
            for task in ws.tasks.read().await.values() {
                let meta = task.meta.read().await.clone();
                let behind = task.behind().await;
                let state = task.receipt_state_with(behind).await;
                let mut value = serde_json::to_value(&meta).unwrap_or_default();
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("behind".into(), json!(behind));
                    obj.insert("receiptState".into(), json!(state.as_str()));
                }
                tasks.push(value);
            }
            tasks.sort_by_key(|t| t.get("createdAt").and_then(Value::as_u64).unwrap_or(0));
            out.push(json!({
                "path": ws.repo.path,
                "name": ws.repo.name,
                "branch": ws.repo.branch,
                "tasks": tasks,
            }));
        }
        out
    }

    pub async fn find_task(&self, id: &str) -> Option<Arc<Task>> {
        for ws in self.workspaces.read().await.values() {
            if let Some(task) = ws.tasks.read().await.get(id) {
                return Some(task.clone());
            }
        }
        None
    }

    async fn save_registry(&self) {
        let repos: Vec<String> = self.workspaces.read().await.keys().cloned().collect();
        let current = self.current.read().await.clone();
        let registry = Registry { repos, current };
        if let Ok(text) = serde_json::to_string_pretty(&registry) {
            let _ = std::fs::create_dir_all(&self.root);
            let path = self.registry_path();
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    pub async fn close_workspace(&self, path: &str) {
        self.workspaces.write().await.remove(path);
        let is_current = self.current.read().await.as_deref() == Some(path);
        if is_current {
            let next = self.workspaces.read().await.keys().next().cloned();
            *self.current.write().await = next;
        }
        self.save_registry().await;
    }
}

impl Workspace {
    async fn load(orch: &Arc<Orch>, root: PathBuf) -> Result<Arc<Workspace>> {
        let path = root.to_string_lossy().to_string();
        let name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo")
            .to_string();
        let (_, branch, _) = git::git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;

        let dir = project_dir(&path);
        let settings = Settings::load(&dir);
        let tasks_dir = dir.join("tasks");
        std::fs::create_dir_all(&tasks_dir)?;
        std::fs::create_dir_all(dir.join("worktrees"))?;

        let ws = Arc::new(Workspace {
            repo: RepoInfo {
                path: path.clone(),
                name,
                branch,
            },
            dir,
            tasks: RwLock::new(BTreeMap::new()),
            orch: Arc::downgrade(orch),
            models: RwLock::new(Vec::new()),
            default_model: RwLock::new(None),
            settings: RwLock::new(settings),
        });

        let mut map = BTreeMap::new();
        for entry in std::fs::read_dir(&tasks_dir)? {
            let file = entry?.path();
            if file.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
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
            let id = stored.task.id.clone();
            let task = Arc::new(Task::new(stored.task, stored.items, &ws, true));
            map.insert(id, task);
        }

        let mut models = Vec::new();
        let mut default = None;
        for task in map.values() {
            let meta = task.meta.read().await;
            if models.is_empty() && !meta.models.is_empty() {
                models = meta.models.clone();
            }
            if default.is_none() {
                default = meta.model.clone();
            }
        }
        *ws.tasks.write().await = map;
        *ws.models.write().await = models;
        *ws.default_model.write().await = default;
        Ok(ws)
    }

    /// Every event leaving a workspace carries the repository it belongs to.
    pub fn emit(&self, value: Value) {
        if let Some(orch) = self.orch.upgrade() {
            let mut value = value;
            if let Some(obj) = value.as_object_mut() {
                obj.insert("repo".into(), json!(self.repo.path));
            }
            orch.emit(value);
        }
    }

    pub async fn create(
        self: &Arc<Self>,
        title: String,
        model: Option<String>,
    ) -> Result<Arc<Task>> {
        let id = short_id();
        let ts = now_ms();
        let known_models = self.models.read().await.clone();
        let known_default = self.default_model.read().await.clone();
        let task = Arc::new(Task::new(
            TaskMeta {
                id: id.clone(),
                title,
                // The worktree and branch are created from the first message,
                // once a title has been chosen.
                branch: String::new(),
                worktree: String::new(),
                base: String::new(),
                status: "idle".into(),
                model: model.or(known_default),
                models: known_models,
                usage: None,
                created_at: ts,
                updated_at: ts,
                base_ref: String::new(),
                session_id: None,
                verify: None,
            },
            Vec::new(),
            self,
            false,
        ));

        self.tasks.write().await.insert(id.clone(), task.clone());
        task.persist().await;
        self.emit(json!({ "type": "task", "task": *task.meta.read().await }));
        Ok(task)
    }

    pub async fn task_count(&self) -> usize {
        self.tasks.read().await.len()
    }
}
