// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! SSL/TLS Certificate Management
//!
//! This module handles certificate loading and generation for the HTTPS server.
//! It supports three modes of operation:
//!
//! 1. **User-provided certificates**: Specified via `--cert` and `--key` CLI arguments
//! 2. **Auto-generated certificates**: Created on first run and persisted to `--cert-dir`
//! 3. **Embedded fallback**: Built-in certificate for development/testing
//!
//! Generated certificates include the device hostname in Subject Alternative Names (SANs)
//! to provide a good user experience with mDNS `.local` hostnames.

use std::fs::{self, File, Permissions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use log::{info, warn};
use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
    PKCS_ECDSA_P256_SHA256,
};

use crate::Args;

/// Default certificate filename
const CERT_FILENAME: &str = "webui.crt";
/// Default private key filename
const KEY_FILENAME: &str = "webui.key";

/// Certificate source for logging purposes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateSource {
    /// User-provided certificate via CLI arguments
    UserProvided,
    /// Certificate loaded from cert-dir
    CertDir,
    /// Newly generated self-signed certificate
    Generated,
    /// Embedded fallback certificate
    Embedded,
}

impl std::fmt::Display for CertificateSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CertificateSource::UserProvided => write!(f, "user-provided"),
            CertificateSource::CertDir => write!(f, "cert-dir"),
            CertificateSource::Generated => write!(f, "auto-generated"),
            CertificateSource::Embedded => write!(f, "embedded fallback"),
        }
    }
}

/// Result of certificate loading/generation
pub struct CertificateResult {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    pub source: CertificateSource,
}

/// Load or generate SSL certificates based on CLI arguments.
///
/// Priority chain:
/// 1. `--cert`/`--key` CLI arguments (if both provided)
/// 2. Files in `--cert-dir` (webui.crt, webui.key)
/// 3. Auto-generate and save to `--cert-dir`
/// 4. Fallback to embedded certificate (with warning)
pub fn load_or_generate_certificate(args: &Args) -> anyhow::Result<CertificateResult> {
    // 1. Check for user-provided certificate paths
    if let (Some(cert_path), Some(key_path)) = (&args.cert, &args.key) {
        info!("Loading user-provided certificate from {:?}", cert_path);
        let (cert_pem, key_pem) = load_certificate_from_files(cert_path, key_path)?;
        return Ok(CertificateResult {
            cert_pem,
            key_pem,
            source: CertificateSource::UserProvided,
        });
    }

    // 2. Check for existing certificate in cert-dir (unless --generate-cert)
    let cert_path = args.cert_dir.join(CERT_FILENAME);
    let key_path = args.cert_dir.join(KEY_FILENAME);

    if !args.generate_cert && cert_path.exists() && key_path.exists() {
        info!("Loading certificate from {:?}", args.cert_dir);
        match load_certificate_from_files(&cert_path, &key_path) {
            Ok((cert_pem, key_pem)) => {
                return Ok(CertificateResult {
                    cert_pem,
                    key_pem,
                    source: CertificateSource::CertDir,
                });
            }
            Err(e) => {
                warn!("Failed to load certificate from cert-dir: {}", e);
                // Continue to generation/fallback
            }
        }
    }

    // 3. Try to generate and save a new certificate
    let hostname = get_device_hostname();
    info!(
        "Generating self-signed certificate for hostname: {}",
        hostname
    );

    match generate_self_signed_certificate(&hostname) {
        Ok((cert_pem, key_pem)) => {
            // Try to save to cert-dir
            if save_certificate_to_dir(&args.cert_dir, &cert_pem, &key_pem).is_ok() {
                info!("Saved certificate to {:?}", args.cert_dir);
            } else {
                warn!(
                    "Could not save certificate to {:?} (using in-memory only)",
                    args.cert_dir
                );
            }

            return Ok(CertificateResult {
                cert_pem: cert_pem.into_bytes(),
                key_pem: key_pem.into_bytes(),
                source: CertificateSource::Generated,
            });
        }
        Err(e) => {
            warn!("Failed to generate certificate: {}", e);
            // Continue to fallback
        }
    }

    // 4. Fallback to embedded certificate
    warn!("Using embedded fallback certificate (not recommended for production)");
    let (cert_pem, key_pem) = load_embedded_certificate()?;
    Ok(CertificateResult {
        cert_pem,
        key_pem,
        source: CertificateSource::Embedded,
    })
}

/// Load certificate and private key from PEM files.
pub fn load_certificate_from_files(
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let cert_pem = fs::read(cert_path)?;
    let key_pem = fs::read(key_path)?;
    Ok((cert_pem, key_pem))
}

/// Generate a self-signed certificate for the given hostname.
///
/// The certificate includes:
/// - Subject CN: hostname
/// - SANs: hostname.local, hostname, localhost, 127.0.0.1, ::1
/// - Validity: 10 years
/// - Key: ECDSA P-256
pub fn generate_self_signed_certificate(hostname: &str) -> anyhow::Result<(String, String)> {
    let mut params = CertificateParams::default();

    // Set subject distinguished name
    params.distinguished_name.push(DnType::CommonName, hostname);

    // Set Subject Alternative Names for various access methods
    params.subject_alt_names = vec![
        SanType::DnsName(format!("{}.local", hostname).try_into()?),
        SanType::DnsName(hostname.to_string().try_into()?),
        SanType::DnsName("localhost".to_string().try_into()?),
        SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
        SanType::IpAddress(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
    ];

    // Set validity period (10 years)
    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2035, 1, 1);

    // Configure as end-entity certificate (not a CA)
    params.is_ca = IsCa::NoCa;

    // Set key usage
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];

    // Set extended key usage for TLS server
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    // Generate ECDSA P-256 key pair
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;

    // Generate the certificate
    let cert = params.self_signed(&key_pair)?;

    Ok((cert.pem(), key_pair.serialize_pem()))
}

/// Save certificate and key to the specified directory.
fn save_certificate_to_dir(dir: &Path, cert_pem: &str, key_pem: &str) -> anyhow::Result<()> {
    // Create directory if it doesn't exist
    fs::create_dir_all(dir)?;

    let cert_path = dir.join(CERT_FILENAME);
    let key_path = dir.join(KEY_FILENAME);

    // Write certificate (world-readable)
    let mut cert_file = File::create(&cert_path)?;
    cert_file.write_all(cert_pem.as_bytes())?;
    fs::set_permissions(&cert_path, Permissions::from_mode(0o644))?;

    // Write private key (owner-only)
    let mut key_file = File::create(&key_path)?;
    key_file.write_all(key_pem.as_bytes())?;
    fs::set_permissions(&key_path, Permissions::from_mode(0o600))?;

    Ok(())
}

/// Load the embedded fallback certificate.
///
/// Generates a self-signed certificate at runtime instead of using a static
/// embedded PEM, since rustls does not support encrypted private keys.
fn load_embedded_certificate() -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let (cert_pem, key_pem) = generate_self_signed_certificate("localhost")?;
    Ok((cert_pem.into_bytes(), key_pem.into_bytes()))
}

/// Get the device hostname for certificate generation.
pub fn get_device_hostname() -> String {
    hostname::get()
        .unwrap_or_else(|_| "localhost".into())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_device_hostname() {
        let hostname = get_device_hostname();
        assert!(!hostname.is_empty());
    }

    #[test]
    fn test_generate_self_signed_certificate() {
        let (cert_pem, key_pem) = generate_self_signed_certificate("test-device").unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(cert_pem.contains("END CERTIFICATE"));
        assert!(key_pem.contains("BEGIN PRIVATE KEY"));
        assert!(key_pem.contains("END PRIVATE KEY"));
    }

    #[test]
    fn test_certificate_source_display() {
        assert_eq!(CertificateSource::UserProvided.to_string(), "user-provided");
        assert_eq!(CertificateSource::CertDir.to_string(), "cert-dir");
        assert_eq!(CertificateSource::Generated.to_string(), "auto-generated");
        assert_eq!(CertificateSource::Embedded.to_string(), "embedded fallback");
    }

    #[test]
    fn test_load_embedded_certificate() {
        let (cert, key) = load_embedded_certificate().unwrap();
        assert!(String::from_utf8_lossy(&cert).contains("BEGIN CERTIFICATE"));
        assert!(String::from_utf8_lossy(&key).contains("BEGIN PRIVATE KEY"));
    }
}
