# Maivin WebSrv

[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
[![Build Status](https://github.com/EdgeFirstAI/websrv/actions/workflows/build.yml/badge.svg)](https://github.com/EdgeFirstAI/websrv/actions/workflows/build.yml)
[![Test Status](https://github.com/EdgeFirstAI/websrv/actions/workflows/test.yml/badge.svg)](https://github.com/EdgeFirstAI/websrv/actions/workflows/test.yml)

HTTPS web server for the EdgeFirst Maivin platform, providing real-time visualization, MCAP recording management, and EdgeFirst Studio integration.

## Quick Start

```bash
# Build
cargo build --release

# Run with default settings (requires root for ports 80/443)
sudo ./target/release/edgefirst-websrv

# Run on non-privileged ports
./target/release/edgefirst-websrv --https-port 8443 --http-port 8080
```

Navigate to `https://localhost:8443` (or `https://<hostname>.local:8443` for mDNS).

## WebUI Setup

### Using EdgeFirstAI/webui (Default)

The server serves static files from `--docroot` (default: `/usr/share/webui`).

```bash
# Clone and build the WebUI
git clone https://github.com/EdgeFirstAI/webui.git
cd webui && npm install && npm run build

# Run websrv with WebUI
edgefirst-websrv --docroot /path/to/webui/dist
```

### Custom Web Content

Serve your own HTML/CSS/JavaScript by pointing `--docroot` to your content directory:

```bash
edgefirst-websrv --docroot /var/www/myapp
```

## Configuration

### SSL/TLS Certificates

The server requires HTTPS for browser features like WebGL and hardware video decoding.

**Certificate Priority**:
1. User-provided via `--cert` and `--key`
2. Existing certificate in `--cert-dir`
3. Auto-generated self-signed certificate (saved to `--cert-dir`)
4. Embedded fallback certificate

**Auto-generated certificates** include the device hostname in Subject Alternative Names (SANs), providing a good experience with mDNS `.local` hostnames.

```bash
# Use auto-generated certificate (default)
edgefirst-websrv --cert-dir /etc/edgefirst/ssl

# Use custom certificate
edgefirst-websrv --cert /path/to/cert.crt --key /path/to/key.pem

# Force regenerate certificate
edgefirst-websrv --generate-cert
```

### Ports

```bash
# Default (requires root)
edgefirst-websrv  # HTTPS:443, HTTP:80

# Non-privileged ports
edgefirst-websrv --https-port 8443 --http-port 8080
```

HTTP automatically redirects to HTTPS.

### Storage

```bash
# Set MCAP storage path
edgefirst-websrv --storage-path /data/recordings
```

### Environment Variables

All CLI options can be set via environment variables:

```bash
HTTPS_PORT=8443 HTTP_PORT=8080 CERT_DIR=/etc/ssl edgefirst-websrv
```

## CLI Reference

| Option | Environment | Default | Description |
|--------|-------------|---------|-------------|
| `--docroot` | `DOCROOT` | `/usr/share/webui` | Web document root |
| `--https-port` | `HTTPS_PORT` | `443` | HTTPS port |
| `--http-port` | `HTTP_PORT` | `80` | HTTP port (redirects to HTTPS) |
| `--cert-dir` | `CERT_DIR` | `/etc/edgefirst/ssl` | Certificate directory |
| `--cert` | `CERT` | - | Certificate file path |
| `--key` | `KEY` | - | Private key file path |
| `--generate-cert` | - | `false` | Force regenerate certificate |
| `--storage-path` | `STORAGE_PATH` | `.` | MCAP storage directory |
| `--system` | `SYSTEM` | `false` | Run in system mode |

Run `edgefirst-websrv --help` for full options.

## Documentation

- [ARCHITECTURE.md](ARCHITECTURE.md) - System design and component overview
- [TESTING.md](TESTING.md) - Testing guide and procedures
- [EdgeFirst Documentation](https://doc.edgefirst.ai/latest/maivin/) - Platform documentation

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Copyright 2025 Au-Zone Technologies Inc.

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE) for details.

## Security

For security vulnerabilities, see [SECURITY.md](SECURITY.md).
