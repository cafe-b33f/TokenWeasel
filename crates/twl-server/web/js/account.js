import { setupLogout } from "/assets/js/nav.js";
import { esc } from "/assets/js/format.js";

/**
 * Shared fetch wrapper that handles RFC 9457 Problem Details JSON errors,
 * 204 No Content responses, and 401 redirects.
 *
 * On success returns the parsed JSON body (or null for 204).
 * On failure calls the optional status callback with the `detail` field.
 */
async function apiFetch(method, url, body, statusEl) {
  const opts = { method, headers: {} };
  if (body !== undefined && body !== null) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  try {
    const res = await fetch(url, opts);
    if (res.status === 401) { window.location.href = "/login"; throw new Error("Unauthorized"); }
    if (res.status === 204) return null;
    if (res.ok) return res.json();
    // Try to parse Problem Details JSON; fall back to status text.
    let detail = res.statusText;
    try {
      const problem = await res.json();
      if (problem.detail) detail = problem.detail;
    } catch {}
    if (statusEl) statusEl.textContent = detail;
    throw new Error(detail);
  } catch (e) {
    if (e.name !== "Error" && statusEl) {
      statusEl.textContent = "Network error";
    }
    throw e;
  }
}

// DOM refs
const displayUsername = document.getElementById("displayUsername");
const adminBadge = document.getElementById("adminBadge");
const adminSection = document.getElementById("adminSection");
const statusMessage = document.getElementById("statusMessage");

const apiKeyBody = document.getElementById("apiKeyBody");
const generateKeyForm = document.getElementById("generateKeyForm");
const keyName = document.getElementById("keyName");
const keyStatus = document.getElementById("keyStatus");
const oneTimeKeyDisplay = document.getElementById("oneTimeKeyDisplay");
const oneTimeKeyValue = document.getElementById("oneTimeKeyValue");
const copyKeyBtn = document.getElementById("copyKeyBtn");

const changePasswordForm = document.getElementById("changePasswordForm");
const oldPassword = document.getElementById("oldPassword");
const newPassword = document.getElementById("newPassword");
const confirmPassword = document.getElementById("confirmPassword");
const passwordStatus = document.getElementById("passwordStatus");

const userBody = document.getElementById("userBody");
const createUserForm = document.getElementById("createUserForm");
const newUsername = document.getElementById("newUsername");
const newUserPassword = document.getElementById("newUserPassword");
const newUserAdmin = document.getElementById("newUserAdmin");
const createUserStatus = document.getElementById("createUserStatus");

let currentUser = null;



async function init() {
  try {
    currentUser = await apiFetch("GET", "/api/me");
  } catch {
    statusMessage.textContent = "Failed to load user profile";
    statusMessage.style.display = "block";
    return;
  }

  displayUsername.textContent = currentUser.username;
  if (currentUser.admin) {
    adminBadge.style.display = "inline";
    adminSection.style.display = "";
    loadUsers();
  }

  loadApiKeys();
  setupLogout();
}



async function loadApiKeys() {
  let data;
  try {
    data = await apiFetch("GET", "/api/keys");
  } catch {
    apiKeyBody.innerHTML =
      '<tr><td colspan="4" class="empty">Failed to load API keys</td></tr>';
    return;
  }

  const keys = data.keys || [];
  if (keys.length === 0) {
    apiKeyBody.innerHTML =
      '<tr><td colspan="4" class="empty">No API keys yet</td></tr>';
    return;
  }

  apiKeyBody.innerHTML = keys
    .map(
      (k) =>
        `<tr>
          <td>${esc(k.name || "")}</td>
          <td><code>${esc(k.prefix)}</code></td>
          <td>${fmtTs(k.created_ts)}</td>
          <td><button class="btn btn-small btn-delete" data-key-id="${k.id}" data-key-prefix="${esc(k.prefix)}">Delete</button></td>
        </tr>`
    )
    .join("");

  apiKeyBody.querySelectorAll(".btn-delete").forEach((btn) => {
    btn.addEventListener("click", () => deleteApiKey(+btn.dataset.keyId, btn.dataset.keyPrefix));
  });
}

async function generateApiKey(e) {
  e.preventDefault();
  keyStatus.textContent = "";
  const name = keyName.value.trim() || undefined;

  let data;
  try {
    data = await apiFetch("POST", "/api/keys", name ? { name } : {}, keyStatus);
  } catch {
    return;
  }

  // Show the key once.
  oneTimeKeyValue.textContent = data.key;
  oneTimeKeyDisplay.style.display = "";
  keyName.value = "";
  loadApiKeys();
}

async function deleteApiKey(id, prefix) {
  if (!confirm(`Delete API key "${prefix}"?`)) return;
  try {
    await apiFetch("DELETE", `/api/keys/${id}`, undefined, keyStatus);
  } catch {
    return;
  }
  keyStatus.textContent = "Key deleted";
  loadApiKeys();
}



async function handleChangePassword(e) {
  e.preventDefault();
  passwordStatus.textContent = "";

  if (Array.from(newPassword.value).length < 8) {
    passwordStatus.textContent = "Password must be at least 8 characters";
    return;
  }

  if (newPassword.value !== confirmPassword.value) {
    passwordStatus.textContent = "Passwords do not match";
    return;
  }

  try {
    await apiFetch(
      "PUT",
      "/api/me/password",
      { old_password: oldPassword.value, new_password: newPassword.value },
      passwordStatus
    );
  } catch {
    return;
  }

  passwordStatus.textContent =
    "Password changed. The browser will ask you to log in again.";
  changePasswordForm.reset();
  setTimeout(() => location.reload(), 2000);
}



async function loadUsers() {
  let data;
  try {
    data = await apiFetch("GET", "/api/users");
  } catch {
    userBody.innerHTML =
      '<tr><td colspan="4" class="empty">Failed to load users</td></tr>';
    return;
  }

  const users = data.users || [];
  if (users.length === 0) {
    userBody.innerHTML =
      '<tr><td colspan="4" class="empty">No users</td></tr>';
    return;
  }

  const isSelf = (u) => u.id === currentUser.id;

  userBody.innerHTML = users
    .map(
      (u) =>
        `<tr>
          <td>${esc(u.username)} ${isSelf(u) ? '<span class="badge">you</span>' : ""}</td>
          <td>${u.is_admin ? "admin" : "user"}</td>
          <td>${fmtTs(u.created_ts)}</td>
          <td>
            ${isSelf(u) ? "" : `<button class="btn btn-small btn-reset-pw" data-user-id="${u.id}" data-username="${esc(u.username)}">Reset password</button>
            <button class="btn btn-small btn-delete" data-user-id="${u.id}" data-username="${esc(u.username)}">Delete</button>`}
          </td>
        </tr>`
    )
    .join("");

  userBody.querySelectorAll(".btn-reset-pw").forEach((btn) => {
    btn.addEventListener("click", () => resetUserPassword(+btn.dataset.userId, btn.dataset.username));
  });
  userBody.querySelectorAll(".btn-delete").forEach((btn) => {
    btn.addEventListener("click", () => deleteUser(+btn.dataset.userId, btn.dataset.username));
  });
}

async function createUser(e) {
  e.preventDefault();
  createUserStatus.textContent = "";

  const username = newUsername.value.trim();
  const password = newUserPassword.value;
  const admin = newUserAdmin.checked;

  if (Array.from(password).length < 8) {
    createUserStatus.textContent = "Password must be at least 8 characters";
    return;
  }

  try {
    await apiFetch(
      "POST",
      "/api/users",
      { username, password, admin },
      createUserStatus
    );
  } catch {
    return;
  }

  newUsername.value = "";
  newUserPassword.value = "";
  newUserAdmin.checked = false;
  createUserStatus.textContent = "User created";
  loadUsers();
}

async function resetUserPassword(id, username) {
  const newPw = prompt(`New password for "${username}":`);
  if (!newPw) return;
  if (Array.from(newPw).length < 8) {
    const el = createUserStatus;
    el.textContent = "Password must be at least 8 characters";
    return;
  }
  try {
    await apiFetch("PUT", `/api/users/${id}/password`, { password: newPw });
  } catch (e) {
    // Show error in a visible spot; admin section status is suitable.
    const msg = e.message || "Failed to reset password";
    createUserStatus.textContent = msg;
    return;
  }
  createUserStatus.textContent = `Password reset for "${username}"`;
}

async function deleteUser(id, username) {
  if (!confirm(`Delete user "${username}"?`)) return;
  try {
    await apiFetch("DELETE", `/api/users/${id}`, undefined, createUserStatus);
  } catch {
    return;
  }
  createUserStatus.textContent = `User "${username}" deleted`;
  loadUsers();
}



function fmtTs(epochSecs) {
  return new Date(epochSecs * 1000).toLocaleString();
}



generateKeyForm.addEventListener("submit", generateApiKey);
copyKeyBtn.addEventListener("click", () => {
  navigator.clipboard.writeText(oneTimeKeyValue.textContent).catch(() => {});
  copyKeyBtn.textContent = "Copied!";
  setTimeout(() => (copyKeyBtn.textContent = "Copy"), 2000);
});
changePasswordForm.addEventListener("submit", handleChangePassword);
createUserForm.addEventListener("submit", createUser);

init();
