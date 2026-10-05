// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Topic availability for `GET /api/topics/status`.
//!
//! Zenoh offers no way to list remote publishers, so a topic counts as
//! available when samples have arrived on it recently. The first query for a
//! topic declares a subscriber whose callback only records the arrival time;
//! payloads are never read or forwarded. Watches no client has asked about for
//! [`WATCH_IDLE_EXPIRY`] are dropped by a periodic sweep, which undeclares the
//! subscriber.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::websocket::{zenoh_key_from_ws_path, WebSocketContext};

/// A topic is available when its last sample is at most this old.
pub const AVAILABILITY_THRESHOLD: Duration = Duration::from_millis(3000);

/// A watch not queried for longer than this is dropped and its subscriber undeclared.
pub const WATCH_IDLE_EXPIRY: Duration = Duration::from_secs(60);

/// How often the background task drops idle watches.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(10);

/// Maximum number of topics in one request.
pub const MAX_TOPICS_PER_REQUEST: usize = 16;

/// Maximum length in bytes of one topic name.
pub const MAX_TOPIC_LEN: usize = 256;

/// Maximum number of topics watched at once; the least recently queried is evicted beyond this.
pub const MAX_WATCHED_TOPICS: usize = 64;

/// Characters that would let a client subscribe to wildcards or the admin space.
const FORBIDDEN_CHARS: [char; 5] = ['*', '$', '@', '?', '#'];

// ============================================================================
// Response Types
// ============================================================================

/// Availability of one topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicStatus {
    /// `true` when `age_ms` is known and at most [`AVAILABILITY_THRESHOLD`].
    pub available: bool,
    /// Milliseconds since the last sample, or `None` if none has been seen
    /// since websrv began watching the topic.
    pub age_ms: Option<u64>,
}

impl TopicStatus {
    /// Applies the availability rule to the time since the last sample.
    pub fn from_age(age: Option<Duration>) -> Self {
        let age_ms = age.map(|a| u64::try_from(a.as_millis()).unwrap_or(u64::MAX));
        let threshold_ms = AVAILABILITY_THRESHOLD.as_millis() as u64;
        Self {
            available: age_ms.is_some_and(|ms| ms <= threshold_ms),
            age_ms,
        }
    }
}

/// Body of a successful `GET /api/topics/status`, keyed by the requested topic names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicStatusResponse {
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
// Sample Clock
// ============================================================================

/// Arrival time of the latest sample, writable from a Zenoh callback without locking.
pub struct SampleClock {
    epoch: Instant,
    /// Nanoseconds after `epoch` plus one; zero means no sample yet.
    nanos_plus_one: AtomicU64,
}

impl SampleClock {
    /// Creates a clock with no sample. Arrivals before `epoch` record as `epoch`.
    pub fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            nanos_plus_one: AtomicU64::new(0),
        }
    }

    /// Records an arrival; an earlier arrival than the latest one is ignored.
    pub fn record(&self, at: Instant) {
        let nanos = u64::try_from(at.saturating_duration_since(self.epoch).as_nanos())
            .unwrap_or(u64::MAX - 1)
            .min(u64::MAX - 1);
        self.nanos_plus_one.fetch_max(nanos + 1, Ordering::Relaxed);
    }

    /// Returns the latest recorded arrival.
    pub fn last(&self) -> Option<Instant> {
        match self.nanos_plus_one.load(Ordering::Relaxed) {
            0 => None,
            n => Some(self.epoch + Duration::from_nanos(n - 1)),
        }
    }
}

// ============================================================================
// Watch Registry
// ============================================================================

struct Watch<H> {
    clock: Arc<SampleClock>,
    last_queried: Instant,
    handle: H,
}

/// Watched keys with their sample clocks, generic over the subscriber handle.
///
/// Methods that remove watches return the removed handles so the caller can
/// drop them, which undeclares a Zenoh subscriber, after releasing any lock.
pub struct WatchRegistry<H> {
    watches: HashMap<String, Watch<H>>,
}

impl<H> Default for WatchRegistry<H> {
    fn default() -> Self {
        Self {
            watches: HashMap::new(),
        }
    }
}

impl<H> WatchRegistry<H> {
    pub fn len(&self) -> usize {
        self.watches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.watches.is_empty()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.watches.contains_key(key)
    }

    /// Marks `key` as queried at `now` and returns the time since its last
    /// sample. Returns `None` when `key` is not watched.
    pub fn query(&mut self, key: &str, now: Instant) -> Option<Option<Duration>> {
        let watch = self.watches.get_mut(key)?;
        watch.last_queried = watch.last_queried.max(now);
        Some(watch.clock.last().map(|t| now.saturating_duration_since(t)))
    }

    /// Starts watching `key`, evicting the least recently queried watches to
    /// stay within [`MAX_WATCHED_TOPICS`]. If `key` is already watched, the
    /// existing watch is kept and `handle` is returned for dropping.
    pub fn insert(
        &mut self,
        key: String,
        clock: Arc<SampleClock>,
        handle: H,
        now: Instant,
    ) -> Vec<H> {
        if self.watches.contains_key(&key) {
            return vec![handle];
        }
        let mut evicted = Vec::new();
        while self.watches.len() >= MAX_WATCHED_TOPICS {
            let Some(oldest) = self
                .watches
                .iter()
                .min_by_key(|(_, w)| w.last_queried)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(w) = self.watches.remove(&oldest) {
                debug!("Evicting topic watch {oldest}");
                evicted.push(w.handle);
            }
        }
        self.watches.insert(
            key,
            Watch {
                clock,
                last_queried: now,
                handle,
            },
        );
        evicted
    }

    /// Removes watches not queried for longer than [`WATCH_IDLE_EXPIRY`].
    pub fn sweep(&mut self, now: Instant) -> Vec<H> {
        let expired: Vec<String> = self
            .watches
            .iter()
            .filter(|(_, w)| now.saturating_duration_since(w.last_queried) > WATCH_IDLE_EXPIRY)
            .map(|(k, _)| k.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|k| {
                debug!("Dropping idle topic watch {k}");
                self.watches.remove(&k).map(|w| w.handle)
            })
            .collect()
    }

    /// Removes all watches.
    pub fn take_all(&mut self) -> Vec<H> {
        self.watches.drain().map(|(_, w)| w.handle).collect()
    }
}

// ============================================================================
// Zenoh-backed Topic Watches
// ============================================================================

/// Shared registry of Zenoh subscribers watching topic availability.
#[derive(Default)]
pub struct TopicWatches {
    registry: Mutex<WatchRegistry<zenoh::pubsub::Subscriber<()>>>,
}

impl TopicWatches {
    fn lock(&self) -> MutexGuard<'_, WatchRegistry<zenoh::pubsub::Subscriber<()>>> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Number of watched keys.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Drops watches idle for longer than [`WATCH_IDLE_EXPIRY`] as of `now`.
    pub fn sweep(&self, now: Instant) {
        let expired = self.lock().sweep(now);
        drop(expired);
    }

    /// Drops every watch, undeclaring all subscribers.
    pub fn clear(&self) {
        let all = self.lock().take_all();
        drop(all);
    }

    /// Reports the status of `topics`, starting a watch for each key not yet
    /// watched. Newly watched topics report `available: false, age_ms: None`.
    pub async fn status(
        &self,
        session: &zenoh::Session,
        topics: &[RequestedTopic],
    ) -> TopicStatusResponse {
        let now = Instant::now();
        let mut statuses = BTreeMap::new();
        let mut missing: Vec<&str> = Vec::new();
        {
            let mut registry = self.lock();
            for topic in topics {
                let age = registry.query(&topic.key, now);
                if age.is_none() && !missing.contains(&topic.key.as_str()) {
                    missing.push(&topic.key);
                }
                statuses.insert(topic.name.clone(), TopicStatus::from_age(age.flatten()));
            }
        }

        for key in missing {
            let clock = Arc::new(SampleClock::new(Instant::now()));
            let callback_clock = clock.clone();
            match session
                .declare_subscriber(key)
                .callback(move |_sample| callback_clock.record(Instant::now()))
                .await
            {
                Ok(subscriber) => {
                    debug!("Watching topic {key}");
                    let dropped =
                        self.lock()
                            .insert(key.to_string(), clock, subscriber, Instant::now());
                    drop(dropped);
                }
                Err(e) => warn!("Failed to watch topic {key}: {e}"),
            }
        }

        TopicStatusResponse { topics: statuses }
    }

    /// Spawns a task that drops idle watches every [`SWEEP_INTERVAL`] until `shutdown` is cancelled.
    ///
    /// A periodic sweep rather than cleanup on request: once clients stop
    /// polling no request arrives to trigger cleanup, and an idle subscriber
    /// on a high-rate topic keeps pulling every sample across the network.
    pub fn spawn_sweeper(
        self: &Arc<Self>,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let watches = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(SWEEP_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = interval.tick() => watches.sweep(Instant::now()),
                }
            }
            watches.clear();
        })
    }
}

// ============================================================================
// HTTP Handler
// ============================================================================

/// Application state needed by the topic status handler.
pub trait TopicStatusContext: WebSocketContext {
    fn topic_watches(&self) -> &Arc<TopicWatches>;
}

/// `GET /api/topics/status?topics=a/b,c/d` — reports whether each topic has
/// published within [`AVAILABILITY_THRESHOLD`]. Invalid queries return `400`
/// with `{"error": "..."}`.
pub async fn topic_status_handler<T: TopicStatusContext>(
    State(ctx): State<Arc<T>>,
    params: Result<Query<TopicStatusParams>, QueryRejection>,
) -> Response {
    let topics = params
        .map_err(|e| e.body_text())
        .and_then(|Query(p)| parse_topics(p.topics.as_deref()));
    match topics {
        Ok(topics) => Json(
            ctx.topic_watches()
                .status(ctx.zenoh_session(), &topics)
                .await,
        )
        .into_response(),
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

    fn names(topics: &[RequestedTopic]) -> Vec<&str> {
        topics.iter().map(|t| t.name.as_str()).collect()
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
    fn parse_rejects_missing_or_empty() {
        assert!(parse_topics(None).is_err());
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
        let ok = "a".repeat(MAX_TOPIC_LEN);
        assert!(parse_topics(Some(&ok)).is_ok());
        let long = "a".repeat(MAX_TOPIC_LEN + 1);
        assert!(parse_topics(Some(&long)).is_err());
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
    }

    #[test]
    fn parse_rejects_invalid_key_expressions() {
        assert!(parse_topics(Some("radar//targets")).is_err());
    }

    #[test]
    fn availability_rule() {
        assert_eq!(
            TopicStatus::from_age(None),
            TopicStatus {
                available: false,
                age_ms: None
            }
        );
        assert_eq!(
            TopicStatus::from_age(Some(Duration::from_micros(55_900))),
            TopicStatus {
                available: true,
                age_ms: Some(55)
            }
        );
        assert_eq!(
            TopicStatus::from_age(Some(AVAILABILITY_THRESHOLD)),
            TopicStatus {
                available: true,
                age_ms: Some(3000)
            }
        );
        assert_eq!(
            TopicStatus::from_age(Some(Duration::from_millis(3001))),
            TopicStatus {
                available: false,
                age_ms: Some(3001)
            }
        );
    }

    #[test]
    fn sample_clock_records_latest_arrival() {
        let epoch = Instant::now();
        let clock = SampleClock::new(epoch);
        assert_eq!(clock.last(), None);
        clock.record(epoch);
        assert_eq!(clock.last(), Some(epoch));
        let later = epoch + Duration::from_millis(250);
        clock.record(later);
        assert_eq!(clock.last(), Some(later));
        clock.record(epoch + Duration::from_millis(100));
        assert_eq!(clock.last(), Some(later));
    }

    #[test]
    fn registry_reports_age_since_last_sample() {
        let t0 = Instant::now();
        let mut reg = WatchRegistry::<u32>::default();
        assert_eq!(reg.query("radar/targets", t0), None);

        let clock = Arc::new(SampleClock::new(t0));
        assert!(reg
            .insert("radar/targets".into(), clock.clone(), 1, t0)
            .is_empty());
        assert_eq!(reg.query("radar/targets", t0), Some(None));

        clock.record(t0 + Duration::from_millis(100));
        let age = reg.query("radar/targets", t0 + Duration::from_millis(155));
        assert_eq!(age, Some(Some(Duration::from_millis(55))));
    }

    #[test]
    fn registry_age_saturates_for_samples_after_now() {
        let t0 = Instant::now();
        let mut reg = WatchRegistry::<u32>::default();
        let clock = Arc::new(SampleClock::new(t0));
        reg.insert("k".into(), clock.clone(), 1, t0);
        clock.record(t0 + Duration::from_millis(10));
        assert_eq!(reg.query("k", t0), Some(Some(Duration::ZERO)));
    }

    #[test]
    fn registry_insert_of_existing_key_keeps_the_original() {
        let t0 = Instant::now();
        let mut reg = WatchRegistry::<u32>::default();
        let clock = Arc::new(SampleClock::new(t0));
        reg.insert("k".into(), clock.clone(), 1, t0);
        let dropped = reg.insert("k".into(), Arc::new(SampleClock::new(t0)), 2, t0);
        assert_eq!(dropped, [2]);
        clock.record(t0);
        assert_eq!(reg.query("k", t0), Some(Some(Duration::ZERO)));
    }

    #[test]
    fn registry_sweep_drops_idle_watches() {
        let t0 = Instant::now();
        let mut reg = WatchRegistry::<u32>::default();
        reg.insert("a".into(), Arc::new(SampleClock::new(t0)), 1, t0);
        reg.insert("b".into(), Arc::new(SampleClock::new(t0)), 2, t0);

        let t1 = t0 + Duration::from_secs(30);
        reg.query("b", t1);

        assert!(reg.sweep(t0 + WATCH_IDLE_EXPIRY).is_empty());
        let mut expired = reg.sweep(t0 + WATCH_IDLE_EXPIRY + Duration::from_millis(1));
        expired.sort_unstable();
        assert_eq!(expired, [1]);
        assert!(!reg.contains("a"));
        assert!(reg.contains("b"));

        let expired = reg.sweep(t1 + WATCH_IDLE_EXPIRY + Duration::from_millis(1));
        assert_eq!(expired, [2]);
        assert!(reg.is_empty());
    }

    #[test]
    fn registry_evicts_least_recently_queried_at_capacity() {
        let t0 = Instant::now();
        let mut reg = WatchRegistry::<usize>::default();
        for i in 0..MAX_WATCHED_TOPICS {
            let at = t0 + Duration::from_millis(i as u64);
            assert!(reg
                .insert(format!("t{i}"), Arc::new(SampleClock::new(t0)), i, at)
                .is_empty());
        }
        assert_eq!(reg.len(), MAX_WATCHED_TOPICS);

        let later = t0 + Duration::from_secs(1);
        reg.query("t0", later);

        let evicted = reg.insert("new".into(), Arc::new(SampleClock::new(t0)), 999, later);
        assert_eq!(evicted, [1]);
        assert_eq!(reg.len(), MAX_WATCHED_TOPICS);
        assert!(reg.contains("t0"));
        assert!(!reg.contains("t1"));
        assert!(reg.contains("new"));
    }

    #[test]
    fn response_serializes_to_contract_shape() {
        let mut topics = BTreeMap::new();
        topics.insert(
            "radar/targets".to_string(),
            TopicStatus {
                available: true,
                age_ms: Some(55),
            },
        );
        topics.insert(
            "lidar/points".to_string(),
            TopicStatus {
                available: false,
                age_ms: None,
            },
        );
        let value = serde_json::to_value(TopicStatusResponse { topics }).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "topics": {
                    "radar/targets": { "available": true, "age_ms": 55 },
                    "lidar/points": { "available": false, "age_ms": null }
                }
            })
        );
    }
}
