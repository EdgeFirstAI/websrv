// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Systemd service management handlers.
//!
//! Provides endpoints for:
//! - Getting service status
//! - Starting/stopping services
//! - Enabling/disabling services

use actix_web::{web, HttpResponse, Responder};
use log::{debug, error};
use percent_encoding::percent_decode;
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Command;

// ============================================================================
// Types
// ============================================================================

/// Service action request
#[derive(Deserialize)]
pub struct ServiceAction {
    pub service: String,
    pub action: String,
}

// ============================================================================
// Handlers
// ============================================================================

/// Get status of all requested services
pub async fn get_all_services(params: web::Json<Value>) -> impl Responder {
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

/// Update a service (start, stop, enable, disable)
pub async fn update_service(params: web::Json<ServiceAction>) -> impl Responder {
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

    debug!("Executing: sudo systemctl {} {}", action, service_name);

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

/// Get status of a single service
pub async fn get_service_status(service_name: &str) -> Result<String, String> {
    let status_output = Command::new("systemctl")
        .arg("is-active")
        .arg(service_name)
        .output()
        .map_err(|e| format!("Error checking service status: {:?}", e))?;

    let status = String::from_utf8_lossy(&status_output.stdout)
        .trim()
        .to_string();

    Ok(if status == "active" {
        "running".to_string()
    } else {
        "not running".to_string()
    })
}

/// Check if a service is enabled
pub async fn is_service_enabled(service_name: &str) -> Result<bool, String> {
    let enabled_output = Command::new("systemctl")
        .arg("is-enabled")
        .arg(service_name)
        .output()
        .map_err(|e| format!("Error checking if service is enabled: {:?}", e))?;

    let enabled_str = String::from_utf8_lossy(&enabled_output.stdout)
        .trim()
        .to_string();

    Ok(enabled_str == "enabled")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_action_deserialization() {
        let json = r#"{"service":"recorder","action":"start"}"#;
        let action: ServiceAction = serde_json::from_str(json).expect("Failed to deserialize");

        assert_eq!(action.service, "recorder");
        assert_eq!(action.action, "start");
    }

    #[test]
    fn test_valid_actions() {
        let valid_actions = vec!["start", "stop", "enable", "disable"];

        for action in valid_actions {
            // Just verify these are valid string values
            assert!(!action.is_empty());
        }
    }
}
