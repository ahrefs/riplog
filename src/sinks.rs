//! Per-run sinks: stats, counters, gathers, sampler, sort buffer, and the
//! per-emit scratch buffers. Bundled into `Sinks` so the streaming helpers
//! don't drown in arguments. Constructed once via `make_sinks` for the
//! master, and again per worker in the parallel path.

use smartstring::alias::String as SmartString;
use std::io::Write;

use crate::bucket::BucketSpec;
use crate::cli::Cli;
use crate::counter::Counter;
use crate::raw_extractor::RawExtractor;
use crate::sampler::Sampler;
use crate::sort::SortBuffer;
use crate::stats::{KeyGather, Stats, ValueGather};
use crate::transform::EmitScratch;

/// How matched lines are emitted. Exactly one branch is active for a given
/// run, decided once in `run::run` from `--color`, `--json`, and whether
/// the memcpy fast path is eligible (no filter, no `--rm`/`--add`).
#[derive(Clone, Copy, Debug)]
pub(crate) enum LineMode {
    /// Copy the input bytes verbatim — the fast path.
    Passthrough,
    /// JSONL: one JSON object per line.
    Json,
    /// ANSI-colored logfmt reconstruction.
    Colored,
    /// Plain logfmt reconstruction (no color).
    Plain,
}

/// Bundles per-line bookkeeping (stats + summarisers) so the streaming
/// helpers don't drown in arguments.
pub(crate) struct Sinks {
    pub(crate) stats: Stats,
    pub(crate) counter: Counter,
    pub(crate) keys: KeyGather,
    pub(crate) values: ValueGather,
    pub(crate) raw: RawExtractor,
    pub(crate) sampler: Option<Sampler>,
    pub(crate) sort_buf: Option<SortBuffer>,
    pub(crate) suppress_lines: bool,
    /// Picks the line emitter (passthrough / json / colored / plain).
    pub(crate) line_mode: LineMode,
    pub(crate) limit: Option<usize>,
    /// Display timezone, used by the streaming bucket flush to format
    /// `bucket.start` / `bucket.end` / `time.start` / `time.end` on the fly.
    pub(crate) tz: jiff::tz::TimeZone,
    pub(crate) emit_scratch: EmitScratch,
}

impl Sinks {
    #[inline]
    pub(crate) fn done(&self) -> bool {
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

/// Construct a fresh `Sinks` for the master or a worker. Configuration
/// fields are derived from `cli`; the `sampler` template is cloned in (its
/// inner `Arc<Filter>` is shared, so cloning is cheap). All collected
/// state (counts, gathers, sort buffer) starts empty.
pub(crate) fn make_sinks(
    cli: &Cli,
    sampler: Option<Sampler>,
    suppress_lines: bool,
    line_mode: LineMode,
    bucket: Option<BucketSpec>,
    tz: jiff::tz::TimeZone,
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
        line_mode,
        limit: cli.limit,
        tz,
        emit_scratch: EmitScratch::default(),
    }
}

pub(crate) fn flush_sort_buf<W: Write>(sinks: &mut Sinks, out: &mut W) -> std::io::Result<()> {
    if let Some(sort_buf) = sinks.sort_buf.take() {
        sort_buf.emit(out)?;
        out.flush()?;
    }
    Ok(())
}
