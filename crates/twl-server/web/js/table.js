import { esc, fmt, fmtCost } from "/assets/js/format.js";

export function renderKeyTable(data, maxRows) {
  const panel = document.getElementById("keyPanel");
  const tbody = document.querySelector("#keyTable tbody");
  let keys = data.keys || [];

  if (!keys.length) {
    panel.style.display = "none";
    return;
  }

  panel.style.display = "";

  // Admin scope aggregates per user; per-user scope lists individual keys.
  const isAll = data.scope && data.scope.mode === "all";
  const title = panel.querySelector("h2");
  if (title) title.textContent = isAll ? "Token usage by user" : "Token usage by API key";
  const firstTh = panel.querySelector("thead th");
  if (firstTh) firstTh.textContent = isAll ? "User" : "Key";

  const total = keys.length;

  if (Number.isFinite(maxRows) && maxRows > 0) keys = keys.slice(0, maxRows);

  let html = keys.map(k =>
    `<tr>`
    + `<td class="model">${esc(k.label)}</td>`
    + `<td>${fmt(k.requests)}</td>`
    + `<td>${fmt(Math.max(k.input_tokens - k.cached_tokens, 0))}</td>`
    + `<td>${fmt(k.cached_tokens)}</td>`
    + `<td>${fmt(k.output_tokens)}</td>`
    + `<td><b>${fmt(k.total_tokens)}</b></td>`
    + `</tr>`
  ).join("");

  if (total > keys.length) {
    html += `<tr><td colspan="6" class="empty">Showing ${keys.length} of ${total} rows</td></tr>`;
  }

  tbody.innerHTML = html;
}

export function renderTable(data, model, maxRows) {
  const tbody = document.querySelector("#modelTable tbody");
  let rows = model
    ? data.models.filter(m => m.model === model)
    : data.models;

  if (!rows.length) {
    tbody.innerHTML = `<tr><td colspan="7" class="empty">No usage recorded yet</td></tr>`;
    return;
  }

  const total = rows.length;

  if (Number.isFinite(maxRows) && maxRows > 0) rows = rows.slice(0, maxRows);

  let html = rows.map(m =>
    `<tr>`
    + `<td class="model">${esc(m.model)}</td>`
    + `<td>${fmt(m.requests)}</td>`
    + `<td>${fmt(Math.max(m.input_tokens - m.cached_tokens, 0))}</td>`
    + `<td>${fmt(m.cached_tokens)}</td>`
    + `<td>${fmt(m.output_tokens)}</td>`
    + `<td><b>${fmt(m.total_tokens)}</b></td>`
    + `<td class="cost-cell">${fmtCost(m.cost ? m.cost.total : null, data?.currency)}</td>`
    + `</tr>`
  ).join("");

  if (total > rows.length) {
    html += `<tr><td colspan="7" class="empty">Showing ${rows.length} of ${total} rows</td></tr>`;
  }

  tbody.innerHTML = html;
}
