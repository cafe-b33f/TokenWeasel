# Contributing

Thank you for improving TokenWeasel. Keep changes focused, follow the existing project conventions, and avoid unrelated refactors or formatting.

## Development setup

The workspace requires Rust 1.88. The repository's `rust-toolchain.toml` selects Rust 1.88.0 with rustfmt and Clippy; install it if needed:

```sh
rustup toolchain install 1.88.0 --component rustfmt clippy
cargo build --workspace --locked
```

## Before submitting

Run the checks that CI requires:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Add or update focused tests for behavior changes. Update relevant documentation when configuration, commands, or user-visible behavior changes. Do not commit credentials, local configuration, databases, certificates, or private keys.

## Pull requests

- Base pull requests on `main` and keep each pull request limited to one coherent change.
- Explain the problem, the chosen solution, and any compatibility or security impact.
- List the validation commands you ran and call out anything you could not run.
- Link related issues when applicable and respond to review feedback with additional focused commits.

For vulnerabilities, do not open a normal issue or pull request. Follow the private process in [SECURITY.md](SECURITY.md).
