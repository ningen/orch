//! HTTP surface: JSON API, SSE event stream, and the embedded UI.

use crate::task::Orch;
use anyhow::anyhow;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::broadcast;
use tokio::sync::mpsc;

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const STYLE: &str = include_str!("../ui/style.css");

pub fn router(orch: Arc<Orch>) -> Router {
    Router::new()
        .route("/", get(index))
        .route(
            "/app.js",
            get(|| async { asset(APP_JS, "text/javascript") }),
        )
        .route("/style.css", get(|| async { asset(STYLE, "text/css") }))
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
        .route("/api/repos", post(open_repo).delete(close_repo))
        .route("/api/repos/current", post(set_current))
        .route("/api/browse", get(browse))
        .route("/api/tasks", post(create))
        .route("/api/tasks/{id}", get(show).delete(remove))
        .route("/api/tasks/{id}/prompt", post(prompt))
        .route("/api/tasks/{id}/cancel", post(cancel))
        .route("/api/tasks/{id}/model", post(set_model))
        .route("/api/tasks/{id}/merge", post(merge))
        .route("/api/tasks/{id}/diff", get(diff))
        .route("/api/tasks/{id}/run", post(run))
        .with_state(orch)
}

struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(value: E) -> Self {
        ApiError(value.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": format!("{:#}", self.0) }));
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}

type Api<T> = Result<Json<T>, ApiError>;

fn asset(body: &'static str, mime: &'static str) -> Response {
    // The UI ships inside the binary; a cached page must never outlive it.
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

async fn index() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        INDEX,
    )
        .into_response()
}

async fn need_task(orch: &Arc<Orch>, id: &str) -> Result<Arc<crate::task::Task>, ApiError> {
    orch.find_task(id)
        .await
        .ok_or_else(|| ApiError(anyhow!("no such task: {id}")))
}

/* ------------------------------------------------------------------ state */

async fn state(State(orch): State<Arc<Orch>>) -> Api<Value> {
    Ok(Json(json!({
        "current": orch.current_path().await,
        "repos": orch.summary().await,
    })))
}

#[derive(Deserialize)]
struct RepoBody {
    path: String,
}

async fn open_repo(State(orch): State<Arc<Orch>>, Json(body): Json<RepoBody>) -> Api<Value> {
    let ws = orch.open_workspace(FsPath::new(body.path.trim())).await?;
    orch.set_current(&ws.repo.path).await;
    Ok(Json(json!({
        "repo": { "path": ws.repo.path, "name": ws.repo.name, "branch": ws.repo.branch },
        "repos": orch.summary().await,
        "current": orch.current_path().await,
    })))
}

async fn set_current(State(orch): State<Arc<Orch>>, Json(body): Json<RepoBody>) -> Api<Value> {
    let path = body.path.trim().to_string();
    if orch.workspace(&path).await.is_none() {
        return Err(ApiError(anyhow!("that repository is not open: {path}")));
    }
    orch.set_current(&path).await;
    Ok(Json(json!({ "current": orch.current_path().await })))
}

async fn close_repo(State(orch): State<Arc<Orch>>, Json(body): Json<RepoBody>) -> Api<Value> {
    orch.close_workspace(body.path.trim()).await;
    Ok(Json(json!({
        "repos": orch.summary().await,
        "current": orch.current_path().await,
    })))
}

#[derive(Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    path: Option<String>,
}

async fn browse(Query(query): Query<BrowseQuery>) -> Api<Value> {
    let start = query
        .path
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(home_dir);
    let path = std::fs::canonicalize(&start).unwrap_or(start);

    let mut dirs: Vec<Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&path) {
        for entry in entries.flatten().take(2000) {
            let child = entry.path();
            if !child.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "node_modules" || name == "target" {
                continue;
            }
            let is_repo = child.join(".git").exists();
            dirs.push(json!({
                "name": name,
                "path": child.to_string_lossy(),
                "repo": is_repo,
            }));
        }
    }
    dirs.sort_by(|a, b| {
        let ra = a["repo"].as_bool().unwrap_or(false);
        let rb = b["repo"].as_bool().unwrap_or(false);
        rb.cmp(&ra).then_with(|| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });

    Ok(Json(json!({
        "path": path.to_string_lossy(),
        "parent": path.parent().map(|p| p.to_string_lossy().to_string()),
        "dirs": dirs,
    })))
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/* ------------------------------------------------------------------ tasks */

async fn show(State(orch): State<Arc<Orch>>, Path(id): Path<String>) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    let (meta, items) = task.snapshot().await;
    Ok(Json(json!({ "task": meta, "items": items })))
}

#[derive(Deserialize)]
struct CreateBody {
    title: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

async fn create(State(orch): State<Arc<Orch>>, Json(body): Json<CreateBody>) -> Api<Value> {
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return Err(ApiError(anyhow!("title is required")));
    }
    let ws = orch
        .current()
        .await
        .ok_or_else(|| ApiError(anyhow!("no repository is selected")))?;
    let task = ws.create(title, body.model).await?;
    if let Some(prompt) = body
        .prompt
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
    {
        task.prompt(prompt).await?;
    }
    let (meta, items) = task.snapshot().await;
    Ok(Json(json!({ "task": meta, "items": items })))
}

#[derive(Deserialize)]
struct PromptBody {
    text: String,
}

async fn prompt(
    State(orch): State<Arc<Orch>>,
    Path(id): Path<String>,
    Json(body): Json<PromptBody>,
) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    let text = body.text.trim().to_string();
    if text.is_empty() {
        return Err(ApiError(anyhow!("prompt is empty")));
    }
    task.prompt(text).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn cancel(State(orch): State<Arc<Orch>>, Path(id): Path<String>) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    task.cancel().await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ModelBody {
    value: String,
}

async fn set_model(
    State(orch): State<Arc<Orch>>,
    Path(id): Path<String>,
    Json(body): Json<ModelBody>,
) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    task.set_model(body.value).await?;
    let (meta, _) = task.snapshot().await;
    Ok(Json(json!({ "task": meta })))
}

async fn merge(State(orch): State<Arc<Orch>>, Path(id): Path<String>) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    let result = task.commit().await?;
    Ok(Json(result))
}

async fn diff(State(orch): State<Arc<Orch>>, Path(id): Path<String>) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    let result = task.diff().await.unwrap_or_else(
        |e| json!({ "stat": "", "diff": format!("could not read the diff: {e:#}") }),
    );
    Ok(Json(result))
}

async fn remove(State(orch): State<Arc<Orch>>, Path(id): Path<String>) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    task.remove().await?;
    Ok(Json(json!({ "ok": true })))
}

/* -------------------------------------------------------------------- run */

#[derive(Deserialize)]
struct RunBody {
    command: String,
}

async fn run(
    State(orch): State<Arc<Orch>>,
    Path(id): Path<String>,
    Json(body): Json<RunBody>,
) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    let (meta, _) = task.snapshot().await;
    let command = body.command.trim().to_string();
    if command.is_empty() {
        return Err(ApiError(anyhow!("command is empty")));
    }
    let run_id = format!("r_{}", uuid::Uuid::new_v4().simple());
    let task_run = run_id.clone();

    tokio::spawn(async move {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(&command)
            .current_dir(&meta.worktree)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                task.emit(json!({
                    "type": "run", "taskId": meta.id, "runId": task_run,
                    "stream": "stderr", "text": format!("{e}\n")
                }));
                task.emit(json!({
                    "type": "run", "taskId": meta.id, "runId": task_run,
                    "stream": "exit", "text": "", "code": -1
                }));
                return;
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

        let emitter = task.clone();
        let task_id = meta.id.clone();
        let run_id = task_run.clone();
        let drain = tokio::spawn(async move {
            while let Some((stream, text)) = rx.recv().await {
                emitter.emit(json!({
                    "type": "run", "taskId": task_id, "runId": run_id,
                    "stream": stream, "text": text
                }));
            }
        });

        let code = child.wait().await.ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = drain.await;
        task.emit(json!({
            "type": "run", "taskId": meta.id, "runId": task_run,
            "stream": "exit", "text": "", "code": code
        }));
    });

    Ok(Json(json!({ "ok": true, "runId": run_id })))
}

async fn pump_pipe<R>(mut pipe: R, name: &'static str, tx: mpsc::Sender<(String, String)>)
where
    R: AsyncRead + Unpin,
{
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

/* ------------------------------------------------------------------ events */

async fn events(
    State(orch): State<Arc<Orch>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let mut rx = orch.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(payload) => yield Ok(Event::default().data(payload)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
