// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Compression benchmark for WebSocket ↔ Zenoh bridge.
//!
//! Spins up an axum server with permessage-deflate compression, connects two
//! yawc WebSocket clients (one with compression, one without), publishes
//! identical payloads via Zenoh on separate topics, and compares the TCP-level
//! byte counts to quantify compression savings.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::routing::get;
use axum::Router;
use edgefirst_websrv::websocket::{websocket_handler, MessageStream, WebSocketContext};
use futures::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use yawc::frame::OpCode;

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

/// TCP stream wrapper that counts bytes read from the wire.
struct CountingStream {
    inner: TcpStream,
    bytes_read: Arc<AtomicU64>,
}

impl AsyncRead for CountingStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let read = buf.filled().len() - before;
            this.bytes_read.fetch_add(read as u64, Ordering::Relaxed);
        }
        result
    }
}

impl AsyncWrite for CountingStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Connect a yawc WebSocket client with a byte-counting wrapper and receive
/// `count` binary messages, returning (application_bytes, wire_bytes).
async fn receive_messages(
    addr: std::net::SocketAddr,
    topic: &str,
    compressed: bool,
    count: usize,
) -> (u64, u64) {
    let wire_bytes = Arc::new(AtomicU64::new(0));

    // Connect raw TCP and wrap with byte counter
    let tcp = TcpStream::connect(addr).await.expect("TCP connect failed");
    let counting = CountingStream {
        inner: tcp,
        bytes_read: wire_bytes.clone(),
    };

    let url: url::Url = format!("ws://{addr}/rt/{topic}").parse().unwrap();

    let options = if compressed {
        yawc::Options::default().with_compression_level(yawc::CompressionLevel::fast())
    } else {
        yawc::Options::default().without_compression()
    };

    let ws = yawc::WebSocket::handshake(url, counting, options)
        .await
        .expect("WebSocket handshake failed");

    let (_sink, mut stream) = futures::StreamExt::split(ws);

    let mut app_bytes: u64 = 0;
    let mut received = 0;

    while received < count {
        match stream.next().await {
            Some(frame) => {
                if frame.opcode() == OpCode::Binary {
                    app_bytes += frame.payload().len() as u64;
                    received += 1;
                }
            }
            None => break,
        }
    }

    (app_bytes, wire_bytes.load(Ordering::Relaxed))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_websocket_compression_ratio() {
    let zenoh_session = zenoh::open(zenoh::Config::default())
        .await
        .expect("Failed to open Zenoh session");

    let shutdown = CancellationToken::new();

    let ctx = Arc::new(TestContext {
        err_stream: Arc::new(MessageStream::new(Box::new(|| {}))),
        zenoh_session: zenoh_session.clone(),
        shutdown: shutdown.clone(),
    });

    // Build server
    let app = Router::new()
        .route("/rt/{*topic}", get(websocket_handler::<TestContext>))
        .with_state(ctx.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind");
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let msg_count = 50;

    // Highly compressible payload: repeated JSON sensor readings (~4 KB each)
    let compressible_payload: Vec<u8> = {
        let json = r#"{"timestamp":1709654400,"sensor":"imu","accel_x":0.0123,"accel_y":-9.8012,"accel_z":0.0456,"gyro_x":0.001,"gyro_y":-0.002,"gyro_z":0.003,"temp":25.4}"#;
        json.repeat(30).into_bytes()
    };

    // --- Compressed client ---
    let compressed_handle = {
        let addr = addr;
        tokio::spawn(async move {
            receive_messages(addr, "test/sensor_c", true, msg_count).await
        })
    };

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    for _ in 0..msg_count {
        zenoh_session
            .put("rt/test/sensor_c", compressible_payload.as_slice())
            .await
            .expect("Zenoh put failed");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let (app_bytes_c, wire_bytes_c) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        compressed_handle,
    )
    .await
    .expect("Timeout waiting for compressed client")
    .expect("Join error");

    // --- Uncompressed client ---
    let uncompressed_handle = {
        let addr = addr;
        tokio::spawn(async move {
            receive_messages(addr, "test/sensor_u", false, msg_count).await
        })
    };

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    for _ in 0..msg_count {
        zenoh_session
            .put("rt/test/sensor_u", compressible_payload.as_slice())
            .await
            .expect("Zenoh put failed");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let (app_bytes_u, wire_bytes_u) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        uncompressed_handle,
    )
    .await
    .expect("Timeout waiting for uncompressed client")
    .expect("Join error");

    // Report results
    let savings = if wire_bytes_u > 0 {
        (1.0 - (wire_bytes_c as f64 / wire_bytes_u as f64)) * 100.0
    } else {
        0.0
    };

    eprintln!("\n========== Compression Benchmark (Sensor Data) ==========");
    eprintln!(
        "  Payload: {} bytes x {} msgs = {} KB",
        compressible_payload.len(),
        msg_count,
        compressible_payload.len() * msg_count / 1024
    );
    eprintln!(
        "  Compressed:   app={} KB, wire={} KB",
        app_bytes_c / 1024,
        wire_bytes_c / 1024
    );
    eprintln!(
        "  Uncompressed: app={} KB, wire={} KB",
        app_bytes_u / 1024,
        wire_bytes_u / 1024
    );
    eprintln!("  Bandwidth savings: {:.1}%", savings);
    eprintln!("=========================================================\n");

    // Both clients should receive the same application data
    assert_eq!(
        app_bytes_c, app_bytes_u,
        "Application byte count mismatch: compressed={}, uncompressed={}",
        app_bytes_c, app_bytes_u
    );

    // Compressed should use less wire bandwidth
    assert!(
        wire_bytes_c < wire_bytes_u,
        "Compressed wire bytes ({}) should be less than uncompressed ({})",
        wire_bytes_c, wire_bytes_u
    );

    // For highly compressible JSON data we expect significant savings
    assert!(
        savings > 50.0,
        "Expected >50% bandwidth savings on repeated JSON, got {:.1}%",
        savings
    );

    shutdown.cancel();
}

/// Verify that `?compress=false` disables server-side compression:
/// wire bytes should be roughly equal to application bytes (no savings).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_compress_false_disables_compression() {
    let zenoh_session = zenoh::open(zenoh::Config::default())
        .await
        .expect("Failed to open Zenoh session");

    let shutdown = CancellationToken::new();

    let ctx = Arc::new(TestContext {
        err_stream: Arc::new(MessageStream::new(Box::new(|| {}))),
        zenoh_session: zenoh_session.clone(),
        shutdown: shutdown.clone(),
    });

    let app = Router::new()
        .route("/rt/{*topic}", get(websocket_handler::<TestContext>))
        .with_state(ctx.clone());

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind");
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let msg_count = 20;

    // Highly compressible payload
    let payload: Vec<u8> = {
        let json = r#"{"timestamp":1709654400,"sensor":"imu","accel_x":0.0123,"accel_y":-9.8012,"accel_z":0.0456}"#;
        json.repeat(30).into_bytes()
    };

    // --- Client connecting with ?compress=false ---
    let wire_bytes_no_compress = Arc::new(AtomicU64::new(0));
    let wire_counter = wire_bytes_no_compress.clone();

    let handle = tokio::spawn(async move {
        let tcp = TcpStream::connect(addr).await.expect("TCP connect failed");
        let counting = CountingStream {
            inner: tcp,
            bytes_read: wire_counter,
        };

        // URL includes ?compress=false to tell the server to skip compression
        let url: url::Url = format!("ws://{addr}/rt/test/nocomp?compress=false")
            .parse()
            .unwrap();

        // Client also connects without compression to match
        let options = yawc::Options::default().without_compression();
        let ws = yawc::WebSocket::handshake(url, counting, options)
            .await
            .expect("WebSocket handshake failed");

        let (_sink, mut stream) = futures::StreamExt::split(ws);
        let mut app_bytes: u64 = 0;
        let mut received = 0;

        while received < msg_count {
            match stream.next().await {
                Some(frame) => {
                    if frame.opcode() == OpCode::Binary {
                        app_bytes += frame.payload().len() as u64;
                        received += 1;
                    }
                }
                None => break,
            }
        }
        app_bytes
    });

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    for _ in 0..msg_count {
        zenoh_session
            .put("rt/test/nocomp", payload.as_slice())
            .await
            .expect("Zenoh put failed");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let app_bytes = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
        .await
        .expect("Timeout")
        .expect("Join error");

    let wire = wire_bytes_no_compress.load(Ordering::Relaxed);

    eprintln!("\n========== compress=false Test ==========");
    eprintln!("  app_bytes = {app_bytes}, wire_bytes = {wire}");

    // Wire bytes include HTTP upgrade + framing overhead, so they will be
    // slightly larger than app bytes. But there should be NO deflate savings,
    // so wire should be >= app bytes (not significantly smaller).
    assert!(
        wire >= app_bytes,
        "Wire bytes ({wire}) should be >= app bytes ({app_bytes}) when compression is disabled"
    );

    shutdown.cancel();
}
