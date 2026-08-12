// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! `.efapp` bundle format — ZIP reading, validation, and config types.
//!
//! An `.efapp` file is a standard ZIP archive containing:
//! - `config.json`    — application metadata (required)
//! - `workspace.json` — Blockly workspace state (required)
//! - `app.py`         — generated Python application (required)
//! - `icon.png`       — application icon, 256×256 (optional)

use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read};

/// Maximum bundle size in bytes (10 MB).
pub const MAX_BUNDLE_SIZE: usize = 10 * 1024 * 1024;

/// Maximum total decompressed size (50 MB) — ZIP bomb protection.
const MAX_DECOMPRESSED_SIZE: u64 = 50 * 1024 * 1024;

/// Maximum number of entries in the ZIP archive.
const MAX_FILE_COUNT: usize = 20;

/// The only bundle format version we support.
const SUPPORTED_VERSION: u32 = 1;

// ============================================================================
// Config Types
// ============================================================================

/// `config.json` schema from an `.efapp` bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleConfig {
    pub version: u32,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub author: String,
    pub created: String,
    pub modified: String,
    pub edgefirst_blockly_version: String,
    #[serde(default)]
    pub python: PythonConfig,
}

/// Python runtime configuration within `config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonConfig {
    #[serde(default = "default_entry")]
    pub entry: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
}

impl Default for PythonConfig {
    fn default() -> Self {
        Self {
            entry: default_entry(),
            dependencies: Vec::new(),
        }
    }
}

fn default_entry() -> String {
    "app.py".to_string()
}

// ============================================================================
// Validated Bundle
// ============================================================================

/// A fully validated bundle ready for deployment.
#[derive(Debug)]
pub struct ValidatedBundle {
    pub config: BundleConfig,
    pub workspace_json: Vec<u8>,
    pub app_py: Vec<u8>,
    pub icon_png: Option<Vec<u8>>,
    /// Original ZIP bytes, preserved so `GET /api/programs/:id` can return them.
    pub raw_zip: Vec<u8>,
}

// ============================================================================
// Validation Errors
// ============================================================================

/// Errors that can occur during bundle validation.
#[derive(Debug)]
pub enum BundleError {
    /// ZIP archive could not be read.
    InvalidZip(String),
    /// A required file is missing from the archive.
    MissingFile(&'static str),
    /// `config.json` could not be parsed.
    InvalidConfig(String),
    /// Bundle format version is higher than we support.
    UnsupportedVersion(u32),
    /// `app.py` is empty.
    EmptyAppPy,
    /// `workspace.json` is not valid JSON.
    InvalidWorkspace(String),
    /// Bundle exceeds size limits.
    TooLarge(String),
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidZip(e) => write!(f, "Invalid ZIP archive: {e}"),
            Self::MissingFile(name) => write!(f, "Missing required file: {name}"),
            Self::InvalidConfig(e) => write!(f, "Invalid config.json: {e}"),
            Self::UnsupportedVersion(v) => {
                write!(
                    f,
                    "Unsupported bundle version {v} (server supports up to {SUPPORTED_VERSION})"
                )
            }
            Self::EmptyAppPy => write!(f, "app.py is empty"),
            Self::InvalidWorkspace(e) => write!(f, "Invalid workspace.json: {e}"),
            Self::TooLarge(e) => write!(f, "Bundle too large: {e}"),
        }
    }
}

impl BundleError {
    /// Return the error code for the REST API response.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedVersion(_) => "UNSUPPORTED_BUNDLE_VERSION",
            Self::TooLarge(_) => "BUNDLE_TOO_LARGE",
            _ => "INVALID_BUNDLE",
        }
    }
}

// ============================================================================
// Validation
// ============================================================================

/// Validate raw ZIP bytes and extract a [`ValidatedBundle`].
///
/// This function runs synchronously (ZIP reading is CPU-bound) and should be
/// called inside `tokio::task::spawn_blocking`.
pub fn validate_bundle(data: Vec<u8>) -> Result<ValidatedBundle, BundleError> {
    let cursor = Cursor::new(&data);
    let mut archive =
        zip::ZipArchive::new(cursor).map_err(|e| BundleError::InvalidZip(e.to_string()))?;

    // ZIP bomb protection
    if archive.len() > MAX_FILE_COUNT {
        return Err(BundleError::TooLarge(format!(
            "archive contains {} entries (max {MAX_FILE_COUNT})",
            archive.len()
        )));
    }

    let mut total_size: u64 = 0;
    for i in 0..archive.len() {
        if let Ok(file) = archive.by_index(i) {
            total_size += file.size();
        }
    }
    if total_size > MAX_DECOMPRESSED_SIZE {
        return Err(BundleError::TooLarge(format!(
            "decompressed size {total_size} bytes exceeds limit of {MAX_DECOMPRESSED_SIZE} bytes"
        )));
    }

    // Extract required files
    let config_bytes = read_entry(&mut archive, "config.json")?;
    let workspace_json = read_entry(&mut archive, "workspace.json")?;
    let app_py = read_entry(&mut archive, "app.py")?;

    // Optional icon
    let icon_png = read_entry(&mut archive, "icon.png").ok();

    // Validate config.json
    let config: BundleConfig = serde_json::from_slice(&config_bytes)
        .map_err(|e| BundleError::InvalidConfig(e.to_string()))?;

    if config.version > SUPPORTED_VERSION {
        return Err(BundleError::UnsupportedVersion(config.version));
    }

    if config.name.is_empty() {
        return Err(BundleError::InvalidConfig("name is empty".into()));
    }

    if config.description.is_empty() {
        return Err(BundleError::InvalidConfig("description is empty".into()));
    }

    if config.created.is_empty() {
        return Err(BundleError::InvalidConfig("created is empty".into()));
    }

    if config.modified.is_empty() {
        return Err(BundleError::InvalidConfig("modified is empty".into()));
    }

    if config.edgefirst_blockly_version.is_empty() {
        return Err(BundleError::InvalidConfig(
            "edgefirst_blockly_version is empty".into(),
        ));
    }

    // Validate workspace.json is valid JSON
    serde_json::from_slice::<serde_json::Value>(&workspace_json)
        .map_err(|e| BundleError::InvalidWorkspace(e.to_string()))?;

    // Validate app.py is non-empty
    if app_py.is_empty() {
        return Err(BundleError::EmptyAppPy);
    }

    Ok(ValidatedBundle {
        config,
        workspace_json,
        app_py,
        icon_png,
        raw_zip: data,
    })
}

/// Read a single entry from the ZIP archive by name.
fn read_entry(
    archive: &mut zip::ZipArchive<Cursor<&Vec<u8>>>,
    name: &'static str,
) -> Result<Vec<u8>, BundleError> {
    let mut file = archive
        .by_name(name)
        .map_err(|_| BundleError::MissingFile(name))?;
    let mut buf = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut buf)
        .map_err(|e| BundleError::InvalidZip(format!("failed to read {name}: {e}")))?;
    Ok(buf)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper to build a minimal valid .efapp ZIP in memory.
    fn make_efapp(
        config_json: Option<&str>,
        workspace_json: Option<&str>,
        app_py: Option<&str>,
        icon_png: Option<&[u8]>,
    ) -> Vec<u8> {
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut zip = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default();

        if let Some(config) = config_json {
            zip.start_file("config.json", options).unwrap();
            zip.write_all(config.as_bytes()).unwrap();
        }

        if let Some(ws) = workspace_json {
            zip.start_file("workspace.json", options).unwrap();
            zip.write_all(ws.as_bytes()).unwrap();
        }

        if let Some(py) = app_py {
            zip.start_file("app.py", options).unwrap();
            zip.write_all(py.as_bytes()).unwrap();
        }

        if let Some(icon) = icon_png {
            zip.start_file("icon.png", options).unwrap();
            zip.write_all(icon).unwrap();
        }

        zip.finish().unwrap().into_inner()
    }

    fn valid_config() -> String {
        serde_json::json!({
            "version": 1,
            "name": "Test App",
            "description": "A test application",
            "created": "2026-03-21T14:30:00Z",
            "modified": "2026-03-21T15:45:00Z",
            "edgefirst_blockly_version": "0.1.0",
            "python": { "entry": "app.py" }
        })
        .to_string()
    }

    #[test]
    fn valid_bundle() {
        let data = make_efapp(
            Some(&valid_config()),
            Some(r#"{"blocks":{}}"#),
            Some("print('hello')"),
            None,
        );
        let bundle = validate_bundle(data).unwrap();
        assert_eq!(bundle.config.name, "Test App");
        assert_eq!(bundle.config.version, 1);
        assert!(bundle.icon_png.is_none());
    }

    #[test]
    fn valid_bundle_with_icon() {
        let fake_png = b"\x89PNG\r\n\x1a\nfake";
        let data = make_efapp(
            Some(&valid_config()),
            Some(r#"{"blocks":{}}"#),
            Some("print('hello')"),
            Some(fake_png),
        );
        let bundle = validate_bundle(data).unwrap();
        assert!(bundle.icon_png.is_some());
    }

    #[test]
    fn missing_config() {
        let data = make_efapp(None, Some(r#"{"blocks":{}}"#), Some("print('hello')"), None);
        let err = validate_bundle(data).unwrap_err();
        assert!(matches!(err, BundleError::MissingFile("config.json")));
    }

    #[test]
    fn missing_workspace() {
        let data = make_efapp(Some(&valid_config()), None, Some("print('hello')"), None);
        let err = validate_bundle(data).unwrap_err();
        assert!(matches!(err, BundleError::MissingFile("workspace.json")));
    }

    #[test]
    fn missing_app_py() {
        let data = make_efapp(Some(&valid_config()), Some(r#"{"blocks":{}}"#), None, None);
        let err = validate_bundle(data).unwrap_err();
        assert!(matches!(err, BundleError::MissingFile("app.py")));
    }

    #[test]
    fn empty_app_py() {
        let data = make_efapp(
            Some(&valid_config()),
            Some(r#"{"blocks":{}}"#),
            Some(""),
            None,
        );
        let err = validate_bundle(data).unwrap_err();
        assert!(matches!(err, BundleError::EmptyAppPy));
    }

    #[test]
    fn unsupported_version() {
        let config = serde_json::json!({
            "version": 99,
            "name": "Future App",
            "description": "A future app",
            "created": "2026-03-21T14:30:00Z",
            "modified": "2026-03-21T15:45:00Z",
            "edgefirst_blockly_version": "99.0.0",
        })
        .to_string();
        let data = make_efapp(
            Some(&config),
            Some(r#"{"blocks":{}}"#),
            Some("print('hello')"),
            None,
        );
        let err = validate_bundle(data).unwrap_err();
        assert!(matches!(err, BundleError::UnsupportedVersion(99)));
        assert_eq!(err.code(), "UNSUPPORTED_BUNDLE_VERSION");
    }

    #[test]
    fn invalid_workspace_json() {
        let data = make_efapp(
            Some(&valid_config()),
            Some("not valid json {{{"),
            Some("print('hello')"),
            None,
        );
        let err = validate_bundle(data).unwrap_err();
        assert!(matches!(err, BundleError::InvalidWorkspace(_)));
    }

    #[test]
    fn empty_name_rejected() {
        let config = serde_json::json!({
            "version": 1,
            "name": "",
            "created": "2026-03-21T14:30:00Z",
            "modified": "2026-03-21T15:45:00Z",
        })
        .to_string();
        let data = make_efapp(
            Some(&config),
            Some(r#"{"blocks":{}}"#),
            Some("print('hello')"),
            None,
        );
        let err = validate_bundle(data).unwrap_err();
        assert!(matches!(err, BundleError::InvalidConfig(_)));
    }

    #[test]
    fn invalid_zip() {
        let err = validate_bundle(b"not a zip file".to_vec()).unwrap_err();
        assert!(matches!(err, BundleError::InvalidZip(_)));
    }
}
