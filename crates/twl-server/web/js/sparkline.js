import { fmt, fmtK } from "/assets/js/format.js";
import { el, sparkSegment } from "/assets/js/svg.js";

export function renderSparkline(today, yesterday) {
  const svg = document.getElementById("sparkline");
  svg.innerHTML = "";
  const wrap = svg.parentElement;

  const W = wrap.clientWidth;
  const H = 52;
  const PAD_L = 38, PAD_R = 8;
  const plotW = W - PAD_L - PAD_R;
  const plotH = H - 4;

  const buckets = today && today.length === 288 ? today : null;
  if (!buckets) return;

  // Build 288 five-minute intervals: today up to the current bucket,
  // yesterday for the rest of the day (grayed)
  const now = new Date();
  const curBucket = now.getHours() * 12 + Math.floor(now.getMinutes() / 5);
  const todayBuckets = buckets; // already 288
  const yesterdayBuckets = yesterday && yesterday.length === 288 ? yesterday : null;

  let maxTok = Math.max(1, ...todayBuckets.map(b => b.input_tokens + b.output_tokens));
  if (yesterdayBuckets) {
    yesterdayBuckets.forEach(b => { const t = b.input_tokens + b.output_tokens; if (t > maxTok) maxTok = t; });
  }

  const step = plotW / 288;

  // Y-axis labels - tokens (left side)
  const yTicks = [0, 0.25, 0.5, 0.75, 1];
  yTicks.forEach(t => {
    const y = 2 + plotH * (1 - t);
    svg.appendChild(el("text", {
      x: PAD_L - 3, y: y + 6, "text-anchor": "end",
      "font-size": "8",
    }, fmtK(Math.round(t * maxTok))));
  });
  // "tokens" rotated label
  const label = el("text", {
    x: 6, y: 2 + plotH / 2,
    "text-anchor": "middle",
    "font-size": "8",
    transform: "rotate(-90, 6," + (2 + plotH / 2) + ")",
    opacity: "0.6",
  }, "tokens");
  svg.appendChild(label);

  // Hour labels - 0h through 24h
  [0, 6, 12, 18].forEach(h => {
    const x = PAD_L + step * (h * 12) + step / 2;
    svg.appendChild(el("text", {
      x, y: H - 1, "text-anchor": "middle",
      "font-size": "9",
    }, String(h).padStart(2, "0") + "h"));
  });
  // 24h at right edge
  svg.appendChild(el("text", {
    x: W - PAD_R, y: H - 1, "text-anchor": "end",
    "font-size": "9",
  }, "24h"));

  // Build 288 points for the line
  const pts = [];
  for (let b = 0; b < 288; b++) {
    const fromYesterday = b > curBucket;
    const src = fromYesterday ? yesterdayBuckets : todayBuckets;
    const total = src && src[b] ? src[b].input_tokens + src[b].output_tokens : 0;
    pts.push({
      x: PAD_L + step * b + step / 2,
      y: 2 + plotH - (total / maxTok) * plotH,
      fromYesterday,
      total,
    });
  }

  // Split into today and yesterday segments for separate paths
  let todayLine = "", yesterdayLine = "";
  pts.forEach(p => {
    if (p.fromYesterday) {
      yesterdayLine += (yesterdayLine ? " L" : "M") + p.x + "," + p.y;
    } else {
      todayLine += (todayLine ? " L" : "M") + p.x + "," + p.y;
    }
  });

  sparkSegment(svg, yesterdayLine, pts.filter(p => p.fromYesterday), plotH, {
    gradId: "sparkYestGrad", color: "var(--muted)",
    strokeWidth: 1, strokeOpacity: "0.4", fillOpacity: "0.1",
  });

  sparkSegment(svg, todayLine, pts.filter(p => !p.fromYesterday), plotH, {
    gradId: "sparkGrad", color: "var(--accent-3)",
    strokeWidth: 1.5, strokeOpacity: "0.8", fillOpacity: "0.2",
  });

  // Current-time indicator (at 5-min bucket index)
  const curX = PAD_L + step * curBucket;

  // Top legend
  const legendY = 11;
  const legendItems = [
    { color: "var(--accent-3)", stroke: "var(--accent-3)", text: "Today" },
    { color: "var(--muted)", stroke: "var(--muted)", text: "Yesterday" },
  ];
  let lx = PAD_L;
  legendItems.forEach(item => {
    svg.appendChild(el("line", {
      x1: lx, y1: legendY, x2: lx + 14, y2: legendY,
      stroke: item.stroke, "stroke-width": "1.5", opacity: "0.8",
    }));
    lx += 18;
    svg.appendChild(el("text", {
      x: lx, y: legendY + 3, "font-size": "8", fill: "var(--muted)",
    }, item.text));
    lx += lx < W - PAD_R - 80 ? 30 : 0;
  });
  if (curX >= PAD_L && curX <= W - PAD_R) {
    svg.appendChild(el("line", {
      x1: curX, y1: 0, x2: curX, y2: H,
      stroke: "#fff", "stroke-width": "1", "stroke-dasharray": "2,3", opacity: "0.3",
    }));
  }

  // Tooltip hit rects - per 5-minute bucket
  for (let b = 0; b < 288; b++) {
    const p = pts[b];
    const hit = el("rect", {
      x: PAD_L + step * b, y: 0, width: step, height: H,
      fill: "transparent", cursor: "crosshair",
    });
    const title = el("title");
    const hrs = Math.floor(b / 12);
    const mins = (b % 12) * 5;
    const time = String(hrs).padStart(2, "0") + ":" + String(mins).padStart(2, "0");
    const dayLabel = p.fromYesterday ? "(yesterday) " : "";
    title.textContent = time + " " + dayLabel + "total: " + fmt(p.total) + " tokens";
    hit.appendChild(title);
    svg.appendChild(hit);
  }
}
