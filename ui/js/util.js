// Small shared helpers. No state lives here.

export const $ = (id) => document.getElementById(id);

export function el(tag, cls) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  return node;
}

export async function api(path, opts = {}) {
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

export function ago(ms) {
  const secs = Math.max(0, (Date.now() - ms) / 1000);
  if (secs < 45) return "now";
  if (secs < 3600) return `${Math.round(secs / 60)}m`;
  if (secs < 86400) return `${Math.round(secs / 3600)}h`;
  return `${Math.round(secs / 86400)}d`;
}

export function shortTokens(n) {
  if (!n) return "0";
  if (n < 1000) return String(n);
  return `${(n / 1000).toFixed(1)}k`;
}

let toastTimer = null;
export function toast(message, isError = false) {
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

export function shortSha(sha) {
  return (sha || "").slice(0, 7);
}
