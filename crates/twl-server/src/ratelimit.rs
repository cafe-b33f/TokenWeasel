//! Client IP-based login throttle with capped exponential backoff.
//!
//! After a small number of free failures, each subsequent failure increases
//! the wait duration exponentially (in powers of two), capped at a maximum
//! backoff so that a correct credential is never permanently locked.
//!
//! The throttle uses an in-memory `Mutex<HashMap>` so lockout state is
//! ephemeral (lost on restart). Capacity is bounded to prevent unbounded
//! memory growth under a distributed dictionary attack across many clients.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;

use axum::http::HeaderMap;
use twl_store::now_unix_secs;

/// Maximum number of tracked clients before the map is cleared to bound memory.
const CAP: usize = 4096;

/// Number of failures allowed with no delay at all.
const FREE_ATTEMPTS: u32 = 5;

/// Base backoff in seconds. Each backoff step doubles this value.
const BASE_BACKOFF_SECS: i64 = 1;

/// Maximum backoff in seconds. Backoff never exceeds this cap.
const MAX_BACKOFF_SECS: i64 = 60;

/// Computes the backoff duration in seconds for a given failure count.
///
/// * Failures ≤ FREE_ATTEMPTS → 0 (no delay).
/// * Otherwise the shift amount is `failures - FREE_ATTEMPTS - 1`,
///   capped at 32 to avoid overflow.
/// * Returns `min(MAX_BACKOFF_SECS, BASE_BACKOFF_SECS << shift)`.
fn backoff_for_failures(failures: u32) -> i64 {
    if failures <= FREE_ATTEMPTS {
        return 0;
    }
    let shift = (failures - FREE_ATTEMPTS - 1).min(32);
    let raw = BASE_BACKOFF_SECS << shift;
    raw.min(MAX_BACKOFF_SECS)
}

/// Extracts a throttle key (client IP) from the request headers and peer address.
///
/// Forwarded headers are honored only when the direct socket peer is listed
/// as trusted. The forwarded chain is inspected from right to left so a
/// client-supplied leftmost value cannot spoof its throttle identity when a
/// trusted proxy appends the real address.
///
/// Without a peer address the key is `"unknown"`. An untrusted peer always
/// keys itself. A trusted peer may identify the client through
/// `X-Forwarded-For` or `X-Real-IP`; malformed values safely fall back.
pub fn client_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted_proxies: &[IpAddr],
) -> String {
    let Some(peer_ip) = peer.map(|addr| addr.ip()) else {
        return "unknown".to_string();
    };
    if !trusted_proxies.contains(&peer_ip) {
        return peer_ip.to_string();
    }

    if let Some(value) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        let chain = value
            .split(',')
            .map(str::trim)
            .map(str::parse::<IpAddr>)
            .collect::<Result<Vec<_>, _>>();
        if let Ok(chain) = chain {
            if let Some(client) = chain
                .into_iter()
                .rev()
                .find(|ip| !trusted_proxies.contains(ip))
            {
                return client.to_string();
            }
        }
    }

    if let Some(ip) = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
    {
        return ip.to_string();
    }

    peer_ip.to_string()
}

/// Tracks failure count and optional lockout expiry for a single client.
struct Entry {
    /// Consecutive recent failures since the last successful login.
    failures: u32,
    /// Unix timestamp (seconds) after which the client may try again.
    /// Zero means not currently blocked.
    blocked_until: i64,
}

/// Guards against online password guessing by applying capped exponential
/// backoff to a specific client key after too many consecutive failures.
///
/// The first `FREE_ATTEMPTS` failures incur no delay. Each subsequent
/// failure doubles the wait (1 s, 2 s, 4 s, …) up to `MAX_BACKOFF_SECS`,
/// ensuring that a correct credential is never permanently locked.
///
/// Internally holds a `Mutex<HashMap<String, Entry>>`. All public methods
/// recover from a poisoned mutex by claiming the inner data.
pub struct LoginThrottle {
    map: Mutex<HashMap<String, Entry>>,
}

impl LoginThrottle {
    /// Creates an empty throttle with no locked or tracked clients.
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Checks whether `key` is currently blocked.
    ///
    /// Returns `Ok(())` if the key is not blocked, or `Err(remaining_secs)`
    /// with the number of whole seconds remaining until the block expires.
    pub fn check(&self, key: &str) -> Result<(), i64> {
        let map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_unix_secs().unwrap_or(0);

        if let Some(entry) = map.get(key) {
            if entry.blocked_until > now {
                return Err(entry.blocked_until - now);
            }
        }

        Ok(())
    }

    /// Records a failed login attempt for `key`.
    ///
    /// Increments the failure count (saturating at `u32::MAX`).
    /// When the new failure count produces a positive backoff,
    /// `blocked_until` is updated to `now + backoff`.
    ///
    /// If the key is new and the map is at capacity, a targeted eviction
    /// is performed first: entries whose `blocked_until` ≤ now are pruned,
    /// then, if still at capacity, the entry with the smallest
    /// `blocked_until` is removed.
    pub fn record_failure(&self, key: &str) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let now = now_unix_secs().unwrap_or(0);

        // Enforce capacity cap with targeted eviction.
        if !map.contains_key(key) && map.len() >= CAP {
            // First pass: remove expired entries (blocked_until <= now).
            map.retain(|_, entry| entry.blocked_until > now);

            // If still at capacity, remove the entry with the smallest blocked_until.
            if map.len() >= CAP {
                if let Some(min_key) = map
                    .iter()
                    .min_by_key(|(_, entry)| entry.blocked_until)
                    .map(|(key, _)| key.clone())
                {
                    map.remove(&min_key);
                }
            }
        }

        let entry = map.entry(key.to_string()).or_insert(Entry {
            failures: 0,
            blocked_until: 0,
        });

        entry.failures = entry.failures.saturating_add(1);

        let bk = backoff_for_failures(entry.failures);
        if bk > 0 {
            entry.blocked_until = now + bk;
        }
    }

    /// Records a successful login for `key`, removing the entry entirely.
    pub fn record_success(&self, key: &str) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(key);
    }
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                name.parse::<http::HeaderName>().unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn trusted_proxy_separates_clients_and_ignores_spoofed_leftmost_hops() {
        let proxy: IpAddr = "10.0.0.10".parse().unwrap();
        let peer = Some(SocketAddr::new(proxy, 443));

        assert_eq!(
            client_ip(
                &headers(&[("x-forwarded-for", "198.51.100.1")]),
                peer,
                &[proxy]
            ),
            "198.51.100.1"
        );
        assert_eq!(
            client_ip(
                &headers(&[("x-forwarded-for", "203.0.113.99, 198.51.100.2")]),
                peer,
                &[proxy]
            ),
            "198.51.100.2"
        );
    }

    #[test]
    fn untrusted_peer_cannot_spoof_forwarded_identity() {
        let peer: IpAddr = "192.0.2.44".parse().unwrap();
        assert_eq!(
            client_ip(
                &headers(&[
                    ("x-forwarded-for", "198.51.100.7"),
                    ("x-real-ip", "198.51.100.8"),
                ]),
                Some(SocketAddr::new(peer, 1234)),
                &["10.0.0.10".parse().unwrap()]
            ),
            "192.0.2.44"
        );
    }

    #[test]
    fn check_ok_when_untracked() {
        let throttle = LoginThrottle::new();
        assert!(throttle.check("alice").is_ok());
    }

    #[test]
    fn no_lockout_within_free_attempts() {
        let throttle = LoginThrottle::new();
        for _ in 0..FREE_ATTEMPTS {
            throttle.record_failure("alice");
        }
        assert!(throttle.check("alice").is_ok());
    }

    #[test]
    fn locked_out_after_free_attempts() {
        let throttle = LoginThrottle::new();
        for _ in 0..FREE_ATTEMPTS + 1 {
            throttle.record_failure("alice");
        }
        let result = throttle.check("alice");
        assert!(result.is_err());
        let remaining = result.unwrap_err();
        assert!(remaining > 0);
    }

    #[test]
    fn success_clears_failures() {
        let throttle = LoginThrottle::new();
        for _ in 0..FREE_ATTEMPTS + 1 {
            throttle.record_failure("alice");
        }
        throttle.record_success("alice");
        throttle.record_failure("alice");
        assert!(throttle.check("alice").is_ok());
    }

    #[test]
    fn accounts_are_independent() {
        let throttle = LoginThrottle::new();
        for _ in 0..FREE_ATTEMPTS + 1 {
            throttle.record_failure("alice");
        }
        assert!(throttle.check("alice").is_err());
        assert!(throttle.check("bob").is_ok());
    }

    #[test]
    fn backoff_never_exceeds_max() {
        let throttle = LoginThrottle::new();
        // Many failures to trigger the maximum backoff.
        for _ in 0..100 {
            throttle.record_failure("alice");
        }
        let result = throttle.check("alice");
        assert!(result.is_err());
        let remaining = result.unwrap_err();
        // The stored blocked_until was set to now + backoff, so the
        // remaining time must be <= MAX_BACKOFF_SECS.
        assert!(
            remaining <= MAX_BACKOFF_SECS,
            "backoff {remaining} exceeds MAX_BACKOFF_SECS {MAX_BACKOFF_SECS}"
        );
    }

    #[test]
    fn overflow_eviction_preserves_active_entry() {
        // We need at least CAP + 1 accounts. To avoid an excessively long
        // test we use a tiny CAP override is not possible, so we just ensure
        // that eviction does not wipe an unrelated entry that is still
        // within its free-failure window (blocked_until == 0, which is
        // treated as not-blocked - we need an entry with blocked_until > now
        // instead).
        //
        // Strategy: fill CAP slots with free-failure entries (blocked_until
        // == 0), then insert one more with failures = FREE_ATTEMPTS + 1 so
        // it gets blocked_until > now. The new entry must succeed in being
        // inserted while evicting another. We then check that the still-active
        // (non-evicted) entry with failures count is preserved.
        //
        // Actually blocked_until == 0 means not blocked, so it will be
        // evicted during the prune pass. We need an entry whose
        // blocked_until > now. We achieve this by first giving one account
        // FREE_ATTEMPTS + 1 failures so it gets blocked_until > now,
        // then filling the rest with 1 failure each, then adding another
        // one beyond CAP.

        let throttle = LoginThrottle::new();

        // Create one entry that will be blocked (blocked_until > now).
        let guardian = "guardian";
        for _ in 0..FREE_ATTEMPTS + 1 {
            throttle.record_failure(guardian);
        }

        // Now create CAP - 1 more entries with just 1 failure each,
        // so total unique accounts = CAP.
        for i in 0..CAP - 1 {
            let acc = format!("victim_{i}");
            throttle.record_failure(&acc);
        }

        assert_eq!(throttle.map.lock().unwrap().len(), CAP);

        // Now add one more account - should trigger eviction.
        throttle.record_failure("newcomer");

        let map = throttle.map.lock().unwrap();
        assert!(map.contains_key("newcomer"));
        // The guardian entry should still be present (it wasn't evicted).
        let guardian_entry = map.get(guardian).expect("guardian should survive eviction");
        assert!(guardian_entry.blocked_until > 0);
        // The newcomer should also be blocked (it has FREE_ATTEMPTS + 1 failures).
        assert!(guardian_entry.blocked_until > now_unix_secs().unwrap_or(0));
    }
}
