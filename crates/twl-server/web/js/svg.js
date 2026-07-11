const SVGNS = "http://www.w3.org/2000/svg";

export { SVGNS };

function el(tag, attrs, text) {
  const e = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs || {})) e.setAttribute(k, v);
  if (text !== undefined) e.textContent = text;
  return e;
}

function bar(svg, x, y, w, h, fill) {
  if (h <= 0) return null;
  const r = el("rect", { x, y, width: w, height: Math.max(0, h), rx: 2, fill });
  svg.appendChild(r);
  return r;
}

function sparkSegment(svg, line, segPts, plotH, { gradId, color, strokeWidth, strokeOpacity, fillOpacity }) {
  if (!line) return;
  svg.appendChild(el("path", {
    d: line, fill: "none", stroke: color, "stroke-width": String(strokeWidth),
    "stroke-linecap": "round", "stroke-linejoin": "round", opacity: strokeOpacity,
  }));
  if (!segPts || segPts.length === 0) return;
  const first = segPts[0], last = segPts[segPts.length - 1];
  const fillD = line + " L" + last.x + "," + (2 + plotH) + " L" + first.x + "," + (2 + plotH) + " Z";
  const defs = el("defs");
  const grad = el("linearGradient", { id: gradId, x1: "0", y1: "0", x2: "0", y2: "1" });
  grad.appendChild(el("stop", { offset: "0%", "stop-color": color, "stop-opacity": fillOpacity }));
  grad.appendChild(el("stop", { offset: "100%", "stop-color": color, "stop-opacity": "0" }));
  defs.appendChild(grad);
  svg.appendChild(defs);
  svg.appendChild(el("path", { d: fillD, fill: "url(#" + gradId + ")", stroke: "none" }));
}

export { el, bar, sparkSegment };
