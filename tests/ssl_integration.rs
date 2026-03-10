// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for SSL certificate management.
//!
//! These tests verify the certificate loading and generation functionality
//! in various scenarios. Validates PEM format without requiring OpenSSL.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

#[test]
fn test_certificate_generation_creates_valid_files() {
    let temp_dir = TempDir::new().unwrap();
    let cert_dir = temp_dir.path().to_path_buf();

    // Generate a certificate using rcgen directly (simulating what ssl.rs does)
    let hostname = "test-device";
    let (cert_pem, key_pem) = generate_test_certificate(hostname);

    // Save to temp directory
    let cert_path = cert_dir.join("webui.crt");
    let key_path = cert_dir.join("webui.key");

    fs::write(&cert_path, &cert_pem).unwrap();
    fs::write(&key_path, &key_pem).unwrap();

    // Set permissions
    fs::set_permissions(&cert_path, fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();

    // Verify files exist and have correct permissions
    assert!(cert_path.exists());
    assert!(key_path.exists());

    let cert_perms = fs::metadata(&cert_path).unwrap().permissions().mode();
    let key_perms = fs::metadata(&key_path).unwrap().permissions().mode();

    // Check permission bits (last 9 bits)
    assert_eq!(cert_perms & 0o777, 0o644);
    assert_eq!(key_perms & 0o777, 0o600);

    // Verify PEM format
    let cert_contents = fs::read_to_string(&cert_path).unwrap();
    let key_contents = fs::read_to_string(&key_path).unwrap();
    assert!(cert_contents.contains("BEGIN CERTIFICATE"));
    assert!(cert_contents.contains("END CERTIFICATE"));
    assert!(key_contents.contains("BEGIN PRIVATE KEY"));
    assert!(key_contents.contains("END PRIVATE KEY"));
}

#[test]
fn test_certificate_pem_format_valid() {
    let (cert_pem, key_pem) = generate_test_certificate("test-device");

    // Verify PEM structure
    assert!(cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
    assert!(cert_pem.trim_end().ends_with("-----END CERTIFICATE-----"));
    assert!(key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
    assert!(key_pem.trim_end().ends_with("-----END PRIVATE KEY-----"));

    // Verify base64 content is present (PEM body should have content between markers)
    let cert_lines: Vec<&str> = cert_pem.lines().collect();
    assert!(
        cert_lines.len() > 2,
        "Certificate PEM should have content between markers"
    );

    let key_lines: Vec<&str> = key_pem.lines().collect();
    assert!(
        key_lines.len() > 2,
        "Key PEM should have content between markers"
    );

    // Verify rustls can actually parse the PEM (catches invalid base64,
    // unsupported key types like encrypted PKCS#8, etc.)
    let certs: Vec<_> = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .expect("rustls should parse certificate PEM");
    assert!(
        !certs.is_empty(),
        "At least one certificate should be present"
    );

    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .expect("rustls should parse key PEM")
        .expect("A usable private key should be present");

    // Verify we can build a ServerConfig (the actual runtime operation)
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("rustls ServerConfig should accept the generated cert+key");
}

#[test]
fn test_load_certificate_from_files() {
    let temp_dir = TempDir::new().unwrap();
    let cert_path = temp_dir.path().join("test.crt");
    let key_path = temp_dir.path().join("test.key");

    // Generate and save certificate
    let (cert_pem, key_pem) = generate_test_certificate("test-device");
    fs::write(&cert_path, &cert_pem).unwrap();
    fs::write(&key_path, &key_pem).unwrap();

    // Load and verify PEM format is preserved
    let cert_contents = fs::read_to_string(&cert_path).unwrap();
    let key_contents = fs::read_to_string(&key_path).unwrap();

    assert!(cert_contents.contains("BEGIN CERTIFICATE"));
    assert!(key_contents.contains("BEGIN PRIVATE KEY"));

    // Verify the content matches what was written
    assert_eq!(cert_contents, cert_pem);
    assert_eq!(key_contents, key_pem);
}

#[test]
fn test_certificate_is_unique_per_generation() {
    let (cert_pem_1, key_pem_1) = generate_test_certificate("device-1");
    let (cert_pem_2, key_pem_2) = generate_test_certificate("device-2");

    // Different hostnames should produce different certificates
    assert_ne!(cert_pem_1, cert_pem_2);
    assert_ne!(key_pem_1, key_pem_2);
}

/// Generate a test certificate using rcgen (mirrors ssl.rs implementation)
fn generate_test_certificate(hostname: &str) -> (String, String) {
    use rcgen::{
        CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
        SanType, PKCS_ECDSA_P256_SHA256,
    };

    let mut params = CertificateParams::default();

    params.distinguished_name.push(DnType::CommonName, hostname);

    params.subject_alt_names = vec![
        SanType::DnsName(format!("{}.local", hostname).try_into().unwrap()),
        SanType::DnsName(hostname.to_string().try_into().unwrap()),
        SanType::DnsName("localhost".to_string().try_into().unwrap()),
        SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1))),
        SanType::IpAddress(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
    ];

    params.not_before = rcgen::date_time_ymd(2025, 1, 1);
    params.not_after = rcgen::date_time_ymd(2035, 1, 1);
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];

    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();

    (cert.pem(), key_pair.serialize_pem())
}
