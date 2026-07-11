//! Account management business logic.
//!
//! Provides sync functions that validate inputs, enforce business rules, and
//! interact with the database. All functions are designed to be called from
//! within [`tokio::task::spawn_blocking`] by the server layer.
//!
//! # Error mapping
//!
//! Each function returns [`AccountError`], which the HTTP layer maps to the
//! appropriate Problem Details response.

use twl_store::{DbError, DeleteUserOutcome, IdentityStore};

use thiserror::Error;

use crate::{generate_api_key, hash_password, password_has_minimum_length, verify_password};

/// Errors returned by account-management operations.
///
/// Each variant maps to a specific HTTP status and message in the server
/// handler layer.
#[derive(Debug, Error)]
pub enum AccountError {
    /// Username failed validation (1-64 ASCII alphanumeric/dot/dash/underscore).
    #[error("{0}")]
    InvalidUsername(String),
    /// Password is too short (< 8 characters).
    #[error("{0}")]
    WeakPassword(String),
    /// Attempted to create a user with a username that already exists.
    #[error("username already exists")]
    UsernameTaken,
    /// The target user id does not exist.
    #[error("user not found")]
    NotFound,
    /// Attempted to delete the last remaining admin.
    #[error("the last admin cannot be deleted")]
    LastAdmin,
    /// Old password did not match when changing password.
    #[error("old password is wrong")]
    WrongPassword,
    /// The password changed after it was verified.
    #[error("password was changed concurrently")]
    ConcurrentPasswordChange,
    /// Internal database error.
    #[error("database error: {0}")]
    Db(#[from] DbError),
    /// Internal error (e.g. password hashing failure).
    #[error("{0}")]
    Internal(String),
}

/// Create a new user after validating username and password constraints.
///
/// # Validation
///
/// * `username` must be 1-64 characters of ASCII alphanumerics, dots, dashes,
///   or underscores.
/// * `password` must be at least 8 characters.
/// * The username must not already exist (UNIQUE constraint).
///
/// # Returns
///
/// The new user's row id on success.
pub fn create_user(
    identity: &IdentityStore,
    username: &str,
    password: &str,
    is_admin: bool,
) -> Result<i64, AccountError> {
    if username.is_empty()
        || username.len() > 64
        || !username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(AccountError::InvalidUsername(
            "username must be 1-64 characters of ASCII alphanumerics, dots, dashes or underscores"
                .into(),
        ));
    }

    if !password_has_minimum_length(password) {
        return Err(AccountError::WeakPassword(
            "password must be at least 8 characters".into(),
        ));
    }

    let hash = hash_password(password)
        .map_err(|e| AccountError::Internal(format!("password hashing failed: {e}")))?;

    match identity.create_user(username, &hash, is_admin) {
        Ok(id) => Ok(id),
        Err(DbError::UniqueViolation) => Err(AccountError::UsernameTaken),
        Err(e) => Err(AccountError::Db(e)),
    }
}

/// Create a new API key for the given user.
///
/// Generates a raw key via [`generate_api_key`], hashes it, stores the hash
/// in the database, and returns the row id together with the raw key in a
/// one-time-only tuple.
///
/// # Returns
///
/// `(id, raw_key_string)` - the row id and the full raw key. The raw key
/// should be displayed to the user exactly once.
pub fn create_api_key(
    identity: &IdentityStore,
    user_id: i64,
    name: Option<String>,
) -> Result<(i64, String), AccountError> {
    let (raw_key, key_hash, prefix) = generate_api_key();
    let id = identity.insert_api_key(user_id, name, &key_hash, &prefix)?;
    Ok((id, raw_key))
}

/// Delete a user by id, including their API keys.
///
/// Prevents deleting the last admin. Returns:
/// - [`AccountError::NotFound`] when the target user does not exist.
/// - [`AccountError::LastAdmin`] when the target is the only remaining admin.
pub fn delete_user(identity: &IdentityStore, target_id: i64) -> Result<(), AccountError> {
    match identity.delete_user_guarded(target_id)? {
        DeleteUserOutcome::Deleted => Ok(()),
        DeleteUserOutcome::NotFound => Err(AccountError::NotFound),
        DeleteUserOutcome::LastAdmin => Err(AccountError::LastAdmin),
    }
}

/// Reset another user's password (admin operation).
///
/// Validates the new password, checks the target exists, hashes the password,
/// and updates the database. All existing API keys for the user are deleted,
/// and changing the password hash invalidates any active session (forcing
/// re-authentication).
pub fn reset_password(
    identity: &IdentityStore,
    target_id: i64,
    new_password: &str,
) -> Result<(), AccountError> {
    if !password_has_minimum_length(new_password) {
        return Err(AccountError::WeakPassword(
            "password must be at least 8 characters".into(),
        ));
    }

    identity
        .get_user_by_id(target_id)?
        .ok_or(AccountError::NotFound)?;

    let hash = hash_password(new_password)
        .map_err(|e| AccountError::Internal(format!("password hashing failed: {e}")))?;

    match identity.replace_password_and_revoke_keys(target_id, &hash)? {
        1 => Ok(()),
        0 => Err(AccountError::NotFound),
        count => Err(AccountError::Internal(format!(
            "password update affected {count} users"
        ))),
    }
}

/// Change the authenticated user's own password.
///
/// Verifies `old_password` against the stored hash, then hashes
/// `new_password` and updates the database. All existing API keys for the
/// user are deleted, and changing the password hash invalidates any active
/// session (forcing re-authentication).
///
/// Returns:
/// - [`AccountError::NotFound`] when the user does not exist.
/// - [`AccountError::WrongPassword`] when `old_password` does not match.
/// - [`AccountError::WeakPassword`] when `new_password` is too short.
/// - [`AccountError::ConcurrentPasswordChange`] when the stored password was
///   replaced after verification, before this change could be committed.
pub fn change_password(
    identity: &IdentityStore,
    user_id: i64,
    old_password: &str,
    new_password: &str,
) -> Result<(), AccountError> {
    if !password_has_minimum_length(new_password) {
        return Err(AccountError::WeakPassword(
            "new password must be at least 8 characters".into(),
        ));
    }

    let row = identity
        .get_user_by_id(user_id)?
        .ok_or_else(|| AccountError::Internal("authenticated user not found in database".into()))?;

    if !verify_password(old_password, &row.password_hash)
        .map_err(|e| AccountError::Internal(format!("password verification failed: {e}")))?
    {
        return Err(AccountError::WrongPassword);
    }

    let hash = hash_password(new_password)
        .map_err(|e| AccountError::Internal(format!("password hashing failed: {e}")))?;

    match identity.compare_and_swap_password_and_revoke_keys(user_id, &row.password_hash, &hash)? {
        1 => Ok(()),
        0 => Err(AccountError::ConcurrentPasswordChange),
        count => Err(AccountError::Internal(format!(
            "password update affected {count} users"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use twl_store::{Database, IdentityStore};

    use super::{create_user, AccountError};

    fn test_store() -> (Database, IdentityStore, std::path::PathBuf) {
        static DB_SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "twl-auth-password-{}-{seq}-{nonce}.db",
            std::process::id()
        ));
        let db = Database::open(path.to_str().expect("UTF-8 path")).expect("open database");
        let identity = IdentityStore::new(&db).expect("create identity store");
        (db, identity, path)
    }

    #[test]
    fn create_user_enforces_unicode_character_password_length() {
        let (_db, identity, path) = test_store();

        let result = create_user(&identity, "short", "🔒🔒", false);
        assert!(matches!(result, Err(AccountError::WeakPassword(_))));

        create_user(&identity, "valid", "éééééééé", false)
            .expect("eight Unicode characters should be accepted");

        drop(identity);
        let _ = std::fs::remove_file(path);
    }
}
