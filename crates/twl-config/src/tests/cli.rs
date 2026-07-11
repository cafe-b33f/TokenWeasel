//! Tests for [`twl_config::cli`] argument parsing.
//!
//! Covers short/long flag variants, aliases (`--bind` → `--host`),
//! inline `--flag=value` syntax, and the `--config` path flag.

use crate::cli::Cli;
use clap::Parser;

/// Shorthand: build a [`Cli`] from a slice of argument strings (no program
/// name prepended, [`Cli::from_args`] does that).
fn args(list: &[&str]) -> Cli {
    Cli::from_args(list.iter().map(|s| s.to_string()))
}

#[test]
fn config_flag_parsed() {
    let cli = args(&["--config", "my-config.json"]);
    assert_eq!(cli.config.as_deref(), Some("my-config.json"));
}

#[test]
fn parses_long_flags_with_separate_values() {
    let cli = args(&[
        "--port",
        "8080",
        "--host",
        "0.0.0.0",
        "--upstream",
        "http://u:1",
        "--db",
        "d.db",
    ]);
    assert_eq!(cli.port, Some(8080));
    assert_eq!(cli.host.as_deref(), Some("0.0.0.0"));
    assert_eq!(cli.upstream.as_deref(), Some("http://u:1"));
    assert_eq!(cli.db.as_deref(), Some("d.db"));
    // Not set on CLI - resolved to default (256) in config
    assert_eq!(cli.max_concurrency, None);
}

#[test]
fn max_concurrency_flag() {
    let cli = args(&["--max-concurrency", "64"]);
    assert_eq!(cli.max_concurrency, Some(64));
}

#[test]
fn max_concurrency_flag_rejects_zero() {
    let err = Cli::try_parse_from(["twl", "--max-concurrency", "0"]).unwrap_err();
    assert!(err.to_string().contains("between 1 and"));
}

#[test]
fn max_concurrency_flag_rejects_values_above_semaphore_limit() {
    let above_limit = (crate::cli::MAX_CONCURRENCY_LIMIT + 1).to_string();
    let err = Cli::try_parse_from(["twl", "--max-concurrency", &above_limit]).unwrap_err();
    assert!(err.to_string().contains("between 1 and"));
}

#[test]
fn parses_inline_equals_form() {
    let cli = args(&["--port=9000", "--upstream=http://x"]);
    assert_eq!(cli.port, Some(9000));
    assert_eq!(cli.upstream.as_deref(), Some("http://x"));
}

#[test]
fn supports_short_and_alias_flags() {
    let cli = args(&["-p", "1234", "-u", "http://y", "--bind", "127.0.0.1"]);
    assert_eq!(cli.port, Some(1234));
    assert_eq!(cli.upstream.as_deref(), Some("http://y"));
    // `--bind` is an alias for `--host`.
    assert_eq!(cli.host.as_deref(), Some("127.0.0.1"));
}
