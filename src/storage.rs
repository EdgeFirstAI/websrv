// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Storage utilities for checking disk space and availability.

use actix_web::{web, HttpResponse, Responder};
use log::error;
use serde::Serialize;
use serde_json::json;
use std::path::Path;

use crate::config::read_storage_directory;

/// Formatted size with value and unit (MB or GB)
#[derive(Serialize)]
pub struct FormattedSize {
    pub value: f64,
    pub unit: String,
}

impl FormattedSize {
    /// Convert bytes to a human-readable format (MB or GB)
    pub fn from_bytes(bytes: u64) -> Self {
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

/// Storage details response
#[derive(Serialize)]
pub struct StorageDetails {
    pub path: String,
    pub exists: bool,
    pub available_space: Option<FormattedSize>,
    pub total_space: Option<FormattedSize>,
}

/// Server context trait for accessing application state
/// This allows storage module to work with the server context without circular dependencies
pub trait StorageContext {
    fn is_system_mode(&self) -> bool;
    fn storage_path(&self) -> &str;
}

/// Check storage availability handler
pub async fn check_storage_availability<T: StorageContext>(data: web::Data<T>) -> impl Responder {
    let storage_dir = if data.is_system_mode() {
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
        data.storage_path().to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_formatted_size_bytes_to_mb() {
        let size = FormattedSize::from_bytes(512 * 1024 * 1024); // 512 MB
        assert!((size.value - 512.0).abs() < 0.01);
        assert_eq!(size.unit, "MB");
    }

    #[test]
    fn test_formatted_size_bytes_to_gb() {
        let size = FormattedSize::from_bytes(2 * 1024 * 1024 * 1024); // 2 GB
        assert!((size.value - 2.0).abs() < 0.01);
        assert_eq!(size.unit, "GB");
    }

    #[test]
    fn test_formatted_size_boundary() {
        // Exactly 1GB should be GB
        let size = FormattedSize::from_bytes(1024 * 1024 * 1024);
        assert_eq!(size.unit, "GB");
        assert!((size.value - 1.0).abs() < 0.01);

        // Just under 1GB should be MB
        let size = FormattedSize::from_bytes(1024 * 1024 * 1024 - 1);
        assert_eq!(size.unit, "MB");
    }
}
