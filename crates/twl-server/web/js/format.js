const esc = s => String(s).replace(/[&<>"']/g, c => ({
  "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;"
}[c]));

const fmt = n => n.toLocaleString();

const fmtK = n =>
  n >= 1e9 ? (n / 1e9).toFixed(2) + "B" :
  n >= 1e6 ? (n / 1e6).toFixed(2) + "M" :
  n >= 1e3 ? (n / 1e3).toFixed(1) + "K" :
  String(n);

const fmtCost = (n, currency) => {
  if (n == null) return "-";
  return (currency === "USD" ? "$" : "")
    + n.toLocaleString(undefined, {
      minimumFractionDigits: 2,
      maximumFractionDigits: n < 1 ? 4 : 2
    })
    + (currency && currency !== "USD" ? " " + currency : "");
};

const fmtKwh = (n, opts) => {
  if (n == null) return opts?.skipUnit ? "-" : "- kWh";
  if (n >= 0.01) return (opts?.skipUnit ? n.toFixed(2) : n.toFixed(2) + " kWh");
  if (n > 0) return (opts?.skipUnit ? n.toFixed(4) : n.toFixed(4) + " kWh");
  return (opts?.skipUnit ? "0" : "0.00 kWh");
};

export { esc, fmt, fmtK, fmtCost, fmtKwh };
