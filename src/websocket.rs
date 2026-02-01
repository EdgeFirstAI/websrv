// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! WebSocket handling for real-time streaming.
//!
//! Provides:
//! - Message broadcasting to WebSocket clients
//! - Zenoh subscription integration
//! - Priority-based message handling

use actix::prelude::*;
use actix_web::{web, HttpRequest, HttpResponse, Result};
use actix_web_actors::ws;
use cdr::{CdrLe, Infinite};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

// ============================================================================
// Message Types
// ============================================================================

/// WebSocket broadcast message - Binary for CDR data, Text for JSON
#[derive(Message, Clone)]
#[rtype(result = "()")]
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

/// Service action request
#[derive(Deserialize)]
pub struct ServiceAction {
    pub service: String,
    pub action: String,
}

// ============================================================================
// Message Stream
// ============================================================================

/// Manages WebSocket client connections and broadcasting
pub struct MessageStream {
    clients: Arc<Mutex<Vec<Recipient<Broadcast>>>>,
    pub on_exit: Sender<String>,
    on_err: Box<dyn Fn() + Send + Sync>,
    high_priority: bool,
}

impl MessageStream {
    /// Create a new MessageStream
    pub fn new(
        on_exit: Sender<String>,
        on_err: Box<dyn Fn() + Send + Sync>,
        high_priority: bool,
    ) -> Self {
        MessageStream {
            clients: Arc::new(Mutex::new(Vec::new())),
            on_exit,
            on_err,
            high_priority,
        }
    }

    /// Add a WebSocket client
    pub fn add_client(&self, client: Recipient<Broadcast>) {
        self.clients.lock().unwrap().push(client);
    }

    /// Broadcast a message to all connected clients
    pub fn broadcast(&self, message: Broadcast) {
        for client in self.clients.lock().unwrap().iter() {
            if self.high_priority {
                client.do_send(message.clone());
            } else {
                let res = client.try_send(message.clone());
                if res.is_err() {
                    (self.on_err)();
                }
            }
        }
    }
}

// ============================================================================
// WebSocket Actor
// ============================================================================

/// WebSocket actor for handling client connections
pub struct MyWebSocket {
    video_stream: Arc<MessageStream>,
    capacity: usize,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl MyWebSocket {
    /// Create a new WebSocket with zenoh shutdown signaling
    pub fn new(
        video_stream: Arc<MessageStream>,
        capacity: usize,
        shutdown_tx: oneshot::Sender<()>,
    ) -> Self {
        MyWebSocket {
            video_stream,
            capacity,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// Create a WebSocket without zenoh shutdown signaling
    /// Used for error streams and upload progress that don't have zenoh subscribers
    pub fn without_zenoh(video_stream: Arc<MessageStream>, capacity: usize) -> Self {
        MyWebSocket {
            video_stream,
            capacity,
            shutdown_tx: None,
        }
    }
}

impl Actor for MyWebSocket {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        if self.capacity > 0 {
            ctx.set_mailbox_capacity(self.capacity);
        }
        self.video_stream.add_client(ctx.address().recipient());
    }
}

impl Drop for MyWebSocket {
    fn drop(&mut self) {
        // Signal the zenoh listener to shutdown
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        // Keep the old signal for backward compatibility
        let _ = self.video_stream.on_exit.send("STOP".to_string());
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for MyWebSocket {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Text(text)) => {
                if let Ok(message) = serde_json::from_str::<WebSocketMessage>(&text) {
                    if message.action.as_str() == "subscribe" {
                        let webpage_topic = message.topic.to_string();
                        info!("Subscribed to topic: {}", webpage_topic);
                        self.video_stream.on_exit.send(webpage_topic).unwrap();
                    }
                }
            }
            Ok(_) => (),
            Err(_) => ctx.stop(),
        }
    }
}

impl Handler<Broadcast> for MyWebSocket {
    type Result = ();

    fn handle(&mut self, msg: Broadcast, ctx: &mut Self::Context) {
        match msg {
            Broadcast::Binary(data) => ctx.binary(data),
            Broadcast::Text(data) => ctx.text(data),
        }
    }
}

// ============================================================================
// Zenoh Integration
// ============================================================================

/// Zenoh subscriber that forwards messages to WebSocket clients
pub async fn zenoh_listener(
    video_stream: Arc<MessageStream>,
    session: zenoh::Session,
    mut shutdown_rx: oneshot::Receiver<()>,
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
            // Check for shutdown signal
            _ = &mut shutdown_rx => {
                debug!("Shutdown signal received for topic: {:?}", topic);
                // Undeclare subscriber (session is shared, don't close it)
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
// WebSocket Handlers
// ============================================================================

/// Trait for accessing WebSocket context
pub trait WebSocketContext {
    fn err_stream(&self) -> &Arc<MessageStream>;
    fn err_count(&self) -> &AtomicI64;
    fn zenoh_session(&self) -> &zenoh::Session;
}

/// WebSocket handler for error stream
pub async fn websocket_handler_errors<T: WebSocketContext>(
    req: HttpRequest,
    stream: web::Payload,
    data: web::Data<T>,
) -> Result<HttpResponse, actix_web::Error> {
    ws::start(
        MyWebSocket::without_zenoh(data.err_stream().clone(), 1),
        &req,
        stream,
    )
    .map_err(|e| {
        error!("WebSocket connection failed: {:?}", e);
        e
    })
}

/// WebSocket handler for high priority streams (e.g., detection masks)
pub async fn websocket_handler_high_priority<T: WebSocketContext + 'static>(
    req: HttpRequest,
    stream: web::Payload,
    data: web::Data<T>,
) -> Result<HttpResponse, actix_web::Error> {
    websocket_handler(req, stream, data, true).await
}

/// WebSocket handler for low priority streams
pub async fn websocket_handler_low_priority<T: WebSocketContext + 'static>(
    req: HttpRequest,
    stream: web::Payload,
    data: web::Data<T>,
) -> Result<HttpResponse, actix_web::Error> {
    websocket_handler(req, stream, data, false).await
}

/// Generic WebSocket handler with priority setting
pub async fn websocket_handler<T: WebSocketContext + 'static>(
    req: HttpRequest,
    stream: web::Payload,
    data: web::Data<T>,
    is_high_priority: bool,
) -> Result<HttpResponse, actix_web::Error> {
    let path = req.uri().path().to_owned();
    let cleaned_path = if let Some(stripped) = path.strip_prefix('/') {
        stripped.to_owned()
    } else {
        path.to_owned()
    };
    let cleaned_path = if cleaned_path.ends_with('/') {
        cleaned_path[..cleaned_path.len() - 1].to_owned()
    } else {
        cleaned_path.clone()
    };

    let topic = cleaned_path.clone();
    let (tx, _rx) = channel();

    // Clone fields before moving into closure
    let err_stream = data.err_stream().clone();
    let err_count_val = data.err_count().load(Ordering::SeqCst);
    let zenoh_session = data.zenoh_session().clone();

    let video_stream = Arc::new(MessageStream::new(
        tx,
        Box::new({
            let err_stream = err_stream.clone();
            let topic = topic.clone();
            move || {
                let new_val = err_count_val + 1;
                let msg = format!(
                    "{{\"dropped frames\": {new_val}, \"dropped_topic\": \"{}\"}}",
                    topic.clone()
                );
                let msg = cdr::serialize::<_, _, CdrLe>(&msg, Infinite).unwrap();
                err_stream.broadcast(Broadcast::Binary(msg));
            }
        }),
        is_high_priority,
    ));
    let video_stream_clone = video_stream.clone();

    // Create shutdown channel for graceful zenoh listener termination
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let capacity = if is_high_priority { 16 } else { 1 };
    let ws_result = ws::start(
        MyWebSocket::new(video_stream, capacity, shutdown_tx),
        &req,
        stream,
    );

    if ws_result.is_ok() {
        // Spawn zenoh listener as a tokio task instead of a separate thread
        tokio::spawn(async move {
            zenoh_listener(video_stream_clone, zenoh_session, shutdown_rx, cleaned_path).await;
        });
    }

    ws_result.map_err(|e| {
        error!("WebSocket connection failed: {:?}", e);
        e
    })
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
}
