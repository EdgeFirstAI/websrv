// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! HTTP and WebSocket handlers for the `/api/programs` endpoints.

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::bundle::{self, BundleError, MAX_BUNDLE_SIZE};
use super::logs::{self, ProgramRuntime};
use super::service;
use super::{slugify, ProgramMetadata, ProgramStatus, ProgramsContext};

// ============================================================================
// Response Types
// ============================================================================

#[derive(Serialize)]
pub struct ProgramSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub status: ProgramStatus,
    pub icon: String,
    pub created: String,
    pub modified: String,
}

#[derive(Serialize)]
pub struct ListProgramsResponse {
    pub programs: Vec<ProgramSummary>,
}

#[derive(Serialize)]
pub struct CreateProgramResponse {
    pub id: String,
    pub name: String,
    pub status: ProgramStatus,
}

#[derive(Serialize)]
pub struct DeleteProgramResponse {
    pub id: String,
    pub deleted: bool,
}

#[derive(Serialize)]
pub struct ProgramActionResponse {
    pub id: String,
    pub status: ProgramStatus,
}

#[derive(Serialize)]
pub struct ProgramStatusResponse {
    pub id: String,
    pub name: String,
    pub status: ProgramStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
}

#[derive(Serialize)]
pub struct ProgramErrorResponse {
    pub error: String,
    pub code: String,
}

// ============================================================================
// Helpers
// ============================================================================

fn error_response(status: StatusCode, error: &str, code: &str) -> axum::response::Response {
    (
        status,
        Json(ProgramErrorResponse {
            error: error.to_string(),
            code: code.to_string(),
        }),
    )
        .into_response()
}

/// Query the live status of a program from systemd.
async fn live_status(id: &str, system_mode: bool) -> ProgramStatus {
    if service::is_active(id, system_mode).await {
        ProgramStatus::Running
    } else {
        ProgramStatus::Stopped
    }
}

/// Validate a bundle in a blocking task, returning an error response on failure.
async fn validate_bundle_or_error(
    body: Bytes,
) -> Result<bundle::ValidatedBundle, axum::response::Response> {
    if body.len() > MAX_BUNDLE_SIZE {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Bundle exceeds maximum size of 10MB",
            "BUNDLE_TOO_LARGE",
        ));
    }

    let data = body.to_vec();
    match tokio::task::spawn_blocking(move || bundle::validate_bundle(data)).await {
        Ok(Ok(bundle)) => Ok(bundle),
        Ok(Err(e)) => {
            let status = match &e {
                BundleError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
                _ => StatusCode::BAD_REQUEST,
            };
            Err(error_response(status, &e.to_string(), e.code()))
        }
        Err(e) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Bundle validation failed: {e}"),
            "INTERNAL_ERROR",
        )),
    }
}

/// Build program metadata from a validated bundle.
fn metadata_from_bundle(id: &str, bundle: &bundle::ValidatedBundle) -> ProgramMetadata {
    ProgramMetadata {
        id: id.to_string(),
        name: bundle.config.name.clone(),
        description: bundle.config.description.clone(),
        author: bundle.config.author.clone(),
        created: bundle.config.created.clone(),
        modified: bundle.config.modified.clone(),
        has_icon: bundle.icon_png.is_some(),
    }
}

/// Abort the log follower task for a program, if one exists.
async fn abort_log_follower(mgr: &super::ProgramManager, id: &str) {
    if let Some(rt) = mgr.runtimes().write().await.remove(id) {
        if let Some(task) = rt.journal_task {
            task.abort();
        }
    }
}

/// Stop a running program's log follower and systemd service.
async fn stop_runtime_and_service(mgr: &super::ProgramManager, id: &str) {
    abort_log_follower(mgr, id).await;
    if let Err(e) = service::stop_service(id, mgr.system_mode()).await {
        log::warn!("Failed to stop {id}: {e}");
    }
}

/// Start the log follower for a program and register its runtime.
async fn start_log_follower(mgr: &super::ProgramManager, id: &str) {
    let rt = ProgramRuntime::new();
    let task = logs::spawn_log_follower(
        id,
        rt.log_tx.clone(),
        rt.log_buffer.clone(),
        mgr.system_mode(),
    );
    mgr.runtimes().write().await.insert(
        id.to_string(),
        ProgramRuntime {
            journal_task: Some(task),
            ..rt
        },
    );
}

/// Deploy a bundle and write the systemd unit file. Rolls back on any failure.
async fn deploy_and_register(
    mgr: &super::ProgramManager,
    id: &str,
    validated: &bundle::ValidatedBundle,
) -> Result<(), axum::response::Response> {
    // Deploy bundle to disk
    if let Err(e) = mgr.deploy_bundle(id, validated).await {
        log::error!("Failed to deploy bundle for {id}: {e}");
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to deploy program",
            "DEPLOY_ERROR",
        ));
    }

    // Write systemd unit file
    let work_dir = mgr.program_dir(id);
    let sm = mgr.system_mode();
    if let Err(e) =
        service::write_unit_file(id, &validated.config.name, &work_dir, mgr.python_path(), sm).await
    {
        log::error!("Failed to write unit file for {id}: {e}");
        let _ = mgr.delete_files(id).await;
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create systemd service",
            "SERVICE_ERROR",
        ));
    }

    // Enable the service so it survives reboots
    if let Err(e) = service::daemon_reload(sm).await {
        log::warn!("daemon-reload failed for {id}: {e}");
    }
    if let Err(e) = service::enable_service(id, sm).await {
        log::warn!("Failed to enable service for {id}: {e}");
    }

    // Save metadata — rollback on failure
    let meta = metadata_from_bundle(id, validated);
    if let Err(e) = mgr.upsert(meta).await {
        log::error!("Failed to save metadata for {id}: {e}");
        let _ = service::remove_unit_file(id, sm).await;
        let _ = service::daemon_reload(sm).await;
        let _ = mgr.delete_files(id).await;
        mgr.remove(id).await;
        return Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to save program metadata",
            "DEPLOY_ERROR",
        ));
    }

    Ok(())
}

// ============================================================================
// GET /api/programs
// ============================================================================

/// List all deployed programs with their current status.
pub async fn list_programs<T: ProgramsContext>(
    State(ctx): State<Arc<T>>,
) -> axum::response::Response {
    let mgr = ctx.program_manager();
    let sm = mgr.system_mode();
    let programs = mgr.list().await;

    let mut summaries = Vec::with_capacity(programs.len());
    for meta in programs {
        let status = live_status(&meta.id, sm).await;
        summaries.push(ProgramSummary {
            icon: format!("/api/programs/{}/icon", meta.id),
            id: meta.id,
            name: meta.name,
            description: meta.description,
            status,
            created: meta.created,
            modified: meta.modified,
        });
    }

    Json(ListProgramsResponse {
        programs: summaries,
    })
    .into_response()
}

// ============================================================================
// POST /api/programs
// ============================================================================

/// Upload an `.efapp` bundle to deploy a new program.
pub async fn create_program<T: ProgramsContext>(
    State(ctx): State<Arc<T>>,
    body: Bytes,
) -> axum::response::Response {
    let validated = match validate_bundle_or_error(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Generate ID from name
    let id = slugify(&validated.config.name);
    if id.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Program name produces an empty slug",
            "INVALID_BUNDLE",
        );
    }

    let mgr = ctx.program_manager();

    // Atomic check-and-insert to prevent TOCTOU race
    if !mgr.try_insert(metadata_from_bundle(&id, &validated)).await {
        return error_response(
            StatusCode::CONFLICT,
            &format!("Program '{id}' already exists — use PUT to update"),
            "CONFLICT",
        );
    }

    // Deploy — rollback the in-memory insert on failure
    if let Err(resp) = deploy_and_register(mgr, &id, &validated).await {
        mgr.remove(&id).await;
        return resp;
    }

    log::info!("Deployed program: {} ({})", validated.config.name, id);

    (
        StatusCode::CREATED,
        Json(CreateProgramResponse {
            id,
            name: validated.config.name,
            status: ProgramStatus::Stopped,
        }),
    )
        .into_response()
}

// ============================================================================
// PUT /api/programs/:id
// ============================================================================

/// Update an existing program with a new `.efapp` bundle.
pub async fn update_program<T: ProgramsContext>(
    State(ctx): State<Arc<T>>,
    Path(id): Path<String>,
    body: Bytes,
) -> axum::response::Response {
    let mgr = ctx.program_manager();

    if !mgr.exists(&id).await {
        return error_response(StatusCode::NOT_FOUND, "Program not found", "NOT_FOUND");
    }

    let validated = match validate_bundle_or_error(body).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // Stop if running
    let was_running = service::is_active(&id, mgr.system_mode()).await;
    if was_running {
        stop_runtime_and_service(mgr, &id).await;
    }

    // Deploy updated bundle
    if let Err(resp) = deploy_and_register(mgr, &id, &validated).await {
        return resp;
    }

    // Restart if it was running before
    let mut status = ProgramStatus::Stopped;
    if was_running {
        if let Err(e) = service::start_service(&id, mgr.system_mode()).await {
            log::error!("Failed to restart {id} after update: {e}");
            status = ProgramStatus::Error;
        } else {
            start_log_follower(mgr, &id).await;
            status = ProgramStatus::Running;
        }
    }

    log::info!("Updated program: {id}");

    (
        StatusCode::OK,
        Json(CreateProgramResponse {
            id,
            name: validated.config.name,
            status,
        }),
    )
        .into_response()
}

// ============================================================================
// GET /api/programs/:id (download)
// ============================================================================

/// Download the `.efapp` bundle for re-editing or sharing.
pub async fn download_program<T: ProgramsContext>(
    State(ctx): State<Arc<T>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let mgr = ctx.program_manager();

    if !mgr.exists(&id).await {
        return error_response(StatusCode::NOT_FOUND, "Program not found", "NOT_FOUND");
    }

    let efapp_path = mgr.program_dir(&id).join("original.efapp");
    let data = match tokio::fs::read(&efapp_path).await {
        Ok(d) => d,
        Err(e) => {
            log::error!("Failed to read {}: {e}", efapp_path.display());
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read program bundle",
                "READ_ERROR",
            );
        }
    };

    // Use the slug ID for the filename — safe for HTTP headers
    let filename = format!("{id}.efapp");
    let headers = [
        (header::CONTENT_TYPE, "application/zip".to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        ),
    ];

    (StatusCode::OK, headers, data).into_response()
}

// ============================================================================
// DELETE /api/programs/:id
// ============================================================================

/// Delete a program — stops it if running, removes all files and the systemd unit.
pub async fn delete_program<T: ProgramsContext>(
    State(ctx): State<Arc<T>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let mgr = ctx.program_manager();

    if !mgr.exists(&id).await {
        return error_response(StatusCode::NOT_FOUND, "Program not found", "NOT_FOUND");
    }

    // Stop if running
    let sm = mgr.system_mode();
    if service::is_active(&id, sm).await {
        stop_runtime_and_service(mgr, &id).await;
    }

    // Remove systemd unit file (includes disable)
    if let Err(e) = service::remove_unit_file(&id, sm).await {
        log::warn!("Failed to remove unit file for {id}: {e}");
    }

    if let Err(e) = service::daemon_reload(sm).await {
        log::warn!("daemon-reload failed after deleting {id}: {e}");
    }

    // Delete files
    let deleted = match mgr.delete_files(&id).await {
        Ok(()) => true,
        Err(e) => {
            log::error!("Failed to delete files for {id}: {e}");
            false
        }
    };

    // Remove from in-memory state
    mgr.remove(&id).await;

    log::info!("Deleted program: {id}");

    (StatusCode::OK, Json(DeleteProgramResponse { id, deleted })).into_response()
}

// ============================================================================
// POST /api/programs/:id/start
// ============================================================================

/// Start a stopped program.
pub async fn start_program<T: ProgramsContext>(
    State(ctx): State<Arc<T>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let mgr = ctx.program_manager();

    if !mgr.exists(&id).await {
        return error_response(StatusCode::NOT_FOUND, "Program not found", "NOT_FOUND");
    }

    if service::is_active(&id, mgr.system_mode()).await {
        return error_response(
            StatusCode::CONFLICT,
            "Program is already running",
            "ALREADY_RUNNING",
        );
    }

    if let Err(e) = service::start_service(&id, mgr.system_mode()).await {
        log::error!("Failed to start {id}: {e}");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to start program: {e}"),
            "START_ERROR",
        );
    }

    start_log_follower(mgr, &id).await;

    log::info!("Started program: {id}");

    (
        StatusCode::OK,
        Json(ProgramActionResponse {
            id,
            status: ProgramStatus::Running,
        }),
    )
        .into_response()
}

// ============================================================================
// POST /api/programs/:id/stop
// ============================================================================

/// Stop a running program.
pub async fn stop_program<T: ProgramsContext>(
    State(ctx): State<Arc<T>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let mgr = ctx.program_manager();

    if !mgr.exists(&id).await {
        return error_response(StatusCode::NOT_FOUND, "Program not found", "NOT_FOUND");
    }

    if !service::is_active(&id, mgr.system_mode()).await {
        return error_response(
            StatusCode::CONFLICT,
            "Program is not running",
            "NOT_RUNNING",
        );
    }

    abort_log_follower(mgr, &id).await;

    if let Err(e) = service::stop_service(&id, mgr.system_mode()).await {
        log::error!("Failed to stop {id}: {e}");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to stop program: {e}"),
            "STOP_ERROR",
        );
    }

    log::info!("Stopped program: {id}");

    (
        StatusCode::OK,
        Json(ProgramActionResponse {
            id,
            status: ProgramStatus::Stopped,
        }),
    )
        .into_response()
}

// ============================================================================
// GET /api/programs/:id/status
// ============================================================================

/// Get detailed runtime status for a program.
pub async fn program_status<T: ProgramsContext>(
    State(ctx): State<Arc<T>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let mgr = ctx.program_manager();

    let meta = match mgr.get(&id).await {
        Some(m) => m,
        None => {
            return error_response(StatusCode::NOT_FOUND, "Program not found", "NOT_FOUND");
        }
    };

    let sm = mgr.system_mode();
    let status = live_status(&id, sm).await;

    let (pid, uptime, memory_mb, started_at) = if status == ProgramStatus::Running {
        let props = service::service_properties(&id, sm)
            .await
            .unwrap_or_default();
        let pid = (props.main_pid > 0).then_some(props.main_pid);
        let memory_mb = pid.and_then(get_process_memory);

        // Compute uptime from monotonic timestamp (microseconds since boot)
        let uptime = if props.active_enter_usec > 0 {
            get_uptime_from_monotonic(props.active_enter_usec)
        } else {
            None
        };

        // Convert monotonic timestamp to ISO 8601 for started_at
        let started_at = if props.active_enter_usec > 0 {
            monotonic_to_iso8601(props.active_enter_usec)
        } else {
            None
        };

        (pid, uptime, memory_mb, started_at)
    } else {
        (None, None, None, None)
    };

    (
        StatusCode::OK,
        Json(ProgramStatusResponse {
            id,
            name: meta.name,
            status,
            pid,
            uptime,
            memory_mb,
            started_at,
        }),
    )
        .into_response()
}

/// Get resident memory for a PID via sysinfo, in whole megabytes.
fn get_process_memory(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, System};
    let mut sys = System::new();
    sys.refresh_processes(
        sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
    );
    sys.process(Pid::from_u32(pid))
        .map(|p| p.memory() / (1024 * 1024))
}

/// Compute uptime in seconds from a systemd monotonic timestamp (microseconds).
fn get_uptime_from_monotonic(active_enter_usec: u64) -> Option<u64> {
    // Read system uptime from /proc/uptime (Linux only)
    let uptime_str = std::fs::read_to_string("/proc/uptime").ok()?;
    let system_uptime_secs: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;
    let system_uptime_usec = (system_uptime_secs * 1_000_000.0) as u64;

    if active_enter_usec <= system_uptime_usec {
        Some((system_uptime_usec - active_enter_usec) / 1_000_000)
    } else {
        None
    }
}

/// Convert a systemd monotonic timestamp to an ISO 8601 UTC string.
fn monotonic_to_iso8601(active_enter_usec: u64) -> Option<String> {
    let uptime_str = std::fs::read_to_string("/proc/uptime").ok()?;
    let system_uptime_secs: f64 = uptime_str.split_whitespace().next()?.parse().ok()?;
    let system_uptime_usec = (system_uptime_secs * 1_000_000.0) as u64;

    if active_enter_usec <= system_uptime_usec {
        let elapsed_usec = system_uptime_usec - active_enter_usec;
        let now = chrono::Utc::now();
        let started = now - chrono::Duration::microseconds(elapsed_usec as i64);
        Some(started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    } else {
        None
    }
}

// ============================================================================
// GET /api/programs/:id/logs (WebSocket)
// ============================================================================

/// Query parameters for the logs WebSocket endpoint.
#[derive(Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_tail")]
    pub tail: usize,
}

fn default_tail() -> usize {
    50
}

/// WebSocket endpoint for streaming live program logs.
pub async fn program_logs<T: ProgramsContext>(
    State(ctx): State<Arc<T>>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
    ws: yawc::IncomingUpgrade,
) -> axum::response::Response {
    let mgr = ctx.program_manager();

    if !mgr.exists(&id).await {
        return error_response(StatusCode::NOT_FOUND, "Program not found", "NOT_FOUND");
    }

    let options = yawc::Options::default().with_compression_level(yawc::CompressionLevel::fast());
    let (response, ws_future) = match ws.upgrade(options) {
        Ok(pair) => pair,
        Err(e) => {
            log::error!("WebSocket upgrade failed for /api/programs/{id}/logs: {e}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("WebSocket upgrade failed: {e}"),
                "WEBSOCKET_ERROR",
            );
        }
    };

    // Clone Arcs outside the lock to avoid holding it across awaits
    let runtimes = mgr.runtimes().read().await;
    let runtime_data = runtimes
        .get(&id)
        .map(|rt| (rt.log_buffer.clone(), rt.subscribe()));
    drop(runtimes);

    let is_running = service::is_active(&id, mgr.system_mode()).await;

    tokio::spawn(async move {
        let Ok(ws) = ws_future.await else { return };
        let (mut sink, mut incoming) = futures::StreamExt::split(ws);

        // If program is not running and has no runtime, send one informational
        // message then hold the connection open (with ping/pong keepalive) until
        // the client disconnects. Closing immediately causes the client to
        // reconnect in a tight loop, flooding the log view.
        if !is_running && runtime_data.is_none() {
            let msg = serde_json::json!({
                "timestamp": chrono::Utc::now().to_rfc3339(),
                "level": "info",
                "message": "Program is not running. Start the program to see logs."
            })
            .to_string();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                futures::SinkExt::send(&mut sink, yawc::frame::Frame::text(msg)),
            )
            .await;

            // Hold connection open until client disconnects
            let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
            ping_interval.tick().await;
            loop {
                tokio::select! {
                    frame = futures::StreamExt::next(&mut incoming) => {
                        match frame {
                            Some(f) if f.opcode() == yawc::frame::OpCode::Close => break,
                            None => break,
                            _ => {}
                        }
                    }
                    _ = ping_interval.tick() => {
                        let ping = yawc::frame::Frame::ping(&[][..]);
                        if tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            futures::SinkExt::send(&mut sink, ping),
                        ).await.is_err() {
                            break;
                        }
                    }
                }
            }
            return;
        }

        if let Some((log_buffer, mut rx)) = runtime_data {
            // Send tail lines
            let tail_lines = log_buffer.read().await.tail(query.tail);
            for line in tail_lines {
                let frame = yawc::frame::Frame::text(line);
                if tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    futures::SinkExt::send(&mut sink, frame),
                )
                .await
                .is_err()
                {
                    return;
                }
            }

            // Stream live lines
            let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
            ping_interval.tick().await;

            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Ok(line) => {
                                let frame = yawc::frame::Frame::text(line);
                                match tokio::time::timeout(
                                    std::time::Duration::from_secs(5),
                                    futures::SinkExt::send(&mut sink, frame),
                                ).await {
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
                            Some(f) if f.opcode() == yawc::frame::OpCode::Close => break,
                            None => break,
                            _ => {}
                        }
                    }
                    _ = ping_interval.tick() => {
                        let ping = yawc::frame::Frame::ping(&[][..]);
                        if tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            futures::SinkExt::send(&mut sink, ping),
                        ).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    response.into_response()
}

// ============================================================================
// GET /api/programs/:id/icon
// ============================================================================

/// Default icon embedded at compile time.
const DEFAULT_ICON: &[u8] = include_bytes!("default_icon.png");

/// Serve the program's icon, falling back to a default.
pub async fn program_icon<T: ProgramsContext>(
    State(ctx): State<Arc<T>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    let mgr = ctx.program_manager();

    if !mgr.exists(&id).await {
        return error_response(StatusCode::NOT_FOUND, "Program not found", "NOT_FOUND");
    }

    let icon_path = mgr.program_dir(&id).join("icon.png");
    let data = if icon_path.exists() {
        tokio::fs::read(&icon_path)
            .await
            .unwrap_or_else(|_| DEFAULT_ICON.to_vec())
    } else {
        DEFAULT_ICON.to_vec()
    };

    (StatusCode::OK, [(header::CONTENT_TYPE, "image/png")], data).into_response()
}
