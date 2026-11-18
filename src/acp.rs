//! Minimal Agent Client Protocol (ACP) client.
//!
//! An ACP agent is a child process that speaks JSON-RPC 2.0 over stdin/stdout,
//! one message per line. The client drives sessions and answers the agent's
//! requests (file access, permissions, terminals).

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

/// Something the agent sent that the session layer needs to react to.
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// A request from the agent. The receiver must send a JSON result back.
    Request {
        method: String,
        params: Value,
        reply: oneshot::Sender<Value>,
    },
    Stderr(String),
    Exited,
}

pub struct Acp {
    stdin: Mutex<tokio::process::ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    next_id: AtomicU64,
}

impl Acp {
    pub fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        tx: mpsc::Sender<Incoming>,
    ) -> Result<Arc<Self>> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{program} {}`", args.join(" ")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("agent has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("agent has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("agent has no stderr"))?;

        let acp = Arc::new(Acp {
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });

        tokio::spawn(read_loop(acp.clone(), stdout, stderr, tx.clone()));
        tokio::spawn(async move {
            let _ = child.wait().await;
            let _ = tx.send(Incoming::Exited).await;
        });

        Ok(acp)
    }

    /// Send a request and wait for the agent's response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (otx, orx) = oneshot::channel();
        self.pending.lock().await.insert(id, otx);
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        match orx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!(e)),
            Err(_) => Err(anyhow!("agent exited before answering {method}")),
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn send(&self, msg: &Value) -> Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn reply(&self, id: Value, result: Value) {
        let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = self.send(&msg).await;
    }

    async fn reply_error(&self, id: Value, code: i64, message: &str) {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        });
        let _ = self.send(&msg).await;
    }
}

async fn read_loop(
    acp: Arc<Acp>,
    stdout: ChildStdout,
    stderr: ChildStderr,
    tx: mpsc::Sender<Incoming>,
) {
    let err_tx = tx.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if err_tx.send(Incoming::Stderr(line)).await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                let _ = tx.send(Incoming::Stderr(line.to_string())).await;
                continue;
            }
        };
        dispatch(&acp, msg, &tx).await;
    }
}

async fn dispatch(acp: &Arc<Acp>, msg: Value, tx: &mpsc::Sender<Incoming>) {
    let id = msg.get("id").cloned().filter(|v| !v.is_null());
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

    match (id, method) {
        // A request from the agent.
        (Some(id), Some(method)) => match method.as_str() {
            "fs/read_text_file" => {
                let result = read_text_file(&params).await;
                match result {
                    Ok(v) => acp.reply(id, v).await,
                    Err(e) => acp.reply_error(id, -32000, &e.to_string()).await,
                }
            }
            "fs/write_text_file" => match write_text_file(&params).await {
                Ok(()) => acp.reply(id, json!(null)).await,
                Err(e) => acp.reply_error(id, -32000, &e.to_string()).await,
            },
            // Any other method is the session layer's business.
            _ => {
                let (rtx, rrx) = oneshot::channel();
                if tx
                    .send(Incoming::Request {
                        method: method.clone(),
                        params,
                        reply: rtx,
                    })
                    .await
                    .is_err()
                {
                    acp.reply_error(id, -32603, "session closed").await;
                    return;
                }
                let result = rrx.await.unwrap_or(Value::Null);
                acp.reply(id, result).await;
            }
        },
        // A response to one of our requests.
        (Some(id), None) => {
            let Some(key) = id.as_u64() else { return };
            let Some(slot) = acp.pending.lock().await.remove(&key) else {
                return;
            };
            if let Some(err) = msg.get("error") {
                let text = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("agent error")
                    .to_string();
                let _ = slot.send(Err(text));
            } else {
                let result = msg.get("result").cloned().unwrap_or(Value::Null);
                let _ = slot.send(Ok(result));
            }
        }
        // A notification.
        (None, Some(method)) => {
            let _ = tx.send(Incoming::Notification { method, params }).await;
        }
        (None, None) => {}
    }
}

async fn read_text_file(params: &Value) -> Result<Value> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing path"))?;
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("read {path}"))?;
    let content = match (
        params.get("line").and_then(Value::as_u64),
        params.get("limit").and_then(Value::as_u64),
    ) {
        (Some(line), limit) => {
            let skip = line.saturating_sub(1) as usize;
            let take = limit.unwrap_or(u64::MAX) as usize;
            content
                .lines()
                .skip(skip)
                .take(take)
                .collect::<Vec<_>>()
                .join("\n")
        }
        _ => content,
    };
    Ok(json!({ "content": content }))
}

async fn write_text_file(params: &Value) -> Result<()> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing path"))?;
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing content"))?;
    if let Some(parent) = Path::new(path).parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::write(path, content)
        .await
        .with_context(|| format!("write {path}"))
}

/// Client capabilities we advertise during `initialize`.
pub fn capabilities() -> Value {
    json!({
        "fs": { "readTextFile": true, "writeTextFile": true },
        "terminal": false
    })
}
