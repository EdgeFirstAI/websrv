// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

mod args;
use crate::args::WebUISettings;
use actix::prelude::*;
use actix_files::{self as fs, NamedFile};
use actix_web::{
    http::header::ContentLength,
    web::{self, Bytes},
    App, HttpRequest, HttpResponse, HttpServer, Responder, Result,
};
use actix_web_actors::ws;
use actix_web_lab::middleware::RedirectHttps;
use anyhow::{Context, Result as res};
use args::Args;
use async_stream::stream;
use camino::Utf8Path;
use cdr::{CdrLe, Infinite};
use chrono::{DateTime, Utc};
use clap::Parser;
use edgefirst_client::{Client, Progress, ProjectID, SnapshotID};
use log::{debug, error, info, warn};
use mcap::Summary;
use memmap::Mmap;
use mime::Mime;
use openssl::{
    pkey::{PKey, Private},
    ssl::{SslAcceptor, SslMethod},
};
use percent_encoding::percent_decode;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufRead},
    path::{Path, PathBuf},
    process::{Child, Command},
    str::FromStr,
    sync::{
        atomic::{AtomicI64, Ordering},
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, UNIX_EPOCH},
};
use sysinfo::System;
use tokio::{
    io::AsyncReadExt as _,
    runtime::Handle,
    sync::{mpsc, RwLock},
    task::JoinHandle,
};
use uuid::Uuid;

#[derive(Serialize)]
struct FileInfo {
    name: String,
    size: u64, // Size in MB
    created: String,
    topics: HashMap<String, TopicInfo>,
    average_video_length: f64,
}
#[derive(Serialize)]
struct DirectoryResponse {
    dir_name: String,
    files: Option<Vec<FileInfo>>,
    message: Option<String>,
    topics: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
struct DeleteParams {
    directory: String,
    file: String,
    url: Option<String>,
    jwt: Option<String>,
    topic: Option<String>,
}

#[derive(Serialize)]
struct TopicInfo {
    message_count: usize,
    average_fps: f64,
    video_length: f64,
}

struct AppState {
    process: Mutex<Option<Child>>,
}

#[allow(dead_code)]
struct ThreadState {
    is_running: Mutex<bool>,
}

#[derive(Deserialize)]
struct ConfigPath {
    service: String,
}

// ============================================================================
// Upload Manager Data Structures
// ============================================================================

/// Unique identifier for an upload task
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UploadId(Uuid);

impl UploadId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

/// Upload mode selection
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum UploadMode {
    Basic,
    Extended {
        project_id: u64,
        labels: Vec<String>,
        dataset_name: Option<String>,
        dataset_description: Option<String>,
    },
}

/// Task lifecycle states
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UploadState {
    Queued,     // Created, not started
    Uploading,  // Uploading MCAP
    Processing, // Auto-labeling (Extended mode only)
    Completed,  // Success
    Failed,     // Error occurred
}

/// In-memory task representation
pub struct UploadTask {
    pub id: UploadId,
    pub mcap_path: PathBuf,
    pub mode: UploadMode,
    pub state: UploadState,
    pub progress: f32, // 0.0 to 100.0
    pub message: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub snapshot_id: Option<u64>,
    pub dataset_id: Option<u64>,
    pub error: Option<String>,
    pub task_handle: Option<JoinHandle<()>>,
}

/// Persistent status file format (.upload-status.json)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadStatus {
    pub upload_id: UploadId,
    pub mcap_path: PathBuf,
    pub mode: UploadMode,
    pub state: UploadState,
    pub progress: f32,
    pub message: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub snapshot_id: Option<u64>,
    pub dataset_id: Option<u64>,
    pub error: Option<String>,
}

impl From<&UploadTask> for UploadStatus {
    fn from(task: &UploadTask) -> Self {
        Self {
            upload_id: task.id,
            mcap_path: task.mcap_path.clone(),
            mode: task.mode.clone(),
            state: task.state,
            progress: task.progress,
            message: task.message.clone(),
            created_at: task.created_at,
            updated_at: task.updated_at,
            snapshot_id: task.snapshot_id,
            dataset_id: task.dataset_id,
            error: task.error.clone(),
        }
    }
}

/// Serializable task info for API responses
#[derive(Debug, Clone, Serialize)]
pub struct UploadTaskInfo {
    pub upload_id: UploadId,
    pub mcap_path: PathBuf,
    pub mode: UploadMode,
    pub state: UploadState,
    pub progress: f32,
    pub message: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub snapshot_id: Option<u64>,
    pub dataset_id: Option<u64>,
    pub error: Option<String>,
}

impl From<&UploadTask> for UploadTaskInfo {
    fn from(task: &UploadTask) -> Self {
        Self {
            upload_id: task.id,
            mcap_path: task.mcap_path.clone(),
            mode: task.mode.clone(),
            state: task.state,
            progress: task.progress,
            message: task.message.clone(),
            created_at: task.created_at,
            updated_at: task.updated_at,
            snapshot_id: task.snapshot_id,
            dataset_id: task.dataset_id,
            error: task.error.clone(),
        }
    }
}

/// Upload Manager - Manages upload tasks and EdgeFirst Studio client
pub struct UploadManager {
    tasks: Arc<RwLock<HashMap<UploadId, UploadTask>>>,
    client: Arc<RwLock<Option<Client>>>,
    storage_path: PathBuf,
    ws_broadcaster: Arc<MessageStream>,
}

impl UploadManager {
    /// Create a new UploadManager
    #[allow(private_interfaces)]
    pub fn new(storage_path: PathBuf, broadcaster: Arc<MessageStream>) -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            client: Arc::new(RwLock::new(None)),
            storage_path,
            ws_broadcaster: broadcaster,
        }
    }

    /// Initialize the upload manager and scan for incomplete uploads (power loss recovery)
    pub async fn initialize(&self) -> anyhow::Result<()> {
        info!(
            "Initializing UploadManager, scanning for incomplete uploads in {:?}",
            self.storage_path
        );

        // Scan for existing status files
        let status_files = Self::scan_status_files(&self.storage_path).await?;
        info!("Found {} upload status files", status_files.len());

        let mut recovered_count = 0;
        let mut tasks = self.tasks.write().await;

        for status_path in status_files {
            match Self::read_status_file(&status_path).await {
                Ok(status) => {
                    // Check if the upload was incomplete (not completed or failed)
                    let needs_recovery = matches!(
                        status.state,
                        UploadState::Queued | UploadState::Uploading | UploadState::Processing
                    );

                    if needs_recovery {
                        info!(
                            "Recovering incomplete upload {} for {:?} (was {:?})",
                            status.upload_id.0, status.mcap_path, status.state
                        );

                        // Create a failed task from the status
                        let task = UploadTask {
                            id: status.upload_id,
                            mcap_path: status.mcap_path.clone(),
                            mode: status.mode,
                            state: UploadState::Failed,
                            progress: status.progress,
                            message: "Server restart - upload incomplete".to_string(),
                            created_at: status.created_at,
                            updated_at: Utc::now(),
                            snapshot_id: status.snapshot_id,
                            dataset_id: status.dataset_id,
                            error: Some("Upload interrupted by server restart".to_string()),
                            task_handle: None,
                        };

                        // Update the status file
                        if let Err(e) = Self::write_status_file(&task).await {
                            error!("Failed to update status file for recovered upload: {}", e);
                        }

                        tasks.insert(status.upload_id, task);
                        recovered_count += 1;
                    } else {
                        // Load completed/failed uploads into memory for status tracking
                        let task = UploadTask {
                            id: status.upload_id,
                            mcap_path: status.mcap_path,
                            mode: status.mode,
                            state: status.state,
                            progress: status.progress,
                            message: status.message,
                            created_at: status.created_at,
                            updated_at: status.updated_at,
                            snapshot_id: status.snapshot_id,
                            dataset_id: status.dataset_id,
                            error: status.error,
                            task_handle: None,
                        };
                        tasks.insert(status.upload_id, task);
                    }
                }
                Err(e) => {
                    error!("Failed to read status file {:?}: {}", status_path, e);
                }
            }
        }

        if recovered_count > 0 {
            info!(
                "Recovered {} incomplete uploads (marked as failed)",
                recovered_count
            );
        }

        Ok(())
    }

    /// Authenticate with EdgeFirst Studio
    pub async fn authenticate(&self, username: &str, password: &str) -> anyhow::Result<()> {
        info!("Authenticating with EdgeFirst Studio as {}", username);

        // Create new client and authenticate
        let client = Client::new()?;
        let authenticated_client = client
            .with_login(username, password)
            .await
            .map_err(|e| anyhow::anyhow!("Authentication failed: {}", e))?;

        // Store authenticated client
        let mut client_lock = self.client.write().await;
        *client_lock = Some(authenticated_client);

        info!("Successfully authenticated with EdgeFirst Studio");
        Ok(())
    }

    /// Logout from EdgeFirst Studio
    pub async fn logout(&self) -> anyhow::Result<()> {
        info!("Logging out from EdgeFirst Studio");
        let mut client_lock = self.client.write().await;
        *client_lock = None;
        Ok(())
    }

    /// Get the username of the authenticated user
    pub async fn get_username(&self) -> Option<String> {
        let client_lock = self.client.read().await;
        if let Some(client) = client_lock.as_ref() {
            client.username().await.ok()
        } else {
            None
        }
    }

    // =========================================================================
    // Status File Management
    // =========================================================================

    /// Generate the status file path for a given MCAP file
    /// Returns: /path/to/file.mcap.upload-status.json
    fn get_status_file_path(mcap_path: &Path) -> PathBuf {
        let mut status_path = mcap_path.to_path_buf();
        let new_extension = match mcap_path.extension() {
            Some(ext) => format!("{}.upload-status.json", ext.to_string_lossy()),
            None => "upload-status.json".to_string(),
        };
        status_path.set_extension(new_extension);
        status_path
    }

    /// Write the upload status to a JSON file alongside the MCAP file
    async fn write_status_file(task: &UploadTask) -> anyhow::Result<()> {
        let status_path = Self::get_status_file_path(&task.mcap_path);
        let status: UploadStatus = task.into();
        let json = serde_json::to_string_pretty(&status)?;

        // Use sync file I/O in a blocking task to avoid holding locks across await
        tokio::task::spawn_blocking(move || {
            std::fs::write(&status_path, json).map_err(|e| {
                anyhow::anyhow!("Failed to write status file {:?}: {}", status_path, e)
            })
        })
        .await??;

        Ok(())
    }

    /// Read the upload status from a JSON file
    async fn read_status_file(path: &Path) -> anyhow::Result<UploadStatus> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| anyhow::anyhow!("Failed to read status file {:?}: {}", path, e))?;
            let status: UploadStatus = serde_json::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse status file {:?}: {}", path, e))?;
            Ok(status)
        })
        .await?
    }

    /// Scan storage directory for upload status files
    async fn scan_status_files(storage_path: &Path) -> anyhow::Result<Vec<PathBuf>> {
        let storage_path = storage_path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            let mut status_files = Vec::new();

            fn scan_dir(dir: &Path, status_files: &mut Vec<PathBuf>) -> std::io::Result<()> {
                if dir.is_dir() {
                    for entry in std::fs::read_dir(dir)? {
                        let entry = entry?;
                        let path = entry.path();
                        if path.is_dir() {
                            scan_dir(&path, status_files)?;
                        } else if path
                            .file_name()
                            .map(|n| n.to_string_lossy().ends_with(".upload-status.json"))
                            .unwrap_or(false)
                        {
                            status_files.push(path);
                        }
                    }
                }
                Ok(())
            }

            scan_dir(&storage_path, &mut status_files)
                .map_err(|e| anyhow::anyhow!("Failed to scan storage directory: {}", e))?;

            Ok(status_files)
        })
        .await?
    }

    /// Check if authenticated
    pub async fn is_authenticated(&self) -> bool {
        let client_lock = self.client.read().await;
        client_lock.is_some()
    }

    /// Get a clone of the authenticated client
    async fn get_client(&self) -> Option<Client> {
        let client_lock = self.client.read().await;
        client_lock.clone()
    }

    /// Update task state and write status file
    #[allow(clippy::too_many_arguments)]
    async fn update_task_state(
        &self,
        id: UploadId,
        state: UploadState,
        progress: f32,
        message: &str,
        snapshot_id: Option<u64>,
        dataset_id: Option<u64>,
        error: Option<String>,
    ) -> anyhow::Result<()> {
        let status: UploadStatus = {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.get_mut(&id) {
                task.state = state;
                task.progress = progress;
                task.message = message.to_string();
                task.updated_at = Utc::now();
                if snapshot_id.is_some() {
                    task.snapshot_id = snapshot_id;
                }
                if dataset_id.is_some() {
                    task.dataset_id = dataset_id;
                }
                if error.is_some() {
                    task.error = error;
                }
                // Convert to UploadStatus for file writing
                UploadStatus::from(&*task)
            } else {
                return Err(anyhow::anyhow!("Task {} not found", id.0));
            }
        };

        // Write status file
        Self::write_status_file_from_status(&status).await?;

        // Broadcast progress via WebSocket
        self.broadcast_progress(&status);

        Ok(())
    }

    /// Broadcast upload progress to all connected WebSocket clients
    fn broadcast_progress(&self, status: &UploadStatus) {
        match serde_json::to_vec(status) {
            Ok(json) => {
                self.ws_broadcaster.broadcast(BroadcastMessage(json));
            }
            Err(e) => {
                error!("Failed to serialize upload status for broadcast: {}", e);
            }
        }
    }

    /// Write the upload status to a JSON file (from UploadStatus directly)
    async fn write_status_file_from_status(status: &UploadStatus) -> anyhow::Result<()> {
        let status_path = Self::get_status_file_path(&status.mcap_path);
        let json = serde_json::to_string_pretty(&status)?;

        // Use sync file I/O in a blocking task
        let status_path_clone = status_path.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::write(&status_path_clone, json).map_err(|e| {
                anyhow::anyhow!("Failed to write status file {:?}: {}", status_path_clone, e)
            })
        })
        .await??;

        Ok(())
    }

    /// Delete the status file for a completed upload
    async fn delete_status_file(mcap_path: &Path) {
        let status_path = Self::get_status_file_path(mcap_path);
        if status_path.exists() {
            match tokio::task::spawn_blocking({
                let path = status_path.clone();
                move || std::fs::remove_file(&path)
            })
            .await
            {
                Ok(Ok(())) => {
                    info!("Cleaned up status file: {:?}", status_path);
                }
                Ok(Err(e)) => {
                    warn!("Failed to delete status file {:?}: {}", status_path, e);
                }
                Err(e) => {
                    warn!("Failed to spawn status file deletion task: {}", e);
                }
            }
        }
    }

    /// Start a new upload task
    pub async fn start_upload(
        self: &Arc<Self>,
        mcap_path: PathBuf,
        mode: UploadMode,
    ) -> anyhow::Result<UploadId> {
        let upload_id = UploadId::new();
        info!("Starting upload task {} for {:?}", upload_id.0, mcap_path);

        // Validate MCAP file exists
        if !mcap_path.exists() {
            return Err(anyhow::anyhow!("MCAP file not found: {:?}", mcap_path));
        }

        // Validate file is within storage_path (prevent path traversal)
        let canonical_mcap = mcap_path
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("Failed to resolve MCAP path: {}", e))?;
        let canonical_storage = self
            .storage_path
            .canonicalize()
            .unwrap_or_else(|_| self.storage_path.clone());
        if !canonical_mcap.starts_with(&canonical_storage) {
            return Err(anyhow::anyhow!(
                "MCAP file is not within the storage directory"
            ));
        }

        // Verify we have an authenticated client
        let client = self
            .get_client()
            .await
            .ok_or_else(|| anyhow::anyhow!("Not authenticated with EdgeFirst Studio"))?;

        // Create new task
        let task = UploadTask {
            id: upload_id,
            mcap_path: mcap_path.clone(),
            mode: mode.clone(),
            state: UploadState::Queued,
            progress: 0.0,
            message: "Queued".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            snapshot_id: None,
            dataset_id: None,
            error: None,
            task_handle: None,
        };

        // Write initial status file
        Self::write_status_file(&task).await?;

        // Store task
        {
            let mut tasks = self.tasks.write().await;
            tasks.insert(upload_id, task);
        }

        // Spawn background worker task
        let manager = Arc::clone(self);
        let task_handle = tokio::spawn(async move {
            if let Err(e) = upload_worker(manager, upload_id, client, mcap_path, mode).await {
                error!("Upload worker failed for task {}: {}", upload_id.0, e);
            }
        });

        // Store the task handle for potential cancellation
        {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.get_mut(&upload_id) {
                task.task_handle = Some(task_handle);
            }
        }

        Ok(upload_id)
    }

    /// List all upload tasks
    pub async fn list_uploads(&self) -> Vec<UploadTaskInfo> {
        let tasks = self.tasks.read().await;
        tasks.values().map(|t| t.into()).collect()
    }

    /// Get a specific upload task
    pub async fn get_upload(&self, id: UploadId) -> Option<UploadTaskInfo> {
        let tasks = self.tasks.read().await;
        tasks.get(&id).map(|t| t.into())
    }

    /// Cancel an upload task
    pub async fn cancel_upload(&self, id: UploadId) -> anyhow::Result<()> {
        info!("Cancelling upload task {}", id.0);

        let task = {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.get_mut(&id) {
                // Abort the task handle if it's running
                if let Some(handle) = task.task_handle.take() {
                    handle.abort();
                    info!("Aborted upload task {}", id.0);
                }
                task.state = UploadState::Failed;
                task.error = Some("Cancelled by user".to_string());
                task.message = "Cancelled by user".to_string();
                task.updated_at = Utc::now();
                // Convert to UploadStatus for file writing
                UploadStatus::from(&*task)
            } else {
                return Err(anyhow::anyhow!("Upload task {} not found", id.0));
            }
        };

        // Write updated status file
        Self::write_status_file_from_status(&task).await?;

        Ok(())
    }
}

/// Background upload worker function
/// Handles the actual upload to EdgeFirst Studio
async fn upload_worker(
    manager: Arc<UploadManager>,
    upload_id: UploadId,
    client: Client,
    mcap_path: PathBuf,
    mode: UploadMode,
) -> anyhow::Result<()> {
    info!(
        "Upload worker started for task {} - {:?}",
        upload_id.0, mcap_path
    );

    // Update state to Uploading
    manager
        .update_task_state(
            upload_id,
            UploadState::Uploading,
            0.0,
            "Uploading to EdgeFirst Studio...",
            None,
            None,
            None,
        )
        .await?;

    // Create progress channel
    let (progress_tx, mut progress_rx) = mpsc::channel::<Progress>(100);

    // Spawn a task to monitor progress
    let progress_manager = Arc::clone(&manager);
    let progress_upload_id = upload_id;
    let progress_handle = tokio::spawn(async move {
        while let Some(progress) = progress_rx.recv().await {
            let percentage = if progress.total > 0 {
                (progress.current as f32 / progress.total as f32) * 100.0
            } else {
                0.0
            };
            let message = format!(
                "Uploading... {:.1}% ({}/{})",
                percentage, progress.current, progress.total
            );
            if let Err(e) = progress_manager
                .update_task_state(
                    progress_upload_id,
                    UploadState::Uploading,
                    percentage,
                    &message,
                    None,
                    None,
                    None,
                )
                .await
            {
                error!("Failed to update progress: {}", e);
            }
        }
    });

    // Perform the upload
    let mcap_path_str = mcap_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid MCAP path"))?;

    let snapshot = match client
        .create_snapshot(mcap_path_str, Some(progress_tx))
        .await
    {
        Ok(snapshot) => {
            info!(
                "Upload completed for task {} - Snapshot ID: {}",
                upload_id.0,
                snapshot.id().value()
            );
            snapshot
        }
        Err(e) => {
            error!("Upload failed for task {}: {}", upload_id.0, e);
            manager
                .update_task_state(
                    upload_id,
                    UploadState::Failed,
                    0.0,
                    "Upload failed",
                    None,
                    None,
                    Some(format!("Upload failed: {}", e)),
                )
                .await?;
            return Err(anyhow::anyhow!("Upload failed: {}", e));
        }
    };

    // Wait for progress task to complete
    if let Err(e) = progress_handle.await {
        error!("Progress monitoring task terminated with error: {}", e);
    }

    let snapshot_id = snapshot.id().value();

    // Handle Extended mode - restore snapshot to dataset
    match &mode {
        UploadMode::Basic => {
            // Basic mode complete
            manager
                .update_task_state(
                    upload_id,
                    UploadState::Completed,
                    100.0,
                    &format!("Upload completed - Snapshot ID: {}", snapshot_id),
                    Some(snapshot_id),
                    None,
                    None,
                )
                .await?;
            // Clean up status file after successful completion
            UploadManager::delete_status_file(&mcap_path).await;
            info!(
                "Task {} completed (Basic mode) - Snapshot ID: {}",
                upload_id.0, snapshot_id
            );
        }
        UploadMode::Extended {
            project_id,
            labels,
            dataset_name,
            dataset_description,
        } => {
            // Update state to Processing
            manager
                .update_task_state(
                    upload_id,
                    UploadState::Processing,
                    100.0,
                    "Processing - Restoring snapshot to dataset...",
                    Some(snapshot_id),
                    None,
                    None,
                )
                .await?;

            // Restore snapshot to dataset with auto-labeling
            let result = client
                .restore_snapshot(
                    ProjectID::from(*project_id),
                    SnapshotID::from(snapshot_id),
                    &[], // All topics
                    labels,
                    false, // autodepth
                    dataset_name.as_deref(),
                    dataset_description.as_deref(),
                )
                .await;

            match result {
                Ok(restore_result) => {
                    let dataset_id = restore_result.dataset_id.value();
                    manager
                        .update_task_state(
                            upload_id,
                            UploadState::Completed,
                            100.0,
                            &format!(
                                "Completed - Snapshot: {}, Dataset: {}",
                                snapshot_id, dataset_id
                            ),
                            Some(snapshot_id),
                            Some(dataset_id),
                            None,
                        )
                        .await?;
                    // Clean up status file after successful completion
                    UploadManager::delete_status_file(&mcap_path).await;
                    info!(
                        "Task {} completed (Extended mode) - Snapshot: {}, Dataset: {}",
                        upload_id.0, snapshot_id, dataset_id
                    );
                }
                Err(e) => {
                    error!("Snapshot restore failed for task {}: {}", upload_id.0, e);
                    manager
                        .update_task_state(
                            upload_id,
                            UploadState::Failed,
                            100.0,
                            "Snapshot uploaded but restore failed",
                            Some(snapshot_id),
                            None,
                            Some(format!("Restore failed: {}", e)),
                        )
                        .await?;
                    return Err(anyhow::anyhow!("Restore failed: {}", e));
                }
            }
        }
    }

    Ok(())
}

const SERVER_PEM: &[u8] = include_bytes!("../server.pem");
const STOP: &str = "STOP";

fn load_encrypted_private_key() -> PKey<Private> {
    let buffer = SERVER_PEM;

    PKey::private_key_from_pem_passphrase(buffer, b"password").expect("Failed to load private key")
}

struct MessageStream {
    clients: Arc<Mutex<Vec<Recipient<BroadcastMessage>>>>,
    on_exit: Sender<String>,
    on_err: Box<dyn Fn() + Send + Sync>,
    high_priority: bool,
    // err: Sender<String>,
}

#[derive(Deserialize)]
struct ServiceAction {
    service: String,
    action: String,
}

impl MessageStream {
    fn new(
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

    fn add_client(&self, client: Recipient<BroadcastMessage>) {
        self.clients.lock().unwrap().push(client);
    }

    fn broadcast(&self, message: BroadcastMessage) {
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

#[derive(Message, Clone)]
#[rtype(result = "()")]
struct BroadcastMessage(Vec<u8>);

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

async fn websocket_handler_errors(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    data: web::Data<ServerContext>,
) -> Result<HttpResponse, actix_web::Error> {
    ws::start(MyWebSocket::new(data.err_stream.clone(), 1), &req, stream).map_err(|e| {
        error!("WebSocket connection failed: {:?}", e);
        e
    })
}

async fn websocket_handler_high_priority(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    data: web::Data<ServerContext>,
) -> Result<HttpResponse, actix_web::Error> {
    websocket_handler(req, stream, data, true).await
}

async fn websocket_handler_low_priority(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    data: web::Data<ServerContext>,
) -> Result<HttpResponse, actix_web::Error> {
    websocket_handler(req, stream, data, false).await
}

async fn websocket_handler(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    data: web::Data<ServerContext>,
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

    let args = data.args.clone();
    let topic = cleaned_path.clone();
    let (tx, rx) = channel();
    let video_stream = Arc::new(MessageStream::new(
        tx,
        Box::new(move || {
            let new_val = data.err_count.fetch_add(1, Ordering::SeqCst) + 1;
            let msg = format!(
                "{{\"dropped frames\": {new_val}, \"dropped_topic\": \"{}\"}}",
                topic.clone()
            );
            let msg = cdr::serialize::<_, _, CdrLe>(&msg, Infinite).unwrap();
            data.err_stream.broadcast(BroadcastMessage(msg));
        }),
        is_high_priority,
    ));
    let video_stream_clone = video_stream.clone();

    let capacity = if is_high_priority { 16 } else { 1 };
    let ws_result = ws::start(MyWebSocket::new(video_stream, capacity), &req, stream);

    if ws_result.is_ok() {
        thread::Builder::new()
            .name("zenoh".to_string())
            .spawn(move || zenoh_listener(video_stream_clone, args, rx, cleaned_path))?;
    } else {
        drop(rx);
    }

    ws_result.map_err(|e| {
        error!("WebSocket connection failed: {:?}", e);
        e
    })
}

struct MyWebSocket {
    video_stream: Arc<MessageStream>,
    capacity: usize,
}

impl MyWebSocket {
    fn new(video_stream: Arc<MessageStream>, capacity: usize) -> Self {
        MyWebSocket {
            video_stream,
            capacity,
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
        let _ = self.video_stream.on_exit.send(STOP.to_string());
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WebSocketMessage {
    action: String,
    topic: String,
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

impl Handler<BroadcastMessage> for MyWebSocket {
    type Result = ();

    fn handle(&mut self, msg: BroadcastMessage, ctx: &mut Self::Context) {
        ctx.binary(msg.0);
    }
}

#[tokio::main]
async fn zenoh_listener(
    video_stream: Arc<MessageStream>,
    args: Args,
    rx: Receiver<String>,
    topic: String,
) {
    let session = zenoh::open(args.clone()).await.unwrap();
    let subscriber = session
        .declare_subscriber(topic.clone())
        .await
        .expect("Failed to declare Zenoh subscriber");
    loop {
        if let Ok(msg) = rx.recv_timeout(Duration::from_millis(0)) {
            if msg == STOP {
                drop(video_stream);
                subscriber
                    .undeclare()
                    .await
                    .expect("Failed to undeclare subscriber");
                session
                    .close()
                    .await
                    .expect("Failed to close Zenoh session");
                return;
            }
        }
        debug!("topic: {:?}", topic);
        // Take all messages from the subscriber. If there are no messages, then wait
        // for the next message. If there were messages, only process the most recent
        // message
        let msgs = subscriber.drain();
        match msgs.last() {
            None => {
                if let Ok(sample) = subscriber.recv_async().await {
                    let data = sample.payload().to_bytes().to_vec();
                    video_stream.broadcast(BroadcastMessage(data));
                }
            }
            Some(sample) => {
                let data = sample.payload().to_bytes().to_vec();
                video_stream.broadcast(BroadcastMessage(data));
            }
        }
    }
}

async fn custom_file_handler(
    req: HttpRequest,
    data: web::Data<ServerContext>,
) -> actix_web::Result<NamedFile> {
    let path: String = req.match_info().query("file").parse().unwrap();
    let base_path = Path::new(&data.args.docroot);

    let file_path = base_path.join(&path);

    if file_path.exists() && file_path.is_file() {
        return Ok(NamedFile::open(file_path)?);
    }

    let html_path = base_path.join(format!("{}.html", path));
    if html_path.exists() && html_path.is_file() {
        return Ok(NamedFile::open(html_path)?);
    }

    Err(actix_web::error::ErrorNotFound(format!(
        "File {:?} not found",
        path
    )))
}

async fn user_mode_check_recorder_status() -> String {
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

async fn check_recorder_status() -> impl Responder {
    let service_status = Command::new("systemctl")
        .arg("is-active")
        .arg("recorder")
        .output();

    match service_status {
        Ok(output) => {
            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if status == "active" {
                HttpResponse::Ok().body("Recorder is running")
            } else {
                HttpResponse::Ok().body("Recorder is not running")
            }
        }
        Err(e) => HttpResponse::InternalServerError()
            .body(format!("Error checking service status: {:?}", e)),
    }
}

async fn check_service_status(service_name: &str) -> Result<String, String> {
    let service_status = Command::new("systemctl")
        .arg("is-active")
        .arg(service_name)
        .output()
        .map_err(|e| format!("Error checking service status: {:?}", e))?;

    let status = String::from_utf8_lossy(&service_status.stdout)
        .trim()
        .to_string();
    debug!("{:?} service is {:?}", service_name, status);
    if status == "active" && service_name != "webui" {
        Command::new("systemctl")
            .arg("restart")
            .arg(service_name)
            .output()
            .map_err(|e| format!("Error restarting service: {:?}", e))?;
        Ok(format!(
            "Service '{}' restarted successfully.",
            service_name
        ))
    } else {
        Ok(format!(
            "Service '{}' is not running. No action taken.",
            service_name
        ))
    }
}

async fn start(data: web::Data<AppState>) -> impl Responder {
    let mut command = Command::new("sudo");
    command.arg("systemctl").arg("start").arg("recorder");

    let process = match command.spawn() {
        Ok(p) => p,
        Err(e) => {
            let error_message = format!("Failed to start recorder: {:?}", e);
            info!("{}", error_message);
            return HttpResponse::Ok().json(json!({
                "status": "started",
                "message": "Recording started successfully"
            }));
        }
    };

    // Store the process ID before taking the mutex lock
    let pid = process.id();

    // Update the process in the mutex
    {
        let mut process_guard = data.process.lock().unwrap();
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
            let mut process_guard = data.process.lock().unwrap();
            if let Some(mut process) = process_guard.take() {
                let _ = process.kill();
            }
        }
        return HttpResponse::InternalServerError().json(json!({
            "status": "error",
            "message": error_message
        }));
    }

    info!("Recorder started with PID: {}", pid);
    HttpResponse::Ok().json(json!({
        "status": "started",
        "message": "Recording started successfully"
    }))
}

async fn user_mode_start(
    data: web::Data<AppState>,
    arg_data: web::Data<ServerContext>,
) -> impl Responder {
    let status_str = user_mode_check_recorder_status().await;
    debug!("Current recorder status: {}", status_str);

    if status_str == "Recorder is running" {
        return HttpResponse::BadRequest().json(json!({
            "status": "error",
            "message": "Recorder is already running"
        }));
    }
    let mut command = Command::new("edgefirst-recorder");
    command
        .env("STORAGE", &arg_data.args.storage_path) // Set STORAGE environment variable
        .arg("--all-topics"); // Add --all-topics flag to recorder command

    debug!("Starting recorder with command: {:?}", command);

    let process = match command.spawn() {
        Ok(p) => {
            debug!("Recorder process started with PID: {:?}", p.id());
            p
        }
        Err(e) => {
            let error_message = format!("Failed to start recorder: {:?}", e);
            error!("{}", error_message);
            return HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": error_message
            }));
        }
    };

    let pid = process.id();
    {
        let mut process_guard = data.process.lock().unwrap();
        *process_guard = Some(process);
    }
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let status = user_mode_check_recorder_status().await;
        if status == "Recorder is running" {
            return HttpResponse::Ok().json(json!({
                "status": "started",
                "message": "Recording started successfully"
            }));
        }
    }

    let process_status = {
        let process = {
            let mut process_guard = data.process.lock().unwrap();
            process_guard.take()
        };
        if let Some(mut process) = process {
            let status = process.try_wait();
            // Put the process back if it's still running
            if let Ok(None) = status {
                let mut process_guard = data.process.lock().unwrap();
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
                let mut process_guard = data.process.lock().unwrap();
                *process_guard = None;
            }
            HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": error_message
            }))
        }
        Ok(None) => {
            // Process is still running, create PID file
            debug!("Creating PID file manually");
            if let Err(e) = std::fs::write("/var/run/recorder.pid", pid.to_string()) {
                let error_message = format!("Failed to create PID file: {:?}", e);
                error!("{}", error_message);
                {
                    let mut process_guard = data.process.lock().unwrap();
                    if let Some(mut process) = process_guard.take() {
                        let _ = process.kill();
                    }
                }
                return HttpResponse::InternalServerError().json(json!({
                    "status": "error",
                    "message": error_message
                }));
            }
            HttpResponse::Ok().json(json!({
                "status": "started",
                "message": "Recording started successfully"
            }))
        }
        Err(e) => {
            let error_message = format!("Error checking process status: {:?}", e);
            error!("{}", error_message);
            {
                let mut process_guard = data.process.lock().unwrap();
                *process_guard = None;
            }
            HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": error_message
            }))
        }
    }
}

async fn stop(data: web::Data<AppState>) -> impl Responder {
    let mut command = Command::new("sudo");
    command.arg("systemctl").arg("stop").arg("recorder");

    match command.status() {
        Ok(status) if status.success() => {
            {
                let mut process_guard = data.process.lock().unwrap();
                *process_guard = None;
            }
            info!("Recorder service stopped");
            HttpResponse::Ok().json(json!({
                "status": "stopped",
                "message": "Recording stopped successfully"
            }))
        }
        Ok(status) => {
            let error_message = format!("Failed to stop recorder service: {:?}", status);
            error!("{}", error_message);
            HttpResponse::InternalServerError().body(error_message)
        }
        Err(e) => {
            let error_message = format!("Failed to run systemctl stop recorder: {:?}", e);
            error!("{}", error_message);
            HttpResponse::InternalServerError().body(error_message)
        }
    }
}

async fn user_mode_stop(data: web::Data<AppState>) -> impl Responder {
    let pid_file = Path::new("/var/run/edgefirst-recorder.pid");
    let process = {
        let mut process_guard = data.process.lock().unwrap();
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
    HttpResponse::Ok().json(json!({
        "status": "stopped",
        "message": "Recording stopped successfully"
    }))
}

async fn delete(params: web::Json<DeleteParams>) -> impl Responder {
    let file_path = format!("{}/{}", params.directory, params.file);
    debug!("Attempting to delete file: {}", file_path);

    if Path::new(&file_path).exists() {
        let mut command = Command::new("sudo");
        command.arg("rm").arg("-rf").arg(&file_path);

        debug!("Command: {:?}", command);

        match command.status() {
            Ok(status) if status.success() => {
                info!("File deleted successfully");
                HttpResponse::Ok().json(json!({
                    "status": "removed",
                    "message": "File deleted successfully"
                }))
            }
            Ok(status) => {
                let error_message = format!("Failed to delete file: {:?}", status);
                error!("{}", error_message);
                HttpResponse::InternalServerError().body(error_message)
            }
            Err(e) => {
                let error_message = format!("Failed to run rm -rf: {:?}", e);
                error!("{}", error_message);
                HttpResponse::InternalServerError().body(error_message)
            }
        }
    } else {
        HttpResponse::NotFound().body("File not found")
    }
}

#[derive(Serialize, Debug)]
struct UploadCredentials {
    url: String,
    jwt: String,
    topic: String,
}

fn read_uploader_credentials() -> io::Result<UploadCredentials> {
    let file_path = "/etc/default/uploader";
    let file = File::open(file_path)?;
    let reader = io::BufReader::new(file);

    let mut url = String::new();
    let mut jwt = String::new();
    let mut topic = String::new();

    for line in reader.lines() {
        let line = line?;
        if line.starts_with("URL") {
            let parts: Vec<&str> = line.split('=').collect();
            if parts.len() == 2 {
                url = parts[1].trim().trim_matches('"').to_string();
            }
        } else if line.starts_with("JWT") {
            let parts: Vec<&str> = line.split('=').collect();
            if parts.len() == 2 {
                jwt = parts[1].trim().trim_matches('"').to_string();
            }
        } else if line.starts_with("TOPIC") {
            let parts: Vec<&str> = line.split('=').collect();
            if parts.len() == 2 {
                topic = parts[1].trim().trim_matches('"').to_string();
            }
        }
    }

    if url.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Found empty string for URL",
        ));
    }

    if jwt.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Found empty string for JWT",
        ));
    }

    if topic.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Found empty string for TOPIC",
        ));
    }
    debug!("Topic = {:?}", topic);
    Ok(UploadCredentials { url, jwt, topic })
}

async fn get_upload_credentials() -> impl Responder {
    match read_uploader_credentials() {
        Ok(credentials) => HttpResponse::Ok().json(credentials),
        Err(err) => {
            let error_message = err.to_string();
            error!("{:?}", error_message);
            HttpResponse::BadRequest().body(error_message)
        }
    }
}

fn read_storage_directory() -> io::Result<String> {
    let file_path = "/etc/default/recorder";
    let file = File::open(file_path)?;
    let reader = io::BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        if line.starts_with("STORAGE") {
            let parts: Vec<&str> = line.split('=').collect();
            if parts.len() == 2 {
                debug!(
                    "MCAP Directory: {:?}",
                    parts[1].trim().trim_matches('"').to_string()
                );
                return Ok(parts[1].trim().trim_matches('"').to_string());
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "STORAGE directory not found in configuration",
    ))
}

// Below function will be needed when we start working on the DVE Uploader
// integration fn read_topics_from_config() -> io::Result<Vec<String>> {
//     let file_path = "/etc/default/recorder";
//     let file = File::open(file_path)?;
//     let reader = io::BufReader::new(file);

//     let topics;

//     for line in reader.lines() {
//         let line = line?;
//         if line.starts_with("TOPICS") {
//             let parts: Vec<&str> = line.split('=').collect();
//             if parts.len() == 2 {
//                 topics = parts[1].split_whitespace().map(|s|
// s.to_string()).collect();                 return Ok(topics);
//             }
//         }
//     }

//     Err(io::Error::new(
//         io::ErrorKind::NotFound,
//         "TOPICS not found in the config file",
//     ))
// }

struct WebSocketSession {
    context: Option<web::Data<ServerContext>>,
}

impl Actor for WebSocketSession {
    type Context = ws::WebsocketContext<Self>;
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WebSocketSession {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let arg_data = match &self.context {
            Some(data) => data,
            None => {
                error!("ServerContext not initialized");
                return;
            }
        };

        match msg {
            Ok(ws::Message::Text(text)) => {
                debug!("Received message: {}", text);
                let mut directory: String = arg_data.args.storage_path.to_string();
                // let mut topics: Option<Vec<String>> = Some(vec!["".to_string()]);
                if arg_data.args.system {
                    directory = match read_storage_directory() {
                        Ok(dir) => dir,
                        Err(e) => {
                            error!("Error reading directory from config: {}", e);
                            let response = serde_json::to_string(
                                &json!({"error": "Error reading directory from config"}),
                            )
                            .unwrap();
                            ctx.text(response);
                            return;
                        }
                    };

                    // let topics = match read_topics_from_config() {
                    //     Ok(topics) => topics,
                    //     Err(e) => {
                    //         error!("Error reading topics from config: {}",
                    // e);         let response =
                    // serde_json::to_string(
                    // &json!({"error": "Error reading topics from config"}),
                    //         )
                    //         .unwrap();
                    //         ctx.text(response);
                    //         return;
                    //     }
                    // };
                }

                debug!("Listing files in directory: {}", directory.clone());

                match std::fs::read_dir(&directory) {
                    Ok(entries) => {
                        let files: Vec<FileInfo> = entries
                            .filter_map(Result::ok)
                            .filter_map(|entry| {
                                if let Some(extension) = entry.path().extension() {
                                    if extension == "mcap" {
                                        let metadata = entry.metadata().ok()?;
                                        let size = metadata.len();
                                        let created = metadata
                                            .created()
                                            .ok()?
                                            .duration_since(UNIX_EPOCH)
                                            .ok()?
                                            .as_secs();

                                        let (topics_info, average_video_length) =
                                            match read_mcap_info(Utf8Path::from_path(
                                                &entry.path(),
                                            )?) {
                                                Ok(info) => info,
                                                Err(_) => (HashMap::new(), 0.0),
                                            };

                                        Some(FileInfo {
                                            name: entry.file_name().to_string_lossy().to_string(),
                                            size: size / (1024 * 1024),
                                            created: DateTime::from_timestamp(created as i64, 0)
                                                .unwrap()
                                                .with_timezone(&chrono::Local)
                                                .format("%Y-%m-%d %H:%M:%S")
                                                .to_string(),
                                            topics: topics_info,
                                            average_video_length,
                                        })
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            .collect();
                        let response = if files.is_empty() {
                            DirectoryResponse {
                                dir_name: directory.clone(),
                                files: None,
                                message: Some("No MCAP files found".to_string()),
                                topics: None,
                            }
                        } else {
                            DirectoryResponse {
                                dir_name: directory.clone(),
                                files: Some(files),
                                message: None,
                                topics: None,
                            }
                        };

                        let response = serde_json::to_string(&response).unwrap();
                        ctx.text(response);
                    }
                    Err(e) => {
                        debug!("Directory not found: {}", e);
                        let response = serde_json::to_string(&DirectoryResponse {
                            dir_name: directory.clone(),
                            files: None,
                            message: Some("No MCAP files found".to_string()),
                            topics: None,
                        })
                        .unwrap();
                        ctx.text(response);
                    }
                }
            }
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            Ok(ws::Message::Close(reason)) => ctx.close(reason),
            _ => (),
        }
    }
}

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

struct ServerContext {
    args: Args,
    err_stream: Arc<MessageStream>,
    err_count: AtomicI64,
    upload_manager: Arc<UploadManager>,
    upload_progress_stream: Arc<MessageStream>,
}

// ============================================================================
// Authentication API Handlers
// ============================================================================

#[derive(Deserialize)]
struct AuthRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct AuthResponse {
    status: String,
    message: String,
}

#[derive(Serialize)]
struct AuthStatusResponse {
    authenticated: bool,
    username: Option<String>,
}

/// POST /api/auth/login - Authenticate with EdgeFirst Studio
async fn auth_login(
    body: web::Json<AuthRequest>,
    data: web::Data<ServerContext>,
) -> impl Responder {
    info!("Login request received for user: {}", body.username);

    match data
        .upload_manager
        .authenticate(&body.username, &body.password)
        .await
    {
        Ok(()) => {
            info!("User {} authenticated successfully", body.username);
            HttpResponse::Ok().json(AuthResponse {
                status: "ok".to_string(),
                message: "Authentication successful".to_string(),
            })
        }
        Err(e) => {
            // Log full error internally but return generic message to client
            error!("Authentication failed for user {}: {}", body.username, e);
            HttpResponse::Unauthorized().json(AuthResponse {
                status: "error".to_string(),
                message: "Authentication failed. Please check your credentials.".to_string(),
            })
        }
    }
}

/// GET /api/auth/status - Check authentication status
async fn auth_status(data: web::Data<ServerContext>) -> impl Responder {
    let is_authenticated = data.upload_manager.is_authenticated().await;
    let username = data.upload_manager.get_username().await;

    HttpResponse::Ok().json(AuthStatusResponse {
        authenticated: is_authenticated,
        username,
    })
}

/// POST /api/auth/logout - Logout from EdgeFirst Studio
async fn auth_logout(data: web::Data<ServerContext>) -> impl Responder {
    match data.upload_manager.logout().await {
        Ok(()) => {
            info!("User logged out successfully");
            HttpResponse::Ok().json(AuthResponse {
                status: "ok".to_string(),
                message: "Logged out successfully".to_string(),
            })
        }
        Err(e) => {
            error!("Logout failed: {}", e);
            HttpResponse::InternalServerError().json(AuthResponse {
                status: "error".to_string(),
                message: format!("Logout failed: {}", e),
            })
        }
    }
}

// ============================================================================
// Upload API Handlers
// ============================================================================

#[derive(Deserialize)]
struct StartUploadRequest {
    mcap_path: String,
    mode: UploadMode,
}

#[derive(Serialize)]
struct StartUploadResponse {
    upload_id: UploadId,
    status: String,
}

#[derive(Serialize)]
struct UploadErrorResponse {
    error: String,
}

/// POST /api/uploads - Start a new upload
async fn start_upload_handler(
    body: web::Json<StartUploadRequest>,
    data: web::Data<ServerContext>,
) -> impl Responder {
    info!("Start upload request for: {}", body.mcap_path);

    let mcap_path = PathBuf::from(&body.mcap_path);

    match data
        .upload_manager
        .start_upload(mcap_path, body.mode.clone())
        .await
    {
        Ok(upload_id) => {
            info!("Upload started with ID: {}", upload_id.0);
            HttpResponse::Accepted().json(StartUploadResponse {
                upload_id,
                status: "queued".to_string(),
            })
        }
        Err(e) => {
            error!("Failed to start upload: {}", e);
            HttpResponse::BadRequest().json(UploadErrorResponse {
                error: format!("{}", e),
            })
        }
    }
}

/// GET /api/uploads - List all uploads
async fn list_uploads_handler(data: web::Data<ServerContext>) -> impl Responder {
    let uploads = data.upload_manager.list_uploads().await;
    HttpResponse::Ok().json(uploads)
}

/// GET /api/uploads/{id} - Get a specific upload
async fn get_upload_handler(
    path: web::Path<String>,
    data: web::Data<ServerContext>,
) -> impl Responder {
    let id_str = path.into_inner();

    // Parse the UUID
    match Uuid::parse_str(&id_str) {
        Ok(uuid) => {
            let upload_id = UploadId(uuid);
            match data.upload_manager.get_upload(upload_id).await {
                Some(upload) => HttpResponse::Ok().json(upload),
                None => HttpResponse::NotFound().json(UploadErrorResponse {
                    error: "Upload not found".to_string(),
                }),
            }
        }
        Err(_) => HttpResponse::BadRequest().json(UploadErrorResponse {
            error: "Invalid upload ID format".to_string(),
        }),
    }
}

/// DELETE /api/uploads/{id} - Cancel an upload
async fn cancel_upload_handler(
    path: web::Path<String>,
    data: web::Data<ServerContext>,
) -> impl Responder {
    let id_str = path.into_inner();

    // Parse the UUID
    match Uuid::parse_str(&id_str) {
        Ok(uuid) => {
            let upload_id = UploadId(uuid);
            match data.upload_manager.cancel_upload(upload_id).await {
                Ok(()) => HttpResponse::Ok().json(json!({
                    "status": "cancelled",
                    "message": "Upload cancelled successfully"
                })),
                Err(e) => HttpResponse::NotFound().json(UploadErrorResponse {
                    error: format!("{}", e),
                }),
            }
        }
        Err(_) => HttpResponse::BadRequest().json(UploadErrorResponse {
            error: "Invalid upload ID format".to_string(),
        }),
    }
}

/// WebSocket handler for upload progress updates
/// GET /ws/uploads
async fn upload_websocket_handler(
    req: HttpRequest,
    stream: web::Payload,
    data: web::Data<ServerContext>,
) -> Result<HttpResponse, actix_web::Error> {
    ws::start(
        MyWebSocket::new(data.upload_progress_stream.clone(), 16),
        &req,
        stream,
    )
    .map_err(|e| {
        error!("Upload WebSocket connection failed: {:?}", e);
        e
    })
}

// =====================================================
// Studio API Endpoints
// =====================================================

/// Response for project list
#[derive(Serialize)]
struct ProjectInfo {
    id: String,
    name: String,
}

/// Response for label list
#[derive(Serialize)]
struct LabelInfo {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    default: bool,
}

/// GET /api/studio/projects - List projects from EdgeFirst Studio
async fn list_studio_projects(data: web::Data<ServerContext>) -> impl Responder {
    let client_lock = data.upload_manager.client.read().await;

    match &*client_lock {
        Some(client) => match client.projects(None).await {
            Ok(projects) => {
                let project_infos: Vec<ProjectInfo> = projects
                    .into_iter()
                    .map(|p| ProjectInfo {
                        id: p.id().to_string(),
                        name: p.name().to_string(),
                    })
                    .collect();
                HttpResponse::Ok().json(project_infos)
            }
            Err(e) => {
                error!("Failed to fetch projects: {}", e);
                HttpResponse::InternalServerError().json(UploadErrorResponse {
                    error: format!("Failed to fetch projects: {}", e),
                })
            }
        },
        None => HttpResponse::Unauthorized().json(UploadErrorResponse {
            error: "Not authenticated with EdgeFirst Studio".to_string(),
        }),
    }
}

/// GET /api/studio/projects/{id}/labels - Get labels for auto-labeling
/// Returns the 80 COCO dataset labels used by YOLOX for AGTG autobox prompts.
/// These labels drive the SAM2 model for automatic annotation.
/// In the future this will be configurable; for now we return the standard COCO classes.
async fn list_project_labels(
    _path: web::Path<String>,
    data: web::Data<ServerContext>,
) -> impl Responder {
    let client_lock = data.upload_manager.client.read().await;

    match &*client_lock {
        Some(_client) => {
            // COCO 80 classes used by YOLOX for AGTG autobox prompts
            // "person" is selected by default as the most common use case
            let coco_labels = vec![
                // Person (default selected)
                LabelInfo {
                    id: "person".to_string(),
                    name: "Person".to_string(),
                    default: true,
                },
                // Vehicles
                LabelInfo {
                    id: "bicycle".to_string(),
                    name: "Bicycle".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "car".to_string(),
                    name: "Car".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "motorcycle".to_string(),
                    name: "Motorcycle".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "airplane".to_string(),
                    name: "Airplane".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "bus".to_string(),
                    name: "Bus".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "train".to_string(),
                    name: "Train".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "truck".to_string(),
                    name: "Truck".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "boat".to_string(),
                    name: "Boat".to_string(),
                    default: false,
                },
                // Outdoor objects
                LabelInfo {
                    id: "traffic_light".to_string(),
                    name: "Traffic Light".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "fire_hydrant".to_string(),
                    name: "Fire Hydrant".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "stop_sign".to_string(),
                    name: "Stop Sign".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "parking_meter".to_string(),
                    name: "Parking Meter".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "bench".to_string(),
                    name: "Bench".to_string(),
                    default: false,
                },
                // Animals
                LabelInfo {
                    id: "bird".to_string(),
                    name: "Bird".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "cat".to_string(),
                    name: "Cat".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "dog".to_string(),
                    name: "Dog".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "horse".to_string(),
                    name: "Horse".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "sheep".to_string(),
                    name: "Sheep".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "cow".to_string(),
                    name: "Cow".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "elephant".to_string(),
                    name: "Elephant".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "bear".to_string(),
                    name: "Bear".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "zebra".to_string(),
                    name: "Zebra".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "giraffe".to_string(),
                    name: "Giraffe".to_string(),
                    default: false,
                },
                // Accessories
                LabelInfo {
                    id: "backpack".to_string(),
                    name: "Backpack".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "umbrella".to_string(),
                    name: "Umbrella".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "handbag".to_string(),
                    name: "Handbag".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "tie".to_string(),
                    name: "Tie".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "suitcase".to_string(),
                    name: "Suitcase".to_string(),
                    default: false,
                },
                // Sports
                LabelInfo {
                    id: "frisbee".to_string(),
                    name: "Frisbee".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "skis".to_string(),
                    name: "Skis".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "snowboard".to_string(),
                    name: "Snowboard".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "sports_ball".to_string(),
                    name: "Sports Ball".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "kite".to_string(),
                    name: "Kite".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "baseball_bat".to_string(),
                    name: "Baseball Bat".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "baseball_glove".to_string(),
                    name: "Baseball Glove".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "skateboard".to_string(),
                    name: "Skateboard".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "surfboard".to_string(),
                    name: "Surfboard".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "tennis_racket".to_string(),
                    name: "Tennis Racket".to_string(),
                    default: false,
                },
                // Kitchen
                LabelInfo {
                    id: "bottle".to_string(),
                    name: "Bottle".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "wine_glass".to_string(),
                    name: "Wine Glass".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "cup".to_string(),
                    name: "Cup".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "fork".to_string(),
                    name: "Fork".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "knife".to_string(),
                    name: "Knife".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "spoon".to_string(),
                    name: "Spoon".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "bowl".to_string(),
                    name: "Bowl".to_string(),
                    default: false,
                },
                // Food
                LabelInfo {
                    id: "banana".to_string(),
                    name: "Banana".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "apple".to_string(),
                    name: "Apple".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "sandwich".to_string(),
                    name: "Sandwich".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "orange".to_string(),
                    name: "Orange".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "broccoli".to_string(),
                    name: "Broccoli".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "carrot".to_string(),
                    name: "Carrot".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "hot_dog".to_string(),
                    name: "Hot Dog".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "pizza".to_string(),
                    name: "Pizza".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "donut".to_string(),
                    name: "Donut".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "cake".to_string(),
                    name: "Cake".to_string(),
                    default: false,
                },
                // Furniture
                LabelInfo {
                    id: "chair".to_string(),
                    name: "Chair".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "couch".to_string(),
                    name: "Couch".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "potted_plant".to_string(),
                    name: "Potted Plant".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "bed".to_string(),
                    name: "Bed".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "dining_table".to_string(),
                    name: "Dining Table".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "toilet".to_string(),
                    name: "Toilet".to_string(),
                    default: false,
                },
                // Electronics
                LabelInfo {
                    id: "tv".to_string(),
                    name: "TV".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "laptop".to_string(),
                    name: "Laptop".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "mouse".to_string(),
                    name: "Mouse".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "remote".to_string(),
                    name: "Remote".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "keyboard".to_string(),
                    name: "Keyboard".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "cell_phone".to_string(),
                    name: "Cell Phone".to_string(),
                    default: false,
                },
                // Appliances
                LabelInfo {
                    id: "microwave".to_string(),
                    name: "Microwave".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "oven".to_string(),
                    name: "Oven".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "toaster".to_string(),
                    name: "Toaster".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "sink".to_string(),
                    name: "Sink".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "refrigerator".to_string(),
                    name: "Refrigerator".to_string(),
                    default: false,
                },
                // Indoor objects
                LabelInfo {
                    id: "book".to_string(),
                    name: "Book".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "clock".to_string(),
                    name: "Clock".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "vase".to_string(),
                    name: "Vase".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "scissors".to_string(),
                    name: "Scissors".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "teddy_bear".to_string(),
                    name: "Teddy Bear".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "hair_drier".to_string(),
                    name: "Hair Drier".to_string(),
                    default: false,
                },
                LabelInfo {
                    id: "toothbrush".to_string(),
                    name: "Toothbrush".to_string(),
                    default: false,
                },
            ];
            HttpResponse::Ok().json(coco_labels)
        }
        None => HttpResponse::Unauthorized().json(UploadErrorResponse {
            error: "Not authenticated with EdgeFirst Studio".to_string(),
        }),
    }
}

async fn serve_settings_page(data: web::Data<ServerContext>) -> Result<NamedFile> {
    let base_path = PathBuf::from(&data.args.docroot);

    let file_path = if base_path.is_dir() {
        base_path.join("config/settings.html")
    } else {
        base_path
    };
    debug!("{:?}", file_path);
    Ok(NamedFile::open(file_path)?)
}

async fn serve_config_page(
    data: web::Data<ServerContext>,
    path: web::Path<String>,
) -> Result<NamedFile> {
    let service = path.into_inner();
    let base_path = PathBuf::from(&data.args.docroot);

    let file_path = if base_path.is_dir() {
        base_path.join(format!("config/{}.html", service))
    } else {
        base_path
    };
    debug!("{:?}", file_path);
    Ok(NamedFile::open(file_path)?)
}

async fn mcap_downloader(req: HttpRequest) -> impl Responder {
    let path: String = req.match_info().query("file").parse().unwrap();
    let file_path = Path::new(&path);

    if !file_path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("MCAP"))
    {
        return Err(actix_web::error::ErrorForbidden(
            "Invalid file extension. Only .mcap files are allowed.",
        ));
    }

    if !file_path.exists() || !file_path.is_file() {
        return Err(actix_web::error::ErrorNotFound(format!(
            "File {:?} not found",
            path
        )));
    }

    let mut file = tokio::fs::File::open(file_path).await?;
    let file_size = file.metadata().await?.len() as usize;

    let mcap_stream = stream! {
        let mut buffer = [0; 64 * 1024];
        loop {
            let bytes_read = file.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }

            yield Result::<Bytes, std::io::Error>::Ok(Bytes::copy_from_slice(&buffer[..bytes_read]));
        }
    };

    let mime_type = Mime::from_str("application/octet-stream").unwrap();

    Ok(HttpResponse::Ok()
        .content_type(mime_type)
        .insert_header(ContentLength(file_size))
        .streaming(mcap_stream))
}

#[derive(Serialize)]
struct FormattedSize {
    value: f64,
    unit: String,
}

impl FormattedSize {
    fn from_bytes(bytes: u64) -> Self {
        if bytes >= 1024 * 1024 * 1024 {
            // Convert to GB if >= 1GB
            FormattedSize {
                value: (bytes as f64) / (1024.0 * 1024.0 * 1024.0),
                unit: "GB".to_string(),
            }
        } else {
            // Convert to MB if < 1GB
            FormattedSize {
                value: (bytes as f64) / (1024.0 * 1024.0),
                unit: "MB".to_string(),
            }
        }
    }
}

#[derive(Serialize)]
struct StorageDetails {
    path: String,
    exists: bool,
    available_space: Option<FormattedSize>,
    total_space: Option<FormattedSize>,
}

async fn check_storage_availability(data: web::Data<ServerContext>) -> impl Responder {
    let storage_dir = if data.args.system {
        match read_storage_directory() {
            Ok(dir) => dir,
            Err(e) => {
                error!("Error reading storage directory from config: {}", e);
                return HttpResponse::InternalServerError().json(json!({
                    "error": "Error reading storage directory from config"
                }));
            }
        }
    } else {
        data.args.storage_path.clone()
    };

    let dir_path = Path::new(&storage_dir);

    let exists = dir_path.exists();
    let available_space = std::fs::metadata(dir_path)
        .and_then(|_| fs2::available_space(dir_path))
        .ok()
        .map(FormattedSize::from_bytes);
    let total_space = std::fs::metadata(dir_path)
        .and_then(|_| fs2::total_space(dir_path))
        .ok()
        .map(FormattedSize::from_bytes);

    let details = StorageDetails {
        path: storage_dir,
        exists,
        available_space,
        total_space,
    };

    HttpResponse::Ok().json(details)
}

#[derive(Deserialize, Debug, Clone)]
struct PlaybackParams {
    directory: String,
    file: String,
}

#[derive(Serialize)]
struct PlaybackResponse {
    status: String,
    message: String,
    current_file: Option<String>,
}

async fn start_replay(params: web::Json<PlaybackParams>) -> impl Responder {
    let file_path = format!("{}/{}", params.directory, params.file);
    debug!("Attempting to play MCAP file: {}", file_path);

    if !Path::new(&file_path).exists() {
        return HttpResponse::NotFound().json(PlaybackResponse {
            status: "error".to_string(),
            message: "File not found".to_string(),
            current_file: None,
        });
    }

    let status = Command::new("systemctl")
        .arg("is-active")
        .arg("replay")
        .output();

    match status {
        Ok(output) => {
            let status_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if status_str == "active" {
                return HttpResponse::BadRequest().json(PlaybackResponse {
                    status: "error".to_string(),
                    message: "Replay service is already running".to_string(),
                    current_file: None,
                });
            }
        }
        Err(e) => {
            error!("Error checking replay service status: {}", e);
            return HttpResponse::InternalServerError().json(PlaybackResponse {
                status: "error".to_string(),
                message: format!("Error checking service status: {}", e),
                current_file: None,
            });
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
            HttpResponse::Ok().json(PlaybackResponse {
                status: "success".to_string(),
                message: "Replay service started successfully".to_string(),
                current_file: Some(params.file.clone()),
            })
        }
        Ok(status) => {
            error!("Failed to start replay service: {:?}", status);
            HttpResponse::InternalServerError().json(PlaybackResponse {
                status: "error".to_string(),
                message: "Failed to start replay service".to_string(),
                current_file: None,
            })
        }
        Err(e) => {
            error!("Error starting replay service: {}", e);
            HttpResponse::InternalServerError().json(PlaybackResponse {
                status: "error".to_string(),
                message: format!("Error starting replay service: {}", e),
                current_file: None,
            })
        }
    }
}

async fn stop_replay() -> impl Responder {
    // Stop fusion service
    let result = Command::new("sudo")
        .arg("systemctl")
        .arg("stop")
        .arg("replay")
        .status();

    match result {
        Ok(status) if status.success() => {
            info!("Replay service stopped successfully");
            HttpResponse::Ok().json(PlaybackResponse {
                status: "success".to_string(),
                message: "Replay service stopped successfully".to_string(),
                current_file: None,
            })
        }
        Ok(status) => {
            error!("Failed to stop replay service: {:?}", status);
            HttpResponse::InternalServerError().json(PlaybackResponse {
                status: "error".to_string(),
                message: "Failed to stop replay service".to_string(),
                current_file: None,
            })
        }
        Err(e) => {
            error!("Error stopping replay service: {}", e);
            HttpResponse::InternalServerError().json(PlaybackResponse {
                status: "error".to_string(),
                message: format!("Error stopping replay service: {}", e),
                current_file: None,
            })
        }
    }
}

#[derive(Deserialize)]
struct IsolateParams {
    target: String,
}

async fn isolate_system(params: web::Json<IsolateParams>) -> impl Responder {
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
            HttpResponse::Ok().json(json!({
                "status": "success",
                "message": format!("System isolated to {}", target)
            }))
        }
        Ok(status) => {
            error!("Failed to isolate system: {:?}", status);
            HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": format!("Failed to isolate system to {}", target)
            }))
        }
        Err(e) => {
            error!("Error isolating system: {}", e);
            HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": format!("Error isolating system: {}", e)
            }))
        }
    }
}

fn extract_recording_filename(status_output: &str) -> Option<String> {
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

async fn get_current_recording() -> impl Responder {
    let status = Command::new("systemctl")
        .arg("status")
        .arg("recorder")
        .output();

    match status {
        Ok(output) => {
            let status_str = String::from_utf8_lossy(&output.stdout);
            match extract_recording_filename(&status_str) {
                Some(filename) => HttpResponse::Ok().json(json!({
                    "status": "recording",
                    "filename": filename
                })),
                None => HttpResponse::Ok().json(json!({
                    "status": "not_recording",
                    "filename": null
                })),
            }
        }
        Err(e) => {
            error!("Error checking recorder status: {}", e);
            HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": format!("Error checking recorder status: {}", e)
            }))
        }
    }
}

async fn user_mode_check_replay_status() -> impl Responder {
    let pid_file = Path::new("/var/run/replay.pid");

    if !pid_file.exists() {
        return HttpResponse::Ok().json(json!({
            "status": "not_running",
            "message": "Replay is not running"
        }));
    }

    match std::fs::read_to_string(pid_file) {
        Ok(pid_str) => {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                let status = Command::new("ps").arg("-p").arg(pid.to_string()).output();

                match status {
                    Ok(output) => {
                        if output.status.success() {
                            HttpResponse::Ok().json(json!({
                                "status": "running",
                                "message": "Replay is running"
                            }))
                        } else {
                            // Process exists but not running, clean up PID file
                            let _ = std::fs::remove_file(pid_file);
                            HttpResponse::Ok().json(json!({
                                "status": "not_running",
                                "message": "Replay is not running"
                            }))
                        }
                    }
                    Err(e) => HttpResponse::InternalServerError().json(json!({
                        "status": "error",
                        "message": format!("Error checking process status: {:?}", e)
                    })),
                }
            } else {
                HttpResponse::InternalServerError().json(json!({
                    "status": "error",
                    "message": "Invalid PID in PID file"
                }))
            }
        }
        Err(e) => HttpResponse::InternalServerError().json(json!({
            "status": "error",
            "message": format!("Error reading PID file: {:?}", e)
        })),
    }
}

async fn check_replay_status() -> impl Responder {
    let service_status = Command::new("systemctl")
        .arg("is-active")
        .arg("replay")
        .output();

    match service_status {
        Ok(output) => {
            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if status == "active" {
                HttpResponse::Ok().body("Replay is running")
            } else {
                HttpResponse::Ok().body("Replay is not running")
            }
        }
        Err(e) => HttpResponse::InternalServerError()
            .body(format!("Error checking service status: {:?}", e)),
    }
}

async fn get_all_services(params: web::Json<Value>) -> impl Responder {
    let services = match params["services"].as_array() {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<String>>(),
        None => Vec::new(),
    };
    let mut service_statuses = Vec::new();

    for service in services {
        let decoded_service = percent_decode(service.as_bytes())
            .decode_utf8_lossy()
            .to_string();
        let status_output = Command::new("systemctl")
            .arg("is-active")
            .arg(&decoded_service)
            .output();

        let enabled_output = Command::new("systemctl")
            .arg("is-enabled")
            .arg(&decoded_service)
            .output();

        let status = match status_output {
            Ok(output) => {
                let status_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if status_str == "active" {
                    "running".to_string()
                } else {
                    "not running".to_string()
                }
            }
            Err(_) => "unknown".to_string(),
        };

        let enabled = match enabled_output {
            Ok(output) => {
                let enabled_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                match enabled_str.as_str() {
                    "enabled" => "enabled".to_string(),
                    "disabled" => "disabled".to_string(),
                    _ => "unknown".to_string(),
                }
            }
            Err(_) => "unknown".to_string(),
        };
        service_statuses.push(json!({
            "service": decoded_service,
            "status": status,
            "enabled": enabled,
        }));
    }
    HttpResponse::Ok().json(service_statuses)
}

async fn update_service(params: web::Json<ServiceAction>) -> impl Responder {
    let service_name = &params.service;
    let action = &params.action;

    let mut command = Command::new("sudo");
    command.arg("systemctl");

    match action.as_str() {
        "start" => command.arg("start").arg(service_name),
        "stop" => command.arg("stop").arg(service_name),
        "enable" => command.arg("enable").arg(service_name),
        "disable" => command.arg("disable").arg(service_name),
        _ => return HttpResponse::BadRequest().body("Invalid action"),
    };

    match command.status() {
        Ok(status) if status.success() => HttpResponse::Ok().body(format!(
            "Service '{}' {}d successfully.",
            service_name, action
        )),
        Ok(status) => {
            let error_message = format!(
                "Failed to {} service '{}': {:?}",
                action, service_name, status
            );
            error!("{}", error_message);
            HttpResponse::InternalServerError().body(error_message)
        }
        Err(e) => {
            let error_message = format!(
                "Failed to run systemctl {} {}: {:?}",
                action, service_name, e
            );
            error!("{}", error_message);
            HttpResponse::InternalServerError().body(error_message)
        }
    }
}

async fn get_config(path: web::Path<ConfigPath>) -> impl Responder {
    let service_name = &path.service;
    let config_file_path = format!("/etc/default/{}", service_name);

    let config_content = std::fs::read_to_string(&config_file_path).unwrap_or_default();
    let mut config_map = serde_json::Map::new();

    for line in config_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let clean_key = key.trim();
            let clean_value = value.trim().replace("\"", "");
            let parts: Vec<&str> = clean_value.split_whitespace().collect();

            if parts.len() > 1 {
                config_map.insert(
                    clean_key.to_string(),
                    serde_json::Value::Array(
                        parts
                            .iter()
                            .map(|s| serde_json::Value::String(s.to_string()))
                            .collect(),
                    ),
                );
            } else {
                config_map.insert(
                    clean_key.to_string(),
                    serde_json::Value::String(clean_value.to_string()),
                );
            }
        }
    }

    HttpResponse::Ok().json(serde_json::Value::Object(config_map))
}

async fn set_config(params: web::Json<Value>) -> impl Responder {
    let file_name = if let Some(file_name_value) = params.get("fileName") {
        if let Some(file_name) = file_name_value.as_str() {
            file_name
        } else {
            error!("fileName is not a string");
            return HttpResponse::BadRequest().body("Invalid fileName");
        }
    } else {
        error!("fileName not found in JSON");
        return HttpResponse::BadRequest().body("Missing fileName");
    };

    let service_name = file_name;

    let config_file_path = format!("/etc/default/{}", file_name);
    debug!("Configuration file path: {}", config_file_path.clone());
    debug!("{:?}", params);

    let config_content = match std::fs::read_to_string(config_file_path.clone()) {
        Ok(content) => content,
        Err(e) => {
            error!("Error reading configuration file: {:?}", e);
            return HttpResponse::InternalServerError().body("Error reading configuration file");
        }
    };

    let config_map = if let Some(map) = params.as_object() {
        map.clone()
    } else {
        serde_json::Map::new()
    };
    let mut updated_config = String::new();

    for line in config_content.lines() {
        let mut found = false;

        for (key, value) in &config_map {
            let escaped_key = regex::escape(key);
            let pattern = format!(r"(?i)^\s*{}\s*=\s*.*", escaped_key);
            let re = Regex::new(&pattern).unwrap();

            if re.is_match(line) {
                updated_config.push_str(&format!(
                    "{} = \"{}\"\n",
                    key.to_uppercase(),
                    value.as_str().unwrap_or("")
                ));
                found = true;
                break;
            }
        }
        if !found {
            updated_config.push_str(&format!("{}\n", line)); // Preserve original line
        }
    }

    match std::fs::write(config_file_path.clone(), updated_config) {
        Ok(_) => match check_service_status(service_name).await {
            Ok(_) => HttpResponse::Ok()
                .body("Configuration saved successfully and service status checked."),
            Err(e) => {
                error!("{}", e);
                HttpResponse::InternalServerError().body("Error handling service status")
            }
        },
        Err(e) => {
            error!("Error saving configuration: {:?}", e);
            HttpResponse::InternalServerError().body("Error saving configuration")
        }
    }
}

async fn user_mode_get_config(data: web::Data<ServerContext>) -> HttpResponse {
    HttpResponse::Ok().json(WebUISettings::from(data.args.clone()))
}

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

    let thread_state = web::Data::new(Arc::new(ThreadState {
        is_running: Mutex::new(false),
    }));

    // Initialize UploadManager
    let (upload_tx, _) = channel();
    let upload_broadcaster = Arc::new(MessageStream::new(upload_tx, Box::new(|| {}), false));
    let upload_manager = Arc::new(UploadManager::new(
        PathBuf::from(&args.storage_path),
        upload_broadcaster,
    ));

    // Initialize upload manager (scan for incomplete uploads from previous session)
    if let Err(e) = Handle::current().block_on(upload_manager.initialize()) {
        error!("Failed to initialize upload manager: {}", e);
    }

    // binding to [::] will also bind to 0.0.0.0. We try to bind both ivp6 and ipv4
    // with [::]. If that fails we will try just ivp4. If we do 0.0.0.0 first, the
    // [::] bind won't happen
    let addrs = ["[::]:443".parse().unwrap(), "0.0.0.0:443".parse().unwrap()];
    let addrs_http = ["[::]:80".parse().unwrap(), "0.0.0.0:80".parse().unwrap()];

    HttpServer::new(move || {
        let (tx, _) = channel();
        // Create upload progress stream (does not need on_exit channel)
        let (upload_tx, _upload_rx) = channel();
        let upload_progress_stream =
            Arc::new(MessageStream::new(upload_tx, Box::new(|| {}), false));

        let server_ctx = ServerContext {
            args: args.clone(),
            err_stream: Arc::new(MessageStream::new(tx, Box::new(|| {}), false)),
            err_count: AtomicI64::new(0),
            upload_manager: upload_manager.clone(),
            upload_progress_stream: upload_progress_stream.clone(),
        };
        let _handle = handle.clone();
        let _thread_state_clone = thread_state.clone();

        if args.system {
            App::new()
                .wrap(RedirectHttps::default())
                // .wrap(middleware::Compress::default())
                .app_data(state.clone())
                .app_data(web::Data::new(server_ctx))
                .service(
                    web::scope("")
                        .route("/", web::get().to(index))
                        .route("/settings", web::get().to(serve_settings_page))
                        .route("/check-storage", web::get().to(check_storage_availability))
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
                        .route("/api/auth/login", web::post().to(auth_login))
                        .route("/api/auth/status", web::get().to(auth_status))
                        .route("/api/auth/logout", web::post().to(auth_logout))
                        // Upload API routes
                        .route("/api/uploads", web::post().to(start_upload_handler))
                        .route("/api/uploads", web::get().to(list_uploads_handler))
                        .route("/api/uploads/{id}", web::get().to(get_upload_handler))
                        .route("/api/uploads/{id}", web::delete().to(cancel_upload_handler))
                        // Studio API routes
                        .route("/api/studio/projects", web::get().to(list_studio_projects))
                        .route(
                            "/api/studio/projects/{id}/labels",
                            web::get().to(list_project_labels),
                        )
                        .route("/recorder-status", web::get().to(check_recorder_status))
                        .route("/current-recording", web::get().to(get_current_recording))
                        .service(
                            web::resource("/ws/dropped")
                                .route(web::get().to(websocket_handler_errors)),
                        )
                        .service(
                            web::resource("/rt/detect/mask")
                                .route(web::get().to(websocket_handler_high_priority)),
                        )
                        .service(
                            web::resource("/rt/{tail:.*}")
                                .route(web::get().to(websocket_handler_low_priority)),
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
                // .wrap(middleware::Compress::default())
                .app_data(state.clone())
                .app_data(web::Data::new(server_ctx))
                .service(
                    web::scope("")
                        .route("/", web::get().to(index))
                        .route("/settings", web::get().to(serve_settings_page))
                        .route("/check-storage", web::get().to(check_storage_availability))
                        .route("/start", web::post().to(user_mode_start))
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
                        .route("/api/auth/login", web::post().to(auth_login))
                        .route("/api/auth/status", web::get().to(auth_status))
                        .route("/api/auth/logout", web::post().to(auth_logout))
                        // Upload API routes
                        .route("/api/uploads", web::post().to(start_upload_handler))
                        .route("/api/uploads", web::get().to(list_uploads_handler))
                        .route("/api/uploads/{id}", web::get().to(get_upload_handler))
                        .route("/api/uploads/{id}", web::delete().to(cancel_upload_handler))
                        // Studio API routes
                        .route("/api/studio/projects", web::get().to(list_studio_projects))
                        .route(
                            "/api/studio/projects/{id}/labels",
                            web::get().to(list_project_labels),
                        )
                        .route(
                            "/recorder-status",
                            web::get().to(user_mode_check_recorder_status),
                        )
                        .route("/current-recording", web::get().to(get_current_recording))
                        .service(
                            web::resource("/ws/dropped")
                                .route(web::get().to(websocket_handler_errors)),
                        )
                        .service(
                            web::resource("/rt/detect/mask")
                                .route(web::get().to(websocket_handler_high_priority)),
                        )
                        .service(
                            web::resource("/rt/{tail:.*}")
                                .route(web::get().to(websocket_handler_low_priority)),
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

fn map_mcap<P: AsRef<Utf8Path>>(p: P) -> res<Mmap> {
    let fd = std::fs::File::open(p.as_ref()).context("Couldn't open MCAP file")?;
    unsafe { Mmap::map(&fd) }.context("Couldn't map MCAP file")
}

fn read_mcap_info<P: AsRef<Utf8Path>>(path: P) -> res<(HashMap<String, TopicInfo>, f64)> {
    let file_length = 0.0;

    let mapped = map_mcap(&path)?;
    let summary = Summary::read(&mapped)?;

    let message_start_time: f64;
    let message_end_time: f64;
    let total_duration: f64;
    let mut topic_infos: HashMap<String, TopicInfo> = HashMap::new();
    let mut total_topic_duration = 0.0;
    let mut topic_count = 0;

    if let Some(summary) = summary {
        if let Some(ref stats) = summary.stats {
            debug!("Statistics: {:?}", stats);
            message_start_time = stats.message_start_time as f64;
            message_end_time = stats.message_end_time as f64;
            total_duration = (message_end_time - message_start_time) / 1_000_000_000.0;

            for (channel_id, channel_arc) in &summary.channels {
                let channel = channel_arc.as_ref();
                let topic = channel.topic.clone();

                let message_count = stats
                    .channel_message_counts
                    .get(channel_id)
                    .copied()
                    .unwrap_or(0);

                let duration = total_duration;
                let fps = if duration > 0.0 {
                    message_count as f64 / duration
                } else {
                    0.0
                };
                topic_infos.insert(
                    topic.clone(),
                    TopicInfo {
                        message_count: message_count.try_into().unwrap(),
                        average_fps: fps,
                        video_length: duration,
                    },
                );
                total_topic_duration += duration;
                topic_count += 1;
            }
        } else {
            error!("No statistics available.");
        }
    }

    let average_duration = if topic_count > 0 {
        total_topic_duration / topic_count as f64
    } else {
        file_length
    };

    Ok((topic_infos, average_duration))
}
