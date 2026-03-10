// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! WebSocket handling for real-time streaming.
//!
//! Provides:
//! - Message broadcasting to WebSocket clients via tokio::sync::broadcast
//! - Zenoh subscription integration
//! - yawc-based WebSocket connections with RFC 7692 compression

use axum::extract::State;
use axum::response::IntoResponse;

use futures::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;
use yawc::{
    frame::{Frame, OpCode},
    IncomingUpgrade, Options,
};

/// Timeout for sending a single WebSocket frame to a client.
/// If a client isn't reading within this window, the connection is dropped.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between server-initiated WebSocket ping frames.
/// Detects dead connections when no data is flowing.
const PING_INTERVAL: Duration = Duration::from_secs(30);

// ============================================================================
// Message Types
// ============================================================================

/// WebSocket broadcast message - Binary for CDR data, Text for JSON
#[derive(Clone)]
pub enum Broadcast {
    Binary(Vec<u8>),
    Text(String),
}

/// WebSocket message for topic subscription
#[derive(Debug, Serialize, Deserialize)]
pub struct WebSocketMessage {
    pub action: String,
    pub topic: String,
}

// ============================================================================
// Message Stream
// ============================================================================

/// Manages WebSocket message broadcasting via tokio::sync::broadcast channels
pub struct MessageStream {
    tx: broadcast::Sender<Broadcast>,
}

impl Default for MessageStream {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageStream {
    /// Create a new MessageStream with a broadcast channel
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        MessageStream { tx }
    }

    /// Subscribe to the broadcast channel
    pub fn subscribe(&self) -> broadcast::Receiver<Broadcast> {
        self.tx.subscribe()
    }

    /// Broadcast a message to all subscribers.
    ///
    /// Note: `broadcast::Sender::send` returns `Err` only when there are
    /// **no active receivers** (e.g., before any client subscribes).  This is
    /// not the same as a dropped/lagged message — that is signaled by
    /// `RecvError::Lagged(n)` on the receiver side.  We silently ignore the
    /// no-subscriber case since it is expected during startup.
    pub fn broadcast(&self, message: Broadcast) {
        let _ = self.tx.send(message);
    }
}

// ============================================================================
// Zenoh Integration
// ============================================================================

/// Zenoh subscriber that forwards messages to WebSocket clients.
///
/// # Arguments
/// * `video_stream` - The message stream to broadcast to
/// * `session` - The shared Zenoh session
/// * `shutdown_rx` - Per-connection shutdown signal (from WebSocket close)
/// * `global_shutdown` - Optional global shutdown token (for server-wide shutdown)
/// * `topic` - The Zenoh topic to subscribe to
pub async fn zenoh_listener(
    video_stream: Arc<MessageStream>,
    session: zenoh::Session,
    mut shutdown_rx: oneshot::Receiver<()>,
    global_shutdown: Option<CancellationToken>,
    topic: String,
) {
    let subscriber = match session.declare_subscriber(topic.clone()).await {
        Ok(sub) => sub,
        Err(e) => {
            error!("Failed to declare Zenoh subscriber for {}: {}", topic, e);
            return;
        }
    };

    debug!("Zenoh subscriber created for topic: {:?}", topic);

    loop {
        tokio::select! {
            // Check for per-connection shutdown signal
            _ = &mut shutdown_rx => {
                debug!("Connection shutdown signal received for topic: {:?}", topic);
                // Undeclare subscriber (session is shared, don't close it)
                if let Err(e) = subscriber.undeclare().await {
                    warn!("Failed to undeclare subscriber for {}: {}", topic, e);
                }
                return;
            }
            // Check for global shutdown signal (if provided)
            _ = async {
                if let Some(ref token) = global_shutdown {
                    token.cancelled().await;
                } else {
                    // If no global shutdown token, this future never completes
                    std::future::pending::<()>().await;
                }
            } => {
                debug!("Global shutdown signal received for topic: {:?}", topic);
                if let Err(e) = subscriber.undeclare().await {
                    warn!("Failed to undeclare subscriber for {}: {}", topic, e);
                }
                return;
            }
            // Wait for Zenoh messages
            sample = subscriber.recv_async() => {
                match sample {
                    Ok(sample) => {
                        let data = sample.payload().to_bytes().to_vec();
                        video_stream.broadcast(Broadcast::Binary(data));
                    }
                    Err(e) => {
                        debug!("Subscriber recv error for {}: {}", topic, e);
                        break;
                    }
                }
            }
        }
    }

    // Clean up subscriber on exit
    if let Err(e) = subscriber.undeclare().await {
        warn!("Failed to undeclare subscriber for {}: {}", topic, e);
    }
}

// ============================================================================
// WebSocket Context Trait
// ============================================================================

/// Trait for accessing WebSocket context from application state
pub trait WebSocketContext: Send + Sync + 'static {
    fn err_stream(&self) -> &Arc<MessageStream>;
    fn zenoh_session(&self) -> &zenoh::Session;
    /// Get the global shutdown token for graceful shutdown coordination.
    /// Returns None if graceful shutdown is not configured.
    fn shutdown_token(&self) -> Option<CancellationToken> {
        None
    }
}

// ============================================================================
// Query Parameters
// ============================================================================

/// Query parameters for WebSocket connections.
#[derive(Debug, Deserialize)]
pub struct WsParams {
    /// Enable permessage-deflate compression (default: true).
    /// Set to `false` for pre-compressed streams (e.g. H.264) to skip deflate.
    /// Note: topics containing "h264" or "jpeg" auto-disable compression
    /// regardless of this parameter.
    #[serde(default = "default_true")]
    pub compress: bool,
}

fn default_true() -> bool {
    true
}

/// Returns true if the topic carries pre-compressed data (H.264 video or JPEG
/// images) where permessage-deflate would waste CPU without shrinking the payload.
fn is_precompressed_topic(topic: &str) -> bool {
    topic.contains("h264") || topic.contains("jpeg")
}

// ============================================================================
// WebSocket Handlers
// ============================================================================

/// WebSocket handler for `/rt/*topic` routes.
///
/// Creates a per-connection MessageStream, spawns a zenoh_listener, and
/// manages the WebSocket connection loop forwarding broadcast messages to
/// the client.
///
/// Supports an optional `?compress=false` query parameter to disable
/// permessage-deflate compression for pre-compressed streams like H.264.
pub async fn websocket_handler<T: WebSocketContext>(
    ws: IncomingUpgrade,
    State(ctx): State<Arc<T>>,
    axum::extract::Path(topic): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<WsParams>,
) -> impl IntoResponse {
    // axum 0.8 wildcard captures include leading slash; strip it and prepend
    // "rt/" since the route prefix /rt/ is the Zenoh key expression namespace
    let topic = format!(
        "rt/{}",
        topic
            .strip_prefix('/')
            .unwrap_or(&topic)
            .trim_end_matches('/')
    );
    let use_compression = params.compress && !is_precompressed_topic(&topic);
    let options = if use_compression {
        Options::default().with_compression_level(yawc::CompressionLevel::fast())
    } else {
        Options::default().without_compression()
    };

    info!(
        "WebSocket {topic} — compression {}",
        if use_compression { "on" } else { "off" }
    );

    let (response, ws_future) = match ws.upgrade(options) {
        Ok(pair) => pair,
        Err(e) => {
            error!("WebSocket upgrade failed for {topic}: {e}");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("WebSocket upgrade failed: {e}"),
            )
                .into_response();
        }
    };

    let zenoh_session = ctx.zenoh_session().clone();
    let global_shutdown = ctx.shutdown_token();

    let video_stream = Arc::new(MessageStream::new());

    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let video_stream_clone = video_stream.clone();
    tokio::spawn(async move {
        zenoh_listener(
            video_stream_clone,
            zenoh_session,
            shutdown_rx,
            global_shutdown,
            topic,
        )
        .await;
    });

    tokio::spawn(async move {
        let ws = match ws_future.await {
            Ok(ws) => ws,
            Err(e) => {
                error!("WebSocket upgrade failed: {e}");
                return;
            }
        };
        let (mut sink, mut stream) = ws.split();
        let mut rx = video_stream.subscribe();
        let mut ping_interval = tokio::time::interval(PING_INTERVAL);
        ping_interval.tick().await; // consume immediate first tick

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Ok(Broadcast::Binary(data)) => {
                            match tokio::time::timeout(SEND_TIMEOUT, sink.send(Frame::binary(data))).await {
                                Ok(Ok(())) => {}
                                Ok(Err(_)) => { debug!("WebSocket sink error, closing"); break; }
                                Err(_) => { info!("WebSocket send timeout, dropping slow client"); break; }
                            }
                        }
                        Ok(Broadcast::Text(text)) => {
                            match tokio::time::timeout(SEND_TIMEOUT, sink.send(Frame::text(text))).await {
                                Ok(Ok(())) => {}
                                Ok(Err(_)) => { debug!("WebSocket sink error, closing"); break; }
                                Err(_) => { info!("WebSocket send timeout, dropping slow client"); break; }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("WebSocket subscriber lagged by {n} messages, skipping to latest");
                        }
                        Err(_) => {
                            debug!("Broadcast channel closed");
                            break;
                        }
                    }
                }
                frame = stream.next() => {
                    match frame {
                        Some(frame) if frame.opcode() == OpCode::Close => {
                            debug!("WebSocket close frame received");
                            break;
                        }
                        None => {
                            debug!("WebSocket stream ended (None)");
                            break;
                        }
                        Some(_) => {}
                    }
                }
                _ = ping_interval.tick() => {
                    let ping = Frame::ping(&[][..]);
                    match tokio::time::timeout(SEND_TIMEOUT, sink.send(ping)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => { debug!("Ping send error, closing"); break; }
                        Err(_) => { info!("Ping timeout, dropping unresponsive client"); break; }
                    }
                }
            }
        }
        let _ = shutdown_tx.send(());
    });

    response.into_response()
}

/// WebSocket handler for `/ws/dropped` error stream.
///
/// Forwards messages from the global error stream to connected clients.
/// No zenoh subscription is created.
pub async fn websocket_handler_errors<T: WebSocketContext>(
    ws: IncomingUpgrade,
    State(ctx): State<Arc<T>>,
) -> impl IntoResponse {
    let options = Options::default().with_compression_level(yawc::CompressionLevel::fast());

    let (response, ws_future) = match ws.upgrade(options) {
        Ok(pair) => pair,
        Err(e) => {
            error!("WebSocket upgrade failed for /ws/dropped: {e}");
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("WebSocket upgrade failed: {e}"),
            )
                .into_response();
        }
    };
    let err_stream = ctx.err_stream().clone();

    tokio::spawn(async move {
        let Ok(ws) = ws_future.await else { return };
        let (mut sink, mut stream) = ws.split();
        let mut rx = err_stream.subscribe();
        let mut ping_interval = tokio::time::interval(PING_INTERVAL);
        ping_interval.tick().await;

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Ok(Broadcast::Binary(data)) => {
                            match tokio::time::timeout(SEND_TIMEOUT, sink.send(Frame::binary(data))).await {
                                Ok(Ok(())) => {}
                                _ => break,
                            }
                        }
                        Ok(Broadcast::Text(text)) => {
                            match tokio::time::timeout(SEND_TIMEOUT, sink.send(Frame::text(text))).await {
                                Ok(Ok(())) => {}
                                _ => break,
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => break,
                    }
                }
                frame = stream.next() => {
                    match frame {
                        Some(frame) if frame.opcode() == OpCode::Close => break,
                        None => break,
                        _ => {}
                    }
                }
                _ = ping_interval.tick() => {
                    let ping = Frame::ping(&[][..]);
                    if tokio::time::timeout(SEND_TIMEOUT, sink.send(ping)).await.is_err() { break; }
                }
            }
        }
    });

    response.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_websocket_message_serialization() {
        let msg = WebSocketMessage {
            action: "subscribe".to_string(),
            topic: "test/topic".to_string(),
        };
        let json = serde_json::to_string(&msg).expect("Failed to serialize");
        assert!(json.contains("\"action\":\"subscribe\""));
        assert!(json.contains("\"topic\":\"test/topic\""));
    }

    #[test]
    fn test_websocket_message_deserialization() {
        let json = r#"{"action":"subscribe","topic":"rt/camera"}"#;
        let msg: WebSocketMessage = serde_json::from_str(json).expect("Failed to deserialize");
        assert_eq!(msg.action, "subscribe");
        assert_eq!(msg.topic, "rt/camera");
    }

    #[test]
    fn test_broadcast_clone() {
        let msg = Broadcast::Binary(vec![1, 2, 3]);
        let cloned = msg.clone();
        match cloned {
            Broadcast::Binary(data) => assert_eq!(data, vec![1, 2, 3]),
            _ => panic!("Expected Binary"),
        }
    }

    #[test]
    fn test_message_stream_broadcast() {
        let stream = MessageStream::new();
        let mut rx = stream.subscribe();
        stream.broadcast(Broadcast::Text("test".to_string()));
        let msg = rx.try_recv().unwrap();
        match msg {
            Broadcast::Text(s) => assert_eq!(s, "test"),
            _ => panic!("Expected Text"),
        }
    }
}
