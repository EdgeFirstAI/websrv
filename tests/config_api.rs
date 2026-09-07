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

/// Guards writes to the shared config directory against the one test that
/// makes it briefly read-only (`a_failed_write_withholds_dispositions_and_unmatched`).
///
/// Tests in this binary run concurrently as threads sharing one process, and
/// `cargo test` proved that out: without this lock, that test's chmod window
/// intermittently broke unrelated tests with spurious permission-denied
/// writes and 500s. Every write-touching helper takes a read lock for the
/// span of its actual disk access; the one test that flips the directory
/// read-only takes the write lock for that same span, so the two can never
/// overlap.
static DIR_LOCK: OnceLock<tokio::sync::RwLock<()>> = OnceLock::new();

fn dir_lock() -> &'static tokio::sync::RwLock<()> {
    DIR_LOCK.get_or_init(|| tokio::sync::RwLock::new(()))
}

/// Seed a uniquely-named service config from a fixture and return its name.
async fn seed(service: &str, fixture: &str) -> String {
    let _guard = dir_lock().read().await;
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
    // Held across the request: `set_config` may write to `config_dir()`.
    let _guard = dir_lock().read().await;
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
    let svc = seed("apply", "lidarpub").await;
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
    let svc = seed("reject", "lidarpub").await;
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
    let svc = seed("noop", "lidarpub").await;
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

/// POST a raw body with an explicit (or absent) content type, so the tests
/// below can exercise the extractor's own rejection paths.
async fn post_raw(service: &str, content_type: Option<&str>, body: &str) -> (StatusCode, Value) {
    let _guard = dir_lock().read().await;
    let mut builder = Request::builder()
        .method("POST")
        .uri(format!("/api/config/{service}"));
    if let Some(value) = content_type {
        builder = builder.header("content-type", value);
    }
    let request = builder
        .body(Body::from(body.to_string()))
        .expect("build request");
    let response = app().oneshot(request).await.expect("handler ran");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// POST a raw body as JSON to a raw (possibly percent-encoded) URL segment.
async fn post_raw_json(raw_segment: &str, body: &str) -> (StatusCode, Value) {
    let _guard = dir_lock().read().await;
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/config/{raw_segment}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("build request");
    let response = app().oneshot(request).await.expect("handler ran");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn extractor_rejections_still_answer_in_the_json_contract() {
    // Json<Value> runs before the handler body, so its rejections used to
    // bypass ConfigWriteResponse entirely and answer in text/plain --
    // "JSON for every outcome" was not true for the cases a broken client
    // is most likely to hit.
    let svc = seed("rejection", "camera").await;

    let cases: [(Option<&str>, &str, StatusCode); 3] = [
        // Syntactically invalid JSON.
        (
            Some("application/json"),
            "{not json",
            StatusCode::BAD_REQUEST,
        ),
        // Missing content type.
        (
            None,
            r#"{"fileName":"x"}"#,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        // Wrong content type.
        (
            Some("text/plain"),
            r#"{"fileName":"x"}"#,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
    ];

    for (content_type, body, expected) in cases {
        let (status, parsed) = post_raw(&svc, content_type, body).await;
        assert_eq!(
            status, expected,
            "content_type={content_type:?} body={body:?}"
        );
        assert!(
            parsed.is_object(),
            "expected a JSON object, got {parsed} for content_type={content_type:?}"
        );
        assert_eq!(parsed["applied"], json!(false));
        assert!(
            parsed["error"].is_string(),
            "expected an error string, got {parsed}"
        );
    }
}

#[tokio::test]
async fn a_filename_disagreeing_with_the_url_is_rejected() {
    // The {service} path segment was inert: the handler read the target
    // exclusively from the body, so POST /api/config/camera could rewrite
    // recorder. Both name the same file or the request is refused.
    let target = seed("url-target", "camera").await;
    let other = seed("url-other", "recorder").await;
    let before = read(&other);

    let (status, body) = post_config(&target, json!({ "fileName": other, "A": "1" })).await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "got {body}");
    assert!(
        body["error"].is_string(),
        "expected an error string: {body}"
    );
    assert_eq!(read(&other), before, "the body's target was written anyway");
}

#[tokio::test]
async fn path_traversal_in_filename_is_rejected() {
    // The URL is percent-encoded so that the {service} segment decodes to the
    // same hostile string the body carries. Posting a mismatched pair instead
    // would prove nothing here: the URL cross-check would reject it before the
    // character whitelist ever ran, leaving the traversal guard untested.
    let cases = [
        ("%2E%2E%2Fetc%2Fpasswd", "../etc/passwd"),
        ("a%2Fb", "a/b"),
        ("%2E%2E", ".."),
        ("semi;colon", "semi;colon"),
    ];
    for (encoded, bad) in cases {
        let (status, body) =
            post_raw_json(encoded, &json!({ "fileName": bad, "A": "1" }).to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "accepted fileName {bad:?}");
        assert_eq!(body["error"], json!("invalid fileName"), "for {bad:?}");
    }

    // An empty fileName has no routable URL form, so it can only ever arrive
    // as a mismatch. Pinned separately: it must still be a 400, not a 500.
    let (status, _) = post_config("websrv-test-x", json!({ "fileName": "", "A": "1" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_filename_naming_a_directory_is_not_found_not_a_server_error() {
    // A fileName of plain alphanumerics passes the path-traversal whitelist,
    // and `Path::exists()` is true for directories too — so before Resolved
    // switched to `is_file()`, this resolved to the directory itself and
    // `read_to_string` on it returned EISDIR, a 500 that leaked the resolved
    // path. It must be a 404 instead, exactly like a name that resolves to
    // nothing at all.
    let name = "websrv-test-a-directory-not-a-file";
    {
        let _guard = dir_lock().read().await;
        std::fs::create_dir(config_dir().join(name)).expect("create directory");
    }

    let (status, body) = post_config(name, json!({ "fileName": name, "A": "1" })).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], json!("no config file"));
}

#[tokio::test]
async fn get_returns_the_written_values() {
    let svc = seed("get", "lidarpub").await;
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

/// GET the service map, then POST it straight back. A round trip through the
/// UI does exactly this, so anything GET emits must be something POST accepts.
async fn get_map(service: &str) -> Value {
    let request = Request::builder()
        .uri(format!("/api/config/{service}"))
        .body(Body::empty())
        .expect("build request");
    let response = app().oneshot(request).await.expect("handler ran");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    serde_json::from_slice(&bytes).expect("json")
}

#[tokio::test]
async fn a_semicolon_commented_key_does_not_break_the_get_post_round_trip() {
    // `;` is an EnvironmentFile comment prefix alongside `#`. When GET emitted
    // ";OLD_KEY" as a setting, posting the map back rejected the entire save,
    // because a save is all-or-nothing and ";OLD_KEY" is not a valid key.
    let svc = seed("semicolon", "camera").await;
    {
        let _guard = dir_lock().read().await;
        let path = config_dir().join(&svc);
        let mut content = std::fs::read_to_string(&path).expect("read seed");
        content.push_str(";OLD_KEY=\"retired\"\n");
        std::fs::write(&path, content).expect("append comment");
    }

    let mut map = get_map(&svc).await;
    assert!(
        map.get("OLD_KEY").is_none() && map.get(";OLD_KEY").is_none(),
        "a commented key must not surface as a setting, got {map}"
    );

    map.as_object_mut()
        .expect("object")
        .insert("fileName".to_string(), json!(svc));
    let (status, body) = post_config(&svc, map).await;
    assert_eq!(status, StatusCode::OK, "round trip rejected: {body}");

    assert!(
        read(&svc).contains(";OLD_KEY=\"retired\""),
        "the comment line must survive untouched"
    );
}

#[tokio::test]
async fn null_unsets_a_key_and_reports_it() {
    // RUST_LOG ships active in lidarpub.default (TARGET ships already
    // commented, which would make the "no active line" assertion vacuous).
    let svc = seed("unset", "lidarpub").await;
    let (status, body) = post_config(&svc, json!({ "fileName": svc, "RUST_LOG": null })).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dispositions"]["RUST_LOG"], json!("unset"));
    assert_eq!(body["restarted"], json!(false));

    let content = read(&svc);
    assert!(
        content.lines().any(|l| l.trim() == "#RUST_LOG="),
        "expected a commented #RUST_LOG= line, got: {content}"
    );
    assert!(
        !content
            .lines()
            .any(|l| l.trim_start().starts_with("RUST_LOG=")),
        "an active RUST_LOG= line remained: {content}"
    );
}

#[tokio::test]
async fn an_unmatched_key_is_appended_and_reported() {
    let svc = seed("append", "lidarpub").await;
    let (status, body) = post_config(
        &svc,
        json!({ "fileName": svc, "BRAND_NEW_KEY_TEST": "value" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["dispositions"]["BRAND_NEW_KEY_TEST"],
        json!("appended")
    );
    assert_eq!(body["unmatched"], json!(["BRAND_NEW_KEY_TEST"]));
    assert_eq!(body["restarted"], json!(false));

    let content = read(&svc);
    let appended = "# --- Added by edgefirst-websrv ---\nBRAND_NEW_KEY_TEST=\"value\"\n";
    assert!(
        content.contains(appended),
        "missing append marker: {content}"
    );
}

#[tokio::test]
async fn a_failed_write_withholds_dispositions_and_unmatched() {
    // Regression test for the fix in 51dc3f7: a write that fails after a real
    // plan was computed must not report dispositions/unmatched for an edit
    // that was never actually applied. Mutation-tested by QA: reordering the
    // dispositions/unmatched assignment to before `write_atomic` left the
    // whole suite green, so this pins the ordering directly.
    //
    // `write_atomic` fails because `NamedTempFile::new_in` cannot create its
    // sibling temp file: the shared config directory is made read-only for
    // the span of this one request, then restored immediately afterwards
    // (in a `finally`-style guard) so no other test in this binary is left
    // running against a read-only directory. This test holds `dir_lock`'s
    // WRITE side for that whole span, so it cannot overlap any other test's
    // read lock — see `dir_lock`'s doc comment for why that matters: without
    // it, this chmod window intermittently broke unrelated tests.
    use std::os::unix::fs::PermissionsExt;

    struct RestoreMode {
        dir: std::path::PathBuf,
        mode: u32,
    }
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(self.mode));
        }
    }

    let svc = seed("write-fails", "lidarpub").await;
    let dir = config_dir().to_path_buf();
    let original_mode = std::fs::metadata(&dir)
        .expect("stat dir")
        .permissions()
        .mode();
    let _restore = RestoreMode {
        dir: dir.clone(),
        mode: original_mode,
    };

    // Exclusive: excludes every reader (`seed`, `post_config`) for as long as
    // the directory is read-only. Built manually rather than through
    // `post_config`, which takes its own read lock and would deadlock
    // against the write lock held by this same thread.
    let _write_guard = dir_lock().write().await;
    // 0o500 (owner r-x, nothing for group or others) is the least permissive
    // mode that still lets this thread traverse the directory to read the
    // seeded file while denying the write `set_config` is about to attempt.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).expect("chmod ro");
    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/config/{svc}"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "fileName": svc, "CLUSTERING": "voxel" }).to_string(),
        ))
        .expect("build request");
    let response = app().oneshot(request).await.expect("handler ran");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("read body");
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    drop(_restore); // restore before any assertion can panic
    drop(_write_guard);

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["applied"], json!(false));
    assert!(body["error"].is_string(), "expected an error message");
    assert!(
        body.get("dispositions").is_none(),
        "dispositions must be withheld on a failed write, got {body}"
    );
    assert!(
        body.get("unmatched").is_none(),
        "unmatched must be withheld on a failed write, got {body}"
    );
}

#[tokio::test]
async fn atomic_write_leaves_no_stray_files() {
    let svc = seed("atomic", "camera").await;
    post_config(&svc, json!({ "fileName": svc, "RUST_LOG": "trace" })).await;

    // Exclusive for the scan only. Every other writer holds a read lock for
    // the span of its disk access, so taking the write side here waits until
    // no request is in flight. Without it this scan can catch a concurrent
    // test's `NamedTempFile` mid-write and report a leak that never happened.
    // Taken after the POST above, never across it: `post_config` acquires its
    // own read lock and would deadlock against a write lock held here.
    let _scan_guard = dir_lock().write().await;

    let strays: Vec<String> = std::fs::read_dir(config_dir())
        .expect("readdir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("websrv-test-"))
        .collect();
    assert!(strays.is_empty(), "stray temp files: {strays:?}");
}
