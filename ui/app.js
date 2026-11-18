"use strict";

/* orch — single page client for the orch daemon. */

const $ = (id) => document.getElementById(id);
const el = (tag, cls) => {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  return node;
};

const state = {
  repos: new Map(), // path -> { path, name, branch, tasks: Map<id, task> }
  currentRepo: null,
  current: null, // task id
  items: [],
  nodes: new Map(),
  pinned: true,
  models: [],
  connected: false,
  everConnected: false,
  browse: null,
  settings: null,
  behind: {},
  unseen: 0,
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

function activeRepo() {
  return state.currentRepo ? state.repos.get(state.currentRepo) : null;
}

function repoTasks() {
  const repo = activeRepo();
  return repo ? [...repo.tasks.values()].sort((a, b) => a.createdAt - b.createdAt) : [];
}

let toastTimer = null;
function toast(message, isError = false) {
  const node = $("toast");
  node.textContent = message;
  node.classList.toggle("error", isError);
  node.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(
    () => {
      node.hidden = true;
    },
    isError ? 9000 : 6000,
  );
}

/* ------------------------------------------------------------------ rail */

function renderRepoLabel() {
  const repo = activeRepo();
  $("repo-name").textContent = repo ? repo.name : "open a repository";
  $("repo-name").title = repo ? repo.path : "click to open a git repository";
  $("repo-branch").textContent = repo ? repo.branch : "";
}

function renderRail() {
  const list = $("task-list");
  list.textContent = "";
  const tasks = repoTasks();

  for (const task of tasks) {
    const row = el("button", "task-row");
    row.type = "button";
    row.dataset.id = task.id;
    if (task.id === state.current) row.classList.add("active");

    const top = el("div", "row-top");
    const dot = el("span", `dot ${task.status}`);
    const title = el("span", "row-title");
    title.textContent = task.title || "new task";
    top.append(dot, title);

    const sub = el("div", "row-sub");
    const branch = el("span", "sub-branch");
    branch.textContent = task.branch || "not started";
    const time = el("span");
    time.textContent = ago(task.updatedAt || task.createdAt);
    sub.append(branch, time);
    if (task.verify && task.verify.status) {
      const badge = el("span", `check-badge ${task.verify.status}`);
      badge.textContent = task.verify.status === "passed" ? "verified" : "failed";
      sub.append(badge);
    }

    row.append(top, sub);
    row.addEventListener("click", () => selectTask(task.id));
    list.appendChild(row);
  }

  $("task-count").textContent = tasks.length ? `${tasks.length}` : "";
  $("task-count").title = state.repos.size > 1 ? `${state.repos.size} repositories open` : "";
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
  const repo = activeRepo();
  const task = repo && state.current ? repo.tasks.get(state.current) : null;
  const topbar = $("topbar");
  const empty = $("empty");

  if (!task) {
    topbar.hidden = true;
    empty.hidden = false;
    $("input").disabled = true;
    $("send").disabled = true;
    $("empty-new").textContent = repo ? "Create the first task" : "Open a repository";
    $("empty-lead").textContent = repo
      ? "A task is an isolated git worktree and one live agent."
      : "Pick the git repository you want to work in.";
    $("empty-body").textContent = repo
      ? "Nothing touches your working tree until you commit and merge. Create a task, describe the change, then review the diff and the command output before anything lands."
      : "orch keeps one worktree per task, so several agents can work in parallel without ever sharing a working tree. Switch repositories at any time — their tasks keep running.";
    return;
  }

  empty.hidden = true;
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

function safeHref(url) {
  const value = String(url || "").trim();
  if (/^(https?:|mailto:)/i.test(value)) return value;
  if (value.startsWith("/") || value.startsWith("#")) return value;
  return null;
}

function renderInline(parent, text) {
  const pattern =
    /(`[^`]+`)|(\*\*[^*]+\*\*)|(__[^_]+__)|(~~[^~]+~~)|(\*[^*\n]+\*)|(_[^_\n]+_)|(\[[^\]]+\]\([^)\s]+\))/g;
  let last = 0;
  let match;
  while ((match = pattern.exec(text))) {
    if (match.index > last) {
      parent.appendChild(document.createTextNode(text.slice(last, match.index)));
    }
    const token = match[0];
    if (token.startsWith("`")) {
      const code = el("code");
      code.textContent = token.slice(1, -1);
      parent.appendChild(code);
    } else if (token.startsWith("**") || token.startsWith("__")) {
      const strong = el("strong");
      renderInline(strong, token.slice(2, -2));
      parent.appendChild(strong);
    } else if (token.startsWith("~~")) {
      const del = el("del");
      renderInline(del, token.slice(2, -2));
      parent.appendChild(del);
    } else if (token.startsWith("[")) {
      const parts = /^\[([^\]]+)\]\(([^)\s]+)\)$/.exec(token);
      const href = parts && safeHref(parts[2]);
      if (href) {
        const link = el("a");
        link.href = href;
        link.target = "_blank";
        link.rel = "noreferrer noopener";
        renderInline(link, parts[1]);
        parent.appendChild(link);
      } else {
        parent.appendChild(document.createTextNode(token));
      }
    } else {
      const em = el("em");
      renderInline(em, token.slice(1, -1));
      parent.appendChild(em);
    }
    last = pattern.lastIndex;
  }
  if (last < text.length) {
    parent.appendChild(document.createTextNode(text.slice(last)));
  }
}

function renderInlineLines(parent, text) {
  const lines = text.split("\n");
  lines.forEach((line, index) => {
    if (index) parent.appendChild(el("br"));
    renderInline(parent, line);
  });
}

function appendCode(box, body, lang) {
  const pre = el("pre");
  if (lang) pre.dataset.lang = lang;
  const code = el("code");
  code.textContent = body.replace(/\n+$/, "");
  pre.appendChild(code);
  box.appendChild(pre);
}

const BLOCK_START =
  /^(\s*```|#{1,6}\s|\s*>|\s*[-*+]\s+|\s*\d+\.\s+|[-*_]{3,}\s*$)/;

function isTableStart(lines, i) {
  return (
    lines[i].includes("|") &&
    i + 1 < lines.length &&
    /^\s*\|?\s*:?-{2,}/.test(lines[i + 1]) &&
    lines[i + 1].includes("|")
  );
}

function tableCells(line) {
  return line
    .trim()
    .replace(/^\|/, "")
    .replace(/\|$/, "")
    .split("|")
    .map((cell) => cell.trim());
}

function renderProse(box, text, streaming) {
  box.textContent = "";
  const lines = String(text).replace(/\r\n/g, "\n").split("\n");
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];

    if (!line.trim()) {
      i += 1;
      continue;
    }

    if (/^\s*```/.test(line)) {
      const lang = line.replace(/^\s*```/, "").trim();
      i += 1;
      const buf = [];
      while (i < lines.length && !/^\s*```/.test(lines[i])) {
        buf.push(lines[i]);
        i += 1;
      }
      i += 1;
      appendCode(box, buf.join("\n"), lang);
      continue;
    }

    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      const node = el(`h${heading[1].length}`);
      renderInline(node, heading[2].trim().replace(/\s+#+\s*$/, ""));
      box.appendChild(node);
      i += 1;
      continue;
    }

    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) {
      box.appendChild(el("hr"));
      i += 1;
      continue;
    }

    if (/^\s*>/.test(line)) {
      const buf = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) {
        buf.push(lines[i].replace(/^\s*>\s?/, ""));
        i += 1;
      }
      const quote = el("blockquote");
      renderProse(quote, buf.join("\n"), false);
      box.appendChild(quote);
      continue;
    }

    if (isTableStart(lines, i)) {
      const headers = tableCells(lines[i]);
      i += 2;
      const table = el("table");
      const thead = el("thead");
      const headRow = el("tr");
      headers.forEach((cell) => {
        const th = el("th");
        renderInline(th, cell);
        headRow.appendChild(th);
      });
      thead.appendChild(headRow);
      table.appendChild(thead);
      const tbody = el("tbody");
      while (i < lines.length && lines[i].includes("|") && lines[i].trim()) {
        const tr = el("tr");
        tableCells(lines[i]).forEach((cell) => {
          const td = el("td");
          renderInline(td, cell);
          tr.appendChild(td);
        });
        tbody.appendChild(tr);
        i += 1;
      }
      table.appendChild(tbody);
      box.appendChild(table);
      continue;
    }

    if (/^\s*[-*+]\s+/.test(line)) {
      const list = el("ul");
      while (i < lines.length && /^\s*[-*+]\s+/.test(lines[i])) {
        const li = el("li");
        renderInline(li, lines[i].replace(/^\s*[-*+]\s+/, ""));
        list.appendChild(li);
        i += 1;
      }
      box.appendChild(list);
      continue;
    }

    if (/^\s*\d+\.\s+/.test(line)) {
      const list = el("ol");
      while (i < lines.length && /^\s*\d+\.\s+/.test(lines[i])) {
        const li = el("li");
        renderInline(li, lines[i].replace(/^\s*\d+\.\s+/, ""));
        list.appendChild(li);
        i += 1;
      }
      box.appendChild(list);
      continue;
    }

    const buf = [line];
    i += 1;
    while (
      i < lines.length &&
      lines[i].trim() &&
      !BLOCK_START.test(lines[i]) &&
      !isTableStart(lines, i)
    ) {
      buf.push(lines[i]);
      i += 1;
    }
    const p = el("p");
    renderInlineLines(p, buf.join("\n"));
    box.appendChild(p);
  }
  if (streaming) box.appendChild(el("span", "caret"));
}

function shortPath(path) {
  const repo = activeRepo();
  const task = repo && state.current ? repo.tasks.get(state.current) : null;
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
        document.createTextNode(rendered.length > 4000 ? `${rendered.slice(0, 4000)}\n…` : rendered),
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

function clearStream() {
  state.items = [];
  state.nodes.clear();
  $("stream").textContent = "";
  $("diff").textContent = "";
  $("diff-meta").textContent = "";
  $("run-out").textContent = "";
}

/* ---------------------------------------------------------------- events */

function handle(event) {
  switch (event.type) {
    case "task": {
      const repo = state.repos.get(event.repo);
      if (!repo || !event.task) return;
      const previous = repo.tasks.get(event.task.id);
      repo.tasks.set(event.task.id, event.task);
      if (previous) notify(event.task, previous.status);
      scheduleRail();
      if (event.repo === state.currentRepo && event.task.id === state.current) renderHeader();
      return;
    }
    case "removed": {
      const repo = state.repos.get(event.repo);
      if (!repo) return;
      repo.tasks.delete(event.taskId);
      if (event.repo === state.currentRepo && event.taskId === state.current) {
        state.current = null;
        clearStream();
        renderHeader();
      }
      scheduleRail();
      return;
    }
    case "item": {
      if (event.repo !== state.currentRepo || event.taskId !== state.current) return;
      upsertItem(event.item);
      return;
    }
    case "delta": {
      if (event.repo !== state.currentRepo || event.taskId !== state.current) return;
      const item = state.items.find((x) => x.id === event.itemId);
      if (!item) return;
      item.text = (item.text || "") + event.text;
      schedulePaint(item);
      return;
    }
    case "run": {
      if (event.repo !== state.currentRepo || event.taskId !== state.current) return;
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

async function loadTask(id) {
  const data = await api(`/api/tasks/${id}`);
  if (state.current !== id) return;
  const repo = state.repos.get(state.currentRepo);
  if (repo) repo.tasks.set(id, data.task);
  state.items = data.items || [];
  state.behind[id] = data.behind || 0;
  renderStream();
  renderHeader();
  loadDiff();
}

async function selectTask(id) {
  const repo = activeRepo();
  if (!repo || !repo.tasks.has(id)) return;
  state.current = id;
  clearStream();
  renderRail();
  renderHeader();
  try {
    await loadTask(id);
  } catch (error) {
    toast(error.message, true);
  }
}

/* ------------------------------------------------------------ repositories */

function renderRepoList() {
  const list = $("repo-list");
  list.textContent = "";
  const repos = [...state.repos.values()].sort((a, b) => a.name.localeCompare(b.name));
  if (!repos.length) {
    const empty = el("div", "repo-empty");
    empty.textContent = "no repositories open yet";
    list.appendChild(empty);
    return;
  }
  for (const repo of repos) {
    const row = el("button", "repo-item");
    row.type = "button";
    if (repo.path === state.currentRepo) row.classList.add("active");

    const name = el("span", "repo-item-name");
    name.textContent = repo.name;
    const branch = el("span", "repo-item-branch mono");
    branch.textContent = repo.branch;
    const count = el("span", "repo-item-count mono");
    count.textContent = `${repo.tasks.size}`;
    const path = el("span", "repo-item-path mono");
    path.textContent = repo.path;

    const head = el("span", "repo-item-head");
    head.append(name, branch, count);

    row.append(head, path);
    row.title = `${repo.path}\n${repo.tasks.size} tasks`;
    row.addEventListener("click", () => switchRepo(repo.path));
    list.appendChild(row);
  }
}

function renderBrowse() {
  const browse = state.browse;
  if (!browse) return;
  $("browse-path").textContent = browse.path;
  $("browse-path").title = browse.path;
  $("browse-up").disabled = !browse.parent;

  const list = $("browse-list");
  list.textContent = "";
  for (const entry of browse.dirs) {
    const row = el("button", entry.repo ? "browse-row repo" : "browse-row");
    row.type = "button";
    const name = el("span", "browse-name");
    name.textContent = entry.name;
    const tag = el("span", "browse-tag mono");
    tag.textContent = entry.repo ? "git" : "";
    row.append(name, tag);
    row.addEventListener("click", () => {
      if (entry.repo) openRepo(entry.path);
      else browseTo(entry.path);
    });
    list.appendChild(row);
  }
  if (!browse.dirs.length) {
    const empty = el("div", "browse-empty");
    empty.textContent = "no subdirectories";
    list.appendChild(empty);
  }
}

async function browseTo(path) {
  try {
    state.browse = await api(`/api/browse?path=${encodeURIComponent(path)}`);
    renderBrowse();
  } catch (error) {
    toast(error.message, true);
  }
}

function openRepoModal() {
  $("repo-modal").hidden = false;
  renderRepoList();
  const start = state.currentRepo || (state.browse && state.browse.path) || "/";
  const parent = start.replace(/\/[^/]+$/, "") || "/";
  browseTo(parent);
  setTimeout(() => $("repo-path").focus(), 30);
}

function closeRepoModal() {
  $("repo-modal").hidden = true;
}

async function switchRepo(path) {
  if (path === state.currentRepo) {
    closeRepoModal();
    return;
  }
  try {
    await api("/api/repos/current", { method: "POST", body: JSON.stringify({ path }) });
    closeRepoModal();
    await refresh();
  } catch (error) {
    toast(error.message, true);
  }
}

async function openRepo(path) {
  const target = (path || "").trim();
  if (!target) return;
  try {
    await api("/api/repos", { method: "POST", body: JSON.stringify({ path: target }) });
    closeRepoModal();
    await refresh();
  } catch (error) {
    toast(error.message, true);
  }
}

/* --------------------------------------------------------------- checks */

function notify(task, previous) {
  const finished = task.status === "done" || task.status === "error";
  const wasRunning = previous === "running" || previous === "waiting";
  if (!finished || !wasRunning) return;
  if (!document.hidden && task.id === state.current) return;
  state.unseen += 1;
  document.title = `(${state.unseen}) orch`;
  if (typeof Notification !== "undefined" && Notification.permission === "granted") {
    const note = new Notification(task.title || "task finished", {
      body: task.status === "error" ? "failed" : "finished",
      tag: task.id,
    });
    note.onclick = () => {
      window.focus();
      selectTask(task.id);
    };
  }
}

function renderChecks() {
  const repo = activeRepo();
  const task = repo && state.current ? repo.tasks.get(state.current) : null;
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
    add.addEventListener("click", openSettings);
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
  $("check-run").disabled = !checks.length || !task || task.status === "running";
}

function renderChangesActions() {
  const task = state.current ? (activeRepo() || { tasks: new Map() }).tasks.get(state.current) : null;
  const behind = state.current ? (state.behind[state.current] || 0) : 0;
  $("rebase").hidden = !(behind > 0 && task);
  $("rebase").textContent = `rebase +${behind}`;
  $("reset").disabled = !task;
  $("copy-path").disabled = !task;
}

async function loadSettings() {
  if (!state.currentRepo) {
    state.settings = null;
    return;
  }
  try {
    state.settings = await api(`/api/settings?repo=${encodeURIComponent(state.currentRepo)}`);
  } catch (error) {
    state.settings = null;
    toast(error.message, true);
  }
}

function openSettings() {
  if (!state.currentRepo) {
    openRepoModal();
    return;
  }
  const settings = state.settings || { checks: [], deny: [], suggested: [] };
  $("settings-modal").hidden = false;
  $("settings-path").textContent = settings.dir || "~/.orch";
  $("settings-checks").value = (settings.checks || []).join("\n");
  $("settings-deny").value = (settings.deny || []).join("\n");

  const wrap = $("settings-suggest");
  const list = $("suggest-list");
  list.textContent = "";
  const suggested = (settings.suggested || []).filter(
    (command) => !(settings.checks || []).includes(command),
  );
  wrap.hidden = !suggested.length;
  for (const command of suggested) {
    const chip = el("button");
    chip.type = "button";
    chip.textContent = command;
    chip.addEventListener("click", () => {
      const box = $("settings-checks");
      box.value = box.value.trim() ? `${box.value.trim()}\n${command}` : command;
    });
    list.appendChild(chip);
  }
  setTimeout(() => $("settings-checks").focus(), 20);
}

function closeSettings() {
  $("settings-modal").hidden = true;
}

async function saveSettings(event) {
  event.preventDefault();
  const lines = (id) =>
    $(id)
      .value.split("\n")
      .map((line) => line.trim())
      .filter(Boolean);
  try {
    await api("/api/settings", {
      method: "PUT",
      body: JSON.stringify({
        repo: state.currentRepo,
        checks: lines("settings-checks"),
        deny: lines("settings-deny"),
      }),
    });
    await loadSettings();
    closeSettings();
    renderChecks();
    toast("checks saved");
  } catch (error) {
    toast(error.message, true);
  }
}

async function copyPath() {
  const task = state.current ? (activeRepo() || { tasks: new Map() }).tasks.get(state.current) : null;
  if (!task) return;
  try {
    await navigator.clipboard.writeText(task.worktree);
    toast("worktree path copied");
  } catch {
    toast(task.worktree);
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
  const repo = activeRepo();
  const task = repo && repo.tasks.get(id);
  if (task && (task.status === "running" || task.status === "waiting")) {
    toast("the agent is still working — stop it first", true);
    return;
  }
  input.value = "";
  autoSize();
  try {
    await api(`/api/tasks/${id}/prompt`, { method: "POST", body: JSON.stringify({ text }) });
  } catch (error) {
    input.value = text;
    autoSize();
    toast(error.message, true);
  }
}

function openModal() {
  if (!state.currentRepo) {
    openRepoModal();
    return;
  }
  $("modal").hidden = false;
  $("new-prompt").value = "";
  const repoSelect = $("new-repo");
  repoSelect.textContent = "";
  for (const repo of state.repos.values()) {
    const opt = el("option");
    opt.value = repo.path;
    opt.textContent = repo.name;
    opt.title = repo.path;
    repoSelect.appendChild(opt);
  }
  repoSelect.value = state.currentRepo || "";
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
  setTimeout(() => $("new-prompt").focus(), 20);
}

function closeModal() {
  $("modal").hidden = true;
}

async function createTask(event) {
  event.preventDefault();
  const prompt = $("new-prompt").value.trim();
  const model = $("new-model").value || null;
  const repo = $("new-repo").value || state.currentRepo;
  const body = {};
  if (prompt) body.prompt = prompt;
  if (model) body.model = model;
  if (repo) body.repo = repo;
  try {
    const data = await api("/api/tasks", { method: "POST", body: JSON.stringify(body) });
    closeModal();
    // The task may belong to a repository other than the active one, and the
    // daemon makes the chosen repository current. Reload before selecting it.
    await refresh();
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
    const repos = new Map();
    for (const repo of data.repos || []) {
      repos.set(repo.path, {
        path: repo.path,
        name: repo.name,
        branch: repo.branch,
        tasks: new Map((repo.tasks || []).map((task) => [task.id, task])),
      });
      if (!state.models.length) {
        for (const task of repo.tasks || []) {
          if (task.models && task.models.length) {
            state.models = task.models;
            break;
          }
        }
      }
    }
    state.repos = repos;
    state.currentRepo =
      data.current && repos.has(data.current) ? data.current : (repos.keys().next().value || null);

    await loadSettings();
    renderRepoLabel();
    renderRail();

    const repo = activeRepo();
    if (!repo) {
      state.current = null;
      clearStream();
      renderHeader();
      return;
    }
    if (state.current && repo.tasks.has(state.current)) {
      await loadTask(state.current);
      return;
    }
    const tasks = repoTasks();
    if (tasks.length) await selectTask(tasks[tasks.length - 1].id);
    else {
      state.current = null;
      clearStream();
      renderHeader();
    }
  } catch (error) {
    toast(error.message, true);
  }
}

/* ------------------------------------------------------------------- init */

function wire() {
  $("repo-switch").addEventListener("click", openRepoModal);
  $("repo-cancel").addEventListener("click", closeRepoModal);
  $("repo-modal").addEventListener("click", (event) => {
    if (event.target === $("repo-modal")) closeRepoModal();
  });
  $("browse-up").addEventListener("click", () => {
    if (state.browse && state.browse.parent) browseTo(state.browse.parent);
  });
  $("repo-open-form").addEventListener("submit", (event) => {
    event.preventDefault();
    openRepo($("repo-path").value);
  });

  $("new-task").addEventListener("click", openModal);
  $("empty-new").addEventListener("click", () => {
    if (state.currentRepo) openModal();
    else openRepoModal();
  });
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
      const repo = activeRepo();
      if (repo) repo.tasks.set(id, data.task);
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
  $("copy-path").addEventListener("click", copyPath);

  $("check-edit").addEventListener("click", openSettings);
  $("settings-cancel").addEventListener("click", closeSettings);
  $("settings-form").addEventListener("submit", saveSettings);
  $("settings-modal").addEventListener("click", (event) => {
    if (event.target === $("settings-modal")) closeSettings();
  });

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

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      if (!$("settings-modal").hidden) closeSettings();
      else if (!$("repo-modal").hidden) closeRepoModal();
      else if (!$("modal").hidden) closeModal();
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
    if (event.key === "r") {
      event.preventDefault();
      openRepoModal();
      return;
    }
    if (event.key === "j" || event.key === "k") {
      const ids = repoTasks().map((task) => task.id);
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
  if (typeof Notification !== "undefined" && Notification.permission === "default") {
    Notification.requestPermission().catch(() => {});
  }
  const clearBadge = () => {
    state.unseen = 0;
    document.title = "orch";
  };
  window.addEventListener("focus", clearBadge);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) clearBadge();
  });
  setInterval(() => {
    if (state.repos.size) renderRail();
  }, 60000);
}

boot();
