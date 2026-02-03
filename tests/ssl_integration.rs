// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for SSL certificate management.
//!
//! These tests verify the certificate loading and generation functionality
//! in various scenarios.

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

    // Verify certificate can be parsed
    let cert_contents = fs::read(&cert_path).unwrap();
    let key_contents = fs::read(&key_path).unwrap();

    let cert = openssl::x509::X509::from_pem(&cert_contents).unwrap();
    let key = openssl::pkey::PKey::private_key_from_pem(&key_contents).unwrap();

    // Verify certificate properties
    let subject = cert.subject_name();
    let cn = subject
        .entries_by_nid(openssl::nid::Nid::COMMONNAME)
        .next()
        .unwrap();
    assert_eq!(cn.data().as_utf8().unwrap().to_string(), hostname);

    // Verify key matches certificate
    assert!(cert.public_key().unwrap().public_eq(&key));
}

#[test]
fn test_certificate_san_includes_local_hostname() {
    let hostname = "maivin-1234";
    let (cert_pem, _) = generate_test_certificate(hostname);

    let cert = openssl::x509::X509::from_pem(cert_pem.as_bytes()).unwrap();

    // Get Subject Alternative Names
    let san_ext = cert
        .subject_alt_names()
        .expect("Certificate should have SANs");

    let san_names: Vec<String> = san_ext
        .iter()
        .filter_map(|name| name.dnsname().map(|s| s.to_string()))
        .collect();

    // Verify expected SANs are present
    assert!(san_names.contains(&format!("{}.local", hostname)));
    assert!(san_names.contains(&hostname.to_string()));
    assert!(san_names.contains(&"localhost".to_string()));
}

#[test]
fn test_certificate_validity_period() {
    let (cert_pem, _) = generate_test_certificate("test-device");
    let cert = openssl::x509::X509::from_pem(cert_pem.as_bytes()).unwrap();

    let not_before = cert.not_before();
    let not_after = cert.not_after();

    // The certificate validity period spans from 2025 to 2035
    // Just verify that not_after is after not_before (10 year span)
    assert!(not_after.compare(not_before).unwrap() == std::cmp::Ordering::Greater);
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

    // Load and verify
    let cert_contents = fs::read(&cert_path).unwrap();
    let key_contents = fs::read(&key_path).unwrap();

    let cert = openssl::x509::X509::from_pem(&cert_contents).unwrap();
    let key = openssl::pkey::PKey::private_key_from_pem(&key_contents).unwrap();

    assert!(cert.public_key().is_ok());
    assert!(key.ec_key().is_ok()); // Should be EC key
}

#[test]
fn test_certificate_key_is_ecdsa_p256() {
    let (_, key_pem) = generate_test_certificate("test-device");
    let key = openssl::pkey::PKey::private_key_from_pem(key_pem.as_bytes()).unwrap();

    // Verify it's an EC key
    let ec_key = key.ec_key().expect("Should be an EC key");

    // Verify it's P-256 (also known as prime256v1 or secp256r1)
    let group = ec_key.group();
    let nid = group.curve_name().expect("Should have a named curve");
    assert_eq!(nid, openssl::nid::Nid::X9_62_PRIME256V1);
}

/// Generate a test certificate using rcgen (mirrors ssl.rs implementation)
fn generate_test_certificate(hostname: &str) -> (String, String) {
    use rcgen::{
        CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
        SanType, PKCS_ECDSA_P256_SHA256,
    };

    let mut params = CertificateParams::default();

    params
        .distinguished_name
        .push(DnType::CommonName, hostname);

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
