// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Graceful shutdown coordination for the web server.
//!
//! This module provides coordinated shutdown of all server components:
//! - Signal handling for SIGTERM/SIGINT
//! - Cancellation token for global shutdown propagation
//! - Broadcast channel for WebSocket shutdown notifications

use log::info;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Coordinates graceful shutdown across all server components.
///
/// The coordinator provides:
/// - A `CancellationToken` that can be cloned and checked by any component
/// - A broadcast channel to notify WebSocket connections of shutdown
/// - Methods to trigger and check shutdown state
#[derive(Clone)]
pub struct ShutdownCoordinator {
    /// Cancellation token for global shutdown signal
    token: CancellationToken,
    /// Broadcast sender for WebSocket shutdown notifications
    ws_shutdown_tx: broadcast::Sender<()>,
}

impl ShutdownCoordinator {
    /// Create a new shutdown coordinator.
    pub fn new() -> Self {
        let (ws_shutdown_tx, _) = broadcast::channel(1);
        Self {
            token: CancellationToken::new(),
            ws_shutdown_tx,
        }
    }

    /// Get a clone of the cancellation token for use in async tasks.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Subscribe to WebSocket shutdown notifications.
    pub fn subscribe_ws_shutdown(&self) -> broadcast::Receiver<()> {
        self.ws_shutdown_tx.subscribe()
    }

    /// Trigger graceful shutdown of all components.
    ///
    /// This will:
    /// 1. Cancel the global token (notifies all tasks watching the token)
    /// 2. Send shutdown notification to all WebSocket connections
    pub fn trigger_shutdown(&self) {
        info!("Triggering graceful shutdown");
        self.token.cancel();
        // Send to all subscribers, ignore errors if no receivers
        let _ = self.ws_shutdown_tx.send(());
    }

    /// Check if shutdown has been triggered.
    pub fn is_shutting_down(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Wait for a shutdown signal (SIGTERM or SIGINT).
    ///
    /// This function blocks until one of:
    /// - SIGTERM is received (common for systemd/container shutdown)
    /// - SIGINT is received (Ctrl+C)
    ///
    /// After receiving a signal, it triggers the shutdown coordinator.
    pub async fn wait_for_signal(&self) {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};

            let mut sigterm =
                signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");
            let mut sigint =
                signal(SignalKind::interrupt()).expect("Failed to register SIGINT handler");

            tokio::select! {
                _ = sigterm.recv() => {
                    info!("Received SIGTERM");
                }
                _ = sigint.recv() => {
                    info!("Received SIGINT (Ctrl+C)");
                }
            }
        }

        #[cfg(not(unix))]
        {
            // On non-Unix platforms, only handle Ctrl+C
            tokio::signal::ctrl_c()
                .await
                .expect("Failed to listen for Ctrl+C");
            info!("Received Ctrl+C");
        }

        self.trigger_shutdown();
    }

    /// Wait for the shutdown signal to be triggered.
    ///
    /// This is useful for tasks that want to wait until shutdown is requested.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shutdown_coordinator_creation() {
        let coordinator = ShutdownCoordinator::new();
        assert!(!coordinator.is_shutting_down());
    }

    #[test]
    fn test_shutdown_trigger() {
        let coordinator = ShutdownCoordinator::new();
        assert!(!coordinator.is_shutting_down());

        coordinator.trigger_shutdown();
        assert!(coordinator.is_shutting_down());
    }

    #[test]
    fn test_token_clone() {
        let coordinator = ShutdownCoordinator::new();
        let token = coordinator.token();

        assert!(!token.is_cancelled());
        coordinator.trigger_shutdown();
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_ws_shutdown_subscription() {
        let coordinator = ShutdownCoordinator::new();
        let mut rx = coordinator.subscribe_ws_shutdown();

        coordinator.trigger_shutdown();

        // Should receive the shutdown notification
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn test_cancelled_future() {
        let coordinator = ShutdownCoordinator::new();
        let coordinator_clone = coordinator.clone();

        // Spawn a task that triggers shutdown after a brief delay
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            coordinator_clone.trigger_shutdown();
        });

        // This should complete when shutdown is triggered
        coordinator.cancelled().await;
        assert!(coordinator.is_shutting_down());
    }
}
