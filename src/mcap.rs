// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! MCAP file handling utilities.
//!
//! Provides functions for:
//! - Reading MCAP file metadata
//! - Memory-mapping MCAP files
//! - Streaming MCAP downloads

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use camino::Utf8Path;
use chrono::DateTime;
use mcap::records::{ChunkHeader, ChunkIndex, MessageIndex, Record};
use mcap::Summary;
use memmap::Mmap;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio_util::io::ReaderStream;

use crate::config::read_storage_directory;
use crate::mcap_timeline::{
    ChannelExtent, ChannelSpans, ClockStep, Span, Timeline, TimelineAccumulator,
    CLOCK_STEP_METADATA, CLOCK_STEP_TOPIC, CLOCK_SYNC_METADATA, STEP_GAP_NS,
};

// ============================================================================
// Types
// ============================================================================

/// File information for MCAP listings
#[derive(Serialize)]
pub struct FileInfo {
    pub name: String,
    pub size: u64, // Size in MB
    pub created: String,
    pub topics: HashMap<String, TopicInfo>,
    pub average_video_length: f64,
    /// Clock steps excluded from `average_video_length`.
    pub clock_steps: usize,
}

/// Directory response with MCAP files
#[derive(Serialize)]
pub struct DirectoryResponse {
    pub dir_name: String,
    pub files: Option<Vec<FileInfo>>,
    pub message: Option<String>,
    pub topics: Option<Vec<String>>,
}

/// Topic information from MCAP
#[derive(Serialize, Clone)]
pub struct TopicInfo {
    pub message_count: usize,
    pub average_fps: f64,
    pub video_length: f64,
}

// ============================================================================
// MCAP Reading Functions
// ============================================================================

/// Memory-map an MCAP file
pub fn map_mcap<P: AsRef<Utf8Path>>(p: P) -> Result<Mmap> {
    let fd = std::fs::File::open(p.as_ref()).context("Couldn't open MCAP file")?;
    unsafe { Mmap::map(&fd) }.context("Couldn't map MCAP file")
}

/// Recording-level information derived from an MCAP file.
#[derive(Serialize, Clone, Default)]
pub struct McapInfo {
    pub topics: HashMap<String, TopicInfo>,
    /// Recording duration with clock steps removed, in seconds.
    pub duration_s: f64,
    /// Clock steps excluded from `duration_s`.
    pub clock_steps: usize,
}

/// Files modified this recently without a summary are still being recorded.
const IN_PROGRESS_WINDOW: Duration = Duration::from_secs(10);

type LinearCache = Mutex<HashMap<PathBuf, (u64, SystemTime, McapInfo)>>;

fn linear_cache() -> &'static LinearCache {
    static CACHE: OnceLock<LinearCache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// How a file's information is obtained.
enum Source {
    /// Read from the summary section.
    Summary(McapInfo),
    /// Needs a linear scan of the data section. `complete` when the file has
    /// a summary, so it is not still being written.
    Linear { complete: bool },
}

fn classify(buf: &[u8]) -> Source {
    match Summary::read(buf) {
        Ok(Some(summary)) if summary.stats.is_some() => {
            if summary.chunk_indexes.is_empty() {
                Source::Linear { complete: true }
            } else {
                Source::Summary(info_from_summary(buf, &summary))
            }
        }
        _ => Source::Linear { complete: false },
    }
}

/// Read MCAP file info including topics and durations.
pub fn read_mcap_info<P: AsRef<Utf8Path>>(path: P) -> Result<McapInfo> {
    let path = path.as_ref();
    let mapped = map_mcap(path)?;
    let complete = match classify(&mapped) {
        Source::Summary(info) => return Ok(info),
        Source::Linear { complete } => complete,
    };
    let meta = std::fs::metadata(path)?;
    let modified = meta.modified()?;
    if !complete && modified.elapsed().is_ok_and(|age| age < IN_PROGRESS_WINDOW) {
        return Ok(McapInfo::default());
    }
    let key = path.as_std_path().to_path_buf();
    if let Some((len, mtime, info)) = linear_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        if *len == meta.len() && *mtime == modified {
            return Ok(info.clone());
        }
    }
    let info = read_linear(&mapped)?;
    linear_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, (meta.len(), modified, info.clone()));
    Ok(info)
}

/// Read MCAP info from an in-memory file.
///
/// Uses the summary section when it carries statistics and chunk indexes;
/// otherwise scans the data section linearly (files cut short by a crash or
/// power loss, and files written without chunk indexes).
pub fn read_mcap_info_bytes(buf: &[u8]) -> Result<McapInfo> {
    match classify(buf) {
        Source::Summary(info) => Ok(info),
        Source::Linear { .. } => read_linear(buf),
    }
}

fn info_from_summary(buf: &[u8], summary: &Summary) -> McapInfo {
    let mut steps: Vec<ClockStep> = summary
        .metadata_indexes
        .iter()
        .filter(|index| index.name == CLOCK_STEP_METADATA)
        .filter_map(|index| {
            let record = mcap::read::metadata(buf, index).ok()?;
            ClockStep::from_metadata(index.offset, &record)
        })
        .collect();
    steps.sort_by_key(|s| s.offset);

    let stats = summary.stats.as_ref().expect("checked by caller");
    let mut chunks: Vec<_> = summary.chunk_indexes.iter().collect();
    chunks.sort_by_key(|c| c.chunk_start_offset);
    let mut pending = steps.into_iter().peekable();
    let mut entries = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        while let Some(step) = pending.next_if(|s| s.offset < chunk.chunk_start_offset) {
            entries.push(Entry::Step(step));
        }
        entries.push(Entry::Chunk(Cow::Borrowed(chunk)));
    }
    entries.extend(pending.map(Entry::Step));
    let authoritative = summary
        .metadata_indexes
        .iter()
        .any(|index| index.name == CLOCK_SYNC_METADATA);
    let (timeline, spans) = feed_entries(buf, &entries, authoritative);
    build_info(channel_counts(summary, stats), &spans, timeline)
}

/// A data-section item in file order, as fed to the timeline.
enum Entry<'a> {
    Step(ClockStep),
    /// A chunk whose messages are described by its MessageIndex records.
    Chunk(Cow<'a, ChunkIndex>),
    /// `(log_time, channel_id)` of a decompressed chunk's messages, in file order.
    Messages(Vec<(u64, u16)>),
}

/// Rebuilds the timeline and per-channel spans from `entries` in file order.
/// `authoritative` when the file carries a `clock_sync` record, so its
/// `clock_step` records are the only source of clock steps.
fn feed_entries(
    buf: &[u8],
    entries: &[Entry],
    authoritative: bool,
) -> (Timeline, HashMap<u16, ChannelExtent>) {
    let mut acc = if authoritative {
        TimelineAccumulator::with_authoritative_records()
    } else {
        TimelineAccumulator::default()
    };
    let mut spans = ChannelSpans::default();
    let mut push_points = |acc: &mut TimelineAccumulator, times: &[(u64, u16)]| {
        for &(t, channel) in times {
            let point = Span {
                start_ns: t,
                end_ns: t,
            };
            acc.push(point);
            spans.push(acc.segment(), channel, point);
        }
    };
    // Unexpanded chunks holding each channel, per segment, in file order.
    let mut holding: HashMap<(usize, u16), Vec<&ChunkIndex>> = HashMap::new();
    for entry in entries {
        let chunk = match entry {
            Entry::Step(step) => {
                acc.clock_step(step);
                continue;
            }
            Entry::Messages(times) => {
                push_points(&mut acc, times);
                continue;
            }
            Entry::Chunk(chunk) => chunk.as_ref(),
        };
        if chunk.message_index_offsets.is_empty()
            && chunk.message_start_time == 0
            && chunk.message_end_time == 0
        {
            // A chunk holding only schema or channel records.
            continue;
        }
        let span = Span {
            start_ns: chunk.message_start_time,
            end_ns: chunk.message_end_time,
        };
        let straddles = span.end_ns.saturating_sub(span.start_ns) > STEP_GAP_NS;
        match straddles.then(|| chunk_message_times(buf, chunk)).flatten() {
            Some(times) => push_points(&mut acc, &times),
            None => {
                acc.push(span);
                for channel in chunk.message_index_offsets.keys() {
                    holding
                        .entry((acc.segment(), *channel))
                        .or_default()
                        .push(chunk);
                }
            }
        }
    }
    for ((segment, channel), chunks) in holding {
        match channel_extent(buf, &chunks, channel) {
            Some(Some(extent)) => spans.push(segment, channel, extent),
            Some(None) => {}
            None => {
                for chunk in chunks {
                    let span = Span {
                        start_ns: chunk.message_start_time,
                        end_ns: chunk.message_end_time,
                    };
                    spans.push(segment, channel, span);
                }
            }
        }
    }
    acc.finish_with_spans(spans)
}

fn channel_counts<'a>(
    summary: &'a Summary,
    stats: &'a mcap::records::Statistics,
) -> impl Iterator<Item = (u16, String, u64)> + 'a {
    summary.channels.iter().map(|(id, channel)| {
        let count = stats.channel_message_counts.get(id).copied().unwrap_or(0);
        (*id, channel.topic.clone(), count)
    })
}

/// Opcode and body of the record at `offset`, and the offset just past it.
/// `None` when the record does not fit in `buf`.
fn record_at(buf: &[u8], offset: usize) -> Option<(u8, &[u8], usize)> {
    let op = *buf.get(offset)?;
    let header_end = offset.checked_add(9)?;
    let len = u64::from_le_bytes(buf.get(offset + 1..header_end)?.try_into().ok()?);
    let end = header_end.checked_add(usize::try_from(len).ok()?)?;
    Some((op, buf.get(header_end..end)?, end))
}

/// Reads the MessageIndex record at `offset`. `None` when the offset is out
/// of bounds or the record there is not a MessageIndex.
fn read_message_index(buf: &[u8], offset: u64) -> Option<MessageIndex> {
    let (op, body, _) = record_at(buf, usize::try_from(offset).ok()?)?;
    match mcap::read::parse_record(op, body).ok()? {
        Record::MessageIndex(index) => Some(index),
        _ => None,
    }
}

/// `(log_time, channel_id)` of a chunk's messages in recording order, read from its
/// message indexes without decompressing the chunk. `None` when the file
/// has no message indexes or they cannot be parsed.
fn chunk_message_times(buf: &[u8], chunk: &ChunkIndex) -> Option<Vec<(u64, u16)>> {
    let mut entries = Vec::new();
    for &offset in chunk.message_index_offsets.values() {
        let index = read_message_index(buf, offset)?;
        let channel = index.channel_id;
        entries.extend(
            index
                .records
                .into_iter()
                .map(|e| (e.offset, e.log_time, channel)),
        );
    }
    if entries.is_empty() {
        return None;
    }
    entries.sort_unstable();
    Some(
        entries
            .into_iter()
            .map(|(_, log_time, channel)| (log_time, channel))
            .collect(),
    )
}

/// First and last `log_time` of `channel` across `chunks` (file order), read
/// from the message indexes of the first and last chunk that holds one of
/// its messages; an empty index means the chunk holds none. `Some(None)` when
/// no chunk holds a message, `None` when an index is missing or unparseable.
fn channel_extent(buf: &[u8], chunks: &[&ChunkIndex], channel: u16) -> Option<Option<Span>> {
    let times = |chunk: &ChunkIndex| -> Option<Vec<u64>> {
        let offset = *chunk.message_index_offsets.get(&channel)?;
        let index = read_message_index(buf, offset)?;
        (index.channel_id == channel).then(|| index.records.iter().map(|e| e.log_time).collect())
    };
    let mut start_ns = None;
    for chunk in chunks {
        let t = times(chunk)?;
        if let Some(&min) = t.iter().min() {
            start_ns = Some(min);
            break;
        }
    }
    let Some(start_ns) = start_ns else {
        return Some(None);
    };
    for chunk in chunks.iter().rev() {
        let t = times(chunk)?;
        if let Some(&end_ns) = t.iter().max() {
            return Some(Some(Span { start_ns, end_ns }));
        }
    }
    Some(None)
}

fn build_info(
    counts: impl IntoIterator<Item = (u16, String, u64)>,
    spans: &HashMap<u16, ChannelExtent>,
    timeline: Timeline,
) -> McapInfo {
    let topics = counts
        .into_iter()
        .filter(|(_, topic, _)| topic != CLOCK_STEP_TOPIC)
        .map(|(id, topic, count)| {
            let extent = spans.get(&id).copied().unwrap_or(ChannelExtent {
                span_ns: timeline.duration_ns,
                segments: 1,
            });
            let span_s = extent.span_ns as f64 / 1_000_000_000.0;
            // Each segment's first message opens no interval.
            let intervals = count.saturating_sub(extent.segments as u64);
            let average_fps = if intervals > 0 && span_s > 0.0 {
                intervals as f64 / span_s
            } else {
                0.0
            };
            let info = TopicInfo {
                message_count: count as usize,
                average_fps,
                video_length: span_s,
            };
            (topic, info)
        })
        .collect();
    McapInfo {
        topics,
        duration_s: timeline.duration_ns as f64 / 1_000_000_000.0,
        clock_steps: timeline.clock_steps,
    }
}

/// Reads a file by walking its top-level records, for files without a usable
/// summary or without chunk indexes. Messages outside chunks are read directly.
///
/// Chunk bodies are skipped: chunk headers give each chunk's time range and
/// the MessageIndex records that follow it give per-channel counts and
/// message times. A chunk is decompressed only when its indexes name a
/// channel whose Channel record has not been seen yet, or when it has no
/// indexes (such as the last chunk before a truncated tail). A truncated
/// tail ends the scan and everything read before it is kept.
fn read_linear(buf: &[u8]) -> Result<McapInfo> {
    let Some(mut offset) = buf.starts_with(mcap::MAGIC).then_some(mcap::MAGIC.len()) else {
        return Ok(McapInfo::default());
    };
    let mut scan = LinearScan::default();
    let mut complete = false;
    while let Some((op, body, next)) = record_at(buf, offset) {
        let Ok(record) = mcap::read::parse_record(op, body) else {
            break;
        };
        match record {
            Record::MessageIndex(index) => scan.message_index(offset, next, index),
            Record::Chunk { header, data } => {
                let Cow::Borrowed(data) = data else { break };
                scan.flush(false);
                scan.chunk = Some(PendingChunk::new(offset, next, header, data));
            }
            Record::Channel(channel) => {
                scan.flush(false);
                scan.topics.insert(channel.id, channel.topic);
            }
            Record::Message { header, .. } => {
                scan.flush(false);
                scan.message(header.log_time, header.channel_id);
            }
            Record::Metadata(metadata) => {
                scan.flush(false);
                scan.clock_sync |= metadata.name == CLOCK_SYNC_METADATA;
                if let Some(step) = ClockStep::from_metadata(offset as u64, &metadata) {
                    scan.entries.push(Entry::Step(step));
                }
            }
            Record::DataEnd(_) | Record::Footer(_) => {
                complete = true;
                break;
            }
            _ => scan.flush(false),
        }
        offset = next;
    }
    // Without the end of the data section, the last chunk's indexes may be incomplete.
    scan.flush(!complete);
    let (timeline, spans) = feed_entries(buf, &scan.entries, scan.clock_sync);
    let counts = scan
        .counts
        .into_iter()
        .filter_map(|(id, count)| Some((id, scan.topics.get(&id)?.clone(), count)));
    Ok(build_info(counts, &spans, timeline))
}

/// A chunk whose MessageIndex records are still being read.
struct PendingChunk<'a> {
    header: ChunkHeader,
    data: &'a [u8],
    index: ChunkIndex,
    counts: HashMap<u16, u64>,
}

impl<'a> PendingChunk<'a> {
    fn new(offset: usize, end: usize, header: ChunkHeader, data: &'a [u8]) -> Self {
        let index = ChunkIndex {
            message_start_time: header.message_start_time,
            message_end_time: header.message_end_time,
            chunk_start_offset: offset as u64,
            chunk_length: (end - offset) as u64,
            message_index_offsets: BTreeMap::new(),
            message_index_length: 0,
            compression: header.compression.clone(),
            compressed_size: header.compressed_size,
            uncompressed_size: header.uncompressed_size,
        };
        Self {
            header,
            data,
            index,
            counts: HashMap::new(),
        }
    }
}

#[derive(Default)]
struct LinearScan<'a> {
    entries: Vec<Entry<'static>>,
    topics: HashMap<u16, String>,
    counts: HashMap<u16, u64>,
    /// Whether a `clock_sync` record was seen.
    clock_sync: bool,
    chunk: Option<PendingChunk<'a>>,
    /// Chunks not decompressed, newest last, searched for Channel records
    /// that an indexed chunk refers to but did not contain.
    skipped: Vec<(ChunkHeader, &'a [u8])>,
}

impl<'a> LinearScan<'a> {
    /// Records a message read outside any chunk.
    fn message(&mut self, log_time: u64, channel: u16) {
        *self.counts.entry(channel).or_default() += 1;
        if let Some(Entry::Messages(times)) = self.entries.last_mut() {
            times.push((log_time, channel));
        } else {
            self.entries
                .push(Entry::Messages(vec![(log_time, channel)]));
        }
    }

    fn message_index(&mut self, offset: usize, end: usize, index: MessageIndex) {
        let Some(chunk) = self.chunk.as_mut() else {
            return;
        };
        chunk
            .index
            .message_index_offsets
            .insert(index.channel_id, offset as u64);
        chunk.index.message_index_length += (end - offset) as u64;
        *chunk.counts.entry(index.channel_id).or_default() += index.records.len() as u64;
    }

    /// Records the pending chunk. `expand` decompresses it even when it has
    /// MessageIndex records.
    fn flush(&mut self, expand: bool) {
        let Some(chunk) = self.chunk.take() else {
            return;
        };
        if expand || chunk.index.message_index_offsets.is_empty() {
            let mut times = Vec::new();
            for record in decompress(&chunk.header, chunk.data) {
                match record {
                    Record::Channel(channel) => {
                        self.topics.insert(channel.id, channel.topic);
                    }
                    Record::Message { header, .. } => {
                        *self.counts.entry(header.channel_id).or_default() += 1;
                        times.push((header.log_time, header.channel_id));
                    }
                    _ => {}
                }
            }
            self.entries.push(Entry::Messages(times));
            return;
        }
        let unseen = |topics: &HashMap<u16, String>| {
            chunk
                .index
                .message_index_offsets
                .keys()
                .any(|id| !topics.contains_key(id))
        };
        if unseen(&self.topics) {
            self.read_channels(&chunk.header, chunk.data);
            while unseen(&self.topics) {
                let Some((header, data)) = self.skipped.pop() else {
                    break;
                };
                self.read_channels(&header, data);
            }
        } else {
            self.skipped.push((chunk.header, chunk.data));
        }
        for (id, count) in chunk.counts {
            *self.counts.entry(id).or_default() += count;
        }
        self.entries.push(Entry::Chunk(Cow::Owned(chunk.index)));
    }

    fn read_channels(&mut self, header: &ChunkHeader, data: &[u8]) {
        for record in decompress(header, data) {
            if let Record::Channel(channel) = record {
                self.topics.insert(channel.id, channel.topic);
            }
        }
    }
}

/// Records of a chunk, up to the first one that cannot be decompressed or parsed.
fn decompress<'a>(header: &ChunkHeader, data: &'a [u8]) -> impl Iterator<Item = Record<'a>> {
    mcap::read::ChunkReader::new(header.clone(), data)
        .into_iter()
        .flatten()
        .map_while(Result::ok)
}

// ============================================================================
// Context Trait
// ============================================================================

/// Trait for accessing server context
pub trait McapContext: Send + Sync + 'static {
    fn is_system_mode(&self) -> bool;
    fn storage_path(&self) -> &str;
}

// ============================================================================
// HTTP Handlers
// ============================================================================

/// GET /mcap - List MCAP files in storage directory
pub async fn list_mcap_files<T: McapContext>(State(data): State<Arc<T>>) -> impl IntoResponse {
    let directory = if data.is_system_mode() {
        match read_storage_directory() {
            Ok(dir) => dir,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    axum::Json(serde_json::json!({"error": "No storage configured"})),
                )
                    .into_response();
            }
        }
    } else {
        data.storage_path().to_string()
    };

    let dir_clone = directory.clone();
    let result = tokio::task::spawn_blocking(move || -> Option<Vec<FileInfo>> {
        let entries = std::fs::read_dir(&dir_clone).ok()?;
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

                        let info =
                            read_mcap_info(Utf8Path::from_path(&entry.path())?).unwrap_or_default();

                        Some(FileInfo {
                            name: entry.file_name().to_string_lossy().to_string(),
                            size: size / (1024 * 1024),
                            created: DateTime::from_timestamp(created as i64, 0)
                                .unwrap()
                                .with_timezone(&chrono::Local)
                                .format("%Y-%m-%d %H:%M:%S")
                                .to_string(),
                            topics: info.topics,
                            average_video_length: info.duration_s,
                            clock_steps: info.clock_steps,
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        Some(files)
    })
    .await;

    let files = result.ok().flatten();

    let response = match files {
        Some(files) if !files.is_empty() => DirectoryResponse {
            dir_name: directory,
            files: Some(files),
            message: None,
            topics: None,
        },
        _ => DirectoryResponse {
            dir_name: directory,
            files: None,
            message: Some("No MCAP files found".to_string()),
            topics: None,
        },
    };

    axum::Json(response).into_response()
}

/// MCAP file download handler
pub async fn mcap_downloader(Path(path): Path<String>) -> impl IntoResponse {
    // axum 0.8 wildcard captures include leading slash; strip it
    let path = path.strip_prefix('/').unwrap_or(&path).to_string();
    let file_path = StdPath::new(&path);

    if !file_path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mcap"))
    {
        return (
            StatusCode::FORBIDDEN,
            "Invalid file extension. Only .mcap files are allowed.",
        )
            .into_response();
    }

    // Path traversal protection: canonicalize and verify within storage directory
    let canonical = match file_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            return (StatusCode::NOT_FOUND, format!("File {:?} not found", path)).into_response();
        }
    };
    let allowed_base = match read_storage_directory() {
        Ok(dir) => match StdPath::new(&dir).canonicalize() {
            Ok(p) => p,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Storage directory not accessible",
                )
                    .into_response();
            }
        },
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Storage directory not configured",
            )
                .into_response();
        }
    };
    if !canonical.starts_with(&allowed_base) {
        return (StatusCode::FORBIDDEN, "Access denied").into_response();
    }
    let file_path = canonical.as_path();

    if !file_path.exists() || !file_path.is_file() {
        return (StatusCode::NOT_FOUND, format!("File {:?} not found", path)).into_response();
    }

    let file = match tokio::fs::File::open(file_path).await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to open file: {}", e),
            )
                .into_response();
        }
    };

    let file_size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, file_size)
        .body(body)
        .unwrap()
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::mcap_timeline::{CLOCK_STEP_METADATA, CLOCK_STEP_TOPIC, CLOCK_SYNC_METADATA};
    use mcap::records::{MessageHeader, Metadata};
    use std::collections::BTreeMap;
    use std::io::Cursor;

    const S: u64 = 1_000_000_000;
    const T0: u64 = 1_748_544_498 * S;
    const STEP: i64 = 41_054_973_261_956_000;

    type TestWriter = mcap::Writer<Cursor<Vec<u8>>>;

    fn write(writer: &mut TestWriter, channel_id: u16, sequence: &mut u32, t: u64) {
        *sequence += 1;
        let header = MessageHeader {
            channel_id,
            sequence: *sequence,
            log_time: t,
            publish_time: t,
        };
        writer.write_to_known_channel(&header, &[0u8; 64]).unwrap();
    }

    /// 10 s of 10 Hz camera messages, a forward clock step, 10 s more.
    fn recording_across_step(options: mcap::WriteOptions, with_record: bool) -> Vec<u8> {
        let mut writer: TestWriter =
            mcap::Writer::with_options(Cursor::new(Vec::new()), options).unwrap();
        let schema = writer
            .add_schema("foxglove_msgs/msg/CompressedVideo", "ros2msg", b"")
            .unwrap();
        let camera = writer
            .add_channel(schema, "/camera/h264", "cdr", &BTreeMap::new())
            .unwrap();
        let marker = writer
            .add_channel(0, CLOCK_STEP_TOPIC, "json", &BTreeMap::new())
            .unwrap();
        let mut sequence = 0;
        for i in 0..100 {
            write(&mut writer, camera, &mut sequence, T0 + i * S / 10);
        }
        let after = T0 + 10 * S + STEP as u64;
        if with_record {
            let mut metadata = BTreeMap::new();
            metadata.insert("log_time_before".into(), (T0 + 10 * S).to_string());
            metadata.insert("log_time_after".into(), after.to_string());
            metadata.insert("step_ns".into(), STEP.to_string());
            metadata.insert("monotonic_ns".into(), "12000000000".into());
            writer
                .write_metadata(&Metadata {
                    name: CLOCK_STEP_METADATA.into(),
                    metadata,
                })
                .unwrap();
            write(&mut writer, marker, &mut sequence, after);
        }
        for i in 0..100 {
            write(&mut writer, camera, &mut sequence, after + i * S / 10);
        }
        writer.finish().unwrap();
        writer.into_inner().into_inner()
    }

    fn assert_true_duration(info: &McapInfo) {
        assert!(
            (19.7..=19.9).contains(&info.duration_s),
            "duration {} s",
            info.duration_s
        );
        let camera = &info.topics["/camera/h264"];
        assert_eq!(camera.message_count, 200);
        assert!(
            (camera.average_fps - 10.1).abs() < 0.2,
            "fps {}",
            camera.average_fps
        );
        assert!(!info.topics.contains_key(CLOCK_STEP_TOPIC));
    }

    #[test]
    fn summary_with_clock_step_record_reports_true_duration() {
        let buf = recording_across_step(mcap::WriteOptions::new().chunk_size(Some(1024)), true);
        let info = read_mcap_info_bytes(&buf).unwrap();
        assert_true_duration(&info);
        assert_eq!(info.clock_steps, 1);
    }

    #[test]
    fn summary_without_record_falls_back_to_gap_detection() {
        let buf = recording_across_step(mcap::WriteOptions::new().chunk_size(Some(1024)), false);
        let info = read_mcap_info_bytes(&buf).unwrap();
        assert_true_duration(&info);
        assert_eq!(info.clock_steps, 1);
    }

    #[test]
    fn recording_without_step_is_unchanged() {
        let mut writer = mcap::Writer::new(Cursor::new(Vec::new())).unwrap();
        let channel = writer
            .add_channel(0, "/imu", "cdr", &BTreeMap::new())
            .unwrap();
        for i in 0..50u32 {
            let t = T0 + u64::from(i) * S / 10;
            let header = MessageHeader {
                channel_id: channel,
                sequence: i,
                log_time: t,
                publish_time: t,
            };
            writer.write_to_known_channel(&header, &[1, 2, 3]).unwrap();
        }
        writer.finish().unwrap();
        let buf = writer.into_inner().into_inner();
        let info = read_mcap_info_bytes(&buf).unwrap();
        assert!((info.duration_s - 4.9).abs() < 1e-6);
        assert_eq!(info.clock_steps, 0);
    }

    #[test]
    fn test_directory_response_serialization() {
        let response = DirectoryResponse {
            dir_name: "/data".to_string(),
            files: None,
            message: Some("No files".to_string()),
            topics: None,
        };

        let json = serde_json::to_string(&response).expect("Failed to serialize");
        assert!(json.contains("\"dir_name\":\"/data\""));
        assert!(json.contains("\"message\":\"No files\""));
    }

    #[test]
    fn test_file_info_serialization() {
        let mut topics = HashMap::new();
        topics.insert(
            "test_topic".to_string(),
            TopicInfo {
                message_count: 100,
                average_fps: 30.0,
                video_length: 10.0,
            },
        );

        let file_info = FileInfo {
            name: "test.mcap".to_string(),
            size: 1024,
            created: "2024-01-15 10:00:00".to_string(),
            topics,
            average_video_length: 10.0,
            clock_steps: 0,
        };

        let json = serde_json::to_string(&file_info).expect("Failed to serialize");
        assert!(json.contains("\"name\":\"test.mcap\""));
        assert!(json.contains("\"size\":1024"));
        assert!(json.contains("\"clock_steps\":0"));
        assert!(json.contains("\"test_topic\""));
    }

    #[test]
    fn test_topic_info_serialization() {
        let topic = TopicInfo {
            message_count: 500,
            average_fps: 25.5,
            video_length: 20.0,
        };

        let json = serde_json::to_string(&topic).expect("Failed to serialize");
        assert!(json.contains("\"message_count\":500"));
        assert!(json.contains("\"average_fps\":25.5"));
        assert!(json.contains("\"video_length\":20.0"));
    }

    #[test]
    fn truncated_file_is_read_linearly() {
        let mut buf = recording_across_step(mcap::WriteOptions::new().chunk_size(Some(1024)), true);
        buf.truncate(buf.len() - 8); // break the end magic, as after a power loss
        let info = read_mcap_info_bytes(&buf).unwrap();
        assert_true_duration(&info);
        assert_eq!(info.clock_steps, 1);
    }

    #[test]
    fn file_without_statistics_is_read_linearly() {
        let options = mcap::WriteOptions::new()
            .chunk_size(Some(1024))
            .emit_statistics(false)
            .emit_chunk_indexes(false)
            .emit_metadata_indexes(false)
            .emit_summary_offsets(false);
        let buf = recording_across_step(options, false);
        let info = read_mcap_info_bytes(&buf).unwrap();
        assert_true_duration(&info);
    }

    #[test]
    fn garbage_is_empty_not_a_panic() {
        let info = read_mcap_info_bytes(b"\x89MCAP0\r\nnot really").unwrap();
        assert!(info.topics.is_empty());
        assert_eq!(info.duration_s, 0.0);
    }

    #[test]
    fn read_mcap_info_respects_mtime_for_in_progress_and_future_dates() {
        use camino::Utf8PathBuf;
        use std::time::SystemTime;

        // Create a summary-less file with a clock step
        let options = mcap::WriteOptions::new()
            .chunk_size(Some(1024))
            .emit_statistics(false)
            .emit_chunk_indexes(false)
            .emit_metadata_indexes(false)
            .emit_summary_offsets(false);
        let buf = recording_across_step(options, false);

        // Test (a): freshly written file → in-progress guard returns empty
        let temp_dir = std::env::temp_dir();
        let path_a = temp_dir.join("mcap_test_in_progress.mcap");
        std::fs::write(&path_a, &buf).unwrap();

        let utf8_path_a =
            Utf8PathBuf::from_path_buf(path_a.clone()).expect("temp path should be valid UTF-8");
        let info_a = read_mcap_info(&utf8_path_a).unwrap();
        assert!(
            info_a.topics.is_empty(),
            "freshly written file should be treated as in-progress (empty)"
        );
        assert_eq!(info_a.duration_s, 0.0);

        // Test (b): set mtime 60s in past → linear scan should work
        let past_time = SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::open(&path_a)
            .unwrap()
            .set_modified(past_time)
            .unwrap();

        let info_b = read_mcap_info(&utf8_path_a).unwrap();
        assert_true_duration(&info_b);
        assert_eq!(info_b.clock_steps, 1);

        // Test (c): set mtime 1 hour in future → should still get true duration (not treated as in-progress)
        let future_time = SystemTime::now() + std::time::Duration::from_secs(3600);
        std::fs::File::open(&path_a)
            .unwrap()
            .set_modified(future_time)
            .unwrap();

        let info_c = read_mcap_info(&utf8_path_a).unwrap();
        assert_true_duration(&info_c);
        assert_eq!(info_c.clock_steps, 1);

        // Cleanup
        let _ = std::fs::remove_file(&path_a);
    }

    /// Camera for 10 s; radar joins 3 s in; gps every 8 s; optional forward step at 10 s.
    fn staggered(options: mcap::WriteOptions, step: bool) -> Vec<u8> {
        let mut writer: TestWriter =
            mcap::Writer::with_options(Cursor::new(Vec::new()), options).unwrap();
        let camera = writer
            .add_channel(0, "/camera/h264", "cdr", &BTreeMap::new())
            .unwrap();
        let radar = writer
            .add_channel(0, "/radar/targets", "cdr", &BTreeMap::new())
            .unwrap();
        let gps = writer
            .add_channel(0, "/gps", "cdr", &BTreeMap::new())
            .unwrap();
        let mut sequence = 0;
        let section = |writer: &mut TestWriter, base: u64, sequence: &mut u32| {
            for i in 0..100u64 {
                let t = base + i * S / 10;
                write(writer, camera, sequence, t);
                if i >= 30 {
                    write(writer, radar, sequence, t + S / 20);
                }
                if i % 80 == 0 {
                    write(writer, gps, sequence, t);
                }
            }
        };
        section(&mut writer, T0, &mut sequence);
        if step {
            section(&mut writer, T0 + 10 * S + STEP as u64, &mut sequence);
        }
        writer.finish().unwrap();
        writer.into_inner().into_inner()
    }

    fn assert_staggered_fps(info: &McapInfo, segments: f64) {
        let camera = &info.topics["/camera/h264"];
        let radar = &info.topics["/radar/targets"];
        // One interval fewer than messages in each segment.
        let camera_fps = (100.0 * segments - segments) / (9.9 * segments);
        let radar_fps = (70.0 * segments - segments) / (6.9 * segments);
        assert!(
            (camera.average_fps - camera_fps).abs() < 0.05,
            "camera {}",
            camera.average_fps
        );
        assert!(
            (radar.average_fps - radar_fps).abs() < 0.05,
            "radar {}",
            radar.average_fps
        );
        assert!(
            (radar.video_length - 6.9 * segments).abs() < 0.05,
            "radar span {}",
            radar.video_length
        );
        let gps = &info.topics["/gps"];
        assert!(
            gps.video_length > 7.0 * segments,
            "gps span {}",
            gps.video_length
        );
        assert!(
            (info.duration_s - 9.95 * segments).abs() < 0.3,
            "duration {}",
            info.duration_s
        );
    }

    #[test]
    fn late_starting_topic_reports_its_own_rate() {
        let buf = staggered(mcap::WriteOptions::new().chunk_size(Some(256)), false);
        assert_staggered_fps(&read_mcap_info_bytes(&buf).unwrap(), 1.0);
    }

    #[test]
    fn late_starting_topic_across_a_step() {
        let buf = staggered(mcap::WriteOptions::new().chunk_size(Some(256)), true);
        assert_staggered_fps(&read_mcap_info_bytes(&buf).unwrap(), 2.0);
    }

    #[test]
    fn late_starting_topic_in_a_truncated_file() {
        let mut buf = staggered(mcap::WriteOptions::new().chunk_size(Some(256)), true);
        buf.truncate(buf.len() - 8);
        assert_staggered_fps(&read_mcap_info_bytes(&buf).unwrap(), 2.0);
    }

    #[test]
    fn chunks_without_message_indexes_fall_back_to_recording_span() {
        let options = mcap::WriteOptions::new()
            .chunk_size(Some(1024))
            .emit_message_indexes(false);
        let buf = staggered(options, false);
        let info = read_mcap_info_bytes(&buf).unwrap();
        assert!(
            (info.duration_s - 9.95).abs() < 0.05,
            "duration {}",
            info.duration_s
        );
        for (topic, t) in &info.topics {
            if t.message_count < 2 {
                continue;
            }
            assert!(t.average_fps > 0.0, "{topic} fps {}", t.average_fps);
            let expected = (t.message_count - 1) as f64 / info.duration_s;
            assert!(
                (t.average_fps - expected).abs() < 0.05,
                "{topic} fps {}",
                t.average_fps
            );
        }
        assert!((info.topics["/camera/h264"].average_fps - 10.0).abs() < 0.3);
    }
    /// 10 s of 10 Hz camera, a backward step with its record and marker,
    /// one `/imu` message stamped just before the step, then 10 s more.
    fn stray_after_backward_record(clock_sync: bool) -> Vec<u8> {
        let options = mcap::WriteOptions::new().chunk_size(Some(256));
        let mut writer: TestWriter =
            mcap::Writer::with_options(Cursor::new(Vec::new()), options).unwrap();
        if clock_sync {
            write_clock_sync(&mut writer);
        }
        let camera = writer
            .add_channel(0, "/camera/h264", "cdr", &BTreeMap::new())
            .unwrap();
        let imu = writer
            .add_channel(0, "/imu", "cdr", &BTreeMap::new())
            .unwrap();
        let marker = writer
            .add_channel(0, CLOCK_STEP_TOPIC, "json", &BTreeMap::new())
            .unwrap();
        let mut sequence = 0;
        for i in 0..100 {
            write(&mut writer, camera, &mut sequence, T0 + i * S / 10);
        }
        let step: i64 = -3_600 * S as i64;
        let after = T0 + 10 * S - 3_600 * S;
        let mut metadata = BTreeMap::new();
        metadata.insert("log_time_before".into(), (T0 + 10 * S).to_string());
        metadata.insert("log_time_after".into(), after.to_string());
        metadata.insert("step_ns".into(), step.to_string());
        writer
            .write_metadata(&Metadata {
                name: CLOCK_STEP_METADATA.into(),
                metadata,
            })
            .unwrap();
        write(&mut writer, marker, &mut sequence, after);
        write(
            &mut writer,
            imu,
            &mut sequence,
            T0 + 99 * S / 10 - 2_000_000,
        );
        for i in 0..100 {
            write(&mut writer, camera, &mut sequence, after + i * S / 10);
        }
        writer.finish().unwrap();
        writer.into_inner().into_inner()
    }

    fn assert_one_step_despite_stray(info: &McapInfo) {
        assert_eq!(info.clock_steps, 1);
        assert!(
            (info.duration_s - 19.8).abs() < 0.001,
            "duration {}",
            info.duration_s
        );
        assert_eq!(info.topics["/imu"].message_count, 1);
        let camera = &info.topics["/camera/h264"];
        assert!(
            (camera.average_fps - 198.0 / 19.8).abs() < 0.01,
            "camera fps {}",
            camera.average_fps
        );
    }

    #[test]
    fn stray_message_after_step_record_is_not_a_step() {
        let buf = stray_after_backward_record(false);
        assert_one_step_despite_stray(&read_mcap_info_bytes(&buf).unwrap());
    }

    #[test]
    fn stray_message_after_step_record_in_a_truncated_file() {
        let mut buf = stray_after_backward_record(false);
        buf.truncate(buf.len() - 8);
        assert_one_step_despite_stray(&read_mcap_info_bytes(&buf).unwrap());
    }

    #[test]
    fn stray_message_after_step_record_with_clock_sync() {
        let mut buf = stray_after_backward_record(true);
        assert_one_step_despite_stray(&read_mcap_info_bytes(&buf).unwrap());
        buf.truncate(buf.len() - 8);
        assert_one_step_despite_stray(&read_mcap_info_bytes(&buf).unwrap());
    }

    fn write_clock_sync(writer: &mut TestWriter) {
        let metadata = BTreeMap::from([("source".to_string(), "none".to_string())]);
        writer
            .write_metadata(&Metadata {
                name: CLOCK_SYNC_METADATA.into(),
                metadata,
            })
            .unwrap();
    }

    /// 10 s of 10 Hz camera messages, then 10 s more from `resume`, with an
    /// optional `clock_sync` record at the start and `clock_step` record
    /// between the two runs. Returns the finished and the truncated file.
    fn camera_runs(clock_sync: bool, resume: u64, step: Option<i64>) -> [Vec<u8>; 2] {
        let options = mcap::WriteOptions::new().chunk_size(Some(1024));
        camera_runs_with(options, clock_sync, resume, step)
    }

    fn camera_runs_with(
        options: mcap::WriteOptions,
        clock_sync: bool,
        resume: u64,
        step: Option<i64>,
    ) -> [Vec<u8>; 2] {
        let mut writer: TestWriter =
            mcap::Writer::with_options(Cursor::new(Vec::new()), options).unwrap();
        if clock_sync {
            write_clock_sync(&mut writer);
        }
        let camera = writer
            .add_channel(0, "/camera/h264", "cdr", &BTreeMap::new())
            .unwrap();
        let mut sequence = 0;
        for i in 0..100 {
            write(&mut writer, camera, &mut sequence, T0 + i * S / 10);
        }
        if let Some(step_ns) = step {
            let metadata = BTreeMap::from([("step_ns".to_string(), step_ns.to_string())]);
            writer
                .write_metadata(&Metadata {
                    name: CLOCK_STEP_METADATA.into(),
                    metadata,
                })
                .unwrap();
        }
        for i in 0..100 {
            write(&mut writer, camera, &mut sequence, resume + i * S / 10);
        }
        writer.finish().unwrap();
        let buf = writer.into_inner().into_inner();
        let truncated = buf[..buf.len() - 8].to_vec();
        [buf, truncated]
    }

    #[test]
    fn pause_counts_in_files_with_clock_sync() {
        for buf in camera_runs(true, T0 + 30 * S, None) {
            let info = read_mcap_info_bytes(&buf).unwrap();
            assert_eq!(info.clock_steps, 0);
            assert!((info.duration_s - 39.9).abs() < 1e-6, "{}", info.duration_s);
            let camera = &info.topics["/camera/h264"];
            assert!((camera.video_length - 39.9).abs() < 1e-6);
        }
    }

    #[test]
    fn pause_is_a_step_in_files_without_clock_sync() {
        for buf in camera_runs(false, T0 + 30 * S, None) {
            let info = read_mcap_info_bytes(&buf).unwrap();
            assert_eq!(info.clock_steps, 1);
            assert!((info.duration_s - 19.8).abs() < 1e-6, "{}", info.duration_s);
        }
    }

    #[test]
    fn recorded_step_in_files_with_clock_sync_is_excluded() {
        for buf in camera_runs(true, T0 + 10 * S + STEP as u64, Some(STEP)) {
            let info = read_mcap_info_bytes(&buf).unwrap();
            assert_eq!(info.clock_steps, 1);
            assert!((info.duration_s - 19.8).abs() < 1e-6, "{}", info.duration_s);
        }
    }

    /// With `clock_sync`: 10 s of 10 Hz camera, a 20.1 s unrecorded pause,
    /// 2 s of camera, a recorded forward step, then 10 s of camera.
    /// Returns the finished and the truncated file.
    fn pause_then_recorded_step() -> [Vec<u8>; 2] {
        let options = mcap::WriteOptions::new().chunk_size(Some(1024));
        let mut writer: TestWriter =
            mcap::Writer::with_options(Cursor::new(Vec::new()), options).unwrap();
        write_clock_sync(&mut writer);
        let camera = writer
            .add_channel(0, "/camera/h264", "cdr", &BTreeMap::new())
            .unwrap();
        let mut sequence = 0;
        for i in 0..100 {
            write(&mut writer, camera, &mut sequence, T0 + i * S / 10);
        }
        for i in 0..20 {
            write(&mut writer, camera, &mut sequence, T0 + 30 * S + i * S / 10);
        }
        let metadata = BTreeMap::from([("step_ns".to_string(), STEP.to_string())]);
        writer
            .write_metadata(&Metadata {
                name: CLOCK_STEP_METADATA.into(),
                metadata,
            })
            .unwrap();
        for i in 0..100 {
            let t = T0 + 32 * S + STEP as u64 + i * S / 10;
            write(&mut writer, camera, &mut sequence, t);
        }
        writer.finish().unwrap();
        let buf = writer.into_inner().into_inner();
        let truncated = buf[..buf.len() - 8].to_vec();
        [buf, truncated]
    }

    #[test]
    fn recorded_step_soon_after_a_pause_is_not_absorbed_by_it() {
        for buf in pause_then_recorded_step() {
            let info = read_mcap_info_bytes(&buf).unwrap();
            assert_eq!(info.clock_steps, 1);
            assert!((info.duration_s - 41.8).abs() < 1e-6, "{}", info.duration_s);
        }
    }

    #[test]
    fn statistics_without_chunk_indexes_split_at_each_step() {
        let hour = 3_600 * S;
        let unindexed = mcap::WriteOptions::new()
            .chunk_size(Some(1024))
            .emit_chunk_indexes(false);
        let unchunked = mcap::WriteOptions::new().use_chunks(false);
        for (name, options) in [("unindexed", unindexed), ("unchunked", unchunked)] {
            for (resume, step) in [
                (T0 + 10 * S + hour, hour as i64),
                (T0 + 10 * S - hour, -(hour as i64)),
            ] {
                let [buf, _] = camera_runs_with(options.clone(), false, resume, Some(step));
                let summary = Summary::read(&buf).unwrap().unwrap();
                assert!(summary.stats.is_some() && summary.chunk_indexes.is_empty());
                let info = read_mcap_info_bytes(&buf).unwrap();
                assert_eq!(info.clock_steps, 1, "{name} step {step}");
                assert!(
                    (info.duration_s - 19.8).abs() < 1e-6,
                    "{name} step {step}: {}",
                    info.duration_s
                );
                let camera = &info.topics["/camera/h264"];
                assert_eq!(camera.message_count, 200, "{name} step {step}");
                assert!(
                    (camera.video_length - 19.8).abs() < 1e-6,
                    "{name} step {step}"
                );
            }
        }
    }

    #[test]
    fn low_rate_topic_spans_its_own_messages_across_chunks() {
        // About 2 s of data per chunk: far coarser than the 1 Hz topic's
        // alignment, but short enough that chunks are not expanded.
        let options = mcap::WriteOptions::new().chunk_size(Some(1900));
        let mut writer: TestWriter =
            mcap::Writer::with_options(Cursor::new(Vec::new()), options).unwrap();
        let camera = writer
            .add_channel(0, "/camera/h264", "cdr", &BTreeMap::new())
            .unwrap();
        let gps = writer
            .add_channel(0, "/gps", "cdr", &BTreeMap::new())
            .unwrap();
        let mut sequence = 0;
        for i in 0..200u64 {
            let t = T0 + i * S / 10;
            write(&mut writer, camera, &mut sequence, t);
            if (30..190).contains(&i) && i % 10 == 0 {
                write(&mut writer, gps, &mut sequence, t + S / 20);
            }
        }
        writer.finish().unwrap();
        let buf = writer.into_inner().into_inner();
        let summary = Summary::read(&buf).unwrap().unwrap();
        assert!(summary.chunk_indexes.len() >= 8, "chunks too large");

        let info = read_mcap_info_bytes(&buf).unwrap();
        let gps = &info.topics["/gps"];
        assert_eq!(gps.message_count, 16);
        assert!(
            (gps.average_fps - 1.0).abs() < 0.01,
            "gps fps {}",
            gps.average_fps
        );
        assert!(
            (gps.video_length - 15.0).abs() < 0.05,
            "gps span {}",
            gps.video_length
        );
        let camera = &info.topics["/camera/h264"];
        assert!(
            (camera.average_fps - 10.0).abs() < 0.01,
            "camera fps {}",
            camera.average_fps
        );
    }

    /// Appends a MessageIndex record for `channel` and returns its offset.
    fn push_message_index(buf: &mut Vec<u8>, channel: u16, times: &[u64]) -> u64 {
        let offset = buf.len() as u64;
        let entries = (times.len() * 16) as u32;
        buf.push(0x07);
        buf.extend_from_slice(&(2 + 4 + u64::from(entries)).to_le_bytes());
        buf.extend_from_slice(&channel.to_le_bytes());
        buf.extend_from_slice(&entries.to_le_bytes());
        for &t in times {
            buf.extend_from_slice(&t.to_le_bytes());
            buf.extend_from_slice(&0u64.to_le_bytes());
        }
        offset
    }

    fn chunk_with_index(channel: u16, offset: u64) -> ChunkIndex {
        ChunkIndex {
            message_start_time: 0,
            message_end_time: 100,
            chunk_start_offset: 0,
            chunk_length: 0,
            message_index_offsets: BTreeMap::from([(channel, offset)]),
            message_index_length: 0,
            compression: String::new(),
            compressed_size: 0,
            uncompressed_size: 0,
        }
    }

    #[test]
    fn channel_extent_skips_empty_indexes_and_rejects_bad_ones() {
        let mut buf = Vec::new();
        let chunks: Vec<ChunkIndex> = [&[][..], &[15, 12], &[30], &[48, 45], &[]]
            .iter()
            .map(|times| chunk_with_index(3, push_message_index(&mut buf, 3, times)))
            .collect();
        let refs: Vec<&ChunkIndex> = chunks.iter().collect();
        assert_eq!(
            channel_extent(&buf, &refs, 3),
            Some(Some(Span {
                start_ns: 12,
                end_ns: 48
            }))
        );
        assert_eq!(channel_extent(&buf, &[refs[0], refs[4]], 3), Some(None));

        let bad = chunk_with_index(3, buf.len() as u64 + 1);
        assert_eq!(channel_extent(&buf, &[&bad, refs[1]], 3), None);
        let wrong_channel = chunk_with_index(4, chunks[1].message_index_offsets[&3]);
        assert_eq!(channel_extent(&buf, &[&wrong_channel], 4), None);
    }

    fn chunks_in_file_order(buf: &[u8]) -> Vec<ChunkIndex> {
        let summary = Summary::read(buf).unwrap().unwrap();
        let mut chunks = summary.chunk_indexes;
        chunks.sort_by_key(|c| c.chunk_start_offset);
        chunks
    }

    fn assert_same_info(actual: &McapInfo, expected: &McapInfo) {
        assert_eq!(actual.clock_steps, expected.clock_steps);
        assert_eq!(actual.duration_s, expected.duration_s);
        assert_eq!(actual.topics.len(), expected.topics.len());
        for (topic, e) in &expected.topics {
            let a = &actual.topics[topic];
            assert_eq!(a.message_count, e.message_count, "{topic}");
            assert_eq!(a.average_fps, e.average_fps, "{topic}");
            assert_eq!(a.video_length, e.video_length, "{topic}");
        }
    }

    #[test]
    fn linear_scan_does_not_decompress_indexed_chunks() {
        for compression in [mcap::Compression::Zstd, mcap::Compression::Lz4] {
            let options = mcap::WriteOptions::new()
                .compression(Some(compression))
                .chunk_size(Some(256));
            let mut buf = staggered(options, true);
            let chunks = chunks_in_file_order(&buf);
            buf.truncate(buf.len() - 8);
            let expected = read_mcap_info_bytes(&buf).unwrap();
            assert_staggered_fps(&expected, 2.0);

            let middle = &chunks[chunks.len() / 2];
            let end = (middle.chunk_start_offset + middle.chunk_length) as usize;
            let body = end - middle.compressed_size as usize;
            buf[body..end].fill(0xA5);
            let info = read_mcap_info_bytes(&buf).unwrap();
            assert_same_info(&info, &expected);
        }
    }

    #[test]
    fn last_chunk_without_message_indexes_is_counted() {
        let buf = staggered(mcap::WriteOptions::new().chunk_size(Some(256)), true);
        let last = chunks_in_file_order(&buf).pop().unwrap();
        let info =
            read_mcap_info_bytes(&buf[..(last.chunk_start_offset + last.chunk_length) as usize])
                .unwrap();
        assert_eq!(info.topics["/camera/h264"].message_count, 200);
        assert_eq!(info.topics["/radar/targets"].message_count, 140);
        assert_eq!(info.topics["/gps"].message_count, 4);
        assert_staggered_fps(&info, 2.0);
    }

    #[test]
    fn channel_record_in_an_earlier_chunk_is_found() {
        // One message per chunk: `/gps` is declared in the chunk before its
        // first message, whose chunk only indexes it.
        let options = mcap::WriteOptions::new().chunk_size(Some(1));
        let mut writer: TestWriter =
            mcap::Writer::with_options(Cursor::new(Vec::new()), options).unwrap();
        let camera = writer
            .add_channel(0, "/camera/h264", "cdr", &BTreeMap::new())
            .unwrap();
        let mut sequence = 0;
        for i in 0..20 {
            write(&mut writer, camera, &mut sequence, T0 + i * S / 10);
        }
        let gps = writer
            .add_channel(0, "/gps", "cdr", &BTreeMap::new())
            .unwrap();
        for i in 20..40 {
            write(&mut writer, camera, &mut sequence, T0 + i * S / 10);
            if i % 10 == 0 {
                write(&mut writer, gps, &mut sequence, T0 + i * S / 10);
            }
        }
        writer.finish().unwrap();
        let mut buf = writer.into_inner().into_inner();
        buf.truncate(buf.len() - 8);
        let info = read_mcap_info_bytes(&buf).unwrap();
        assert_eq!(info.topics["/camera/h264"].message_count, 40);
        assert_eq!(info.topics["/gps"].message_count, 2);
        assert!((info.topics["/gps"].video_length - 1.0).abs() < 1e-9);
    }

    /// Checks rates against a real recording without clock steps:
    /// `verdin-imx8mp-15141064_2026_09_16_17_02_42.mcap`, about 7 s recorded
    /// by the EdgeFirst recorder on a Verdin i.MX 8M Plus. It is not in the
    /// repository; copy it from the device's recording storage into
    /// `.claude/tmp/` to run this test with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a real recording under .claude/tmp"]
    fn real_recording_reports_true_rates() {
        let path = StdPath::new(env!("CARGO_MANIFEST_DIR"))
            .join(".claude/tmp/verdin-imx8mp-15141064_2026_09_16_17_02_42.mcap");
        let Ok(buf) = std::fs::read(&path) else {
            eprintln!("skipping: {} not found", path.display());
            return;
        };
        let info = read_mcap_info_bytes(&buf).unwrap();
        println!(
            "duration_s={:.6} clock_steps={}",
            info.duration_s, info.clock_steps
        );
        let mut topics: Vec<_> = info.topics.iter().collect();
        topics.sort_by_key(|(name, _)| name.as_str());
        for (name, t) in &topics {
            println!(
                "{name}: count={} fps={:.4} span={:.4}",
                t.message_count, t.average_fps, t.video_length
            );
        }
        for (topic, rate) in [
            ("/camera/frame", 59.48),
            ("/camera/h264", 50.75),
            ("/model/output", 8.47),
            ("/radar/targets", 17.86),
            ("/gps", 1.01),
        ] {
            let fps = info.topics[topic].average_fps;
            assert!(
                (fps - rate).abs() <= rate * 0.005,
                "{topic} fps {fps}, expected {rate}"
            );
        }
        assert!(
            (info.duration_s - 6.961).abs() <= 0.001,
            "duration {}",
            info.duration_s
        );
        assert_eq!(info.clock_steps, 0);
    }
}
