//! Configuration resolution for the LLM proxy.
//!
//! Handles CLI parsing via `clap`, config-file loading via `serde_json`,
//! and multi-layer merging (CLI > env > config-file > defaults).
//! The bind address has special resolution rules documented in
//! [`Config`].

pub(crate) mod cli;
pub(crate) mod config;

pub use cli::Cli;
pub use config::BackendConfig;
pub use config::BackendConfigFile;
pub use config::Config;
pub use config::ConfigError;
pub use config::DashboardConfig;
pub use config::FairQueueConfig;
pub use config::LbStrategy;
pub use config::LoadBalancing;
pub use config::TlsConfig;
pub use twl_provider::ProviderKind;

#[cfg(test)]
mod tests;
