// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Configuration file reading and service configuration management.

use axum::extract::{Json, Path};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use log::{debug, error};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::io;
use std::path::{Path as FsPath, PathBuf};
use std::sync::OnceLock;

use crate::envfile::{parse_config_content, plan_edit};

const EDGEFIRST_PREFIX: &str = "edgefirst-";

/// Directory holding service configuration files.
///
/// Set once at startup from `Args::config_dir`. A `OnceLock` rather than a
/// threaded parameter because [`read_storage_directory`] is a free function
/// called from four places that have no server state in scope, and the value
/// is an immutable startup constant.
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set the configuration directory. Only the first call has any effect.
pub fn init_config_dir(dir: PathBuf) {
    if CONFIG_DIR.set(dir).is_err() {
        debug!("Configuration directory already initialised; ignoring");
    }
}

/// The configuration directory, defaulting to `/etc/default`.
fn config_dir() -> &'static FsPath {
    CONFIG_DIR
        .get()
        .map(PathBuf::as_path)
        .unwrap_or_else(|| FsPath::new("/etc/default"))
}

/// The outcome of resolving a service name to a configuration file.
pub(crate) struct Resolved {
    /// The path to use: the one that exists, or the primary candidate.
    pub path: PathBuf,
    /// Whether any candidate actually exists.
    // Scaffolding: unused until Task 6 reports it. Remove this attribute then.
    #[allow(dead_code)]
    pub exists: bool,
    /// Every candidate examined, in order, for error reporting.
    // Scaffolding: unused until Task 6 reports it. Remove this attribute then.
    #[allow(dead_code)]
    pub tried: Vec<String>,
}

/// Resolve a service name to a config file, supporting both
/// `edgefirst-{service}` and `{service}` naming conventions.
pub(crate) fn resolve_config(service: &str) -> Resolved {
    let alt_name = match service.strip_prefix(EDGEFIRST_PREFIX) {
        Some(short) => short.to_string(),
        None => format!("{EDGEFIRST_PREFIX}{service}"),
    };

    let primary = config_dir().join(service);
    let alternate = config_dir().join(&alt_name);
    let tried = vec![
        primary.to_string_lossy().into_owned(),
        alternate.to_string_lossy().into_owned(),
    ];

    if primary.exists() {
        return Resolved {
            path: primary,
            exists: true,
            tried,
        };
    }
    if alternate.exists() {
        debug!("Config {:?} not found, using {:?}", primary, alternate);
        return Resolved {
            path: alternate,
            exists: true,
            tried,
        };
    }

    // Neither found — return the primary so callers get the expected error.
    Resolved {
        path: primary,
        exists: false,
        tried,
    }
}

/// Resolve a service name to a config file path.
fn resolve_config_file(service: &str) -> String {
    resolve_config(service).path.to_string_lossy().into_owned()
}

/// Read storage directory from /etc/default/recorder (or edgefirst-recorder)
pub fn read_storage_directory() -> io::Result<String> {
    let file_path = resolve_config_file("recorder");
    let content = std::fs::read_to_string(&file_path)?;
    let storage_dir = parse_storage_directory(&content)?;
    debug!("MCAP Directory: {:?}", storage_dir);
    Ok(storage_dir)
}

/// Service configuration path parameter
#[derive(Deserialize)]
pub struct ConfigPath {
    pub service: String,
}

/// Get service configuration from /etc/default/{service}
pub async fn get_config(Path(path): Path<ConfigPath>) -> impl IntoResponse {
    let service_name = &path.service;
    let config_file_path = resolve_config_file(service_name);

    let config_content = std::fs::read_to_string(&config_file_path).unwrap_or_default();
    let config_map = parse_config_content(&config_content);

    Json(serde_json::Value::Object(config_map)).into_response()
}

/// Check service status and restart if active
pub async fn check_service_status(service_name: &str) -> Result<String, String> {
    use std::process::Command;

    use crate::services::resolve_service_name;

    let resolved = resolve_service_name(service_name);
    let service_status = Command::new("systemctl")
        .arg("is-active")
        .arg(&resolved)
        .output()
        .map_err(|e| format!("Error checking service status: {:?}", e))?;

    let status = String::from_utf8_lossy(&service_status.stdout)
        .trim()
        .to_string();
    debug!("{:?} service is {:?}", resolved, status);
    if status == "active" {
        Command::new("systemctl")
            .arg("restart")
            .arg(&resolved)
            .output()
            .map_err(|e| format!("Error restarting service: {:?}", e))?;
        Ok(format!("Service '{}' restarted successfully.", resolved))
    } else {
        Ok(format!(
            "Service '{}' is not running. No action taken.",
            resolved
        ))
    }
}

// ============================================================================
// Internal parsing functions (extracted for testability)
// ============================================================================

/// Expand shell-style environment variables (`$VAR` and `${VAR}`) in a string.
fn expand_env_vars(input: &str) -> String {
    let re = Regex::new(r"\$\{([^}]+)\}|\$([A-Za-z_][A-Za-z0-9_]*)").unwrap();
    re.replace_all(input, |caps: &regex::Captures| {
        let var_name = caps.get(1).or_else(|| caps.get(2)).unwrap().as_str();
        std::env::var(var_name).unwrap_or_default()
    })
    .to_string()
}

/// Parse storage directory from config file content
pub fn parse_storage_directory(content: &str) -> io::Result<String> {
    for line in content.lines() {
        if line.starts_with("STORAGE") {
            let parts: Vec<&str> = line.split('=').collect();
            if parts.len() == 2 {
                let raw = parts[1].trim().trim_matches('"');
                return Ok(expand_env_vars(raw));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "STORAGE directory not found in configuration",
    ))
}

/// Set service configuration in /etc/default/{service}
pub async fn set_config(Json(params): Json<Value>) -> impl IntoResponse {
    let file_name = if let Some(file_name_value) = params.get("fileName") {
        if let Some(file_name) = file_name_value.as_str() {
            file_name.to_string()
        } else {
            error!("fileName is not a string");
            return (StatusCode::BAD_REQUEST, "Invalid fileName").into_response();
        }
    } else {
        error!("fileName not found in JSON");
        return (StatusCode::BAD_REQUEST, "Missing fileName").into_response();
    };

    // Validate fileName to prevent path traversal
    if file_name.contains('/') || file_name.contains("..") {
        error!("Invalid fileName: path traversal attempt detected");
        return (StatusCode::BAD_REQUEST, "Invalid fileName").into_response();
    }
    if !file_name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        error!("Invalid fileName: contains disallowed characters");
        return (StatusCode::BAD_REQUEST, "Invalid fileName").into_response();
    }

    let service_name = file_name.clone();

    let config_file_path = resolve_config_file(&file_name);
    debug!("Configuration file path: {}", config_file_path.clone());
    debug!("{:?}", params);

    let config_content = match std::fs::read_to_string(config_file_path.clone()) {
        Ok(content) => content,
        Err(e) => {
            error!("Error reading configuration file: {:?}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Error reading configuration file",
            )
                .into_response();
        }
    };

    // `fileName` names the target file, not a setting; strip it before
    // planning the edit so it is never written into the config file.
    let config_map: serde_json::Map<String, Value> = params
        .as_object()
        .map(|map| {
            map.iter()
                .filter(|(key, _)| !key.eq_ignore_ascii_case("filename"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();

    // TODO(EDGEAI-1402 Task 6): this whole handler body is replaced wholesale
    // by Task 6, which reports per-key dispositions instead of writing plain
    // text. This is a minimal stopgap so the crate keeps compiling now that
    // `update_config_content` is gone. Task 6 replaces this silent filename
    // filter with a reported `reserved` list instead of a silent drop.
    let plan = match plan_edit(&config_content, &config_map) {
        Ok(plan) => plan,
        Err(rejected) => {
            error!("Rejected configuration keys: {:?}", rejected);
            return (StatusCode::BAD_REQUEST, "Invalid configuration values").into_response();
        }
    };

    match std::fs::write(config_file_path.clone(), plan.content) {
        Ok(_) => match check_service_status(&service_name).await {
            Ok(_) => (
                StatusCode::OK,
                "Configuration saved successfully and service status checked.",
            )
                .into_response(),
            Err(e) => {
                error!("{}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Error handling service status",
                )
                    .into_response()
            }
        },
        Err(e) => {
            error!("Error saving configuration: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Error saving configuration",
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // Storage directory parsing tests
    // ========================================================================

    #[test]
    fn test_parse_storage_directory_valid() {
        let content = r#"
STORAGE=/media/DATA/recordings
OTHER_VAR=value
"#;
        let result = parse_storage_directory(content);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/media/DATA/recordings");
    }

    #[test]
    fn test_parse_storage_directory_with_quotes() {
        let content = r#"STORAGE="/path/with spaces/data""#;
        let result = parse_storage_directory(content);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/path/with spaces/data");
    }

    #[test]
    fn test_parse_storage_directory_missing() {
        let content = r#"
OTHER_VAR=value
ANOTHER=123
"#;
        let result = parse_storage_directory(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("STORAGE"));
    }

    #[test]
    fn test_parse_storage_directory_empty() {
        let content = "";
        let result = parse_storage_directory(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_storage_directory_env_var() {
        std::env::set_var("TEST_WEBSRV_HOME", "/home/testuser");
        let content = r#"STORAGE="$TEST_WEBSRV_HOME/recordings""#;
        let result = parse_storage_directory(content);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/home/testuser/recordings");
        std::env::remove_var("TEST_WEBSRV_HOME");
    }

    #[test]
    fn test_parse_storage_directory_env_var_braces() {
        std::env::set_var("TEST_WEBSRV_HOME2", "/home/testuser");
        let content = r#"STORAGE="${TEST_WEBSRV_HOME2}/recordings""#;
        let result = parse_storage_directory(content);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/home/testuser/recordings");
        std::env::remove_var("TEST_WEBSRV_HOME2");
    }

    #[test]
    fn test_expand_env_vars_no_vars() {
        assert_eq!(
            expand_env_vars("/media/DATA/recordings"),
            "/media/DATA/recordings"
        );
    }

    #[test]
    fn test_expand_env_vars_unset_var() {
        std::env::remove_var("THIS_VAR_SHOULD_NOT_EXIST_EVER");
        assert_eq!(
            expand_env_vars("$THIS_VAR_SHOULD_NOT_EXIST_EVER/data"),
            "/data"
        );
    }

    // ========================================================================
    // Struct deserialization tests
    // ========================================================================

    #[test]
    fn test_config_path_deserialization() {
        let json = r#"{"service": "recorder"}"#;
        let path: ConfigPath = serde_json::from_str(json).expect("Failed to deserialize");
        assert_eq!(path.service, "recorder");
    }

    // ========================================================================
    // Config file resolution tests
    // ========================================================================

    #[test]
    fn resolve_config_reports_both_candidate_paths() {
        let r = resolve_config("nonexistent-test-service-xyz");
        assert!(!r.exists);
        assert_eq!(r.tried.len(), 2);
        assert!(r.tried[0].ends_with("/nonexistent-test-service-xyz"));
        assert!(r.tried[1].ends_with("/edgefirst-nonexistent-test-service-xyz"));
    }

    #[test]
    fn resolve_config_strips_the_edgefirst_prefix_for_the_alternate() {
        let r = resolve_config("edgefirst-nonexistent-test-xyz");
        assert_eq!(r.tried.len(), 2);
        assert!(r.tried[0].ends_with("/edgefirst-nonexistent-test-xyz"));
        assert!(r.tried[1].ends_with("/nonexistent-test-xyz"));
    }

    #[test]
    fn config_dir_defaults_to_etc_default_when_uninitialised() {
        // Only meaningful when nothing has called init_config_dir in this
        // binary; the lib test binary does not.
        assert!(resolve_config("anything").tried[0].starts_with("/etc/default/"));
    }
}
