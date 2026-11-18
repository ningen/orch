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
};

export function activeRepo() {
  return state.currentRepo ? state.repos.get(state.currentRepo) : null;
}

export function repoTasks() {
  const repo = activeRepo();
  return repo ? [...repo.tasks.values()].sort((a, b) => a.createdAt - b.createdAt) : [];
}

/** Attention classification: what makes a task worth a human's next look. */
export function classify(task) {
  const behind = state.behind[task.id] || 0;
  if (task.status === "error") {
    return { kind: "error", rank: 0, reason: "agent error" };
  }
  const verify = task.verify;
  if (verify && verify.status === "failed") {
    return { kind: "failed", rank: 1, reason: "verify failed" };
  }
  if (verify && verify.status === "needs_review") {
    return { kind: "policy", rank: 2, reason: "policy — needs review" };
  }
  if (verify && verify.status === "passed" && behind > 0) {
    return { kind: "stale", rank: 3, reason: `base moved +${behind}` };
  }
  if (task.status === "done" && !verify) {
    const gated = state.settings && (state.settings.checks || []).length > 0;
    return {
      kind: "review",
      rank: gated ? 4 : 5,
      reason: gated ? "done — not verified" : "done — no gate configured",
    };
  }
  return null;
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
