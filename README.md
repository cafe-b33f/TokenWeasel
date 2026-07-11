# TokenWeasel

[![CI](https://github.com/CafeB33f/TokenWeasel/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/CafeB33f/TokenWeasel/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/CafeB33f/TokenWeasel?display_name=tag&sort=semver)](https://github.com/CafeB33f/TokenWeasel/releases)
[![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](rust-toolchain.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

TokenWeasel is a single-binary reverse proxy for OpenAI-compatible LLM
backends. It routes requests to local or hosted providers and records token
usage, estimated cost, and optional GPU energy usage in SQLite.

It works with Ollama, llama.cpp, LM Studio, vLLM, DeepSeek, OpenRouter, and
other services that expose an OpenAI-compatible API.

## Features

- Proxies chat, completion, embedding, streaming, and backend-native requests
- Routes by model across one or more backends, with failover and load balancing
- Publishes a cached OpenAI-compatible model catalog at `/v1/models`
- Tracks per-user and per-model tokens, cost, requests, and estimated energy use
- Includes a self-contained usage dashboard with no external web assets
- Supports dashboard accounts, hashed API keys, TLS, and Prometheus metrics
- Runs as one binary with file-backed SQLite storage

TokenWeasel does not run models itself. You need an LLM backend or a compatible
hosted API.

## Requirements

- Rust 1.88 or later to build from source
- An OpenAI-compatible backend

The repository pins the required Rust toolchain in `rust-toolchain.toml`.
Release builds target Linux x86-64, Linux ARM64, and Windows x86-64; other
platforms can build from source.

## Quick start with Ollama

Install [Ollama](https://ollama.com/), then download the models used by the
example configuration:

```sh
ollama pull qwen3:8b
ollama pull nomic-embed-text
```

Build TokenWeasel and copy the example configuration:

```sh
cp config.example.json config.json
cargo build --release --locked
./target/release/twl
```

TokenWeasel listens on `https://localhost:3000`. On its first start it:

- creates a self-signed certificate in `tls/`;
- creates the SQLite database;
- creates the `admin` account and prints a random password to the console.

Open `https://localhost:3000/usage`, accept or trust the local certificate, and
sign in with the generated password. Then open `/account` to create an API key.
API keys start with `twl_` and are shown only when created.

The example is ready for local Ollama use. It exposes `qwen3:8b` for chat and
`nomic-embed-text` for embeddings, keeps access private by default, and uses a
250 W estimate for GPU energy accounting. Change `gpu_watts` to match your
hardware, or set it to `0` to disable energy estimates.

## API usage

`/v1/models` is public and returns TokenWeasel's cached model catalog:

```sh
curl --cacert tls/cert.pem https://localhost:3000/v1/models
```

Proxied requests require an API key by default:

```sh
export TOKENWEASEL_API_KEY='twl_your_generated_key'

curl --cacert tls/cert.pem https://localhost:3000/v1/chat/completions \
  -H "Authorization: Bearer ${TOKENWEASEL_API_KEY}" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "qwen3:8b",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

Most OpenAI-compatible clients can use TokenWeasel by setting their base URL to
`https://localhost:3000/v1` and their API key to the generated `twl_` value.
The client must trust `tls/cert.pem` when the generated certificate is used.

## Configuration

TokenWeasel loads `config.json` from the working directory by default. Use
`--config PATH` to load another file. Settings are resolved in this order:

1. command-line options;
2. environment variables;
3. the JSON configuration file;
4. built-in defaults.

Unknown JSON fields and invalid combinations are rejected at startup. See
[`config.example.json`](config.example.json) for a complete configuration with
every supported field.

The most commonly changed settings are:

| Setting | Purpose |
|---|---|
| `host`, `port` | Listener address; defaults to `127.0.0.1:3000` |
| `upstream` | Base URL for a single backend |
| `backends` | Provider, URL, models, credentials, filters, and energy settings for one or more named backends |
| `db_path` | SQLite database path |
| `pricing` | Currency and per-model input, cached-input, and output rates |
| `max_concurrency` | Per-workload concurrency limit |
| `require_api_key` | Require a valid `twl_` key for proxied requests; defaults to `true` |
| `public_usage` | Allow unauthenticated read-only dashboard access |
| `dashboard` | Visible cards and graphs, row limits, and trusted proxy addresses |
| `retention_days` | Usage-data retention; `0` disables pruning |
| `load_balancing` | Multi-backend routing strategy and overload threshold |
| `fair_queue` | Optional rolling-window priority between users |
| `tls` | Listener certificate, key, hostnames, and enablement |
| `metrics_token` | Dedicated bearer token for `/metrics`; unset disables the endpoint |

`upstream` and `backends` are alternative modes and cannot be configured
together. Within a backend, use either `api_key` or `api_key_env`, not both.
Prefer `api_key_env` for hosted providers so credentials do not appear in the
configuration file.

Common command-line overrides include:

```text
--host HOST
--port PORT
--upstream URL
--db PATH
--max-concurrency N
--config PATH
--tls true|false
--tls-cert PATH
--tls-key PATH
```

Run `twl --help` for the full command-line reference. Supported environment
overrides include `BIND_ADDR`, `UPSTREAM_URL`, `DB_PATH`, `MAX_CONCURRENCY`,
`CONFIG_PATH`, `DASHBOARD_USER`, `DASHBOARD_PASSWORD`, `METRICS_TOKEN`,
`TLS_ENABLED`, `TLS_CERT_PATH`, and `TLS_KEY_PATH`.

### Multiple backends

When several backends serve the same model, TokenWeasel keeps requests with the
same prompt prefix on the same backend where possible, which helps preserve
prompt-cache locality. It can place other requests using `least_loaded` or
`round_robin`, spill work away from overloaded backends, and retry another
backend when a connection fails. Completed HTTP responses, including upstream
`5xx` responses, are returned without retrying.

The optional fair queue applies when proxy capacity is full. It prioritizes
users with fewer admitted requests in a rolling window; API keys owned by the
same user share one identity.

## Web and monitoring endpoints

| Endpoint | Description | Access |
|---|---|---|
| `/usage` | Usage dashboard | Dashboard session, unless `public_usage` is enabled |
| `/account` | Account and API-key management | Dashboard session |
| `/v1/models` | Cached model catalog | Public |
| `/health` | Liveness check | Public |
| `/metrics` | Prometheus metrics | Dedicated metrics bearer token |

The metrics token must contain at least 16 characters. It grants access only to
`/metrics`; dashboard sessions and `twl_` API keys are intentionally separate.

## TLS

TLS is enabled by default. If no certificate and key exist, TokenWeasel creates
and reuses a self-signed pair at `tls/cert.pem` and `tls/key.pem`. Supply your
own PEM certificate and private key for a public deployment, or disable
TokenWeasel's TLS only when a trusted ingress terminates HTTPS.

See [TLS.md](TLS.md) for certificate names, trust configuration, environment
overrides, and reverse-proxy guidance.

## Operational notes

- The default listener is loopback-only. Review authentication, TLS, firewall,
  and reverse-proxy settings before binding to `0.0.0.0`.
- Keep `require_api_key` enabled on untrusted networks. Avoid combining a public
  bind address with `public_usage: true` unless the dashboard is meant to be
  public.
- Put passwords and provider credentials in environment variables. Do not
  commit `config.json`, databases, certificates, or private keys.
- Configure `dashboard.trusted_proxies` only with the exact IP addresses of
  proxies allowed to supply forwarding headers. CIDR ranges are not accepted.
- Cost figures depend on the configured rates. Energy figures use configured
  wattage over busy intervals rather than hardware telemetry; both are
  estimates.

TokenWeasel is currently at version 0.1. Review its configuration and behavior
before relying on it in production.

## Development

Run the workspace checks before submitting changes:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for development and pull-request
guidelines. Report security issues using the private process described in
[SECURITY.md](SECURITY.md). Release history is recorded in
[CHANGELOG.md](CHANGELOG.md).

## Disclaimer on AI assistance

Claude, Codex, and locally hosted models were used to accelerate development. All AI-assisted modifications were reviewed before inclusion.

## License

TokenWeasel is available under the [MIT License](LICENSE).