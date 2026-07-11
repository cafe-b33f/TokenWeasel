//! SQLite-backed store for user identities and API keys.
//!
//! Uses two connections: a read connection (shared via `Mutex`) and a write
//! connection (shared via `Mutex`), both opened with the standard WAL
//! configuration.

use std::sync::Mutex;

use rusqlite::Connection;
use rusqlite::OptionalExtension;
use serde::Serialize;

use crate::db::{now_unix_secs, Database, DbError};

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// One row of the `users` table.
#[derive(Debug, Clone)]
pub struct UserRow {
    /// Stable database identifier for the user.
    pub id: i64,
    /// Unique login name as stored in SQLite.
    pub username: String,
    /// Password-verifier hash; callers must never expose it through API responses.
    pub password_hash: String,
    /// Whether the user is authorized for administrative operations.
    pub is_admin: bool,
    /// Creation time as Unix seconds.
    pub created_ts: i64,
}

/// One row of the `api_keys` table.
#[derive(Debug, Clone)]
pub struct ApiKeyRow {
    /// Stable database identifier for the key.
    pub id: i64,
    /// Optional human-readable label supplied by the owner.
    pub name: Option<String>,
    /// First few characters of the raw key, for display.
    pub prefix: String,
    /// Creation time as Unix seconds.
    pub created_ts: i64,
}

/// [`UserRow`] without the password hash, for listings.
#[derive(Debug, Serialize)]
pub struct UserListRow {
    /// Stable database identifier for the user.
    pub id: i64,
    /// Unique login name as stored in SQLite.
    pub username: String,
    /// Whether the user is authorized for administrative operations.
    pub is_admin: bool,
    /// Creation time as Unix seconds.
    pub created_ts: i64,
}

/// Result of a guarded user-deletion attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteUserOutcome {
    /// The requested user existed and was deleted.
    Deleted,
    /// No user matched the requested identifier.
    NotFound,
    /// The user is the last remaining administrator; deletion refused.
    LastAdmin,
}

// ---------------------------------------------------------------------------
// IdentityStore
// ---------------------------------------------------------------------------

/// SQLite-backed store for user identities and API keys.
#[derive(Debug)]
pub struct IdentityStore {
    read: Mutex<Connection>,
    write: Mutex<Connection>,
}

impl IdentityStore {
    /// Opens independent read and write connections using the database's standard configuration.
    pub fn new(db: &Database) -> Result<Self, DbError> {
        let read = db.connect()?;
        let write = db.connect()?;
        Ok(IdentityStore {
            read: Mutex::new(read),
            write: Mutex::new(write),
        })
    }

    /// Returns the total number of user records.
    pub fn count_users(&self) -> Result<i64, DbError> {
        let conn = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM users")?;
        Ok(stmt.query_row([], |r| r.get(0))?)
    }

    /// Looks up a user by exact username, including the verifier needed for authentication.
    pub fn get_user(&self, username: &str) -> Result<Option<UserRow>, DbError> {
        let conn = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, username, password_hash, is_admin, created_ts FROM users WHERE username = ?1",
        )?;
        Ok(stmt
            .query_row([username], |r| {
                Ok(UserRow {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    password_hash: r.get(2)?,
                    is_admin: r.get::<_, i64>(3)? != 0,
                    created_ts: r.get(4)?,
                })
            })
            .optional()?)
    }

    /// Creates a user with an already-derived password hash and returns its row id.
    ///
    /// Hash derivation and username validation are caller responsibilities; database
    /// uniqueness violations are returned as [`DbError`].
    pub fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        is_admin: bool,
    ) -> Result<i64, DbError> {
        let ts = now_unix_secs().ok_or(DbError::Clock)?;
        let conn = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "INSERT INTO users (username, password_hash, is_admin, created_ts)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        stmt.execute(rusqlite::params![
            username,
            password_hash,
            if is_admin { 1 } else { 0 },
            ts
        ])?;
        Ok(conn.last_insert_rowid())
    }

    /// Insert a new API key and return the new row id.
    pub fn insert_api_key(
        &self,
        user_id: i64,
        name: Option<String>,
        key_hash: &str,
        prefix: &str,
    ) -> Result<i64, DbError> {
        let ts = now_unix_secs().ok_or(DbError::Clock)?;
        let mut conn = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO api_keys (user_id, name, key_hash, prefix, created_ts)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![user_id, name, key_hash, prefix, ts],
        )?;
        let key_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO api_key_attribution (api_key_id, user_id, name, prefix)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![key_id, user_id, name, prefix],
        )?;
        tx.commit()?;
        Ok(key_id)
    }

    /// List all API keys owned by `user_id`, ordered by creation time descending.
    pub fn list_api_keys(&self, user_id: i64) -> Result<Vec<ApiKeyRow>, DbError> {
        let conn = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, name, prefix, created_ts
             FROM api_keys
             WHERE user_id = ?1
             ORDER BY created_ts DESC",
        )?;
        let rows = stmt.query_map([user_id], |r| {
            Ok(ApiKeyRow {
                id: r.get(0)?,
                name: r.get(1)?,
                prefix: r.get(2)?,
                created_ts: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// List all users, ordered by creation time ascending. Never returns password hashes.
    pub fn list_users(&self) -> Result<Vec<UserListRow>, DbError> {
        let conn = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, username, is_admin, created_ts
             FROM users
             ORDER BY created_ts ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(UserListRow {
                id: r.get(0)?,
                username: r.get(1)?,
                is_admin: r.get::<_, i64>(2)? != 0,
                created_ts: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Look up a user by numeric id. Returns `None` if not found.
    pub fn get_user_by_id(&self, id: i64) -> Result<Option<UserRow>, DbError> {
        let conn = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, username, password_hash, is_admin, created_ts FROM users WHERE id = ?1",
        )?;
        Ok(stmt
            .query_row([id], |r| {
                Ok(UserRow {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    password_hash: r.get(2)?,
                    is_admin: r.get::<_, i64>(3)? != 0,
                    created_ts: r.get(4)?,
                })
            })
            .optional()?)
    }

    /// Update a user's password hash. Returns the number of rows affected.
    pub fn update_password(&self, id: i64, password_hash: &str) -> Result<usize, DbError> {
        let conn = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare("UPDATE users SET password_hash = ?1 WHERE id = ?2")?;
        Ok(stmt.execute(rusqlite::params![password_hash, id])?)
    }

    /// Atomically replace a password hash and revoke all of the user's API keys.
    ///
    /// Returns the number of password rows updated. If key revocation fails,
    /// the password change is rolled back with the transaction.
    pub fn replace_password_and_revoke_keys(
        &self,
        id: i64,
        password_hash: &str,
    ) -> Result<usize, DbError> {
        let mut conn = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE users SET password_hash = ?1 WHERE id = ?2",
            rusqlite::params![password_hash, id],
        )?;
        tx.execute("DELETE FROM api_keys WHERE user_id = ?1", [id])?;
        tx.commit()?;
        Ok(updated)
    }

    /// Atomically replace a password only if its current hash matches, then
    /// revoke all API keys for the user.
    ///
    /// A zero return means the user disappeared or the password was changed
    /// concurrently. In that case no API keys are revoked.
    pub fn compare_and_swap_password_and_revoke_keys(
        &self,
        id: i64,
        expected_password_hash: &str,
        password_hash: &str,
    ) -> Result<usize, DbError> {
        let mut conn = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE users SET password_hash = ?1 WHERE id = ?2 AND password_hash = ?3",
            rusqlite::params![password_hash, id, expected_password_hash],
        )?;
        if updated == 1 {
            tx.execute("DELETE FROM api_keys WHERE user_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Atomically check admin count and delete a user (API keys,
    /// user row) inside a single transaction.
    ///
    /// Returns:
    /// - [`DeleteUserOutcome::Deleted`] on success.
    /// - [`DeleteUserOutcome::NotFound`] if the target id does not exist.
    /// - [`DeleteUserOutcome::LastAdmin`] if the target is the only remaining
    ///   admin (deletion prevented).
    pub fn delete_user_guarded(&self, id: i64) -> Result<DeleteUserOutcome, DbError> {
        let mut conn = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;

        // Check whether the user exists and is an admin.
        let is_admin: Option<bool> = tx
            .prepare("SELECT is_admin FROM users WHERE id = ?1")?
            .query_row([id], |r| r.get::<_, i64>(0).map(|v| v != 0))
            .optional()?;

        match is_admin {
            None => return Ok(DeleteUserOutcome::NotFound),
            Some(true) => {
                let admin_count: i64 = tx
                    .prepare("SELECT COUNT(*) FROM users WHERE is_admin = 1")?
                    .query_row([], |r| r.get(0))?;
                if admin_count <= 1 {
                    return Ok(DeleteUserOutcome::LastAdmin);
                }
            }
            Some(false) => {}
        }

        tx.execute("DELETE FROM api_keys WHERE user_id = ?1", [id])?;
        tx.execute("DELETE FROM users WHERE id = ?1", [id])?;

        tx.commit()?;
        Ok(DeleteUserOutcome::Deleted)
    }

    /// Delete a single API key, scoped to the owning user (both `id` and `user_id` must match).
    pub fn delete_api_key(&self, id: i64, user_id: i64) -> Result<usize, DbError> {
        let conn = self.write.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn.execute(
            "DELETE FROM api_keys WHERE id = ?1 AND user_id = ?2",
            rusqlite::params![id, user_id],
        )?)
    }

    /// Look up the user associated with an API key hash. Returns `None` if not found.
    pub fn get_api_key_user(&self, key_hash: &str) -> Result<Option<UserRow>, DbError> {
        let conn = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT u.id, u.username, u.password_hash, u.is_admin, u.created_ts
             FROM api_keys k
             JOIN users u ON u.id = k.user_id
             WHERE k.key_hash = ?1",
        )?;
        Ok(stmt
            .query_row([key_hash], |r| {
                Ok(UserRow {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    password_hash: r.get(2)?,
                    is_admin: r.get::<_, i64>(3)? != 0,
                    created_ts: r.get(4)?,
                })
            })
            .optional()?)
    }

    /// Look up `(api_key_id, user_id)` for a given key hash. Returns `None` if not found.
    pub fn get_api_key_id(&self, key_hash: &str) -> Result<Option<(i64, i64)>, DbError> {
        let conn = self.read.lock().unwrap_or_else(|e| e.into_inner());
        // Join users defensively as well as enforcing the foreign key. This
        // keeps orphan rows from older databases/imports from authenticating.
        let mut stmt = conn.prepare(
            "SELECT k.id, k.user_id
             FROM api_keys k
             JOIN users u ON u.id = k.user_id
             WHERE k.key_hash = ?1",
        )?;
        Ok(stmt
            .query_row([key_hash], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    /// Delete all API keys belonging to a given user.
    pub fn delete_api_keys_for_user(&self, user_id: i64) -> Result<usize, DbError> {
        let conn = self.write.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn.execute("DELETE FROM api_keys WHERE user_id = ?1", [user_id])?)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
