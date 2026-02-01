// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Configuration file reading and service configuration management.

use actix_web::{web, HttpResponse, Responder};
use log::{debug, error};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;

/// Upload credentials for DVE integration
#[derive(Serialize, Debug)]
pub struct UploadCredentials {
    pub url: String,
    pub jwt: String,
    pub topic: String,
}

/// Read uploader credentials from /etc/default/uploader
pub fn read_uploader_credentials() -> io::Result<UploadCredentials> {
    let file_path = "/etc/default/uploader";
    let content = std::fs::read_to_string(file_path)?;
    let credentials = parse_uploader_credentials(&content)?;
    debug!("Topic = {:?}", credentials.topic);
    Ok(credentials)
}

/// Read storage directory from /etc/default/recorder
pub fn read_storage_directory() -> io::Result<String> {
    let file_path = "/etc/default/recorder";
    let content = std::fs::read_to_string(file_path)?;
    let storage_dir = parse_storage_directory(&content)?;
    debug!("MCAP Directory: {:?}", storage_dir);
    Ok(storage_dir)
}

/// Service configuration path parameter
#[derive(Deserialize)]
pub struct ConfigPath {
    pub service: String,
}

/// Handler to get upload credentials
pub async fn get_upload_credentials() -> impl Responder {
    match read_uploader_credentials() {
        Ok(credentials) => HttpResponse::Ok().json(credentials),
        Err(err) => {
            let error_message = err.to_string();
            error!("{:?}", error_message);
            HttpResponse::BadRequest().body(error_message)
        }
    }
}

/// Get service configuration from /etc/default/{service}
pub async fn get_config(path: web::Path<ConfigPath>) -> impl Responder {
    let service_name = &path.service;
    let config_file_path = format!("/etc/default/{}", service_name);

    let config_content = std::fs::read_to_string(&config_file_path).unwrap_or_default();
    let config_map = parse_config_content(&config_content);

    HttpResponse::Ok().json(serde_json::Value::Object(config_map))
}

/// Check service status and restart if active
pub async fn check_service_status(service_name: &str) -> Result<String, String> {
    use std::process::Command;

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

// ============================================================================
// Internal parsing functions (extracted for testability)
// ============================================================================

/// Parse uploader credentials from config file content
pub fn parse_uploader_credentials(content: &str) -> io::Result<UploadCredentials> {
    let mut url = String::new();
    let mut jwt = String::new();
    let mut topic = String::new();

    for line in content.lines() {
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

    Ok(UploadCredentials { url, jwt, topic })
}

/// Parse storage directory from config file content
pub fn parse_storage_directory(content: &str) -> io::Result<String> {
    for line in content.lines() {
        if line.starts_with("STORAGE") {
            let parts: Vec<&str> = line.split('=').collect();
            if parts.len() == 2 {
                return Ok(parts[1].trim().trim_matches('"').to_string());
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
pub async fn set_config(params: web::Json<Value>) -> impl Responder {
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

    let updated_config = update_config_content(&config_content, &config_map);

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

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // UploadCredentials parsing tests
    // ========================================================================

    #[test]
    fn test_parse_uploader_credentials_valid() {
        let content = r#"
URL=https://studio.example.com/api
JWT=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.test
TOPIC=my/upload/topic
"#;
        let result = parse_uploader_credentials(content);
        assert!(result.is_ok());
        let creds = result.unwrap();
        assert_eq!(creds.url, "https://studio.example.com/api");
        assert_eq!(creds.jwt, "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.test");
        assert_eq!(creds.topic, "my/upload/topic");
    }

    #[test]
    fn test_parse_uploader_credentials_with_quotes() {
        let content = r#"
URL="https://studio.example.com/api"
JWT="my-jwt-token"
TOPIC="uploads/data"
"#;
        let result = parse_uploader_credentials(content);
        assert!(result.is_ok());
        let creds = result.unwrap();
        assert_eq!(creds.url, "https://studio.example.com/api");
        assert_eq!(creds.jwt, "my-jwt-token");
        assert_eq!(creds.topic, "uploads/data");
    }

    #[test]
    fn test_parse_uploader_credentials_missing_url() {
        let content = r#"
JWT=token
TOPIC=topic
"#;
        let result = parse_uploader_credentials(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("URL"));
    }

    #[test]
    fn test_parse_uploader_credentials_missing_jwt() {
        let content = r#"
URL=https://example.com
TOPIC=topic
"#;
        let result = parse_uploader_credentials(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("JWT"));
    }

    #[test]
    fn test_parse_uploader_credentials_missing_topic() {
        let content = r#"
URL=https://example.com
JWT=token
"#;
        let result = parse_uploader_credentials(content);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("TOPIC"));
    }

    #[test]
    fn test_parse_uploader_credentials_empty_content() {
        let content = "";
        let result = parse_uploader_credentials(content);
        assert!(result.is_err());
    }

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
    // Struct serialization tests
    // ========================================================================

    #[test]
    fn test_upload_credentials_serialization() {
        let creds = UploadCredentials {
            url: "https://example.com".to_string(),
            jwt: "token123".to_string(),
            topic: "my/topic".to_string(),
        };

        let json = serde_json::to_string(&creds).expect("Failed to serialize");
        assert!(json.contains("\"url\":\"https://example.com\""));
        assert!(json.contains("\"jwt\":\"token123\""));
        assert!(json.contains("\"topic\":\"my/topic\""));
    }

    #[test]
    fn test_config_path_deserialization() {
        let json = r#"{"service": "recorder"}"#;
        let path: ConfigPath = serde_json::from_str(json).expect("Failed to deserialize");
        assert_eq!(path.service, "recorder");
    }
}
