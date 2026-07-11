//! HTTP server, reverse proxy, and shared application state.
//!
//! Assembles the server: builds shared state from config, wires up routes,
//! and serves HTTP traffic via axum. The reverse-proxy handler forwards
//! requests to an upstream LLM server while extracting token usage and energy data.

pub(crate) mod handlers;
mod localtime;
pub(crate) mod metrics;
pub(crate) mod proxy;
pub(crate) mod retention;
mod routes;
pub(crate) mod server;
mod session_store;
pub(crate) mod sink;
pub(crate) mod state;
mod tls;
pub(crate) mod watchdog;

pub(crate) mod auth;
pub(crate) mod fair_queue;
pub(crate) mod ratelimit;

use rust_embed::RustEmbed;

/// Embedded web assets from the `web/` directory.
#[derive(RustEmbed)]
#[folder = "web"]
struct WebAssets;

#[cfg(test)]
mod tests;

pub use server::{run, run_with_listener, ServerError};
