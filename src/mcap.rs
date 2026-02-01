// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! MCAP file handling utilities.
//!
//! Provides functions for:
//! - Reading MCAP file metadata
//! - Memory-mapping MCAP files
//! - Streaming MCAP downloads

use actix_web::{
    http::header::ContentLength,
    web::{self, Bytes},
    HttpRequest, HttpResponse, Responder,
};
use actix_web_actors::ws;
use anyhow::{Context, Result};
use async_stream::stream;
use camino::Utf8Path;
use chrono::DateTime;
use log::{debug, error};
use mcap::Summary;
use memmap::Mmap;
use mime::Mime;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::time::UNIX_EPOCH;
use tokio::io::AsyncReadExt as _;

use crate::config::read_storage_directory;

// ============================================================================
// Types
// ============================================================================

/// File information for MCAP listings
#[derive(Serialize)]
pub struct FileInfo {
    pub name: String,
    pub size: u64, // Size in MB
    pub created: String,
    pub topics: HashMap<String, TopicInfo>,
    pub average_video_length: f64,
}

/// Directory response with MCAP files
#[derive(Serialize)]
pub struct DirectoryResponse {
    pub dir_name: String,
    pub files: Option<Vec<FileInfo>>,
    pub message: Option<String>,
    pub topics: Option<Vec<String>>,
}

/// Topic information from MCAP
#[derive(Serialize)]
pub struct TopicInfo {
    pub message_count: usize,
    pub average_fps: f64,
    pub video_length: f64,
}

// ============================================================================
// MCAP Reading Functions
// ============================================================================

/// Memory-map an MCAP file
pub fn map_mcap<P: AsRef<Utf8Path>>(p: P) -> Result<Mmap> {
    let fd = std::fs::File::open(p.as_ref()).context("Couldn't open MCAP file")?;
    unsafe { Mmap::map(&fd) }.context("Couldn't map MCAP file")
}

/// Read MCAP file info including topics and durations
pub fn read_mcap_info<P: AsRef<Utf8Path>>(path: P) -> Result<(HashMap<String, TopicInfo>, f64)> {
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

// ============================================================================
// WebSocket Session for MCAP Directory Listing
// ============================================================================

/// Trait for accessing server context
pub trait McapContext {
    fn is_system_mode(&self) -> bool;
    fn storage_path(&self) -> &str;
}

/// WebSocket session for MCAP file listing
pub struct WebSocketSession<T> {
    pub context: Option<web::Data<T>>,
}

impl<T: McapContext + Unpin + 'static> actix::Actor for WebSocketSession<T> {
    type Context = ws::WebsocketContext<Self>;
}

impl<T: McapContext + Unpin + 'static> actix::StreamHandler<Result<ws::Message, ws::ProtocolError>>
    for WebSocketSession<T>
{
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
                let mut directory: String = arg_data.storage_path().to_string();

                if arg_data.is_system_mode() {
                    directory = match read_storage_directory() {
                        Ok(dir) => dir,
                        Err(e) => {
                            error!("Error reading directory from config: {}", e);
                            let response = serde_json::to_string(
                                &serde_json::json!({"error": "Error reading directory from config"}),
                            )
                            .unwrap();
                            ctx.text(response);
                            return;
                        }
                    };
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

// ============================================================================
// HTTP Handlers
// ============================================================================

/// MCAP file download handler
pub async fn mcap_downloader(req: HttpRequest) -> impl Responder {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_directory_response_serialization() {
        let response = DirectoryResponse {
            dir_name: "/data".to_string(),
            files: None,
            message: Some("No files".to_string()),
            topics: None,
        };

        let json = serde_json::to_string(&response).expect("Failed to serialize");
        assert!(json.contains("\"dir_name\":\"/data\""));
        assert!(json.contains("\"message\":\"No files\""));
    }

    #[test]
    fn test_file_info_serialization() {
        let mut topics = HashMap::new();
        topics.insert(
            "test_topic".to_string(),
            TopicInfo {
                message_count: 100,
                average_fps: 30.0,
                video_length: 10.0,
            },
        );

        let file_info = FileInfo {
            name: "test.mcap".to_string(),
            size: 1024,
            created: "2024-01-15 10:00:00".to_string(),
            topics,
            average_video_length: 10.0,
        };

        let json = serde_json::to_string(&file_info).expect("Failed to serialize");
        assert!(json.contains("\"name\":\"test.mcap\""));
        assert!(json.contains("\"size\":1024"));
        assert!(json.contains("\"test_topic\""));
    }

    #[test]
    fn test_topic_info_serialization() {
        let topic = TopicInfo {
            message_count: 500,
            average_fps: 25.5,
            video_length: 20.0,
        };

        let json = serde_json::to_string(&topic).expect("Failed to serialize");
        assert!(json.contains("\"message_count\":500"));
        assert!(json.contains("\"average_fps\":25.5"));
        assert!(json.contains("\"video_length\":20.0"));
    }
}
