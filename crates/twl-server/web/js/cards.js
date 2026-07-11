import { esc, fmt, fmtK, fmtCost, fmtKwh } from "/assets/js/format.js";

export function renderCards(data, { model, days, totalKwh, cards: cardsOption }) {
  const daysData = model
    ? data.days.filter(d => d.model === model)
    : data.days;
  const sum = k => daysData.reduce((a, r) => a + (r[k] || 0), 0);
  const totalPrompt = sum("input_tokens");
  const totalCompletion = sum("output_tokens");
  const totalCached = sum("cached_tokens");
  const totalReq = sum("requests");
  const hitRate = totalPrompt ? (100 * totalCached / totalPrompt) : 0;
  const totalCost = daysData.reduce((a, d) => a + (d.cost?.total || 0), 0);

  const cardMap = {
    cost: { label: "Total cost", value: fmtCost(totalCost, data?.currency), sub: model || "all models", cls: "cost" },
    energy: {
      label: "GPU energy",
      value: fmtKwh(totalKwh),
      // Energy is recorded globally, not per model, so this card ignores
      // the model filter - say so instead of implying it's filtered.
      sub: model ? "all models, " + days + " days" : days + " days",
    },
    tokens: { label: "Total tokens", value: fmtK(totalPrompt + totalCompletion), sub: fmt(totalPrompt + totalCompletion) },
    requests: { label: "Requests", value: fmtK(totalReq), sub: model || "all models" },
    io: { label: "Input / Output", value: fmtK(totalPrompt) + " / " + fmtK(totalCompletion), sub: "tokens" },
    cache_hit: { label: "Cache hit rate", value: hitRate.toFixed(1) + "%", sub: fmt(totalCached) + " cached tokens", cls: "cache" },
  };

  const defaultOrder = ["cost", "energy", "tokens", "requests", "io", "cache_hit"];
  const ids = Array.isArray(cardsOption) ? cardsOption : defaultOrder;
  const activeCards = ids.map(id => cardMap[id]).filter(Boolean);

  document.getElementById("cards").innerHTML = activeCards.map(c =>
    `<div class="card"><div class="label">${esc(c.label)}</div>`
    + `<div class="value${c.cls ? ' ' + c.cls : ''}">${esc(c.value)}</div>`
    + `<div class="sub">${esc(c.sub)}</div></div>`
  ).join("");
}
