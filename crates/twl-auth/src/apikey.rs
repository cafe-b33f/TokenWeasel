//! Client API-key extraction and validation for the proxy path.
//! Extracts `twl_` keys from `Authorization: Bearer` or `x-api-key` headers,
//! hashes them, and looks them up in the database. Non-`twl_` header values
//! pass through untouched in optional mode.

use std::sync::Arc;

use http::HeaderMap;

use twl_store::IdentityStore;

/// A validated API key with its database identifiers.
#[derive(Debug, Clone)]
pub struct ValidatedKey {
    /// Row id from the `api_keys` table.
    pub key_id: i64,
    /// Owning user id. Fair scheduling is per user, not per API key.
    pub user_id: i64,
}

/// Outcome of attempting to extract and validate a client API key.
#[derive(Debug)]
pub enum ClientKeyResult {
    /// No `twl_`-prefixed key was found in any of the recognised headers.
    NotPresent,
    /// An `twl_`-prefixed key was found but doesn't match any key in the
    /// database.
    Invalid,
    /// A transient error occurred during DB lookup or join (e.g. rusqlite
    /// failure, spawn_blocking join error). The caller should surface this as
    /// a 5xx response, not 401.
    Error(String),
    /// An `twl_`-prefixed key was found and validated.
    Valid(ValidatedKey),
}

/// Extract a raw `twl_` key string from request headers.
///
/// Checks `Authorization: Bearer twl_<key>` (case-insensitive scheme match),
/// then `x-api-key: twl_<key>`. Returns the raw key (with `twl_` prefix)
/// or `None` if no `twl_` key is found. Non-`twl_` values pass through.
pub fn extract_client_key(headers: &HeaderMap) -> Option<String> {
    if let Some(auth_val) = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) =
            crate::parse_bearer_credentials(auth_val).filter(|token| token.starts_with("twl_"))
        {
            return Some(token.to_string());
        }
    }

    if let Some(key_val) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if let Some(token) = key_val.strip_prefix("twl_") {
            return Some(format!("twl_{token}"));
        }
    }

    None
}

/// Extract a client API key from headers, hash it, and look it up in the
/// identity store. Runs the DB lookup via [`tokio::task::spawn_blocking`] to
/// avoid blocking the async runtime on the SQLite mutex.
pub async fn extract_and_validate_client_key(
    headers: &HeaderMap,
    identity: &Arc<IdentityStore>,
) -> ClientKeyResult {
    let raw_key = match extract_client_key(headers) {
        Some(k) => k,
        None => return ClientKeyResult::NotPresent,
    };

    let hash = crate::sha256_hex(&raw_key);

    let identity_clone = identity.clone();
    let result = tokio::task::spawn_blocking(move || identity_clone.get_api_key_id(&hash)).await;

    match result {
        Ok(Ok(Some((key_id, user_id)))) => ClientKeyResult::Valid(ValidatedKey { key_id, user_id }),
        Ok(Ok(None)) => ClientKeyResult::Invalid,
        Ok(Err(e)) => ClientKeyResult::Error(e.to_string()),
        Err(join) => ClientKeyResult::Error(join.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderName;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(k.parse::<HeaderName>().unwrap(), v.parse().unwrap());
        }
        map
    }

    #[test]
    fn extract_bearer_twl_key() {
        let headers = hm(&[(
            "authorization",
            "Bearer twl_abcdef12345678901234567890123456789012",
        )]);
        assert_eq!(
            extract_client_key(&headers),
            Some("twl_abcdef12345678901234567890123456789012".to_string())
        );
    }

    #[test]
    fn extract_lowercase_bearer_twl_key() {
        let headers = hm(&[(
            "authorization",
            "bearer twl_abcdef12345678901234567890123456789012",
        )]);
        assert_eq!(
            extract_client_key(&headers),
            Some("twl_abcdef12345678901234567890123456789012".to_string())
        );
    }

    #[test]
    fn extract_bearer_key_with_multiple_spaces_and_trailing_whitespace() {
        let headers = hm(&[("authorization", "Bearer   twl_multispace \t")]);
        assert_eq!(
            extract_client_key(&headers).as_deref(),
            Some("twl_multispace")
        );
    }

    #[test]
    fn extract_x_api_key_twl() {
        let headers = hm(&[("x-api-key", "twl_abcdef12345678901234567890123456789012")]);
        assert_eq!(
            extract_client_key(&headers),
            Some("twl_abcdef12345678901234567890123456789012".to_string())
        );
    }

    #[test]
    fn authorization_takes_precedence_over_x_api_key() {
        let headers = hm(&[
            (
                "authorization",
                "Bearer twl_aaaabbbbccccddddeeeeffff0000111122223333444455556666777788889999",
            ),
            (
                "x-api-key",
                "twl_zzzzyyyyxxxxwwwwvvvvuuuuttttssssrrrrqqqqppppoooonnnnmmmmllll",
            ),
        ]);
        assert_eq!(
            extract_client_key(&headers),
            Some(
                "twl_aaaabbbbccccddddeeeeffff0000111122223333444455556666777788889999".to_string()
            )
        );
    }

    #[test]
    fn non_twl_bearer_ignored() {
        let headers = hm(&[("authorization", "Bearer sk-abc123")]);
        assert_eq!(extract_client_key(&headers), None);
    }

    #[test]
    fn non_twl_x_api_key_ignored() {
        let headers = hm(&[("x-api-key", "sk-abc123")]);
        assert_eq!(extract_client_key(&headers), None);
    }

    #[test]
    fn no_auth_headers_returns_none() {
        let headers = hm(&[]);
        assert_eq!(extract_client_key(&headers), None);
    }

    #[test]
    fn basic_auth_ignored() {
        let headers = hm(&[("authorization", "Basic dXNlcjpwYXNz")]);
        assert_eq!(extract_client_key(&headers), None);
    }

    #[test]
    fn bearer_without_twl_ignored() {
        let headers = hm(&[("authorization", "Bearer "), ("x-api-key", "something")]);
        assert_eq!(extract_client_key(&headers), None);
    }
}
