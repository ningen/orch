// The task view: one agent's transcript, composer, and inspector panes.
// This is a lens opened from the inbox, not the home of the UI.

import { $, el, api, ago, shortTokens, toast, shortSha } from "./util.js";
import { state, activeRepo, setView } from "./store.js";
import { renderProse } from "./markdown.js";

export function statusLabel(status) {
  switch (status) {
    case "running":
      return "working";
    case "waiting":
      return "waiting for you";
    case "done":
      return "done";
    case "error":
      return "error";
    case "merged":
      return "merged";
    default:
      return "idle";
  }
}

function currentTask() {
  const repo = activeRepo();
  return repo && state.current ? repo.tasks.get(state.current) : null;
}

/* ---------------------------------------------------------------- header */

export function renderHeader() {
  const task = currentTask();
  if (!task) return;
  const topbar = $("topbar");

  topbar.hidden = false;
  $("task-title").textContent = task.title || "new task";
  $("task-title").title = task.title;
  $("task-branch").textContent = task.branch || "not started";
  $("task-branch").title = task.branch;

  const status = $("task-status");
  status.querySelector("i").className = `dot ${task.status}`;
  status.querySelector("span").textContent = statusLabel(task.status);

  const usage = task.usage;
  if (usage && (usage.used || usage.cost)) {
    const cost = typeof usage.cost === "number" ? `$${usage.cost.toFixed(4)}` : "";
    const size = usage.size ? `/${shortTokens(usage.size)}` : "";
    $("task-usage").textContent = `${shortTokens(usage.used)}${size} tok${cost ? ` · ${cost}` : ""}`;
  } else {
    $("task-usage").textContent = "";
  }

  const select = $("model-select");
  select.textContent = "";
  for (const option of task.models || []) {
    const opt = el("option");
    opt.value = option.value;
    opt.textContent = option.name || option.value;
    select.appendChild(opt);
  }
  if (!(task.models || []).length) {
    const pending = el("option");
    pending.textContent = "no model yet";
    select.appendChild(pending);
  } else if (task.model) {
    select.value = task.model;
  }
  select.disabled = !(task.models || []).length;
  select.title = task.model || "";

  renderChecks();
  renderChangesActions();

  const busy = task.status === "running" || task.status === "waiting";
  const started = Boolean(task.branch || task.sessionId);
  $("cancel").hidden = !busy;
  $("input").disabled = false;
  $("send").disabled = false;
  $("input").placeholder = busy
    ? "The agent is working — stop it first to send another message"
    : started
      ? "Describe the next step"
      : "Describe what this task should do — the worktree and agent start on send";
}

/* ---------------------------------------------------------------- stream */

function itemNode(item) {
  let node = state.nodes.get(item.id);
  if (node) return node;

  node = el("div", "item");
  node.dataset.id = item.id;
  state.nodes.set(item.id, node);
  $("stream").appendChild(node);
  return node;
}

function shortPath(path) {
  const task = currentTask();
  if (task && task.worktree && path.startsWith(task.worktree)) {
    return path.slice(task.worktree.length).replace(/^\//, "") || ".";
  }
  return path;
}

function renderToolBody(box, item) {
  box.textContent = "";
  const input = item.input;
  if (input && typeof input === "object" && Object.keys(input).length) {
    for (const [key, value] of Object.entries(input)) {
      const line = el("div", "kv");
      const label = el("b");
      label.textContent = `${key}: `;
      line.appendChild(label);
      const rendered =
        typeof value === "string"
          ? /^\//.test(value)
            ? shortPath(value)
            : value
          : JSON.stringify(value, null, 2);
      line.appendChild(
        document.createTextNode(
          rendered.length > 4000 ? `${rendered.slice(0, 4000)}\n…` : rendered,
        ),
      );
      box.appendChild(line);
    }
  }
  if (item.output) {
    const pre = el("pre");
    pre.textContent = item.output;
    box.appendChild(pre);
  }
}

function paint(item) {
  const node = itemNode(item);

  if (item.kind === "user") {
    node.className = "item user";
    node.textContent = item.text;
    return;
  }
  if (item.kind === "note") {
    node.className = `item note ${item.tone || "info"}`;
    node.textContent = item.text;
    return;
  }
  if (item.kind === "thought") {
    node.className = "item thought";
    const body = el("div", "thought-body");
    body.textContent = item.text;
    if (item.text.length > 320) body.classList.add("clamped");
    node.textContent = "";
    node.appendChild(body);
    if (!node.dataset.wired) {
      node.dataset.wired = "1";
      node.addEventListener("click", () => {
        const target = node.querySelector(".thought-body");
        if (target) target.classList.toggle("clamped");
      });
    }
    return;
  }
  if (item.kind === "agent") {
    node.className = "item agent";
    renderProse(node, item.text || "", !!item.streaming);
    return;
  }
  if (item.kind === "tool") {
    node.className = "item";
    if (node.firstChild) {
      const badge = node.querySelector(".tool-state");
      if (badge) {
        badge.textContent = item.status;
        badge.className = `tool-state ${item.status}`;
      }
      const paths = node.querySelector(".tool-paths");
      if (paths) paths.textContent = (item.locations || []).map(shortPath).join("  ");
      const title = node.querySelector(".tool-title");
      if (title) title.textContent = item.title || "tool";
      const body = node.querySelector(".tool-body");
      if (body) renderToolBody(body, item);
      return;
    }
    const card = el("div", "tool");
    const head = el("div", "tool-head");
    const caret = el("span", "tool-caret");
    caret.textContent = "›";
    const title = el("span", "tool-title");
    title.textContent = item.title || "tool";
    const kind = el("span", "tool-kind");
    kind.textContent = item.toolKind && item.toolKind !== item.title ? item.toolKind : "";
    const badge = el("span", `tool-state ${item.status}`);
    badge.textContent = item.status;
    const paths = el("span", "tool-paths");
    paths.textContent = (item.locations || []).map(shortPath).join("  ");
    paths.title = (item.locations || []).join("\n");
    head.append(caret, title, kind, badge, paths);

    const body = el("div", "tool-body");
    renderToolBody(body, item);

    card.append(head, body);
    if (item.status === "in_progress" || item.status === "failed" || item.status === "pending") {
      card.classList.add("open");
    } else {
      body.hidden = true;
    }
    head.addEventListener("click", () => {
      const open = card.classList.toggle("open");
      body.hidden = !open;
    });
    node.appendChild(card);
  }
}

/** A task that has not started yet has no transcript — point at the composer. */
function renderEmpty() {
  const task = currentTask();
  if (!task || state.items.length || task.branch || task.sessionId) return;
  const box = el("div", "stream-empty");
  const lead = el("p", "empty-lead");
  lead.textContent = "Describe what this task should do.";
  const body = el("p", "empty-body");
  body.textContent =
    "The first message creates the worktree and starts the agent, and names the task from it. Nothing touches your repository until you merge.";
  box.append(lead, body);
  $("stream").appendChild(box);
}

export function renderStream() {
  const stream = $("stream");
  stream.textContent = "";
  state.nodes.clear();
  const sorted = [...state.items].sort((a, b) => (a.at || 0) - (b.at || 0));
  for (const item of sorted) paint(item);
  renderEmpty();
  scrollToEnd(true);
}

export function scrollToEnd(force = false) {
  const stream = $("stream");
  if (force || state.pinned) {
    stream.scrollTop = stream.scrollHeight;
    state.pinned = true;
    $("jump").hidden = true;
  }
}

const pendingPaint = new Set();
let rafHandle = 0;
function schedulePaint(item) {
  pendingPaint.add(item);
  if (rafHandle) return;
  rafHandle = requestAnimationFrame(() => {
    rafHandle = 0;
    for (const entry of pendingPaint) paint(entry);
    pendingPaint.clear();
    scrollToEnd();
  });
}

export function schedulePaintItem(item) {
  schedulePaint(item);
}

export function upsertItem(item) {
  const empty = $("stream").querySelector(".stream-empty");
  if (empty) empty.remove();
  const existing = state.items.find((x) => x.id === item.id);
  if (existing) {
    Object.assign(existing, item);
    paint(existing);
  } else {
    state.items.push(item);
    paint(item);
  }
  scrollToEnd();
}

export function appendRun(event) {
  const out = $("run-out");
  if (event.stream === "exit") {
    const line = el("div", `exit ${event.code === 0 ? "ok" : "fail"}`);
    line.textContent = `\n[exit ${event.code}]`;
    out.appendChild(line);
  } else {
    const line = el("span", event.stream === "stderr" ? "err" : "");
    line.textContent = event.text;
    out.appendChild(line);
  }
  out.scrollTop = out.scrollHeight;
}

export function clearStream() {
  state.items = [];
  state.nodes.clear();
  $("stream").textContent = "";
  $("diff").textContent = "";
  $("diff-meta").textContent = "";
  $("run-out").textContent = "";
}

/* ------------------------------------------------------------- selection */

export async function loadTask(id) {
  const data = await api(`/api/tasks/${id}`);
  if (state.current !== id) return;
  const repo = activeRepo();
  if (repo) repo.tasks.set(id, data.task);
  state.behind[id] = data.behind || 0;
  state.items = data.items || [];
  renderStream();
  renderHeader();
  loadDiff();
}

export async function selectTask(id) {
  const repo = activeRepo();
  if (!repo || !repo.tasks.has(id)) return;
  state.current = id;
  setView("task");
  clearStream();
  renderHeader();
  try {
    await loadTask(id);
  } catch (error) {
    toast(error.message, true);
  }
}

export function backToInbox() {
  state.current = null;
  setView("inbox");
}

/* ---------------------------------------------------------------- checks */

export function renderChecks() {
  const task = currentTask();
  const checks = (state.settings && state.settings.checks) || [];
  const list = $("check-list");
  if (!list) return;
  list.textContent = "";

  if (!checks.length) {
    const empty = el("div", "check-empty");
    empty.textContent = "no checks configured";
    const add = el("button");
    add.type = "button";
    add.textContent = "set up";
    add.addEventListener("click", () => document.dispatchEvent(new CustomEvent("orch:open-settings")));
    empty.appendChild(add);
    list.appendChild(empty);
  } else {
    const results = (task && task.verify && task.verify.commands) || [];
    checks.forEach((command, index) => {
      const row = el("div", "check-item");
      const result = results[index];
      const mark = el("span", "mark");
      mark.textContent = result ? (result.code === 0 ? "ok" : "ng") : "·";
      if (result) row.classList.add(result.code === 0 ? "ok" : "ng");
      const text = el("span");
      text.textContent = command;
      row.append(mark, text);
      list.appendChild(row);
    });
  }

  const line = $("check-state");
  if (!line) return;
  line.className = "check-state mono";
  if (!task || !task.verify) {
    line.textContent = checks.length ? "not run" : "";
  } else {
    line.textContent = `${task.verify.status} · ${ago(task.verify.at)}`;
    line.classList.add(task.verify.status);
  }

  // The receipt: which commits the result describes, and what policy found.
  const receipt = $("receipt");
  const verify = task && task.verify;
  if (!receipt) return;
  if (!verify || (!verify.headSha && !verify.baseSha)) {
    receipt.hidden = true;
    receipt.textContent = "";
  } else {
    receipt.hidden = false;
    receipt.textContent = "";
    const row = el("div", "receipt-row mono");
    row.textContent = `receipt · head ${shortSha(verify.headSha)} · base ${shortSha(
      verify.baseSha,
    )} · ${ago(verify.at)}`;
    receipt.appendChild(row);
    for (const finding of verify.policy || []) {
      const item = el("div", `receipt-policy ${finding.kind}`);
      item.textContent =
        finding.kind === "path"
          ? `path matched \`${finding.pattern}\`: ${finding.path}`
          : `added \`${finding.pattern}\` in ${finding.path}`;
      receipt.appendChild(item);
    }
  }

  $("check-run").disabled = !checks.length || !task || task.status === "running";
}

export function renderChangesActions() {
  const task = currentTask();
  const behind = state.current ? (state.behind[state.current] || 0) : 0;
  $("rebase").hidden = !(behind > 0 && task);
  $("rebase").textContent = `rebase +${behind}`;
  $("reset").disabled = !task;
  $("copy-path").disabled = !task;
}

/* ------------------------------------------------------------------ diff */

function renderDiff(text) {
  const out = $("diff");
  out.textContent = "";
  if (!text || !text.trim()) {
    const empty = el("div", "empty-diff");
    empty.textContent = "no changes yet";
    out.appendChild(empty);
    return;
  }
  for (const line of text.split("\n")) {
    const span = el("span", "ln");
    if (line.startsWith("diff --git") || line.startsWith("--- ") || line.startsWith("+++ "))
      span.classList.add("file");
    else if (line.startsWith("@@")) span.classList.add("hunk");
    else if (line.startsWith("+")) span.classList.add("add");
    else if (line.startsWith("-")) span.classList.add("del");
    else if (
      line.startsWith("index ") ||
      line.startsWith("new file") ||
      line.startsWith("deleted file")
    )
      span.classList.add("meta");
    span.textContent = line || " ";
    out.appendChild(span);
  }
}

export async function loadDiff() {
  if (!state.current) return;
  const id = state.current;
  try {
    const data = await api(`/api/tasks/${id}/diff`);
    if (state.current !== id) return;
    $("diff-meta").textContent = data.stat || "";
    renderDiff(data.diff);
  } catch (error) {
    toast(error.message, true);
  }
}

/* ------------------------------------------------------------------ wire */

export function mergeCurrent(force = false) {
  if (!state.current) return Promise.resolve();
  return api(`/api/tasks/${state.current}/merge`, {
    method: "POST",
    body: JSON.stringify({ force }),
  });
}

function autoSize() {
  const input = $("input");
  input.style.height = "auto";
  input.style.height = `${Math.min(input.scrollHeight, 200)}px`;
}

async function send() {
  const input = $("input");
  const text = input.value.trim();
  if (!text || !state.current) return;
  const task = currentTask();
  if (task && (task.status === "running" || task.status === "waiting")) {
    toast("the agent is still working — stop it first", true);
    return;
  }
  input.value = "";
  autoSize();
  try {
    await api(`/api/tasks/${state.current}/prompt`, {
      method: "POST",
      body: JSON.stringify({ text }),
    });
  } catch (error) {
    input.value = text;
    autoSize();
    toast(error.message, true);
  }
}

async function copyPath() {
  const task = currentTask();
  if (!task) return;
  try {
    await navigator.clipboard.writeText(task.worktree);
    toast("worktree path copied");
  } catch {
    toast(task.worktree);
  }
}

export function wireTaskView() {
  $("back").addEventListener("click", backToInbox);

  $("composer").addEventListener("submit", (event) => {
    event.preventDefault();
    send();
  });

  $("input").addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      send();
    }
  });
  $("input").addEventListener("input", autoSize);

  $("model-select").addEventListener("change", async (event) => {
    const id = state.current;
    if (!id) return;
    try {
      const data = await api(`/api/tasks/${id}/model`, {
        method: "POST",
        body: JSON.stringify({ value: event.target.value }),
      });
      const repo = activeRepo();
      if (repo) repo.tasks.set(id, data.task);
      renderHeader();
    } catch (error) {
      toast(error.message, true);
    }
  });

  $("cancel").addEventListener("click", async () => {
    if (!state.current) return;
    try {
      await api(`/api/tasks/${state.current}/cancel`, { method: "POST" });
    } catch (error) {
      toast(error.message, true);
    }
  });

  $("commit").addEventListener("click", async () => {
    if (!state.current) return;
    try {
      const data = await mergeCurrent(false);
      toast(data.message || "merged");
      loadDiff();
    } catch (error) {
      // A policy flag is the one refusal a human may accept on purpose.
      if (/diff policy flagged/.test(error.message)) {
        if (window.confirm(`${error.message}\n\nMerge anyway, accepting the findings?`)) {
          try {
            const data = await mergeCurrent(true);
            toast(data.message || "merged with override");
            loadDiff();
          } catch (again) {
            toast(again.message, true);
          }
        }
        return;
      }
      toast(error.message, true);
    }
  });

  $("refresh-diff").addEventListener("click", loadDiff);
  $("copy-path").addEventListener("click", copyPath);

  $("check-edit").addEventListener("click", () =>
    document.dispatchEvent(new CustomEvent("orch:open-settings")),
  );

  $("check-run").addEventListener("click", async () => {
    if (!state.current) return;
    try {
      await api(`/api/tasks/${state.current}/verify`, { method: "POST" });
    } catch (error) {
      toast(error.message, true);
    }
  });

  $("rebase").addEventListener("click", async () => {
    if (!state.current) return;
    try {
      const data = await api(`/api/tasks/${state.current}/rebase`, { method: "POST" });
      toast(data.message || "rebased");
      await loadTask(state.current);
    } catch (error) {
      toast(error.message, true);
    }
  });

  $("reset").addEventListener("click", async () => {
    if (!state.current) return;
    if (!window.confirm("Discard every change in this worktree?")) return;
    try {
      const data = await api(`/api/tasks/${state.current}/reset`, { method: "POST" });
      toast(data.message || "reset");
      await loadTask(state.current);
    } catch (error) {
      toast(error.message, true);
    }
  });

  $("run-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!state.current) return;
    const command = $("run-cmd").value.trim();
    if (!command) return;
    $("run-out").textContent = "";
    try {
      await api(`/api/tasks/${state.current}/run`, {
        method: "POST",
        body: JSON.stringify({ command }),
      });
    } catch (error) {
      toast(error.message, true);
    }
  });

  for (const tab of document.querySelectorAll(".tab")) {
    tab.addEventListener("click", () => {
      for (const other of document.querySelectorAll(".tab")) other.classList.remove("active");
      tab.classList.add("active");
      $("pane-changes").hidden = tab.dataset.tab !== "changes";
      $("pane-run").hidden = tab.dataset.tab !== "verify";
      if (tab.dataset.tab === "changes") loadDiff();
    });
  }

  const stream = $("stream");
  stream.addEventListener("scroll", () => {
    const bottom = stream.scrollTop + stream.clientHeight >= stream.scrollHeight - 40;
    state.pinned = bottom;
    $("jump").hidden = bottom;
  });
  $("jump").addEventListener("click", () => scrollToEnd(true));
}
