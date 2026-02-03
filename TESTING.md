# Testing Guide

This document covers testing procedures for the EdgeFirst WebSrv project, including automated tests and manual verification with the WebUI.

## Quick Start

```bash
# Run all tests
cargo test

# Run tests with nextest (faster, better output)
cargo nextest run

# Run specific test module
cargo test --test ssl_integration
cargo test --bin edgefirst-websrv ssl::
```

## Test Categories

### Unit Tests

Unit tests are embedded in the source files using `#[cfg(test)]` modules.

**Location**: `src/*.rs`

**Run**:
```bash
cargo test --lib
cargo test --bin edgefirst-websrv
```

**Coverage**:
- `src/ssl.rs` - Certificate generation, loading, hostname extraction
- `src/upload.rs` - Upload state management
- `src/auth.rs` - Authentication token handling

### Integration Tests

Integration tests verify component interactions and end-to-end functionality.

**Location**: `tests/*.rs`

**Run**:
```bash
cargo test --test ssl_integration
cargo test --test studio_api_client
```

**Available tests**:
- `ssl_integration.rs` - Certificate generation and validation
- `studio_api_client.rs` - EdgeFirst Studio API integration

## Certificate Testing

### Automated Certificate Tests

The SSL module includes comprehensive tests for certificate management:

```bash
# Run all certificate tests
cargo test ssl

# Run integration tests for certificates
cargo test --test ssl_integration
```

**Test coverage**:
- Certificate generation produces valid X.509
- SANs include hostname.local, hostname, localhost
- ECDSA P-256 key generation
- File permissions (cert: 0644, key: 0600)
- Certificate loading from PEM files

### Manual Certificate Testing

#### 1. Test Auto-Generation (Default)

```bash
# Create a temporary certificate directory
mkdir -p /tmp/test-ssl

# Run server with custom cert-dir
cargo run -- --cert-dir /tmp/test-ssl --https-port 8443 --http-port 8080

# Verify certificate was generated
ls -la /tmp/test-ssl/
# Should show:
#   webui.crt (0644)
#   webui.key (0600)

# Inspect the certificate
openssl x509 -in /tmp/test-ssl/webui.crt -text -noout
```

#### 2. Test User-Provided Certificate

```bash
# Generate a custom certificate
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 \
  -keyout custom.key -out custom.crt -days 365 -nodes \
  -subj "/CN=my-device"

# Run server with custom certificate
cargo run -- --cert custom.crt --key custom.key --https-port 8443
```

#### 3. Test Certificate Regeneration

```bash
# Force regenerate certificate
cargo run -- --cert-dir /tmp/test-ssl --generate-cert --https-port 8443

# Verify new certificate was created (check timestamp)
ls -la /tmp/test-ssl/
```

#### 4. Test Fallback to Embedded Certificate

```bash
# Run with read-only directory (will fall back to embedded)
cargo run -- --cert-dir /nonexistent --https-port 8443

# Should see warning in logs:
# "Using embedded fallback certificate (not recommended for production)"
```

## Manual Testing with WebUI

The [EdgeFirstAI/webui](https://github.com/EdgeFirstAI/webui) project provides the default HTML/CSS/JavaScript interface for testing the websrv.

### Setup

```bash
# Clone the WebUI project
git clone https://github.com/EdgeFirstAI/webui.git
cd webui

# Build the WebUI (if needed)
npm install
npm run build

# The built files are in dist/
```

### Running with WebUI

```bash
# From the websrv directory
cargo run -- \
  --docroot /path/to/webui/dist \
  --cert-dir /tmp/test-ssl \
  --https-port 8443 \
  --http-port 8080
```

### Browser Testing

1. **Navigate to the server**:
   ```
   https://localhost:8443
   # or using hostname
   https://$(hostname).local:8443
   ```

2. **Accept the self-signed certificate**:
   - Chrome: Click "Advanced" → "Proceed to localhost (unsafe)"
   - Firefox: Click "Advanced" → "Accept the Risk and Continue"
   - Safari: Click "Show Details" → "visit this website"

3. **Verify certificate details**:
   - Click the lock/warning icon in the address bar
   - Verify the certificate shows:
     - Subject CN matches hostname
     - SANs include hostname.local
     - Valid for ~10 years

4. **Test WebUI functionality**:
   - Verify the page loads correctly
   - Check WebSocket connections work (real-time updates)
   - Test any HTTPS-required features (WebGL, video decoding)

### Testing on Actual Devices

For testing on Maivin devices with mDNS:

1. **Find the device**:
   ```bash
   avahi-browse -art | grep maivin
   # or
   ping maivin-XXXX.local
   ```

2. **Access via mDNS hostname**:
   ```
   https://maivin-XXXX.local
   ```

3. **Verify certificate matches hostname**:
   - Certificate CN should be `maivin-XXXX`
   - SANs should include `maivin-XXXX.local`

## Troubleshooting

### Certificate Errors

**"Certificate not yet valid"**:
- Check system clock is correct
- Certificate not_before date may be in the future

**"Certificate expired"**:
- Regenerate with `--generate-cert`
- Check system clock

**"Certificate hostname mismatch"**:
- Access using the hostname in the certificate
- Check `openssl x509 -in cert.crt -text` for SANs

### Permission Errors

**"Permission denied" when saving certificate**:
- Run as root, or
- Use a writable `--cert-dir` path
- Server will fall back to embedded certificate

**"Failed to read private key"**:
- Check key file permissions (should be readable by server process)
- Verify key file is valid PEM format

### WebSocket Issues

**WebSocket connection failed**:
- Verify HTTPS is working first
- Check browser console for mixed-content errors
- Ensure certificate is accepted by browser

## CI/CD Testing

The project uses GitHub Actions for automated testing:

- **test.yml**: Runs unit and integration tests on x86_64 and aarch64
- **sbom.yml**: Validates license compliance and NOTICE file

See `.github/workflows/` for workflow definitions.
