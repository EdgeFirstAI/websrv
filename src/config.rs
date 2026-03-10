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

const EDGEFIRST_PREFIX: &str = "edgefirst-";

/// Resolve a config file path under `/etc/default/`, supporting both
/// `edgefirst-{service}` and `{service}` naming conventions.
///
/// Tries the given name first; if the file doesn't exist, tries the
/// alternate name with or without the `edgefirst-` prefix.
/// Returns the path that exists, or the original path if neither does.
fn resolve_config_file(service: &str) -> String {
    let primary = format!("/etc/default/{}", service);
    if std::path::Path::new(&primary).exists() {
        return primary;
    }

    let alt_name = if let Some(short) = service.strip_prefix(EDGEFIRST_PREFIX) {
        short.to_string()
    } else {
        format!("{}{}", EDGEFIRST_PREFIX, service)
    };
    let alternate = format!("/etc/default/{}", alt_name);
    if std::path::Path::new(&alternate).exists() {
        debug!("Config '{}' not found, using '{}'", primary, alternate);
        return alternate;
    }

    // Neither found — return original so callers get the expected error
    primary
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

/// Parse config file content into a JSON map
pub fn parse_config_content(content: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut config_map = serde_json::Map::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let clean_key = key.trim();
            let raw_value = value.trim();

            // If the value is quoted, treat it as a single string value
            if raw_value.starts_with('"') && raw_value.ends_with('"') && raw_value.len() >= 2 {
                let unquoted = &raw_value[1..raw_value.len() - 1];
                config_map.insert(
                    clean_key.to_string(),
                    serde_json::Value::String(unquoted.to_string()),
                );
            } else {
                // Unquoted: split on whitespace for multiple values
                let clean_value = raw_value.replace("\"", "");
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
    }

    config_map
}

/// Update config content with new values
pub fn update_config_content(
    original_content: &str,
    updates: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut updated_config = String::new();

    for line in original_content.lines() {
        let mut found = false;

        for (key, value) in updates {
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
            updated_config.push_str(&format!("{}\n", line));
        }
    }

    updated_config
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

    let config_map = if let Some(map) = params.as_object() {
        map.clone()
    } else {
        serde_json::Map::new()
    };

    let updated_config = update_config_content(&config_content, &config_map);

    match std::fs::write(config_file_path.clone(), updated_config) {
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
    // Config content parsing tests
    // ========================================================================

    #[test]
    fn test_parse_config_content_simple() {
        let content = r#"
KEY1=value1
KEY2=value2
"#;
        let result = parse_config_content(content);
        assert_eq!(result.len(), 2);
        assert_eq!(result.get("KEY1").unwrap(), "value1");
        assert_eq!(result.get("KEY2").unwrap(), "value2");
    }

    #[test]
    fn test_parse_config_content_with_quotes() {
        let content = r#"
PATH="/usr/local/bin"
NAME="My Service"
"#;
        let result = parse_config_content(content);
        assert_eq!(result.get("PATH").unwrap(), "/usr/local/bin");
        assert_eq!(result.get("NAME").unwrap(), "My Service");
    }

    #[test]
    fn test_parse_config_content_with_comments() {
        let content = r#"
# This is a comment
KEY1=value1
# Another comment
KEY2=value2
"#;
        let result = parse_config_content(content);
        assert_eq!(result.len(), 2);
        assert!(!result.contains_key("# This is a comment"));
    }

    #[test]
    fn test_parse_config_content_empty_lines() {
        let content = r#"
KEY1=value1

KEY2=value2

"#;
        let result = parse_config_content(content);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_parse_config_content_multiple_values() {
        let content = r#"TOPICS=topic1 topic2 topic3"#;
        let result = parse_config_content(content);
        let topics = result.get("TOPICS").unwrap().as_array().unwrap();
        assert_eq!(topics.len(), 3);
        assert_eq!(topics[0], "topic1");
        assert_eq!(topics[1], "topic2");
        assert_eq!(topics[2], "topic3");
    }

    #[test]
    fn test_parse_config_content_empty() {
        let content = "";
        let result = parse_config_content(content);
        assert!(result.is_empty());
    }

    // ========================================================================
    // Config update tests
    // ========================================================================

    #[test]
    fn test_update_config_content_simple() {
        let original = "KEY1=old_value\nKEY2=keep_this\n";
        let mut updates = serde_json::Map::new();
        updates.insert(
            "KEY1".to_string(),
            serde_json::Value::String("new_value".to_string()),
        );

        let result = update_config_content(original, &updates);
        assert!(result.contains("KEY1 = \"new_value\""));
        assert!(result.contains("KEY2=keep_this"));
    }

    #[test]
    fn test_update_config_content_case_insensitive() {
        let original = "key1=old_value\n";
        let mut updates = serde_json::Map::new();
        updates.insert(
            "KEY1".to_string(),
            serde_json::Value::String("new_value".to_string()),
        );

        let result = update_config_content(original, &updates);
        assert!(result.contains("KEY1 = \"new_value\""));
    }

    #[test]
    fn test_update_config_content_preserves_comments() {
        let original = "# Comment line\nKEY1=value\n";
        let updates = serde_json::Map::new();

        let result = update_config_content(original, &updates);
        assert!(result.contains("# Comment line"));
    }

    #[test]
    fn test_update_config_content_multiple_updates() {
        let original = "KEY1=old1\nKEY2=old2\nKEY3=old3\n";
        let mut updates = serde_json::Map::new();
        updates.insert(
            "KEY1".to_string(),
            serde_json::Value::String("new1".to_string()),
        );
        updates.insert(
            "KEY3".to_string(),
            serde_json::Value::String("new3".to_string()),
        );

        let result = update_config_content(original, &updates);
        assert!(result.contains("KEY1 = \"new1\""));
        assert!(result.contains("KEY2=old2"));
        assert!(result.contains("KEY3 = \"new3\""));
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
    fn test_resolve_config_file_neither_exists() {
        // When neither file exists, returns the primary path
        let result = resolve_config_file("nonexistent-test-service-xyz");
        assert_eq!(result, "/etc/default/nonexistent-test-service-xyz");
    }

    #[test]
    fn test_resolve_config_file_adds_prefix() {
        // When given "recorder", alternate is "edgefirst-recorder"
        let service = "recorder";
        let alt = if let Some(short) = service.strip_prefix(EDGEFIRST_PREFIX) {
            short.to_string()
        } else {
            format!("{}{}", EDGEFIRST_PREFIX, service)
        };
        assert_eq!(alt, "edgefirst-recorder");
    }

    #[test]
    fn test_resolve_config_file_strips_prefix() {
        // When given "edgefirst-recorder", alternate is "recorder"
        let service = "edgefirst-recorder";
        let alt = if let Some(short) = service.strip_prefix(EDGEFIRST_PREFIX) {
            short.to_string()
        } else {
            format!("{}{}", EDGEFIRST_PREFIX, service)
        };
        assert_eq!(alt, "recorder");
    }
}
