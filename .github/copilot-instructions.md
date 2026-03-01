# EdgeFirst WebSrv - Project Instructions

## Project Overview

EdgeFirst WebSrv (`edgefirst-websrv`) is the backend server for the Maivin edge AI
platform's web interface. It is a Rust application built on Actix-web that provides
REST APIs, WebSocket streaming (via Zenoh), MCAP recording management, systemd service
control, and snapshot uploads to EdgeFirst Studio.

The web frontend lives in a separate repository:
[github.com/EdgeFirstAI/webui](https://github.com/EdgeFirstAI/webui). The two projects
work hand-in-hand: **websrv** implements the server and **webui** implements the web
interface served by the server. The compiled webui assets are served from the path
configured via `--docroot` (default `/usr/share/edgefirst/webui`).

## Repository Structure

```
src/
  main.rs        # HTTP server, WebSocket handlers, API endpoints, Upload Manager
  args.rs        # Command-line argument parsing and configuration
tests/           # Integration tests
websrv.default   # Default systemd EnvironmentFile with all configuration options
NOTICE           # Third-party dependency attributions (must stay in sync with SBOM)
sbom.json        # CycloneDX Software Bill of Materials
ARCHITECTURE.md  # Detailed architecture documentation
```

## Build and Development

### Native Build

```sh
cargo build                  # Debug build
cargo build --release        # Release build (LTO + stripped)
```

### Cross-Compilation with cargo-zigbuild

Use `cargo zigbuild` to cross-compile for target hardware (typically aarch64):

```sh
# Install zigbuild once
cargo install cargo-zigbuild

# Cross-compile for aarch64
cargo zigbuild --release --target aarch64-unknown-linux-gnu
```

The resulting binary is at `target/aarch64-unknown-linux-gnu/release/edgefirst-websrv`
and can be deployed directly to Maivin devices for testing.

### Running Locally

```sh
cargo run -- --docroot /path/to/webui/dist --storage-path ./recordings
```

See `websrv.default` for a complete reference of all configuration options and their
descriptions.

## Pre-Commit Checklist

Before every commit, run:

```sh
make format lint check sbom
```

This ensures:
- `format` - Code is formatted with `rustfmt` (nightly preferred)
- `lint` - Clippy passes with `-D warnings`
- `check` - `cargo check` passes (use `build` for a full compile)
- `sbom` - SBOM is regenerated and the NOTICE file matches current dependencies

Do not commit if any of these fail.

## Updating Dependencies

When asked to update Cargo dependencies, follow this workflow:

1. **Update version specifiers** in `Cargo.toml` to the desired versions
2. **Run `cargo update`** to resolve and write the new `Cargo.lock`
3. **Run `make sbom`** to regenerate `sbom.json` and validate the NOTICE file
4. If `make sbom` reports missing/extra entries in NOTICE, **update the NOTICE file**
   to match the new first-level dependency versions
5. Re-run `make sbom` to confirm it passes cleanly
6. Run `make format lint` before committing

## Code Style and Conventions

- Rust 2021 edition
- Format with `cargo +nightly fmt --all` (falls back to stable `cargo fmt`)
- Clippy with `--all-targets --all-features -- -D warnings`
- Error handling with `anyhow::Result`
- Async runtime: Tokio (multi-threaded, 8 workers)
- WebSocket actors via `actix-web-actors`
- Real-time pub/sub via Zenoh

## Testing

```sh
make test    # Runs cargo-nextest with llvm-cov coverage
```

Integration tests are in the `tests/` directory.

## CI Workflows

| Workflow | Purpose |
|----------|---------|
| `build.yml` | Builds x86_64 and aarch64 binaries, generates rustdoc |
| `test.yml` | Runs tests with coverage |
| `sbom.yml` | Generates SBOM and validates NOTICE + license compliance |
| `release.yml` | Publishes release artifacts |

## Deployment

The server runs as a systemd service on Maivin devices. Configuration is loaded from
`/etc/default/websrv` (see `websrv.default` for the template). It supports two modes:

- **System mode** (`--system`): Controls services via systemd, reads config from `/etc/default/`
- **User mode** (default): Spawns processes directly, useful for development
