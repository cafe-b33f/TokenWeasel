//! Shared application state handed to every request handler.
//!
//! `AppState` is constructed once at startup and `Arc`-shared into the
//! axum router. All fields are safe for concurrent read access.

use std::sync::Arc;

use twl_backends::BackendRegistry;
use twl_pricing::Pricing;
use twl_store::IdentityStore;
use twl_store::UsageStore;

use crate::fair_queue::FairQueue;
use crate::ratelimit::LoginThrottle;
use crate::sink::AccountingWriter;

/// Dependencies shared across all handlers. Cheap to clone (everything inside
/// is `Arc` or an already-pooled `reqwest::Client`).
pub struct AppState {
    /// HTTP client for upstream requests (connection-pooled).
    pub client: reqwest::Client,
    /// Backend registry: model-to-backend routing and energy meters.
    pub registry: Arc<BackendRegistry>,
    /// SQLite-backed token usage and energy store.
    pub usage: Arc<UsageStore>,
    /// Nonblocking handle to the dedicated usage/energy writer thread.
    pub(crate) accounting: AccountingWriter,
    /// SQLite-backed identity store (users, API keys).
    pub identity: Arc<IdentityStore>,
    /// Price table for cost computation, loaded once at startup.
    pub pricing: Arc<Pricing>,
    /// Resolved dashboard display configuration.
    pub dashboard: twl_config::DashboardConfig,
    /// Per-workload concurrency cap. Proxy execution, API-key validation, and
    /// the combined set of management handlers use independent limits of this
    /// size so validation cannot occupy fair-queue capacity.
    pub max_concurrency: usize,
    /// Bounds database-backed API-key validation before proxy admission.
    pub api_key_validation_slots: Arc<tokio::sync::Semaphore>,
    /// Proxy admission queue enforcing the concurrency cap and optional fairness.
    pub fair_queue: Arc<FairQueue>,
    /// When true, the dashboard and read-only APIs are served without auth.
    pub public_usage: bool,
    /// When true, proxied LLM requests without a valid API key are rejected.
    /// An invalid `twl_` key is always rejected.
    pub require_api_key: bool,
    /// Per-account login lockout tracker.
    pub login_throttle: Arc<LoginThrottle>,
    /// Bearer token granting read-only access to `/metrics`.
    /// `None` disables the endpoint entirely.
    pub metrics_token: Option<String>,
}
