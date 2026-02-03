// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Upload management for EdgeFirst Studio integration.
//!
//! This module handles MCAP file uploads to EdgeFirst Studio, including:
//! - Background upload workers with progress tracking
//! - Status file persistence for power-loss recovery
//! - WebSocket progress broadcasting

use actix_web::{web, HttpResponse, Responder};
use chrono::{DateTime, Utc};
use edgefirst_client::{Client, Progress, ProjectID, SnapshotID};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::websocket::{Broadcast, MessageStream};

// ============================================================================
// Upload Manager Data Structures
// ============================================================================

/// Unique identifier for an upload task
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UploadId(pub Uuid);

impl UploadId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for UploadId {
    fn default() -> Self {
        Self::new()
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// Upload error response for API
#[derive(Serialize, Deserialize)]
pub struct UploadErrorResponse {
    pub error: String,
}

/// Start upload request body
#[derive(Deserialize)]
pub struct StartUploadRequest {
    pub mcap_path: String,
    pub mode: UploadMode,
}

/// Start upload response
#[derive(Serialize)]
pub struct StartUploadResponse {
    pub upload_id: UploadId,
    pub status: String,
}

// ============================================================================
// Upload Manager
// ============================================================================

/// Upload Manager - Manages upload tasks and EdgeFirst Studio client
pub struct UploadManager {
    tasks: Arc<RwLock<HashMap<UploadId, UploadTask>>>,
    pub client: Arc<RwLock<Option<Client>>>,
    storage_path: PathBuf,
    ws_broadcaster: Arc<MessageStream>,
}

impl UploadManager {
    /// Create a new UploadManager
    pub fn new(storage_path: PathBuf, broadcaster: Arc<MessageStream>) -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            client: Arc::new(RwLock::new(None)),
            storage_path,
            ws_broadcaster: broadcaster,
        }
    }

    /// Initialize the upload manager and scan for incomplete uploads (power
    /// loss recovery)
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
    /// server: "saas" (default), "stage", "test", or "dev"
    pub async fn authenticate(
        &self,
        username: &str,
        password: &str,
        server: &str,
    ) -> anyhow::Result<()> {
        info!(
            "Authenticating with EdgeFirst Studio ({}) as {}",
            server, username
        );

        // Create new client with specified server and authenticate
        let client = Client::new()?.with_server(server)?;
        let authenticated_client = client
            .with_login(username, password)
            .await
            .map_err(|e| anyhow::anyhow!("Authentication failed: {}", e))?;

        // Store authenticated client
        let mut client_lock = self.client.write().await;
        *client_lock = Some(authenticated_client);

        info!(
            "Successfully authenticated with EdgeFirst Studio ({})",
            server
        );
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
    pub fn get_status_file_path(mcap_path: &Path) -> PathBuf {
        let mut status_path = mcap_path.to_path_buf();
        let new_extension = match mcap_path.extension() {
            Some(ext) => format!("{}.upload-status.json", ext.to_string_lossy()),
            None => "upload-status.json".to_string(),
        };
        status_path.set_extension(new_extension);
        status_path
    }

    /// Write the upload status to a JSON file alongside the MCAP file
    pub async fn write_status_file(task: &UploadTask) -> anyhow::Result<()> {
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
    pub async fn read_status_file(path: &Path) -> anyhow::Result<UploadStatus> {
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
    pub async fn update_task_state(
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
        match serde_json::to_string(status) {
            Ok(json) => {
                self.ws_broadcaster.broadcast(Broadcast::Text(json));
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
    pub async fn delete_status_file(mcap_path: &Path) {
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

    /// Cancel all active uploads during shutdown.
    ///
    /// Returns the number of uploads that were cancelled.
    pub async fn cancel_all_uploads(&self) -> usize {
        let mut cancelled_count = 0;
        let mut tasks = self.tasks.write().await;

        for (id, task) in tasks.iter_mut() {
            // Only cancel active uploads (Queued, Uploading, Processing)
            if matches!(
                task.state,
                UploadState::Queued | UploadState::Uploading | UploadState::Processing
            ) {
                // Abort the task handle if it's running
                if let Some(handle) = task.task_handle.take() {
                    handle.abort();
                    info!("Aborted upload task {} during shutdown", id.0);
                }
                task.state = UploadState::Failed;
                task.error = Some("Cancelled due to server shutdown".to_string());
                task.message = "Cancelled due to server shutdown".to_string();
                task.updated_at = Utc::now();
                cancelled_count += 1;

                // Write updated status file (ignore errors during shutdown)
                let status = UploadStatus::from(&*task);
                if let Err(e) = Self::write_status_file_from_status(&status).await {
                    warn!("Failed to write status file during shutdown: {}", e);
                }
            }
        }

        cancelled_count
    }

    /// Wait for all active uploads to complete with a timeout.
    ///
    /// Returns true if all uploads completed, false if timeout was reached.
    pub async fn wait_for_completion(&self, timeout: std::time::Duration) -> bool {
        let start = std::time::Instant::now();

        loop {
            // Check if any uploads are still active
            let has_active = {
                let tasks = self.tasks.read().await;
                tasks.values().any(|t| {
                    matches!(
                        t.state,
                        UploadState::Queued | UploadState::Uploading | UploadState::Processing
                    )
                })
            };

            if !has_active {
                return true;
            }

            // Check timeout
            if start.elapsed() >= timeout {
                return false;
            }

            // Brief sleep before checking again
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

// ============================================================================
// Upload Worker
// ============================================================================

/// Background upload worker function
/// Handles the actual upload to EdgeFirst Studio
pub async fn upload_worker(
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

// ============================================================================
// HTTP Handlers
// ============================================================================

/// Trait for accessing upload manager from server context
pub trait UploadContext {
    fn upload_manager(&self) -> &Arc<UploadManager>;
}

/// POST /api/uploads - Start a new upload
pub async fn start_upload_handler<T: UploadContext>(
    body: web::Json<StartUploadRequest>,
    data: web::Data<T>,
) -> impl Responder {
    info!("Start upload request for: {}", body.mcap_path);

    let mcap_path = PathBuf::from(&body.mcap_path);

    match data
        .upload_manager()
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
pub async fn list_uploads_handler<T: UploadContext>(data: web::Data<T>) -> impl Responder {
    let uploads = data.upload_manager().list_uploads().await;
    HttpResponse::Ok().json(uploads)
}

/// GET /api/uploads/{id} - Get a specific upload
pub async fn get_upload_handler<T: UploadContext>(
    path: web::Path<String>,
    data: web::Data<T>,
) -> impl Responder {
    let id_str = path.into_inner();

    // Parse the UUID
    match Uuid::parse_str(&id_str) {
        Ok(uuid) => {
            let upload_id = UploadId(uuid);
            match data.upload_manager().get_upload(upload_id).await {
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
pub async fn cancel_upload_handler<T: UploadContext>(
    path: web::Path<String>,
    data: web::Data<T>,
) -> impl Responder {
    let id_str = path.into_inner();

    // Parse the UUID
    match Uuid::parse_str(&id_str) {
        Ok(uuid) => {
            let upload_id = UploadId(uuid);
            match data.upload_manager().cancel_upload(upload_id).await {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upload_id_uniqueness() {
        let id1 = UploadId::new();
        let id2 = UploadId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_upload_id_serialization() {
        let id = UploadId::new();
        let serialized = serde_json::to_string(&id).expect("Failed to serialize");
        let deserialized: UploadId =
            serde_json::from_str(&serialized).expect("Failed to deserialize");
        assert_eq!(id, deserialized);
    }

    #[test]
    fn test_upload_mode_basic_serialization() {
        let mode = UploadMode::Basic;
        let serialized = serde_json::to_string(&mode).expect("Failed to serialize");
        assert!(serialized.contains("Basic"));

        let deserialized: UploadMode =
            serde_json::from_str(&serialized).expect("Failed to deserialize");
        assert!(matches!(deserialized, UploadMode::Basic));
    }

    #[test]
    fn test_upload_mode_extended_serialization() {
        let mode = UploadMode::Extended {
            project_id: 123,
            labels: vec!["person".to_string(), "car".to_string()],
            dataset_name: Some("Test Dataset".to_string()),
            dataset_description: None,
        };

        let serialized = serde_json::to_string(&mode).expect("Failed to serialize");
        assert!(serialized.contains("Extended"));
        assert!(serialized.contains("123"));
        assert!(serialized.contains("person"));

        let deserialized: UploadMode =
            serde_json::from_str(&serialized).expect("Failed to deserialize");
        match deserialized {
            UploadMode::Extended {
                project_id,
                labels,
                dataset_name,
                dataset_description,
            } => {
                assert_eq!(project_id, 123);
                assert_eq!(labels, vec!["person", "car"]);
                assert_eq!(dataset_name, Some("Test Dataset".to_string()));
                assert!(dataset_description.is_none());
            }
            _ => panic!("Expected Extended mode"),
        }
    }

    #[test]
    fn test_upload_state_serialization() {
        let states = vec![
            UploadState::Queued,
            UploadState::Uploading,
            UploadState::Processing,
            UploadState::Completed,
            UploadState::Failed,
        ];

        for state in states {
            let serialized = serde_json::to_string(&state).expect("Failed to serialize");
            let deserialized: UploadState =
                serde_json::from_str(&serialized).expect("Failed to deserialize");
            assert_eq!(state, deserialized);
        }
    }

    #[test]
    fn test_status_file_path_generation() {
        let mcap_path = PathBuf::from("/data/recordings/test.mcap");
        let status_path = UploadManager::get_status_file_path(&mcap_path);
        assert_eq!(
            status_path,
            PathBuf::from("/data/recordings/test.mcap.upload-status.json")
        );
    }

    #[test]
    fn test_status_file_path_no_extension() {
        let mcap_path = PathBuf::from("/data/recordings/testfile");
        let status_path = UploadManager::get_status_file_path(&mcap_path);
        assert_eq!(
            status_path,
            PathBuf::from("/data/recordings/testfile.upload-status.json")
        );
    }

    #[test]
    fn test_start_upload_response_structure() {
        let response = StartUploadResponse {
            upload_id: UploadId::new(),
            status: "queued".to_string(),
        };

        let json = serde_json::to_string(&response).expect("Failed to serialize");
        assert!(json.contains("upload_id"));
        assert!(json.contains("\"status\":\"queued\""));
    }

    #[test]
    fn test_upload_error_response_structure() {
        let response = UploadErrorResponse {
            error: "File not found".to_string(),
        };

        let json = serde_json::to_string(&response).expect("Failed to serialize");
        assert!(json.contains("\"error\":\"File not found\""));
    }
}
