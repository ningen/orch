// Shared client state. Every module reads and writes this one object.

import { $ } from "./util.js";

export const state = {
  repos: new Map(), // path -> { path, name, branch, tasks: Map<id, task> }
  currentRepo: null,
  current: null, // task id when the task view is open
  view: "inbox", // "inbox" | "task"
  items: [],
  nodes: new Map(),
  pinned: true,
  connected: false,
  everConnected: false,
  browse: null,
  settings: null,
  behind: {}, // task id -> commits the base is ahead (from /api/state)
  unseen: 0,
  inboxCursor: 0, // index into the inbox order (attention then quiet)
  cardData: new Map(), // task id -> { items, behind } loaded for an expanded card
  composerOpen: false, // the composer shows only when asked for (or before the first message)
};

export function activeRepo() {
  return state.currentRepo ? state.repos.get(state.currentRepo) : null;
}

export function repoTasks() {
  const repo = activeRepo();
  return repo ? [...repo.tasks.values()].sort((a, b) => a.createdAt - b.createdAt) : [];
}

/**
 * What the last receipt says about this task's work. The server ships its
 * verdict as `task.receiptState` (see src/receipt.rs — one state machine,
 * consumed everywhere); this fallback re-derives it only when a payload
 * predates that field.
 */
export function receiptState(task, behind, gated) {
  if (task.receiptState) return task.receiptState;
  const verify = task.verify;
  if (!verify) return gated ? "unverified" : "ungated";
  if (verify.status === "failed") return "failed";
  if (verify.status === "needs_review") return "needs_review";
  return behind > 0 ? "stale" : "passed";
}

/** Attention classification: what makes a task worth a human's next look. */
export function classify(task) {
  const behind = state.behind[task.id] || 0;
  if (task.status === "error") {
    return { kind: "error", rank: 0, reason: "agent error" };
  }
  const gated = Boolean(state.settings && (state.settings.checks || []).length);
  switch (receiptState(task, behind, gated)) {
    case "failed":
      return { kind: "failed", rank: 1, reason: "verify failed" };
    case "needs_review":
      return { kind: "policy", rank: 2, reason: "policy — needs review" };
    case "stale":
      return { kind: "stale", rank: 3, reason: `base moved +${behind}` };
    case "unverified":
      if (task.status === "done") {
        return { kind: "review", rank: 4, reason: "done — not verified" };
      }
      return null;
    case "ungated":
      if (task.status === "done") {
        return { kind: "review", rank: 5, reason: "done — no gate configured" };
      }
      return null;
    default:
      return null; // "passed": calm, nothing needed
  }
}

/** The inbox order: attention first (rank, then newest), calm below. */
export function inboxOrder() {
  const attention = [];
  const quiet = [];
  for (const task of repoTasks()) {
    const info = classify(task);
    if (info) attention.push({ task, info });
    else quiet.push({ task, info: null });
  }
  attention.sort(
    (a, b) =>
      a.info.rank - b.info.rank ||
      (b.task.updatedAt || 0) - (a.task.updatedAt || 0),
  );
  quiet.sort((a, b) => {
    const merged = (t) => (t.status === "merged" ? 1 : 0);
    if (merged(a.task) !== merged(b.task)) return merged(a.task) - merged(b.task);
    return (a.task.createdAt || 0) - (b.task.createdAt || 0);
  });
  return { attention, quiet };
}

export function setView(view) {
  state.view = view;
  $("app").dataset.view = view;
  // `[hidden]` beats the layout rules, so the panes must be switched by
  // attribute — data-view alone would leave the task view unrendered.
  $("inbox-view").hidden = view !== "inbox";
  $("task-view").hidden = view !== "task";
  if (view === "inbox") {
    state.inboxCursor = clampCursor();
    document.body.classList.remove("task-open");
  } else {
    document.body.classList.add("task-open");
  }
}

function clampCursor() {
  const { attention, quiet } = inboxOrder();
  const total = attention.length + quiet.length;
  return Math.min(state.inboxCursor, Math.max(0, total - 1));
}

export function cursorList() {
  const { attention, quiet } = inboxOrder();
  return [...attention, ...quiet];
}

/**
 * The recommended next action for a task. Derived from the receipt, the
 * status, and git state only — never from what the agent says about its
 * own work. `null` means nothing needs a human decision right now.
 */
export function nextAction(task, behind, gated) {
  if (!task) return null;
  if (task.status === "running" || task.status === "waiting" || task.status === "merged") {
    return null;
  }
  const alt = (id, label) => ({ id, label });
  const started = Boolean(task.branch || task.sessionId);

  if (task.status === "error") {
    return {
      id: "instruct",
      label: "send back",
      why: "the agent errored — steer it from the transcript",
      alternatives: [alt("open", "read the transcript")],
    };
  }
  switch (receiptState(task, behind, gated)) {
    case "failed":
      return {
        id: "instruct",
        label: "send back",
        why: "a check failed — the receipt has the output",
        alternatives: [alt("open", "see the receipt"), alt("verify", "run verify again")],
      };
    case "needs_review": {
      const n = (task.verify && task.verify.policy || []).length;
      return {
        id: "merge",
        label: "accept & merge",
        confirm: true,
        why: `checks green · policy flagged ${n} finding${n === 1 ? "" : "s"}`,
        alternatives: [alt("open", "see the findings"), alt("instruct", "send back")],
      };
    }
    case "stale":
      return {
        id: "rebase",
        label: `rebase +${behind}`,
        why: `the base moved ${behind} commit${behind === 1 ? "" : "s"} — the receipt is stale`,
        alternatives: [alt("open", "open the task")],
      };
    case "passed":
      return {
        id: "merge",
        label: "merge",
        confirm: true,
        why: "checks passed against the current base",
        alternatives: [alt("open", "see the diff"), alt("instruct", "send back")],
      };
    default:
      break;
  }
  if (!started) {
    return {
      id: "instruct",
      label: "describe the task",
      why: "nothing has started — the first message creates the worktree",
      alternatives: [],
    };
  }
  if (receiptState(task, behind, gated) === "unverified") {
    return {
      id: "verify",
      label: "verify",
      why: "no receipt for this worktree yet",
      alternatives: [alt("open", "open the task")],
    };
  }
  return {
    id: "merge",
    label: "merge",
    confirm: true,
    why: "no gate is configured for this repository",
    alternatives: [alt("open", "see the diff")],
  };
}
