// The inbox: orch's home view. It shows what needs a human decision,
// with the evidence attached, and folds everything calm into one line each.
// The point is attention routing: failures first, then review-ready work,
// then quiet activity that needs nothing.

import { $, el, api, ago, shortTokens, toast, shortSha } from "./util.js";
import { state, activeRepo, repoTasks, classify, inboxOrder, setView } from "./store.js";
import { selectTask, loadTask } from "./task.js";

function currentTask() {
  const repo = activeRepo();
  return repo && state.current ? repo.tasks.get(state.current) : null;
}

/** The last thing the agent said — evidence about intent, self-reported. */
function agentSummary(items) {
  for (let i = items.length - 1; i >= 0; i -= 1) {
    const item = items[i];
    if (item.kind === "agent" && item.text && item.text.trim()) {
      const text = item.text.trim();
      return text.length > 900 ? `${text.slice(0, 900)}…` : text;
    }
  }
  return null;
}

async function cardData(id) {
  if (state.cardData.has(id)) return state.cardData.get(id);
  try {
    const data = await api(`/api/tasks/${id}`);
    const payload = { items: data.items || [], behind: data.behind || 0 };
    state.behind[id] = data.behind || 0;
    const repo = activeRepo();
    if (repo) repo.tasks.set(id, data.task);
    state.cardData.set(id, payload);
    return payload;
  } catch (error) {
    toast(error.message, true);
    return { items: [], behind: 0 };
  }
}

function receiptNode(verify) {
  const box = el("div", "card-receipt");
  const head = el("div", "receipt-row mono");
  head.textContent =
    `receipt · ${verify.status} · ${ago(verify.at)}` +
    (verify.headSha ? ` · head ${shortSha(verify.headSha)}` : "") +
    (verify.baseSha ? ` · base ${shortSha(verify.baseSha)}` : "");
  box.appendChild(head);

  for (const result of verify.commands || []) {
    const row = el("div", `card-check mono ${result.code === 0 ? "ok" : "ng"}`);
    row.textContent = `${result.code === 0 ? "ok" : `exit ${result.code}`} · ${result.command}`;
    box.appendChild(row);
  }
  for (const finding of verify.policy || []) {
    const row = el("div", "card-policy mono");
    row.textContent =
      finding.kind === "path"
        ? `policy · touched ${finding.pattern} → ${finding.path}`
        : `policy · added \`${finding.pattern}\` in ${finding.path}`;
    box.appendChild(row);
  }
  return box;
}

function usageLine(task) {
  const usage = task.usage;
  if (!usage || (!usage.used && !usage.cost)) return null;
  const cost = typeof usage.cost === "number" ? `$${usage.cost.toFixed(4)}` : "";
  return `${shortTokens(usage.used)}${usage.size ? `/${shortTokens(usage.size)}` : ""} tok${cost ? ` · ${cost}` : ""}`;
}

function actionButton(label, cls, run) {
  const btn = el("button", `btn ${cls || ""}`);
  btn.type = "button";
  btn.textContent = label;
  btn.addEventListener("click", (event) => {
    event.stopPropagation();
    run();
  });
  return btn;
}

async function act(id, path, body) {
  try {
    const data = await api(path, {
      method: "POST",
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (data && data.message) toast(data.message);
    state.cardData.delete(id);
    await reloadTask(id);
  } catch (error) {
    if (/diff policy flagged/.test(error.message)) {
      if (window.confirm(`${error.message}\n\nMerge anyway, accepting the findings?`)) {
        await act(id, path, { force: true });
        return;
      }
      return;
    }
    toast(error.message, true);
  }
}

async function reloadTask(id) {
  try {
    const data = await api(`/api/tasks/${id}`);
    const repo = activeRepo();
    if (repo) repo.tasks.set(id, data.task);
    state.behind[id] = data.behind || 0;
  } catch {
    /* the task may be gone; renderInbox will drop it */
  }
  renderInbox();
  if (state.current === id && state.view === "task") {
    await loadTask(id);
  }
}

function cardBody(task, payload) {
  const body = el("div", "card-body");

  const summary = agentSummary(payload.items);
  if (summary) {
    const lead = el("div", "card-lead");
    const tag = el("div", "selfreport mono");
    tag.textContent = "agent summary — self-reported, not evidence";
    const text = el("p");
    text.textContent = summary;
    lead.append(tag, text);
    body.appendChild(lead);
  }

  if (task.verify) body.appendChild(receiptNode(task.verify));
  else {
    const none = el("div", "card-receipt");
    const row = el("div", "receipt-row mono");
    row.textContent =
      (state.settings && (state.settings.checks || []).length)
        ? "no receipt — verify has not run against this worktree"
        : "no gate configured for this repository";
    none.appendChild(row);
    body.appendChild(none);
  }

  const usage = usageLine(task);
  if (usage) {
    const line = el("div", "card-usage mono");
    line.textContent = usage;
    body.appendChild(line);
  }

  const actions = el("div", "card-actions");
  actions.appendChild(
    actionButton("open", "primary", () => selectTask(task.id)),
  );
  if ((state.behind[task.id] || 0) > 0) {
    actions.appendChild(
      actionButton(`rebase +${state.behind[task.id]}`, "ghost", () =>
        act(task.id, `/api/tasks/${task.id}/rebase`),
      ),
    );
  }
  const running = task.status === "running" || task.status === "waiting";
  if (!running) {
    actions.appendChild(
      actionButton("verify", "", () => act(task.id, `/api/tasks/${task.id}/verify`)),
    );
    if (task.status !== "merged") {
      actions.appendChild(
        actionButton("merge", "", () => act(task.id, `/api/tasks/${task.id}/merge`, {})),
      );
    }
  }
  actions.appendChild(
    actionButton("send back", "ghost", () => {
      selectTask(task.id);
      setTimeout(() => $("input") && $("input").focus(), 60);
    }),
  );
  body.appendChild(actions);
  return body;
}

async function expandCard(article, task) {
  const head = article.querySelector(".card-head");
  const open = article.classList.toggle("open");
  const bodySlot = article.querySelector(".card-body-slot");
  if (!open) {
    if (bodySlot.firstChild) bodySlot.firstChild.hidden = true;
    return;
  }
  head.setAttribute("aria-expanded", "true");
  if (!bodySlot.firstChild) {
    const payload = await cardData(task.id);
    const refreshed = repoTasks().find((t) => t.id === task.id) || task;
    const body = cardBody(refreshed, payload);
    bodySlot.appendChild(body);
  } else {
    bodySlot.firstChild.hidden = false;
  }
}

function attentionCard(task, info, active) {
  const article = el("article", `card kind-${info.kind}${active ? " cursor" : ""}`);
  article.dataset.id = task.id;

  const head = el("button", "card-head");
  head.type = "button";
  head.setAttribute("aria-expanded", "false");
  const dot = el("span", `dot ${task.status}`);
  const title = el("span", "card-title");
  title.textContent = task.title || "new task";
  const reason = el("span", `card-reason mono ${info.kind}`);
  reason.textContent = info.reason;
  const time = el("span", "card-time mono");
  time.textContent = ago(task.updatedAt || task.createdAt);
  head.append(dot, title, reason, time);

  const bodySlot = el("div", "card-body-slot");
  head.addEventListener("click", () => expandCard(article, task));
  article.append(head, bodySlot);
  return article;
}

function quietRow(task, cursor) {
  const row = el("button", `task-row${cursor ? " cursor" : ""}`);
  row.type = "button";
  row.dataset.id = task.id;

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
  if (task.status === "merged") {
    const badge = el("span", "check-badge merged");
    badge.textContent = "merged";
    sub.append(badge);
  } else if (task.verify && task.verify.status) {
    const badge = el("span", `check-badge ${task.verify.status}`);
    badge.textContent = task.verify.status.replace("_", " ");
    sub.append(badge);
  }

  row.append(top, sub);
  row.addEventListener("click", () => selectTask(task.id));
  return row;
}

export function renderInbox() {
  const repo = activeRepo();
  const attentionList = $("attention-list");
  const quietList = $("quiet-list");
  const quietHead = $("quiet-head");
  if (!attentionList || !quietList) return;
  attentionList.textContent = "";
  quietList.textContent = "";

  if (!repo) {
    $("inbox-counts").textContent = "";
    $("inbox-empty").hidden = false;
    $("quiet-head").hidden = true;
    $("inbox-empty-lead").textContent = "Open a repository to start supervising agents.";
    return;
  }

  const { attention, quiet } = inboxOrder();
  const rows = [...attention, ...quiet];

  attention.forEach((entry, index) => {
    attentionList.appendChild(attentionCard(entry.task, entry.info, index === state.inboxCursor));
  });
  quiet.forEach((entry, index) => {
    const cursor = attention.length + index;
    quietList.appendChild(quietRow(entry.task, cursor === state.inboxCursor));
  });
  quietHead.hidden = quiet.length === 0;
  const bothEmpty = attention.length === 0 && quiet.length === 0;
  $("inbox-empty").hidden = !bothEmpty;
  if (bothEmpty) {
    $("inbox-empty-lead").textContent = "No tasks yet. Create one and describe the change.";
  }

  const counts = attention.length
    ? `${attention.length} need${attention.length === 1 ? "s" : ""} you`
    : "all calm";
  $("inbox-counts").textContent = `${counts} · ${quiet.length} quiet`;
  $("task-count").textContent = `${attention.length + quiet.length}`;
}

/** j/k navigation over the inbox order. Returns the task id under the cursor. */
export function moveCursor(delta) {
  const rows = [...document.querySelectorAll("#attention-list .card, #quiet-list .task-row")];
  if (!rows.length) return null;
  state.inboxCursor = Math.min(Math.max(0, state.inboxCursor + delta), rows.length - 1);
  for (const row of rows) row.classList.remove("cursor");
  const target = rows[state.inboxCursor];
  target.classList.add("cursor");
  target.scrollIntoView({ block: "nearest" });
  return target.dataset.id || null;
}

export function cursorTaskId() {
  const rows = [...document.querySelectorAll("#attention-list .card, #quiet-list .task-row")];
  const target = rows[state.inboxCursor];
  return target ? target.dataset.id || null : null;
}

export async function newTask() {
  const repo = activeRepo();
  if (!repo) {
    document.dispatchEvent(new CustomEvent("orch:open-repos"));
    return;
  }
  try {
    const data = await api("/api/tasks", {
      method: "POST",
      body: JSON.stringify({ repo: state.currentRepo }),
    });
    repo.tasks.set(data.task.id, data.task);
    renderInbox();
    await selectTask(data.task.id);
    setTimeout(() => $("input") && $("input").focus(), 20);
  } catch (error) {
    toast(error.message, true);
  }
}

export function wireInbox() {
  $("attention-list").addEventListener("click", (event) => {
    const head = event.target.closest(".card-head");
    if (!head) return;
    const card = head.closest(".card");
    state.inboxCursor = [
      ...document.querySelectorAll("#attention-list .card, #quiet-list .task-row"),
    ].indexOf(card);
  });
  $("quiet-list").addEventListener("click", (event) => {
    const row = event.target.closest(".task-row");
    if (!row) return;
    state.inboxCursor = [
      ...document.querySelectorAll("#attention-list .card, #quiet-list .task-row"),
    ].indexOf(row);
  });
}
