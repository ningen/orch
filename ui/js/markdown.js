// Markdown-ish rendering for agent messages. Moved as-is from the original
// single-file client; rendering stays in the DOM, never innerHTML.

import { el } from "./util.js";

function safeHref(url) {
  const value = String(url || "").trim();
  if (/^(https?:|mailto:)/i.test(value)) return value;
  if (value.startsWith("/") || value.startsWith("#")) return value;
  return null;
}

export function renderInline(parent, text) {
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

export function renderProse(box, text, streaming) {
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
