# Changelog

All notable changes to Maivin WebSrv will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [3.8.3] - 2026-03-01

### Fixed

- Default docroot path changed from `/usr/share/webui` to `/usr/share/edgefirst/webui`
- Removed `SYSTEM` variable from `websrv.default` (system mode is configured via systemd unit file)

## [3.8.2] - 2026-02-26

### Changed

- Use short environment variable names for systemd configuration
- Add complete `websrv.default` reference file with all options documented

### Fixed

- Update NOTICE file dependency versions to match current Cargo.lock

### Maintenance

- Update Cargo.lock with latest dependency versions
- Add `.github/copilot-instructions.md` project guidance

## [3.8.1] - 2026-02-03

### Fixed

- Release artifact filenames now use `X.Y.Z` format instead of `vX.Y.Z`

## [3.8.0] - 2026-02-03

### Added

- Graceful shutdown coordination with SIGTERM/SIGINT signal handling
- `ShutdownCoordinator` module for coordinated shutdown of all server components
- Global shutdown token propagation to WebSocket connections and Zenoh subscribers
- `cancel_all_uploads()` and `wait_for_completion()` methods for upload manager shutdown
- Flexible SSL certificate management with auto-generation support
- CLI options for certificate configuration (`--cert-dir`, `--cert`, `--key`, `--generate-cert`)
- Self-signed certificate generation with mDNS hostname in SANs
- TESTING.md documentation for comprehensive testing guide

### Changed

- WebSocket Zenoh listeners now respond to both per-connection and global shutdown signals
- Server performs explicit cleanup of active uploads and Zenoh session on shutdown
- Certificate priority chain: CLI args → cert-dir → auto-generate → embedded fallback
- README.md rewritten with focus on usage and configuration
- ARCHITECTURE.md updated with TLS certificate management section

## [3.7.0] - 2026-01-04

### Added

- Initial open source release
- MCAP upload to EdgeFirst Studio with real-time progress tracking
- EdgeFirst Studio authentication (login/logout/status APIs)
- Background upload worker with power loss recovery
- Extended upload mode with AGTG auto-labeling support
- 80 COCO classes for YOLOX/SAM2 auto-labeling
- SBOM generation and license compliance validation
- Web UI for Maivin platform control
- Real-time data visualization
- MCAP file playback and analysis
- System monitoring and diagnostics
- RESTful API
- WebSocket support

### Changed

- Migrated from Bitbucket to GitHub
- Updated license to Apache-2.0
- SPS v2.1.1 compliance with complete documentation
- Updated to Rust 1.90.0

### Security

- Added security policy and vulnerability reporting process
- Sanitized authentication error messages to prevent information leakage
