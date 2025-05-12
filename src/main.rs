mod args;
use crate::args::WebUISettings;
use actix::prelude::*;
use actix_files::{self as fs, NamedFile};
use actix_web::{
    http::header::ContentLength,
    web::{self, Bytes},
    App, HttpRequest, HttpResponse, HttpServer, Responder, Result,
};
use actix_web_actors::ws;
use actix_web_lab::middleware::RedirectHttps;
use anyhow::{Context, Result as res};
use args::Args;
use async_stream::stream;
use camino::Utf8Path;
use cdr::{CdrLe, Infinite};
use chrono::DateTime;
use clap::Parser;
use log::{debug, error, info, warn};
use maivin_publisher::{
    client::{self, Metrics},
    mcap::{self as pub_mcap},
};
use mcap::Summary;
use memmap::Mmap;
use mime::Mime;
use openssl::{
    pkey::{PKey, Private},
    ssl::{SslAcceptor, SslMethod},
};
use percent_encoding::percent_decode;
use pnet::datalink;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, BufRead, Write},
    ops::Deref,
    path::{Path, PathBuf},
    process::{Child, Command},
    str::FromStr,
    sync::{
        atomic::{AtomicI64, Ordering},
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt as _, runtime::Handle};
use uuid::{
    v1::{Context as uuidContex, Timestamp},
    Uuid,
};

#[derive(Serialize)]
struct FileInfo {
    name: String,
    size: u64, // Size in MB
    created: String,
    topics: HashMap<String, TopicInfo>,
    average_video_length: f64,
}
#[derive(Serialize)]
struct DirectoryResponse {
    dir_name: String,
    files: Option<Vec<FileInfo>>,
    message: Option<String>,
    topics: Option<Vec<String>>,
}

#[derive(Deserialize, Debug, Clone)]
struct DeleteParams {
    directory: String,
    file: String,
    url: Option<String>,
    jwt: Option<String>,
    topic: Option<String>,
}

#[derive(Serialize)]
struct TopicInfo {
    message_count: usize,
    average_fps: f64,
    video_length: f64,
}

struct AppState {
    process: Mutex<Option<Child>>,
}

struct ThreadState {
    is_running: Mutex<bool>,
}

#[derive(Deserialize)]
struct ConfigPath {
    service: String,
}

const SERVER_PEM: &[u8] = include_bytes!("../server.pem");
const STOP: &str = "STOP";

fn load_encrypted_private_key() -> PKey<Private> {
    let buffer = SERVER_PEM;

    PKey::private_key_from_pem_passphrase(buffer, b"password").expect("Failed to load private key")
}

struct MessageStream {
    clients: Arc<Mutex<Vec<Recipient<BroadcastMessage>>>>,
    on_exit: Sender<String>,
    on_err: Box<dyn Fn() + Send + Sync>,
    high_priority: bool,
    // err: Sender<String>,
}

#[derive(Deserialize)]
struct ServiceAction {
    service: String,
    action: String,
}

impl MessageStream {
    fn new(
        on_exit: Sender<String>,
        on_err: Box<dyn Fn() + Send + Sync>,
        high_priority: bool,
    ) -> Self {
        MessageStream {
            clients: Arc::new(Mutex::new(Vec::new())),
            on_exit,
            on_err,
            high_priority,
        }
    }

    fn add_client(&self, client: Recipient<BroadcastMessage>) {
        self.clients.lock().unwrap().push(client);
    }

    fn broadcast(&self, message: BroadcastMessage) {
        for client in self.clients.lock().unwrap().iter() {
            if self.high_priority {
                client.do_send(message.clone());
            } else {
                let res = client.try_send(message.clone());
                if res.is_err() {
                    (self.on_err)();
                }
            }
        }
    }
}

#[derive(Message, Clone)]
#[rtype(result = "()")]
struct BroadcastMessage(Vec<u8>);

async fn index(data: web::Data<ServerContext>) -> Result<fs::NamedFile> {
    let base_path = PathBuf::from(&data.args.docroot);

    let data_path = if base_path.is_dir() {
        base_path.join("index.html")
    } else {
        base_path
    };

    debug!("{:?}", data_path);

    match fs::NamedFile::open(data_path) {
        Ok(file) => Ok(file),
        Err(_) => {
            error!("Index file not found");
            Err(actix_web::error::ErrorNotFound("Index file not found"))
        }
    }
}

async fn websocket_handler_errors(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    data: web::Data<ServerContext>,
) -> Result<HttpResponse, actix_web::Error> {
    ws::start(MyWebSocket::new(data.err_stream.clone(), 1), &req, stream).map_err(|e| {
        error!("WebSocket connection failed: {:?}", e);
        e
    })
}

async fn websocket_handler_high_priority(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    data: web::Data<ServerContext>,
) -> Result<HttpResponse, actix_web::Error> {
    websocket_handler(req, stream, data, true).await
}

async fn websocket_handler_low_priority(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    data: web::Data<ServerContext>,
) -> Result<HttpResponse, actix_web::Error> {
    websocket_handler(req, stream, data, false).await
}

async fn websocket_handler(
    req: actix_web::HttpRequest,
    stream: web::Payload,
    data: web::Data<ServerContext>,
    is_high_priority: bool,
) -> Result<HttpResponse, actix_web::Error> {
    let path = req.uri().path().to_owned();
    let cleaned_path = if let Some(stripped) = path.strip_prefix('/') {
        stripped.to_owned()
    } else {
        path.to_owned()
    };
    let cleaned_path = if cleaned_path.ends_with('/') {
        cleaned_path[..cleaned_path.len() - 1].to_owned()
    } else {
        cleaned_path.clone()
    };

    let args = data.args.clone();
    let topic = cleaned_path.clone();
    let (tx, rx) = channel();
    let video_stream = Arc::new(MessageStream::new(
        tx,
        Box::new(move || {
            let new_val = data.err_count.fetch_add(1, Ordering::SeqCst) + 1;
            let msg = format!(
                "{{\"dropped frames\": {new_val}, \"dropped_topic\": \"{}\"}}",
                topic.clone()
            );
            let msg = cdr::serialize::<_, _, CdrLe>(&msg, Infinite).unwrap();
            data.err_stream.broadcast(BroadcastMessage(msg));
        }),
        is_high_priority,
    ));
    let video_stream_clone = video_stream.clone();

    let capacity = if is_high_priority { 16 } else { 1 };
    let ws_result = ws::start(MyWebSocket::new(video_stream, capacity), &req, stream);

    if ws_result.is_ok() {
        thread::Builder::new()
            .name("zenoh".to_string())
            .spawn(move || zenoh_listener(video_stream_clone, args, rx, cleaned_path))?;
    } else {
        drop(rx);
    }

    ws_result.map_err(|e| {
        error!("WebSocket connection failed: {:?}", e);
        e
    })
}

struct MyWebSocket {
    video_stream: Arc<MessageStream>,
    capacity: usize,
}

impl MyWebSocket {
    fn new(video_stream: Arc<MessageStream>, capacity: usize) -> Self {
        MyWebSocket {
            video_stream,
            capacity,
        }
    }
}

impl Actor for MyWebSocket {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        if self.capacity > 0 {
            ctx.set_mailbox_capacity(self.capacity);
        }
        self.video_stream.add_client(ctx.address().recipient());
    }
}

impl Drop for MyWebSocket {
    fn drop(&mut self) {
        let _ = self.video_stream.on_exit.send(STOP.to_string());
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WebSocketMessage {
    action: String,
    topic: String,
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for MyWebSocket {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Text(text)) => {
                if let Ok(message) = serde_json::from_str::<WebSocketMessage>(&text) {
                    if message.action.as_str() == "subscribe" {
                        let webpage_topic = message.topic.to_string();
                        info!("Subscribed to topic: {}", webpage_topic);
                        self.video_stream.on_exit.send(webpage_topic).unwrap();
                    }
                }
            }
            Ok(_) => (),
            Err(_) => ctx.stop(),
        }
    }
}

impl Handler<BroadcastMessage> for MyWebSocket {
    type Result = ();

    fn handle(&mut self, msg: BroadcastMessage, ctx: &mut Self::Context) {
        ctx.binary(msg.0);
    }
}

#[tokio::main]
async fn zenoh_listener(
    video_stream: Arc<MessageStream>,
    args: Args,
    rx: Receiver<String>,
    topic: String,
) {
    let session = zenoh::open(args.clone()).await.unwrap();
    let subscriber = session
        .declare_subscriber(topic.clone())
        .await
        .expect("Failed to declare Zenoh subscriber");
    loop {
        if let Ok(msg) = rx.recv_timeout(Duration::from_millis(0)) {
            if msg == STOP {
                drop(video_stream);
                subscriber
                    .undeclare()
                    .await
                    .expect("Failed to undeclare subscriber");
                session
                    .close()
                    .await
                    .expect("Failed to close Zenoh session");
                return;
            }
        }
        debug!("topic: {:?}", topic);
        // Take all messages from the subscriber. If there are no messages, then wait
        // for the next message. If there were messages, only process the most recent
        // message
        let msgs = subscriber.drain();
        match msgs.last() {
            None => {
                if let Ok(sample) = subscriber.recv_async().await {
                    let data = sample.payload().to_bytes().to_vec();
                    video_stream.broadcast(BroadcastMessage(data));
                }
            }
            Some(sample) => {
                let data = sample.payload().to_bytes().to_vec();
                video_stream.broadcast(BroadcastMessage(data));
            }
        }
    }
}

async fn custom_file_handler(
    req: HttpRequest,
    data: web::Data<ServerContext>,
) -> actix_web::Result<NamedFile> {
    let path: String = req.match_info().query("file").parse().unwrap();
    let base_path = Path::new(&data.args.docroot);

    let file_path = base_path.join(&path);

    if file_path.exists() && file_path.is_file() {
        return Ok(NamedFile::open(file_path)?);
    }

    let html_path = base_path.join(format!("{}.html", path));
    if html_path.exists() && html_path.is_file() {
        return Ok(NamedFile::open(html_path)?);
    }

    Err(actix_web::error::ErrorNotFound(format!(
        "File {:?} not found",
        path
    )))
}

async fn user_mode_check_recorder_status() -> String {
    let pid_file = Path::new("/var/run/recorder.pid");

    if !pid_file.exists() {
        // Even if PID file doesn't exist, check if process is running
        let ps_output = Command::new("ps").arg("aux").output();

        match ps_output {
            Ok(output) => {
                let processes = String::from_utf8_lossy(&output.stdout);
                let recorder_running = processes
                    .lines()
                    .any(|line| line.contains("recorder") && !line.contains("grep"));

                if recorder_running {
                    // Process is running but PID file is missing
                    return "Recorder is running".to_string();
                }
            }
            Err(e) => {
                error!("Failed to execute ps command: {:?}", e);
            }
        }
        return "Recorder is not running".to_string();
    }

    match std::fs::read_to_string(pid_file) {
        Ok(pid_str) => {
            if let Ok(_pid) = pid_str.trim().parse::<i32>() {
                // Check both the PID file process and ps output
                let ps_output = Command::new("ps").arg("aux").output();

                match ps_output {
                    Ok(output) => {
                        let processes = String::from_utf8_lossy(&output.stdout);
                        let recorder_running = processes
                            .lines()
                            .any(|line| line.contains("recorder") && !line.contains("grep"));

                        if recorder_running {
                            "Recorder is running".to_string()
                        } else {
                            // Process not found, clean up PID file
                            let _ = std::fs::remove_file(pid_file);
                            "Recorder is not running".to_string()
                        }
                    }
                    Err(_) => {
                        let _ = std::fs::remove_file(pid_file);
                        "Error checking process status".to_string()
                    }
                }
            } else {
                let _ = std::fs::remove_file(pid_file);
                "Invalid PID in PID file".to_string()
            }
        }
        Err(_) => {
            let _ = std::fs::remove_file(pid_file);
            "Error reading PID file".to_string()
        }
    }
}

async fn check_recorder_status() -> impl Responder {
    let service_status = Command::new("systemctl")
        .arg("is-active")
        .arg("recorder")
        .output();

    match service_status {
        Ok(output) => {
            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if status == "active" {
                HttpResponse::Ok().body("Recorder is running")
            } else {
                HttpResponse::Ok().body("Recorder is not running")
            }
        }
        Err(e) => HttpResponse::InternalServerError()
            .body(format!("Error checking service status: {:?}", e)),
    }
}

async fn check_service_status(service_name: &str) -> Result<String, String> {
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

async fn start(data: web::Data<AppState>) -> impl Responder {
    let mut process_guard = data.process.lock().unwrap();

    let mut command = Command::new("sudo");
    command.arg("systemctl").arg("start").arg("recorder");

    let process = match command.spawn() {
        Ok(p) => p,
        Err(e) => {
            let error_message = format!("Failed to start recorder: {:?}", e);
            info!("{}", error_message);
            return HttpResponse::Ok().json(json!({
                "status": "started",
                "message": "Recording started successfully"
            }));
        }
    };

    *process_guard = Some(process);

    HttpResponse::Ok().body("Recorder started")
}

async fn user_mode_start(
    data: web::Data<AppState>,
    arg_data: web::Data<ServerContext>,
) -> impl Responder {
    let mut process_guard = data.process.lock().unwrap();

    // Check if already running
    let status_str = user_mode_check_recorder_status().await;
    debug!("Current recorder status: {}", status_str);

    if status_str == "Recorder is running" {
        return HttpResponse::BadRequest().json(json!({
            "status": "error",
            "message": "Recorder is already running"
        }));
    }

    let mut command = Command::new("sudo");
    command
        .env("STORAGE", &arg_data.args.storage_path) // Set STORAGE environment variable
        .arg("-E") // Preserve environment variables
        .arg("/home/root/recorder")
        .arg("--all-topics"); // Add --all-topics flag to recorder command

    debug!("Starting recorder with command: {:?}", command);

    let process = match command.spawn() {
        Ok(p) => {
            debug!("Recorder process started with PID: {:?}", p.id());
            p
        }
        Err(e) => {
            let error_message = format!("Failed to start recorder: {:?}", e);
            error!("{}", error_message);
            return HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": error_message
            }));
        }
    };

    let pid = process.id();
    *process_guard = Some(process);

    // Wait a bit for the process to start and create PID file
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let status = user_mode_check_recorder_status().await;
        if status == "Recorder is running" {
            return HttpResponse::Ok().json(json!({
                "status": "started",
                "message": "Recording started successfully"
            }));
        }
    }

    // If we get here, the process started but PID file wasn't created
    // Let's create it ourselves
    if let Some(process) = process_guard.as_mut() {
        match process.try_wait() {
            Ok(Some(status)) => {
                let error_message = format!("Recorder process exited with status: {:?}", status);
                error!("{}", error_message);
                *process_guard = None;
                return HttpResponse::InternalServerError().json(json!({
                    "status": "error",
                    "message": error_message
                }));
            }
            Ok(None) => {
                // Process is still running, create PID file
                debug!("Creating PID file manually");
                if let Err(e) = std::fs::write("/var/run/recorder.pid", pid.to_string()) {
                    let error_message = format!("Failed to create PID file: {:?}", e);
                    error!("{}", error_message);
                    if let Some(mut process) = process_guard.take() {
                        let _ = process.kill();
                    }
                    return HttpResponse::InternalServerError().json(json!({
                        "status": "error",
                        "message": error_message
                    }));
                }
                return HttpResponse::Ok().json(json!({
                    "status": "started",
                    "message": "Recording started successfully"
                }));
            }
            Err(e) => {
                let error_message = format!("Error checking process status: {:?}", e);
                error!("{}", error_message);
                *process_guard = None;
                return HttpResponse::InternalServerError().json(json!({
                    "status": "error",
                    "message": error_message
                }));
            }
        }
    }

    let error_message = "Failed to verify recorder started";
    error!("{}", error_message);
    if let Some(mut process) = process_guard.take() {
        let _ = process.kill();
    }
    HttpResponse::InternalServerError().json(json!({
        "status": "error",
        "message": error_message
    }))
}

async fn stop(data: web::Data<AppState>) -> impl Responder {
    let mut process_guard = data.process.lock().unwrap();
    let mut command = Command::new("sudo");
    command.arg("systemctl").arg("stop").arg("recorder");

    match command.status() {
        Ok(status) if status.success() => {
            *process_guard = None;
            info!("Recorder service stopped");
            HttpResponse::Ok().json(json!({
                "status": "stopped",
                "message": "Recording stopped successfully"
            }))
        }
        Ok(status) => {
            let error_message = format!("Failed to stop recorder service: {:?}", status);
            error!("{}", error_message);
            HttpResponse::InternalServerError().body(error_message)
        }
        Err(e) => {
            let error_message = format!("Failed to run systemctl stop recorder: {:?}", e);
            error!("{}", error_message);
            HttpResponse::InternalServerError().body(error_message)
        }
    }
}

async fn user_mode_stop(data: web::Data<AppState>) -> impl Responder {
    let mut process_guard = data.process.lock().unwrap();
    let pid_file = Path::new("/var/run/recorder.pid");

    // First try to stop the process if we have it in our guard
    if let Some(mut process) = process_guard.take() {
        debug!(
            "Attempting to stop recorder process with PID: {:?}",
            process.id()
        );
        // Try to terminate gracefully first
        if let Err(e) = process.kill() {
            error!("Failed to kill process directly: {:?}", e);
            // Process might have already terminated
        }
    }

    // Find and kill all recorder processes gracefully
    let ps_output = Command::new("ps").arg("aux").output().map_err(|e| {
        error!("Failed to execute ps command: {:?}", e);
        e
    });

    if let Ok(output) = ps_output {
        let processes = String::from_utf8_lossy(&output.stdout);
        for line in processes.lines() {
            if line.contains("recorder") && !line.contains("grep") {
                if let Some(pid) = line.split_whitespace().nth(1) {
                    debug!("Found recorder process: {}", line);
                    // First try SIGTERM (15) for graceful shutdown
                    let term_result = Command::new("sudo")
                        .arg("kill")
                        .arg("-15") // SIGTERM
                        .arg(pid)
                        .status();

                    if let Ok(status) = term_result {
                        if !status.success() {
                            warn!("Failed to send SIGTERM to process {}, trying SIGKILL", pid);
                            // If SIGTERM fails, wait a bit and then try SIGKILL
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            let _ = Command::new("sudo")
                                .arg("kill")
                                .arg("-9") // SIGKILL
                                .arg(pid)
                                .status();
                        }
                    }
                }
            }
        }
    }

    // Clean up PID file if it exists
    if pid_file.exists() {
        if let Err(e) = std::fs::remove_file(pid_file) {
            error!("Failed to remove PID file: {:?}", e);
        }
    }

    // Wait a moment for processes to terminate
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Double check if any recorder processes are still running
    let ps_check = Command::new("ps").arg("aux").output().map_err(|e| {
        error!("Failed to execute ps check command: {:?}", e);
        e
    });

    if let Ok(output) = ps_check {
        let processes = String::from_utf8_lossy(&output.stdout);
        let still_running = processes
            .lines()
            .any(|line| line.contains("recorder") && !line.contains("grep"));

        if still_running {
            let error_message = "Failed to stop all recorder processes";
            error!("{}", error_message);
            return HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": error_message
            }));
        }
    }

    debug!("All recorder processes stopped successfully");
    HttpResponse::Ok().json(json!({
        "status": "stopped",
        "message": "Recording stopped successfully"
    }))
}

async fn delete(params: web::Json<DeleteParams>) -> impl Responder {
    let file_path = format!("{}/{}", params.directory, params.file);
    debug!("Attempting to delete file: {}", file_path);

    if Path::new(&file_path).exists() {
        let mut command = Command::new("sudo");
        command.arg("rm").arg("-rf").arg(&file_path);

        debug!("Command: {:?}", command);

        match command.status() {
            Ok(status) if status.success() => {
                info!("File deleted successfully");
                HttpResponse::Ok().json(json!({
                    "status": "removed",
                    "message": "File deleted successfully"
                }))
            }
            Ok(status) => {
                let error_message = format!("Failed to delete file: {:?}", status);
                error!("{}", error_message);
                HttpResponse::InternalServerError().body(error_message)
            }
            Err(e) => {
                let error_message = format!("Failed to run rm -rf: {:?}", e);
                error!("{}", error_message);
                HttpResponse::InternalServerError().body(error_message)
            }
        }
    } else {
        HttpResponse::NotFound().body("File not found")
    }
}

#[derive(Serialize, Debug)]
struct UploadCredentials {
    url: String,
    jwt: String,
    topic: String,
}

fn read_uploader_credentials() -> io::Result<UploadCredentials> {
    let file_path = "/etc/default/uploader";
    let file = File::open(file_path)?;
    let reader = io::BufReader::new(file);

    let mut url = String::new();
    let mut jwt = String::new();
    let mut topic = String::new();

    for line in reader.lines() {
        let line = line?;
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
    debug!("Topic = {:?}", topic);
    Ok(UploadCredentials { url, jwt, topic })
}

async fn get_upload_credentials() -> impl Responder {
    match read_uploader_credentials() {
        Ok(credentials) => HttpResponse::Ok().json(credentials),
        Err(err) => {
            let error_message = err.to_string();
            error!("{:?}", error_message);
            HttpResponse::BadRequest().body(error_message)
        }
    }
}

async fn save_credentials(url: String, jwt: String, topic: String) -> impl Responder {
    let file_path = "/etc/default/uploader";
    let content = format!(
        "URL = \"{}\"\nJWT = \"{}\"\nTOPIC = \"{}\"",
        url, jwt, topic
    );

    match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(file_path)
    {
        Ok(mut file) => {
            if let Err(e) = writeln!(file, "{}", content) {
                error!("Failed to write to file: {}", e);
                return HttpResponse::InternalServerError().finish();
            }
            HttpResponse::Ok().finish()
        }
        Err(e) => {
            error!("Failed to open file: {}", e);
            HttpResponse::InternalServerError().finish()
        }
    }
}

fn get_mac_address() -> Option<[u8; 6]> {
    for iface in datalink::interfaces() {
        if iface.is_up() && !iface.is_loopback() {
            let mac = iface.mac;
            if mac != Some(pnet::datalink::MacAddr::new(0, 0, 0, 0, 0, 0)) {
                debug!("{:?}", mac?.octets());
                return Some(mac?.octets());
            }
        }
    }
    None
}

fn convert_time_to_timestamp(time: u64, counter: u16) -> Timestamp {
    let seconds = time / 1_000_000_000;
    let nanos = (time % 1_000_000_000) as u32;
    Timestamp::from_unix(uuidContex::new(counter), seconds, nanos)
}

async fn upload(
    params: web::Json<DeleteParams>,
    handle: tokio::runtime::Handle,
    state: web::Data<Arc<ThreadState>>,
) -> impl Responder {
    let file_path = format!("{}/{}", params.directory, params.file);
    debug!("Attempting to delete file: {}", file_path);

    let sequence_name = <str as AsRef<Path>>::as_ref(file_path.as_str())
        .file_stem()
        .unwrap()
        .to_os_string()
        .into_string()
        .unwrap();
    let source = sequence_name
        .split_once('_')
        .map(|(source, _)| source.to_string());

    let url = params.url.as_ref().map_or_else(
        || "https://dveml.com/samples/upload".to_string(),
        |s| s.clone(),
    );
    let jwt = params
        .jwt
        .as_ref()
        .map_or_else(|| "".to_string(), |s| s.clone());

    let topic = params
        .topic
        .as_ref()
        .map_or_else(|| "".to_string(), |s| s.clone());

    save_credentials(url.clone(), jwt.clone(), topic.clone()).await;

    debug!("url = {:?} jwt = {:?}", url, jwt);
    debug!("{:?}", sequence_name);

    {
        let mut is_running = state.is_running.lock().unwrap();
        if *is_running {
            return HttpResponse::TooManyRequests().json(json!({
                "status": "Busy",
                "message": "Another upload is already in progress. Please try again later."
            }));
        }
        *is_running = true;
    }

    let state_clone = state.clone();
    let mat = Arc::new(Mutex::new(Metrics::default()));
    let mat_clone = Arc::clone(&mat);

    handle
        .clone()
        .spawn_blocking(move || {
            let handle_ref = &handle;
            handle_ref.block_on(async move {
                let parse_settings = pub_mcap::ParseSettings {
                    threshold: 0.0,
                    framerate: 0,
                    image_type: pub_mcap::ImageType::None,
                    topic: Some(topic),
                    labels: None,
                    absolute_offset: None,
                    relative_offset: None,
                    skip_messages: None,
                    disable_topics: None,
                };
                let mmap = pub_mcap::open_mmap(&file_path).unwrap();
                let file_path_vec = vec![file_path.clone()];
                let mmaps = vec![mmap];
                let (messages, sample_count) =
                    match pub_mcap::parse(file_path_vec.deref(), &mmaps, parse_settings) {
                        Ok(messages) => messages,
                        Err(e) => {
                            let error_message = e.to_string();
                            if error_message.contains("Bad magic number") {
                                let mut mat = mat_clone.lock().unwrap();
                                *mat = Metrics {
                                    success: 0,
                                    already_uploaded: 0,
                                    missing_image: 0,
                                    duration: Duration::MAX,
                                };
                                return HttpResponse::InternalServerError().json(json!({
                                    "status": "BadMagic",
                                    "message": "Invalid MCAP file, please check the MCAP file"
                                }));
                            }
                            let error_message = format!("Failed to parse MCAP file: {:?}", e);
                            error!("{}", error_message);
                            return HttpResponse::InternalServerError().json(json!({
                                "status": "Invalid",
                                "message": "Invalid topic, please check /etc/default/uploader"
                            }));
                        }
                    };

                let mac = get_mac_address().expect("Unable to find MAC address");
                let time = match mcap_start_time(file_path.clone()) {
                    Ok(info) => info,
                    Err(_) => todo!(),
                };
                let timestamp = convert_time_to_timestamp(time, 42);
                debug!("{:?}", timestamp);
                let uuid = Uuid::new_v1(timestamp, &mac);

                debug!("UUID = {:?}", uuid);

                let pb = maivin_publisher::create_pb(sample_count, "sending samples");
                let client = client::Client::new(url, jwt, Some(uuid), sequence_name, None, source);
                let (metrics, upload_handler) =
                    client::send_all_messages(Arc::new(client), messages, handle_ref, Some(pb))
                        .await;
                let mut mat = mat_clone.lock().unwrap();
                *mat = metrics.clone();
                info!("{:?}", metrics);
                if metrics.success > 0 && metrics.already_uploaded == 0 {
                    match upload_handler {
                        Ok(()) => HttpResponse::Ok().json(json!({
                            "status": "Uploading",
                            "message": "MCAP uploaded successfully"
                        })),
                        Err(e) => HttpResponse::InternalServerError().json(json!({
                            "status": "Error",
                            "message": format!("Failed to upload MCAP: {:?}", e)
                        })),
                    };
                }
                HttpResponse::Ok().json(json!({
                    "status": "Uploading",
                    "message": "MCAP uploaded successfully"
                }))
            });
            let mut is_running = state_clone.is_running.lock().unwrap();
            *is_running = false;
        })
        .await
        .unwrap();
    let metrics_data = mat.lock().unwrap();
    debug!("{:?}", metrics_data.clone());
    if metrics_data.duration == Duration::ZERO {
        HttpResponse::InternalServerError().json(json!({
            "status": "Invalid",
            "message": "Invalid topic, please check /etc/default/uploader"
        }))
    } else if metrics_data.duration == Duration::MAX {
        HttpResponse::InternalServerError().json(json!({
            "status": "BadMagic",
            "message": "Invalid MCAP file, please check the MCAP file"
        }))
    } else if metrics_data.success == 0 && metrics_data.already_uploaded > 0 {
        HttpResponse::InternalServerError().json(json!({
            "status": "Already",
            "message": "MCAP already uploaded, unable to upload again"
        }))
    } else {
        HttpResponse::Ok().json(json!({
            "status": "Uploading",
            "message": "MCAP uploaded successfully"
        }))
    }
}

fn read_storage_directory() -> io::Result<String> {
    let file_path = "/etc/default/recorder";
    let file = File::open(file_path)?;
    let reader = io::BufReader::new(file);

    for line in reader.lines() {
        let line = line?;
        if line.starts_with("STORAGE") {
            let parts: Vec<&str> = line.split('=').collect();
            if parts.len() == 2 {
                debug!(
                    "MCAP Directory: {:?}",
                    parts[1].trim().trim_matches('"').to_string()
                );
                return Ok(parts[1].trim().trim_matches('"').to_string());
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "STORAGE directory not found",
    ))
}

// Below function will be needed when we start working on the DVE Uploader integration
// fn read_topics_from_config() -> io::Result<Vec<String>> {
//     let file_path = "/etc/default/recorder";
//     let file = File::open(file_path)?;
//     let reader = io::BufReader::new(file);

//     let topics;

//     for line in reader.lines() {
//         let line = line?;
//         if line.starts_with("TOPICS") {
//             let parts: Vec<&str> = line.split('=').collect();
//             if parts.len() == 2 {
//                 topics = parts[1].split_whitespace().map(|s| s.to_string()).collect();
//                 return Ok(topics);
//             }
//         }
//     }

//     Err(io::Error::new(
//         io::ErrorKind::NotFound,
//         "TOPICS not found in the config file",
//     ))
// }

struct WebSocketSession {
    context: Option<web::Data<ServerContext>>,
}

impl Actor for WebSocketSession {
    type Context = ws::WebsocketContext<Self>;
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WebSocketSession {
    fn handle(&mut self, msg: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let arg_data = match &self.context {
            Some(data) => data,
            None => {
                error!("ServerContext not initialized");
                return;
            }
        };

        match msg {
            Ok(ws::Message::Text(text)) => {
                debug!("Received message: {}", text);
                let mut directory: String = arg_data.args.storage_path.to_string();
                // let mut topics: Option<Vec<String>> = Some(vec!["".to_string()]);
                if !arg_data.args.user_mode {
                    directory = match read_storage_directory() {
                        Ok(dir) => dir,
                        Err(e) => {
                            error!("Error reading directory from config: {}", e);
                            let response = serde_json::to_string(
                                &json!({"error": "Error reading directory from config"}),
                            )
                            .unwrap();
                            ctx.text(response);
                            return;
                        }
                    };

                    // let topics = match read_topics_from_config() {
                    //     Ok(topics) => topics,
                    //     Err(e) => {
                    //         error!("Error reading topics from config: {}", e);
                    //         let response = serde_json::to_string(
                    //             &json!({"error": "Error reading topics from config"}),
                    //         )
                    //         .unwrap();
                    //         ctx.text(response);
                    //         return;
                    //     }
                    // };
                }

                debug!("Listing files in directory: {}", directory.clone());

                match std::fs::read_dir(&directory) {
                    Ok(entries) => {
                        let files: Vec<FileInfo> = entries
                            .filter_map(Result::ok)
                            .filter_map(|entry| {
                                if let Some(extension) = entry.path().extension() {
                                    if extension == "mcap" {
                                        let metadata = entry.metadata().ok()?;
                                        let size = metadata.len();
                                        let created = metadata
                                            .created()
                                            .ok()?
                                            .duration_since(UNIX_EPOCH)
                                            .ok()?
                                            .as_secs();

                                        let (topics_info, average_video_length) =
                                            match read_mcap_info(Utf8Path::from_path(
                                                &entry.path(),
                                            )?) {
                                                Ok(info) => info,
                                                Err(_) => (HashMap::new(), 0.0),
                                            };

                                        Some(FileInfo {
                                            name: entry.file_name().to_string_lossy().to_string(),
                                            size: size / (1024 * 1024),
                                            created: DateTime::from_timestamp(created as i64, 0)
                                                .unwrap()
                                                .with_timezone(&chrono::Local)
                                                .format("%Y-%m-%d %H:%M:%S")
                                                .to_string(),
                                            topics: topics_info,
                                            average_video_length,
                                        })
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            })
                            .collect();
                        let response = if files.is_empty() {
                            DirectoryResponse {
                                dir_name: directory.clone(),
                                files: None,
                                message: Some("No MCAP files found".to_string()),
                                topics: None,
                            }
                        } else {
                            DirectoryResponse {
                                dir_name: directory.clone(),
                                files: Some(files),
                                message: None,
                                topics: None,
                            }
                        };

                        let response = serde_json::to_string(&response).unwrap();
                        ctx.text(response);
                    }
                    Err(e) => {
                        error!("Directory Not Defined: {}", e);
                        let response = serde_json::to_string(
                            &json!({"error": "Directory Not Defined", "dir_name": directory.clone()}),
                        )
                        .unwrap();
                        ctx.text(response);
                    }
                }
            }
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            Ok(ws::Message::Close(reason)) => ctx.close(reason),
            _ => (),
        }
    }
}

async fn mcap_websocket_handler(req: HttpRequest, stream: web::Payload) -> impl Responder {
    let data = req.app_data::<web::Data<ServerContext>>().unwrap().clone();
    ws::start(
        WebSocketSession {
            context: Some(data),
        },
        &req,
        stream,
    )
}

struct ServerContext {
    args: Args,
    err_stream: Arc<MessageStream>,
    err_count: AtomicI64,
}

async fn serve_settings_page(data: web::Data<ServerContext>) -> Result<NamedFile> {
    let base_path = PathBuf::from(&data.args.docroot);

    let file_path = if base_path.is_dir() {
        base_path.join("config/settings.html")
    } else {
        base_path
    };
    debug!("{:?}", file_path);
    Ok(NamedFile::open(file_path)?)
}

async fn serve_config_page(
    data: web::Data<ServerContext>,
    path: web::Path<String>,
) -> Result<NamedFile> {
    let service = path.into_inner();
    let base_path = PathBuf::from(&data.args.docroot);

    let file_path = if base_path.is_dir() {
        base_path.join(format!("config/{}.html", service))
    } else {
        base_path
    };
    debug!("{:?}", file_path);
    Ok(NamedFile::open(file_path)?)
}

async fn mcap_downloader(req: HttpRequest) -> impl Responder {
    let path: String = req.match_info().query("file").parse().unwrap();
    let base_path = Path::new("/");

    let file_path = base_path.join(&path);

    if !file_path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("MCAP"))
    {
        return Err(actix_web::error::ErrorForbidden(
            "Invalid file extension. Only .mcap files are allowed.",
        ));
    }

    if !file_path.exists() || !file_path.is_file() {
        return Err(actix_web::error::ErrorNotFound(format!(
            "File {:?} not found",
            path
        )));
    }

    let mut file = tokio::fs::File::open(file_path).await?;
    let file_size = file.metadata().await?.len() as usize;

    let mcap_stream = stream! {
        let mut buffer = [0; 64 * 1024];
        loop {
            let bytes_read = file.read(&mut buffer).await?;
            if bytes_read == 0 {
                break;
            }

            yield Result::<Bytes, std::io::Error>::Ok(Bytes::copy_from_slice(&buffer[..bytes_read]));
        }
    };

    let mime_type = Mime::from_str("application/octet-stream").unwrap();

    Ok(HttpResponse::Ok()
        .content_type(mime_type)
        .insert_header(ContentLength(file_size))
        .streaming(mcap_stream))
}

#[derive(Serialize)]
struct FormattedSize {
    value: f64,
    unit: String,
}

impl FormattedSize {
    fn from_bytes(bytes: u64) -> Self {
        if bytes >= 1024 * 1024 * 1024 {
            // Convert to GB if >= 1GB
            FormattedSize {
                value: (bytes as f64) / (1024.0 * 1024.0 * 1024.0),
                unit: "GB".to_string(),
            }
        } else {
            // Convert to MB if < 1GB
            FormattedSize {
                value: (bytes as f64) / (1024.0 * 1024.0),
                unit: "MB".to_string(),
            }
        }
    }
}

#[derive(Serialize)]
struct StorageDetails {
    path: String,
    exists: bool,
    available_space: Option<FormattedSize>,
    total_space: Option<FormattedSize>,
}

#[derive(Serialize)]
struct StorageInfo {
    internal: StorageDetails,
    external: StorageDetails,
    sd_card_present: bool,
    sd_card_mounted: bool,
    sd_card_formatted: bool,
}

async fn check_storage_availability() -> impl Responder {
    let internal_path = Path::new("/");
    let external_path = Path::new("/media/DATA");

    let sd_card_present = std::fs::read_dir("/sys/block")
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                name_str == "mmcblk1"
            })
        })
        .unwrap_or(false);

    let sd_card_mounted = std::fs::read_to_string("/proc/mounts")
        .map(|contents| {
            contents.lines().any(|line| {
                line.contains("/dev/mmcblk1p1")
                    && (line.contains("/media/DATA") || line.contains("/var/rootdirs/media/DATA"))
            })
        })
        .unwrap_or(false);

    let sd_card_formatted = if sd_card_present {
        std::fs::read_to_string("/proc/partitions")
            .map(|contents| contents.contains("mmcblk1p1"))
            .unwrap_or(false)
    } else {
        false
    };

    let internal_details = StorageDetails {
        path: "/home/torizon/recordings".to_string(),
        exists: true,
        available_space: std::fs::metadata(internal_path)
            .and_then(|_| fs2::available_space(internal_path))
            .ok()
            .map(FormattedSize::from_bytes),
        total_space: std::fs::metadata(internal_path)
            .and_then(|_| fs2::total_space(internal_path))
            .ok()
            .map(FormattedSize::from_bytes),
    };

    let external_details = StorageDetails {
        path: external_path.to_string_lossy().to_string(),
        exists: external_path.exists() && sd_card_mounted,
        available_space: if sd_card_mounted {
            let mount_path = Path::new("/var/rootdirs/media/DATA");
            std::fs::metadata(mount_path)
                .and_then(|_| fs2::available_space(mount_path))
                .ok()
                .map(FormattedSize::from_bytes)
        } else {
            None
        },
        total_space: if sd_card_mounted {
            let mount_path = Path::new("/var/rootdirs/media/DATA");
            std::fs::metadata(mount_path)
                .and_then(|_| fs2::total_space(mount_path))
                .ok()
                .map(FormattedSize::from_bytes)
        } else {
            None
        },
    };

    let storage_info = StorageInfo {
        internal: internal_details,
        external: external_details,
        sd_card_present,
        sd_card_mounted,
        sd_card_formatted,
    };

    debug!(
        "Storage info: internal={:?} external={:?} sd_present={:?} mounted={:?} formatted={:?}",
        storage_info.internal.exists,
        storage_info.external.exists,
        storage_info.sd_card_present,
        storage_info.sd_card_mounted,
        storage_info.sd_card_formatted
    );

    HttpResponse::Ok().json(storage_info)
}

#[derive(Deserialize, Debug, Clone)]
struct PlaybackParams {
    directory: String,
    file: String,
}

#[derive(Serialize)]
struct PlaybackResponse {
    status: String,
    message: String,
    current_file: Option<String>,
}

async fn start_replay(params: web::Json<PlaybackParams>) -> impl Responder {
    let file_path = format!("{}/{}", params.directory, params.file);
    debug!("Attempting to play MCAP file: {}", file_path);

    if !Path::new(&file_path).exists() {
        return HttpResponse::NotFound().json(PlaybackResponse {
            status: "error".to_string(),
            message: "File not found".to_string(),
            current_file: None,
        });
    }

    let status = Command::new("systemctl")
        .arg("is-active")
        .arg("replay")
        .output();

    match status {
        Ok(output) => {
            let status_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if status_str == "active" {
                return HttpResponse::BadRequest().json(PlaybackResponse {
                    status: "error".to_string(),
                    message: "Replay service is already running".to_string(),
                    current_file: None,
                });
            }
        }
        Err(e) => {
            error!("Error checking replay service status: {}", e);
            return HttpResponse::InternalServerError().json(PlaybackResponse {
                status: "error".to_string(),
                message: format!("Error checking service status: {}", e),
                current_file: None,
            });
        }
    }

    std::env::set_var("MCAP_FILE", &file_path);

    let result = Command::new("sudo")
        .arg("systemctl")
        .arg("start")
        .arg("replay")
        .status();

    match result {
        Ok(status) if status.success() => {
            info!(
                "Replay service started successfully with file: {}",
                file_path
            );
            HttpResponse::Ok().json(PlaybackResponse {
                status: "success".to_string(),
                message: "Replay service started successfully".to_string(),
                current_file: Some(params.file.clone()),
            })
        }
        Ok(status) => {
            error!("Failed to start replay service: {:?}", status);
            HttpResponse::InternalServerError().json(PlaybackResponse {
                status: "error".to_string(),
                message: "Failed to start replay service".to_string(),
                current_file: None,
            })
        }
        Err(e) => {
            error!("Error starting replay service: {}", e);
            HttpResponse::InternalServerError().json(PlaybackResponse {
                status: "error".to_string(),
                message: format!("Error starting replay service: {}", e),
                current_file: None,
            })
        }
    }
}

async fn stop_replay() -> impl Responder {
    // Stop fusion service
    let result = Command::new("sudo")
        .arg("systemctl")
        .arg("stop")
        .arg("replay")
        .status();

    match result {
        Ok(status) if status.success() => {
            info!("Replay service stopped successfully");
            HttpResponse::Ok().json(PlaybackResponse {
                status: "success".to_string(),
                message: "Replay service stopped successfully".to_string(),
                current_file: None,
            })
        }
        Ok(status) => {
            error!("Failed to stop replay service: {:?}", status);
            HttpResponse::InternalServerError().json(PlaybackResponse {
                status: "error".to_string(),
                message: "Failed to stop replay service".to_string(),
                current_file: None,
            })
        }
        Err(e) => {
            error!("Error stopping replay service: {}", e);
            HttpResponse::InternalServerError().json(PlaybackResponse {
                status: "error".to_string(),
                message: format!("Error stopping replay service: {}", e),
                current_file: None,
            })
        }
    }
}

#[derive(Deserialize)]
struct IsolateParams {
    target: String,
}

async fn isolate_system(params: web::Json<IsolateParams>) -> impl Responder {
    let target = format!("{}.target", params.target);
    debug!("Attempting to isolate system to target: {}", target);

    let result = Command::new("sudo")
        .arg("systemctl")
        .arg("isolate")
        .arg(&target)
        .status();

    match result {
        Ok(status) if status.success() => {
            info!("System successfully isolated to {}", target);
            HttpResponse::Ok().json(json!({
                "status": "success",
                "message": format!("System isolated to {}", target)
            }))
        }
        Ok(status) => {
            error!("Failed to isolate system: {:?}", status);
            HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": format!("Failed to isolate system to {}", target)
            }))
        }
        Err(e) => {
            error!("Error isolating system: {}", e);
            HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": format!("Error isolating system: {}", e)
            }))
        }
    }
}

fn extract_recording_filename(status_output: &str) -> Option<String> {
    for line in status_output.lines() {
        if line.contains("Recording to") {
            let parts: Vec<&str> = line.split("Recording to ").collect();
            if parts.len() > 1 {
                let path = parts[1].trim();
                if let Some(filename) = path.split('/').last() {
                    return Some(filename.replace("…", "").trim().to_string());
                }
            }
        }
    }
    None
}

async fn get_current_recording() -> impl Responder {
    let status = Command::new("systemctl")
        .arg("status")
        .arg("recorder")
        .output();

    match status {
        Ok(output) => {
            let status_str = String::from_utf8_lossy(&output.stdout);
            match extract_recording_filename(&status_str) {
                Some(filename) => HttpResponse::Ok().json(json!({
                    "status": "recording",
                    "filename": filename
                })),
                None => HttpResponse::Ok().json(json!({
                    "status": "not_recording",
                    "filename": null
                })),
            }
        }
        Err(e) => {
            error!("Error checking recorder status: {}", e);
            HttpResponse::InternalServerError().json(json!({
                "status": "error",
                "message": format!("Error checking recorder status: {}", e)
            }))
        }
    }
}

async fn user_mode_check_replay_status() -> impl Responder {
    let pid_file = Path::new("/var/run/replay.pid");

    if !pid_file.exists() {
        return HttpResponse::Ok().json(json!({
            "status": "not_running",
            "message": "Replay is not running"
        }));
    }

    match std::fs::read_to_string(pid_file) {
        Ok(pid_str) => {
            if let Ok(pid) = pid_str.trim().parse::<i32>() {
                let status = Command::new("ps").arg("-p").arg(pid.to_string()).output();

                match status {
                    Ok(output) => {
                        if output.status.success() {
                            HttpResponse::Ok().json(json!({
                                "status": "running",
                                "message": "Replay is running"
                            }))
                        } else {
                            // Process exists but not running, clean up PID file
                            let _ = std::fs::remove_file(pid_file);
                            HttpResponse::Ok().json(json!({
                                "status": "not_running",
                                "message": "Replay is not running"
                            }))
                        }
                    }
                    Err(e) => HttpResponse::InternalServerError().json(json!({
                        "status": "error",
                        "message": format!("Error checking process status: {:?}", e)
                    })),
                }
            } else {
                HttpResponse::InternalServerError().json(json!({
                    "status": "error",
                    "message": "Invalid PID in PID file"
                }))
            }
        }
        Err(e) => HttpResponse::InternalServerError().json(json!({
            "status": "error",
            "message": format!("Error reading PID file: {:?}", e)
        })),
    }
}

async fn check_replay_status() -> impl Responder {
    let service_status = Command::new("systemctl")
        .arg("is-active")
        .arg("replay")
        .output();

    match service_status {
        Ok(output) => {
            let status = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if status == "active" {
                HttpResponse::Ok().body("Replay is running")
            } else {
                HttpResponse::Ok().body("Replay is not running")
            }
        }
        Err(e) => HttpResponse::InternalServerError()
            .body(format!("Error checking service status: {:?}", e)),
    }
}

async fn get_all_services(params: web::Json<Value>) -> impl Responder {
    let services = match params["services"].as_array() {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect::<Vec<String>>(),
        None => Vec::new(),
    };
    let mut service_statuses = Vec::new();

    for service in services {
        let decoded_service = percent_decode(service.as_bytes())
            .decode_utf8_lossy()
            .to_string();
        let status_output = Command::new("systemctl")
            .arg("is-active")
            .arg(&decoded_service)
            .output();

        let enabled_output = Command::new("systemctl")
            .arg("is-enabled")
            .arg(&decoded_service)
            .output();

        let status = match status_output {
            Ok(output) => {
                let status_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if status_str == "active" {
                    "running".to_string()
                } else {
                    "not running".to_string()
                }
            }
            Err(_) => "unknown".to_string(),
        };

        let enabled = match enabled_output {
            Ok(output) => {
                let enabled_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                match enabled_str.as_str() {
                    "enabled" => "enabled".to_string(),
                    "disabled" => "disabled".to_string(),
                    _ => "unknown".to_string(),
                }
            }
            Err(_) => "unknown".to_string(),
        };
        service_statuses.push(json!({
            "service": decoded_service,
            "status": status,
            "enabled": enabled,
        }));
    }
    HttpResponse::Ok().json(service_statuses)
}

async fn update_service(params: web::Json<ServiceAction>) -> impl Responder {
    let service_name = &params.service;
    let action = &params.action;

    let mut command = Command::new("sudo");
    command.arg("systemctl");

    match action.as_str() {
        "start" => command.arg("start").arg(service_name),
        "stop" => command.arg("stop").arg(service_name),
        "enable" => command.arg("enable").arg(service_name),
        "disable" => command.arg("disable").arg(service_name),
        _ => return HttpResponse::BadRequest().body("Invalid action"),
    };

    match command.status() {
        Ok(status) if status.success() => HttpResponse::Ok().body(format!(
            "Service '{}' {}d successfully.",
            service_name, action
        )),
        Ok(status) => {
            let error_message = format!(
                "Failed to {} service '{}': {:?}",
                action, service_name, status
            );
            error!("{}", error_message);
            HttpResponse::InternalServerError().body(error_message)
        }
        Err(e) => {
            let error_message = format!(
                "Failed to run systemctl {} {}: {:?}",
                action, service_name, e
            );
            error!("{}", error_message);
            HttpResponse::InternalServerError().body(error_message)
        }
    }
}

async fn get_config(path: web::Path<ConfigPath>) -> impl Responder {
    let service_name = &path.service;
    let config_file_path = format!("/etc/default/{}", service_name);

    let config_content = std::fs::read_to_string(&config_file_path).unwrap_or_default();
    let mut config_map = serde_json::Map::new();

    for line in config_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let clean_key = key.trim();
            let clean_value = value.trim().replace("\"", "");
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

    HttpResponse::Ok().json(serde_json::Value::Object(config_map))
}

async fn set_config(params: web::Json<Value>) -> impl Responder {
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
    let mut updated_config = String::new();

    for line in config_content.lines() {
        let mut found = false;

        for (key, value) in &config_map {
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
            updated_config.push_str(&format!("{}\n", line)); // Preserve original line
        }
    }

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

async fn user_mode_get_config(data: web::Data<ServerContext>) -> HttpResponse {
    HttpResponse::Ok().json(WebUISettings::from(data.args.clone()))
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let handle = Handle::current();

    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    let certificate =
        openssl::x509::X509::from_pem(SERVER_PEM).expect("Failed to parse certificate");

    builder
        .set_private_key(&load_encrypted_private_key())
        .expect("Failed to set private key");
    builder
        .set_certificate(&certificate)
        .expect("Failed to set certificate");

    let hostname = hostname::get()
        .unwrap_or_else(|_| "unknown".into())
        .to_string_lossy()
        .into_owned();
    info!("To visualize navigate to https://{} ", hostname);
    let state = web::Data::new(AppState {
        process: Mutex::new(None),
    });

    let thread_state = web::Data::new(Arc::new(ThreadState {
        is_running: Mutex::new(false),
    }));
    // binding to [::] will also bind to 0.0.0.0. We try to bind both ivp6 and ipv4
    // with [::]. If that fails we will try just ivp4. If we do 0.0.0.0 first, the
    // [::] bind won't happen
    let addrs = ["[::]:443".parse().unwrap(), "0.0.0.0:443".parse().unwrap()];
    let addrs_http = ["[::]:80".parse().unwrap(), "0.0.0.0:80".parse().unwrap()];

    HttpServer::new(move || {
        let (tx, _) = channel();
        let server_ctx = ServerContext {
            args: args.clone(),
            err_stream: Arc::new(MessageStream::new(tx, Box::new(|| {}), false)),
            err_count: AtomicI64::new(0),
        };
        let handle = handle.clone();
        let thread_state_clone = thread_state.clone();

        if args.user_mode {
            App::new()
                .wrap(RedirectHttps::default())
                // .wrap(middleware::Compress::default())
                .app_data(state.clone())
                .app_data(web::Data::new(server_ctx))
                .service(
                    web::scope("")
                        .route("/", web::get().to(index))
                        .route("/settings", web::get().to(serve_settings_page))
                        .route("/check-storage", web::get().to(check_storage_availability))
                        .route("/start", web::post().to(user_mode_start))
                        .route("/stop", web::post().to(user_mode_stop))
                        .route("/delete", web::post().to(delete))
                        .route("/replay", web::post().to(start_replay))
                        .route("/replay-end", web::post().to(stop_replay))
                        .route(
                            "/replay-status",
                            web::get().to(user_mode_check_replay_status),
                        )
                        .route("/live-run", web::post().to(isolate_system))
                        .route("/download/{file:.*}", web::get().to(mcap_downloader))
                        .route(
                            "/get-upload-credentials",
                            web::get().to(get_upload_credentials),
                        )
                        .route(
                            "/recorder-status",
                            web::get().to(user_mode_check_recorder_status),
                        )
                        .route("/current-recording", web::get().to(get_current_recording))
                        .service(
                            web::resource("/ws/dropped")
                                .route(web::get().to(websocket_handler_errors)),
                        )
                        .service(
                            web::resource("/rt/detect/mask")
                                .route(web::get().to(websocket_handler_high_priority)),
                        )
                        .service(
                            web::resource("/rt/{tail:.*}")
                                .route(web::get().to(websocket_handler_low_priority)),
                        )
                        .route("/config/service/status", web::post().to(get_all_services))
                        .route("/config/services/update", web::post().to(update_service))
                        .service(
                            web::resource("/mcap/").route(web::get().to(mcap_websocket_handler)),
                        )
                        .route("/config/{service}", web::get().to(serve_config_page))
                        .route(
                            "/config/{service}/details",
                            web::get().to(user_mode_get_config),
                        )
                        .route("/config/{service}", web::post().to(set_config))
                        .route("/{file:.*}", web::get().to(custom_file_handler))
                        .route(
                            "/upload",
                            web::post().to(move |params| {
                                upload(params, handle.clone(), thread_state_clone.clone())
                            }),
                        ),
                )
        } else {
            App::new()
                .wrap(RedirectHttps::default())
                // .wrap(middleware::Compress::default())
                .app_data(state.clone())
                .app_data(web::Data::new(server_ctx))
                .service(
                    web::scope("")
                        .route("/", web::get().to(index))
                        .route("/settings", web::get().to(serve_settings_page))
                        .route("/check-storage", web::get().to(check_storage_availability))
                        .route("/start", web::post().to(start))
                        .route("/stop", web::post().to(stop))
                        .route("/delete", web::post().to(delete))
                        .route("/replay", web::post().to(start_replay))
                        .route("/replay-end", web::post().to(stop_replay))
                        .route("/replay-status", web::get().to(check_replay_status))
                        .route("/live-run", web::post().to(isolate_system))
                        .route("/download/{file:.*}", web::get().to(mcap_downloader))
                        .route(
                            "/get-upload-credentials",
                            web::get().to(get_upload_credentials),
                        )
                        .route("/recorder-status", web::get().to(check_recorder_status))
                        .route("/current-recording", web::get().to(get_current_recording))
                        .service(
                            web::resource("/ws/dropped")
                                .route(web::get().to(websocket_handler_errors)),
                        )
                        .service(
                            web::resource("/rt/detect/mask")
                                .route(web::get().to(websocket_handler_high_priority)),
                        )
                        .service(
                            web::resource("/rt/{tail:.*}")
                                .route(web::get().to(websocket_handler_low_priority)),
                        )
                        .route("/config/service/status", web::post().to(get_all_services))
                        .route("/config/services/update", web::post().to(update_service))
                        .service(
                            web::resource("/mcap/").route(web::get().to(mcap_websocket_handler)),
                        )
                        .route("/config/{service}", web::get().to(serve_config_page))
                        .route("/config/{service}/details", web::get().to(get_config))
                        .route("/config/{service}", web::post().to(set_config))
                        .route("/{file:.*}", web::get().to(custom_file_handler))
                        .route(
                            "/upload",
                            web::post().to(move |params| {
                                upload(params, handle.clone(), thread_state_clone.clone())
                            }),
                        ),
                )
        }
    })
    .bind(&addrs_http[..])?
    .bind_openssl(&addrs[..], builder)?
    .workers(8)
    .run()
    .await
}

fn map_mcap<P: AsRef<Utf8Path>>(p: P) -> res<Mmap> {
    let fd = std::fs::File::open(p.as_ref()).context("Couldn't open MCAP file")?;
    unsafe { Mmap::map(&fd) }.context("Couldn't map MCAP file")
}

fn mcap_start_time<P: AsRef<Utf8Path>>(path: P) -> res<u64> {
    let mapped = map_mcap(&path).context("Failed to map MCAP file")?;
    let summary = Summary::read(&mapped).context("Failed to read MCAP summary")?;

    if let Some(summary) = summary {
        if let Some(ref stats) = summary.stats {
            debug!("Statistics: {:?}", stats);
            let message_start_time = stats.message_start_time;
            return Ok(message_start_time);
        }
    }
    Ok(0)
}

fn read_mcap_info<P: AsRef<Utf8Path>>(path: P) -> res<(HashMap<String, TopicInfo>, f64)> {
    let file_length = 0.0;

    let mapped = map_mcap(&path)?;
    let summary = Summary::read(&mapped)?;

    let message_start_time: f64;
    let message_end_time: f64;
    let total_duration: f64;
    let mut topic_infos: HashMap<String, TopicInfo> = HashMap::new();
    let mut total_topic_duration = 0.0;
    let mut topic_count = 0;

    if let Some(summary) = summary {
        if let Some(ref stats) = summary.stats {
            debug!("Statistics: {:?}", stats);
            message_start_time = stats.message_start_time as f64;
            message_end_time = stats.message_end_time as f64;
            total_duration = (message_end_time - message_start_time) / 1_000_000_000.0;

            for (channel_id, channel_arc) in &summary.channels {
                let channel = channel_arc.as_ref();
                let topic = channel.topic.clone();

                let message_count = stats
                    .channel_message_counts
                    .get(channel_id)
                    .copied()
                    .unwrap_or(0);

                let duration = total_duration;
                let fps = if duration > 0.0 {
                    message_count as f64 / duration
                } else {
                    0.0
                };
                topic_infos.insert(
                    topic.clone(),
                    TopicInfo {
                        message_count: message_count.try_into().unwrap(),
                        average_fps: fps,
                        video_length: duration,
                    },
                );
                total_topic_duration += duration;
                topic_count += 1;
            }
        } else {
            error!("No statistics available.");
        }
    }

    let average_duration = if topic_count > 0 {
        total_topic_duration / topic_count as f64
    } else {
        file_length
    };

    Ok((topic_infos, average_duration))
}
