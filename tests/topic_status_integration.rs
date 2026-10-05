// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `GET /api/topics/status` against a local Zenoh session.

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use edgefirst_websrv::topics::{
    topic_status_handler, TopicStatusContext, TopicStatusResponse, TopicWatches,
    AVAILABILITY_THRESHOLD,
};
use edgefirst_websrv::websocket::{MessageStream, WebSocketContext};
use tokio::net::TcpListener;

struct TestContext {
    err_stream: Arc<MessageStream>,
    zenoh_session: zenoh::Session,
    topic_watches: Arc<TopicWatches>,
}

impl WebSocketContext for TestContext {
    fn err_stream(&self) -> &Arc<MessageStream> {
        &self.err_stream
    }

    fn zenoh_session(&self) -> &zenoh::Session {
        &self.zenoh_session
    }
}

impl TopicStatusContext for TestContext {
    fn topic_watches(&self) -> &Arc<TopicWatches> {
        &self.topic_watches
    }
}

async fn start_server() -> (String, zenoh::Session, Arc<TestContext>) {
    let zenoh_session = zenoh::open(zenoh::Config::default())
        .await
        .expect("Failed to open Zenoh session");
    let ctx = Arc::new(TestContext {
        err_stream: Arc::new(MessageStream::new()),
        zenoh_session: zenoh_session.clone(),
        topic_watches: Arc::new(TopicWatches::default()),
    });
    let app = Router::new()
        .route(
            "/api/topics/status",
            get(topic_status_handler::<TestContext>),
        )
        .with_state(ctx.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), zenoh_session, ctx)
}

async fn status(base: &str, topics: &str) -> TopicStatusResponse {
    let resp = reqwest::get(format!("{base}/api/topics/status?topics={topics}"))
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "application/json"
    );
    resp.json().await.expect("invalid JSON")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_status_tracks_sample_arrival() {
    let (base, session, ctx) = start_server().await;
    let key = "test/topic_status/radar";

    let publisher = session.declare_publisher(key).await.unwrap();
    publisher.put(b"before watch".as_slice()).await.unwrap();

    let first = status(&base, &format!("{key},test/topic_status/lidar")).await;
    assert_eq!(first.topics.len(), 2);
    for (name, st) in &first.topics {
        assert!(!st.available, "{name} should start unavailable");
        assert_eq!(st.age_ms, None, "{name} should have no age yet");
    }
    assert_eq!(ctx.topic_watches.len(), 2);

    tokio::time::sleep(Duration::from_millis(200)).await;
    publisher.put(b"sample".as_slice()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let second = status(&base, &format!("{key},test/topic_status/lidar")).await;
    let radar = second.topics[key];
    assert!(radar.available, "radar should be available: {radar:?}");
    assert!(radar.age_ms.unwrap() < 1000, "age too large: {radar:?}");
    let lidar = second.topics["test/topic_status/lidar"];
    assert!(!lidar.available);
    assert_eq!(lidar.age_ms, None);
    assert_eq!(
        ctx.topic_watches.len(),
        2,
        "no per-request subscriber churn"
    );

    tokio::time::sleep(AVAILABILITY_THRESHOLD + Duration::from_millis(300)).await;
    let third = status(&base, key).await;
    let radar = third.topics[key];
    assert!(!radar.available, "radar should be stale: {radar:?}");
    assert!(radar.age_ms.unwrap() > AVAILABILITY_THRESHOLD.as_millis() as u64);

    ctx.topic_watches.clear();
    assert!(ctx.topic_watches.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_status_rejects_bad_requests() {
    let (base, _session, ctx) = start_server().await;
    let too_many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
    let long = "a".repeat(300);
    for query in [
        "".to_string(),
        "?topics=".to_string(),
        "?topics=**".to_string(),
        "?topics=radar/*".to_string(),
        "?topics=%40/session".to_string(),
        "?topics=radar/%24*".to_string(),
        format!("?topics={}", too_many.join(",")),
        format!("?topics={long}"),
    ] {
        let resp = reqwest::get(format!("{base}/api/topics/status{query}"))
            .await
            .expect("request failed");
        assert_eq!(resp.status(), 400, "query {query:?}");
        let body: serde_json::Value = resp.json().await.expect("error body must be JSON");
        assert!(body["error"].is_string(), "query {query:?}: {body}");
    }
    assert!(
        ctx.topic_watches.is_empty(),
        "rejected requests must not watch"
    );
}
