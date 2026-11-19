# orch API contract (v0)

The `orch` daemon serves a single-page UI at `/` and a JSON API at `/api/*` on
`127.0.0.1:<port>`. The UI is embedded in the binary, has no build step, and
uses no external network resources.

All request and response bodies are JSON. Errors return a non-2xx status with
`{"error": "message"}`.

## Types

```ts
type TaskStatus = "idle" | "running" | "waiting" | "done" | "error" | "merged";

type ModelOption = { value: string; name: string };

type Usage = { used: number; size: number; cost: number };

type Task = {
  id: string;          // "t_ab12cd"
  title: string;       // empty until the first message names the task
  branch: string;      // "orch/<slug>-<id>"; empty until the task starts
  worktree: string;    // absolute path; empty until the task starts
  status: TaskStatus;
  model: string | null;
  models: ModelOption[];
  usage: Usage | null;
  createdAt: number;   // unix millis
  updatedAt: number;
  baseRef: string;     // branch this task was cut from
  sessionId: string | null; // agent session, reused after a restart
  verify: Verify | null;
  receiptState?: "failed" | "needs_review" | "stale" | "passed" | "unverified" | "ungated";
  behind?: number;           // commits the base branch is ahead (list responses)
};

// A verify run is a receipt: it names the commits it describes.
type Verify = {
  status: "passed" | "failed" | "needs_review";
  at: number;
  fingerprint: string;         // worktree contents this result describes
  headSha: string;            // task branch at verify time
  baseSha: string;            // base branch it was evaluated against
  commands: { command: string; code: number }[];
  policy: { kind: "path" | "line"; pattern: string; path: string }[];
  // "needs_review" means the checks were green but the diff policy found
  // something a human should look at; merging then needs { force: true }.
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
      bodyOmitted?: boolean; // true in list responses: fetch the item to see input/output
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
| GET | `/api/tasks/{id}` | – | `{ task, items: Item[], behind }` — the task carries the server's `receiptState` verdict; tool items ship without `input`/`output` (`bodyOmitted: true`), fetch the body only when someone opens it |
| GET | `/api/tasks/{id}/items/{itemId}` | – | one full `Item`, including a tool's `input` and `output` |
| POST | `/api/tasks` | `{ title?, prompt?, model?, repo? }` | `{ task, items, repo }` |
| POST | `/api/tasks/{id}/prompt` | `{ text }` | `{ ok: true }` |
| POST | `/api/tasks/{id}/cancel` | – | `{ ok: true }` |
| POST | `/api/tasks/{id}/model` | `{ value }` | `{ task }` |
| POST | `/api/tasks/{id}/merge` | `{ force?: boolean }` | commit, then merge when the repository is clean; refuses a stale base or a `needs_review` receipt without `force` |
| POST | `/api/tasks/{id}/verify` | – | run the repository's checks and diff policy (result on SSE) |
| POST | `/api/tasks/{id}/rebase` | – | replay onto the base branch, aborting on conflict |
| POST | `/api/tasks/{id}/reset` | – | discard the worktree and return to the base commit |
| DELETE | `/api/tasks/{id}` | – | `{ ok: true }` |
| GET | `/api/tasks/{id}/diff` | – | `{ stat, diff }` (unified diff vs. merge base) |
| POST | `/api/tasks/{id}/run` | `{ command }` | `{ ok: true }` (output arrives on SSE) |
| GET | `/api/settings?repo=` | – | `{ repo, dir, checks, deny, policyPaths, policyLines, autopilot, suggested, suggestedPolicyPaths, suggestedPolicyLines }` |
| PUT | `/api/settings` | `{ repo?, checks, deny, policyPaths, policyLines, autopilot? }` | same shape as `GET` |
| GET | `/api/events` | – | `text/event-stream` |

`RepoSummary` is what `/api/state` returns per open repository:

```ts
type RepoSummary = {
  path: string;      // absolute path, also the workspace key
  name: string;      // last path component
  branch: string;    // current HEAD branch
  tasks: (Task & { behind: number })[];  // commits the base has that the task does not
};                                        // each task also carries receiptState
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

- `POST /api/tasks` creates only a record. The git worktree is created when the
  first message arrives, and its branch is named after the task title. When no
  title is supplied, a small model reads the first message and proposes one
  (`--title-model` overrides the choice). One ACP agent session is then spawned
  for the task. `repo` names the target repository (it is opened if needed and
  becomes current); without it the current repository is used.
- `status` transitions: `idle` → `running` while the agent works, `waiting`
  when the agent asks for permission, `error` on a failed turn, `done` when the
  turn ends normally, `merged` after a successful merge.
- `GET /api/tasks/{id}/diff` compares the worktree against the merge base with
  the repo's default branch, so it shows everything the task changed.
- `GET /api/tasks/{id}` also returns `behind`: how many commits the base branch
  has that this task does not. `/api/state` carries the same number per task.
- When a turn ends normally, the autopilot runs: rebase onto a moved base,
  verify, and (mode permitting) merge — each decided from the receipt state
  and the gate, never from what the agent says. Every decision is recorded as
  a `note` item. `autopilot` is `"off" | "keep-fresh" (default) | "auto-merge"`;
  `auto-merge` never merges without a configured gate, with findings, or on a
  failed check — those stay human decisions.
- `receiptState` is the server's one verdict per task
  (`src/receipt.rs`), served in list and task payloads. Clients should consume
  it instead of re-deriving the state machine; a local derivation is only a
  fallback for older payloads.
- Verification settings live in the daemon's state directory
  (`<project>/settings.json`), never in the repository: an agent must not be
  able to edit the gate that judges it. `suggested` is a guess derived from
  marker files (`flake.nix`, `Cargo.toml`, ...).
- Checks are run in order inside the worktree. A configured check that has not
  passed against the current worktree blocks `merge`; changing a file after a
  passing run invalidates it.
- The diff policy (`policyPaths` globs, `policyLines` substrings in added
  lines) never fails the checks by itself. Findings are attached to the
  receipt as `needs_review`; merging then requires `{ force: true }`, which is
  recorded in the transcript.
- `deny` patterns are refused for commands the daemon executes (verify, run).
  They do not constrain an agent that runs tools on its own.
- `POST /api/tasks/{id}/run` runs a shell command in the task worktree and
  streams `stdout`/`stderr` on SSE, finishing with a `stream: "exit"` frame.
- Tool bodies are invisible unless sought: `GET /api/tasks/{id}` strips a
  tool item's `input` and `output` and marks it `bodyOmitted`; the lens
  fetches `GET /api/tasks/{id}/items/{itemId}` when the card is opened.
