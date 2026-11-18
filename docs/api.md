# orch API contract (v0)

The `orch` daemon serves a single-page UI at `/` and a JSON API at `/api/*` on
`127.0.0.1:<port>`. The UI is embedded in the binary, has no build step, and
uses no external network resources.

All request and response bodies are JSON. Errors return a non-2xx status with
`{"error": "message"}`.

## Types

```ts
type TaskStatus = "idle" | "running" | "waiting" | "done" | "error";

type ModelOption = { value: string; name: string };

type Usage = { used: number; size: number; cost: number };

type Task = {
  id: string;          // "t_ab12cd"
  title: string;
  branch: string;      // "orch/<slug>"
  worktree: string;    // absolute path
  status: TaskStatus;
  model: string | null;
  models: ModelOption[];
  usage: Usage | null;
  createdAt: number;   // unix millis
  updatedAt: number;
};

// Ordered transcript entries. Items are upserted by `id`.
type Item =
  | { kind: "user";  id: string; at: number; text: string }
  | { kind: "agent"; id: string; at: number; text: string; streaming: boolean }
  | { kind: "thought"; id: string; at: number; text: string }
  | { kind: "note";  id: string; at: number; text: string; tone: "info" | "warn" | "error" }
  | {
      kind: "tool";
      id: string;            // same as toolCallId
      at: number;
      title: string;         // "write", "ls", "bash ..."
      toolKind: string;      // "edit" | "execute" | "read" | "search" | "think" | ...
      status: "pending" | "in_progress" | "completed" | "failed";
      locations: string[];   // file paths touched
      input: unknown;        // rawInput, may be {}
      output: string;        // rendered text, may be ""
    };
```

## Endpoints

| Method | Path | Body | Response |
| --- | --- | --- | --- |
| GET | `/api/state` | – | `{ current, repos: RepoSummary[] }` |
| POST | `/api/repos` | `{ path }` | open a repository and switch to it |
| POST | `/api/repos/current` | `{ path }` | switch to an already-open repository |
| DELETE | `/api/repos` | `{ path }` | close a repository |
| GET | `/api/browse?path=` | – | `{ path, parent, dirs: [{ name, path, repo }] }` |
| GET | `/api/tasks/{id}` | – | `{ task: Task, items: Item[] }` |
| POST | `/api/tasks` | `{ title, prompt?, model? }` | `{ task, items }` |
| POST | `/api/tasks/{id}/prompt` | `{ text }` | `{ ok: true }` |
| POST | `/api/tasks/{id}/cancel` | – | `{ ok: true }` |
| POST | `/api/tasks/{id}/model` | `{ value }` | `{ task }` |
| POST | `/api/tasks/{id}/merge` | – | `{ ok, message }` |
| DELETE | `/api/tasks/{id}` | – | `{ ok: true }` |
| GET | `/api/tasks/{id}/diff` | – | `{ stat, diff }` (unified diff vs. merge base) |
| POST | `/api/tasks/{id}/run` | `{ command }` | `{ ok: true }` (output arrives on SSE) |
| GET | `/api/events` | – | `text/event-stream` |

`RepoSummary` is what `/api/state` returns per open repository:

```ts
type RepoSummary = {
  path: string;      // absolute path, also the workspace key
  name: string;      // last path component
  branch: string;    // current HEAD branch
  tasks: Task[];
};
```

Several repositories can be open at once. Their agent sessions keep running
while you switch, and every task route resolves the task across all of them, so
`/api/tasks/{id}` works regardless of which repository is current.

## SSE stream

`GET /api/events` emits `data: <json>\n\n` frames. Every frame has a `type`.

Every frame carries `repo` (the absolute repository path) in addition to the
fields below, so a client can route events without extra lookups.

```jsonc
{ "type": "task", "repo": "…", "task": Task }                     // upsert task
{ "type": "removed", "repo": "…", "taskId": "…" }                 // task deleted
{ "type": "item", "repo": "…", "taskId": "...", "item": Item }    // upsert item (create or replace)
{ "type": "delta", "repo": "…", "taskId": "...", "itemId": "...", "text": "…" } // append to an item
{ "type": "run", "repo": "…", "taskId": "...", "runId": "...", "stream": "stdout" | "stderr" | "exit", "text": "…", "code": 0 }
{ "type": "log", "text": "…" }                                    // daemon-level notice
```

Clients should reconnect automatically (EventSource does this) and re-fetch
`/api/state` plus the open task on reconnect.

## Behaviour notes

- `POST /api/tasks` creates a git worktree at
  `<repo>/.orch/worktrees/<id>` on branch `orch/<slug>` and spawns one ACP agent
  child process for that task. The optional `prompt` is sent immediately.
- `status` transitions: `idle` → `running` while the agent works, `waiting`
  when the agent asks for permission, `error` on a failed turn, `done` when the
  turn ends normally.
- `GET /api/tasks/{id}/diff` compares the worktree against the merge base with
  the repo's default branch, so it shows everything the task changed.
- `POST /api/tasks/{id}/run` runs a shell command in the task worktree and
  streams `stdout`/`stderr` on SSE, finishing with a `stream: "exit"` frame.
