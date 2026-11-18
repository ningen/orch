"use strict";

/* orch — single page client for the orch daemon. */

const $ = (id) => document.getElementById(id);
const el = (tag, cls) => {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  return node;
};

const state = {
  repo: null,
  tasks: new Map(),
  current: null,
  items: [],
  nodes: new Map(),
  pinned: true,
  models: [],
  connected: false,
  everConnected: false,
};

/* ------------------------------------------------------------------ api */

async function api(path, opts = {}) {
  const res = await fetch(path, {
    headers: opts.body ? { "content-type": "application/json" } : undefined,
    ...opts,
  });
  const text = await res.text();
  let data = null;
  try {
    data = text ? JSON.parse(text) : null;
  } catch {
    data = { error: text };
  }
  if (!res.ok) throw new Error((data && data.error) || `HTTP ${res.status}`);
  return data;
}

/* --------------------------------------------------------------- helpers */

function ago(ms) {
  const secs = Math.max(0, (Date.now() - ms) / 1000);
  if (secs < 45) return "now";
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  if (secs < 86400) return `${Math.round(secs / 3600)}h`;
  return `${Math.round(secs / 86400)}d`;
}

function shortTokens(n) {
  if (!n) return "0";
  if (n < 1000) return String(n);
  return `${(n / 1000).toFixed(1)}k`;
}

let toastTimer = null;
function toast(message, isError = false) {
  const node = $("toast");
  node.textContent = message;
  node.classList.toggle("error", isError);
  node.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    node.hidden = true;
  }, isError ? 9000 : 6000);
}

/* ------------------------------------------------------------------ rail */

function renderRail() {
  const list = $("task-list");
  list.textContent = "";
  const tasks = [...state.tasks.values()].sort((a, b) => a.createdAt - b.createdAt);

  for (const task of tasks) {
    const row = el("button", "task-row");
    row.type = "button";
    row.dataset.id = task.id;
    if (task.id === state.current) row.classList.add("active");

    const top = el("div", "row-top");
    const dot = el("span", `dot ${task.status}`);
    const title = el("span", "row-title");
    title.textContent = task.title;
    top.append(dot, title);

    const sub = el("div", "row-sub");
    const branch = el("span", "sub-branch");
    branch.textContent = task.branch;
    const time = el("span");
    time.textContent = ago(task.updatedAt || task.createdAt);
    sub.append(branch, time);

    row.append(top, sub);
    row.addEventListener("click", () => selectTask(task.id));
    list.appendChild(row);
  }

  $("task-count").textContent = tasks.length ? `${tasks.length}` : "";
}

let railPending = false;
function scheduleRail() {
  if (railPending) return;
  railPending = true;
  requestAnimationFrame(() => {
    railPending = false;
    renderRail();
  });
}

function touchTask(task) {
  state.tasks.set(task.id, task);
  if (task.models && task.models.length) state.models = task.models;
  scheduleRail();
  if (task.id === state.current) renderHeader();
}

/* ---------------------------------------------------------------- header */

function statusLabel(status) {
  switch (status) {
    case "running":
      return "working";
    case "waiting":
      return "waiting for you";
    case "done":
      return "done";
    case "error":
      return "error";
    default:
      return "idle";
  }
}

function renderHeader() {
  const task = state.tasks.get(state.current);
  const topbar = $("topbar");
  const empty = $("empty");
  if (!task) {
    topbar.hidden = true;
    empty.hidden = false;
    $("input").disabled = true;
    $("send").disabled = true;
    return;
  }
  empty.hidden = true;
  topbar.hidden = false;

  $("task-title").textContent = task.title;
  $("task-title").title = task.title;
  $("task-branch").textContent = task.branch;
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
  if (task.model) select.value = task.model;
  select.disabled = !(task.models || []).length;
  select.title = task.model || "";

  const busy = task.status === "running" || task.status === "waiting";
  $("cancel").hidden = !busy;
  $("input").disabled = false;
  $("send").disabled = false;
  $("input").placeholder = busy
    ? "The agent is working — stop it first to send another message"
    : "Describe the next step";
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

function renderProse(box, text, streaming) {
  box.textContent = "";
  const parts = text.split("```");
  for (let i = 0; i < parts.length; i += 1) {
    const chunk = parts[i];
    if (i % 2 === 1) {
      const nl = chunk.indexOf("\n");
      let body = chunk;
      if (nl >= 0) {
        const head = chunk.slice(0, nl);
        if (head.length < 24 && !head.includes(" ")) body = chunk.slice(nl + 1);
      }
      const pre = el("pre");
      const code = el("code");
      code.textContent = body.replace(/\n+$/, "");
      pre.appendChild(code);
      box.appendChild(pre);
    } else {
      const clean = chunk.replace(/^\n+/, "").replace(/\n+$/, "");
      if (!clean) continue;
      const p = el("p");
      p.textContent = clean;
      box.appendChild(p);
    }
  }
  if (streaming) box.appendChild(el("span", "caret"));
}

function shortPath(path) {
  const task = state.tasks.get(state.current);
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
          ? (/^\//.test(value) ? shortPath(value) : value)
          : JSON.stringify(value, null, 2);
      line.appendChild(document.createTextNode(rendered.length > 4000 ? `${rendered.slice(0, 4000)}\n…` : rendered));
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
    node.textContent = item.text;
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
      // already built: only refresh the parts that change
      const state = node.querySelector(".tool-state");
      if (state) {
        state.textContent = item.status;
        state.className = `tool-state ${item.status}`;
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
    kind.textContent = item.toolKind || "";
    const status = el("span", `tool-state ${item.status}`);
    status.textContent = item.status;
    const paths = el("span", "tool-paths");
    paths.textContent = (item.locations || []).map(shortPath).join("  ");
    paths.title = (item.locations || []).join("\n");
    head.append(caret, title, kind, status, paths);

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

function renderStream() {
  const stream = $("stream");
  stream.textContent = "";
  state.nodes.clear();
  const sorted = [...state.items].sort((a, b) => (a.at || 0) - (b.at || 0));
  for (const item of sorted) paint(item);
  scrollToEnd(true);
}

function scrollToEnd(force = false) {
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

function upsertItem(item) {
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

/* ---------------------------------------------------------------- events */

function handle(event) {
  switch (event.type) {
    case "task": {
      if (!event.task) {
        if (event.removed) {
          state.tasks.delete(event.removed);
          if (state.current === event.removed) {
            state.current = null;
            renderStream();
            renderHeader();
          }
          renderRail();
        }
        return;
      }
      touchTask(event.task);
      return;
    }
    case "item": {
      if (event.taskId !== state.current) return;
      upsertItem(event.item);
      return;
    }
    case "delta": {
      if (event.taskId !== state.current) return;
      const item = state.items.find((x) => x.id === event.itemId);
      if (!item) return;
      item.text = (item.text || "") + event.text;
      schedulePaint(item);
      return;
    }
    case "run": {
      if (event.taskId !== state.current) return;
      appendRun(event);
      return;
    }
    case "log": {
      toast(event.text);
      return;
    }
    default:
  }
}

/* -------------------------------------------------------------- selection */

async function selectTask(id) {
  if (!state.tasks.has(id)) return;
  state.current = id;
  state.items = [];
  state.nodes.clear();
  $("stream").textContent = "";
  renderRail();
  renderHeader();
  $("diff").textContent = "";
  $("diff-meta").textContent = "";
  $("run-out").textContent = "";

  try {
    const data = await api(`/api/tasks/${id}`);
    if (state.current !== id) return;
    state.tasks.set(id, data.task);
    state.items = data.items || [];
    renderStream();
    renderHeader();
    loadDiff();
  } catch (error) {
    toast(error.message, true);
  }
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
    if (line.startsWith("diff --git") || line.startsWith("--- ") || line.startsWith("+++ ")) span.classList.add("file");
    else if (line.startsWith("@@")) span.classList.add("hunk");
    else if (line.startsWith("+")) span.classList.add("add");
    else if (line.startsWith("-")) span.classList.add("del");
    else if (line.startsWith("index ") || line.startsWith("new file") || line.startsWith("deleted file")) span.classList.add("meta");
    span.textContent = line || " ";
    out.appendChild(span);
  }
}

async function loadDiff() {
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

/* ------------------------------------------------------------------- run */

function appendRun(event) {
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

/* ------------------------------------------------------------------ wire */

function autoSize() {
  const input = $("input");
  input.style.height = "auto";
  input.style.height = `${Math.min(input.scrollHeight, 200)}px`;
}

async function send() {
  const input = $("input");
  const text = input.value.trim();
  if (!text || !state.current) return;
  const id = state.current;
  const task = state.tasks.get(id);
  if (task && (task.status === "running" || task.status === "waiting")) {
    toast("the agent is still working — stop it first", true);
    return;
  }
  input.value = "";
  autoSize();
  try {
    await api(`/api/tasks/${id}/prompt`, {
      method: "POST",
      body: JSON.stringify({ text }),
    });
  } catch (error) {
    input.value = text;
    autoSize();
    toast(error.message, true);
  }
}

function openModal() {
  $("modal").hidden = false;
  $("new-title").value = "";
  $("new-prompt").value = "";
  const select = $("new-model");
  select.textContent = "";
  const empty = el("option");
  empty.value = "";
  empty.textContent = "agent default";
  select.appendChild(empty);
  for (const option of state.models) {
    const opt = el("option");
    opt.value = option.value;
    opt.textContent = option.name || option.value;
    select.appendChild(opt);
  }
  setTimeout(() => $("new-title").focus(), 20);
}

function closeModal() {
  $("modal").hidden = true;
}

async function createTask(event) {
  event.preventDefault();
  const title = $("new-title").value.trim();
  if (!title) return;
  const prompt = $("new-prompt").value.trim();
  const model = $("new-model").value || null;
  const body = { title };
  if (prompt) body.prompt = prompt;
  if (model) body.model = model;
  try {
    const data = await api("/api/tasks", { method: "POST", body: JSON.stringify(body) });
    closeModal();
    state.tasks.set(data.task.id, data.task);
    await selectTask(data.task.id);
    renderRail();
  } catch (error) {
    toast(error.message, true);
  }
}

function connect() {
  const source = new EventSource("/api/events");

  source.onopen = () => {
    const back = state.everConnected;
    state.connected = true;
    state.everConnected = true;
    $("conn").className = "conn live";
    $("conn-label").textContent = "live";
    if (back) refresh();
  };

  source.onerror = () => {
    state.connected = false;
    $("conn").className = "conn dead";
    $("conn-label").textContent = "reconnecting";
  };

  source.onmessage = (event) => {
    try {
      handle(JSON.parse(event.data));
    } catch {
      /* ignore malformed frames */
    }
  };
}

async function refresh() {
  try {
    const data = await api("/api/state");
    state.repo = data.repo;
    state.tasks = new Map((data.tasks || []).map((task) => [task.id, task]));
    for (const task of state.tasks.values()) {
      if (task.models && task.models.length) {
        state.models = task.models;
        break;
      }
    }
    $("repo-name").textContent = data.repo.name;
    $("repo-branch").textContent = data.repo.branch;
    renderRail();
    if (state.current && state.tasks.has(state.current)) {
      const fresh = await api(`/api/tasks/${state.current}`);
      state.items = fresh.items || [];
      state.tasks.set(state.current, fresh.task);
      renderStream();
      renderHeader();
    } else if (!state.current) {
      const tasks = [...state.tasks.values()].sort((a, b) => a.createdAt - b.createdAt);
      if (tasks.length) await selectTask(tasks[tasks.length - 1].id);
      else renderHeader();
    }
  } catch (error) {
    toast(error.message, true);
  }
}

/* ------------------------------------------------------------------- init */

function wire() {
  $("new-task").addEventListener("click", openModal);
  $("empty-new").addEventListener("click", openModal);
  $("new-cancel").addEventListener("click", closeModal);
  $("new-form").addEventListener("submit", createTask);
  $("modal").addEventListener("click", (event) => {
    if (event.target === $("modal")) closeModal();
  });

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
      state.tasks.set(id, data.task);
      renderHeader();
      renderRail();
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
      const data = await api(`/api/tasks/${state.current}/merge`, { method: "POST" });
      toast(data.message || "committed");
      loadDiff();
    } catch (error) {
      toast(error.message, true);
    }
  });

  $("refresh-diff").addEventListener("click", loadDiff);

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
      $("pane-run").hidden = tab.dataset.tab !== "run";
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

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && !$("modal").hidden) {
      closeModal();
      return;
    }
    const typing =
      event.target instanceof HTMLInputElement ||
      event.target instanceof HTMLTextAreaElement ||
      event.target instanceof HTMLSelectElement;
    if (typing || event.metaKey || event.ctrlKey || event.altKey) return;

    if (event.key === "n") {
      event.preventDefault();
      openModal();
      return;
    }
    if (event.key === "j" || event.key === "k") {
      const ids = [...state.tasks.values()]
        .sort((a, b) => a.createdAt - b.createdAt)
        .map((task) => task.id);
      if (!ids.length) return;
      const index = ids.indexOf(state.current);
      const next = event.key === "j" ? index + 1 : index - 1;
      if (next >= 0 && next < ids.length) {
        event.preventDefault();
        selectTask(ids[next]);
      }
    }
  });
}

async function boot() {
  wire();
  connect();
  await refresh();
  autoSize();
  setInterval(() => {
    if (state.tasks.size) renderRail();
  }, 60000);
}

boot();
