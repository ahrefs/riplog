//! Per-line aggregation counter: groups matched lines by a key tuple
//! (and optionally a time bucket), tracks per-group count + observed
//! timestamp range, and produces either an end-of-run report or a
//! streaming flush when running under follow + bucket.

use rapidhash::RapidHashMap;
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;
use std::collections::BTreeMap;
use std::io::Write;

use crate::bucket::BucketSpec;
use crate::output;
use crate::raw_extractor::unescape_for_key;
use crate::timestamp::Timestamp;

pub(crate) type Combo = SmallVec<[SmartString; 3]>;

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

/// Groups matched lines by the value tuple of `keys` (and optionally a time
/// bucket) and records per-group count + observed timestamp range. Missing
/// user keys produce an empty value slot (rendered as `key.<k>=""` in the
/// logfmt report).
#[derive(Default)]
pub(crate) struct Counter {
    keys: Vec<SmartString>,
    bucket: Option<BucketSpec>,
    counts: BTreeMap<Option<Timestamp>, RapidHashMap<Combo, GroupStats>>,
    scratch: String,
    /// In streaming mode, track the highest timestamp observed across all
    /// matched lines. Used to decide which buckets are past the close
    /// threshold (`bucket.end + close_grace_nanos`).
    max_ts_seen: Option<Timestamp>,
    /// Reorder grace; mirrors `--window-secs`. Bucket B is closed (and
    /// streamed out) once `max_ts_seen > B.end + close_grace_nanos`.
    close_grace_nanos: i64,
    /// Set in follow mode when `--bucket` is active. When `false`, the
    /// streaming flush methods are no-ops and end-of-process output goes
    /// through `report` (today's count-desc, single-emission behaviour).
    pub(crate) streaming: bool,
}

impl Counter {
    pub(crate) fn new(keys: Vec<SmartString>, bucket: Option<BucketSpec>) -> Self {
        Self {
            keys,
            bucket,
            counts: BTreeMap::default(),
            scratch: String::new(),
            max_ts_seen: None,
            close_grace_nanos: 0,
            streaming: false,
        }
    }

    /// Enable streaming output: per-bucket rows emit as soon as
    /// `max_ts_seen > bucket.end + close_grace_nanos`. No-op unless
    /// `bucket` is also set (streaming an unbucketed group has no
    /// completion signal).
    pub(crate) fn enable_streaming(&mut self, close_grace_nanos: i64) {
        if self.bucket.is_some() {
            self.streaming = true;
            self.close_grace_nanos = close_grace_nanos;
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

        if self.streaming {
            if let Some(t) = ts {
                fold_max(&mut self.max_ts_seen, t);
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
            (Some(bspec), Some(start)) => Some((start, start + bspec.nanos)),
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

    /// Batch-mode end-of-run report: count desc, ties broken by key.
    pub(crate) fn report<W: Write>(
        &self,
        out: &mut W,
        tz: &jiff::tz::TimeZone,
        json: bool,
    ) -> std::io::Result<()> {
        if !self.is_active() || self.counts.is_empty() {
            return Ok(());
        }
        let mut entries = Vec::new();
        for (bucket_ts, groups) in &self.counts {
            for (combo, stats) in groups {
                entries.push((combo, *bucket_ts, stats));
            }
        }
        entries.sort_unstable_by(|a, b| {
            b.2.count
                .cmp(&a.2.count)
                .then_with(|| a.0.cmp(b.0))
                .then_with(|| a.1.cmp(&b.1))
        });
        for (combo, bucket_ts, stats) in entries {
            self.write_row(out, combo, bucket_ts, stats, tz, json)?;
        }
        Ok(())
    }

    /// Streaming flush: emit and remove every group whose bucket has
    /// passed the close threshold (`bucket.end + close_grace_nanos <
    /// max_ts_seen`). Rows go out in (bucket.start asc, count desc, combo
    /// asc) order. No-op when `streaming` is false.
    pub(crate) fn flush_closed<W: Write + ?Sized>(
        &mut self,
        out: &mut W,
        tz: &jiff::tz::TimeZone,
        json: bool,
    ) -> std::io::Result<()> {
        if !self.streaming {
            return Ok(());
        }
        let Some(bspec) = self.bucket else {
            return Ok(());
        };
        let Some(seen) = self.max_ts_seen else {
            return Ok(());
        };

        let close_threshold = seen - bspec.nanos - self.close_grace_nanos;

        let mut open_buckets = self.counts.split_off(&Some(close_threshold));
        if let Some(none_groups) = self.counts.remove(&None) {
            open_buckets.insert(None, none_groups);
        }

        if self.counts.is_empty() {
            self.counts = open_buckets;
            return Ok(());
        }

        let mut to_close = Vec::new();
        let closing_counts = std::mem::replace(&mut self.counts, open_buckets);
        for (bucket_ts, groups) in closing_counts.into_iter() {
            for (combo, stats) in groups {
                to_close.push((combo, bucket_ts, stats));
            }
        }

        // Since we extracted them in bucket order, and split_off splits at bucket level,
        // we can just sort to_close as needed.
        // stream_order is: bucket_ts asc, count desc, combo asc.
        to_close.sort_unstable_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.count.cmp(&a.2.count))
                .then_with(|| a.0.cmp(&b.0))
        });
        for (combo, bucket_ts, stats) in &to_close {
            self.write_row(out, combo, *bucket_ts, stats, tz, json)?;
        }
        out.flush()
    }

    /// End-of-stream flush: emit any still-open buckets in time order.
    /// Used in place of `report` when streaming.
    pub(crate) fn flush_remaining<W: Write>(
        &self,
        out: &mut W,
        tz: &jiff::tz::TimeZone,
        json: bool,
    ) -> std::io::Result<()> {
        if self.counts.is_empty() {
            return Ok(());
        }
        let mut entries = Vec::new();
        for (bucket_ts, groups) in &self.counts {
            for (combo, stats) in groups {
                entries.push((combo, *bucket_ts, stats));
            }
        }
        entries.sort_unstable_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| b.2.count.cmp(&a.2.count))
                .then_with(|| a.0.cmp(b.0))
        });
        for (combo, bucket_ts, stats) in entries {
            self.write_row(out, combo, bucket_ts, stats, tz, json)?;
        }
        Ok(())
    }

    pub(crate) fn merge(&mut self, other: Self) {
        if let Some(t) = other.max_ts_seen {
            fold_max(&mut self.max_ts_seen, t);
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
