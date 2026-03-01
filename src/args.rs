// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use clap::Parser;
use serde::Serialize;
use serde_json::json;
use zenoh::config::{Config, WhatAmI};

pub type Boolean = bool; //Need this to be able to set bool to true/false in the cmd line

/// Command-line arguments for EdgeFirst Web UI Server.
///
/// This structure defines all configuration options for the web server,
/// including document root, Zenoh topic subscriptions, SSL/TLS settings,
/// and networking options. Arguments can be specified via command line or
/// environment variables.
///
/// # Example
///
/// ```bash
/// # Via command line
/// edgefirst-websrv --docroot /usr/share/edgefirst/webui --http-port 8080
///
/// # Via environment variables
/// export DOCROOT=/usr/share/edgefirst/webui
/// export HTTP_PORT=8080
/// edgefirst-websrv
/// ```
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// The web document root for serving html pages
    #[arg(
        short,
        long,
        env = "DOCROOT",
        default_value = "/usr/share/edgefirst/webui"
    )]
    pub docroot: String,

    /// Run the application in system mode
    #[arg(long, env = "SYSTEM")]
    pub system: bool,

    /// Zenoh participant mode (peer, client, or router)
    #[arg(long, env = "MODE", default_value = "peer")]
    mode: WhatAmI,

    /// Zenoh endpoints to connect to (can specify multiple)
    #[arg(long, env = "CONNECT")]
    connect: Vec<String>,

    /// Zenoh endpoints to listen on (can specify multiple)
    #[arg(long, env = "LISTEN")]
    listen: Vec<String>,

    /// Disable Zenoh multicast peer discovery
    #[arg(long, env = "NO_MULTICAST_SCOUTING")]
    no_multicast_scouting: bool,

    /// Zenoh topic for segmentation mask overlay
    #[arg(
        long,
        env = "MASK",
        default_value = "/rt/model/mask_compressed",
        conflicts_with = "system"
    )]
    mask: String,

    /// Zenoh topic for 2D object detection bounding boxes
    #[arg(
        long,
        env = "DETECT",
        default_value = "/rt/model/boxes2d",
        conflicts_with = "system"
    )]
    detect: String,

    /// Zenoh topic for H.264 video stream
    #[arg(
        long,
        env = "H264",
        default_value = "/rt/camera/h264",
        conflicts_with = "system"
    )]
    h264: String,

    /// Draw bounding boxes on the video overlay
    #[arg(long, default_value = "true", conflicts_with = "system")]
    draw_box: Boolean,

    /// Draw class labels on bounding boxes
    #[arg(long, default_value = "true", conflicts_with = "system")]
    draw_labels: Boolean,

    /// Mirror the video display horizontally
    #[arg(long, default_value = "true", conflicts_with = "system")]
    mirror: Boolean,

    /// Path for recording storage
    #[arg(long, default_value = ".", conflicts_with = "system")]
    pub storage_path: String,

    /// HTTP port (default: 80, use 8080 for non-root)
    #[arg(long, env = "HTTP_PORT", default_value = "80")]
    pub http_port: u16,

    /// HTTPS port (default: 443, use 8443 for non-root)
    #[arg(long, env = "HTTPS_PORT", default_value = "443")]
    pub https_port: u16,

    /// SSL certificate directory
    #[arg(long, env = "CERT_DIR", default_value = "/etc/edgefirst/ssl")]
    pub cert_dir: PathBuf,

    /// SSL certificate file (overrides cert_dir)
    #[arg(long, env = "CERT")]
    pub cert: Option<PathBuf>,

    /// SSL private key file (overrides cert_dir)
    #[arg(long, env = "KEY")]
    pub key: Option<PathBuf>,

    /// Force regeneration of self-signed certificate
    #[arg(long)]
    pub generate_cert: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub struct WebUISettings {
    mask_topic: String,
    detect_topic: String,
    h264_topic: String,
    draw_box: bool,
    draw_box_text: bool,
    mirror: bool,
}

impl From<Args> for WebUISettings {
    fn from(value: Args) -> Self {
        Self {
            mask_topic: value.mask,
            detect_topic: value.detect,
            h264_topic: value.h264,
            draw_box: value.draw_box,
            draw_box_text: value.draw_labels,
            mirror: value.mirror,
        }
    }
}

impl From<Args> for Config {
    fn from(args: Args) -> Self {
        let mut config = Config::default();

        config
            .insert_json5("mode", &json!(args.mode).to_string())
            .unwrap();

        let connect: Vec<_> = args.connect.into_iter().filter(|s| !s.is_empty()).collect();
        if !connect.is_empty() {
            config
                .insert_json5("connect/endpoints", &json!(connect).to_string())
                .unwrap();
        }

        let listen: Vec<_> = args.listen.into_iter().filter(|s| !s.is_empty()).collect();
        if !listen.is_empty() {
            config
                .insert_json5("listen/endpoints", &json!(listen).to_string())
                .unwrap();
        }

        if args.no_multicast_scouting {
            config
                .insert_json5("scouting/multicast/enabled", &json!(false).to_string())
                .unwrap();
        }

        config
            .insert_json5("scouting/multicast/interface", &json!("lo").to_string())
            .unwrap();

        config
    }
}
