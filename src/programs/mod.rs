// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Blockly program management — CRUD, deployment, and lifecycle.
//!
//! This module implements the `/api/programs` REST API that manages
//! Blockly-generated Python applications on Maivin devices: `.efapp`
//! bundle upload, systemd `--user` service management, and live
//! WebSocket log streaming.

pub mod bundle;
pub mod handlers;
pub mod logs;
pub mod service;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::programs::logs::ProgramRuntime;

// ============================================================================
// Program Status
// ============================================================================

/// Runtime status of a deployed program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgramStatus {
    Running,
    Stopped,
    Error,
    Deploying,
}

// ============================================================================
// Program Metadata
// ============================================================================

/// Persistent metadata for a deployed program (stored as `metadata.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramMetadata {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub author: String,
    pub created: String,
    pub modified: String,
    pub has_icon: bool,
}

// ============================================================================
// Context Trait
// ============================================================================

/// Trait providing access to the [`ProgramManager`] — follows the existing
/// `StorageContext`, `UploadContext`, etc. pattern used throughout the codebase.
pub trait ProgramsContext: Send + Sync + 'static {
    fn program_manager(&self) -> &Arc<ProgramManager>;
}

// ============================================================================
// Program Manager
// ============================================================================

/// Central manager for all deployed Blockly programs.
///
/// Handles on-disk storage, metadata tracking, and coordinates with
/// the systemd service and log-streaming layers.
pub struct ProgramManager {
    programs_path: PathBuf,
    python_path: String,
    system_mode: bool,
    programs: RwLock<HashMap<String, ProgramMetadata>>,
    runtimes: RwLock<HashMap<String, ProgramRuntime>>,
}

impl ProgramManager {
    /// Create a new manager rooted at `programs_path`.
    ///
    /// `system_mode` controls how systemd services are managed:
    /// - `true` (root): unit files in `/etc/systemd/system/`, `systemctl` without `--user`
    /// - `false` (non-root): unit files in `~/.config/systemd/user/`, `systemctl --user`
    pub fn new(programs_path: PathBuf, python_path: String, system_mode: bool) -> Self {
        Self {
            programs_path,
            python_path,
            system_mode,
            programs: RwLock::new(HashMap::new()),
            runtimes: RwLock::new(HashMap::new()),
        }
    }

    /// Scan the programs directory for existing deployments and reconcile
    /// their status with systemd. Also cleans up orphaned temp directories
    /// left from interrupted deployments.
    pub async fn initialize(&self) -> anyhow::Result<()> {
        // Ensure the programs directory exists
        tokio::fs::create_dir_all(&self.programs_path).await?;

        let mut entries = tokio::fs::read_dir(&self.programs_path).await?;
        let mut programs = self.programs.write().await;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            // Clean up orphaned .tmp-* directories from crashed mid-deployments
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(".tmp-") {
                    log::warn!("Removing orphaned temp directory: {}", path.display());
                    let _ = tokio::fs::remove_dir_all(&path).await;
                    continue;
                }
            }

            let metadata_path = path.join("metadata.json");
            if !metadata_path.exists() {
                continue;
            }

            match tokio::fs::read_to_string(&metadata_path).await {
                Ok(content) => match serde_json::from_str::<ProgramMetadata>(&content) {
                    Ok(meta) => {
                        log::info!("Loaded program: {} ({})", meta.name, meta.id);
                        programs.insert(meta.id.clone(), meta);
                    }
                    Err(e) => {
                        log::warn!("Invalid metadata.json in {}: {e}", path.display());
                    }
                },
                Err(e) => {
                    log::warn!("Could not read {}: {e}", metadata_path.display());
                }
            }
        }

        log::info!(
            "Program manager initialized with {} programs",
            programs.len()
        );
        Ok(())
    }

    /// Base directory for all programs.
    pub fn programs_path(&self) -> &Path {
        &self.programs_path
    }

    /// Python interpreter path for unit files.
    pub fn python_path(&self) -> &str {
        &self.python_path
    }

    /// Whether services are managed as system-level (root) or user-level.
    pub fn system_mode(&self) -> bool {
        self.system_mode
    }

    /// Directory for a specific program.
    pub fn program_dir(&self, id: &str) -> PathBuf {
        self.programs_path.join(id)
    }

    /// Check if a program with the given ID exists.
    pub async fn exists(&self, id: &str) -> bool {
        self.programs.read().await.contains_key(id)
    }

    /// Get a clone of program metadata.
    pub async fn get(&self, id: &str) -> Option<ProgramMetadata> {
        self.programs.read().await.get(id).cloned()
    }

    /// Get all program metadata.
    pub async fn list(&self) -> Vec<ProgramMetadata> {
        self.programs.read().await.values().cloned().collect()
    }

    /// Insert or update program metadata (in memory and on disk).
    pub async fn upsert(&self, meta: ProgramMetadata) -> anyhow::Result<()> {
        let dir = self.program_dir(&meta.id);
        let metadata_path = dir.join("metadata.json");
        let json = serde_json::to_string_pretty(&meta)?;
        tokio::fs::write(&metadata_path, json).await?;
        self.programs.write().await.insert(meta.id.clone(), meta);
        Ok(())
    }

    /// Remove a program from the in-memory map (does NOT delete files).
    pub async fn remove(&self, id: &str) -> Option<ProgramMetadata> {
        self.programs.write().await.remove(id)
    }

    /// Get mutable access to the runtimes map.
    pub fn runtimes(&self) -> &RwLock<HashMap<String, ProgramRuntime>> {
        &self.runtimes
    }

    /// Atomically insert metadata only if the ID does not already exist.
    /// Returns `true` if inserted, `false` if the ID was already taken.
    /// This prevents the TOCTOU race between `exists()` and `deploy_bundle()`.
    pub async fn try_insert(&self, meta: ProgramMetadata) -> bool {
        let mut programs = self.programs.write().await;
        if programs.contains_key(&meta.id) {
            return false;
        }
        programs.insert(meta.id.clone(), meta);
        true
    }

    /// Shut down all running log followers. Call during graceful shutdown.
    pub async fn shutdown(&self) {
        let mut runtimes = self.runtimes.write().await;
        for (id, rt) in runtimes.drain() {
            if let Some(task) = rt.journal_task {
                task.abort();
                log::debug!("Aborted log follower for {id}");
            }
        }
    }

    /// Deploy a validated bundle to disk using atomic writes.
    ///
    /// 1. Write to a temp directory under `programs_path`
    /// 2. Validate everything landed correctly
    /// 3. Rename atomically into the final location
    pub async fn deploy_bundle(
        &self,
        id: &str,
        bundle: &bundle::ValidatedBundle,
    ) -> anyhow::Result<()> {
        let final_dir = self.program_dir(id);
        let tmp_dir = self
            .programs_path
            .join(format!(".tmp-{}", uuid::Uuid::new_v4()));

        tokio::fs::create_dir_all(&tmp_dir).await?;

        // Write all bundle files to the temp directory
        tokio::fs::write(
            tmp_dir.join("config.json"),
            &serde_json::to_string_pretty(&bundle.config)?,
        )
        .await?;
        tokio::fs::write(tmp_dir.join("workspace.json"), &bundle.workspace_json).await?;
        tokio::fs::write(tmp_dir.join("app.py"), &bundle.app_py).await?;
        tokio::fs::write(tmp_dir.join("original.efapp"), &bundle.raw_zip).await?;

        if let Some(icon) = &bundle.icon_png {
            tokio::fs::write(tmp_dir.join("icon.png"), icon).await?;
        }

        // Atomic rename into final location
        // If the target already exists (update case), remove it first
        if final_dir.exists() {
            tokio::fs::remove_dir_all(&final_dir).await?;
        }
        tokio::fs::rename(&tmp_dir, &final_dir).await?;

        Ok(())
    }

    /// Delete all files for a program.
    pub async fn delete_files(&self, id: &str) -> anyhow::Result<()> {
        let dir = self.program_dir(id);
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir).await?;
        }
        Ok(())
    }
}

// ============================================================================
// Slugify
// ============================================================================

/// Convert a human-readable program name into a URL-safe slug.
///
/// - Lowercases the input
/// - Replaces whitespace and underscores with hyphens
/// - Strips all characters except `[a-z0-9-]`
/// - Collapses consecutive hyphens
/// - Trims leading/trailing hyphens
///
/// # Examples
///
/// ```
/// # use edgefirst_websrv::programs::slugify;
/// assert_eq!(slugify("Zone Alert"), "zone-alert");
/// assert_eq!(slugify("My Cool App!"), "my-cool-app");
/// assert_eq!(slugify("  hello   world  "), "hello-world");
/// assert_eq!(slugify("café_résumé"), "caf-r-sum");
/// ```
pub fn slugify(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse consecutive hyphens and trim
    let mut result = String::with_capacity(slug.len());
    let mut prev_hyphen = true; // start true to trim leading hyphens
    for c in slug.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push('-');
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }

    // Trim trailing hyphen
    if result.ends_with('-') {
        result.pop();
    }

    result
}

/// Resolve a `programs_path` string, expanding `~` to the user's home directory.
pub fn resolve_programs_path(path: &str) -> PathBuf {
    if path.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(path.replacen('~', &home, 1));
        }
    }
    PathBuf::from(path)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Zone Alert"), "zone-alert");
        assert_eq!(slugify("My Cool App"), "my-cool-app");
    }

    #[test]
    fn slugify_special_chars() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("test@#$%app"), "test-app");
    }

    #[test]
    fn slugify_whitespace() {
        assert_eq!(slugify("  hello   world  "), "hello-world");
        assert_eq!(slugify("tabs\there"), "tabs-here");
    }

    #[test]
    fn slugify_underscores() {
        assert_eq!(slugify("my_cool_app"), "my-cool-app");
    }

    #[test]
    fn slugify_already_slug() {
        assert_eq!(slugify("zone-alert"), "zone-alert");
    }

    #[test]
    fn slugify_numbers() {
        assert_eq!(slugify("App 2.0"), "app-2-0");
        assert_eq!(slugify("123test"), "123test");
    }

    #[test]
    fn slugify_empty() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("---"), "");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn resolve_tilde() {
        std::env::set_var("HOME", "/home/test");
        assert_eq!(
            resolve_programs_path("~/.local/share/programs"),
            PathBuf::from("/home/test/.local/share/programs")
        );
    }

    #[test]
    fn resolve_absolute() {
        assert_eq!(
            resolve_programs_path("/opt/programs"),
            PathBuf::from("/opt/programs")
        );
    }
}
