# Changelog

All notable changes to Maivin WebSrv will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Graceful shutdown coordination with SIGTERM/SIGINT signal handling
- `ShutdownCoordinator` module for coordinated shutdown of all server components
- Global shutdown token propagation to WebSocket connections and Zenoh subscribers
- `cancel_all_uploads()` and `wait_for_completion()` methods for upload manager shutdown

### Changed

- WebSocket Zenoh listeners now respond to both per-connection and global shutdown signals
- Server performs explicit cleanup of active uploads and Zenoh session on shutdown

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
