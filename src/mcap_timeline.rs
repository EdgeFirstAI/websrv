// Copyright 2025 Au-Zone Technologies Inc.
// SPDX-License-Identifier: Apache-2.0

//! Recording timeline reconstruction across wall-clock steps.
//!
//! MCAP statistics only give the first and last `log_time`, which spans any
//! step of `CLOCK_REALTIME` taken during the recording. The duration is rebuilt
//! as the sum of internally consistent segments, split at `clock_step`
//! Metadata records and at jumps larger than [`STEP_GAP_NS`].

use mcap::records::Metadata;
use std::collections::HashMap;

/// Jumps in `log_time` larger than this between consecutive spans, in either
/// direction, are treated as clock steps.
pub const STEP_GAP_NS: u64 = 5_000_000_000;

/// Name of the Metadata record the recorder writes for each clock step.
pub const CLOCK_STEP_METADATA: &str = "clock_step";

/// Name of the Metadata record the recorder writes when it opens a file.
/// Files carrying it record every clock step as a `clock_step` record.
pub const CLOCK_SYNC_METADATA: &str = "clock_sync";

/// Timeline-marker channel the recorder writes alongside each record.
pub const CLOCK_STEP_TOPIC: &str = "/clock_step";

/// A clock step read from a `clock_step` Metadata record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockStep {
    /// File offset of the Metadata record; orders the step among chunks.
    pub offset: u64,
    /// Signed size of the step in nanoseconds.
    pub step_ns: i64,
}

impl ClockStep {
    /// Parses a `clock_step` record. Returns `None` for other records or
    /// when `step_ns` is missing or not a decimal integer.
    pub fn from_metadata(offset: u64, metadata: &Metadata) -> Option<Self> {
        if metadata.name != CLOCK_STEP_METADATA {
            return None;
        }
        let step_ns = metadata.metadata.get("step_ns")?.trim().parse().ok()?;
        Some(Self { offset, step_ns })
    }
}

/// A `log_time` interval covered by one chunk or one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start_ns: u64,
    pub end_ns: u64,
}

impl Span {
    fn len(self) -> u64 {
        self.end_ns.saturating_sub(self.start_ns)
    }

    fn merge(self, other: Span) -> Span {
        Span {
            start_ns: self.start_ns.min(other.start_ns),
            end_ns: self.end_ns.max(other.end_ns),
        }
    }
}

/// Reconstructed recording timeline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timeline {
    pub duration_ns: u64,
    /// Number of discontinuities excluded from `duration_ns`.
    pub clock_steps: usize,
}

/// Accumulates spans in file order and sums the duration of each segment.
///
/// A gap-detected excursion that returns to the timeline it left is stray
/// data, not a pair of clock steps: when a gap closes a segment that was
/// itself opened by a gap, is no longer than [`STEP_GAP_NS`], and the new
/// span resumes within [`STEP_GAP_NS`] of the preceding segment's last span,
/// the preceding segment is re-opened, the excursion adds no duration, and
/// neither of its boundaries counts as a step.
///
/// When records are authoritative (the file carries a `clock_sync` record),
/// only `clock_step` records count as steps. A forward jump larger than
/// [`STEP_GAP_NS`] without a record is a pause in the data: it is held as
/// tentative until the data after it either returns as a stray excursion or
/// spans more than [`STEP_GAP_NS`], and a pause joins the segments on either
/// side so its duration counts. A backward jump without a record cannot be a
/// pause; it still splits the timeline but is not counted as a step.
#[derive(Debug, Default)]
pub struct TimelineAccumulator {
    authoritative: bool,
    total_ns: u64,
    clock_steps: usize,
    /// Index of the open segment; indexes are never reused.
    segment: usize,
    next_segment: usize,
    earlier: Option<Span>,
    last: Option<Span>,
    /// The jump that opened the current segment, when gap-detected; a record
    /// of about the same size arriving soon after describes that same jump.
    gap: Option<Gap>,
    /// The segment closed most recently, kept so an excursion can re-open it.
    previous: Option<Closed>,
    /// Segments joined to an earlier one across a pause, as `(from, into)`.
    merged: Vec<(usize, usize)>,
    /// Stray excursions dropped when the segment before them was re-opened.
    abandoned: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GapKind {
    /// Counted as a clock step.
    Step,
    /// Splits the timeline without counting as a step.
    Split,
    /// A pause, unless the data after it turns out to be a stray excursion.
    Pause,
}

#[derive(Debug, Clone, Copy)]
struct Gap {
    /// Log time of the first span after the jump.
    at: u64,
    /// Signed size of the jump, from the end of the span before it.
    jump: i128,
    kind: GapKind,
}

impl Gap {
    /// Whether `step` describes this jump, which it does when it arrives
    /// within [`STEP_GAP_NS`] of data after the jump and either matches the
    /// jump to within [`STEP_GAP_NS`] or is in the same direction and smaller
    /// in forward time, the rest being a pause in the data around the step.
    /// Returns the length of that pause, zero for a match.
    fn described_by(self, step: &ClockStep, last: Option<Span>) -> Option<u64> {
        let since = last.map_or(0, |l| l.end_ns.saturating_sub(self.at));
        if since > STEP_GAP_NS {
            return None;
        }
        let paused = self.jump - i128::from(step.step_ns);
        if paused.unsigned_abs() <= u128::from(STEP_GAP_NS) {
            return Some(0);
        }
        let same_direction = (self.jump > 0) == (step.step_ns > 0);
        (paused > 0 && same_direction).then(|| u64::try_from(paused).unwrap_or(u64::MAX))
    }
}

#[derive(Debug, Clone, Copy)]
struct Closed {
    segment: usize,
    earlier: Option<Span>,
    last: Span,
    duration_ns: u64,
    gap: Option<Gap>,
}

impl TimelineAccumulator {
    /// An accumulator for files whose `clock_step` records are the only source of steps.
    pub fn with_authoritative_records() -> Self {
        Self {
            authoritative: true,
            ..Self::default()
        }
    }

    pub fn push(&mut self, span: Span) {
        if let Some(last) = self.last {
            let gap = i128::from(span.start_ns) - i128::from(last.end_ns);
            if gap.unsigned_abs() > u128::from(STEP_GAP_NS) && !self.reopen_before_excursion(span) {
                self.settle_pause();
                self.close(None);
                let kind = match (self.authoritative, gap > 0) {
                    (false, _) => GapKind::Step,
                    (true, true) => GapKind::Pause,
                    (true, false) => GapKind::Split,
                };
                if kind == GapKind::Step {
                    self.clock_steps += 1;
                }
                self.gap = Some(Gap {
                    at: span.start_ns,
                    jump: gap,
                    kind,
                });
            }
        }
        if let Some(last) = self.last.take() {
            self.earlier = Some(self.earlier.map_or(last, |e| e.merge(last)));
        }
        self.last = Some(span);
        let extent = self.earlier.map_or(span, |e| e.merge(span));
        if extent.len() > STEP_GAP_NS {
            self.settle_pause();
        }
    }

    /// Records a clock step. A step that describes the gap-detected jump
    /// opening the current segment confirms that jump as a step; any other
    /// closes the current segment as a step of its own. When records are
    /// authoritative, a pause found in the jump beside the step counts toward
    /// the duration; otherwise the whole jump is excluded, as for any gap.
    pub fn clock_step(&mut self, step: &ClockStep) {
        if let Some(gap) = self.gap {
            if let Some(paused) = gap.described_by(step, self.last) {
                self.gap = None;
                if gap.kind != GapKind::Step {
                    self.clock_steps += 1;
                }
                if self.authoritative {
                    self.total_ns = self.total_ns.saturating_add(paused);
                }
                return;
            }
            self.settle_pause();
        }
        self.gap = None;
        self.close(Some(step.step_ns.unsigned_abs()));
        self.clock_steps += 1;
    }

    /// Index of the segment the next span belongs to.
    pub fn segment(&self) -> usize {
        self.segment
    }

    pub fn finish(self) -> Timeline {
        self.finish_with_spans(ChannelSpans::default()).0
    }

    /// Finishes the timeline and the per-channel spans recorded against its
    /// segment indexes, joining the spans of segments joined across a pause
    /// and dropping those of stray excursions.
    pub fn finish_with_spans(
        mut self,
        spans: ChannelSpans,
    ) -> (Timeline, HashMap<u16, ChannelExtent>) {
        self.settle_pause();
        self.close(None);
        let timeline = Timeline {
            duration_ns: self.total_ns,
            clock_steps: self.clock_steps,
        };
        (timeline, spans.finish_merged(&self.merged, &self.abandoned))
    }

    /// Drops the open segment and re-opens the previous one when the open
    /// segment is a stray excursion that `next` returns from.
    fn reopen_before_excursion(&mut self, next: Span) -> bool {
        let (Some(gap), Some(previous), Some(last)) = (self.gap, self.previous, self.last) else {
            return false;
        };
        let excursion = self.earlier.map_or(last, |e| e.merge(last));
        let resumes = i128::from(next.start_ns) - i128::from(previous.last.end_ns);
        if excursion.len() > STEP_GAP_NS || resumes.unsigned_abs() > u128::from(STEP_GAP_NS) {
            return false;
        }
        self.total_ns = self.total_ns.saturating_sub(previous.duration_ns);
        if gap.kind == GapKind::Step {
            self.clock_steps = self.clock_steps.saturating_sub(1);
        }
        self.abandoned.push(self.segment);
        self.segment = previous.segment;
        self.earlier = previous.earlier;
        self.last = Some(previous.last);
        self.gap = previous.gap;
        self.previous = None;
        true
    }

    /// Joins the open segment to the previous one when a tentative pause opened it.
    fn settle_pause(&mut self) {
        let Some(Gap {
            kind: GapKind::Pause,
            ..
        }) = self.gap
        else {
            return;
        };
        let Some(previous) = self.previous.take() else {
            return;
        };
        self.total_ns = self.total_ns.saturating_sub(previous.duration_ns);
        self.merged.push((self.segment, previous.segment));
        self.segment = previous.segment;
        let before = previous
            .earlier
            .map_or(previous.last, |e| e.merge(previous.last));
        self.earlier = Some(self.earlier.map_or(before, |e| e.merge(before)));
        self.gap = previous.gap;
    }

    fn close(&mut self, step_abs: Option<u64>) {
        let earlier = self.earlier.take();
        let Some(last) = self.last.take() else {
            return;
        };
        let duration_ns = match step_abs {
            Some(step) if last.len() >= step => earlier.map_or(0, Span::len) + (last.len() - step),
            _ => earlier.map_or(last, |e| e.merge(last)).len(),
        };
        self.total_ns = self.total_ns.saturating_add(duration_ns);
        self.previous = Some(Closed {
            segment: self.segment,
            earlier,
            last,
            duration_ns,
            gap: self.gap,
        });
        self.next_segment += 1;
        self.segment = self.next_segment;
    }
}

/// A channel's `log_time` extent summed over the timeline segments it appears in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelExtent {
    pub span_ns: u64,
    /// Number of segments holding at least one of the channel's messages.
    pub segments: usize,
}

/// Per-channel `log_time` extent within each timeline segment.
#[derive(Debug, Default)]
pub struct ChannelSpans {
    open: HashMap<(usize, u16), Span>,
}

impl ChannelSpans {
    pub fn push(&mut self, segment: usize, channel: u16, span: Span) {
        self.open
            .entry((segment, channel))
            .and_modify(|s| *s = s.merge(span))
            .or_insert(span);
    }

    pub fn finish(self) -> HashMap<u16, ChannelExtent> {
        self.finish_merged(&[], &[])
    }

    /// Sums each channel's spans, first joining the spans of each segment
    /// in `merged` (`(from, into)` pairs) to the segment it was joined to.
    /// Spans in `abandoned` segments are dropped; a channel seen only there
    /// has an empty extent.
    fn finish_merged(
        self,
        merged: &[(usize, usize)],
        abandoned: &[usize],
    ) -> HashMap<u16, ChannelExtent> {
        let into: HashMap<usize, usize> = merged.iter().copied().collect();
        let canonical = |mut segment: usize| {
            while let Some(&next) = into.get(&segment) {
                segment = next;
            }
            segment
        };
        let mut totals: HashMap<u16, ChannelExtent> = HashMap::new();
        let mut joined: HashMap<(usize, u16), Span> = HashMap::new();
        for ((segment, channel), span) in self.open {
            if abandoned.contains(&segment) {
                totals.entry(channel).or_default();
                continue;
            }
            joined
                .entry((canonical(segment), channel))
                .and_modify(|s| *s = s.merge(span))
                .or_insert(span);
        }
        for ((_, channel), span) in joined {
            let total = totals.entry(channel).or_default();
            total.span_ns += span.len();
            total.segments += 1;
        }
        totals
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const S: u64 = 1_000_000_000;
    const T0: u64 = 1_748_544_498 * S; // a boot-epoch wall time
    const STEP: i64 = 41_054_973_261_956_000; // 475.17 days

    fn run(items: &[Item]) -> Timeline {
        feed(TimelineAccumulator::default(), items)
    }

    /// Runs `items` as for a file that carries a `clock_sync` record.
    fn run_synced(items: &[Item]) -> Timeline {
        feed(TimelineAccumulator::with_authoritative_records(), items)
    }

    fn feed(mut acc: TimelineAccumulator, items: &[Item]) -> Timeline {
        for item in items {
            match *item {
                Item::Span(start_ns, end_ns) => acc.push(Span { start_ns, end_ns }),
                Item::Step(step_ns) => acc.clock_step(&ClockStep { offset: 0, step_ns }),
            }
        }
        acc.finish()
    }

    enum Item {
        Span(u64, u64),
        Step(i64),
    }

    fn one_second_chunks(start: u64, count: u64) -> Vec<Item> {
        (0..count)
            .map(|i| Item::Span(start + i * S, start + i * S + S - S / 10))
            .collect()
    }

    #[test]
    fn continuous_recording_is_its_span() {
        let t = run(&one_second_chunks(T0, 10));
        assert_eq!(t.duration_ns, 10 * S - S / 10);
        assert_eq!(t.clock_steps, 0);
    }

    #[test]
    fn forward_step_with_record_excludes_the_step() {
        let mut items = one_second_chunks(T0, 10);
        items.push(Item::Step(STEP));
        items.extend(one_second_chunks(T0 + 10 * S + STEP as u64, 10));
        let t = run(&items);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
        assert_eq!(t.clock_steps, 1);
    }

    #[test]
    fn forward_step_without_record_is_detected_by_gap() {
        let mut items = one_second_chunks(T0, 10);
        items.extend(one_second_chunks(T0 + 10 * S + STEP as u64, 10));
        let t = run(&items);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
        assert_eq!(t.clock_steps, 1);
    }

    #[test]
    fn backward_step_without_record_is_detected_by_gap() {
        let mut items = one_second_chunks(T0 + STEP as u64, 10);
        items.extend(one_second_chunks(T0, 10));
        let t = run(&items);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
        assert_eq!(t.clock_steps, 1);
    }

    #[test]
    fn record_after_gap_counts_once() {
        let mut items = one_second_chunks(T0, 5);
        items.extend(one_second_chunks(T0 + 5 * S + STEP as u64, 1));
        items.push(Item::Step(STEP));
        items.extend(one_second_chunks(T0 + 6 * S + STEP as u64, 4));
        let t = run(&items);
        assert_eq!(t.clock_steps, 1);
        assert!(
            t.duration_ns >= 9 * S && t.duration_ns <= 10 * S,
            "{}",
            t.duration_ns
        );
    }

    #[test]
    fn leaked_chunk_before_forward_record_is_corrected() {
        let mut items = one_second_chunks(T0, 9);
        // Last pre-step chunk also holds 0.2 s of post-step messages.
        items.push(Item::Span(
            T0 + 9 * S,
            T0 + 9 * S + S / 2 + STEP as u64 + S / 5,
        ));
        items.push(Item::Step(STEP));
        items.extend(one_second_chunks(T0 + 10 * S + STEP as u64, 10));
        let t = run(&items);
        let expected = (9 * S - S / 10) + (S / 2 + S / 5) + (10 * S - S / 10);
        let error = t.duration_ns.abs_diff(expected);
        assert!(
            error < S,
            "duration {} expected ≈{}",
            t.duration_ns,
            expected
        );
    }

    #[test]
    fn small_step_below_gap_threshold_uses_record() {
        let mut items = one_second_chunks(T0, 10);
        items.push(Item::Step(2 * S as i64));
        items.extend(one_second_chunks(T0 + 12 * S, 10));
        let t = run(&items);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
        assert_eq!(t.clock_steps, 1);
    }

    #[test]
    fn later_record_after_a_gap_is_a_separate_step() {
        let mut items = one_second_chunks(T0, 5);
        items.extend(one_second_chunks(T0 + 5 * S + STEP as u64, 10));
        items.push(Item::Step(2 * S as i64));
        items.extend(one_second_chunks(T0 + 17 * S + STEP as u64, 5));
        let t = run(&items);
        assert_eq!(t.clock_steps, 2);
        assert_eq!(
            t.duration_ns,
            (5 * S - S / 10) + (10 * S - S / 10) + (5 * S - S / 10)
        );
    }

    #[test]
    fn empty_input_is_zero() {
        let t = run(&[]);
        assert_eq!(t.duration_ns, 0);
        assert_eq!(t.clock_steps, 0);
    }

    #[test]
    fn clock_step_parses_signed_decimal_strings() {
        let mut metadata = BTreeMap::new();
        metadata.insert("step_ns".to_string(), "-41054973261956000".to_string());
        metadata.insert("log_time_before".to_string(), "1".to_string());
        let record = mcap::records::Metadata {
            name: CLOCK_STEP_METADATA.to_string(),
            metadata,
        };
        let step = ClockStep::from_metadata(42, &record).unwrap();
        assert_eq!(step.step_ns, -41_054_973_261_956_000);
        assert_eq!(step.offset, 42);
    }

    #[test]
    fn other_metadata_and_bad_values_are_ignored() {
        let mut metadata = BTreeMap::new();
        metadata.insert("step_ns".to_string(), "not a number".to_string());
        let bad = mcap::records::Metadata {
            name: CLOCK_STEP_METADATA.to_string(),
            metadata,
        };
        assert!(ClockStep::from_metadata(0, &bad).is_none());
        let other = mcap::records::Metadata {
            name: "clock_sync".to_string(),
            metadata: BTreeMap::new(),
        };
        assert!(ClockStep::from_metadata(0, &other).is_none());
    }

    #[test]
    fn channel_spans_sum_per_segment() {
        let mut spans = ChannelSpans::default();
        spans.push(
            0,
            7,
            Span {
                start_ns: 10 * S,
                end_ns: 11 * S,
            },
        );
        spans.push(
            0,
            7,
            Span {
                start_ns: 12 * S,
                end_ns: 13 * S,
            },
        );
        spans.push(
            1,
            7,
            Span {
                start_ns: T0,
                end_ns: T0 + 2 * S,
            },
        );
        spans.push(
            1,
            9,
            Span {
                start_ns: T0 + S,
                end_ns: T0 + S,
            },
        );
        let totals = spans.finish();
        assert_eq!(totals[&7].span_ns, 3 * S + 2 * S);
        assert_eq!(totals[&9].span_ns, 0);
    }

    const HOUR: i64 = 3_600 * S as i64;
    const PRE_END: u64 = T0 + 10 * S - S / 10;

    /// Ten 1 s spans, a step, ten more. `strays` single-message spans on the
    /// pre-step timeline are written after the first post-step message.
    fn stray_after_step(step_ns: i64, record: bool, strays: u64) -> Vec<Item> {
        let post = T0.checked_add_signed(10 * S as i64 + step_ns).unwrap();
        let mut items = one_second_chunks(T0, 10);
        if record {
            items.push(Item::Step(step_ns));
        }
        items.push(Item::Span(post, post));
        for i in 0..strays {
            let t = PRE_END - 2_000_000 + i * 1_000_000;
            items.push(Item::Span(t, t));
        }
        items.extend(one_second_chunks(post, 10));
        items
    }

    #[test]
    fn stray_span_after_backward_record_is_not_a_step() {
        let t = run(&stray_after_step(-HOUR, true, 1));
        assert_eq!(t.clock_steps, 1);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
    }

    #[test]
    fn stray_span_after_backward_gap_is_not_a_step() {
        let t = run(&stray_after_step(-HOUR, false, 1));
        assert_eq!(t.clock_steps, 1);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
    }

    #[test]
    fn stray_span_after_forward_record_is_not_a_step() {
        let t = run(&stray_after_step(HOUR, true, 1));
        assert_eq!(t.clock_steps, 1);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
    }

    #[test]
    fn two_stray_spans_in_a_row_are_one_excursion() {
        let t = run(&stray_after_step(-HOUR, true, 2));
        assert_eq!(t.clock_steps, 1);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
    }

    #[test]
    fn genuine_round_trip_counts_both_steps() {
        let mut items = one_second_chunks(T0, 10);
        items.extend(one_second_chunks(T0 + 10 * S + HOUR as u64, 20));
        items.extend(one_second_chunks(T0 + 30 * S, 10));
        let t = run(&items);
        assert_eq!(t.clock_steps, 2);
        assert_eq!(
            t.duration_ns,
            (10 * S - S / 10) + (20 * S - S / 10) + (10 * S - S / 10)
        );
    }

    #[test]
    fn genuine_round_trip_with_records_counts_both_steps() {
        let mut items = one_second_chunks(T0, 10);
        items.push(Item::Step(HOUR));
        items.extend(one_second_chunks(T0 + 10 * S + HOUR as u64, 20));
        items.push(Item::Step(-HOUR));
        items.extend(one_second_chunks(T0 + 30 * S, 10));
        let t = run(&items);
        assert_eq!(t.clock_steps, 2);
        assert_eq!(
            t.duration_ns,
            (10 * S - S / 10) + (20 * S - S / 10) + (10 * S - S / 10)
        );
    }

    #[test]
    fn excursion_reopens_its_segment_and_retires_its_own() {
        let point = |t: u64| Span {
            start_ns: t,
            end_ns: t,
        };
        let post = T0 + 10 * S - HOUR as u64;
        let mut acc = TimelineAccumulator::default();
        acc.push(point(T0));
        acc.clock_step(&ClockStep {
            offset: 0,
            step_ns: -HOUR,
        });
        acc.push(point(post));
        assert_eq!(acc.segment(), 1);
        acc.push(point(PRE_END));
        assert_eq!(acc.segment(), 2);
        acc.push(point(post + S));
        assert_eq!(acc.segment(), 1, "the excursion re-opens its segment");
        acc.push(point(post + HOUR as u64 * 2));
        assert_eq!(acc.segment(), 3, "an excursion's index is not reused");
        assert_eq!(acc.finish().clock_steps, 2);
    }

    #[test]
    fn channel_spans_count_the_segments_each_channel_is_in() {
        let span = |start_ns: u64, end_ns: u64| Span { start_ns, end_ns };
        let mut spans = ChannelSpans::default();
        spans.push(0, 7, span(T0, T0 + S));
        spans.push(0, 7, span(T0 + 2 * S, T0 + 3 * S));
        spans.push(2, 7, span(T0, T0 + S));
        spans.push(2, 9, span(T0, T0 + S));
        spans.push(3, 7, span(T0 + 5 * S, T0 + 6 * S));
        let totals = spans.finish_merged(&[(3, 2)], &[]);
        assert_eq!(totals[&7].segments, 2);
        assert_eq!(totals[&7].span_ns, 3 * S + 6 * S);
        assert_eq!(totals[&9].segments, 1);
    }

    #[test]
    fn channel_spans_merge_a_reopened_segment() {
        let span = |start_ns: u64, end_ns: u64| Span { start_ns, end_ns };
        let mut spans = ChannelSpans::default();
        spans.push(0, 7, span(T0, T0 + 4 * S));
        spans.push(1, 7, span(PRE_END, PRE_END));
        spans.push(0, 7, span(T0 + 5 * S, T0 + 9 * S));
        spans.push(2, 7, span(T0 + HOUR as u64, T0 + HOUR as u64 + S));
        let totals = spans.finish();
        assert_eq!(totals[&7].span_ns, 9 * S + S);
    }

    #[test]
    fn segment_index_advances_on_each_close() {
        let mut acc = TimelineAccumulator::default();
        assert_eq!(acc.segment(), 0);
        acc.push(Span {
            start_ns: T0,
            end_ns: T0 + S,
        });
        acc.clock_step(&ClockStep {
            offset: 0,
            step_ns: STEP,
        });
        assert_eq!(acc.segment(), 1);
        acc.push(Span {
            start_ns: T0 + STEP as u64,
            end_ns: T0 + STEP as u64 + S,
        });
        acc.push(Span {
            start_ns: T0,
            end_ns: T0 + S,
        }); // backward gap
        assert_eq!(acc.segment(), 2);
    }

    /// Ten 1 s spans, a pause of `pause` after the last one ends, ten more.
    fn paused(pause: u64) -> Vec<Item> {
        let mut items = one_second_chunks(T0, 10);
        items.extend(one_second_chunks(PRE_END + pause, 10));
        items
    }

    #[test]
    fn pause_counts_when_records_are_authoritative() {
        let t = run_synced(&paused(20 * S));
        assert_eq!(t.clock_steps, 0);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10) + 20 * S);
    }

    #[test]
    fn pause_is_a_step_without_clock_sync() {
        let t = run(&paused(20 * S));
        assert_eq!(t.clock_steps, 1);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
    }

    #[test]
    fn short_run_after_a_pause_at_the_end_is_still_a_pause() {
        let mut items = one_second_chunks(T0, 10);
        items.extend(one_second_chunks(PRE_END + 20 * S, 2));
        let t = run_synced(&items);
        assert_eq!(t.clock_steps, 0);
        assert_eq!(t.duration_ns, (10 * S - S / 10) + 20 * S + (2 * S - S / 10));
    }

    #[test]
    fn consecutive_pauses_join_one_segment() {
        let mut items = one_second_chunks(T0, 10);
        let second = PRE_END + 20 * S;
        items.extend(one_second_chunks(second, 2));
        let third = second + 2 * S - S / 10 + 30 * S;
        items.extend(one_second_chunks(third, 10));
        let t = run_synced(&items);
        assert_eq!(t.clock_steps, 0);
        assert_eq!(t.duration_ns, third + 10 * S - S / 10 - T0);
    }

    #[test]
    fn recorded_forward_step_is_excluded_when_records_are_authoritative() {
        let mut items = one_second_chunks(T0, 10);
        items.push(Item::Step(STEP));
        items.extend(one_second_chunks(T0 + 10 * S + STEP as u64, 10));
        let t = run_synced(&items);
        assert_eq!(t.clock_steps, 1);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
    }

    #[test]
    fn record_after_a_forward_gap_makes_it_a_step() {
        let mut items = one_second_chunks(T0, 5);
        items.extend(one_second_chunks(T0 + 5 * S + STEP as u64, 1));
        items.push(Item::Step(STEP));
        items.extend(one_second_chunks(T0 + 6 * S + STEP as u64, 4));
        let t = run_synced(&items);
        assert_eq!(t.clock_steps, 1);
        assert!(
            t.duration_ns >= 9 * S && t.duration_ns <= 10 * S,
            "{}",
            t.duration_ns
        );
    }

    #[test]
    fn stray_bounce_around_a_record_is_one_step_when_authoritative() {
        for step_ns in [HOUR, -HOUR] {
            for strays in [1, 2] {
                let t = run_synced(&stray_after_step(step_ns, true, strays));
                assert_eq!(t.clock_steps, 1, "step {step_ns} strays {strays}");
                assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
            }
        }
    }

    #[test]
    fn unrecorded_stray_bounce_adds_nothing_when_authoritative() {
        for offset in [HOUR, -HOUR] {
            let mut items = one_second_chunks(T0, 10);
            let stray = (T0 + 10 * S).checked_add_signed(offset).unwrap();
            items.push(Item::Span(stray, stray));
            items.extend(one_second_chunks(T0 + 10 * S, 10));
            let t = run_synced(&items);
            assert_eq!(t.clock_steps, 0, "offset {offset}");
            assert_eq!(t.duration_ns, 20 * S - S / 10, "offset {offset}");
        }
    }

    #[test]
    fn unrecorded_backward_jump_splits_but_is_not_counted_when_authoritative() {
        let mut items = one_second_chunks(T0 + STEP as u64, 10);
        items.extend(one_second_chunks(T0, 10));
        let t = run_synced(&items);
        assert_eq!(t.clock_steps, 0);
        assert_eq!(t.duration_ns, 2 * (10 * S - S / 10));
    }

    /// Ten 1 s spans, a 20 s unrecorded pause, two 1 s spans, then a
    /// recorded step of `step_ns` and ten more 1 s spans.
    fn pause_then_recorded_step(step_ns: i64) -> Vec<Item> {
        let mut items = one_second_chunks(T0, 10);
        let resume = PRE_END + 20 * S;
        items.extend(one_second_chunks(resume, 2));
        items.push(Item::Step(step_ns));
        let after = (resume + 2 * S).checked_add_signed(step_ns).unwrap();
        items.extend(one_second_chunks(after, 10));
        items
    }

    /// Ten 1 s spans, then a jump of a 20 s pause plus `step_ns`, two 1 s
    /// spans, the record of `step_ns`, and ten more 1 s spans.
    fn step_during_pause(step_ns: i64) -> Vec<Item> {
        let mut items = one_second_chunks(T0, 10);
        let resume = (PRE_END + 20 * S).checked_add_signed(step_ns).unwrap();
        items.extend(one_second_chunks(resume, 2));
        items.push(Item::Step(step_ns));
        items.extend(one_second_chunks(resume + 2 * S, 10));
        items
    }

    #[test]
    fn record_after_a_step_during_a_pause_describes_it() {
        for step_ns in [STEP, -HOUR] {
            let t = run_synced(&step_during_pause(step_ns));
            assert_eq!(t.clock_steps, 1, "step {step_ns}");
            assert_eq!(
                t.duration_ns,
                (10 * S - S / 10) + 20 * S + (12 * S - S / 10),
                "step {step_ns}"
            );

            let t = run(&step_during_pause(step_ns));
            assert_eq!(t.clock_steps, 1, "step {step_ns}");
            assert_eq!(
                t.duration_ns,
                (10 * S - S / 10) + (12 * S - S / 10),
                "step {step_ns}"
            );
        }
    }

    #[test]
    fn opposite_record_after_a_pause_is_its_own_step() {
        let t = run_synced(&pause_then_recorded_step(-HOUR));
        assert_eq!(t.clock_steps, 1);
        let before = PRE_END + 20 * S + 2 * S - S / 10 - T0;
        assert_eq!(t.duration_ns, before + (10 * S - S / 10));
    }

    #[test]
    fn record_after_an_unrelated_pause_is_its_own_step_when_authoritative() {
        let t = run_synced(&pause_then_recorded_step(STEP));
        assert_eq!(t.clock_steps, 1);
        let before = PRE_END + 20 * S + 2 * S - S / 10 - T0;
        assert_eq!(t.duration_ns, before + (10 * S - S / 10));
    }

    #[test]
    fn record_not_matching_a_gap_is_a_separate_step() {
        for step_ns in [STEP, -2 * S as i64] {
            let t = run(&pause_then_recorded_step(step_ns));
            assert_eq!(t.clock_steps, 2, "step {step_ns}");
            assert_eq!(
                t.duration_ns,
                2 * (10 * S - S / 10) + (2 * S - S / 10),
                "step {step_ns}"
            );
        }
    }

    #[test]
    fn abandoned_excursion_adds_no_channel_segment_or_span() {
        let point = |t: u64| Span {
            start_ns: t,
            end_ns: t,
        };
        let post = T0 + 10 * S - HOUR as u64;
        for strays in [1, 2] {
            let mut acc = TimelineAccumulator::default();
            let mut spans = ChannelSpans::default();
            let mut feed = |acc: &mut TimelineAccumulator, channel: u16, t: u64| {
                acc.push(point(t));
                spans.push(acc.segment(), channel, point(t));
            };
            for i in 0..100 {
                feed(&mut acc, 7, T0 + i * S / 10);
            }
            acc.clock_step(&ClockStep {
                offset: 0,
                step_ns: -HOUR,
            });
            feed(&mut acc, 7, post);
            for i in 0..strays {
                feed(&mut acc, 7, PRE_END - 2_000_000 + i * 1_000_000);
                feed(&mut acc, 9, PRE_END - 2_000_000 + i * 1_000_000);
            }
            for i in 1..100 {
                feed(&mut acc, 7, post + i * S / 10);
            }
            let (timeline, totals) = acc.finish_with_spans(spans);
            assert_eq!(timeline.clock_steps, 1, "strays {strays}");
            assert_eq!(
                totals[&7],
                ChannelExtent {
                    span_ns: 2 * (99 * S / 10),
                    segments: 2
                },
                "strays {strays}"
            );
            assert_eq!(totals[&9], ChannelExtent::default(), "strays {strays}");
        }
    }

    #[test]
    fn pause_merges_channel_spans_across_it() {
        let point = |t: u64| Span {
            start_ns: t,
            end_ns: t,
        };
        let mut acc = TimelineAccumulator::with_authoritative_records();
        let mut spans = ChannelSpans::default();
        for i in 0..100 {
            let t = T0 + i * S / 10;
            acc.push(point(t));
            spans.push(acc.segment(), 7, point(t));
        }
        for i in 0..100 {
            let t = PRE_END + 20 * S + i * S / 10;
            acc.push(point(t));
            spans.push(acc.segment(), 7, point(t));
        }
        let (timeline, totals) = acc.finish_with_spans(spans);
        assert_eq!(timeline.clock_steps, 0);
        assert_eq!(totals[&7].span_ns, PRE_END + 20 * S + 99 * S / 10 - T0);
        assert_eq!(totals[&7].segments, 1);
    }
}
