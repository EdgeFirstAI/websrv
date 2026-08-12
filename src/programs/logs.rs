// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Log capture and ring-buffer streaming for Blockly programs.
//!
//! Each running program gets a `journalctl --user-unit` follower task
//! that captures stdout/stderr and broadcasts lines to connected
//! WebSocket clients.

use std::collections::VecDeque;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{broadcast, RwLock};
use tokio::task::JoinHandle;

/// Maximum number of log lines retained in the ring buffer.
const LOG_BUFFER_CAPACITY: usize = 1000;

/// Broadcast channel capacity for log lines.
const LOG_CHANNEL_CAPACITY: usize = 256;

// ============================================================================
// Log Buffer
// ============================================================================

/// Ring buffer storing recent log lines for a program.
pub struct LogBuffer {
    lines: VecDeque<String>,
    capacity: usize,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            lines: VecDeque::with_capacity(LOG_BUFFER_CAPACITY),
            capacity: LOG_BUFFER_CAPACITY,
        }
    }

    /// Push a new line, evicting the oldest if at capacity.
    pub fn push(&mut self, line: String) {
        if self.lines.len() >= self.capacity {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// Return the last `n` lines.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let start = self.lines.len().saturating_sub(n);
        self.lines.iter().skip(start).cloned().collect()
    }
}

// ============================================================================
// Program Runtime
// ============================================================================

/// Runtime state for a running program (in-memory only).
pub struct ProgramRuntime {
    pub log_tx: broadcast::Sender<String>,
    pub log_buffer: Arc<RwLock<LogBuffer>>,
    pub journal_task: Option<JoinHandle<()>>,
}

impl Default for ProgramRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgramRuntime {
    /// Create a new runtime with an empty log buffer.
    pub fn new() -> Self {
        let (log_tx, _) = broadcast::channel(LOG_CHANNEL_CAPACITY);
        Self {
            log_tx,
            log_buffer: Arc::new(RwLock::new(LogBuffer::new())),
            journal_task: None,
        }
    }

    /// Subscribe to the log broadcast channel.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.log_tx.subscribe()
    }
}

// ============================================================================
// Journal Follower
// ============================================================================

/// Spawn a `journalctl` follower task that captures output from a program's
/// systemd service and broadcasts it.
///
/// In user mode, uses `journalctl --user --user-unit=...`.
/// In system mode, uses `journalctl --unit=...`.
pub fn spawn_log_follower(
    id: &str,
    log_tx: broadcast::Sender<String>,
    log_buffer: Arc<RwLock<LogBuffer>>,
    system_mode: bool,
) -> JoinHandle<()> {
    let unit = super::service::service_name(id);
    let id = id.to_string();

    tokio::spawn(async move {
        let mut cmd = tokio::process::Command::new("journalctl");
        if system_mode {
            cmd.args(["--unit", &unit]);
        } else {
            cmd.args(["--user", "--user-unit", &unit]);
        }
        cmd.args(["-f", "-n", "0", "--output=short-iso"]);

        let child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn();

        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                log::error!("Failed to spawn journalctl for {id}: {e}");
                return;
            }
        };

        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                log::error!("No stdout from journalctl for {id}");
                return;
            }
        };

        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let log_entry = format_log_line(&line);

            // Store in ring buffer
            log_buffer.write().await.push(log_entry.clone());

            // Broadcast to WebSocket clients (ignore if no receivers)
            let _ = log_tx.send(log_entry);
        }

        // Clean up child process
        let _ = child.kill().await;
        log::debug!("Journal follower exited for {id}");
    })
}

/// Format a raw journalctl line into a JSON log message.
///
/// Input format (short-iso): `2026-03-21T15:45:01-0600 hostname efapp-zone-alert[123]: message text`
/// Output: `{"timestamp":"...","level":"info","message":"..."}`
fn format_log_line(line: &str) -> String {
    // Try to parse the short-iso format; if it fails, treat the whole line as the message
    let (timestamp, message) = parse_journal_line(line);

    // Infer log level from message content
    let level = infer_log_level(&message);

    serde_json::json!({
        "timestamp": timestamp,
        "level": level,
        "message": message,
    })
    .to_string()
}

/// Parse a journalctl short-iso line into (timestamp, message).
fn parse_journal_line(line: &str) -> (String, String) {
    // short-iso format: "2026-03-21T15:45:01-0600 hostname unit[pid]: message"
    // Try to find the message after the unit identifier
    if let Some(colon_pos) = line.find("]: ") {
        let message = line[colon_pos + 3..].to_string();

        // Extract timestamp (first space-separated token)
        let timestamp = line.split_whitespace().next().unwrap_or("").to_string();

        return (timestamp, message);
    }

    // Fallback: use current time and the whole line
    (chrono::Utc::now().to_rfc3339(), line.to_string())
}

/// Infer the log level from message content.
fn infer_log_level(message: &str) -> &'static str {
    let lower = message.to_lowercase();
    if lower.contains("error") || lower.contains("traceback") || lower.contains("exception") {
        "error"
    } else if lower.contains("warn") {
        "warn"
    } else if lower.contains("debug") {
        "debug"
    } else {
        "info"
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_buffer_push_and_tail() {
        let mut buf = LogBuffer::new();
        for i in 0..10 {
            buf.push(format!("line {i}"));
        }
        let tail = buf.tail(3);
        assert_eq!(tail, vec!["line 7", "line 8", "line 9"]);
    }

    #[test]
    fn log_buffer_capacity() {
        let mut buf = LogBuffer {
            lines: VecDeque::new(),
            capacity: 5,
        };
        for i in 0..10 {
            buf.push(format!("line {i}"));
        }
        assert_eq!(buf.lines.len(), 5);
        assert_eq!(
            buf.tail(5),
            vec!["line 5", "line 6", "line 7", "line 8", "line 9"]
        );
    }

    #[test]
    fn log_buffer_tail_more_than_available() {
        let mut buf = LogBuffer::new();
        buf.push("only one".to_string());
        let tail = buf.tail(100);
        assert_eq!(tail, vec!["only one"]);
    }

    #[test]
    fn format_journal_line() {
        let line =
            "2026-03-21T15:45:01-0600 maivin efapp-test[123]: Subscribed to rt/model/boxes2d";
        let json = format_log_line(line);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["level"], "info");
        assert_eq!(parsed["message"], "Subscribed to rt/model/boxes2d");
        assert_eq!(parsed["timestamp"], "2026-03-21T15:45:01-0600");
    }

    #[test]
    fn format_error_line() {
        let line = "2026-03-21T15:45:05-0600 maivin efapp-test[123]: ERROR: Connection failed";
        let json = format_log_line(line);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["level"], "error");
    }

    #[test]
    fn format_plain_line() {
        let line = "some plain text without journal format";
        let json = format_log_line(line);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["message"], line);
        assert_eq!(parsed["level"], "info");
    }

    #[test]
    fn infer_levels() {
        assert_eq!(infer_log_level("ERROR: something broke"), "error");
        assert_eq!(infer_log_level("WARNING: low memory"), "warn");
        assert_eq!(infer_log_level("DEBUG: entering loop"), "debug");
        assert_eq!(infer_log_level("Started processing"), "info");
        assert_eq!(
            infer_log_level("Traceback (most recent call last):"),
            "error"
        );
    }
}
