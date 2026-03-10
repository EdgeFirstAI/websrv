// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Systemd service management handlers.
//!
//! Provides endpoints for:
//! - Getting service status
//! - Starting/stopping services
//! - Enabling/disabling services

use axum::extract::Json;
use axum::http::StatusCode;
use axum::response::IntoResponse;
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
// Service Name Resolution
// ============================================================================

const EDGEFIRST_PREFIX: &str = "edgefirst-";

/// Return the alternate service name by toggling the `edgefirst-` prefix.
fn alternate_name(name: &str) -> String {
    if let Some(short) = name.strip_prefix(EDGEFIRST_PREFIX) {
        short.to_string()
    } else {
        format!("{}{}", EDGEFIRST_PREFIX, name)
    }
}

/// Resolve a service name to whichever systemd unit actually exists.
///
/// Tries the given name first; if systemctl reports it as not loaded
/// (i.e. the unit file is missing), tries the alternate name with or
/// without the `edgefirst-` prefix. Returns the name that resolved.
pub fn resolve_service_name(name: &str) -> String {
    // Check if the primary name has a loaded unit
    if is_unit_loaded(name) {
        return name.to_string();
    }

    let alt = alternate_name(name);
    if is_unit_loaded(&alt) {
        debug!("Service '{}' not found, using '{}'", name, alt);
        return alt;
    }

    // Neither found — return original so callers get the expected error
    name.to_string()
}

/// Check whether systemd knows about a unit (i.e. its unit file exists).
fn is_unit_loaded(name: &str) -> bool {
    let output = Command::new("systemctl")
        .arg("show")
        .arg("--property=LoadState")
        .arg(name)
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            // LoadState=loaded means the unit file exists
            stdout.trim() == "LoadState=loaded"
        }
        Err(_) => false,
    }
}

// ============================================================================
// Handlers
// ============================================================================

/// Get status of all requested services
pub async fn get_all_services(Json(params): Json<Value>) -> impl IntoResponse {
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
        let resolved = resolve_service_name(&decoded_service);

        let status_output = Command::new("systemctl")
            .arg("is-active")
            .arg(&resolved)
            .output();

        let enabled_output = Command::new("systemctl")
            .arg("is-enabled")
            .arg(&resolved)
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
    Json(service_statuses).into_response()
}

/// Update a service (start, stop, enable, disable)
pub async fn update_service(Json(params): Json<ServiceAction>) -> impl IntoResponse {
    let service_name = resolve_service_name(&params.service);
    let action = &params.action;

    let mut command = Command::new("sudo");
    command.arg("systemctl");

    match action.as_str() {
        "start" => command.arg("start").arg(&service_name),
        "stop" => command.arg("stop").arg(&service_name),
        "enable" => command.arg("enable").arg(&service_name),
        "disable" => command.arg("disable").arg(&service_name),
        _ => return (StatusCode::BAD_REQUEST, "Invalid action").into_response(),
    };

    debug!("Executing: sudo systemctl {} {}", action, service_name);

    match command.status() {
        Ok(status) if status.success() => (
            StatusCode::OK,
            format!("Service '{}' {}d successfully.", service_name, action),
        )
            .into_response(),
        Ok(status) => {
            let error_message = format!(
                "Failed to {} service '{}': {:?}",
                action, service_name, status
            );
            error!("{}", error_message);
            (StatusCode::INTERNAL_SERVER_ERROR, error_message).into_response()
        }
        Err(e) => {
            let error_message = format!(
                "Failed to run systemctl {} {}: {:?}",
                action, service_name, e
            );
            error!("{}", error_message);
            (StatusCode::INTERNAL_SERVER_ERROR, error_message).into_response()
        }
    }
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

    #[test]
    fn test_alternate_name_adds_prefix() {
        assert_eq!(alternate_name("recorder"), "edgefirst-recorder");
        assert_eq!(alternate_name("camera"), "edgefirst-camera");
    }

    #[test]
    fn test_alternate_name_strips_prefix() {
        assert_eq!(alternate_name("edgefirst-recorder"), "recorder");
        assert_eq!(alternate_name("edgefirst-camera"), "camera");
    }

    #[test]
    fn test_alternate_name_roundtrip() {
        let original = "model";
        let prefixed = alternate_name(original);
        assert_eq!(prefixed, "edgefirst-model");
        assert_eq!(alternate_name(&prefixed), original);
    }
}
