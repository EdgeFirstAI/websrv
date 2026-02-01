// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! EdgeFirst WebUI Server
//!
//! Main entry point for the web server that provides:
//! - Real-time video streaming via WebSocket
//! - MCAP recording and replay management
//! - EdgeFirst Studio integration for uploads
//! - System service management

use actix_files as fs;
use actix_web::{web, App, HttpRequest, HttpResponse, HttpServer, Responder, Result};
use actix_web_actors::ws;
use actix_web_lab::middleware::RedirectHttps;
use clap::Parser;
use log::{debug, error, info};
use openssl::{
    pkey::{PKey, Private},
    ssl::{SslAcceptor, SslMethod},
};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicI64;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;

// Import from the library
use edgefirst_websrv::{
    args::{Args, WebUISettings},
    auth::{auth_login, auth_logout, auth_status, AuthContext},
    config::{get_config, get_upload_credentials, read_storage_directory, set_config},
    mcap::{mcap_downloader, McapContext, WebSocketSession},
    recording::{
        check_recorder_status, check_replay_status, delete, get_current_recording, isolate_system,
        start, start_replay, stop, stop_replay, user_mode_check_recorder_status,
        user_mode_check_replay_status, user_mode_start, user_mode_stop, AppState, RecordingContext,
    },
    services::{get_all_services, update_service},
    storage::{check_storage_availability, StorageContext},
    studio::{list_project_labels, list_studio_projects, StudioContext},
    upload::{
        cancel_upload_handler, get_upload_handler, list_uploads_handler, start_upload_handler,
        UploadContext, UploadManager,
    },
    websocket::{
        websocket_handler_errors, websocket_handler_high_priority, websocket_handler_low_priority,
        MessageStream, WebSocketContext,
    },
};

// ============================================================================
// SSL Configuration
// ============================================================================

const SERVER_PEM: &[u8] = include_bytes!("../server.pem");

fn load_encrypted_private_key() -> PKey<Private> {
    let buffer = SERVER_PEM;
    PKey::private_key_from_pem_passphrase(buffer, b"password").expect("Failed to load private key")
}

// ============================================================================
// Server Context
// ============================================================================

/// Server context containing shared state for all handlers
pub struct ServerContext {
    pub args: Args,
    pub err_stream: Arc<MessageStream>,
    pub err_count: AtomicI64,
    pub upload_manager: Arc<UploadManager>,
    pub upload_progress_stream: Arc<MessageStream>,
    pub zenoh_session: zenoh::Session,
}

// Implement all required traits for ServerContext
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

    fn err_count(&self) -> &AtomicI64 {
        &self.err_count
    }

    fn zenoh_session(&self) -> &zenoh::Session {
        &self.zenoh_session
    }
}

// ============================================================================
// Page Handlers
// ============================================================================

async fn index(data: web::Data<ServerContext>) -> Result<fs::NamedFile> {
    let base_path = PathBuf::from(&data.args.docroot);

    let data_path = if base_path.is_dir() {
        base_path.join("index.html")
    } else {
        base_path
    };

    debug!("{:?}", data_path);

    match fs::NamedFile::open(data_path) {
        Ok(file) => Ok(file),
        Err(_) => {
            error!("Index file not found");
            Err(actix_web::error::ErrorNotFound("Index file not found"))
        }
    }
}

async fn custom_file_handler(
    req: HttpRequest,
    data: web::Data<ServerContext>,
) -> actix_web::Result<fs::NamedFile> {
    let path: String = req.match_info().query("file").parse().unwrap();
    let base_path = Path::new(&data.args.docroot);

    let file_path = base_path.join(&path);

    if file_path.exists() && file_path.is_file() {
        return Ok(fs::NamedFile::open(file_path)?);
    }

    let html_path = base_path.join(format!("{}.html", path));
    if html_path.exists() && html_path.is_file() {
        return Ok(fs::NamedFile::open(html_path)?);
    }

    Err(actix_web::error::ErrorNotFound(format!(
        "File {:?} not found",
        path
    )))
}

async fn serve_settings_page(data: web::Data<ServerContext>) -> Result<fs::NamedFile> {
    let base_path = PathBuf::from(&data.args.docroot);

    let file_path = if base_path.is_dir() {
        base_path.join("config/settings.html")
    } else {
        base_path
    };
    debug!("{:?}", file_path);
    Ok(fs::NamedFile::open(file_path)?)
}

async fn serve_config_page(
    data: web::Data<ServerContext>,
    path: web::Path<String>,
) -> Result<fs::NamedFile> {
    let service = path.into_inner();
    let base_path = PathBuf::from(&data.args.docroot);

    let file_path = if base_path.is_dir() {
        base_path.join(format!("config/{}.html", service))
    } else {
        base_path
    };
    debug!("{:?}", file_path);
    Ok(fs::NamedFile::open(file_path)?)
}

async fn user_mode_get_config(data: web::Data<ServerContext>) -> HttpResponse {
    HttpResponse::Ok().json(WebUISettings::from(data.args.clone()))
}

// ============================================================================
// MCAP WebSocket Handler
// ============================================================================

async fn mcap_websocket_handler(req: HttpRequest, stream: web::Payload) -> impl Responder {
    let data = req.app_data::<web::Data<ServerContext>>().unwrap().clone();
    ws::start(
        WebSocketSession {
            context: Some(data),
        },
        &req,
        stream,
    )
}

/// WebSocket handler for upload progress updates
async fn upload_websocket_handler(
    req: HttpRequest,
    stream: web::Payload,
    data: web::Data<ServerContext>,
) -> Result<HttpResponse, actix_web::Error> {
    use edgefirst_websrv::websocket::MyWebSocket;

    ws::start(
        MyWebSocket::without_zenoh(data.upload_progress_stream.clone(), 16),
        &req,
        stream,
    )
    .map_err(|e| {
        error!("Upload WebSocket connection failed: {:?}", e);
        e
    })
}

// ============================================================================
// Main Entry Point
// ============================================================================

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let handle = Handle::current();

    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    let certificate =
        openssl::x509::X509::from_pem(SERVER_PEM).expect("Failed to parse certificate");

    builder
        .set_private_key(&load_encrypted_private_key())
        .expect("Failed to set private key");
    builder
        .set_certificate(&certificate)
        .expect("Failed to set certificate");

    let hostname = hostname::get()
        .unwrap_or_else(|_| "unknown".into())
        .to_string_lossy()
        .into_owned();
    info!("To visualize navigate to https://{} ", hostname);
    let state = web::Data::new(AppState {
        process: Mutex::new(None),
    });

    // Initialize UploadManager
    // In system mode, read storage path from /etc/default/recorder
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
    let (upload_tx, _) = channel();
    let upload_broadcaster = Arc::new(MessageStream::new(upload_tx, Box::new(|| {}), false));
    let upload_manager = Arc::new(UploadManager::new(storage_path, upload_broadcaster.clone()));

    // Initialize upload manager (scan for incomplete uploads from previous session)
    if let Err(e) = upload_manager.initialize().await {
        error!("Failed to initialize upload manager: {}", e);
    }

    // Initialize shared Zenoh session
    let zenoh_session = zenoh::open(args.clone())
        .await
        .expect("Failed to open Zenoh session");
    info!("Zenoh session initialized");

    // Binding to [::] will also bind to 0.0.0.0
    // Use configurable ports (defaults: 443 HTTPS, 80 HTTP)
    let https_port = args.https_port;
    let http_port = args.http_port;
    let addrs: Vec<std::net::SocketAddr> = vec![
        format!("[::]:{}", https_port).parse().unwrap(),
        format!("0.0.0.0:{}", https_port).parse().unwrap(),
    ];
    let addrs_http: Vec<std::net::SocketAddr> = vec![
        format!("[::]:{}", http_port).parse().unwrap(),
        format!("0.0.0.0:{}", http_port).parse().unwrap(),
    ];
    info!("Listening on HTTPS port {} and HTTP port {}", https_port, http_port);

    HttpServer::new(move || {
        let (tx, _) = channel();

        let server_ctx = ServerContext {
            args: args.clone(),
            err_stream: Arc::new(MessageStream::new(tx, Box::new(|| {}), false)),
            err_count: AtomicI64::new(0),
            upload_manager: upload_manager.clone(),
            upload_progress_stream: upload_broadcaster.clone(),
            zenoh_session: zenoh_session.clone(),
        };
        let _handle = handle.clone();

        if args.system {
            App::new()
                .wrap(RedirectHttps::default())
                .app_data(state.clone())
                .app_data(web::Data::new(server_ctx))
                .service(
                    web::scope("")
                        .route("/", web::get().to(index))
                        .route("/settings", web::get().to(serve_settings_page))
                        .route(
                            "/check-storage",
                            web::get().to(check_storage_availability::<ServerContext>),
                        )
                        .route("/start", web::post().to(start))
                        .route("/stop", web::post().to(stop))
                        .route("/delete", web::post().to(delete))
                        .route("/replay", web::post().to(start_replay))
                        .route("/replay-end", web::post().to(stop_replay))
                        .route("/replay-status", web::get().to(check_replay_status))
                        .route("/live-run", web::post().to(isolate_system))
                        .route("/download/{file:.*}", web::get().to(mcap_downloader))
                        .route(
                            "/get-upload-credentials",
                            web::get().to(get_upload_credentials),
                        )
                        // Authentication API routes
                        .route(
                            "/api/auth/login",
                            web::post().to(auth_login::<ServerContext>),
                        )
                        .route(
                            "/api/auth/status",
                            web::get().to(auth_status::<ServerContext>),
                        )
                        .route(
                            "/api/auth/logout",
                            web::post().to(auth_logout::<ServerContext>),
                        )
                        // Upload API routes
                        .route(
                            "/api/uploads",
                            web::post().to(start_upload_handler::<ServerContext>),
                        )
                        .route(
                            "/api/uploads",
                            web::get().to(list_uploads_handler::<ServerContext>),
                        )
                        .route(
                            "/api/uploads/{id}",
                            web::get().to(get_upload_handler::<ServerContext>),
                        )
                        .route(
                            "/api/uploads/{id}",
                            web::delete().to(cancel_upload_handler::<ServerContext>),
                        )
                        // Studio API routes
                        .route(
                            "/api/studio/projects",
                            web::get().to(list_studio_projects::<ServerContext>),
                        )
                        .route(
                            "/api/studio/projects/{id}/labels",
                            web::get().to(list_project_labels::<ServerContext>),
                        )
                        .route("/recorder-status", web::get().to(check_recorder_status))
                        .route("/current-recording", web::get().to(get_current_recording))
                        .service(
                            web::resource("/ws/dropped")
                                .route(web::get().to(websocket_handler_errors::<ServerContext>)),
                        )
                        .service(
                            web::resource("/rt/detect/mask").route(
                                web::get().to(websocket_handler_high_priority::<ServerContext>),
                            ),
                        )
                        .service(
                            web::resource("/rt/{tail:.*}").route(
                                web::get().to(websocket_handler_low_priority::<ServerContext>),
                            ),
                        )
                        .route("/config/service/status", web::post().to(get_all_services))
                        .route("/config/services/update", web::post().to(update_service))
                        .service(
                            web::resource("/mcap/").route(web::get().to(mcap_websocket_handler)),
                        )
                        .service(
                            web::resource("/ws/uploads")
                                .route(web::get().to(upload_websocket_handler)),
                        )
                        .route("/config/{service}", web::get().to(serve_config_page))
                        .route("/config/{service}/details", web::get().to(get_config))
                        .route("/config/{service}", web::post().to(set_config))
                        .route("/{file:.*}", web::get().to(custom_file_handler)),
                )
        } else {
            App::new()
                .wrap(RedirectHttps::default())
                .app_data(state.clone())
                .app_data(web::Data::new(server_ctx))
                .service(
                    web::scope("")
                        .route("/", web::get().to(index))
                        .route("/settings", web::get().to(serve_settings_page))
                        .route(
                            "/check-storage",
                            web::get().to(check_storage_availability::<ServerContext>),
                        )
                        .route("/start", web::post().to(user_mode_start::<ServerContext>))
                        .route("/stop", web::post().to(user_mode_stop))
                        .route("/delete", web::post().to(delete))
                        .route("/replay", web::post().to(start_replay))
                        .route("/replay-end", web::post().to(stop_replay))
                        .route(
                            "/replay-status",
                            web::get().to(user_mode_check_replay_status),
                        )
                        .route("/live-run", web::post().to(isolate_system))
                        .route("/download/{file:.*}", web::get().to(mcap_downloader))
                        .route(
                            "/get-upload-credentials",
                            web::get().to(get_upload_credentials),
                        )
                        // Authentication API routes
                        .route(
                            "/api/auth/login",
                            web::post().to(auth_login::<ServerContext>),
                        )
                        .route(
                            "/api/auth/status",
                            web::get().to(auth_status::<ServerContext>),
                        )
                        .route(
                            "/api/auth/logout",
                            web::post().to(auth_logout::<ServerContext>),
                        )
                        // Upload API routes
                        .route(
                            "/api/uploads",
                            web::post().to(start_upload_handler::<ServerContext>),
                        )
                        .route(
                            "/api/uploads",
                            web::get().to(list_uploads_handler::<ServerContext>),
                        )
                        .route(
                            "/api/uploads/{id}",
                            web::get().to(get_upload_handler::<ServerContext>),
                        )
                        .route(
                            "/api/uploads/{id}",
                            web::delete().to(cancel_upload_handler::<ServerContext>),
                        )
                        // Studio API routes
                        .route(
                            "/api/studio/projects",
                            web::get().to(list_studio_projects::<ServerContext>),
                        )
                        .route(
                            "/api/studio/projects/{id}/labels",
                            web::get().to(list_project_labels::<ServerContext>),
                        )
                        .route(
                            "/recorder-status",
                            web::get().to(user_mode_check_recorder_status),
                        )
                        .route("/current-recording", web::get().to(get_current_recording))
                        .service(
                            web::resource("/ws/dropped")
                                .route(web::get().to(websocket_handler_errors::<ServerContext>)),
                        )
                        .service(
                            web::resource("/rt/detect/mask").route(
                                web::get().to(websocket_handler_high_priority::<ServerContext>),
                            ),
                        )
                        .service(
                            web::resource("/rt/{tail:.*}").route(
                                web::get().to(websocket_handler_low_priority::<ServerContext>),
                            ),
                        )
                        .route("/config/service/status", web::post().to(get_all_services))
                        .route("/config/services/update", web::post().to(update_service))
                        .service(
                            web::resource("/mcap/").route(web::get().to(mcap_websocket_handler)),
                        )
                        .service(
                            web::resource("/ws/uploads")
                                .route(web::get().to(upload_websocket_handler)),
                        )
                        .route("/config/{service}", web::get().to(serve_config_page))
                        .route(
                            "/config/{service}/details",
                            web::get().to(user_mode_get_config),
                        )
                        .route("/config/{service}", web::post().to(set_config))
                        .route("/{file:.*}", web::get().to(custom_file_handler)),
                )
        }
    })
    .bind(&addrs_http[..])?
    .bind_openssl(&addrs[..], builder)?
    .workers(8)
    .run()
    .await
}
