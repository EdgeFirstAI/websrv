// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! EdgeFirst WebUI Server Library
//!
//! This library provides the core functionality for the EdgeFirst WebUI server,
//! organized into domain-specific modules.

pub mod args;
pub mod auth;
pub mod config;
pub mod mcap;
pub mod recording;
pub mod services;
pub mod shutdown;
pub mod storage;
pub mod studio;
pub mod upload;
pub mod websocket;

// Re-export commonly used types for convenience
pub use args::{Args, WebUISettings};
pub use auth::{AuthRequest, AuthResponse, AuthStatusResponse};
pub use config::read_storage_directory;
pub use mcap::{list_mcap_files, DirectoryResponse, FileInfo, TopicInfo};
pub use recording::{extract_recording_filename, PlaybackParams, PlaybackResponse};
pub use storage::{check_storage_availability, FormattedSize, StorageDetails};
pub use studio::{LabelInfo, ProjectInfo};
pub use upload::{
    UploadErrorResponse, UploadId, UploadManager, UploadMode, UploadState, UploadStatus,
    UploadTask, UploadTaskInfo,
};
pub use websocket::{
    websocket_handler, websocket_handler_errors, Broadcast, MessageStream, WebSocketContext,
};
