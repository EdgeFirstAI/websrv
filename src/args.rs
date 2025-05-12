use clap::Parser;
use serde::Serialize;
use serde_json::json;
use zenoh::config::{Config, WhatAmI};

type Boolean = bool;

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// The web document root for serving html pages.
    #[arg(short, long, env, default_value = "/usr/share/webui")]
    pub docroot: String,

    /// Run the applitation in user mode
    #[arg(long, env)]
    pub user_mode: bool,

    /// zenoh connection mode
    #[arg(long, env, default_value = "peer")]
    mode: WhatAmI,

    /// connect to zenoh endpoints
    #[arg(long, env)]
    connect: Vec<String>,

    /// listen to zenoh endpoints
    #[arg(long, env)]
    listen: Vec<String>,

    /// disable zenoh multicast scouting
    #[arg(long, env)]
    no_multicast_scouting: bool,

    #[arg(
        long,
        env,
        default_value = "/rt/model/mask_compressed",
        requires = "user_mode"
    )]
    mask_topic: String,

    #[arg(
        long,
        env,
        default_value = "/rt/model/boxes2d/",
        requires = "user_mode"
    )]
    detect_topic: String,

    #[arg(long, env, default_value = "/rt/camera/h264/", requires = "user_mode")]
    h264_topic: String,

    #[arg(long, env, default_value = "true", requires = "user_mode")]
    draw_box: Boolean,

    #[arg(long, env, default_value = "true", requires = "user_mode")]
    draw_box_text: Boolean,

    #[arg(long, env, default_value = "true", requires = "user_mode")]
    mirror: Boolean,

    #[arg(
        long,
        env,
        default_value = "/home/root/recordings",
        requires = "user_mode"
    )]
    pub storage_path: String,
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
            mask_topic: value.mask_topic,
            detect_topic: value.detect_topic,
            h264_topic: value.h264_topic,
            draw_box: value.draw_box,
            draw_box_text: value.draw_box_text,
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

        if !args.connect.is_empty() {
            config
                .insert_json5("connect/endpoints", &json!(args.connect).to_string())
                .unwrap();
        }

        if !args.listen.is_empty() {
            config
                .insert_json5("listen/endpoints", &json!(args.listen).to_string())
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
