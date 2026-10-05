// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `GET /api/topics/status` against a local Zenoh
//! session, with short sampling constants.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::get;
use axum::Router;
use edgefirst_websrv::topics::{
    topic_status_handler, BridgeObserver, LivenessConfig, TopicLiveness, TopicStatusContext,
    TopicStatusResponse,
};
use edgefirst_websrv::websocket::{websocket_handler, MessageStream, WebSocketContext};
use futures::StreamExt;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

struct TestContext {
    err_stream: Arc<MessageStream>,
    zenoh_session: zenoh::Session,
    liveness: Arc<TopicLiveness>,
}

impl WebSocketContext for TestContext {
    fn err_stream(&self) -> &Arc<MessageStream> {
        &self.err_stream
    }

    fn zenoh_session(&self) -> &zenoh::Session {
        &self.zenoh_session
    }

    fn bridge_observer(&self, topic: &str) -> Option<BridgeObserver> {
        self.liveness.bridge(topic)
    }
}

impl TopicStatusContext for TestContext {
    fn topic_liveness(&self) -> &Arc<TopicLiveness> {
        &self.liveness
    }
}

struct Server {
    base: String,
    addr: std::net::SocketAddr,
    session: zenoh::Session,
    liveness: Arc<TopicLiveness>,
    shutdown: CancellationToken,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn unique(prefix: &str) -> String {
    format!("test/{}/{prefix}", uuid::Uuid::new_v4().simple())
}

async fn start_server(builtin: &[&str], config: LivenessConfig) -> Server {
    let session = zenoh::open(zenoh::Config::default())
        .await
        .expect("Failed to open Zenoh session");
    let liveness = Arc::new(TopicLiveness::new(builtin, config));
    let ctx = Arc::new(TestContext {
        err_stream: Arc::new(MessageStream::new()),
        zenoh_session: session.clone(),
        liveness: liveness.clone(),
    });
    let app = Router::new()
        .route(
            "/api/topics/status",
            get(topic_status_handler::<TestContext>),
        )
        .route("/api/rt/{*topic}", get(websocket_handler::<TestContext>))
        .with_state(ctx);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let shutdown = CancellationToken::new();
    liveness.spawn(session.clone(), shutdown.clone());
    Server {
        base: format!("http://{addr}"),
        addr,
        session,
        liveness,
        shutdown,
    }
}

/// Publishes on `key` every 20 ms while `on` is set.
fn spawn_publisher(session: &zenoh::Session, key: &str, on: Arc<AtomicBool>) {
    let session = session.clone();
    let key = key.to_string();
    tokio::spawn(async move {
        loop {
            if on.load(Ordering::Relaxed) {
                session.put(&key, b"x".as_slice()).await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}

async fn status(base: &str, query: &str) -> TopicStatusResponse {
    let resp = reqwest::get(format!("{base}/api/topics/status{query}"))
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 200, "query {query:?}");
    assert_eq!(
        resp.headers()["content-type"].to_str().unwrap(),
        "application/json"
    );
    resp.json().await.expect("invalid JSON")
}

/// Polls the unfiltered listing until `pred` holds for `key`, or panics after `limit`.
async fn wait_for(
    base: &str,
    key: &str,
    limit: Duration,
    pred: impl Fn(&TopicStatusResponse) -> bool,
) -> TopicStatusResponse {
    let start = Instant::now();
    loop {
        let st = status(base, "").await;
        if pred(&st) {
            return st;
        }
        assert!(
            start.elapsed() < limit,
            "timed out waiting on {key}: {st:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn short_config() -> LivenessConfig {
    LivenessConfig {
        refresh: Duration::from_millis(1000),
        sample_window: Duration::from_millis(300),
        availability_window: Duration::from_millis(1500),
        request_expiry: Duration::from_millis(1500),
        max_topics: 64,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_sampling_and_periodic_refresh() {
    let alive = unique("alive");
    let silent = unique("silent");
    let on = Arc::new(AtomicBool::new(true));
    let session = zenoh::open(zenoh::Config::default()).await.unwrap();
    spawn_publisher(&session, &alive, on.clone());

    let server = start_server(&[&alive, &silent], short_config()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let st = status(&server.base, "").await;
    assert_eq!(st.refresh_ms, 1000);
    assert!(
        st.last_cycle_ms.is_some(),
        "start-up cycle completed: {st:?}"
    );
    assert_eq!(st.topics.len(), 2);
    let a = st.topics[&alive];
    assert!(a.available, "{a:?}");
    assert!(
        a.last_seen_ms.unwrap() <= st.last_cycle_ms.unwrap(),
        "the sighting is no older than the start of the cycle that produced it: {st:?}"
    );
    let s = st.topics[&silent];
    assert!(!s.available);
    assert_eq!(s.last_seen_ms, None);
    assert_eq!(
        server.liveness.active_samplers(),
        0,
        "no subscriber stays declared between cycles"
    );

    on.store(false, Ordering::Relaxed);
    let st = wait_for(&server.base, &alive, Duration::from_secs(4), |st| {
        !st.topics[&alive].available
    })
    .await;
    assert!(st.topics[&alive].last_seen_ms.unwrap() > 1500);

    on.store(true, Ordering::Relaxed);
    let st = wait_for(&server.base, &alive, Duration::from_millis(2500), |st| {
        st.topics[&alive].available
    })
    .await;
    assert!(st.topics[&alive].last_seen_ms.unwrap() < 1300);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribers_are_undeclared_after_the_first_sample() {
    let keys: Vec<String> = (0..3).map(|i| unique(&format!("busy{i}"))).collect();
    let silent = unique("silent");
    let on = Arc::new(AtomicBool::new(true));
    let session = zenoh::open(zenoh::Config::default()).await.unwrap();
    for key in &keys {
        spawn_publisher(&session, key, on.clone());
    }
    let mut builtin: Vec<&str> = keys.iter().map(String::as_str).collect();
    builtin.push(&silent);
    let config = LivenessConfig {
        refresh: Duration::from_secs(30),
        sample_window: Duration::from_secs(3),
        ..short_config()
    };

    let server = start_server(&builtin, config).await;
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        server.liveness.active_samplers(),
        1,
        "only the silent topic still waits for its window"
    );
    let st = status(&server.base, "").await;
    assert_eq!(st.last_cycle_ms, None, "cycle still running");
    for key in &keys {
        assert!(st.topics[key].available, "{key}: {:?}", st.topics[key]);
    }

    tokio::time::sleep(Duration::from_millis(3000)).await;
    assert_eq!(server.liveness.active_samplers(), 0);
    let st = status(&server.base, "").await;
    assert!(st.last_cycle_ms.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requested_unknown_topic_is_added_sampled_and_expires() {
    let builtin = unique("builtin");
    let extra = unique("extra");
    let on = Arc::new(AtomicBool::new(true));
    let session = zenoh::open(zenoh::Config::default()).await.unwrap();
    spawn_publisher(&session, &extra, on.clone());
    let config = LivenessConfig {
        refresh: Duration::from_secs(30),
        ..short_config()
    };

    let server = start_server(&[&builtin], config).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!status(&server.base, "").await.topics.contains_key(&extra));

    let first = status(&server.base, &format!("?topics={extra}")).await;
    assert_eq!(first.topics.len(), 1, "only the requested topics");
    assert!(!first.topics[&extra].available);
    assert_eq!(first.topics[&extra].last_seen_ms, None);

    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = status(&server.base, &format!("?topics={extra},{builtin}")).await;
    assert_eq!(second.topics.len(), 2);
    assert!(
        second.topics[&extra].available,
        "sampled without waiting for the 30 s refresh: {second:?}"
    );

    let listing = status(&server.base, "").await;
    assert!(listing.topics.contains_key(&extra));
    assert!(listing.topics.contains_key(&builtin));

    tokio::time::sleep(Duration::from_millis(1600)).await;
    let listing = status(&server.base, "").await;
    assert!(
        !listing.topics.contains_key(&extra),
        "expired after its last request: {listing:?}"
    );
    assert!(listing.topics.contains_key(&builtin));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bridge_observations_update_last_seen() {
    let key = unique("bridged");
    let config = LivenessConfig {
        refresh: Duration::from_secs(30),
        sample_window: Duration::from_millis(200),
        ..short_config()
    };
    let server = start_server(&[&key], config).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let st = status(&server.base, "").await;
    assert!(st.last_cycle_ms.is_some());
    assert_eq!(st.topics[&key].last_seen_ms, None);

    let url = format!("ws://{}/api/rt/{key}", server.addr);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect WebSocket");
    tokio::time::sleep(Duration::from_millis(300)).await;
    server
        .session
        .put(&key, b"bridged".as_slice())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("Timed out waiting for WebSocket message")
        .expect("WebSocket stream ended")
        .expect("WebSocket error");

    let st = status(&server.base, "").await;
    let t = st.topics[&key];
    assert!(t.available, "{t:?}");
    assert!(t.last_seen_ms.unwrap() < 1000, "{t:?}");
    assert_eq!(server.liveness.active_samplers(), 0);
    let _ = ws.close(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_bad_requests() {
    let builtin = unique("builtin");
    let server = start_server(&[&builtin], short_config()).await;
    let too_many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
    let long = "a".repeat(300);
    for query in [
        "?topics=".to_string(),
        "?topics=,".to_string(),
        "?topics=**".to_string(),
        "?topics=radar/*".to_string(),
        "?topics=%40/session".to_string(),
        "?topics=radar/%24*".to_string(),
        format!("?topics={}", too_many.join(",")),
        format!("?topics={long}"),
    ] {
        let resp = reqwest::get(format!("{}/api/topics/status{query}", server.base))
            .await
            .expect("request failed");
        assert_eq!(resp.status(), 400, "query {query:?}");
        let body: serde_json::Value = resp.json().await.expect("error body must be JSON");
        assert!(body["error"].is_string(), "query {query:?}: {body}");
    }
    let listing = status(&server.base, "").await;
    assert_eq!(listing.topics.len(), 1, "rejected requests add nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extra_sampling_never_delays_the_full_cycle() {
    let builtin = unique("builtin");
    let config = LivenessConfig {
        refresh: Duration::from_millis(1000),
        sample_window: Duration::from_millis(400),
        ..short_config()
    };
    let server = start_server(&[&builtin], config).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    let limit = 1000 + 400 + 100;
    let mut worst = 0;
    let start = Instant::now();
    let mut i = 0;
    while start.elapsed() < Duration::from_secs(5) {
        let fresh = unique(&format!("spam{i}"));
        i += 1;
        let st = status(&server.base, &format!("?topics={fresh}")).await;
        let age = st.last_cycle_ms.expect("a cycle has completed");
        worst = worst.max(age);
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(
        worst <= limit,
        "last_cycle_ms reached {worst} ms; a full cycle was delayed past its deadline"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cycle_is_not_recorded_when_every_declare_fails() {
    let session = zenoh::open(zenoh::Config::default()).await.unwrap();
    session.close().await.unwrap();
    let config = LivenessConfig {
        refresh: Duration::from_millis(200),
        sample_window: Duration::from_millis(50),
        ..short_config()
    };
    let shutdown = CancellationToken::new();

    let failing = Arc::new(TopicLiveness::new(
        &["test/closed/a", "test/closed/b"],
        config,
    ));
    failing.spawn(session.clone(), shutdown.clone());
    let empty = Arc::new(TopicLiveness::new(&[], config));
    empty.spawn(session, shutdown.clone());

    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        failing.status(None).last_cycle_ms,
        None,
        "no cycle counts as completed when nothing could be sampled"
    );
    assert!(
        empty.status(None).last_cycle_ms.is_some(),
        "a cycle with nothing to sample still completes"
    );
    shutdown.cancel();
}
