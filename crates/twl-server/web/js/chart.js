import { fmt, fmtK, fmtKwh } from "/assets/js/format.js";
import { SVGNS, el, bar } from "/assets/js/svg.js";

// When all models are shown, multiple rows share the same day - aggregate into one.
// When filtered to one model, rows are already one-per-day.
function chartData(data, model) {
  if (model) {
    return data.days
      .filter(d => d.model === model)
      .sort((a, b) => a.day < b.day ? -1 : 1);
  }
  const byDay = new Map();
  data.days.forEach(d => {
    const e = byDay.get(d.day) || {
      day: d.day,
      input_tokens: 0,
      output_tokens: 0,
      cached_tokens: 0,
      total_tokens: 0,
      requests: 0,
    };
    e.input_tokens += d.input_tokens;
    e.output_tokens += d.output_tokens;
    e.cached_tokens += d.cached_tokens;
    e.total_tokens += d.total_tokens;
    e.requests += d.requests;
    byDay.set(d.day, e);
  });
  return [...byDay.values()].sort((a, b) => a.day < b.day ? -1 : 1);
}

export function renderChart(dash, { model, days }) {
  if (!dash) return;
  const svg = document.getElementById("chart");
  svg.innerHTML = "";
  const wrap = svg.parentElement;

  const W = wrap.clientWidth;
  const tokenData = chartData(dash, model);
  // Energy is recorded globally, not per model - only overlay it when
  // the chart shows all models.
  const energyRaw = !model && Array.isArray(dash.energy_days)
    ? dash.energy_days
    : [];
  const hasEnergy = energyRaw.length > 0;

  const H = 260, padL = 52, padR = hasEnergy ? 52 : 12, padT = 12, padB = 34;
  svg.setAttribute("width", W);
  svg.setAttribute("viewBox", `0 0 ${W} ${H}`);

  // Every calendar day in the selected range, zero-filled, so days
  // without traffic still occupy space on the time axis.
  const dates = [];
  const today = new Date();
  for (let i = days - 1; i >= 0; i--) {
    const dt = new Date(today.getFullYear(), today.getMonth(), today.getDate() - i);
    dates.push(dt.getFullYear() + "-"
      + String(dt.getMonth() + 1).padStart(2, "0") + "-"
      + String(dt.getDate()).padStart(2, "0"));
  }

  // Build merged data map
  const tokenMap = new Map();
  tokenData.forEach(d => tokenMap.set(d.day, d));
  const energyMap = new Map();
  energyRaw.forEach(d => energyMap.set(d.day, d));
  const data = dates.map(day => {
    const t = tokenMap.get(day) || { day, input_tokens: 0, output_tokens: 0, cached_tokens: 0, total_tokens: 0, requests: 0 };
    const e = energyMap.get(day);
    const kwh = (e && typeof e === 'object' && 'kwh' in e) ? (Number(e.kwh) || 0) : 0;
    return { ...t, kwh };
  });

  const hasAnyData = tokenData.length > 0 || hasEnergy;
  if (!hasAnyData) {
    const t = document.createElementNS(SVGNS, "text");
    t.setAttribute("x", W / 2); t.setAttribute("y", H / 2);
    t.setAttribute("text-anchor", "middle");
    t.textContent = "No usage in this range";
    svg.appendChild(t);
    return;
  }

  const plotW = W - padL - padR, plotH = H - padT - padB;
  const bw = Math.min(28, plotW / data.length * 0.62);
  const step = plotW / data.length;

  // Scales
  const maxTokens = Math.max(1, ...data.map(d => d.input_tokens + d.output_tokens));
  const maxEnergy = hasEnergy ? Math.max(1e-6, ...data.map(d => {
    const v = d.kwh;
    return (typeof v === 'number' && isFinite(v) && v > 0) ? v : 1e-6;
  })) : 0;
  const yToken = v => padT + plotH - (v / maxTokens) * plotH;
  const yEnergy = v => padT + plotH - (v / maxEnergy) * plotH;

  // Left Y axis (tokens)
  for (let i = 0; i <= 4; i++) {
    const v = maxTokens * i / 4, yy = yToken(v);
    svg.appendChild(el("line", { x1: padL, x2: W - padR, y1: yy, y2: yy, stroke: "var(--grid)" }));
    svg.appendChild(el("text", { x: padL - 8, y: yy + 3, "text-anchor": "end" }, fmtK(Math.round(v))));
  }

  // Right Y axis (kWh)
  if (hasEnergy) {
    // Labels only - the left axis already draws the gridlines, and both
    // scales span the same plot height so the tick positions coincide.
    for (let i = 0; i <= 4; i++) {
      const v = maxEnergy * i / 4, yy = yEnergy(v);
      svg.appendChild(el("text", {
        x: W - padR + 8, y: yy + 3, "text-anchor": "start",
        "font-size": "10", "fill": "#3aff5c"
      }, fmtKwh(v, { skipUnit: true })));
    }
    svg.appendChild(el("text", {
      x: W - padR + 8, y: padT - 2, "text-anchor": "start",
      "font-size": "10", "fill": "#3aff5c", "font-weight": "bold"
    }, "kWh"));

    // Show energy legend item
    const legendItem = document.querySelectorAll(".legend span")[3];
    if (legendItem) legendItem.style.display = "";
  } else {
    const legendItem = document.querySelectorAll(".legend span")[3];
    if (legendItem) legendItem.style.display = "none";
  }

  // X labels
  data.forEach((d, i) => {
    const cx = padL + step * i + step / 2;
    if (data.length <= 15 || i % Math.ceil(data.length / 15) === 0) {
      svg.appendChild(el("text", { x: cx, y: H - padB + 16, "text-anchor": "middle" }, d.day.slice(5)));
    }
  });

  // Token bars
  data.forEach((d, i) => {
    const cx = padL + step * i + step / 2;
    const x = cx - bw / 2;
    bar(svg, x, yToken(d.input_tokens), bw, (d.input_tokens / maxTokens) * plotH, "#00f0ff");

    if (d.cached_tokens > 0) {
      const cache = Math.min(d.cached_tokens, d.input_tokens);
      const hCache = (cache / maxTokens) * plotH;
      bar(svg, x, padT + plotH - hCache, bw, hCache, "#2a2a4e");
    }

    bar(svg, x, yToken(d.input_tokens + d.output_tokens), bw,
      (d.output_tokens / maxTokens) * plotH, "#ff0055");
  });

  // Energy line + points
  if (hasEnergy) {
    const points = data.map((d, i) => {
      const cx = padL + step * i + step / 2;
      return { cx, cy: yEnergy(d.kwh || 0), day: d.day, kwh: d.kwh };
    });

    // Line
    const linePath = points.map((p, i) => (i === 0 ? "M" : "L") + p.cx + "," + p.cy).join(" ");
    svg.appendChild(el("path", { d: linePath, fill: "none", stroke: "#ffa500", "stroke-width": "2" }));

    // Points
    points.forEach(p => {
      if (p.kwh > 0) {
        svg.appendChild(el("circle", { cx: p.cx, cy: p.cy, r: 3.5, fill: "#ffa500" }));
      }
    });
  }

  // Hit rects + tooltips
  data.forEach((d, i) => {
    const cx = padL + step * i + step / 2;
    const hit = document.createElementNS(SVGNS, "rect");
    hit.setAttribute("x", padL + step * i); hit.setAttribute("y", padT);
    hit.setAttribute("width", step); hit.setAttribute("height", plotH);
    hit.setAttribute("fill", "transparent");
    const title = document.createElementNS(SVGNS, "title");
    let tip = d.day + "\n";
    tip += "input: " + fmt(d.input_tokens) + " (cached " + fmt(d.cached_tokens) + ")\n";
    tip += "output: " + fmt(d.output_tokens) + "\n";
    tip += "total: " + fmt(d.total_tokens) + "\n";
    tip += "requests: " + fmt(d.requests);
    if (hasEnergy) {
      tip += "\nkWh: " + fmtKwh(d.kwh);
    }
    title.textContent = tip;
    hit.appendChild(title);
    svg.appendChild(hit);
  });
}
