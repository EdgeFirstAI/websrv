// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Configuration file reading and service configuration management.

use axum::extract::{Json, Path};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use log::{debug, error, warn};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path as FsPath, PathBuf};
use std::sync::OnceLock;

use crate::envfile::{parse_config_content, plan_edit, Disposition, Reject};

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
    pub exists: bool,
    /// Every candidate examined, in order, for error reporting.
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

/// Check service status and restart if active.
///
/// Reports a restart that actually succeeded, not merely one that was issued:
/// `systemctl restart`'s exit status is inspected, so a unit that fails to
/// come back up after the restart is reported as an error even though the
/// `systemctl` process itself was spawned and ran to completion.
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
        let restart = Command::new("systemctl")
            .arg("restart")
            .arg(&resolved)
            .output()
            .map_err(|e| format!("Error restarting service: {:?}", e))?;

        if !restart.status.success() {
            let stderr = String::from_utf8_lossy(&restart.stderr).trim().to_string();
            let detail = if stderr.is_empty() {
                restart.status.to_string()
            } else {
                stderr
            };
            return Err(format!(
                "Service '{}' failed to restart: {}",
                resolved, detail
            ));
        }
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

/// JSON body returned by [`set_config`] for every outcome.
///
/// Optional members are omitted when empty so a plain successful save stays
/// compact, and the shape is additive for clients that only check the status.
#[derive(Serialize, Default)]
struct ConfigWriteResponse {
    service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    applied: bool,
    restarted: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    dispositions: BTreeMap<String, Disposition>,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    unmatched: BTreeSet<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    reserved: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    rejected: BTreeMap<String, Reject>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tried: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    restart_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ConfigWriteResponse {
    fn new(service: &str) -> Self {
        Self {
            service: service.to_string(),
            ..Default::default()
        }
    }

    fn into_response_with(self, status: StatusCode) -> axum::response::Response {
        (status, Json(self)).into_response()
    }
}

/// Key the webui sends alongside the config values to name the target file.
/// It is stripped rather than rejected: every settings page posts it, so
/// rejecting it would fail every save.
const RESERVED_KEY: &str = "filename";

/// Set service configuration in `{config_dir}/{service}`.
pub async fn set_config(Json(params): Json<Value>) -> impl IntoResponse {
    let Some(params) = params.as_object() else {
        error!("Request body is not a JSON object");
        let mut response = ConfigWriteResponse::new("");
        response.error = Some("request body must be a JSON object".to_string());
        return response.into_response_with(StatusCode::BAD_REQUEST);
    };

    let file_name = match params.get("fileName").map(|v| v.as_str()) {
        Some(Some(name)) => name.to_string(),
        Some(None) => {
            error!("fileName is not a string");
            let mut response = ConfigWriteResponse::new("");
            response.error = Some("fileName must be a string".to_string());
            return response.into_response_with(StatusCode::BAD_REQUEST);
        }
        None => {
            error!("fileName not found in JSON");
            let mut response = ConfigWriteResponse::new("");
            response.error = Some("missing fileName".to_string());
            return response.into_response_with(StatusCode::BAD_REQUEST);
        }
    };

    // Validate fileName to prevent path traversal.
    //
    // The alphanumeric/`-`/`_` whitelist already rejects every character in
    // "..", so the `/` and ".." checks are redundant today. They stay as
    // defence-in-depth: they become load-bearing the moment the whitelist is
    // widened (e.g. to allow `.` so names like `foo.conf` work).
    let safe = !file_name.contains('/')
        && !file_name.contains("..")
        && file_name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
    if !safe {
        error!("Invalid fileName: {:?}", file_name);
        let mut response = ConfigWriteResponse::new(&file_name);
        response.error = Some("invalid fileName".to_string());
        return response.into_response_with(StatusCode::BAD_REQUEST);
    }

    let resolved = resolve_config(&file_name);
    if !resolved.exists {
        error!("No configuration file for service {:?}", file_name);
        let mut response = ConfigWriteResponse::new(&file_name);
        response.error = Some("no config file".to_string());
        response.tried = resolved.tried;
        return response.into_response_with(StatusCode::NOT_FOUND);
    }
    let path = resolved.path;
    debug!("Configuration file path: {:?}", path);

    let original = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) => {
            error!("Error reading configuration file {:?}: {:?}", path, e);
            let mut response = ConfigWriteResponse::new(&file_name);
            response.path = Some(path.to_string_lossy().into_owned());
            response.error = Some("error reading configuration file".to_string());
            return response.into_response_with(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    // Strip the reserved key; it names the file rather than a setting.
    let mut reserved = Vec::new();
    let mut updates = serde_json::Map::new();
    for (key, value) in params {
        if key.eq_ignore_ascii_case(RESERVED_KEY) {
            reserved.push(key.clone());
        } else {
            updates.insert(key.clone(), value.clone());
        }
    }

    let mut response = ConfigWriteResponse::new(&file_name);
    response.path = Some(path.to_string_lossy().into_owned());
    response.reserved = reserved;

    let plan = match plan_edit(&original, &updates) {
        Ok(plan) => plan,
        Err(rejected) => {
            error!("Rejected configuration keys: {:?}", rejected);
            response.rejected = rejected;
            return response.into_response_with(StatusCode::BAD_REQUEST);
        }
    };

    if !plan.changed {
        debug!("No configuration change for {:?}", file_name);
        response.dispositions = plan.dispositions;
        response.unmatched = plan.unmatched;
        response.reason = Some("no changes".to_string());
        return response.into_response_with(StatusCode::OK);
    }

    if let Err(e) = write_atomic(&path, &plan.content) {
        error!("Error saving configuration {:?}: {:?}", path, e);
        response.error = Some("error saving configuration".to_string());
        return response.into_response_with(StatusCode::INTERNAL_SERVER_ERROR);
    }
    response.applied = true;
    response.dispositions = plan.dispositions;
    response.unmatched = plan.unmatched;

    for key in &response.unmatched {
        warn!(
            "Key {:?} was not present in {:?} and has been appended; \
             it may not be a setting this service reads",
            key, path
        );
    }

    match check_service_status(&file_name).await {
        Ok(message) => {
            debug!("{}", message);
            response.restarted = message.contains("restarted successfully");
        }
        Err(e) => {
            // The configuration was applied, so this is not a server error.
            error!("{}", e);
            response.restart_error = Some(e);
        }
    }

    response.into_response_with(StatusCode::OK)
}

/// Write `content` to `path` atomically.
///
/// Writes a sibling temp file, syncs it, copies the original file's mode onto
/// it, then renames over the target. Same-directory rename is atomic on Linux,
/// so a reader never sees a partial file and a crash leaves the original
/// intact — which matters because a truncated `/etc/default` file stops the
/// service from starting at all.
///
/// Ownership is deliberately not copied: websrv must already run as root to
/// write `/etc/default` and to restart units, so the renamed file lands
/// root-owned exactly as the original was.
fn write_atomic(path: &FsPath, content: &str) -> io::Result<()> {
    use std::io::Write;

    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration path has no parent directory",
        )
    })?;
    // Fails if the target does not exist, which is what we want: this function
    // replaces a config file, it does not create one.
    let permissions = std::fs::metadata(path)?.permissions();

    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(content.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.as_file().set_permissions(permissions)?;
    temp.persist(path).map_err(|e| e.error)?;

    Ok(())
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

    // ========================================================================
    // Atomic write tests
    // ========================================================================

    #[test]
    fn write_atomic_replaces_content_and_preserves_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("svc");
        std::fs::write(&path, "old\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        write_atomic(&path, "new\n").expect("write");

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "new\n");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was not preserved");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "svc")
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
    }

    #[test]
    fn write_atomic_fails_when_the_target_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = write_atomic(&dir.path().join("absent"), "x\n");
        assert!(
            result.is_err(),
            "should not create a file that does not exist"
        );
    }
}
