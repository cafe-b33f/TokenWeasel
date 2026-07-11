import { fetchDashboard, fetchBuckets, fetchUsers } from "/assets/js/api.js";
import { setupLogout } from "/assets/js/nav.js";
import { renderCards } from "/assets/js/cards.js";
import { renderKeyTable, renderTable } from "/assets/js/table.js";
import { renderChart } from "/assets/js/chart.js";
import { renderSparkline } from "/assets/js/sparkline.js";

let loading = false;
let currentDays = 7;
let currentModel = "";
let currentUserId = "";
let dashboardData = null;
let totalKwh = 0;
let todayData = null;
let yesterdayData = null;
let lastRenderSig = null;
let lastModelsSig = null;
let lastViewsSig = null;
let adminUsers = [];
let usersLoaded = false;

async function load() {
  if (loading) return;
  loading = true;
  try {
    const res = await fetchDashboard(currentDays, currentUserId);
    dashboardData = res;
    totalKwh = res.total_kwh || 0;
    if (res.viewer_is_admin && !usersLoaded) {
      adminUsers = await fetchUsers();
      if (adminUsers.length > 0) usersLoaded = true;
    }
    const modelNames = (res.models || []).map(m => m.model);
    if (currentModel && !modelNames.includes(currentModel)) currentModel = "";
    populateModels(res.models || []);
    populateViews(res);
    const todayUrl = "/api/today" + (currentUserId ? "?user_id=" + currentUserId : "");
    const yesterdayUrl = "/api/yesterday" + (currentUserId ? "?user_id=" + currentUserId : "");
    [todayData, yesterdayData] = await Promise.all([fetchBuckets(todayUrl), fetchBuckets(yesterdayUrl)]);
    const sig = JSON.stringify([res, todayData, yesterdayData]);
    if (sig !== lastRenderSig) {
      lastRenderSig = sig;
      render();
    }
    document.getElementById("refreshStatus").textContent = "Updated " + new Date().toLocaleTimeString();
  } catch (e) {
    document.getElementById("refreshStatus").textContent = "Refresh failed";
  } finally {
    loading = false;
  }
}

function render() {
  const dashboardCfg = (dashboardData && dashboardData.dashboard) || {};
  let cards = Array.isArray(dashboardCfg.cards) ? dashboardCfg.cards : undefined;
  const graphs = dashboardCfg.graphs;

  const scope = dashboardData && dashboardData.scope;
  if (scope && scope.mode === "user") {
    const order = cards || ["cost", "energy", "tokens", "requests", "io", "cache_hit"];
    cards = order.filter(c => c !== "energy");
  }
  const maxRows = dashboardCfg.max_rows;

  renderCards(dashboardData, { model: currentModel, days: currentDays, totalKwh, cards });

  const graphsIsArray = Array.isArray(graphs);
  const todayPanel = document.getElementById("todayPanel");
  const overTimePanel = document.getElementById("overTimePanel");

  if (graphsIsArray && !graphs.includes("today")) {
    todayPanel.style.display = "none";
  } else {
    todayPanel.style.display = "";
    renderSparkline(todayData, yesterdayData);
  }

  if (graphsIsArray && !graphs.includes("over_time")) {
    overTimePanel.style.display = "none";
  } else {
    overTimePanel.style.display = "";
    renderChart(dashboardData, { model: currentModel, days: currentDays });
  }

  renderTable(dashboardData, currentModel, maxRows);
  renderKeyTable(dashboardData, maxRows);
}

function populateModels(models) {
  const sig = models.map(m => m.model).sort().join("|");
  if (sig === lastModelsSig) return;
  lastModelsSig = sig;
  const sel = document.getElementById("modelSelect");
  const names = models.map(m => m.model);
  // Rebuild via DOM APIs (value/textContent) so model names are never
  // interpreted as HTML - injection-safe regardless of upstream content.
  sel.innerHTML = '<option value="">All models</option>';
  names.forEach(n => {
    const o = document.createElement("option");
    o.value = n;
    o.textContent = n;
    sel.appendChild(o);
  });
  sel.value = currentModel;
}

function populateViews(res) {
  const sig = JSON.stringify([res.viewer_is_admin, adminUsers.map(u => [u.id, u.username])]);
  if (sig === lastViewsSig) return;
  lastViewsSig = sig;
  const ctrl = document.getElementById("viewControl");
  if (res.viewer_is_admin) {
    ctrl.style.display = "";
    const sel = document.getElementById("viewSelect");
    sel.innerHTML = '<option value="">All usage (admin)</option>';
    adminUsers.forEach(u => {
      const o = document.createElement("option");
      o.value = u.id;
      o.textContent = u.username;
      sel.appendChild(o);
    });
    sel.value = currentUserId;
  } else {
    ctrl.style.display = "none";
  }
}

document.querySelectorAll("button.range").forEach(b => {
  // Highlight the button matching the default/current day range.
  b.classList.toggle("active", +b.dataset.days === currentDays);
  b.addEventListener("click", () => {
    document.querySelectorAll("button.range").forEach(x => x.classList.remove("active"));
    b.classList.add("active");
    currentDays = +b.dataset.days;
    lastRenderSig = null;
    load();
  });
});

document.getElementById("modelSelect").addEventListener("change", e => {
  currentModel = e.target.value;
  render();
});

document.getElementById("viewSelect").addEventListener("change", e => {
  currentUserId = e.target.value;
  lastRenderSig = null;
  load();
});

document.getElementById("refresh").addEventListener("click", e => {
  e.preventDefault();
  load();
});

const REFRESH_MS = 5000; // 5 seconds
let refreshInterval = setInterval(load, REFRESH_MS);

// Pause polling while tab is hidden so we don't hammer the API
window.addEventListener("visibilitychange", () => {
  if (document.hidden) {
    clearInterval(refreshInterval);
  } else {
    load();
    refreshInterval = setInterval(load, REFRESH_MS);
  }
});

window.addEventListener("resize", () => {
  if (document.getElementById("overTimePanel").style.display !== "none") renderChart(dashboardData, { model: currentModel, days: currentDays });
  if (document.getElementById("todayPanel").style.display !== "none") renderSparkline(todayData, yesterdayData);
});

load();
setupLogout();
