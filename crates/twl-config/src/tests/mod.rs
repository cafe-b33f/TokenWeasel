//! Unit tests for CLI argument parsing and config-file resolution.
//!
//! Covers:
//! - **CLI parsing** (`cli.rs`): flag parsing, short/long variants, aliases,
//!   inline `--flag=value` syntax.
//! - **Config resolution** (`config.rs`): precedence chains (CLI > env > file >
//!   default), bind address logic with IPv6 bracketing, gpu_watts validation,
//!   config-file JSON parsing, and end-to-end `Config::resolve` with explicit
//!   file paths.

mod cli;
mod config;
