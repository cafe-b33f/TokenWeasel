# Changelog

Notable changes to TokenWeasel are documented in this file.

## [0.1.1] - 2026-07-11

### Changed

- Linux x86-64 release binaries are now built against GNU/glibc instead of
  MUSL; they are no longer statically linked.
- Updated dependencies, notably sha2 0.11, rand 0.10, rcgen 0.14, and
  tower-http 0.7, and unified dependency versions across the workspace.

## [0.1.0]

Initial public release.

### Added

- OpenAI-compatible proxying for chat, completion, embedding, streaming, and
  backend-native requests.
- Model-aware routing across multiple backends, with load balancing, failover,
  prompt-prefix affinity, and optional fair queuing.
- SQLite accounting for requests, tokens, configured costs, and estimated GPU
  energy use.
- A self-contained usage dashboard, dashboard accounts, hashed API keys, TLS,
  health checks, and optional Prometheus metrics.
- Configuration through JSON, environment variables, and command-line options,
  with a complete Ollama example.
- Automated Linux x86-64, Linux ARM64, and Windows x86-64 release builds,
  packaged with SHA-256 checksums.

[0.1.1]: https://github.com/cafe-b33f/TokenWeasel/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/cafe-b33f/TokenWeasel/releases/tag/v0.1.0
