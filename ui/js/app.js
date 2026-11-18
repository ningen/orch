// orch client entry. Wires the inbox, the task lens, and the modals to one
// SSE feed. Views: "inbox" is home; "task" is a lens opened from it.

import { $, el, api, toast } from "./util.js";
import { state, activeRepo, repoTasks, setView } from "./store.js";
import {
  renderHeader,
  renderChecks,
  renderChangesActions,
  upsertItem,
  appendRun,
  clearStream,
  loadTask,
  selectTask,
  backToInbox,
  wireTaskView,
  schedulePaintItem,
} from "./task.js";
import { renderInbox, newTask, moveCursor, cursorTaskId, wireInbox } from "./inbox.js";
import { wireModals, setHooks, openRepoModal } from "./modals.js";

/* --------------------------------------------------------------- events */

function notify(task, previous) {
  const finished = task.status === "done" || task.status === "error";
  const wasRunning = previous === "running" || previous === "waiting";
  if (!finished || !wasRunning) return;
  if (!document.hidden && state.view === "task" && task.id === state.current) return;
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

function handle(event) {
  switch (event.type) {
    case "task": {
      const repo = state.repos.get(event.repo);
      if (!repo || !event.task) return;
      const previous = repo.tasks.get(event.task.id);
      repo.tasks.set(event.task.id, event.task);
      if (previous) notify(event.task, previous.status);
      // A receipt update or a status change makes card evidence stale.
      if (
        previous &&
        (JSON.stringify(previous.verify) !== JSON.stringify(event.task.verify) ||
          previous.status !== event.task.status)
      ) {
        state.cardData.delete(event.task.id);
      }
      renderInbox();
      if (state.view === "task" && event.repo === state.currentRepo && event.task.id === state.current) {
        renderHeader();
      }
      return;
    }
    case "removed": {
      const repo = state.repos.get(event.repo);
      if (!repo) return;
      repo.tasks.delete(event.taskId);
      state.cardData.delete(event.taskId);
      if (state.view === "task" && event.repo === state.currentRepo && event.taskId === state.current) {
        backToInbox();
        clearStream();
      }
      renderInbox();
      return;
    }
    case "item": {
      if (state.view !== "task" || event.repo !== state.currentRepo || event.taskId !== state.current)
        return;
      upsertItem(event.item);
      return;
    }
    case "delta": {
      if (state.view !== "task" || event.repo !== state.currentRepo || event.taskId !== state.current)
        return;
      const item = state.items.find((x) => x.id === event.itemId);
      if (!item) return;
      item.text = (item.text || "") + event.text;
      schedulePaintItem(item);
      return;
    }
    case "run": {
      if (state.view !== "task" || event.repo !== state.currentRepo || event.taskId !== state.current)
        return;
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

/* --------------------------------------------------------------- refresh */

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
      for (const task of repo.tasks || []) {
        if (typeof task.behind === "number") state.behind[task.id] = task.behind;
      }
    }
    state.repos = repos;
    state.currentRepo =
      data.current && repos.has(data.current) ? data.current : (repos.keys().next().value || null);

    await loadSettings();
    renderRepoLabel();
    renderInbox();

    // Return to the inbox on a full refresh; a live task view re-opens its
    // own data below.
    if (state.view === "task" && state.current) {
      const repo = activeRepo();
      if (repo && repo.tasks.has(state.current)) {
        await loadTask(state.current);
        return;
      }
    }
    setView("inbox");
    state.current = null;
    clearStream();
  } catch (error) {
    toast(error.message, true);
  }
}

function renderRepoLabel() {
  const repo = activeRepo();
  $("repo-name").textContent = repo ? repo.name : "open a repository";
  $("repo-name").title = repo ? repo.path : "click to open a git repository";
  $("repo-branch").textContent = repo ? repo.branch : "";
}

/* --------------------------------------------------------------- wiring */

function wire() {
  $("repo-switch").addEventListener("click", openRepoModal);
  $("new-task").addEventListener("click", newTask);

  wireInbox();
  wireTaskView();
  wireModals();
  setHooks({
    onReposChanged: async () => {
      state.current = null;
      setView("inbox");
      clearStream();
      await refresh();
    },
    onSettingsChanged: async () => {
      await loadSettings();
      renderInbox();
      if (state.view === "task") {
        renderChecks();
        renderChangesActions();
      }
    },
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      if (!$("settings-modal").hidden) {
        $("settings-modal").hidden = true;
        return;
      }
      if (!$("repo-modal").hidden) {
        $("repo-modal").hidden = true;
        return;
      }
      if (state.view === "task") {
        backToInbox();
        renderInbox();
        return;
      }
      // Collapse an open card, if any.
      const open = document.querySelector("#attention-list .card.open");
      if (open) open.classList.remove("open");
      return;
    }
    const typing =
      event.target instanceof HTMLInputElement ||
      event.target instanceof HTMLTextAreaElement ||
      event.target instanceof HTMLSelectElement;
    if (typing || event.metaKey || event.ctrlKey || event.altKey) return;

    if (event.key === "n") {
      event.preventDefault();
      newTask();
      return;
    }
    if (event.key === "r") {
      event.preventDefault();
      openRepoModal();
      return;
    }
    if (state.view === "inbox") {
      if (event.key === "j" || event.key === "k") {
        event.preventDefault();
        moveCursor(event.key === "j" ? 1 : -1);
        return;
      }
      if (event.key === "Enter") {
        event.preventDefault();
        const id = cursorTaskId();
        if (id) selectTask(id);
        return;
      }
    }
  });

  const clearBadge = () => {
    state.unseen = 0;
    document.title = "orch";
  };
  window.addEventListener("focus", clearBadge);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) clearBadge();
  });
  setInterval(() => {
    if (state.repos.size) renderInbox();
  }, 60000);
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

async function boot() {
  wire();
  connect();
  setView("inbox");
  await refresh();
  if (typeof Notification !== "undefined" && Notification.permission === "default") {
    Notification.requestPermission().catch(() => {});
  }
}

boot();
