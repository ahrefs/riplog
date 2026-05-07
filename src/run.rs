//! Top-level pipeline: open input, optionally bisect a time slice, stream
//! lines through the filter, write matches to the chosen output, optionally
//! follow.

use anyhow::Context as _;
use humanize_bytes::humanize_bytes_binary;
use rapidhash::{RapidHashMap, RapidHashSet};
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;
use std::{
    fs::File,
    io::{BufRead, BufReader, BufWriter, IsTerminal, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::bisect::{self, Side};
use crate::cli::{Cli, ColorMode};
use crate::filter::Filter;
use crate::logfmt;
use crate::sort::SortBuffer;
use crate::timestamp::{self, Timestamp};
use crate::transform::{
    parse_line_transform, validate_rm_vs_features, write_colored_line, write_plain_reconstructed,
    EmitScratch, LineTransform,
};

/// Set by the SIGINT handler; checked in tight loops so we can exit cleanly
/// and still emit `--count` / `--list-keys` / `--count-by` summaries.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn install_signal_handler() {
    // Idempotent — `set_handler` errors if called twice. Ignore that path so
    // the binary stays usable when run as a library.
    let _ = ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst));
}

#[inline]
fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

type Combo = SmallVec<[SmartString; 3]>;

/// Collects every distinct key seen on matched lines. Active only when
/// `--list-keys` is set; in that mode line output is suppressed.
#[derive(Default)]
struct KeyGather {
    enabled: bool,
    keys: RapidHashSet<SmartString>,
}

impl KeyGather {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            keys: RapidHashSet::default(),
        }
    }

    fn record(&mut self, pairs: &[(&str, &str)]) {
        if !self.enabled {
            return;
        }
        for (k, _) in pairs {
            intern_into_set(&mut self.keys, k);
        }
    }

    fn report<W: Write>(&self, out: &mut W) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        emit_sorted(&self.keys, out)
    }

    fn merge(&mut self, other: Self) {
        self.keys.extend(other.keys);
    }
}

/// Find the first pair with key `key`, unescape its value into `scratch`,
/// and return a borrow of the unescaped string. Returns `None` if no pair
/// matches; in that case `scratch` is unspecified.
pub(crate) fn unescape_for_key<'s>(
    pairs: &[(&str, &str)],
    key: &str,
    scratch: &'s mut String,
) -> Option<&'s str> {
    for (pk, pv) in pairs {
        if *pk == key {
            scratch.clear();
            logfmt::unescape_value(pv.as_bytes(), scratch);
            return Some(scratch.as_str());
        }
    }
    None
}

/// Insert `s` into `set` only if not already present, allocating a
/// `SmartString` lazily.
fn intern_into_set(set: &mut RapidHashSet<SmartString>, s: &str) {
    if !set.contains(s) {
        let mut x = SmartString::new_const();
        x.push_str(s);
        set.insert(x);
    }
}

fn emit_sorted<W: Write>(set: &RapidHashSet<SmartString>, out: &mut W) -> std::io::Result<()> {
    let mut sorted: Vec<&SmartString> = set.iter().collect();
    sorted.sort_unstable();
    for v in sorted {
        writeln!(out, "{v}")?;
    }
    Ok(())
}

/// Collects every distinct value seen for each requested key, on matched
/// lines. Active when at least one `--list-values-for=<key>` is given;
/// suppresses line output.
#[derive(Default)]
struct ValueGather {
    keys: Vec<SmartString>,
    values: Vec<RapidHashSet<SmartString>>,
    scratch: String,
}

impl ValueGather {
    fn new(keys: Vec<SmartString>) -> Self {
        let n = keys.len();
        Self {
            keys,
            values: (0..n).map(|_| RapidHashSet::default()).collect(),
            scratch: String::new(),
        }
    }

    #[inline]
    fn is_active(&self) -> bool {
        !self.keys.is_empty()
    }

    fn record(&mut self, pairs: &[(&str, &str)]) {
        if !self.is_active() {
            return;
        }
        for (i, key) in self.keys.iter().enumerate() {
            if unescape_for_key(pairs, key, &mut self.scratch).is_some() {
                intern_into_set(&mut self.values[i], &self.scratch);
            }
        }
    }

    fn report<W: Write>(&self, out: &mut W) -> std::io::Result<()> {
        if !self.is_active() {
            return Ok(());
        }
        let multi = self.keys.len() > 1;
        for (key, set) in self.keys.iter().zip(self.values.iter()) {
            if multi {
                writeln!(out, "# {key}")?;
            }
            emit_sorted(set, out)?;
        }
        Ok(())
    }

    fn merge(&mut self, other: Self) {
        debug_assert_eq!(self.values.len(), other.values.len());
        for (a, b) in self.values.iter_mut().zip(other.values) {
            a.extend(b);
        }
    }
}

/// Per-line value extractor: for each matched line, emit the unquoted,
/// unescaped value of `key`. Active when `--raw-key=<key>` is given;
/// suppresses the normal full-line output. Lines lacking the key are
/// silently skipped.
struct RawExtractor {
    raw_key: Option<SmartString>,
    scratch: String,
}

impl RawExtractor {
    fn new(raw_key: Option<&str>) -> Self {
        Self {
            raw_key: raw_key.map(SmartString::from),
            scratch: String::new(),
        }
    }

    fn emit<W: Write + ?Sized>(
        &mut self,
        pairs: &[(&str, &str)],
        transform: Option<&LineTransform>,
        out: &mut W,
    ) -> std::io::Result<()> {
        let Some(key) = self.raw_key.as_deref() else {
            return Ok(());
        };
        match transform {
            None => {
                if let Some(v) = unescape_for_key(pairs, key, &mut self.scratch) {
                    out.write_all(v.as_bytes())?;
                    out.write_all(b"\n")?;
                }
                Ok(())
            }
            Some(tf) => {
                for (k, v) in pairs {
                    if tf.key_removed(k) {
                        continue;
                    }
                    if *k == key {
                        self.scratch.clear();
                        logfmt::unescape_value(v.as_bytes(), &mut self.scratch);
                        out.write_all(self.scratch.as_bytes())?;
                        out.write_all(b"\n")?;
                        return Ok(());
                    }
                }
                for (k, v) in &tf.add {
                    if k.as_str() == key {
                        out.write_all(v.as_bytes())?;
                        out.write_all(b"\n")?;
                        return Ok(());
                    }
                }
                Ok(())
            }
        }
    }
}

/// Aggregated stats for a single (group-keys [, bucket]) combination.
#[derive(Default)]
struct GroupStats {
    count: usize,
    min_ts: Option<Timestamp>,
    max_ts: Option<Timestamp>,
}

#[inline]
fn fold_min(slot: &mut Option<Timestamp>, t: Timestamp) {
    *slot = Some(slot.map_or(t, |cur| cur.min(t)));
}

#[inline]
fn fold_max(slot: &mut Option<Timestamp>, t: Timestamp) {
    *slot = Some(slot.map_or(t, |cur| cur.max(t)));
}

/// Comparator for streaming output: bucket time ascending, then count
/// descending, then combo ascending. Shared by `flush_closed` and
/// `flush_remaining` so both paths agree on ordering. (`report` uses a
/// different order — count desc — for batch-mode summaries.)
fn stream_order(
    ka: &GroupKey,
    sa: &GroupStats,
    kb: &GroupKey,
    sb: &GroupStats,
) -> std::cmp::Ordering {
    ka.1.cmp(&kb.1)
        .then_with(|| sb.count.cmp(&sa.count))
        .then_with(|| ka.0.cmp(&kb.0))
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

/// Time bucketing config: width in nanoseconds, plus the origin the bucket
/// grid is aligned to. `--bucket=DURATION` uses origin=0 (epoch-aligned, so
/// 5-minute buckets fall on `:00`, `:05`, ...). `--n-buckets=N` uses
/// origin=window-start *and* `n_buckets=Some(N)`, which clamps the bucket
/// index to `[0, N-1]` so a line at the inclusive `end` boundary lands in
/// the last bucket instead of overflowing into an N+1-th one.
#[derive(Clone, Copy)]
pub(crate) struct BucketSpec {
    nanos: i64,
    origin: i64,
    /// When set, clamps the bucket index to `[0, n_buckets-1]`.
    n_buckets: Option<usize>,
}

impl BucketSpec {
    #[inline]
    fn floor(&self, ts: Timestamp) -> i64 {
        let mut idx = (ts - self.origin).div_euclid(self.nanos);
        if let Some(n) = self.n_buckets {
            let max = (n as i64) - 1;
            if idx < 0 {
                idx = 0;
            } else if idx > max {
                idx = max;
            }
        }
        self.origin + idx * self.nanos
    }
}

/// Map key for one group. The user-keys combo and the bucket boundary are
/// kept as separate typed fields rather than smushed into a single
/// `SmallVec<SmartString>`, which avoids a per-line `format!` + reverse
/// `parse::<i64>()` round-trip on the hot path.
type GroupKey = (Combo, Option<Timestamp>);

/// Groups matched lines by the value tuple of `keys` (and optionally a time
/// bucket) and records per-group count + observed timestamp range. Missing
/// user keys produce an empty value slot (rendered as `key.<k>=""` in the
/// logfmt report).
#[derive(Default)]
struct Counter {
    keys: Vec<SmartString>,
    bucket: Option<BucketSpec>,
    counts: RapidHashMap<GroupKey, GroupStats>,
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
    streaming: bool,
}

impl Counter {
    fn new(keys: Vec<SmartString>, bucket: Option<BucketSpec>) -> Self {
        Self {
            keys,
            bucket,
            counts: RapidHashMap::default(),
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
    fn enable_streaming(&mut self, close_grace_nanos: i64) {
        if self.bucket.is_some() {
            self.streaming = true;
            self.close_grace_nanos = close_grace_nanos;
        }
    }

    #[inline]
    fn is_active(&self) -> bool {
        !self.keys.is_empty() || self.bucket.is_some()
    }

    fn record(&mut self, pairs: &[(&str, &str)], ts: Option<Timestamp>) {
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
            .entry((combo, bucket_ts))
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
    ) -> std::io::Result<()> {
        write!(out, "count={}", stats.count)?;
        for (k, v) in self.keys.iter().zip(combo.iter()) {
            write!(out, " key.{k}=")?;
            logfmt::write_logfmt_value(out, v.as_str())?;
        }
        if let (Some(bspec), Some(start)) = (self.bucket, bucket_ts) {
            let end = start + bspec.nanos;
            write!(
                out,
                " bucket.start={}",
                timestamp::format_rfc3339(start, tz)
            )?;
            write!(out, " bucket.end={}", timestamp::format_rfc3339(end, tz))?;
        }
        if let (Some(a), Some(b)) = (stats.min_ts, stats.max_ts) {
            write!(out, " time.start={}", timestamp::format_rfc3339(a, tz))?;
            write!(out, " time.end={}", timestamp::format_rfc3339(b, tz))?;
        }
        writeln!(out)
    }

    /// Batch-mode end-of-run report: count desc, ties broken by key.
    fn report<W: Write>(&self, out: &mut W, tz: &jiff::tz::TimeZone) -> std::io::Result<()> {
        if !self.is_active() || self.counts.is_empty() {
            return Ok(());
        }
        let mut entries: Vec<(&GroupKey, &GroupStats)> = self.counts.iter().collect();
        entries.sort_unstable_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
        for ((combo, bucket_ts), stats) in entries {
            self.write_row(out, combo, *bucket_ts, stats, tz)?;
        }
        Ok(())
    }

    /// Streaming flush: emit and remove every group whose bucket has
    /// passed the close threshold (`bucket.end + close_grace_nanos <
    /// max_ts_seen`). Rows go out in (bucket.start asc, count desc, combo
    /// asc) order. No-op when `streaming` is false.
    fn flush_closed<W: Write + ?Sized>(
        &mut self,
        out: &mut W,
        tz: &jiff::tz::TimeZone,
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

        let to_close: Vec<GroupKey> = self
            .counts
            .keys()
            .filter(|key| matches!(key.1, Some(bts) if bts < close_threshold))
            .cloned()
            .collect();
        if to_close.is_empty() {
            return Ok(());
        }
        // Move ownership of stats out of the map in one pass — no extra
        // lookups during the sort comparator.
        let mut closed: Vec<(GroupKey, GroupStats)> = to_close
            .into_iter()
            .map(|key| {
                let stats = self.counts.remove(&key).expect("just enumerated");
                (key, stats)
            })
            .collect();
        closed.sort_unstable_by(|a, b| stream_order(&a.0, &a.1, &b.0, &b.1));
        for (key, stats) in &closed {
            self.write_row(out, &key.0, key.1, stats, tz)?;
        }
        out.flush()
    }

    /// End-of-stream flush: emit any still-open buckets in time order.
    /// Used in place of `report` when streaming.
    fn flush_remaining<W: Write>(
        &self,
        out: &mut W,
        tz: &jiff::tz::TimeZone,
    ) -> std::io::Result<()> {
        if self.counts.is_empty() {
            return Ok(());
        }
        let mut entries: Vec<(&GroupKey, &GroupStats)> = self.counts.iter().collect();
        entries.sort_unstable_by(|a, b| stream_order(a.0, a.1, b.0, b.1));
        for ((combo, bucket_ts), stats) in entries {
            self.write_row(out, combo, *bucket_ts, stats, tz)?;
        }
        Ok(())
    }

    fn merge(&mut self, other: Self) {
        for (key, stats) in other.counts {
            self.counts.entry(key).or_default().merge(stats);
        }
    }
}

/// Random per-line sampling. When `sample_if` is set, only lines matching it
/// are subject to the dice roll; all other matched lines pass through.
#[derive(Clone)]
pub(crate) struct Sampler {
    rate: f64,
    sample_if: Option<Arc<Filter>>,
}

impl Sampler {
    fn keep(&self, pairs: &[(&str, &str)]) -> bool {
        let subject = self.sample_if.as_ref().is_none_or(|f| f.matches(pairs));
        !subject || fastrand::f64() < self.rate
    }
}

/// Strict timestamp filter applied per-line on top of the bisected byte range.
/// The bisect is an over-approximation, so the byte range can include lines
/// outside `[from, to]`; this filter drops them.
///
/// Lines with no parseable timestamp are dropped when either bound is set
/// (we can't prove they're in range).
#[derive(Default, Clone, Copy)]
pub(crate) struct TimeFilter {
    from: Option<Timestamp>,
    to: Option<Timestamp>,
}

impl TimeFilter {
    fn is_empty(&self) -> bool {
        self.from.is_none() && self.to.is_none()
    }

    /// Test the (already-parsed) timestamp against the bounds. `None` means
    /// the line had no parseable timestamp; with bounds set, that's a drop.
    fn check(&self, ts: Option<Timestamp>) -> bool {
        if self.is_empty() {
            return true;
        }
        let Some(ts) = ts else {
            return false;
        };
        if let Some(t1) = self.from {
            if ts < t1 {
                return false;
            }
        }
        if let Some(t2) = self.to {
            if ts > t2 {
                return false;
            }
        }
        true
    }
}

const FOLLOW_POLL: Duration = Duration::from_millis(200);

pub fn run(cli: &Cli) -> anyhow::Result<()> {
    install_signal_handler();

    if cli.time_range && (cli.follow || cli.follow_reopen) {
        anyhow::bail!("`--time-range` cannot be combined with `-f` or `-F`");
    }

    if resolve_parallelism(cli) > 1 && cli.limit.is_some() {
        anyhow::bail!("`-j`/`--parallel` cannot be combined with `-n`/`--limit`");
    }

    let filter = Filter::parse(&cli.keys)?;
    let line_transform = parse_line_transform(cli)?;
    if let Some(ref t) = line_transform {
        validate_rm_vs_features(cli, &t.remove)?;
    }
    let following = cli.follow || cli.follow_reopen;

    if following && cli.n_buckets.is_some() {
        anyhow::bail!(
            "`--n-buckets` cannot be combined with `-f`/`-F`: bucket width \
             requires a bounded time range. Use `--bucket=DURATION` instead."
        );
    }
    if following && !cli.group_by.is_empty() && cli.bucket.is_none() {
        anyhow::bail!(
            "`--group-by` under `-f`/`-F` requires `--bucket=DURATION`: \
             without a time dimension, no group is ever 'complete' so \
             nothing would print until you Ctrl-C."
        );
    }
    let suppress_lines = cli.list_keys
        || cli.count
        || !cli.list_values_for.is_empty()
        || !cli.group_by.is_empty()
        || cli.bucket.is_some()
        || cli.n_buckets.is_some()
        || cli.raw_key.is_some();
    let tz = timestamp::resolve_tz(cli.tz.as_deref())?;
    // The bare-number `--count` line is redundant when grouping/bucketing is
    // active (each row already carries its `count=`), so suppress it then.
    let bare_count =
        cli.count && cli.group_by.is_empty() && cli.bucket.is_none() && cli.n_buckets.is_none();

    let sampler = build_sampler(cli)?;

    let colorize = match cli.color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => cli.output.is_none() && std::io::stdout().is_terminal(),
    };

    // Memcpy fast path: unchanged for entire run (`filter` / CLI transforms / color).
    let passthrough_emit = !colorize && line_transform.is_none() && filter.is_empty();

    // `Send` so the parallel path can hand `&mut output` to its workers
    // through a shared `Mutex`. Using the unlocked `Stdout` (rather than
    // `stdout().lock()`) makes this cross-thread-safe; the per-call lock
    // inside `Stdout::write` is amortised by `BufWriter` batching.
    let mut output: Box<dyn Write + Send> = match &cli.output {
        Some(path) => Box::new(BufWriter::new(File::create(path)?)),
        None => Box::new(BufWriter::new(std::io::stdout())),
    };

    let need_seek = cli.from.is_some() || cli.to.is_some() || following || cli.time_range;
    if cli.files.is_empty() {
        if need_seek {
            anyhow::bail!("`-f`, `-F`, `--from`, `--to`, `--time-range` require a file argument");
        }
        if cli.n_buckets.is_some() {
            anyhow::bail!(
                "`--n-buckets` requires a file argument: the bucket width is derived \
                 from the file's time range"
            );
        }
        // Epoch-aligned grid for `--bucket=DURATION` on stdin.
        let bucket = cli
            .bucket
            .as_deref()
            .map(timestamp::parse_duration_nanos)
            .transpose()?
            .map(|nanos| BucketSpec {
                nanos,
                origin: 0,
                n_buckets: None,
            });
        // stdin can't follow (rejected earlier), but `--bucket` still
        // enables streaming output: the per-line `flush_closed` hook in
        // `process_line` emits closed buckets in time order as we go,
        // without any seek (pipes can't seek). At EOF, `emit_summaries`
        // calls `flush_remaining` for the still-open buckets.
        let mut sinks = make_sinks(
            cli,
            sampler.clone(),
            suppress_lines,
            colorize,
            bucket,
            tz.clone(),
            line_transform.clone(),
            passthrough_emit,
        );
        if bucket.is_some() {
            let grace = (cli.window_secs as i64).saturating_mul(1_000_000_000);
            sinks.enable_streaming(grace);
        }
        stream_unbounded(
            &mut std::io::stdin().lock(),
            &filter,
            &mut output,
            &mut sinks,
        )?;
        output.flush()?;
        flush_sort_buf(&mut sinks, &mut output)?;
        emit_summaries(&sinks, bare_count, &tz, &mut output)?;
        return Ok(());
    }

    if cli.time_range {
        // Min of per-file firsts, max of per-file lasts — the union span.
        let mut overall_first: Option<Timestamp> = None;
        let mut overall_last: Option<Timestamp> = None;
        for path in &cli.files {
            let mut file = File::open(path)?;
            let t0 = Instant::now();
            let (first, last) = bisect::time_range(&mut file)?;
            log::info!(
                "time-range {}: {} .. {} in {:.3}s",
                path.display(),
                first
                    .map(|t| timestamp::format_rfc3339(t, &tz))
                    .as_deref()
                    .unwrap_or("-"),
                last.map(|t| timestamp::format_rfc3339(t, &tz))
                    .as_deref()
                    .unwrap_or("-"),
                t0.elapsed().as_secs_f64(),
            );
            if let Some(t) = first {
                overall_first = Some(overall_first.map_or(t, |cur| cur.min(t)));
            }
            if let Some(t) = last {
                overall_last = Some(overall_last.map_or(t, |cur| cur.max(t)));
            }
        }
        match (overall_first, overall_last) {
            (Some(a), Some(b)) => writeln!(
                output,
                "{} .. {}  ({})",
                timestamp::format_rfc3339(a, &tz),
                timestamp::format_rfc3339(b, &tz),
                timestamp::format_duration(b - a),
            )?,
            _ => writeln!(output, "no parseable timestamps in file")?,
        }
        output.flush()?;
        return Ok(());
    }

    // Resolve `--from`/`--to` once against the union of all files' time
    // windows. Symbolic anchors (`start`, `end`, `start+1h`, etc.) refer to
    // the *global* span, not each file's local one — so with two log files
    // around a rotation, `--from start+1h --to start+2h` is one contiguous
    // absolute window applied across both files, not two disjoint slices.
    let need_global = cli.from.is_some() || cli.to.is_some() || cli.n_buckets.is_some();
    let (global_first, global_last) = if need_global {
        peek_global_window(&cli.files)?
    } else {
        (None, None)
    };
    let mut tf = TimeFilter::default();
    if let Some(s) = cli.from.as_deref() {
        tf.from = Some(timestamp::resolve_bound(
            s,
            global_first,
            global_last,
            global_first,
        )?);
    }
    if let Some(s) = cli.to.as_deref() {
        tf.to = Some(timestamp::resolve_bound(
            s,
            global_first,
            global_last,
            global_last,
        )?);
    }

    // Resolve the bucket spec now that the time window is known. Two forms:
    // - `--bucket=DURATION`: epoch-aligned grid (origin = 0).
    // - `--n-buckets=N`: divide the *active* window into N equal-width slices
    //   aligned to the window start, so the output has exactly N rows per
    //   group (no edge-alignment off-by-one).
    let bucket = resolve_bucket_spec(cli, &tf, global_first, global_last)?;

    let mut sinks = make_sinks(
        cli,
        sampler.clone(),
        suppress_lines,
        colorize,
        bucket,
        tz.clone(),
        line_transform.clone(),
        passthrough_emit,
    );
    // Master streams under follow; workers always batch (their output would
    // interleave on the shared writer otherwise) and merge into the master.
    if following {
        let grace = (cli.window_secs as i64).saturating_mul(1_000_000_000);
        sinks.enable_streaming(grace);
    }

    // Phase 1: bisect every file up front against the resolved absolute
    // window. Output is suppressed during planning — only summaries and
    // matched lines are written, in file order, in phase 2.
    let last_idx = cli.files.len() - 1;
    let plans: Vec<FilePlan<'_>> = cli
        .files
        .iter()
        .enumerate()
        .map(|(i, path)| plan_file(path, cli, tf, following && i == last_idx))
        .collect::<anyhow::Result<_>>()?;

    // Phase 2: stream each planned range in order. Only the last file may
    // attach the follow loop (set during planning).
    let n_workers = resolve_parallelism(cli);
    for plan in plans {
        if interrupted() || sinks.done() {
            break;
        }
        if n_workers > 1 && !plan.follow_this_file {
            crate::parallel::run(crate::parallel::Job {
                path: plan.path,
                start_byte: plan.start_byte,
                max_bytes: plan.max_bytes,
                tf: plan.tf,
                n_workers,
                cli,
                filter: &filter,
                sampler: sampler.clone(),
                suppress_lines,
                colorize,
                bucket,
                tz: tz.clone(),
                output: &mut *output,
                master: &mut sinks,
                line_transform: line_transform.clone(),
                passthrough_emit,
            })?;
            output.flush()?;
        } else {
            stream_plan(plan, cli, &filter, &mut output, &mut sinks)?;
        }
    }

    output.flush()?;
    flush_sort_buf(&mut sinks, &mut output)?;
    emit_summaries(&sinks, bare_count, &tz, &mut output)?;

    Ok(())
}

fn flush_sort_buf<W: Write>(sinks: &mut Sinks, out: &mut W) -> std::io::Result<()> {
    if let Some(sort_buf) = sinks.sort_buf.take() {
        sort_buf.emit(out)?;
        out.flush()?;
    }
    Ok(())
}

/// Resolved per-file plan produced by phase 1: a byte slice (start, len),
/// the strict time-filter to apply on top of it, and whether this file
/// should be followed after EOF. The file is reopened in phase 2 so phase 1
/// doesn't hold N file descriptors simultaneously.
struct FilePlan<'a> {
    path: &'a Path,
    start_byte: u64,
    /// `end_byte - start_byte`. Phase 2 reads exactly this many bytes.
    max_bytes: u64,
    tf: TimeFilter,
    follow_this_file: bool,
}

/// Compute the union span across all files (min of per-file first
/// timestamps, max of per-file lasts) used as the anchor for symbolic
/// `--from`/`--to` bounds. One head + one tail seek per file.
fn peek_global_window(
    files: &[std::path::PathBuf],
) -> anyhow::Result<(Option<Timestamp>, Option<Timestamp>)> {
    let mut first: Option<Timestamp> = None;
    let mut last: Option<Timestamp> = None;
    for path in files {
        let mut file = File::open(path)?;
        if let Some(t) = bisect::peek_first_timestamp(&mut file)? {
            first = Some(first.map_or(t, |cur| cur.min(t)));
        }
        if let Some(t) = bisect::peek_last_timestamp(&mut file)? {
            last = Some(last.map_or(t, |cur| cur.max(t)));
        }
    }
    Ok((first, last))
}

/// Phase 1: open `path`, bisect to the absolute byte slice corresponding to
/// `tf`, and return the resolved range. The file is dropped on return so
/// phase 1 doesn't pin a file descriptor; phase 2 reopens it. Independent
/// across files (so it parallelizes cleanly).
fn plan_file<'a>(
    path: &'a Path,
    cli: &Cli,
    tf: TimeFilter,
    follow_this_file: bool,
) -> anyhow::Result<FilePlan<'a>> {
    let mut file = File::open(path)?;
    let file_len = file.seek(SeekFrom::End(0))?;
    let window = (cli.window_secs as i64).saturating_mul(1_000_000_000);

    let t_bisect = Instant::now();
    let start_byte: u64 = match tf.from {
        Some(t1) => bisect::bisect(&mut file, t1, window, Side::Lower)?,
        None if follow_this_file => file_len, // tail-from-EOF when no --from
        None => 0,
    };
    let end_byte: u64 = match tf.to {
        Some(t2) if !follow_this_file => bisect::bisect(&mut file, t2, window, Side::Upper)?,
        _ => file_len,
    };

    if cli.from.is_some() || cli.to.is_some() {
        log::info!(
            "bisect {}: [{}, {}] window={}s -> bytes [{start_byte}, {end_byte}) ({}) in {:.3}s",
            path.display(),
            cli.from.as_deref().unwrap_or("-"),
            cli.to.as_deref().unwrap_or("-"),
            cli.window_secs,
            humanize_bytes_binary!(end_byte.saturating_sub(start_byte)),
            t_bisect.elapsed().as_secs_f64()
        );
    }

    Ok(FilePlan {
        path,
        start_byte,
        max_bytes: end_byte.saturating_sub(start_byte),
        tf,
        follow_this_file,
    })
}

/// Phase 2: open `plan.path`, seek to the planned start, stream the bounded
/// byte range through filters and sinks, then optionally attach the follow
/// loop.
fn stream_plan<W: Write>(
    plan: FilePlan<'_>,
    cli: &Cli,
    filter: &Filter,
    output: &mut W,
    sinks: &mut Sinks,
) -> anyhow::Result<()> {
    let FilePlan {
        path,
        start_byte,
        max_bytes,
        tf,
        follow_this_file,
    } = plan;

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start_byte))?;
    let file_for_reopen = if follow_this_file {
        Some(file.try_clone().with_context(|| {
            format!(
                "dup file handle for follow-mode rotation tracking: {}",
                path.display()
            )
        })?)
    } else {
        None
    };
    let mut reader = BufReader::new(file);

    stream_bounded(&mut reader, max_bytes, filter, &tf, output, sinks)?;
    output.flush()?;

    if let Some(handle) = file_for_reopen {
        if !interrupted() && !sinks.done() {
            follow_loop(
                path,
                handle,
                reader,
                cli.follow_reopen,
                filter,
                output,
                sinks,
            )?;
        }
    }

    Ok(())
}

fn emit_summaries<W: Write>(
    sinks: &Sinks,
    count_only: bool,
    tz: &jiff::tz::TimeZone,
    output: &mut W,
) -> anyhow::Result<()> {
    sinks.stats.report();
    if sinks.counter.streaming {
        sinks.counter.flush_remaining(output, tz)?;
    } else {
        sinks.counter.report(output, tz)?;
    }
    sinks.keys.report(output)?;
    sinks.values.report(output)?;
    if count_only {
        writeln!(output, "{}", sinks.stats.matched_lines)?;
    }
    output.flush()?;
    Ok(())
}

struct Stats {
    bytes: usize,
    matched_lines: usize,
    total_lines: usize,
    invalid_utf: usize,
    pairs: usize,
    overflow: usize,
    started: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            bytes: 0,
            matched_lines: 0,
            total_lines: 0,
            invalid_utf: 0,
            pairs: 0,
            overflow: 0,
            started: Instant::now(),
        }
    }
}

impl Stats {
    fn merge(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.matched_lines += other.matched_lines;
        self.total_lines += other.total_lines;
        self.invalid_utf += other.invalid_utf;
        self.pairs += other.pairs;
        self.overflow += other.overflow;
        // `started` stays as the master's earliest start time.
    }

    fn report(&self) {
        let elapsed = self.started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            (self.bytes as f64 / elapsed) as u64
        } else {
            0
        };
        log::info!(
            "{} read in {:.3}s, {}/{} lines matched ({} invalid utf8), {} pairs ({} overflow), {}/s",
            humanize_bytes_binary!(self.bytes),
            elapsed,
            self.matched_lines,
            self.total_lines,
            self.invalid_utf,
            self.pairs,
            self.overflow,
            humanize_bytes_binary!(rate),
        );
    }
}

/// Bundles per-line bookkeeping (stats + summarisers) so the streaming
/// helpers don't drown in arguments.
pub(crate) struct Sinks {
    stats: Stats,
    counter: Counter,
    keys: KeyGather,
    values: ValueGather,
    raw: RawExtractor,
    sampler: Option<Sampler>,
    sort_buf: Option<SortBuffer>,
    suppress_lines: bool,
    colorize: bool,
    limit: Option<usize>,
    /// Display timezone, used by the streaming bucket flush to format
    /// `bucket.start` / `bucket.end` / `time.start` / `time.end` on the fly.
    tz: jiff::tz::TimeZone,
    /// When set, `--rm` / `--add` mutate emitted lines (not used for `--if`).
    transform: Option<LineTransform>,
    emit_scratch: EmitScratch,
    /// When true, emit matched lines by copying input bytes (no parse-roundtrip).
    passthrough_emit: bool,
}

impl Sinks {
    #[inline]
    fn done(&self) -> bool {
        matches!(self.limit, Some(n) if self.stats.matched_lines >= n)
    }

    /// Activate streaming bucket emission on the underlying counter. Should
    /// only be called on the *master* `Sinks` in follow mode — workers must
    /// stay batched so their output can't interleave on the shared writer.
    pub(crate) fn enable_streaming(&mut self, close_grace_nanos: i64) {
        self.counter.enable_streaming(close_grace_nanos);
    }

    /// Fold per-worker collected state into `self`. `started` and the
    /// configuration fields (`suppress_lines`, `colorize`, `limit`, ...)
    /// are kept from `self`.
    pub(crate) fn merge(&mut self, other: Self) {
        self.stats.merge(other.stats);
        self.counter.merge(other.counter);
        self.keys.merge(other.keys);
        self.values.merge(other.values);
        if let (Some(a), Some(b)) = (self.sort_buf.as_mut(), other.sort_buf) {
            a.merge(b);
        }
    }
}

/// Resolve `--parallel` to a worker count. `None` → 1 (sequential).
/// `Some(0)` → all available cores (set by `default_missing_value` when the
/// flag is given without a value). `Some(n)` → `n` workers.
pub(crate) fn resolve_parallelism(cli: &Cli) -> usize {
    match cli.parallel {
        None => 1,
        Some(0) => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        Some(n) => n,
    }
}

/// Parse the sampler config out of `--sample-rate` / `--sample-if`. Done
/// once up front so workers can clone it cheaply (the inner `Filter` is
/// shared via `Arc`).
fn build_sampler(cli: &Cli) -> anyhow::Result<Option<Sampler>> {
    match (cli.sample_rate, cli.sample_if.as_deref()) {
        (None, Some(_)) => anyhow::bail!("--sample-if requires --sample-rate"),
        (None, None) => Ok(None),
        (Some(rate), _) if !(0.0..=1.0).contains(&rate) => {
            anyhow::bail!("--sample-rate must be in [0, 1], got {rate}")
        }
        (Some(rate), sample_if) => Ok(Some(Sampler {
            rate,
            sample_if: sample_if.map(Filter::parse_one).transpose()?.map(Arc::new),
        })),
    }
}

/// Compute the bucket spec from `--bucket` / `--n-buckets`. Caller has
/// already resolved `tf`; `global_first`/`global_last` are the file-side
/// bounds returned by `peek_global_window` (or `None` if it wasn't run).
/// Returns `None` when neither flag is set. Errors when `--n-buckets`
/// cannot be sized (no resolvable window) or the resulting width is zero.
fn resolve_bucket_spec(
    cli: &Cli,
    tf: &TimeFilter,
    global_first: Option<Timestamp>,
    global_last: Option<Timestamp>,
) -> anyhow::Result<Option<BucketSpec>> {
    if let Some(s) = cli.bucket.as_deref() {
        let nanos = timestamp::parse_duration_nanos(s)?;
        return Ok(Some(BucketSpec {
            nanos,
            origin: 0,
            n_buckets: None,
        }));
    }
    if let Some(n) = cli.n_buckets {
        if n == 0 {
            anyhow::bail!("--n-buckets must be > 0");
        }
        let start = tf.from.or(global_first).ok_or_else(|| {
            anyhow::anyhow!(
                "--n-buckets needs a window start: pass --from, or use a file with parseable timestamps"
            )
        })?;
        let end = tf.to.or(global_last).ok_or_else(|| {
            anyhow::anyhow!(
                "--n-buckets needs a window end: pass --to, or use a file with parseable timestamps"
            )
        })?;
        let span = end - start;
        if span <= 0 {
            anyhow::bail!("--n-buckets: time range is empty (end <= start)");
        }
        let nanos = span / n as i64;
        if nanos == 0 {
            anyhow::bail!("--n-buckets={n}: span {span}ns is too small to split into {n} buckets");
        }
        return Ok(Some(BucketSpec {
            nanos,
            origin: start,
            n_buckets: Some(n),
        }));
    }
    Ok(None)
}

/// Construct a fresh `Sinks` for the master or a worker. Configuration
/// fields are derived from `cli`; the `sampler` template is cloned in (its
/// inner `Arc<Filter>` is shared, so cloning is cheap). All collected
/// state (counts, gathers, sort buffer) starts empty.
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_sinks(
    cli: &Cli,
    sampler: Option<Sampler>,
    suppress_lines: bool,
    colorize: bool,
    bucket: Option<BucketSpec>,
    tz: jiff::tz::TimeZone,
    transform: Option<LineTransform>,
    passthrough_emit: bool,
) -> Sinks {
    let counter = Counter::new(cli.group_by.iter().map(SmartString::from).collect(), bucket);
    Sinks {
        stats: Stats::default(),
        counter,
        keys: KeyGather::new(cli.list_keys),
        values: ValueGather::new(cli.list_values_for.iter().map(SmartString::from).collect()),
        raw: RawExtractor::new(cli.raw_key.as_deref()),
        sampler,
        sort_buf: cli.sort_by.as_deref().map(SortBuffer::new),
        suppress_lines,
        colorize,
        limit: cli.limit,
        tz,
        transform,
        emit_scratch: EmitScratch::default(),
        passthrough_emit,
    }
}

/// Read a fixed byte budget from `reader`, write matching lines to `output`.
pub(crate) fn stream_bounded<R: BufRead, W: Write + ?Sized>(
    reader: &mut R,
    max_bytes: u64,
    filter: &Filter,
    tf: &TimeFilter,
    output: &mut W,
    sinks: &mut Sinks,
) -> anyhow::Result<()> {
    let mut line_buf = Vec::new();
    let mut total_read: u64 = 0;

    while total_read < max_bytes && !interrupted() {
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            break;
        }
        total_read += n as u64;
        process_line(&mut line_buf, filter, tf, output, sinks, n)?;
        if sinks.done() {
            break;
        }
    }
    Ok(())
}

/// Read until EOF (e.g. stdin), write matching lines to `output`.
/// Flushes after every line so interactive pipelines (`tail -f | riplog`)
/// don't stall in the output BufWriter.
fn stream_unbounded<R: Read, W: Write>(
    reader: &mut R,
    filter: &Filter,
    output: &mut W,
    sinks: &mut Sinks,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut line_buf = Vec::new();

    while !interrupted() {
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            break;
        }
        process_line(
            &mut line_buf,
            filter,
            &TimeFilter::default(),
            output,
            sinks,
            n,
        )?;
        output.flush()?;
        if sinks.done() {
            break;
        }
    }
    Ok(())
}

fn process_line<W: Write + ?Sized>(
    line_buf: &mut Vec<u8>,
    filter: &Filter,
    tf: &TimeFilter,
    output: &mut W,
    sinks: &mut Sinks,
    n_bytes: usize,
) -> anyhow::Result<()> {
    // Preserve the original bytes for output; trim a trailing newline for parsing.
    let raw_len = line_buf.len();
    let mut parse_end = raw_len;
    while parse_end > 0 && matches!(line_buf[parse_end - 1], b'\n' | b'\r') {
        parse_end -= 1;
    }

    sinks.stats.bytes += n_bytes;
    sinks.stats.total_lines += 1;

    let parse_slice = &line_buf[..parse_end];
    let line_str = match std::str::from_utf8(parse_slice) {
        Ok(s) => s,
        Err(_) => {
            sinks.stats.invalid_utf += 1;
            line_buf.clear();
            return Ok(());
        }
    };

    let mut pairs = logfmt::PairsBuffer::<256>::new();
    let (parsed, overflow) = pairs.parse(line_str);

    // Extract the timestamp at most once per line, only when something
    // downstream actually needs it (time-window filter or grouping counter).
    let ts = if !tf.is_empty() || sinks.counter.is_active() {
        timestamp::extract_timestamp(parsed)
    } else {
        None
    };

    let matched = tf.check(ts)
        && (filter.is_empty() || filter.matches(parsed))
        && sinks.sampler.as_ref().is_none_or(|s| s.keep(parsed));

    sinks.stats.pairs += parsed.len();
    sinks.stats.overflow += overflow as usize;
    if matched {
        sinks.stats.matched_lines += 1;
        sinks.counter.record(parsed, ts);
        // Streaming mode (`-f`/`-F` + `--bucket`): emit any buckets that
        // have passed the close threshold. Cheap when nothing is closeable;
        // a no-op when streaming is off.
        sinks.counter.flush_closed(output, &sinks.tz)?;
        sinks.keys.record(parsed);
        sinks.values.record(parsed);
        let line_tf = sinks.transform.as_ref();
        if let Some(sort_buf) = sinks.sort_buf.as_mut() {
            // Aggregation modes without --raw-key produce no per-line bytes;
            // skip the capture in that case (counters above already recorded).
            if !sinks.suppress_lines || sinks.raw.raw_key.is_some() {
                let raw = &mut sinks.raw;
                let scratch = &mut sinks.emit_scratch;
                let suppress = sinks.suppress_lines;
                let color = sinks.colorize;
                sort_buf.capture(parsed, |w| {
                    emit_match(
                        parsed,
                        line_buf,
                        raw_len,
                        parse_end,
                        raw,
                        suppress,
                        color,
                        line_tf,
                        scratch,
                        sinks.passthrough_emit,
                        w,
                    )
                })?;
            }
        } else {
            emit_match(
                parsed,
                line_buf,
                raw_len,
                parse_end,
                &mut sinks.raw,
                sinks.suppress_lines,
                sinks.colorize,
                line_tf,
                &mut sinks.emit_scratch,
                sinks.passthrough_emit,
                output,
            )?;
        }
    }
    line_buf.clear();
    Ok(())
}

#[inline]
fn append_reconstructed_plain_tail(
    buf: &mut Vec<u8>,
    line_buf: &[u8],
    raw_len: usize,
    parse_end: usize,
) {
    if raw_len == parse_end {
        buf.push(b'\n');
    } else {
        buf.extend_from_slice(&line_buf[parse_end..raw_len]);
    }
}

/// Emit one matched line to `out`: the `--raw-key` extraction (if any),
/// followed by the full line (colored or raw) unless line output is
/// suppressed by an aggregation/raw-key mode.
#[allow(clippy::too_many_arguments)]
fn emit_match<W: Write + ?Sized>(
    parsed: &[(&str, &str)],
    line_buf: &[u8],
    raw_len: usize,
    parse_end: usize,
    raw: &mut RawExtractor,
    suppress_lines: bool,
    colorize: bool,
    transform: Option<&LineTransform>,
    scratch: &mut EmitScratch,
    passthrough_emit: bool,
    out: &mut W,
) -> std::io::Result<()> {
    raw.emit(parsed, transform, out)?;

    if suppress_lines {
        return Ok(());
    }

    if passthrough_emit {
        out.write_all(line_buf)?;
        if raw_len == parse_end {
            out.write_all(b"\n")?;
        }
    } else {
        scratch.write_slow(out, |buf| {
            if colorize {
                write_colored_line(buf, parsed, transform)
            } else {
                write_plain_reconstructed(buf, parsed, transform)?;
                append_reconstructed_plain_tail(buf, line_buf, raw_len, parse_end);
                Ok(())
            }
        })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn follow_loop<W: Write>(
    path: &Path,
    mut handle: File,
    mut reader: BufReader<File>,
    reopen: bool,
    filter: &Filter,
    output: &mut W,
    sinks: &mut Sinks,
) -> anyhow::Result<()> {
    let mut line_buf = Vec::new();
    let mut pos = reader.stream_position()?;

    while !interrupted() {
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            output.flush()?;
            if reopen {
                if let Some((new_handle, new_reader)) = check_rotation(path, &handle, pos)? {
                    log::info!(
                        "follow: file rotated/truncated; reopening {}",
                        path.display()
                    );
                    handle = new_handle;
                    reader = new_reader;
                    pos = 0;
                    line_buf.clear();
                    continue;
                }
            }
            std::thread::sleep(FOLLOW_POLL);
            continue;
        }
        if !line_buf.ends_with(b"\n") {
            // Partial line — wait for the rest.
            std::thread::sleep(FOLLOW_POLL);
            continue;
        }
        pos += n as u64;
        process_line(
            &mut line_buf,
            filter,
            &TimeFilter::default(),
            output,
            sinks,
            n,
        )?;
        if sinks.done() {
            break;
        }
    }
    Ok(())
}

/// On EOF, decide whether the path now resolves to a different file (rotation)
/// or has shrunk below our position (truncation). Returns a fresh
/// `(File, BufReader)` if so.
fn check_rotation(
    path: &Path,
    #[cfg_attr(not(unix), allow(unused_variables))] current: &File,
    pos: u64,
) -> anyhow::Result<Option<(File, BufReader<File>)>> {
    let path_meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None), // file may be momentarily missing during rotation
    };

    let mut should_reopen = path_meta.len() < pos;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !should_reopen {
            if let Ok(cur_meta) = current.metadata() {
                if path_meta.ino() != cur_meta.ino() || path_meta.dev() != cur_meta.dev() {
                    should_reopen = true;
                }
            }
        }
    }
    if !should_reopen {
        return Ok(None);
    }
    let f = File::open(path)?;
    let dup = f.try_clone()?;
    Ok(Some((f, BufReader::new(dup))))
}
