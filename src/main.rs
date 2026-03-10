// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! EdgeFirst WebUI Server
//!
//! Main entry point for the web server that provides:
//! - Real-time video streaming via WebSocket
//! - MCAP recording and replay management
//! - EdgeFirst Studio integration for uploads
//! - System service management

mod ssl;

use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use clap::Parser;
use listenfd::ListenFd;
use log::{error, info};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower_http::services::ServeDir;

use edgefirst_websrv::{
    args::{Args, WebUISettings},
    auth::{auth_login, auth_logout, auth_status, AuthContext},
    config::{get_config, read_storage_directory, set_config},
    mcap::{list_mcap_files, mcap_downloader, McapContext},
    recording::{
        check_recorder_status, check_replay_status, delete as recording_delete, get_current_recording,
        isolate_system, start, start_replay, stop, stop_replay, user_mode_check_recorder_status,
        user_mode_check_replay_status, user_mode_start, user_mode_stop, RecordingContext,
    },
    services::{get_all_services, update_service},
    shutdown::ShutdownCoordinator,
    storage::{check_storage_availability, StorageContext},
    studio::{list_project_labels, list_studio_projects, StudioContext},
    upload::{
        cancel_upload_handler, get_upload_handler, list_uploads_handler, start_upload_handler,
        UploadContext, UploadManager,
    },
    websocket::{
        websocket_handler, websocket_handler_errors, Broadcast, MessageStream, WebSocketContext,
    },
};

// ============================================================================
// Server Context
// ============================================================================

/// Server context containing shared state for all handlers
pub struct ServerContext {
    pub args: Args,
    pub err_stream: Arc<MessageStream>,
    pub upload_manager: Arc<UploadManager>,
    pub upload_progress_stream: Arc<MessageStream>,
    pub zenoh_session: zenoh::Session,
    pub shutdown_coordinator: ShutdownCoordinator,
    pub process: Mutex<Option<std::process::Child>>,
}

impl AuthContext for ServerContext {
    fn upload_manager(&self) -> &Arc<UploadManager> {
        &self.upload_manager
    }
}

impl UploadContext for ServerContext {
    fn upload_manager(&self) -> &Arc<UploadManager> {
        &self.upload_manager
    }
}

impl StudioContext for ServerContext {
    fn upload_manager(&self) -> &Arc<UploadManager> {
        &self.upload_manager
    }
}

impl StorageContext for ServerContext {
    fn is_system_mode(&self) -> bool {
        self.args.system
    }

    fn storage_path(&self) -> &str {
        &self.args.storage_path
    }
}

impl RecordingContext for ServerContext {
    fn storage_path(&self) -> &str {
        &self.args.storage_path
    }

    fn process(&self) -> &Mutex<Option<std::process::Child>> {
        &self.process
    }
}

impl McapContext for ServerContext {
    fn is_system_mode(&self) -> bool {
        self.args.system
    }

    fn storage_path(&self) -> &str {
        &self.args.storage_path
    }
}

impl WebSocketContext for ServerContext {
    fn err_stream(&self) -> &Arc<MessageStream> {
        &self.err_stream
    }

    fn zenoh_session(&self) -> &zenoh::Session {
        &self.zenoh_session
    }

    fn shutdown_token(&self) -> Option<tokio_util::sync::CancellationToken> {
        Some(self.shutdown_coordinator.token())
    }
}

// ============================================================================
// Upload WebSocket Handler
// ============================================================================

/// WebSocket handler for `/ws/uploads` — forwards from upload_progress_stream.
async fn websocket_handler_uploads(
    ws: yawc::IncomingUpgrade,
    State(ctx): State<Arc<ServerContext>>,
) -> impl IntoResponse {
    let options = yawc::Options::default().with_compression_level(yawc::CompressionLevel::fast());
    let (response, ws_future) = ws.upgrade(options).unwrap();
    let stream = ctx.upload_progress_stream.clone();

    tokio::spawn(async move {
        let Ok(ws) = ws_future.await else { return };
        let (mut sink, mut incoming) = futures::StreamExt::split(ws);
        let mut rx = stream.subscribe();
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
        ping_interval.tick().await;

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Ok(Broadcast::Binary(data)) => {
                            match tokio::time::timeout(std::time::Duration::from_secs(5), futures::SinkExt::send(&mut sink, yawc::frame::Frame::binary(data))).await {
                                Ok(Ok(())) => {}
                                _ => break,
                            }
                        }
                        Ok(Broadcast::Text(text)) => {
                            match tokio::time::timeout(std::time::Duration::from_secs(5), futures::SinkExt::send(&mut sink, yawc::frame::Frame::text(text))).await {
                                Ok(Ok(())) => {}
                                _ => break,
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => break,
                    }
                }
                frame = futures::StreamExt::next(&mut incoming) => {
                    match frame {
                        Some(frame) if frame.opcode() == yawc::frame::OpCode::Close => break,
                        None => break,
                        _ => {}
                    }
                }
                _ = ping_interval.tick() => {
                    let ping = yawc::frame::Frame::ping(&[][..]);
                    if tokio::time::timeout(std::time::Duration::from_secs(5), futures::SinkExt::send(&mut sink, ping)).await.is_err() { break; }
                }
            }
        }
    });

    response
}

// ============================================================================
// User Mode Config Handler
// ============================================================================

/// GET /config/:service/details (user mode) — returns WebUISettings as JSON.
async fn user_mode_get_config(State(ctx): State<Arc<ServerContext>>) -> impl IntoResponse {
    axum::Json(WebUISettings::from(ctx.args.clone()))
}

// ============================================================================
// HTTP → HTTPS Redirect
// ============================================================================

async fn redirect_to_https(headers: HeaderMap, uri: Uri) -> Redirect {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let https_uri = format!("https://{host}{uri}");
    Redirect::permanent(&https_uri)
}

// ============================================================================
// Router Construction
// ============================================================================

/// Routes shared between system mode and user mode.
fn common_routes(ctx: Arc<ServerContext>) -> Router {
    Router::new()
        // Storage
        .route(
            "/check-storage",
            get(check_storage_availability::<ServerContext>),
        )
        // Recording (shared, no state needed)
        .route("/delete", post(recording_delete))
        .route("/replay", post(start_replay))
        .route("/replay-end", post(stop_replay))
        .route("/live-run", post(isolate_system))
        .route("/current-recording", get(get_current_recording))
        // MCAP download
        .route("/download/{*path}", get(mcap_downloader))
        // MCAP listing
        .route("/mcap", get(list_mcap_files::<ServerContext>))
        // Auth API
        .route("/api/auth/login", post(auth_login::<ServerContext>))
        .route("/api/auth/status", get(auth_status::<ServerContext>))
        .route("/api/auth/logout", post(auth_logout::<ServerContext>))
        // Upload API
        .route(
            "/api/uploads",
            post(start_upload_handler::<ServerContext>).get(list_uploads_handler::<ServerContext>),
        )
        .route(
            "/api/uploads/{id}",
            get(get_upload_handler::<ServerContext>)
                .delete(cancel_upload_handler::<ServerContext>),
        )
        // Studio API
        .route(
            "/api/studio/projects",
            get(list_studio_projects::<ServerContext>),
        )
        .route(
            "/api/studio/projects/{id}/labels",
            get(list_project_labels::<ServerContext>),
        )
        // Service config (POST only in common; GET /details split into mode routes)
        .route("/config/{service}", post(set_config))
        .route("/config/service/status", post(get_all_services))
        .route("/config/services/update", post(update_service))
        // WebSocket: error stream and real-time topics
        .route("/ws/dropped", get(websocket_handler_errors::<ServerContext>))
        .route("/ws/uploads", get(websocket_handler_uploads))
        .route("/rt/{*topic}", get(websocket_handler::<ServerContext>))
        .with_state(ctx)
}

/// Routes for system mode only.
fn system_routes(ctx: Arc<ServerContext>) -> Router {
    Router::new()
        .route("/start", post(start::<ServerContext>))
        .route("/stop", post(stop::<ServerContext>))
        .route("/recorder-status", get(check_recorder_status))
        .route("/replay-status", get(check_replay_status))
        // System mode: config details returns raw service config
        .route("/config/{service}/details", get(get_config))
        .with_state(ctx)
}

/// Routes for user mode only.
fn user_routes(ctx: Arc<ServerContext>) -> Router {
    Router::new()
        .route("/start", post(user_mode_start::<ServerContext>))
        .route("/stop", post(user_mode_stop::<ServerContext>))
        .route("/recorder-status", get(user_mode_check_recorder_status))
        .route("/replay-status", get(user_mode_check_replay_status))
        // User mode: config details returns WebUISettings JSON
        .route("/config/{service}/details", get(user_mode_get_config))
        .with_state(ctx)
}

/// Assemble the full router for system or user mode, with ServeDir fallback.
fn build_router(ctx: Arc<ServerContext>, is_system: bool) -> Router {
    let docroot = ctx.args.docroot.clone();

    let mut router = common_routes(ctx.clone());
    if is_system {
        router = router.merge(system_routes(ctx));
    } else {
        router = router.merge(user_routes(ctx));
    }

    // Static file fallback: serve the webui from docroot.
    // When a file isn't found, try appending .html (e.g. /camera -> camera.html).
    let html_fallback = {
        let docroot = docroot.clone();
        axum::routing::any(move |uri: Uri| {
            let docroot = docroot.clone();
            async move {
                let path = uri.path().trim_start_matches('/');
                let html_path = PathBuf::from(&docroot).join(format!("{path}.html"));
                if html_path.is_file() {
                    let body = tokio::fs::read(&html_path).await.unwrap_or_default();
                    axum::http::Response::builder()
                        .header("content-type", "text/html")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                        .into_response()
                } else {
                    axum::http::StatusCode::NOT_FOUND.into_response()
                }
            }
        })
    };
    router.fallback_service(ServeDir::new(docroot).fallback(html_fallback))
}

// ============================================================================
// Main Entry Point
// ============================================================================

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");

    let args = Args::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Load or generate TLS certificate
    let cert_result = ssl::load_or_generate_certificate(&args)?;
    info!("Using {} certificate", cert_result.source);

    let hostname = ssl::get_device_hostname();
    info!("To visualize navigate to https://{} ", hostname);

    // Determine storage path (system mode reads from /etc/default/recorder)
    let storage_path = if args.system {
        match read_storage_directory() {
            Ok(dir) => PathBuf::from(dir),
            Err(e) => {
                error!("Failed to read storage directory from config: {}", e);
                PathBuf::from(&args.storage_path)
            }
        }
    } else {
        PathBuf::from(&args.storage_path)
    };

    // Initialize upload progress broadcaster and manager
    let upload_broadcaster = Arc::new(MessageStream::new(Box::new(|| {})));
    let upload_manager = Arc::new(UploadManager::new(storage_path, upload_broadcaster.clone()));

    if let Err(e) = upload_manager.initialize().await {
        error!("Failed to initialize upload manager: {}", e);
    }

    // Initialize shared Zenoh session
    let zenoh_session = zenoh::open(args.clone())
        .await
        .expect("Failed to open Zenoh session");
    info!("Zenoh session initialized");

    // Create shutdown coordinator for graceful shutdown
    let shutdown_coordinator = ShutdownCoordinator::new();

    // Build server context
    let ctx = Arc::new(ServerContext {
        args: args.clone(),
        err_stream: Arc::new(MessageStream::new(Box::new(|| {}))),
        upload_manager: upload_manager.clone(),
        upload_progress_stream: upload_broadcaster,
        zenoh_session: zenoh_session.clone(),
        shutdown_coordinator: shutdown_coordinator.clone(),
        process: Mutex::new(None),
    });

    // Build TLS config from PEM bytes.
    // Force HTTP/1.1 only — WebSocket upgrade requires HTTP/1.1 and does not
    // work with the HTTP/2 CONNECT mechanism used by browsers.
    let tls_config = {
        let certs = rustls_pemfile::certs(&mut &cert_result.cert_pem[..])
            .collect::<Result<Vec<_>, _>>()?;
        let key = rustls_pemfile::private_key(&mut &cert_result.key_pem[..])?.unwrap();
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        RustlsConfig::from_config(Arc::new(config))
    };

    let https_port = args.https_port;
    let http_port = args.http_port;

    // Build the application router
    let is_system = args.system;
    let app = build_router(ctx, is_system);

    // Check for systemd socket activation (fd 0 = HTTP, fd 1 = HTTPS)
    let mut listenfd = ListenFd::from_env();

    // HTTP listener: socket activation fd 0, or direct bind
    let http_listener = match listenfd.take_tcp_listener(0)? {
        Some(listener) => {
            listener.set_nonblocking(true)?;
            info!("Using socket-activated HTTP listener");
            tokio::net::TcpListener::from_std(listener)?
        }
        None => {
            let addr: std::net::SocketAddr = format!("[::]:{}", http_port).parse()?;
            tokio::net::TcpListener::bind(addr).await?
        }
    };

    // HTTPS listener: socket activation fd 1, or direct bind
    let https_server = match listenfd.take_tcp_listener(1)? {
        Some(listener) => {
            listener.set_nonblocking(true)?;
            info!("Using socket-activated HTTPS listener");
            axum_server::from_tcp_rustls(listener, tls_config)
        }
        None => {
            let addr: std::net::SocketAddr = format!("[::]:{}", https_port).parse()?;
            axum_server::bind_rustls(addr, tls_config)
        }
    };

    info!(
        "Listening on HTTPS port {} and HTTP port {}",
        https_port, http_port
    );

    // Spawn HTTP → HTTPS redirect server
    tokio::spawn(async move {
        let redirect_app = Router::new().fallback(redirect_to_https);
        axum::serve(http_listener, redirect_app.into_make_service())
            .await
            .expect("HTTP redirect server failed");
    });

    // axum-server handle for graceful shutdown
    let handle = axum_server::Handle::new();
    let handle_clone = handle.clone();

    // Spawn signal handler for graceful shutdown
    tokio::spawn(async move {
        shutdown_coordinator.wait_for_signal().await;
        info!("Stopping HTTPS server...");
        handle_clone.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    });

    // Start HTTPS server
    https_server
        .handle(handle)
        .serve(app.into_make_service())
        .await?;

    // Graceful shutdown cleanup
    info!("HTTPS server stopped, performing cleanup...");

    let cancelled = upload_manager.cancel_all_uploads().await;
    if cancelled > 0 {
        info!("Cancelled {} active upload(s)", cancelled);
    }

    info!("Closing Zenoh session...");
    if let Err(e) = zenoh_session.close().await {
        error!("Failed to close Zenoh session: {}", e);
    }

    info!("Graceful shutdown complete");
    Ok(())
}
