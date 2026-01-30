// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! HTTP Client Integration Tests for Studio API
//!
//! These tests exercise the HTTP API endpoints the same way the webui does,
//! making actual HTTP requests using reqwest (similar to browser fetch()).
//!
//! To run these tests, you need:
//! 1. A running websrv instance (either locally or on a device)
//! 2. Studio credentials set in environment variables:
//!    - STUDIO_SERVER (e.g., "test")
//!    - STUDIO_USERNAME
//!    - STUDIO_PASSWORD
//! 3. Optionally set WEBSRV_URL to point to your server (default: https://localhost)
//!
//! Run with: source env.sh && cargo test --test studio_api_client

use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use std::{env, time::Duration};

/// Base URL for the websrv server
fn get_base_url() -> String {
    env::var("WEBSRV_URL").unwrap_or_else(|_| "https://localhost".to_string())
}

/// Get test credentials from environment
fn get_credentials() -> Option<(String, String, String)> {
    let server = env::var("STUDIO_SERVER").ok()?;
    let username = env::var("STUDIO_USERNAME").ok()?;
    let password = env::var("STUDIO_PASSWORD").ok()?;
    Some((server, username, password))
}

/// Create an HTTP client that accepts self-signed certificates
fn create_client() -> Client {
    Client::builder()
        .danger_accept_invalid_certs(true) // For self-signed dev certs
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to create HTTP client")
}

/// Create an HTTP client with cookie store for session persistence
fn create_session_client() -> Client {
    Client::builder()
        .danger_accept_invalid_certs(true)
        .cookie_store(true)
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Failed to create HTTP client")
}

// ============================================================================
// API Response Types (matching server responses)
// ============================================================================

#[derive(Debug, Deserialize)]
struct AuthResponse {
    status: String,
    #[allow(dead_code)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct AuthStatusResponse {
    authenticated: bool,
    username: Option<String>,
}

#[derive(Debug, Serialize)]
struct LoginRequest {
    username: String,
    password: String,
    server: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ProjectInfo {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct LabelInfo {
    id: String,
    #[allow(dead_code)]
    name: String,
    #[serde(default)]
    default: bool,
}

#[derive(Debug, Deserialize)]
struct UploadErrorResponse {
    error: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UploadTaskInfo {
    upload_id: serde_json::Value,
    mcap_path: String,
    state: String,
    progress: f32,
    message: String,
}

// ============================================================================
// Authentication Tests
// ============================================================================

#[tokio::test]
async fn test_auth_status_unauthenticated() {
    let client = create_client();
    let base_url = get_base_url();

    let result = client
        .get(format!("{}/api/auth/status", base_url))
        .send()
        .await;

    match result {
        Ok(response) => {
            assert!(response.status().is_success());
            // Note: Server may have existing session, so we just verify the endpoint works
            let _status = response
                .json::<AuthStatusResponse>()
                .await
                .expect("Failed to parse JSON");
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
        }
    }
}

#[tokio::test]
async fn test_auth_login_invalid_credentials() {
    let client = create_client();
    let base_url = get_base_url();

    let result = client
        .post(format!("{}/api/auth/login", base_url))
        .json(&LoginRequest {
            username: "invalid_user_12345".to_string(),
            password: "invalid_password_67890".to_string(),
            server: "test".to_string(),
        })
        .send()
        .await;

    match result {
        Ok(response) => {
            assert_eq!(
                response.status(),
                401,
                "Invalid credentials should return 401"
            );
            let body = response
                .json::<AuthResponse>()
                .await
                .expect("Failed to parse JSON");
            assert_eq!(body.status, "error");
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
        }
    }
}

#[tokio::test]
async fn test_auth_login_valid_credentials() {
    let (server, username, password) = match get_credentials() {
        Some(creds) => creds,
        None => {
            eprintln!("Skipping test: STUDIO_* env vars not set");
            return;
        }
    };

    let client = create_client();
    let base_url = get_base_url();

    let result = client
        .post(format!("{}/api/auth/login", base_url))
        .json(&LoginRequest {
            username,
            password,
            server,
        })
        .send()
        .await;

    match result {
        Ok(response) => {
            assert!(response.status().is_success(), "Login should succeed");
            let body = response
                .json::<AuthResponse>()
                .await
                .expect("Failed to parse JSON");
            assert_eq!(body.status, "ok");
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
        }
    }
}

#[tokio::test]
async fn test_auth_full_flow() {
    let (server, username, password) = match get_credentials() {
        Some(creds) => creds,
        None => {
            eprintln!("Skipping test: STUDIO_* env vars not set");
            return;
        }
    };

    let client = create_session_client();
    let base_url = get_base_url();

    // 1. Login
    let result = client
        .post(format!("{}/api/auth/login", base_url))
        .json(&LoginRequest {
            username: username.clone(),
            password,
            server,
        })
        .send()
        .await;

    let response: Response = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
            return;
        }
    };

    assert!(response.status().is_success(), "Login should succeed");

    // 2. Check status - should be authenticated
    let response: Response = client
        .get(format!("{}/api/auth/status", base_url))
        .send()
        .await
        .expect("Failed to get status");

    assert!(response.status().is_success());
    let status = response
        .json::<AuthStatusResponse>()
        .await
        .expect("Failed to parse JSON");
    assert!(status.authenticated, "Should be authenticated after login");
    assert_eq!(status.username, Some(username));

    // 3. Logout
    let response: Response = client
        .post(format!("{}/api/auth/logout", base_url))
        .send()
        .await
        .expect("Failed to logout");

    assert!(response.status().is_success(), "Logout should succeed");

    // 4. Check status - should be unauthenticated
    let response: Response = client
        .get(format!("{}/api/auth/status", base_url))
        .send()
        .await
        .expect("Failed to get status");

    let status = response
        .json::<AuthStatusResponse>()
        .await
        .expect("Failed to parse JSON");
    assert!(
        !status.authenticated,
        "Should be unauthenticated after logout"
    );
}

// ============================================================================
// Studio Projects API Tests
// ============================================================================

#[tokio::test]
async fn test_studio_projects_requires_auth() {
    let client = create_session_client();
    let base_url = get_base_url();

    // First logout to ensure clean state
    let _ = client
        .post(format!("{}/api/auth/logout", base_url))
        .send()
        .await;

    // Try to get projects without auth
    let result = client
        .get(format!("{}/api/studio/projects", base_url))
        .send()
        .await;

    match result {
        Ok(response) => {
            assert_eq!(response.status(), 401, "Should require authentication");
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
        }
    }
}

#[tokio::test]
async fn test_studio_projects_authenticated() {
    let (server, username, password) = match get_credentials() {
        Some(creds) => creds,
        None => {
            eprintln!("Skipping test: STUDIO_* env vars not set");
            return;
        }
    };

    let client = create_session_client();
    let base_url = get_base_url();

    // Login first
    let result = client
        .post(format!("{}/api/auth/login", base_url))
        .json(&LoginRequest {
            username,
            password,
            server,
        })
        .send()
        .await;

    match result {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            eprintln!("Login failed with status: {}", r.status());
            return;
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
            return;
        }
    }

    // Get projects
    let response: Response = client
        .get(format!("{}/api/studio/projects", base_url))
        .send()
        .await
        .expect("Failed to get projects");

    assert!(response.status().is_success(), "Should return projects");

    let projects = response
        .json::<Vec<ProjectInfo>>()
        .await
        .expect("Failed to parse JSON");
    println!("Found {} projects", projects.len());
}

#[tokio::test]
async fn test_studio_labels_returns_coco_classes() {
    let (server, username, password) = match get_credentials() {
        Some(creds) => creds,
        None => {
            eprintln!("Skipping test: STUDIO_* env vars not set");
            return;
        }
    };

    let client = create_session_client();
    let base_url = get_base_url();

    // Login first
    let result = client
        .post(format!("{}/api/auth/login", base_url))
        .json(&LoginRequest {
            username,
            password,
            server,
        })
        .send()
        .await;

    match result {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            eprintln!("Login failed with status: {}", r.status());
            return;
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
            return;
        }
    }

    // Get labels (using dummy project ID - the endpoint returns static COCO labels)
    let response: Response = client
        .get(format!("{}/api/studio/projects/1/labels", base_url))
        .send()
        .await
        .expect("Failed to get labels");

    assert!(response.status().is_success());

    let labels = response
        .json::<Vec<LabelInfo>>()
        .await
        .expect("Failed to parse JSON");

    // Should have 80 COCO classes
    assert_eq!(labels.len(), 80, "Should return 80 COCO classes");

    // Person should be default
    let person = labels.iter().find(|l| l.id == "person");
    assert!(person.is_some(), "Should have 'person' label");
    assert!(person.unwrap().default, "Person should be default");

    // Should have common automotive classes
    assert!(labels.iter().any(|l| l.id == "car"), "Should have 'car'");
    assert!(
        labels.iter().any(|l| l.id == "truck"),
        "Should have 'truck'"
    );
    assert!(
        labels.iter().any(|l| l.id == "bicycle"),
        "Should have 'bicycle'"
    );
}

// ============================================================================
// Upload API Tests
// ============================================================================

#[tokio::test]
async fn test_uploads_list() {
    let client = create_client();
    let base_url = get_base_url();

    let result = client.get(format!("{}/api/uploads", base_url)).send().await;

    match result {
        Ok(response) => {
            assert!(response.status().is_success());
            let _uploads = response
                .json::<Vec<UploadTaskInfo>>()
                .await
                .expect("Failed to parse JSON");
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
        }
    }
}

#[tokio::test]
async fn test_upload_get_invalid_uuid() {
    let client = create_client();
    let base_url = get_base_url();

    let result = client
        .get(format!("{}/api/uploads/not-a-valid-uuid", base_url))
        .send()
        .await;

    match result {
        Ok(response) => {
            assert_eq!(response.status(), 400, "Invalid UUID should return 400");
            let body = response
                .json::<UploadErrorResponse>()
                .await
                .expect("Failed to parse JSON");
            assert!(body.error.to_lowercase().contains("invalid"));
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
        }
    }
}

#[tokio::test]
async fn test_upload_get_nonexistent() {
    let client = create_client();
    let base_url = get_base_url();

    let result = client
        .get(format!(
            "{}/api/uploads/12345678-1234-1234-1234-123456789abc",
            base_url
        ))
        .send()
        .await;

    match result {
        Ok(response) => {
            assert_eq!(
                response.status(),
                404,
                "Nonexistent upload should return 404"
            );
            let body = response
                .json::<UploadErrorResponse>()
                .await
                .expect("Failed to parse JSON");
            assert!(body.error.to_lowercase().contains("not found"));
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
        }
    }
}

#[tokio::test]
async fn test_upload_requires_auth() {
    let client = create_session_client();
    let base_url = get_base_url();

    // Logout to ensure clean state
    let _ = client
        .post(format!("{}/api/auth/logout", base_url))
        .send()
        .await;

    // Try to start an upload without auth
    let result = client
        .post(format!("{}/api/uploads", base_url))
        .json(&serde_json::json!({
            "mcap_path": "/some/path.mcap",
            "mode": { "type": "Basic" }
        }))
        .send()
        .await;

    match result {
        Ok(response) => {
            assert_eq!(response.status(), 400, "Should fail without auth");
            let body = response
                .json::<UploadErrorResponse>()
                .await
                .expect("Failed to parse JSON");
            assert!(
                body.error.to_lowercase().contains("authenticated")
                    || body.error.to_lowercase().contains("login"),
                "Error should mention authentication: {}",
                body.error
            );
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
        }
    }
}

// ============================================================================
// WebUI Flow Tests - Simulating exact webui behavior
// ============================================================================

/// This test simulates the exact login flow from webui/src/js/status.js
#[tokio::test]
async fn test_webui_login_flow() {
    let (server, username, password) = match get_credentials() {
        Some(creds) => creds,
        None => {
            eprintln!("Skipping test: STUDIO_* env vars not set");
            return;
        }
    };

    // Create client with cookie store (like browser)
    let client = create_session_client();
    let base_url = get_base_url();

    // Step 1: checkStudioAuthStatus() - GET /api/auth/status
    println!("Step 1: Checking auth status...");
    let result = client
        .get(format!("{}/api/auth/status", base_url))
        .send()
        .await;

    let response: Response = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
            return;
        }
    };

    let _status = response
        .json::<AuthStatusResponse>()
        .await
        .expect("Failed to parse status");

    // Step 2: Login via showStudioLoginDialog() - POST /api/auth/login
    println!("Step 2: Logging in...");
    let response: Response = client
        .post(format!("{}/api/auth/login", base_url))
        .header("Content-Type", "application/json")
        .json(&LoginRequest {
            username: username.clone(),
            password,
            server,
        })
        .send()
        .await
        .expect("Failed to login");

    assert!(response.status().is_success(), "Login should succeed");
    let login_resp = response
        .json::<AuthResponse>()
        .await
        .expect("Failed to parse login response");
    assert_eq!(login_resp.status, "ok");

    // Step 3: Verify checkStudioAuthStatus() returns authenticated
    println!("Step 3: Verifying authenticated status...");
    let response: Response = client
        .get(format!("{}/api/auth/status", base_url))
        .send()
        .await
        .expect("Failed to get status");

    let status = response
        .json::<AuthStatusResponse>()
        .await
        .expect("Failed to parse status");
    assert!(status.authenticated);
    assert_eq!(status.username, Some(username));

    println!("WebUI login flow completed successfully!");
}

/// This test simulates the upload dialog flow from webui/src/js/status.js
#[tokio::test]
async fn test_webui_upload_dialog_flow() {
    let (server, username, password) = match get_credentials() {
        Some(creds) => creds,
        None => {
            eprintln!("Skipping test: STUDIO_* env vars not set");
            return;
        }
    };

    let client = create_session_client();
    let base_url = get_base_url();

    // Step 1: Login first
    println!("Step 1: Logging in...");
    let result = client
        .post(format!("{}/api/auth/login", base_url))
        .json(&LoginRequest {
            username,
            password,
            server,
        })
        .send()
        .await;

    match result {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => {
            eprintln!("Login failed with status: {}", r.status());
            return;
        }
        Err(e) => {
            eprintln!("Skipping test - server not reachable: {}", e);
            return;
        }
    }

    // Step 2: populateProjectDropdown() - GET /api/studio/projects
    println!("Step 2: Fetching projects...");
    let response: Response = client
        .get(format!("{}/api/studio/projects", base_url))
        .send()
        .await
        .expect("Failed to get projects");

    assert!(response.status().is_success());
    let projects = response
        .json::<Vec<ProjectInfo>>()
        .await
        .expect("Failed to parse projects");
    println!("Found {} projects", projects.len());

    // Step 3: populateLabelCheckboxes() - GET /api/studio/projects/{id}/labels
    println!("Step 3: Fetching labels...");
    let response: Response = client
        .get(format!("{}/api/studio/projects/1/labels", base_url))
        .send()
        .await
        .expect("Failed to get labels");

    assert!(response.status().is_success());
    let labels = response
        .json::<Vec<LabelInfo>>()
        .await
        .expect("Failed to parse labels");

    assert_eq!(labels.len(), 80, "Should return 80 COCO classes");

    // Verify "person" is pre-selected (default: true)
    let person = labels.iter().find(|l| l.id == "person").unwrap();
    assert!(person.default, "Person should be default");

    println!("WebUI upload dialog flow completed successfully!");
}
