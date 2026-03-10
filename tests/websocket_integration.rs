// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the WebSocket ↔ Zenoh bridge.
//!
//! Spins up a minimal axum server with the `websocket_handler`, publishes
//! a Zenoh message on the subscribed topic, and verifies the WebSocket
//! client receives it.

use std::sync::Arc;

use axum::routing::get;
use axum::Router;
use edgefirst_websrv::websocket::{websocket_handler, MessageStream, WebSocketContext};
use futures::StreamExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

/// Minimal context that satisfies WebSocketContext for testing.
struct TestContext {
    err_stream: Arc<MessageStream>,
    zenoh_session: zenoh::Session,
    shutdown: CancellationToken,
}

impl WebSocketContext for TestContext {
    fn err_stream(&self) -> &Arc<MessageStream> {
        &self.err_stream
    }

    fn zenoh_session(&self) -> &zenoh::Session {
        &self.zenoh_session
    }

    fn shutdown_token(&self) -> Option<CancellationToken> {
        Some(self.shutdown.clone())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_websocket_receives_zenoh_message() {
    // Create a Zenoh session for both publisher and subscriber sides
    let zenoh_session = zenoh::open(zenoh::Config::default())
        .await
        .expect("Failed to open Zenoh session");

    let ctx = Arc::new(TestContext {
        err_stream: Arc::new(MessageStream::new(Box::new(|| {}))),
        zenoh_session: zenoh_session.clone(),
        shutdown: CancellationToken::new(),
    });

    // Build a minimal router with only the WebSocket route
    let app = Router::new()
        .route("/api/rt/{*topic}", get(websocket_handler::<TestContext>))
        .with_state(ctx.clone());

    // Bind to a random port
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind");
    let addr = listener.local_addr().unwrap();

    // Spawn the server
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Connect a WebSocket client to /api/rt/test/topic
    let url = format!("ws://{addr}/api/rt/test/topic");
    let (mut ws, _response) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("Failed to connect WebSocket");

    // Give the Zenoh subscriber a moment to set up
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Publish a message on the same topic via Zenoh
    let payload = b"hello from zenoh";
    zenoh_session
        .put("rt/test/topic", payload.as_slice())
        .await
        .expect("Failed to publish Zenoh message");

    // Wait for the WebSocket message with a timeout
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("Timed out waiting for WebSocket message")
        .expect("WebSocket stream ended")
        .expect("WebSocket error");

    match msg {
        Message::Binary(data) => {
            assert_eq!(data.as_slice(), payload, "Payload mismatch");
        }
        other => panic!("Expected binary message, got: {other:?}"),
    }

    // Clean shutdown
    let _ = ws.close(None).await;
    ctx.shutdown.cancel();
}
