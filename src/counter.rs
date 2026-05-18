//! Per-line aggregation counter: groups matched lines by a key tuple
//! (and optionally a time bucket), tracks per-group count + observed
//! timestamp range, and produces either an end-of-run report or a
//! streaming flush when running under follow + bucket.

use rapidhash::RapidHashMap;
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;
use std::collections::BTreeMap;
use std::io::Write;

use crate::bucket::ResolvedBucket;
use crate::output;
use crate::raw_extractor::unescape_for_key;
use crate::timestamp::Timestamp;

pub(crate) type Combo = SmallVec<[SmartString; 3]>;

/// Sort order for emitting aggregated rows.
enum SortOrder {
    /// `report` (batched end-of-run): largest groups first.
    /// `count desc, combo asc, bucket asc`.
    CountDesc,
    /// `flush_closed` (streaming mid-run) and `flush_remaining`
    /// (streaming end-of-run): chronological.
    /// `bucket asc, count desc, combo asc`.
    BucketAsc,
}

/// Aggregated stats for a single (group-keys [, bucket]) combination.
#[derive(Default)]
pub(crate) struct GroupStats {
    pub(crate) count: usize,
    pub(crate) min_ts: Option<Timestamp>,
    pub(crate) max_ts: Option<Timestamp>,
}

#[inline]
pub(crate) fn fold_min(slot: &mut Option<Timestamp>, t: Timestamp) {
    *slot = Some(slot.map_or(t, |cur| cur.min(t)));
}

#[inline]
pub(crate) fn fold_max(slot: &mut Option<Timestamp>, t: Timestamp) {
    *slot = Some(slot.map_or(t, |cur| cur.max(t)));
}

impl GroupStats {
    fn record(&mut self, ts: Option<Timestamp>) {
        self.count += 1;
        if let Some(t) = ts {
            fold_min(&mut self.min_ts, t);
            fold_max(&mut self.max_ts, t);
        }
    }

    fn merge(&mut self, other: GroupStats) {
        self.count += other.count;
        if let Some(t) = other.min_ts {
            fold_min(&mut self.min_ts, t);
        }
        if let Some(t) = other.max_ts {
            fold_max(&mut self.max_ts, t);
        }
    }
}

/// Run-time mode of a `Counter`. Decided once at `Counter::new` from
/// `(bucket, streaming_close_grace_nanos)` and never changes thereafter.
///
/// Encoding the two modes (batched end-of-run report vs. per-bucket
/// streaming flush) as a sum type makes the precondition impossible to
/// violate: `Streaming` only exists when a bucket is also present, so the
/// streaming-flush methods can `let Streaming { .. } = mode` without
/// re-checking `bucket.is_some()`.
#[derive(Default)]
pub(crate) enum CounterMode {
    #[default]
    Batched,
    Streaming {
        /// Reorder grace; mirrors `--window-secs`. Bucket B is closed (and
        /// streamed out) once `max_ts_seen > B.end + close_grace_nanos`.
        close_grace_nanos: i64,
        /// Highest timestamp observed across all matched lines. Used to
        /// decide which buckets are past the close threshold.
        max_ts_seen: Option<Timestamp>,
    },
}

/// Groups matched lines by the value tuple of `keys` (and optionally a time
/// bucket) and records per-group count + observed timestamp range. Missing
/// user keys produce an empty value slot (rendered as `key.<k>=""` in the
/// logfmt report).
#[derive(Default)]
pub(crate) struct Counter {
    keys: Vec<SmartString>,
    bucket: Option<ResolvedBucket>,
    counts: BTreeMap<Option<Timestamp>, RapidHashMap<Combo, GroupStats>>,
    scratch: String,
    mode: CounterMode,
}

impl Counter {
    /// Build a counter. `streaming_close_grace_nanos`:
    /// - `None` → batched (end-of-run report only).
    /// - `Some(g)` with `bucket: Some(_)` → streaming with reorder grace `g`.
    /// - `Some(_)` with `bucket: None` → silently downgraded to batched
    ///   (streaming an unbucketed group has no completion signal). Preserves
    ///   the pre-refactor `enable_streaming` no-op behaviour.
    pub(crate) fn new(
        keys: Vec<SmartString>,
        bucket: Option<ResolvedBucket>,
        streaming_close_grace_nanos: Option<i64>,
    ) -> Self {
        let mode = match (bucket, streaming_close_grace_nanos) {
            (Some(_), Some(close_grace_nanos)) => CounterMode::Streaming {
                close_grace_nanos,
                max_ts_seen: None,
            },
            _ => CounterMode::Batched,
        };
        Self {
            keys,
            bucket,
            counts: BTreeMap::default(),
            scratch: String::new(),
            mode,
        }
    }

    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        !self.keys.is_empty() || self.bucket.is_some()
    }

    pub(crate) fn record(&mut self, pairs: &[(&str, &str)], ts: Option<Timestamp>) {
        if !self.is_active() {
            return;
        }
        let bucket_ts = match (self.bucket, ts) {
            (Some(b), Some(t)) => Some(b.floor(t)),
            // Bucketing on but the line has no timestamp — can't place it.
            (Some(_), None) => return,
            (None, _) => None,
        };

        if let CounterMode::Streaming { max_ts_seen, .. } = &mut self.mode {
            if let Some(t) = ts {
                fold_max(max_ts_seen, t);
            }
        }

        let mut combo: Combo = SmallVec::with_capacity(self.keys.len());
        for k in &self.keys {
            let mut value = SmartString::new_const();
            if let Some(v) = unescape_for_key(pairs, k, &mut self.scratch) {
                value.push_str(v);
            }
            combo.push(value);
        }
        self.counts
            .entry(bucket_ts)
            .or_default()
            .entry(combo)
            .or_default()
            .record(ts);
    }

    fn write_row<W: Write + ?Sized>(
        &self,
        out: &mut W,
        combo: &Combo,
        bucket_ts: Option<Timestamp>,
        stats: &GroupStats,
        tz: &jiff::tz::TimeZone,
        json: bool,
    ) -> std::io::Result<()> {
        let bucket = match (self.bucket, bucket_ts) {
            (Some(bspec), Some(start)) => Some((start, start + bspec.dur_nanos)),
            _ => None,
        };
        let time_range = match (stats.min_ts, stats.max_ts) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        };
        let count = stats.count as u64;
        if json {
            crate::json::write_agg_row_json(out, count, &self.keys, combo, bucket, time_range, tz)
        } else {
            output::write_agg_row_logfmt(out, count, &self.keys, combo, bucket, time_range, tz)
        }
    }

    /// Sort `entries` per `order` and write each row via `write_row`. Does
    /// NOT flush `out` — callers decide flush points.
    fn emit_rows<'a, W: Write + ?Sized>(
        &self,
        out: &mut W,
        tz: &jiff::tz::TimeZone,
        json: bool,
        mut entries: Vec<(&'a Combo, Option<Timestamp>, &'a GroupStats)>,
        order: SortOrder,
    ) -> std::io::Result<()> {
        match order {
            SortOrder::CountDesc => entries.sort_unstable_by(|a, b| {
                b.2.count
                    .cmp(&a.2.count)
                    .then_with(|| a.0.cmp(b.0))
                    .then_with(|| a.1.cmp(&b.1))
            }),
            SortOrder::BucketAsc => entries.sort_unstable_by(|a, b| {
                a.1.cmp(&b.1)
                    .then_with(|| b.2.count.cmp(&a.2.count))
                    .then_with(|| a.0.cmp(b.0))
            }),
        }
        for (combo, bucket_ts, stats) in entries {
            self.write_row(out, combo, bucket_ts, stats, tz, json)?;
        }
        Ok(())
    }

    /// Batch-mode end-of-run report: count desc, ties broken by key.
    fn report<W: Write>(
        &self,
        out: &mut W,
        tz: &jiff::tz::TimeZone,
        json: bool,
    ) -> std::io::Result<()> {
        if !self.is_active() || self.counts.is_empty() {
            return Ok(());
        }
        let entries: Vec<_> = self
            .counts
            .iter()
            .flat_map(|(bucket_ts, groups)| {
                groups
                    .iter()
                    .map(move |(combo, stats)| (combo, *bucket_ts, stats))
            })
            .collect();
        self.emit_rows(out, tz, json, entries, SortOrder::CountDesc)
    }

    /// Streaming flush: emit and remove every group whose bucket has
    /// passed the close threshold (`bucket.end + close_grace_nanos <
    /// max_ts_seen`). Rows go out in (bucket.start asc, count desc, combo
    /// asc) order. No-op in `Batched` mode.
    pub(crate) fn flush_closed<W: Write + ?Sized>(
        &mut self,
        out: &mut W,
        tz: &jiff::tz::TimeZone,
        json: bool,
    ) -> std::io::Result<()> {
        let CounterMode::Streaming {
            close_grace_nanos,
            max_ts_seen,
        } = &self.mode
        else {
            return Ok(());
        };
        let Some(bspec) = self.bucket else {
            return Ok(());
        };
        let Some(seen) = *max_ts_seen else {
            return Ok(());
        };

        let close_threshold = seen - bspec.dur_nanos - *close_grace_nanos;

        let mut open_buckets = self.counts.split_off(&Some(close_threshold));
        if let Some(none_groups) = self.counts.remove(&None) {
            open_buckets.insert(None, none_groups);
        }

        if self.counts.is_empty() {
            self.counts = open_buckets;
            return Ok(());
        }

        let closing_counts = std::mem::replace(&mut self.counts, open_buckets);
        let entries: Vec<_> = closing_counts
            .iter()
            .flat_map(|(bucket_ts, groups)| {
                groups
                    .iter()
                    .map(move |(combo, stats)| (combo, *bucket_ts, stats))
            })
            .collect();
        self.emit_rows(out, tz, json, entries, SortOrder::BucketAsc)?;
        out.flush()
    }

    /// End-of-stream flush: emit any still-open buckets in time order.
    /// Used in place of `report` when streaming.
    fn flush_remaining<W: Write>(
        &self,
        out: &mut W,
        tz: &jiff::tz::TimeZone,
        json: bool,
    ) -> std::io::Result<()> {
        if self.counts.is_empty() {
            return Ok(());
        }
        let entries: Vec<_> = self
            .counts
            .iter()
            .flat_map(|(bucket_ts, groups)| {
                groups
                    .iter()
                    .map(move |(combo, stats)| (combo, *bucket_ts, stats))
            })
            .collect();
        self.emit_rows(out, tz, json, entries, SortOrder::BucketAsc)
    }

    /// End-of-run output: dispatches between batched `report` (count desc,
    /// single emission) and streaming `flush_remaining` (still-open buckets
    /// in time order) based on `self.mode`. This is the only public
    /// end-of-run entry point; the two underlying methods stay private to
    /// this module so callers can't accidentally pick the wrong one.
    pub(crate) fn emit_final<W: Write>(
        &self,
        out: &mut W,
        tz: &jiff::tz::TimeZone,
        json: bool,
    ) -> std::io::Result<()> {
        match self.mode {
            CounterMode::Batched => self.report(out, tz, json),
            CounterMode::Streaming { .. } => self.flush_remaining(out, tz, json),
        }
    }

    pub(crate) fn merge(&mut self, other: Self) {
        // Only fold `max_ts_seen` when both sides are streaming. In practice
        // workers always batch and the master decides streaming once, so
        // mixed pairs only occur if the call path changes; be defensive and
        // leave `self.mode` untouched in any non-(Streaming, Streaming) case.
        if let (
            CounterMode::Streaming {
                max_ts_seen: self_seen,
                ..
            },
            CounterMode::Streaming {
                max_ts_seen: Some(other_seen),
                ..
            },
        ) = (&mut self.mode, &other.mode)
        {
            fold_max(self_seen, *other_seen);
        }
        for (bucket_ts, groups) in other.counts {
            match self.counts.entry(bucket_ts) {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(groups);
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    let self_groups = e.get_mut();
                    for (combo, stats) in groups {
                        self_groups.entry(combo).or_default().merge(stats);
                    }
                }
            }
        }
    }
}
