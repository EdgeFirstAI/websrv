# Maivin WebSrv

[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Build Status](https://github.com/EdgeFirstAI/websrv/actions/workflows/build.yml/badge.svg)](https://github.com/EdgeFirstAI/websrv/actions/workflows/build.yml)
[![Test Status](https://github.com/EdgeFirstAI/websrv/actions/workflows/test.yml/badge.svg)](https://github.com/EdgeFirstAI/websrv/actions/workflows/test.yml)

Web UI server for EdgeFirst Maivin platform.

## Overview

Maivin WebSrv is a web-based user interface server for monitoring and controlling the EdgeFirst Maivin platform.

## Features

- Web UI for Maivin platform control
- Real-time data visualization
- MCAP file playback and analysis
- System monitoring and diagnostics
- RESTful API
- WebSocket support for real-time updates

## Requirements

- Rust 1.70 or later
- ROS 2 Humble or later
- EdgeFirst runtime environment

## Building

```bash
cargo build --release
```

## Running

```bash
cargo run --release
```

## Testing

```bash
cargo test
```

## Documentation

For detailed documentation, visit [EdgeFirst Documentation](https://doc.edgefirst.ai/latest/maivin/).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines on contributing to this project.

## License

Copyright 2025 Au-Zone Technologies Inc.

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Security

For security vulnerabilities, see [SECURITY.md](SECURITY.md).
