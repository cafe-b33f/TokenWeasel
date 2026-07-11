async function fetchDashboard(days, userId) {
  let url = "/api/dashboard?days=" + days;
  if (userId) url += "&user_id=" + userId;
  const res = await fetch(url);
  if (res.status === 401) { window.location.href = "/login"; return; }
  return res.json();
}

async function fetchUsers() {
  try {
    const res = await fetch("/api/users");
    if (res.status === 401) { window.location.href = "/login"; return []; }
    if (res.status !== 200) return [];
    const data = await res.json();
    return data.users || [];
  } catch {
    return [];
  }
}

async function fetchBuckets(url) {
  try {
    const res = await fetch(url);
    if (res.status === 401) { window.location.href = "/login"; return []; }
    const data = await res.json();
    return data.buckets || [];
  } catch {
    return [];
  }
}

export { fetchDashboard, fetchBuckets, fetchUsers };
