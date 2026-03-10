// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Recording and replay control handlers.
//!
//! Provides endpoints for:
//! - Starting/stopping recording
//! - Starting/stopping replay
//! - Checking recorder and replay status

use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use sysinfo::System;

// ============================================================================
// Types
// ============================================================================

/// Playback request parameters
#[derive(Deserialize, Debug, Clone)]
pub struct PlaybackParams {
    pub directory: String,
    pub file: String,
}

/// Playback response
#[derive(Serialize)]
pub struct PlaybackResponse {
    pub status: String,
    pub message: String,
    pub current_file: Option<String>,
}

/// Delete request parameters
#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct DeleteParams {
    pub directory: String,
    pub file: String,
    pub url: Option<String>,
    pub jwt: Option<String>,
    pub topic: Option<String>,
}

/// Isolate system request parameters
#[derive(Deserialize)]
pub struct IsolateParams {
    pub target: String,
}

// ============================================================================
// Status Helpers
// ============================================================================

/// Check recorder status in user mode (process-based)
pub async fn user_mode_check_recorder_status() -> String {
    let mut sys = System::new_all();
    sys.refresh_all();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    if let Some(process) = sys.processes().values().find(|process| {
        let name = process.name().to_string_lossy();
        name.contains("edgefirst-recor")
    }) {
        match process.status() {
            sysinfo::ProcessStatus::Run | sysinfo::ProcessStatus::Sleep => {
                // Store the PID for future checks
                if let Err(e) =
                    std::fs::write("/var/run/edgefirst-recorder.pid", process.pid().to_string())
                {
                    debug!("Failed to write PID file: {}", e);
                }
                return "Recorder is running".to_string();
            }
            _ => {
                let _ = std::fs::remove_file("/var/run/edgefirst-recorder.pid");
            }
        }
    }
    "Recorder is not running".to_string()
}

/// Extract recording filename from systemctl status output
pub fn extract_recording_filename(status_output: &str) -> Option<String> {
    for line in status_output.lines() {
        if line.contains("Recording to") {
            let parts: Vec<&str> = line.split("Recording to ").collect();
            if parts.len() > 1 {
                let path = parts[1].trim();
                if let Some(filename) = path.split('/').next_back() {
                    return Some(filename.replace("…", "").trim().to_string());
                }
            }
        }
    }
    None
}

// ============================================================================
// Recording Handlers
// ============================================================================

/// Trait for accessing server context in recording handlers
pub trait RecordingContext: Send + Sync + 'static {
    fn storage_path(&self) -> &str;
    fn process(&self) -> &Mutex<Option<std::process::Child>>;
}

/// Check recorder status (system mode)
pub async fn check_recorder_status() -> impl IntoResponse {
    let service_status = Command::new("systemctl")
        .arg("is-active")
        .arg("recorder")
        .output();

    match service_status {
        Ok(output) => {
            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if status == "active" {
                "Recorder is running".into_response()
            } else {
                "Recorder is not running".into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error checking service status: {:?}", e),
        )
            .into_response(),
    }
}

/// Start recording (system mode)
pub async fn start<T: RecordingContext>(State(data): State<Arc<T>>) -> impl IntoResponse {
    let mut command = Command::new("sudo");
    command.arg("systemctl").arg("start").arg("recorder");

    let process = match command.spawn() {
        Ok(p) => p,
        Err(e) => {
            let error_message = format!("Failed to start recorder: {:?}", e);
            error!("{}", error_message);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "message": error_message
                })),
            )
                .into_response();
        }
    };

    // Store the process ID before taking the mutex lock
    let pid = process.id();

    // Update the process in the mutex
    {
        let mut process_guard = data.process().lock().unwrap();
        *process_guard = Some(process);
    }

    // Verify the process is running
    if let Err(e) = Command::new("systemctl")
        .arg("is-active")
        .arg("recorder")
        .status()
    {
        let error_message = format!("Failed to verify recorder status: {:?}", e);
        error!("{}", error_message);
        // Clean up the process if verification fails
        {
            let mut process_guard = data.process().lock().unwrap();
            if let Some(mut process) = process_guard.take() {
                let _ = process.kill();
            }
        }
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "status": "error",
                "message": error_message
            })),
        )
            .into_response();
    }

    info!("Recorder started with PID: {}", pid);
    (
        StatusCode::OK,
        Json(json!({
            "status": "started",
            "message": "Recording started successfully"
        })),
    )
        .into_response()
}

/// Start recording (user mode)
pub async fn user_mode_start<T: RecordingContext>(State(data): State<Arc<T>>) -> impl IntoResponse {
    let status_str = user_mode_check_recorder_status().await;
    debug!("Current recorder status: {}", status_str);

    if status_str == "Recorder is running" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "status": "error",
                "message": "Recorder is already running"
            })),
        )
            .into_response();
    }
    let mut command = Command::new("edgefirst-recorder");
    command
        .env("STORAGE", data.storage_path())
        .arg("--all-topics");

    debug!("Starting recorder with command: {:?}", command);

    let process = match command.spawn() {
        Ok(p) => {
            debug!("Recorder process started with PID: {:?}", p.id());
            p
        }
        Err(e) => {
            let error_message = format!("Failed to start recorder: {:?}", e);
            error!("{}", error_message);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "message": error_message
                })),
            )
                .into_response();
        }
    };

    let pid = process.id();
    {
        let mut process_guard = data.process().lock().unwrap();
        *process_guard = Some(process);
    }

    // Wait for recorder to start
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let status = user_mode_check_recorder_status().await;
        if status == "Recorder is running" {
            return (
                StatusCode::OK,
                Json(json!({
                    "status": "started",
                    "message": "Recording started successfully"
                })),
            )
                .into_response();
        }
    }

    let process_status = {
        let process = {
            let mut process_guard = data.process().lock().unwrap();
            process_guard.take()
        };
        if let Some(mut process) = process {
            let status = process.try_wait();
            // Put the process back if it's still running
            if let Ok(None) = status {
                let mut process_guard = data.process().lock().unwrap();
                *process_guard = Some(process);
            }
            status
        } else {
            Ok(None)
        }
    };

    match process_status {
        Ok(Some(status)) => {
            let error_message = format!("Recorder process exited with status: {:?}", status);
            error!("{}", error_message);
            {
                let mut process_guard = data.process().lock().unwrap();
                *process_guard = None;
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "message": error_message
                })),
            )
                .into_response()
        }
        Ok(None) => {
            debug!("Creating PID file manually");
            if let Err(e) = std::fs::write("/var/run/recorder.pid", pid.to_string()) {
                let error_message = format!("Failed to create PID file: {:?}", e);
                error!("{}", error_message);
                {
                    let mut process_guard = data.process().lock().unwrap();
                    if let Some(mut process) = process_guard.take() {
                        let _ = process.kill();
                    }
                }
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "status": "error",
                        "message": error_message
                    })),
                )
                    .into_response();
            }
            (
                StatusCode::OK,
                Json(json!({
                    "status": "started",
                    "message": "Recording started successfully"
                })),
            )
                .into_response()
        }
        Err(e) => {
            let error_message = format!("Error checking process status: {:?}", e);
            error!("{}", error_message);
            {
                let mut process_guard = data.process().lock().unwrap();
                *process_guard = None;
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "message": error_message
                })),
            )
                .into_response()
        }
    }
}

/// Stop recording (system mode)
pub async fn stop<T: RecordingContext>(State(data): State<Arc<T>>) -> impl IntoResponse {
    let mut command = Command::new("sudo");
    command.arg("systemctl").arg("stop").arg("recorder");

    match command.status() {
        Ok(status) if status.success() => {
            {
                let mut process_guard = data.process().lock().unwrap();
                *process_guard = None;
            }
            info!("Recorder service stopped");
            (
                StatusCode::OK,
                Json(json!({
                    "status": "stopped",
                    "message": "Recording stopped successfully"
                })),
            )
                .into_response()
        }
        Ok(status) => {
            let error_message = format!("Failed to stop recorder service: {:?}", status);
            error!("{}", error_message);
            (StatusCode::INTERNAL_SERVER_ERROR, error_message).into_response()
        }
        Err(e) => {
            let error_message = format!("Failed to run systemctl stop recorder: {:?}", e);
            error!("{}", error_message);
            (StatusCode::INTERNAL_SERVER_ERROR, error_message).into_response()
        }
    }
}

/// Stop recording (user mode)
pub async fn user_mode_stop<T: RecordingContext>(State(data): State<Arc<T>>) -> impl IntoResponse {
    let pid_file = Path::new("/var/run/edgefirst-recorder.pid");
    let process = {
        let mut process_guard = data.process().lock().unwrap();
        process_guard.take()
    };

    if let Some(process) = process {
        debug!(
            "Attempting to stop recorder process with PID: {:?}",
            process.id()
        );
        let mut sys = System::new_all();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        if let Some(process_info) = sys.process(sysinfo::Pid::from(process.id() as usize)) {
            if process_info.kill_with(sysinfo::Signal::Interrupt).is_none() {
                error!("Failed to send SIGINT to process: {:?}", process.id());
            }
        }
    }

    let mut sys = System::new_all();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    if pid_file.exists() {
        if let Err(e) = std::fs::remove_file(pid_file) {
            error!("Failed to remove PID file: {:?}", e);
        }
    }
    std::thread::sleep(std::time::Duration::from_secs(2));

    debug!("All recorder processes stopped successfully");
    Json(json!({
        "status": "stopped",
        "message": "Recording stopped successfully"
    }))
    .into_response()
}

/// Delete a file
pub async fn delete(Json(params): Json<DeleteParams>) -> impl IntoResponse {
    let file_path = format!("{}/{}", params.directory, params.file);
    debug!("Attempting to delete file: {}", file_path);

    if Path::new(&file_path).exists() {
        let mut command = Command::new("sudo");
        command.arg("rm").arg("-rf").arg(&file_path);

        debug!("Command: {:?}", command);

        match command.status() {
            Ok(status) if status.success() => {
                info!("File deleted successfully");
                (
                    StatusCode::OK,
                    Json(json!({
                        "status": "removed",
                        "message": "File deleted successfully"
                    })),
                )
                    .into_response()
            }
            Ok(status) => {
                let error_message = format!("Failed to delete file: {:?}", status);
                error!("{}", error_message);
                (StatusCode::INTERNAL_SERVER_ERROR, error_message).into_response()
            }
            Err(e) => {
                let error_message = format!("Failed to run rm -rf: {:?}", e);
                error!("{}", error_message);
                (StatusCode::INTERNAL_SERVER_ERROR, error_message).into_response()
            }
        }
    } else {
        (StatusCode::NOT_FOUND, "File not found").into_response()
    }
}

/// Get current recording filename
pub async fn get_current_recording() -> impl IntoResponse {
    let status = Command::new("systemctl")
        .arg("status")
        .arg("recorder")
        .output();

    match status {
        Ok(output) => {
            let status_str = String::from_utf8_lossy(&output.stdout);
            match extract_recording_filename(&status_str) {
                Some(filename) => (
                    StatusCode::OK,
                    Json(json!({
                        "status": "recording",
                        "filename": filename
                    })),
                )
                    .into_response(),
                None => (
                    StatusCode::OK,
                    Json(json!({
                        "status": "not_recording",
                        "filename": null
                    })),
                )
                    .into_response(),
            }
        }
        Err(e) => {
            error!("Error checking recorder status: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "message": format!("Error checking recorder status: {}", e)
                })),
            )
                .into_response()
        }
    }
}

// ============================================================================
// Replay Handlers
// ============================================================================

/// Start replay
pub async fn start_replay(Json(params): Json<PlaybackParams>) -> impl IntoResponse {
    let file_path = format!("{}/{}", params.directory, params.file);
    debug!("Attempting to play MCAP file: {}", file_path);

    if !Path::new(&file_path).exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(PlaybackResponse {
                status: "error".to_string(),
                message: "File not found".to_string(),
                current_file: None,
            }),
        )
            .into_response();
    }

    let status = Command::new("systemctl")
        .arg("is-active")
        .arg("replay")
        .output();

    match status {
        Ok(output) => {
            let status_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if status_str == "active" {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(PlaybackResponse {
                        status: "error".to_string(),
                        message: "Replay service is already running".to_string(),
                        current_file: None,
                    }),
                )
                    .into_response();
            }
        }
        Err(e) => {
            error!("Error checking replay service status: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PlaybackResponse {
                    status: "error".to_string(),
                    message: format!("Error checking service status: {}", e),
                    current_file: None,
                }),
            )
                .into_response();
        }
    }

    unsafe { std::env::set_var("MCAP_FILE", &file_path) }

    let result = Command::new("sudo")
        .arg("systemctl")
        .arg("start")
        .arg("replay")
        .status();

    match result {
        Ok(status) if status.success() => {
            info!(
                "Replay service started successfully with file: {}",
                file_path
            );
            (
                StatusCode::OK,
                Json(PlaybackResponse {
                    status: "success".to_string(),
                    message: "Replay service started successfully".to_string(),
                    current_file: Some(params.file.clone()),
                }),
            )
                .into_response()
        }
        Ok(status) => {
            error!("Failed to start replay service: {:?}", status);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PlaybackResponse {
                    status: "error".to_string(),
                    message: "Failed to start replay service".to_string(),
                    current_file: None,
                }),
            )
                .into_response()
        }
        Err(e) => {
            error!("Error starting replay service: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PlaybackResponse {
                    status: "error".to_string(),
                    message: format!("Error starting replay service: {}", e),
                    current_file: None,
                }),
            )
                .into_response()
        }
    }
}

/// Stop replay
pub async fn stop_replay() -> impl IntoResponse {
    let result = Command::new("sudo")
        .arg("systemctl")
        .arg("stop")
        .arg("replay")
        .status();

    match result {
        Ok(status) if status.success() => {
            info!("Replay service stopped successfully");
            (
                StatusCode::OK,
                Json(PlaybackResponse {
                    status: "success".to_string(),
                    message: "Replay service stopped successfully".to_string(),
                    current_file: None,
                }),
            )
                .into_response()
        }
        Ok(status) => {
            error!("Failed to stop replay service: {:?}", status);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PlaybackResponse {
                    status: "error".to_string(),
                    message: "Failed to stop replay service".to_string(),
                    current_file: None,
                }),
            )
                .into_response()
        }
        Err(e) => {
            error!("Error stopping replay service: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PlaybackResponse {
                    status: "error".to_string(),
                    message: format!("Error stopping replay service: {}", e),
                    current_file: None,
                }),
            )
                .into_response()
        }
    }
}

/// Check replay status (user mode)
pub async fn user_mode_check_replay_status() -> impl IntoResponse {
    let pid_file = Path::new("/var/run/replay.pid");

    if !pid_file.exists() {
        return (
            StatusCode::OK,
            Json(json!({
                "status": "not_running",
                "message": "Replay is not running"
            })),
        )
            .into_response();
    }

    match std::fs::read_to_string(pid_file) {
        Ok(pid_str) => {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                let status = Command::new("ps").arg("-p").arg(pid.to_string()).output();

                match status {
                    Ok(output) => {
                        if output.status.success() {
                            (
                                StatusCode::OK,
                                Json(json!({
                                    "status": "running",
                                    "message": "Replay is running"
                                })),
                            )
                                .into_response()
                        } else {
                            // Process exists but not running, clean up PID file
                            let _ = std::fs::remove_file(pid_file);
                            (
                                StatusCode::OK,
                                Json(json!({
                                    "status": "not_running",
                                    "message": "Replay is not running"
                                })),
                            )
                                .into_response()
                        }
                    }
                    Err(e) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "status": "error",
                            "message": format!("Error checking process status: {:?}", e)
                        })),
                    )
                        .into_response(),
                }
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "status": "error",
                        "message": "Invalid PID in PID file"
                    })),
                )
                    .into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "status": "error",
                "message": format!("Error reading PID file: {:?}", e)
            })),
        )
            .into_response(),
    }
}

/// Check replay status (system mode)
pub async fn check_replay_status() -> impl IntoResponse {
    let service_status = Command::new("systemctl")
        .arg("is-active")
        .arg("replay")
        .output();

    match service_status {
        Ok(output) => {
            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if status == "active" {
                "Replay is running".into_response()
            } else {
                "Replay is not running".into_response()
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error checking service status: {:?}", e),
        )
            .into_response(),
    }
}

/// Isolate system to a target
pub async fn isolate_system(Json(params): Json<IsolateParams>) -> impl IntoResponse {
    let target = format!("{}.target", params.target);
    debug!("Attempting to isolate system to target: {}", target);

    let result = Command::new("sudo")
        .arg("systemctl")
        .arg("isolate")
        .arg(&target)
        .status();

    match result {
        Ok(status) if status.success() => {
            info!("System successfully isolated to {}", target);
            (
                StatusCode::OK,
                Json(json!({
                    "status": "success",
                    "message": format!("System isolated to {}", target)
                })),
            )
                .into_response()
        }
        Ok(status) => {
            error!("Failed to isolate system: {:?}", status);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "message": format!("Failed to isolate system to {}", target)
                })),
            )
                .into_response()
        }
        Err(e) => {
            error!("Error isolating system: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "message": format!("Error isolating system: {}", e)
                })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_recording_filename() {
        let status_output = r#"
● recorder.service - MCAP Recorder
   Loaded: loaded
   Active: active (running)
   CGroup: Recording to /data/recordings/test_2024-01-15.mcap
"#;
        let result = extract_recording_filename(status_output);
        assert_eq!(result, Some("test_2024-01-15.mcap".to_string()));
    }

    #[test]
    fn test_extract_recording_filename_truncated() {
        let status_output = r#"
● recorder.service - MCAP Recorder
   Recording to /data/recordings/very_long_filename…
"#;
        let result = extract_recording_filename(status_output);
        assert!(result.is_some());
        assert!(!result.unwrap().contains('…'));
    }

    #[test]
    fn test_extract_recording_filename_not_found() {
        let status_output = r#"
● recorder.service - MCAP Recorder
   Loaded: loaded
   Active: inactive (dead)
"#;
        let result = extract_recording_filename(status_output);
        assert!(result.is_none());
    }

    #[test]
    fn test_playback_response_serialization() {
        let response = PlaybackResponse {
            status: "success".to_string(),
            message: "Started".to_string(),
            current_file: Some("test.mcap".to_string()),
        };

        let json = serde_json::to_string(&response).expect("Failed to serialize");
        assert!(json.contains("\"status\":\"success\""));
        assert!(json.contains("\"current_file\":\"test.mcap\""));
    }
}
