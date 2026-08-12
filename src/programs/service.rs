// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! systemd service management for Blockly programs.
//!
//! Supports two modes, controlled by the `--system` flag:
//!
//! - **System mode** (`--system`): Unit files in `/etc/systemd/system/`,
//!   managed via `systemctl`, apps in `/var/lib/edgefirst/programs/`.
//! - **User mode** (default): Unit files in `~/.config/systemd/user/`,
//!   managed via `systemctl --user`, apps in `~/.local/share/edgefirst/programs/`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Service name prefix for all Blockly program units.
const SERVICE_PREFIX: &str = "efapp-";

/// Default programs path for system mode.
pub const SYSTEM_PROGRAMS_PATH: &str = "/var/lib/edgefirst/programs";

/// Default programs path for user mode.
pub const USER_PROGRAMS_PATH: &str = "~/.local/share/edgefirst/programs";

/// Sanitize a string for safe interpolation into a systemd unit file.
/// Strips control characters (newlines, carriage returns, etc.) that could
/// inject arbitrary directives into the unit file.
fn sanitize_unit_value(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

// ============================================================================
// Unit File Management
// ============================================================================

/// Return the unit directory for the given mode.
fn unit_dir(system_mode: bool) -> Result<PathBuf> {
    if system_mode {
        Ok(PathBuf::from("/etc/systemd/system"))
    } else {
        let home = std::env::var("HOME").context("HOME not set")?;
        Ok(PathBuf::from(home).join(".config/systemd/user"))
    }
}

/// Full service name for a program ID: `efapp-{id}.service`.
pub fn service_name(id: &str) -> String {
    format!("{SERVICE_PREFIX}{id}.service")
}

/// Generate and write the systemd unit file for a program.
pub async fn write_unit_file(
    id: &str,
    name: &str,
    work_dir: &Path,
    python_path: &str,
    system_mode: bool,
) -> Result<()> {
    let dir = unit_dir(system_mode)?;
    tokio::fs::create_dir_all(&dir).await?;

    let unit_path = dir.join(service_name(id));
    let service_id = format!("{SERVICE_PREFIX}{id}");
    let safe_name = sanitize_unit_value(name);

    let after = if system_mode {
        "multi-user.target"
    } else {
        "default.target"
    };
    let wanted_by = after;

    let unit_content = format!(
        r#"[Unit]
Description=EdgeFirst App: {name}
After={after}

[Service]
Type=simple
WorkingDirectory={work_dir}
ExecStart={python} app.py
Restart=no
StandardOutput=journal
StandardError=journal
SyslogIdentifier={syslog_id}

[Install]
WantedBy={wanted_by}
"#,
        name = safe_name,
        after = after,
        work_dir = work_dir.display(),
        python = python_path,
        syslog_id = service_id,
        wanted_by = wanted_by,
    );

    tokio::fs::write(&unit_path, unit_content).await?;
    log::info!("Wrote unit file: {}", unit_path.display());
    Ok(())
}

/// Remove the systemd unit file for a program.
pub async fn remove_unit_file(id: &str, system_mode: bool) -> Result<()> {
    // Disable before removing so symlinks are cleaned up
    let _ = disable_service(id, system_mode).await;

    let dir = unit_dir(system_mode)?;
    let unit_path = dir.join(service_name(id));

    if unit_path.exists() {
        tokio::fs::remove_file(&unit_path).await?;
        log::info!("Removed unit file: {}", unit_path.display());
    }

    Ok(())
}

// ============================================================================
// systemctl Commands
// ============================================================================

/// Run a `systemctl` command, optionally with `--user`, and check for success.
async fn systemctl(args: &[&str], system_mode: bool) -> Result<std::process::Output> {
    let mut cmd = Command::new("systemctl");
    if !system_mode {
        cmd.arg("--user");
    }
    cmd.args(args);

    let mode_str = if system_mode { "" } else { " --user" };
    let output = cmd
        .output()
        .await
        .with_context(|| format!("failed to run: systemctl{mode_str} {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("systemctl{mode_str} {} failed: {stderr}", args.join(" "));
    }

    Ok(output)
}

/// Run `systemctl [--user] daemon-reload`.
pub async fn daemon_reload(system_mode: bool) -> Result<()> {
    systemctl(&["daemon-reload"], system_mode).await?;
    Ok(())
}

/// Start a program's systemd service.
pub async fn start_service(id: &str, system_mode: bool) -> Result<()> {
    let name = service_name(id);
    systemctl(&["start", &name], system_mode).await?;
    log::info!("Started service: {name}");
    Ok(())
}

/// Stop a program's systemd service.
pub async fn stop_service(id: &str, system_mode: bool) -> Result<()> {
    let name = service_name(id);
    let mut cmd = Command::new("systemctl");
    if !system_mode {
        cmd.arg("--user");
    }
    cmd.args(["stop", &name]);

    let output = cmd
        .output()
        .await
        .with_context(|| format!("failed to stop {name}"))?;

    if !output.status.success() {
        // Exit code 5 = unit not found/not loaded — acceptable during cleanup
        if output.status.code() != Some(5) {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("failed to stop {name}: {stderr}");
        }
    }

    log::info!("Stopped service: {name}");
    Ok(())
}

/// Enable a program's service so it starts on boot.
pub async fn enable_service(id: &str, system_mode: bool) -> Result<()> {
    let name = service_name(id);
    systemctl(&["enable", &name], system_mode).await?;
    log::info!("Enabled service: {name}");
    Ok(())
}

/// Disable a program's service.
pub async fn disable_service(id: &str, system_mode: bool) -> Result<()> {
    let name = service_name(id);
    let mut cmd = Command::new("systemctl");
    if !system_mode {
        cmd.arg("--user");
    }
    cmd.args(["disable", &name]);

    let output = cmd
        .output()
        .await
        .with_context(|| format!("failed to disable {name}"))?;

    // Ignore failure if the unit doesn't exist
    if !output.status.success() && output.status.code() != Some(5) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to disable {name}: {stderr}");
    }

    log::info!("Disabled service: {name}");
    Ok(())
}

/// Check if a program's service is currently active.
pub async fn is_active(id: &str, system_mode: bool) -> bool {
    let name = service_name(id);
    let mut cmd = Command::new("systemctl");
    if !system_mode {
        cmd.arg("--user");
    }
    cmd.args(["is-active", &name]);

    match cmd.output().await {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim() == "active",
        Err(_) => false,
    }
}

/// Query detailed service status via `systemctl [--user] show`.
pub async fn service_properties(id: &str, system_mode: bool) -> Result<ServiceProperties> {
    let name = service_name(id);
    let mut cmd = Command::new("systemctl");
    if !system_mode {
        cmd.arg("--user");
    }
    cmd.args([
        "show",
        &name,
        "--property=MainPID,ActiveEnterTimestampMonotonic,ActiveState",
    ]);

    let output = cmd
        .output()
        .await
        .context(format!("failed to query {name}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut props = ServiceProperties::default();

    for line in stdout.lines() {
        if let Some(val) = line.strip_prefix("MainPID=") {
            props.main_pid = val.parse().unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("ActiveEnterTimestampMonotonic=") {
            props.active_enter_usec = val.trim().parse().unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("ActiveState=") {
            props.active_state = val.trim().to_string();
        }
    }

    Ok(props)
}

/// Properties returned by `systemctl show`.
#[derive(Debug, Default)]
pub struct ServiceProperties {
    pub main_pid: u32,
    /// Monotonic timestamp in microseconds when the service entered active state.
    pub active_enter_usec: u64,
    pub active_state: String,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_newlines() {
        assert_eq!(
            sanitize_unit_value("App\nExecStartPre=/bin/rm"),
            "AppExecStartPre=/bin/rm"
        );
    }

    #[test]
    fn sanitize_strips_carriage_return() {
        assert_eq!(sanitize_unit_value("App\r\nName"), "AppName");
    }

    #[test]
    fn sanitize_preserves_normal_text() {
        assert_eq!(sanitize_unit_value("My Cool App 2.0"), "My Cool App 2.0");
    }

    #[test]
    fn sanitize_strips_tabs() {
        assert_eq!(sanitize_unit_value("App\tName"), "AppName");
    }

    #[test]
    fn service_name_format() {
        assert_eq!(service_name("zone-alert"), "efapp-zone-alert.service");
    }

    #[test]
    fn unit_dir_system() {
        let dir = unit_dir(true).unwrap();
        assert_eq!(dir, PathBuf::from("/etc/systemd/system"));
    }

    #[test]
    fn unit_dir_user() {
        std::env::set_var("HOME", "/home/test");
        let dir = unit_dir(false).unwrap();
        assert_eq!(dir, PathBuf::from("/home/test/.config/systemd/user"));
    }
}
