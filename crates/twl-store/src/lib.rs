//! Consolidated SQLite-backed store for usage and identity data.
//!
//! Combines database connection management ([`Database`]), an identity store
//! ([`IdentityStore`]) for users and API keys, and a usage store
//! ([`UsageStore`]) for token and energy recording.

pub mod db;
pub mod identity;
pub mod usage;

pub use db::{now_unix_secs, Database, DbError, DbPool};
pub use identity::{DeleteUserOutcome, IdentityStore};
pub use twl_types::{
    DailyEnergy, DailyStats, EnergyRecord, KeyStats, ModelStats, TodayBucket, Usage,
};
pub use usage::UsageStore;
