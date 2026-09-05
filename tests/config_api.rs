// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Handler-level tests for the service configuration API.
//!
//! The config directory is a process-wide `OnceLock`, so this binary sets it
//! once to a shared temp directory and every test uses a distinct service name
//! inside it. Service names are prefixed `websrv-test-` so they can never
//! match a loaded systemd unit on the machine running the tests.

use std::path::Path;
use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Router;
use edgefirst_websrv::config::{get_config, init_config_dir, set_config};
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::ServiceExt;

static TEST_DIR: OnceLock<TempDir> = OnceLock::new();

/// The shared config directory for this test binary.
fn config_dir() -> &'static Path {
    TEST_DIR
        .get_or_init(|| {
            let dir = tempfile::tempdir().expect("create temp config dir");
            init_config_dir(dir.path().to_path_buf());
            dir
        })
        .path()
}

/// Seed a uniquely-named service config from a fixture and return its name.
fn seed(service: &str, fixture: &str) -> String {
    let name = format!("websrv-test-{service}");
    let source = format!(
        "{}/tests/fixtures/{fixture}.default",
        env!("CARGO_MANIFEST_DIR")
    );
    let content = std::fs::read_to_string(&source).unwrap_or_else(|e| panic!("{source}: {e}"));
    std::fs::write(config_dir().join(&name), content).expect("seed config");
    name
}

fn app() -> Router {
    Router::new().route("/api/config/{service}", get(get_config).post(set_config))
}

/// POST a JSON body and return the status and parsed response.
async fn post_config(service: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/config/{service}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");

    let response = app().oneshot(request).await.expect("handler ran");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let parsed = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

fn read(service: &str) -> String {
    std::fs::read_to_string(config_dir().join(service)).expect("read config")
}

#[tokio::test]
async fn applies_a_real_change_and_reports_dispositions() {
    let svc = seed("apply", "lidarpub");
    let (status, body) = post_config(
        &svc,
        json!({ "fileName": svc, "CLUSTERING": "voxel", "CLUSTERING_EPS": 250 }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["applied"], json!(true));
    assert_eq!(body["dispositions"]["CLUSTERING"], json!("inserted"));
    assert_eq!(body["dispositions"]["CLUSTERING_EPS"], json!("inserted"));
    assert_eq!(body["reserved"], json!(["fileName"]));

    let content = read(&svc);
    assert!(content.contains("CLUSTERING=\"voxel\"\n"));
    // The number must not be blanked, per EDGEAI-1402.
    assert!(content.contains("CLUSTERING_EPS=\"250\"\n"));
    assert!(
        !content.contains("FILENAME="),
        "fileName leaked into the file"
    );
}

#[tokio::test]
async fn rejects_the_whole_request_and_leaves_the_file_untouched() {
    let svc = seed("reject", "lidarpub");
    let before = read(&svc);

    let (status, body) = post_config(
        &svc,
        json!({ "fileName": svc, "RUST_LOG": "debug", "TARGET": "a\nEVIL=pwned" }),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["applied"], json!(false));
    assert!(body["rejected"]["TARGET"].is_object());
    assert!(body["rejected"].get("RUST_LOG").is_none());
    assert_eq!(read(&svc), before, "file must not change on rejection");
}

#[tokio::test]
async fn a_body_that_is_not_an_object_is_a_bad_request() {
    let (status, _) = post_config("websrv-test-anything", json!(["not", "an", "object"])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_missing_config_file_is_not_found() {
    let (status, body) = post_config(
        "websrv-test-absent",
        json!({ "fileName": "websrv-test-absent", "A": "1" }),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], json!("no config file"));
    let tried = body["tried"].as_array().expect("tried list");
    assert_eq!(tried.len(), 2);
}

#[tokio::test]
async fn an_unchanged_save_writes_nothing() {
    let svc = seed("noop", "lidarpub");
    // lidarpub.default already reads RUST_LOG="info" in canonical form, so
    // both saves are no-ops. The first proves a save of an identical value
    // does not rewrite; the second proves it stays that way.
    let payload = json!({ "fileName": svc, "RUST_LOG": "info" });
    let (first, _) = post_config(&svc, payload.clone()).await;
    assert_eq!(first, StatusCode::OK);

    let before = read(&svc);
    let modified_at = std::fs::metadata(config_dir().join(&svc))
        .expect("stat")
        .modified()
        .expect("mtime");

    let (status, body) = post_config(&svc, payload).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["applied"], json!(false));
    assert_eq!(body["reason"], json!("no changes"));
    assert_eq!(body["restarted"], json!(false));
    assert_eq!(read(&svc), before);
    assert_eq!(
        std::fs::metadata(config_dir().join(&svc))
            .expect("stat")
            .modified()
            .expect("mtime"),
        modified_at,
        "file was rewritten despite no changes"
    );
}

#[tokio::test]
async fn path_traversal_in_filename_is_rejected() {
    for bad in ["../etc/passwd", "a/b", "..", "semi;colon"] {
        let (status, _) = post_config("websrv-test-x", json!({ "fileName": bad, "A": "1" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted fileName {bad:?}");
    }
}

#[tokio::test]
async fn get_returns_the_written_values() {
    let svc = seed("get", "lidarpub");
    post_config(&svc, json!({ "fileName": svc, "CLUSTERING": "dbscan" })).await;

    let request = Request::builder()
        .uri(format!("/api/config/{svc}"))
        .body(Body::empty())
        .expect("build request");
    let response = app().oneshot(request).await.expect("handler ran");
    assert_eq!(response.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let body: Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(body["CLUSTERING"], json!("dbscan"));
}

#[tokio::test]
async fn atomic_write_leaves_no_stray_files() {
    let svc = seed("atomic", "camera");
    post_config(&svc, json!({ "fileName": svc, "RUST_LOG": "trace" })).await;

    let strays: Vec<String> = std::fs::read_dir(config_dir())
        .expect("readdir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("websrv-test-"))
        .collect();
    assert!(strays.is_empty(), "stray temp files: {strays:?}");
}
