// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Topic liveliness for `GET /api/topics/status`.
//!
//! Zenoh offers no way to list remote publishers, and a subscriber receives
//! every sample of its key, so websrv cannot watch topics continuously without
//! pulling their full data. Instead a background task samples a set of topics
//! at start-up and then every [`REFRESH`]: it declares one subscriber per topic,
//! all at once, and undeclares each as soon as its first sample arrives or when
//! [`SAMPLE_WINDOW`] ends. A topic is available when a sample was seen within
//! [`AVAILABILITY_WINDOW`], so it stays available until it misses a full cycle.
//!
//! Samples forwarded by `/api/rt` bridges also count as sightings, and a topic
//! with a live bridge is not sampled.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::websocket::{zenoh_key_from_ws_path, WebSocketContext};

/// Period of the sampling cycle.
pub const REFRESH: Duration = Duration::from_secs(10);

/// Longest a sampling subscriber waits for a first sample.
pub const SAMPLE_WINDOW: Duration = Duration::from_secs(2);

/// Slack added to one cycle before a topic is reported unavailable.
pub const AVAILABILITY_MARGIN: Duration = Duration::from_secs(3);

/// A topic is available when its last sample is at most this old.
pub const AVAILABILITY_WINDOW: Duration = Duration::from_secs(
    REFRESH.as_secs() + SAMPLE_WINDOW.as_secs() + AVAILABILITY_MARGIN.as_secs(),
);

/// A requested topic outside [`BUILTIN_TOPICS`] stops being sampled this long after its last request.
pub const REQUEST_EXPIRY: Duration = Duration::from_secs(60);

/// Maximum number of topics in one request.
pub const MAX_TOPICS_PER_REQUEST: usize = 16;

/// Maximum length in bytes of one topic name.
pub const MAX_TOPIC_LEN: usize = 256;

/// Maximum number of sampled topics, built-in and requested.
pub const MAX_SAMPLED_TOPICS: usize = 64;

/// Application keys sampled from start-up: the `/api/rt` topics the web UI
/// subscribes to. `camera/frame` (DMA handles) and `radar/cube` (large) are
/// left out; clients can still request them. `lidar/points` and
/// `camera/h264` are large but carry the overlays and video the UI gates on,
/// and one sample per cycle is a small fraction of their stream.
pub const BUILTIN_TOPICS: &[&str] = &[
    "camera/h264",
    "camera/h264/tl",
    "camera/h264/tr",
    "camera/h264/bl",
    "camera/h264/br",
    "camera/info",
    "model/output",
    "model/info",
    "lidar/points",
    "lidar/clusters",
    "radar/targets",
    "radar/clusters",
    "fusion/lidar",
    "fusion/radar",
    "imu",
    "gps",
    "tf_static",
];

/// Characters that would let a client subscribe to wildcards or the admin space.
const FORBIDDEN_CHARS: [char; 5] = ['*', '$', '@', '?', '#'];

/// Timing and capacity of [`TopicLiveness`]; the default uses the module constants.
#[derive(Debug, Clone, Copy)]
pub struct LivenessConfig {
    pub refresh: Duration,
    pub sample_window: Duration,
    pub availability_window: Duration,
    pub request_expiry: Duration,
    pub max_topics: usize,
}

impl Default for LivenessConfig {
    fn default() -> Self {
        Self {
            refresh: REFRESH,
            sample_window: SAMPLE_WINDOW,
            availability_window: AVAILABILITY_WINDOW,
            request_expiry: REQUEST_EXPIRY,
            max_topics: MAX_SAMPLED_TOPICS,
        }
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

// ============================================================================
// Response Types
// ============================================================================

/// Liveliness of one topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicStatus {
    /// `true` when `last_seen_ms` is known and within the availability window.
    pub available: bool,
    /// Milliseconds since the last sample seen, or `None` if none has been seen since start-up.
    pub last_seen_ms: Option<u64>,
}

impl TopicStatus {
    /// Applies the availability rule to the time since the last sample.
    pub fn from_last_seen(age: Option<Duration>, window: Duration) -> Self {
        Self {
            available: age.is_some_and(|a| millis(a) <= millis(window)),
            last_seen_ms: age.map(millis),
        }
    }
}

/// Body of a successful `GET /api/topics/status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicStatusResponse {
    /// Sampling cycle period in milliseconds.
    pub refresh_ms: u64,
    /// Milliseconds since the last completed cycle started, or `None` before the first completes.
    pub last_cycle_ms: Option<u64>,
    /// Keyed by the requested names, or by key when no topics were requested.
    pub topics: BTreeMap<String, TopicStatus>,
}

// ============================================================================
// Query Parsing
// ============================================================================

/// Query parameters for `GET /api/topics/status`.
#[derive(Debug, Deserialize)]
pub struct TopicStatusParams {
    /// Comma-separated topic names, as used in `/api/rt/<topic>`.
    pub topics: Option<String>,
}

/// One requested topic: the name as the client sent it and its Zenoh key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedTopic {
    pub name: String,
    pub key: String,
}

/// Parses and validates the `topics` query value.
///
/// Blank entries and repeated names are ignored. Fails when no topic remains,
/// when there are more than [`MAX_TOPICS_PER_REQUEST`], or when a topic is
/// longer than [`MAX_TOPIC_LEN`], contains a wildcard or admin-space
/// character, or is not a valid Zenoh key expression.
pub fn parse_topics(raw: Option<&str>) -> Result<Vec<RequestedTopic>, String> {
    let raw = raw.ok_or_else(|| "missing `topics` query parameter".to_string())?;
    let mut topics: Vec<RequestedTopic> = Vec::new();
    for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if name.len() > MAX_TOPIC_LEN {
            return Err(format!("topic exceeds {MAX_TOPIC_LEN} bytes"));
        }
        if let Some(c) = name.chars().find(|c| FORBIDDEN_CHARS.contains(c)) {
            return Err(format!("topic {name:?} contains forbidden character {c:?}"));
        }
        let key = zenoh_key_from_ws_path(name);
        if key.is_empty() {
            return Err(format!("topic {name:?} is empty"));
        }
        if let Err(e) = zenoh::key_expr::KeyExpr::try_from(key.as_str()) {
            return Err(format!("topic {name:?} is not a valid key: {e}"));
        }
        if topics.iter().any(|t| t.name == name) {
            continue;
        }
        topics.push(RequestedTopic {
            name: name.to_string(),
            key,
        });
        if topics.len() > MAX_TOPICS_PER_REQUEST {
            return Err(format!(
                "at most {MAX_TOPICS_PER_REQUEST} topics per request"
            ));
        }
    }
    if topics.is_empty() {
        return Err("`topics` is empty".to_string());
    }
    Ok(topics)
}

// ============================================================================
// Observers
// ============================================================================

/// Last sighting of one topic, writable from Zenoh callbacks without locking.
pub struct TopicObserver {
    epoch: Instant,
    /// Nanoseconds after `epoch` plus one; zero means never seen.
    nanos_plus_one: AtomicU64,
    bridges: AtomicUsize,
    sampled: AtomicBool,
}

impl TopicObserver {
    /// Creates an observer with no sighting. Sightings before `epoch` record as `epoch`.
    pub fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            nanos_plus_one: AtomicU64::new(0),
            bridges: AtomicUsize::new(0),
            sampled: AtomicBool::new(false),
        }
    }

    /// Records a sighting; one earlier than the latest is ignored.
    pub fn observe(&self, at: Instant) {
        let nanos = u64::try_from(at.saturating_duration_since(self.epoch).as_nanos())
            .unwrap_or(u64::MAX - 1)
            .min(u64::MAX - 1);
        self.nanos_plus_one.fetch_max(nanos + 1, Ordering::Relaxed);
    }

    /// Returns the latest sighting.
    pub fn last_seen(&self) -> Option<Instant> {
        match self.nanos_plus_one.load(Ordering::Relaxed) {
            0 => None,
            n => Some(self.epoch + Duration::from_nanos(n - 1)),
        }
    }

    /// Records that a sampling cycle has covered this topic.
    pub fn mark_sampled(&self) {
        self.sampled.store(true, Ordering::Relaxed);
    }

    fn is_sampled(&self) -> bool {
        self.sampled.load(Ordering::Relaxed)
    }

    fn is_bridged(&self) -> bool {
        self.bridges.load(Ordering::Relaxed) > 0
    }
}

/// Held by an `/api/rt` bridge for a sampled topic: reports each forwarded
/// sample, and while held the topic is not sampled.
pub struct BridgeObserver(Arc<TopicObserver>);

impl BridgeObserver {
    fn new(observer: Arc<TopicObserver>) -> Self {
        observer.bridges.fetch_add(1, Ordering::Relaxed);
        Self(observer)
    }

    /// Records a sample forwarded by the bridge now.
    pub fn observe(&self) {
        self.0.observe(Instant::now());
    }
}

impl Drop for BridgeObserver {
    fn drop(&mut self) {
        self.0.bridges.fetch_sub(1, Ordering::Relaxed);
    }
}

// ============================================================================
// Topic Set
// ============================================================================

struct Entry {
    observer: Arc<TopicObserver>,
    /// `None` for built-in topics, which never expire.
    last_requested: Option<Instant>,
}

/// The sampled topics and their observers. Methods take the current time so
/// tests can drive them with arbitrary instants.
pub struct TopicSet {
    entries: HashMap<String, Entry>,
    epoch: Instant,
    config: LivenessConfig,
    last_cycle: Option<Instant>,
}

impl TopicSet {
    /// Creates the set with the built-in topics.
    pub fn new(builtin: &[&str], epoch: Instant, config: LivenessConfig) -> Self {
        let entries = builtin
            .iter()
            .map(|key| {
                (
                    (*key).to_string(),
                    Entry {
                        observer: Arc::new(TopicObserver::new(epoch)),
                        last_requested: None,
                    },
                )
            })
            .collect();
        Self {
            entries,
            epoch,
            config,
            last_cycle: None,
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Sampled keys in sorted order.
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.entries.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn observer(&self, key: &str) -> Option<Arc<TopicObserver>> {
        self.entries.get(key).map(|e| e.observer.clone())
    }

    /// Records a client request for `key`, adding it to the set if absent.
    /// When the set is full the least recently requested topic is evicted;
    /// built-in topics are never evicted. Returns `true` if `key` was added.
    pub fn request(&mut self, key: &str, now: Instant) -> bool {
        if let Some(entry) = self.entries.get_mut(key) {
            if let Some(t) = entry.last_requested.as_mut() {
                *t = (*t).max(now);
            }
            return false;
        }
        if self.entries.len() >= self.config.max_topics {
            let oldest = self
                .entries
                .iter()
                .filter_map(|(k, e)| e.last_requested.map(|t| (t, k)))
                .min()
                .map(|(_, k)| k.clone());
            match oldest {
                Some(k) => {
                    debug!("Evicting requested topic {k}");
                    self.entries.remove(&k);
                }
                None => return false,
            }
        }
        self.entries.insert(
            key.to_string(),
            Entry {
                observer: Arc::new(TopicObserver::new(self.epoch)),
                last_requested: Some(now),
            },
        );
        true
    }

    /// Removes requested topics not requested within the expiry.
    pub fn expire(&mut self, now: Instant) {
        let expiry = self.config.request_expiry;
        self.entries.retain(|key, e| {
            let keep = e
                .last_requested
                .is_none_or(|t| now.saturating_duration_since(t) <= expiry);
            if !keep {
                debug!("Requested topic {key} expired");
            }
            keep
        });
    }

    /// Topics to sample: all without a live bridge, or with `pending_only`
    /// just those no cycle has covered yet.
    pub fn due(&self, pending_only: bool) -> Vec<(String, Arc<TopicObserver>)> {
        self.entries
            .iter()
            .filter(|(_, e)| !e.observer.is_bridged())
            .filter(|(_, e)| !pending_only || !e.observer.is_sampled())
            .map(|(k, e)| (k.clone(), e.observer.clone()))
            .collect()
    }

    /// Returns a bridge handle for `key` if it is sampled.
    pub fn bridge(&self, key: &str) -> Option<BridgeObserver> {
        self.entries
            .get(key)
            .map(|e| BridgeObserver::new(e.observer.clone()))
    }

    /// Status of `key`; unknown keys report never seen.
    pub fn status(&self, key: &str, now: Instant) -> TopicStatus {
        let age = self
            .entries
            .get(key)
            .and_then(|e| e.observer.last_seen())
            .map(|t| now.saturating_duration_since(t));
        TopicStatus::from_last_seen(age, self.config.availability_window)
    }

    /// Records a completed cycle that started at `started`.
    pub fn cycle_completed(&mut self, started: Instant) {
        self.last_cycle = Some(started);
    }

    /// Time since the last completed cycle started.
    pub fn last_cycle_age(&self, now: Instant) -> Option<Duration> {
        self.last_cycle.map(|t| now.saturating_duration_since(t))
    }
}

// ============================================================================
// Sampling
// ============================================================================

/// Shared topic set plus the background sampling task.
pub struct TopicLiveness {
    set: Mutex<TopicSet>,
    config: LivenessConfig,
    /// Wakes the sampling task to cover newly requested topics.
    wake: Notify,
    active_samplers: Arc<AtomicUsize>,
}

impl TopicLiveness {
    pub fn new(builtin: &[&str], config: LivenessConfig) -> Self {
        Self {
            set: Mutex::new(TopicSet::new(builtin, Instant::now(), config)),
            config,
            wake: Notify::new(),
            active_samplers: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, TopicSet> {
        self.set.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of sampling subscribers currently declared.
    pub fn active_samplers(&self) -> usize {
        self.active_samplers.load(Ordering::Relaxed)
    }

    /// Returns a bridge handle for `key` if it is sampled.
    pub fn bridge(&self, key: &str) -> Option<BridgeObserver> {
        self.lock().bridge(key)
    }

    /// Builds the response for `requested`, or for every sampled topic when
    /// `None`. Requested topics not yet sampled are added and the sampling
    /// task is woken to cover them.
    pub fn status(&self, requested: Option<&[RequestedTopic]>) -> TopicStatusResponse {
        let now = Instant::now();
        let mut set = self.lock();
        set.expire(now);
        let mut topics = BTreeMap::new();
        match requested {
            None => {
                for key in set.keys() {
                    let st = set.status(&key, now);
                    topics.insert(key, st);
                }
            }
            Some(requested) => {
                let mut added = false;
                for t in requested {
                    added |= set.request(&t.key, now);
                    topics.insert(t.name.clone(), set.status(&t.key, now));
                }
                if added {
                    self.wake.notify_one();
                }
            }
        }
        TopicStatusResponse {
            refresh_ms: millis(self.config.refresh),
            last_cycle_ms: set.last_cycle_age(now).map(millis),
            topics,
        }
    }

    /// Samples the due topics concurrently, each until its first sample or the window ends.
    async fn sample(&self, session: &zenoh::Session, pending_only: bool) {
        let due = self.lock().due(pending_only);
        if due.is_empty() {
            return;
        }
        debug!("Sampling {} topics", due.len());
        let window = self.config.sample_window;
        futures::future::join_all(due.into_iter().map(|(key, observer)| {
            sample_one(session, key, observer, window, self.active_samplers.clone())
        }))
        .await;
    }

    /// Runs one full cycle and records its start.
    pub async fn run_cycle(&self, session: &zenoh::Session) {
        let started = Instant::now();
        self.lock().expire(started);
        self.sample(session, false).await;
        self.lock().cycle_completed(started);
    }

    /// Spawns the sampling task: a cycle at once, then every `refresh`, with
    /// newly requested topics sampled between cycles. Cycles never overlap.
    pub fn spawn(
        self: &Arc<Self>,
        session: zenoh::Session,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                let started = tokio::time::Instant::now();
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = this.run_cycle(&session) => {}
                }
                let next = started + this.config.refresh;
                loop {
                    tokio::select! {
                        _ = shutdown.cancelled() => return,
                        _ = tokio::time::sleep_until(next) => break,
                        _ = this.wake.notified() => {
                            tokio::select! {
                                _ = shutdown.cancelled() => return,
                                _ = this.sample(&session, true) => {}
                            }
                        }
                    }
                }
            }
        })
    }
}

/// Declares a subscriber on `key` that records the arrival time of samples,
/// and undeclares it at the first sample or when `window` ends.
async fn sample_one(
    session: &zenoh::Session,
    key: String,
    observer: Arc<TopicObserver>,
    window: Duration,
    active: Arc<AtomicUsize>,
) {
    let first = Arc::new(Notify::new());
    let fired = Arc::new(AtomicBool::new(false));
    let callback = {
        let observer = observer.clone();
        let first = first.clone();
        move |_sample: zenoh::sample::Sample| {
            observer.observe(Instant::now());
            if !fired.swap(true, Ordering::Relaxed) {
                first.notify_one();
            }
        }
    };
    let subscriber = match session.declare_subscriber(&key).callback(callback).await {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to sample topic {key}: {e}");
            return;
        }
    };
    active.fetch_add(1, Ordering::Relaxed);
    let _ = tokio::time::timeout(window, first.notified()).await;
    if let Err(e) = subscriber.undeclare().await {
        warn!("Failed to undeclare sampler for {key}: {e}");
    }
    active.fetch_sub(1, Ordering::Relaxed);
    observer.mark_sampled();
}

// ============================================================================
// HTTP Handler
// ============================================================================

/// Application state needed by the topic status handler.
pub trait TopicStatusContext: WebSocketContext {
    fn topic_liveness(&self) -> &Arc<TopicLiveness>;
}

/// `GET /api/topics/status[?topics=a/b,c/d]` — reports whether topics have
/// been seen within [`AVAILABILITY_WINDOW`]. Without `topics`, every sampled
/// topic is listed. Invalid queries return `400` with `{"error": "..."}`.
pub async fn topic_status_handler<T: TopicStatusContext>(
    State(ctx): State<Arc<T>>,
    params: Result<Query<TopicStatusParams>, QueryRejection>,
) -> Response {
    let requested = params
        .map_err(|e| e.body_text())
        .and_then(|Query(p)| p.topics.map(|raw| parse_topics(Some(&raw))).transpose());
    match requested {
        Ok(requested) => Json(ctx.topic_liveness().status(requested.as_deref())).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LivenessConfig {
        LivenessConfig {
            refresh: Duration::from_secs(10),
            sample_window: Duration::from_secs(2),
            availability_window: Duration::from_secs(15),
            request_expiry: Duration::from_secs(60),
            max_topics: 4,
        }
    }

    fn names(topics: &[RequestedTopic]) -> Vec<&str> {
        topics.iter().map(|t| t.name.as_str()).collect()
    }

    fn due_keys(set: &TopicSet, pending_only: bool) -> Vec<String> {
        let mut keys: Vec<String> = set.due(pending_only).into_iter().map(|(k, _)| k).collect();
        keys.sort();
        keys
    }

    #[test]
    fn parse_accepts_comma_separated_keys() {
        let topics = parse_topics(Some("radar/targets,lidar/points")).unwrap();
        assert_eq!(names(&topics), ["radar/targets", "lidar/points"]);
        assert_eq!(topics[0].key, "radar/targets");
        assert_eq!(topics[1].key, "lidar/points");
    }

    #[test]
    fn parse_maps_names_like_rt_paths() {
        let topics = parse_topics(Some("/camera/h264/")).unwrap();
        assert_eq!(topics[0].name, "/camera/h264/");
        assert_eq!(topics[0].key, "camera/h264");
    }

    #[test]
    fn parse_ignores_blank_entries_and_duplicates() {
        let topics = parse_topics(Some(" radar/targets , ,radar/targets,")).unwrap();
        assert_eq!(names(&topics), ["radar/targets"]);
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_topics(Some("")).is_err());
        assert!(parse_topics(Some(" , ,")).is_err());
        assert!(parse_topics(Some("/")).is_err());
    }

    #[test]
    fn parse_rejects_too_many_topics() {
        let ok: Vec<String> = (0..MAX_TOPICS_PER_REQUEST)
            .map(|i| format!("t{i}"))
            .collect();
        assert!(parse_topics(Some(&ok.join(","))).is_ok());
        let too_many: Vec<String> = (0..=MAX_TOPICS_PER_REQUEST)
            .map(|i| format!("t{i}"))
            .collect();
        assert!(parse_topics(Some(&too_many.join(","))).is_err());
    }

    #[test]
    fn parse_rejects_long_topics() {
        assert!(parse_topics(Some(&"a".repeat(MAX_TOPIC_LEN))).is_ok());
        assert!(parse_topics(Some(&"a".repeat(MAX_TOPIC_LEN + 1))).is_err());
    }

    #[test]
    fn parse_rejects_wildcards_and_admin_space() {
        for bad in [
            "**",
            "radar/*",
            "radar/$*x",
            "@/session",
            "radar/@x",
            "a?b",
            "a#b",
        ] {
            assert!(parse_topics(Some(bad)).is_err(), "{bad} must be rejected");
        }
        assert!(parse_topics(Some("radar/targets,**")).is_err());
        assert!(parse_topics(Some("radar//targets")).is_err());
    }

    #[test]
    fn availability_window_is_refresh_plus_sample_window_plus_margin() {
        assert_eq!(REFRESH, Duration::from_secs(10));
        assert_eq!(SAMPLE_WINDOW, Duration::from_secs(2));
        assert_eq!(
            AVAILABILITY_WINDOW,
            REFRESH + SAMPLE_WINDOW + AVAILABILITY_MARGIN
        );
        assert_eq!(AVAILABILITY_WINDOW, Duration::from_secs(15));
        assert_eq!(
            LivenessConfig::default().availability_window,
            AVAILABILITY_WINDOW
        );
    }

    #[test]
    fn builtin_topics_are_valid_and_exclude_heavy_defaults() {
        for key in BUILTIN_TOPICS {
            let parsed = parse_topics(Some(key)).unwrap();
            assert_eq!(parsed[0].key, *key);
        }
        assert!(BUILTIN_TOPICS.contains(&"radar/targets"));
        assert!(BUILTIN_TOPICS.contains(&"lidar/points"));
        assert!(!BUILTIN_TOPICS.contains(&"camera/frame"));
        assert!(!BUILTIN_TOPICS.contains(&"radar/cube"));
        assert!(BUILTIN_TOPICS.len() < MAX_SAMPLED_TOPICS);
    }

    #[test]
    fn availability_rule() {
        let window = Duration::from_secs(15);
        assert_eq!(
            TopicStatus::from_last_seen(None, window),
            TopicStatus {
                available: false,
                last_seen_ms: None
            }
        );
        assert_eq!(
            TopicStatus::from_last_seen(Some(Duration::from_micros(3_150_900)), window),
            TopicStatus {
                available: true,
                last_seen_ms: Some(3150)
            }
        );
        assert_eq!(
            TopicStatus::from_last_seen(Some(window), window),
            TopicStatus {
                available: true,
                last_seen_ms: Some(15_000)
            }
        );
        assert_eq!(
            TopicStatus::from_last_seen(Some(Duration::from_millis(15_001)), window),
            TopicStatus {
                available: false,
                last_seen_ms: Some(15_001)
            }
        );
    }

    #[test]
    fn observer_keeps_latest_arrival() {
        let epoch = Instant::now();
        let obs = TopicObserver::new(epoch);
        assert_eq!(obs.last_seen(), None);
        obs.observe(epoch + Duration::from_millis(250));
        obs.observe(epoch + Duration::from_millis(100));
        assert_eq!(obs.last_seen(), Some(epoch + Duration::from_millis(250)));
    }

    #[test]
    fn status_reports_time_since_last_seen() {
        let t0 = Instant::now();
        let set = TopicSet::new(&["radar/targets", "lidar/points"], t0, cfg());
        let radar = set.observer("radar/targets").unwrap();
        radar.observe(t0 + Duration::from_millis(100));
        let now = t0 + Duration::from_millis(3250);
        assert_eq!(
            set.status("radar/targets", now),
            TopicStatus {
                available: true,
                last_seen_ms: Some(3150)
            }
        );
        assert_eq!(
            set.status("lidar/points", now),
            TopicStatus {
                available: false,
                last_seen_ms: None
            }
        );
        assert_eq!(
            set.status("unknown/key", now),
            TopicStatus {
                available: false,
                last_seen_ms: None
            }
        );
        let stale = t0 + Duration::from_millis(100) + Duration::from_millis(15_001);
        assert!(!set.status("radar/targets", stale).available);
    }

    #[test]
    fn last_cycle_age_is_none_until_a_cycle_completes() {
        let t0 = Instant::now();
        let mut set = TopicSet::new(&["a"], t0, cfg());
        assert_eq!(set.last_cycle_age(t0), None);
        set.cycle_completed(t0 + Duration::from_secs(1));
        assert_eq!(
            set.last_cycle_age(t0 + Duration::from_millis(4120)),
            Some(Duration::from_millis(3120))
        );
    }

    #[test]
    fn requested_unknown_topic_is_added_then_expires() {
        let t0 = Instant::now();
        let mut set = TopicSet::new(&["builtin"], t0, cfg());
        assert!(set.request("extra", t0));
        assert!(!set.request("extra", t0 + Duration::from_secs(30)));
        assert!(!set.request("builtin", t0));
        assert_eq!(set.keys(), ["builtin", "extra"]);

        set.expire(t0 + Duration::from_secs(90));
        assert!(set.contains("extra"));
        set.expire(t0 + Duration::from_secs(90) + Duration::from_millis(1));
        assert!(!set.contains("extra"));
        assert!(set.contains("builtin"), "built-in topics never expire");
        set.expire(t0 + Duration::from_secs(3600));
        assert!(set.contains("builtin"));
    }

    #[test]
    fn requests_beyond_capacity_evict_least_recently_requested() {
        let t0 = Instant::now();
        let mut set = TopicSet::new(&["b1", "b2"], t0, cfg());
        assert!(set.request("r1", t0));
        assert!(set.request("r2", t0 + Duration::from_secs(1)));
        set.request("r1", t0 + Duration::from_secs(2));
        assert!(set.request("r3", t0 + Duration::from_secs(3)));
        assert_eq!(set.keys(), ["b1", "b2", "r1", "r3"]);
    }

    #[test]
    fn requests_never_evict_builtin_topics() {
        let t0 = Instant::now();
        let mut set = TopicSet::new(&["b1", "b2", "b3", "b4"], t0, cfg());
        assert!(!set.request("r1", t0));
        assert!(!set.contains("r1"));
        assert_eq!(set.keys(), ["b1", "b2", "b3", "b4"]);
    }

    #[test]
    fn due_skips_bridged_topics_and_pending_only_selects_unsampled() {
        let t0 = Instant::now();
        let mut set = TopicSet::new(&["a", "b", "c"], t0, cfg());
        assert_eq!(due_keys(&set, false), ["a", "b", "c"]);
        assert_eq!(due_keys(&set, true), ["a", "b", "c"]);

        for (_, obs) in set.due(false) {
            obs.mark_sampled();
        }
        assert_eq!(due_keys(&set, true), Vec::<String>::new());
        set.request("d", t0);
        assert_eq!(due_keys(&set, true), ["d"]);

        let bridge = set.bridge("b").expect("b is sampled");
        assert_eq!(due_keys(&set, false), ["a", "c", "d"]);
        drop(bridge);
        assert_eq!(due_keys(&set, false), ["a", "b", "c", "d"]);
        assert!(set.bridge("not/sampled").is_none());
    }

    #[test]
    fn bridge_observations_update_last_seen() {
        let t0 = Instant::now();
        let set = TopicSet::new(&["lidar/points"], t0, cfg());
        let bridge = set.bridge("lidar/points").unwrap();
        bridge.observe();
        let st = set.status("lidar/points", Instant::now());
        assert!(st.available);
        assert!(st.last_seen_ms.unwrap() < 1000);
    }

    #[test]
    fn response_serializes_to_contract_shape() {
        let mut topics = BTreeMap::new();
        topics.insert(
            "radar/targets".to_string(),
            TopicStatus {
                available: true,
                last_seen_ms: Some(3150),
            },
        );
        topics.insert(
            "lidar/points".to_string(),
            TopicStatus {
                available: false,
                last_seen_ms: None,
            },
        );
        let value = serde_json::to_value(TopicStatusResponse {
            refresh_ms: 10_000,
            last_cycle_ms: Some(3120),
            topics,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "refresh_ms": 10000,
                "last_cycle_ms": 3120,
                "topics": {
                    "radar/targets": { "available": true, "last_seen_ms": 3150 },
                    "lidar/points": { "available": false, "last_seen_ms": null }
                }
            })
        );
        let before_first_cycle = serde_json::to_value(TopicStatusResponse {
            refresh_ms: 10_000,
            last_cycle_ms: None,
            topics: BTreeMap::new(),
        })
        .unwrap();
        assert_eq!(before_first_cycle["last_cycle_ms"], serde_json::Value::Null);
    }
}
