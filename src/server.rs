//! HTTP surface: JSON API, SSE event stream, and the embedded UI.

use crate::settings::Settings;
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
use tokio::sync::broadcast;

const INDEX: &str = include_str!("../ui/index.html");
const STYLE: &str = include_str!("../ui/style.css");
const APP_JS: &str = include_str!("../ui/js/app.js");
const INBOX_JS: &str = include_str!("../ui/js/inbox.js");
const TASK_JS: &str = include_str!("../ui/js/task.js");
const MODALS_JS: &str = include_str!("../ui/js/modals.js");
const MARKDOWN_JS: &str = include_str!("../ui/js/markdown.js");
const STORE_JS: &str = include_str!("../ui/js/store.js");
const UTIL_JS: &str = include_str!("../ui/js/util.js");

pub fn router(orch: Arc<Orch>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/style.css", get(|| async { asset(STYLE, "text/css") }))
        .route("/js/app.js", get(|| async { js(APP_JS) }))
        .route("/js/inbox.js", get(|| async { js(INBOX_JS) }))
        .route("/js/task.js", get(|| async { js(TASK_JS) }))
        .route("/js/modals.js", get(|| async { js(MODALS_JS) }))
        .route("/js/markdown.js", get(|| async { js(MARKDOWN_JS) }))
        .route("/js/store.js", get(|| async { js(STORE_JS) }))
        .route("/js/util.js", get(|| async { js(UTIL_JS) }))
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
        .route("/api/repos", post(open_repo).delete(close_repo))
        .route("/api/repos/current", post(set_current))
        .route("/api/browse", get(browse))
        .route("/api/tasks", post(create))
        .route("/api/tasks/{id}", get(show).delete(remove))
        .route("/api/tasks/{id}/items/{item}", get(show_item))
        .route("/api/tasks/{id}/prompt", post(prompt))
        .route("/api/tasks/{id}/cancel", post(cancel))
        .route("/api/tasks/{id}/model", post(set_model))
        .route("/api/tasks/{id}/merge", post(merge))
        .route("/api/tasks/{id}/verify", post(verify))
        .route("/api/tasks/{id}/rebase", post(rebase))
        .route("/api/tasks/{id}/reset", post(reset))
        .route("/api/settings", get(get_settings).put(put_settings))
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

fn js(body: &'static str) -> Response {
    asset(body, "text/javascript; charset=utf-8")
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

/// A transcript item without the parts nobody asked to see. Tool inputs and
/// outputs can be large (file contents, command tails); the list ships a
/// one-line summary and the lens fetches the body only when it is opened.
fn light_item(item: &Value) -> Value {
    if item.get("kind").and_then(Value::as_str) != Some("tool") {
        return item.clone();
    }
    let mut out = item.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("input".into(), json!({}));
        obj.insert("output".into(), json!(""));
        obj.insert("bodyOmitted".into(), json!(true));
    }
    out
}

async fn show(State(orch): State<Arc<Orch>>, Path(id): Path<String>) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    let (meta, items) = task.snapshot().await;
    let behind = task.behind().await;
    let state = task.receipt_state_with(behind).await;
    let items = items.iter().map(light_item).collect::<Vec<_>>();
    let mut task_value = serde_json::to_value(&meta).unwrap_or_default();
    if let Some(obj) = task_value.as_object_mut() {
        // The task JSON carries the server's verdict, so the client consumes
        // the state machine instead of re-deriving it.
        obj.insert("receiptState".into(), json!(state.as_str()));
    }
    Ok(Json(
        json!({ "task": task_value, "items": items, "behind": behind }),
    ))
}

/// The full item — what a lens asks for when someone opens a tool body.
async fn show_item(
    State(orch): State<Arc<Orch>>,
    Path((id, item_id)): Path<(String, String)>,
) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    let (_, items) = task.snapshot().await;
    let item = items
        .into_iter()
        .find(|i| i.get("id").and_then(Value::as_str) == Some(item_id.as_str()))
        .ok_or_else(|| ApiError(anyhow!("no such item: {item_id}")))?;
    Ok(Json(item))
}

#[derive(Deserialize)]
struct CreateBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    repo: Option<String>,
}

async fn create(State(orch): State<Arc<Orch>>, Json(body): Json<CreateBody>) -> Api<Value> {
    let title = body
        .title
        .map(|title| title.trim().to_string())
        .unwrap_or_default();
    let ws = match body
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(repo) => orch.open_workspace(FsPath::new(repo)).await?,
        None => orch
            .current()
            .await
            .ok_or_else(|| ApiError(anyhow!("no repository is selected")))?,
    };
    orch.set_current(&ws.repo.path).await;
    let task = ws.create(title, body.model).await?;
    if let Some(prompt) = body
        .prompt
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
    {
        task.prompt(prompt).await?;
    }
    let (meta, items) = task.snapshot().await;
    Ok(Json(
        json!({ "task": meta, "items": items, "repo": ws.repo.path }),
    ))
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

#[derive(Deserialize)]
struct MergeBody {
    /// Accept a `needs_review` receipt (policy findings) explicitly.
    #[serde(default)]
    force: bool,
}

async fn merge(
    State(orch): State<Arc<Orch>>,
    Path(id): Path<String>,
    body: Option<Json<MergeBody>>,
) -> Api<Value> {
    let force = body.map(|Json(b)| b.force).unwrap_or(false);
    let task = need_task(&orch, &id).await?;
    Ok(Json(task.merge(force).await?))
}

async fn verify(State(orch): State<Arc<Orch>>, Path(id): Path<String>) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    task.verify().await?;
    Ok(Json(json!({ "ok": true })))
}

async fn rebase(State(orch): State<Arc<Orch>>, Path(id): Path<String>) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    Ok(Json(task.rebase().await?))
}

async fn reset(State(orch): State<Arc<Orch>>, Path(id): Path<String>) -> Api<Value> {
    let task = need_task(&orch, &id).await?;
    Ok(Json(task.reset().await?))
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
    let command = body.command.trim().to_string();
    if command.is_empty() {
        return Err(ApiError(anyhow!("command is empty")));
    }
    // Refuse anything the repository settings marked as out of bounds.
    task.settings().await.check_command(&command)?;
    task.stage_intent().await.ok();

    let run_id = format!("r_{}", uuid::Uuid::new_v4().simple());
    let spawned = task.clone();
    let rid = run_id.clone();
    tokio::spawn(async move {
        if let Err(error) = spawned.exec(&command, &rid).await {
            eprintln!("run failed: {error:#}");
        }
    });
    Ok(Json(json!({ "ok": true, "runId": run_id })))
}

/* ---------------------------------------------------------------- settings */

#[derive(Deserialize)]
struct SettingsQuery {
    #[serde(default)]
    repo: Option<String>,
}

async fn resolve_repo(orch: &Arc<Orch>, wanted: Option<String>) -> Result<String, ApiError> {
    match wanted {
        Some(path) => Ok(path),
        None => orch
            .current_path()
            .await
            .ok_or_else(|| ApiError(anyhow!("no repository is selected"))),
    }
}

async fn get_settings(
    State(orch): State<Arc<Orch>>,
    Query(query): Query<SettingsQuery>,
) -> Api<Value> {
    let path = resolve_repo(&orch, query.repo).await?;
    let ws = orch
        .workspace(&path)
        .await
        .ok_or_else(|| ApiError(anyhow!("that repository is not open: {path}")))?;
    let settings = ws.settings.read().await.clone();
    let suggested = Settings::suggested(FsPath::new(&ws.repo.path));
    let (policy_paths, policy_lines) = Settings::suggested_policy(FsPath::new(&ws.repo.path));
    Ok(Json(json!({
        "repo": path,
        "dir": ws.dir,
        "checks": settings.checks,
        "deny": settings.deny,
        "policyPaths": settings.policy_paths,
        "policyLines": settings.policy_lines,
        "autopilot": settings.autopilot,
        "suggested": suggested,
        "suggestedPolicyPaths": policy_paths,
        "suggestedPolicyLines": policy_lines,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsBody {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    checks: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    policy_paths: Vec<String>,
    #[serde(default)]
    policy_lines: Vec<String>,
    #[serde(default)]
    autopilot: Option<String>,
}

async fn put_settings(State(orch): State<Arc<Orch>>, Json(body): Json<SettingsBody>) -> Api<Value> {
    let path = resolve_repo(&orch, body.repo).await?;
    let ws = orch
        .workspace(&path)
        .await
        .ok_or_else(|| ApiError(anyhow!("that repository is not open: {path}")))?;

    // Comment lines make sense only in the checks textarea; a policy or
    // deny pattern may legitimately start with `#` (e.g. `#[ignore]`).
    let clean_checks = |list: Vec<String>| {
        list.into_iter()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect::<Vec<_>>()
    };
    let clean_patterns = |list: Vec<String>| {
        list.into_iter()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
    };
    let settings = Settings {
        checks: clean_checks(body.checks),
        deny: clean_patterns(body.deny),
        policy_paths: clean_patterns(body.policy_paths),
        policy_lines: clean_patterns(body.policy_lines),
        // An absent or unknown value keeps the mode at its default.
        autopilot: body
            .autopilot
            .map(|value| {
                crate::receipt::AutopilotMode::parse(&value)
                    .as_str()
                    .to_string()
            })
            .unwrap_or_else(default_settings_autopilot),
    };
    settings.save(&ws.dir)?;
    *ws.settings.write().await = settings.clone();
    Ok(Json(json!({
        "checks": settings.checks,
        "deny": settings.deny,
        "policyPaths": settings.policy_paths,
        "policyLines": settings.policy_lines,
        "autopilot": settings.autopilot,
    })))
}

fn default_settings_autopilot() -> String {
    crate::receipt::AutopilotMode::KeepFresh
        .as_str()
        .to_string()
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
