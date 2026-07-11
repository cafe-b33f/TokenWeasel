//! Pure authentication primitives shared across crates.
//!
//! Provides password hashing (Argon2id), verification, API key generation,
//! and client API-key extraction / validation.

pub mod accounts;
pub mod apikey;

use argon2::password_hash::{
    Error as PasswordHashError, PasswordHash, PasswordHasher, PasswordVerifier, Salt, SaltString,
};
use argon2::Argon2;
use rand::distr::{Alphanumeric, SampleString};
use rand::Rng;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Error returned by password hashing or verification.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct PasswordError(String);

/// Minimum number of Unicode characters required for account passwords.
pub const MIN_PASSWORD_CHARACTERS: usize = 8;

/// Return whether a password satisfies the shared minimum-length policy.
///
/// Length is measured in Unicode scalar values rather than UTF-8 bytes.
pub fn password_has_minimum_length(password: &str) -> bool {
    password.chars().count() >= MIN_PASSWORD_CHARACTERS
}

/// Parse credentials from an HTTP `Authorization: Bearer` field value.
///
/// The scheme is case-insensitive and must be followed by one or more spaces.
/// Optional trailing HTTP whitespace is ignored; whitespace inside the
/// credential is rejected.
pub fn parse_bearer_credentials(value: &str) -> Option<&str> {
    let value = value.trim_end_matches([' ', '\t']);
    let (scheme, credentials) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let credentials = credentials.trim_start_matches(' ');
    if credentials.is_empty() || credentials.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    Some(credentials)
}

/// Compute the lowercase hexadecimal SHA-256 digest of a string slice.
pub fn sha256_hex(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

/// A database-backed user returned on successful authentication.
#[derive(Clone, Debug)]
pub struct AuthUser {
    /// Unique row identifier from the identity store.
    pub id: i64,
    /// Login name.
    pub username: String,
    /// Whether this user has administrative privileges.
    pub is_admin: bool,
}

/// Hash a plaintext password using Argon2id with default parameters and a
/// random salt generated from the operating system's random source.
///
/// Returns the encoded password hash string on success, or a [`PasswordError`]
/// on failure.
pub fn hash_password(password: &str) -> Result<String, PasswordError> {
    let mut salt_bytes = [0u8; Salt::RECOMMENDED_LENGTH];
    rand::rng().fill_bytes(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| PasswordError(format!("salt encoding: {e}")))?;
    let hasher = Argon2::default();
    hasher
        .hash_password(password.as_bytes(), &salt)
        .map(|ph| ph.to_string())
        .map_err(|e| PasswordError(format!("argon2 hash: {e}")))
}

/// Verify a plaintext password against a stored Argon2 hash.
///
/// Returns `Ok(true)` when the password matches, `Ok(false)` when it does
/// not, or a [`PasswordError`] if the stored hash is malformed or
/// verification fails.
pub fn verify_password(password: &str, stored_hash: &str) -> Result<bool, PasswordError> {
    let hasher = Argon2::default();
    let parsed = PasswordHash::new(stored_hash)
        .map_err(|e| PasswordError(format!("malformed stored hash: {e}")))?;
    match hasher.verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(PasswordHashError::Password) => Ok(false),
        Err(error) => Err(PasswordError(format!(
            "stored hash verification failed: {error}"
        ))),
    }
}

fn random_alphanumeric(len: usize) -> String {
    Alphanumeric.sample_string(&mut rand::rng(), len)
}

/// Generate a random alphanumeric password of 24 characters.
///
/// Uses a cryptographically secure random source and an unbiased
/// selection over a 62-character alphabet (a-z, A-Z, 0-9).
/// Suitable as a generated initial admin password.
pub fn generate_random_password() -> String {
    random_alphanumeric(24)
}

/// Generate a new API key.
///
/// Returns a tuple of (raw_key, sha256_hex_hash, display_prefix).
/// - raw_key: literal `"twl_"` prefix followed by 32 random alphanumeric chars.
/// - hash: lowercase hex sha256 of the full raw key.
/// - prefix: first 10 characters of the raw key.
pub fn generate_api_key() -> (String, String, String) {
    let mut key = String::with_capacity(37);
    key.push_str("twl_");
    key.push_str(&random_alphanumeric(32));
    let hash = sha256_hex(&key);
    let prefix = key.chars().take(10).collect();
    (key, hash, prefix)
}

#[cfg(test)]
mod tests {
    use super::{
        hash_password, parse_bearer_credentials, password_has_minimum_length, verify_password,
    };

    #[test]
    fn password_length_counts_unicode_characters_not_utf8_bytes() {
        // This is eight UTF-8 bytes but only two characters, and was
        // previously accepted by checks based on `str::len()`.
        assert!(!password_has_minimum_length("🔒🔒"));

        // Eight non-ASCII characters satisfy the documented policy even
        // though their UTF-8 representation is longer than eight bytes.
        assert!(password_has_minimum_length("éééééééé"));
    }

    #[test]
    fn verifier_distinguishes_mismatch_from_hash_failures() {
        let hash = hash_password("correct password").unwrap();
        assert!(!verify_password("wrong password", &hash).unwrap());

        let unsupported = hash.replacen("$argon2id$", "$scrypt$", 1);
        assert!(verify_password("correct password", &unsupported).is_err());

        let invalid_params = hash.replacen("m=19456", "m=0", 1);
        assert!(verify_password("correct password", &invalid_params).is_err());
    }

    #[test]
    fn bearer_parser_handles_http_spacing_and_rejects_malformed_values() {
        assert_eq!(parse_bearer_credentials("Bearer   token"), Some("token"));
        assert_eq!(parse_bearer_credentials("bearer token \t"), Some("token"));
        assert_eq!(parse_bearer_credentials("Bearer\ttoken"), None);
        assert_eq!(parse_bearer_credentials("Bearer"), None);
        assert_eq!(parse_bearer_credentials("Bearer   "), None);
        assert_eq!(parse_bearer_credentials("Basic token"), None);
        assert_eq!(parse_bearer_credentials("Bearer token extra"), None);
    }
}
