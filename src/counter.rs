//! Per-line aggregation counter: groups matched lines by a key tuple
//! (and optionally a time bucket), tracks per-group `Aggregates` (count,
//! observed timestamp range, plus any numeric aggregates), and produces
//! either an end-of-run report or a streaming flush when running under
//! follow + bucket.

use rapidhash::RapidHashMap;
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;
use std::collections::BTreeMap;
use std::io::Write;

use crate::aggregate::{AggregateSpec, Aggregates};
use crate::bucket::ResolvedBucket;
use crate::output::{Formatter, OutputFormat};
use crate::raw_extractor::unescape_for_key;
use crate::timestamp::{fold_max, Timestamp};

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
/// bucket) and records per-group `Aggregates`. Missing user keys produce
/// an empty value slot (rendered as `key.<k>=""` in the logfmt report).
#[derive(Default)]
pub(crate) struct Counter {
    keys: Vec<SmartString>,
    bucket: Option<ResolvedBucket>,
    spec: AggregateSpec,
    counts: BTreeMap<Option<Timestamp>, RapidHashMap<Combo, Aggregates>>,
    scratch: String,
    mode: CounterMode,
    /// Cached `!keys.is_empty() || bucket.is_some() || spec.has_numeric()`.
    /// Read twice per line on the hot path (timestamp-extract gate +
    /// `record` short-circuit); recomputing it forced 4 dependent loads
    /// through `spec`'s `Vec` lengths per call.
    is_active: bool,
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
        spec: AggregateSpec,
        streaming_close_grace_nanos: Option<i64>,
    ) -> Self {
        let mode = match (bucket, streaming_close_grace_nanos) {
            (Some(_), Some(close_grace_nanos)) => CounterMode::Streaming {
                close_grace_nanos,
                max_ts_seen: None,
            },
            _ => CounterMode::Batched,
        };
        let is_active = !keys.is_empty() || bucket.is_some() || spec.has_numeric();
        Self {
            keys,
            bucket,
            spec,
            counts: BTreeMap::default(),
            scratch: String::new(),
            mode,
            is_active,
        }
    }

    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        // Cached at construction: bare `--count` alone is handled by
        // `count_only` in `emit_summaries`; the counter only needs to fire
        // when there are group keys, a bucket, or a numeric aggregate.
        self.is_active
    }

    /// Borrow the spec — used by the formatter to know which aggregate
    /// fields to emit.
    pub(crate) fn spec(&self) -> &AggregateSpec {
        &self.spec
    }

    /// Iterate over every per-group `Aggregates` (read-only). Used by
    /// end-of-run warning collection.
    pub(crate) fn iter_aggregates(&self) -> impl Iterator<Item = &Aggregates> {
        self.counts.values().flat_map(|groups| groups.values())
    }

    #[inline]
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
        let spec = &self.spec;
        let scratch = &mut self.scratch;
        self.counts
            .entry(bucket_ts)
            .or_default()
            .entry(combo)
            .or_insert_with(|| Aggregates::new(spec))
            .record(pairs, ts, scratch);
    }

    /// Sort `entries` per `order` and emit each row through `formatter`.
    /// Does NOT flush `out`. Takes `bucket`/`keys`/`spec` directly so the
    /// caller can hold a mutable borrow on `self.counts` across the call
    /// (the per-entry `&mut Aggregates` overlaps that borrow).
    #[allow(clippy::too_many_arguments)]
    fn emit_rows<'a, W: Write + ?Sized>(
        out: &mut W,
        formatter: &Formatter,
        tz: &jiff::tz::TimeZone,
        bucket: Option<ResolvedBucket>,
        keys: &[SmartString],
        spec: &AggregateSpec,
        mut entries: Vec<(&'a Combo, Option<Timestamp>, &'a mut Aggregates)>,
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
        for (combo, bucket_ts, aggregates) in entries {
            let bucket_range = match (bucket, bucket_ts) {
                (Some(bspec), Some(start)) => Some((start, start + bspec.dur_nanos)),
                _ => None,
            };
            formatter.agg_row(out, spec, aggregates, keys, combo, bucket_range, tz)?;
        }
        Ok(())
    }

    /// Batch-mode end-of-run report: count desc, ties broken by key.
    fn report<W: Write>(
        &mut self,
        out: &mut W,
        formatter: &Formatter,
        tz: &jiff::tz::TimeZone,
    ) -> std::io::Result<()> {
        if !self.is_active() {
            return Ok(());
        }
        // Empty-combo bare row: when aggregates are active but there's no
        // grouping/bucketing and no lines matched, we still want a single
        // row (count=0, no aggregate fields). Construct one on the fly.
        if self.counts.is_empty() {
            if self.keys.is_empty() && self.bucket.is_none() && self.spec.is_active() {
                let combo: Combo = SmallVec::new();
                let mut empty = Aggregates::new(&self.spec);
                return formatter
                    .agg_row(out, &self.spec, &mut empty, &self.keys, &combo, None, tz);
            }
            return Ok(());
        }
        let mut entries: Vec<(&Combo, Option<Timestamp>, &mut Aggregates)> = Vec::new();
        for (bucket_ts, groups) in self.counts.iter_mut() {
            for (combo, agg) in groups.iter_mut() {
                entries.push((combo, *bucket_ts, agg));
            }
        }
        Self::emit_rows(
            out,
            formatter,
            tz,
            self.bucket,
            &self.keys,
            &self.spec,
            entries,
            SortOrder::CountDesc,
        )
    }

    /// Streaming flush: emit and remove every group whose bucket has
    /// passed the close threshold (`bucket.end + close_grace_nanos <
    /// max_ts_seen`). Rows go out in (bucket.start asc, count desc, combo
    /// asc) order. No-op in `Batched` mode.
    pub(crate) fn flush_closed<W: Write + ?Sized>(
        &mut self,
        out: &mut W,
        formatter: &Formatter,
        tz: &jiff::tz::TimeZone,
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

        let mut closing_counts = std::mem::replace(&mut self.counts, open_buckets);
        let mut entries: Vec<(&Combo, Option<Timestamp>, &mut Aggregates)> = Vec::new();
        for (bucket_ts, groups) in closing_counts.iter_mut() {
            for (combo, agg) in groups.iter_mut() {
                entries.push((combo, *bucket_ts, agg));
            }
        }
        Self::emit_rows(
            out,
            formatter,
            tz,
            self.bucket,
            &self.keys,
            &self.spec,
            entries,
            SortOrder::BucketAsc,
        )?;
        drop(closing_counts);
        out.flush()
    }

    /// End-of-stream flush: emit any still-open buckets in time order.
    /// Used in place of `report` when streaming.
    fn flush_remaining<W: Write>(
        &mut self,
        out: &mut W,
        formatter: &Formatter,
        tz: &jiff::tz::TimeZone,
    ) -> std::io::Result<()> {
        if self.counts.is_empty() {
            return Ok(());
        }
        let mut entries: Vec<(&Combo, Option<Timestamp>, &mut Aggregates)> = Vec::new();
        for (bucket_ts, groups) in self.counts.iter_mut() {
            for (combo, agg) in groups.iter_mut() {
                entries.push((combo, *bucket_ts, agg));
            }
        }
        Self::emit_rows(
            out,
            formatter,
            tz,
            self.bucket,
            &self.keys,
            &self.spec,
            entries,
            SortOrder::BucketAsc,
        )
    }

    /// End-of-run output: dispatches between batched `report` (count desc,
    /// single emission) and streaming `flush_remaining` (still-open buckets
    /// in time order) based on `self.mode`. This is the only public
    /// end-of-run entry point; the two underlying methods stay private to
    /// this module so callers can't accidentally pick the wrong one.
    pub(crate) fn emit_final<W: Write>(
        &mut self,
        out: &mut W,
        formatter: &Formatter,
        tz: &jiff::tz::TimeZone,
    ) -> std::io::Result<()> {
        match self.mode {
            CounterMode::Batched => self.report(out, formatter, tz),
            CounterMode::Streaming { .. } => self.flush_remaining(out, formatter, tz),
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
                    let spec = &self.spec;
                    for (combo, agg) in groups {
                        self_groups
                            .entry(combo)
                            .or_insert_with(|| Aggregates::new(spec))
                            .merge(agg);
                    }
                }
            }
        }
    }
}
