/**
 * Shared nav logout handler. Import on pages that have .nav and #signoutBtn.
 */
export function setupLogout(btnSelector) {
  const btn = document.querySelector(btnSelector || '#signoutBtn');
  if (!btn) return;
  btn.addEventListener('click', async e => {
    e.preventDefault();
    try {
      await fetch('/api/logout', { method: 'POST' });
    } catch { /* ignore */ }
    window.location.href = '/login';
  });
}
