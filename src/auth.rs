// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Authentication handlers for EdgeFirst Studio integration.

use actix_web::{web, HttpResponse, Responder};
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::upload::UploadManager;

/// Authentication request body
#[derive(Deserialize)]
pub struct AuthRequest {
    pub username: String,
    pub password: String,
    /// Server instance: "saas" (default), "stage", "test", or "dev"
    #[serde(default = "default_server")]
    pub server: String,
}

fn default_server() -> String {
    "saas".to_string()
}

/// Authentication response
#[derive(Serialize, Deserialize)]
pub struct AuthResponse {
    pub status: String,
    pub message: String,
}

/// Authentication status response
#[derive(Serialize, Deserialize)]
pub struct AuthStatusResponse {
    pub authenticated: bool,
    pub username: Option<String>,
}

/// Trait for accessing upload manager from server context
pub trait AuthContext {
    fn upload_manager(&self) -> &Arc<UploadManager>;
}

/// POST /api/auth/login - Authenticate with EdgeFirst Studio
pub async fn auth_login<T: AuthContext>(
    body: web::Json<AuthRequest>,
    data: web::Data<T>,
) -> impl Responder {
    info!(
        "Login request received for user: {} on server: {}",
        body.username, body.server
    );

    match data
        .upload_manager()
        .authenticate(&body.username, &body.password, &body.server)
        .await
    {
        Ok(()) => {
            info!(
                "User {} authenticated successfully on {}",
                body.username, body.server
            );
            HttpResponse::Ok().json(AuthResponse {
                status: "ok".to_string(),
                message: "Authentication successful".to_string(),
            })
        }
        Err(e) => {
            // Log full error internally but return generic message to client
            error!("Authentication failed for user {}: {}", body.username, e);
            HttpResponse::Unauthorized().json(AuthResponse {
                status: "error".to_string(),
                message: "Authentication failed. Please check your credentials.".to_string(),
            })
        }
    }
}

/// GET /api/auth/status - Check authentication status
pub async fn auth_status<T: AuthContext>(data: web::Data<T>) -> impl Responder {
    let is_authenticated = data.upload_manager().is_authenticated().await;
    let username = data.upload_manager().get_username().await;

    HttpResponse::Ok().json(AuthStatusResponse {
        authenticated: is_authenticated,
        username,
    })
}

/// POST /api/auth/logout - Logout from EdgeFirst Studio
pub async fn auth_logout<T: AuthContext>(data: web::Data<T>) -> impl Responder {
    match data.upload_manager().logout().await {
        Ok(()) => {
            info!("User logged out successfully");
            HttpResponse::Ok().json(AuthResponse {
                status: "ok".to_string(),
                message: "Logged out successfully".to_string(),
            })
        }
        Err(e) => {
            error!("Logout failed: {}", e);
            HttpResponse::InternalServerError().json(AuthResponse {
                status: "error".to_string(),
                message: format!("Logout failed: {}", e),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_response_structure() {
        let response = AuthResponse {
            status: "ok".to_string(),
            message: "Authentication successful".to_string(),
        };

        let json = serde_json::to_string(&response).expect("Failed to serialize");
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"message\""));
    }

    #[test]
    fn test_auth_status_response_structure() {
        let authenticated_response = AuthStatusResponse {
            authenticated: true,
            username: Some("testuser".to_string()),
        };

        let json = serde_json::to_string(&authenticated_response).expect("Failed to serialize");
        assert!(json.contains("\"authenticated\":true"));
        assert!(json.contains("\"username\":\"testuser\""));

        let unauthenticated_response = AuthStatusResponse {
            authenticated: false,
            username: None,
        };

        let json = serde_json::to_string(&unauthenticated_response).expect("Failed to serialize");
        assert!(json.contains("\"authenticated\":false"));
        assert!(json.contains("\"username\":null"));
    }

    #[test]
    fn test_default_server() {
        assert_eq!(default_server(), "saas");
    }
}
