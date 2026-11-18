// Modal sheets: repository picker and the gate settings (checks, deny
// patterns, and the diff policy). The settings live outside the repository
// on purpose — an agent must not be able to rewrite its own gate.

import { $, el, api, toast } from "./util.js";
import { state, activeRepo } from "./store.js";

let onReposChanged = async () => {};
let onSettingsChanged = () => {};

export function setHooks(hooks) {
  if (hooks.onReposChanged) onReposChanged = hooks.onReposChanged;
  if (hooks.onSettingsChanged) onSettingsChanged = hooks.onSettingsChanged;
}

/* ------------------------------------------------------- repository picker */

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

export function openRepoModal() {
  $("repo-modal").hidden = false;
  renderRepoList();
  const start = state.currentRepo || (state.browse && state.browse.path) || "/";
  const parent = start.replace(/\/[^/]+$/, "") || "/";
  browseTo(parent);
  setTimeout(() => $("repo-path").focus(), 30);
}

export function closeRepoModal() {
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
    await onReposChanged();
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
    await onReposChanged();
  } catch (error) {
    toast(error.message, true);
  }
}

/* ------------------------------------------------------------ gate settings */

export function openSettings() {
  if (!state.currentRepo) {
    openRepoModal();
    return;
  }
  const settings = state.settings || {
    checks: [],
    deny: [],
    policyPaths: [],
    policyLines: [],
    suggested: [],
    suggestedPolicyPaths: [],
    suggestedPolicyLines: [],
  };
  $("settings-modal").hidden = false;
  $("settings-path").textContent = settings.dir || "~/.orch";
  $("settings-checks").value = (settings.checks || []).join("\n");
  $("settings-deny").value = (settings.deny || []).join("\n");
  $("settings-policy-paths").value = (settings.policyPaths || []).join("\n");
  $("settings-policy-lines").value = (settings.policyLines || []).join("\n");

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

  const policyWrap = $("settings-policy-suggest");
  const policyList = $("policy-suggest-list");
  policyList.textContent = "";
  const suggestPolicy = [
    ...(settings.suggestedPolicyPaths || []).map((p) => ({ kind: "path", value: p })),
    ...(settings.suggestedPolicyLines || []).map((p) => ({ kind: "line", value: p })),
  ].filter(
    ({ kind, value }) =>
      !(kind === "path"
        ? (settings.policyPaths || []).includes(value)
        : (settings.policyLines || []).includes(value)),
  );
  policyWrap.hidden = !suggestPolicy.length;
  for (const entry of suggestPolicy) {
    const chip = el("button");
    chip.type = "button";
    chip.textContent = entry.value;
    chip.addEventListener("click", () => {
      const box = $(entry.kind === "path" ? "settings-policy-paths" : "settings-policy-lines");
      box.value = box.value.trim() ? `${box.value.trim()}\n${entry.value}` : entry.value;
    });
    policyList.appendChild(chip);
  }
  setTimeout(() => $("settings-checks").focus(), 20);
}

export function closeSettings() {
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
        policyPaths: lines("settings-policy-paths"),
        policyLines: lines("settings-policy-lines"),
      }),
    });
    await onSettingsChanged();
    closeSettings();
    toast("gate settings saved");
  } catch (error) {
    toast(error.message, true);
  }
}

/* -------------------------------------------------------------------- wire */

export function wireModals() {
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

  $("settings-cancel").addEventListener("click", closeSettings);
  $("settings-form").addEventListener("submit", saveSettings);
  $("settings-modal").addEventListener("click", (event) => {
    if (event.target === $("settings-modal")) closeSettings();
  });

  document.addEventListener("orch:open-repos", openRepoModal);
  document.addEventListener("orch:open-settings", openSettings);
}
