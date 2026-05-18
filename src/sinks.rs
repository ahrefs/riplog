//! Per-run state split into three structs with distinct lifecycles:
//!
//! - [`RunConfig`]: built once in `run()`, borrowed everywhere. Read-only
//!   configuration (formatter, suppression, limit, tz, sampler, add/remove
//!   key views).
//! - [`Recorders`]: accumulators that record matches and are merged across
//!   workers at the end of a parallel run.
//! - [`LineEmitter`]: per-thread scratch + sort buffer for formatting matched
//!   lines. Only the optional `sort_buf` is merged across workers.
//!
//! `Pipeline` (in `pipeline.rs`) bundles `&RunConfig`, `&mut Recorders`, and
//! `&mut LineEmitter` (plus filters) for the hot per-line path.

use smartstring::alias::String as SmartString;
use std::io::Write;

use crate::bucket::ResolvedBucket;
use crate::cli::Cli;
use crate::counter::Counter;
use crate::output::{Formatter, OutputFormat};
use crate::raw_extractor::RawExtractor;
use crate::sampler::Sampler;
use crate::sort::SortBuffer;
use crate::stats::{KeyGather, Stats, ValueGather};
use crate::transform::EmitScratch;

/// Read-only run configuration: branching constants and shared views that
/// don't change after `run()` builds them. Borrowed by `Pipeline` and the
/// emit hot path; never mutated during the run.
///
/// The `'a` lifetime ties `formatter`, `add_pairs`, and `remove_keys` to
/// the run-scoped values they're borrowed from.
pub(crate) struct RunConfig<'a> {
    /// Format engine for every output shape (line, agg row, summary, count).
    /// Decided once in `run::run` from `--color` / `--json`. The orthogonal
    /// `passthrough` flag overrides this on the per-line path.
    ///
    /// Stored as `&Formatter` (an enum dispatched via `match`) rather than
    /// `&dyn OutputFormat` so the per-line emit path resolves to direct,
    /// inlinable calls instead of vtable indirections — critical for the
    /// inner `write_all` loop in `LogfmtFormat::line`.
    pub(crate) formatter: &'a Formatter,
    /// Memcpy fast path: when true, the emit path writes `line_buf` verbatim
    /// (plus a trailing newline if needed) and ignores `formatter`.
    pub(crate) passthrough: bool,
    pub(crate) suppress_lines: bool,
    pub(crate) limit: Option<usize>,
    pub(crate) tz: jiff::tz::TimeZone,
    pub(crate) sampler: Option<Sampler>,
    pub(crate) add_pairs: &'a [(&'a str, &'a str)],
    pub(crate) remove_keys: &'a [&'a str],
}

/// Accumulators that record matches across the run. Merged across workers
/// after a parallel job completes (every field; no silent skips).
pub(crate) struct Recorders {
    pub(crate) stats: Stats,
    pub(crate) counter: Counter,
    pub(crate) keys: KeyGather,
    pub(crate) values: ValueGather,
}

impl Recorders {
    /// Build a fresh `Recorders` for the master or a worker. All collected
    /// state starts empty; `bucket` is the resolved bucket spec (master-side
    /// constructed once, then passed by value into each worker).
    ///
    /// `streaming_close_grace_nanos` requests streaming output from the
    /// counter when both a bucket is present and a close-grace is given.
    /// Workers pass `None`; only the master can stream (workers always
    /// batch and merge into the master). See `Counter::new` for the
    /// silent-downgrade rule when `bucket.is_none()`.
    pub(crate) fn new(
        cli: &Cli,
        bucket: Option<ResolvedBucket>,
        streaming_close_grace_nanos: Option<i64>,
    ) -> Self {
        let counter = Counter::new(
            cli.group_by.iter().map(SmartString::from).collect(),
            bucket,
            streaming_close_grace_nanos,
        );
        Self {
            stats: Stats::default(),
            counter,
            keys: KeyGather::new(cli.list_keys),
            values: ValueGather::new(cli.list_values_for.iter().map(SmartString::from).collect()),
        }
    }

    /// Total merge: every field gets folded in. The master's `started`
    /// instant is preserved (see `Stats::merge`); everything else
    /// accumulates.
    pub(crate) fn merge(&mut self, other: Self) {
        let Recorders {
            stats,
            counter,
            keys,
            values,
        } = other;
        self.stats.merge(stats);
        self.counter.merge(counter);
        self.keys.merge(keys);
        self.values.merge(values);
    }
}

/// Per-thread emit machinery: scratch buffers for line formatting, the
/// `--raw-key` extractor, and (optionally) the `--sort-by` capture buffer.
///
/// `raw` and `scratch` are pure per-worker scratch and are *not* merged.
/// `sort_buf`, when present, IS merged from workers into the master so all
/// captured rows are sorted together before emission. See
/// [`LineEmitter::merge_sort_buf`].
pub(crate) struct LineEmitter {
    raw: RawExtractor,
    scratch: EmitScratch,
    pub(crate) sort_buf: Option<SortBuffer>,
}

impl LineEmitter {
    pub(crate) fn new(raw_key: Option<&str>, sort_by: Option<&str>) -> Self {
        Self {
            raw: RawExtractor::new(raw_key),
            scratch: EmitScratch::default(),
            sort_buf: sort_by.map(SortBuffer::new),
        }
    }

    /// Fold a worker's sort buffer into this one. No-op when either side
    /// has none. Mirrors the pre-refactor `Sinks::merge` behaviour.
    pub(crate) fn merge_sort_buf(&mut self, other: Option<SortBuffer>) {
        if let (Some(a), Some(b)) = (self.sort_buf.as_mut(), other) {
            a.merge(b);
        }
    }

    /// Emit one matched line. When `sort_buf` is active, the bytes are
    /// captured into the sort buffer (keyed by `--sort-by`); otherwise
    /// they're written straight to `out`.
    ///
    /// Handles `--raw-key` extraction, the passthrough memcpy fast path,
    /// and the format-dispatched slow path.
    pub(crate) fn emit<W: Write + ?Sized>(
        &mut self,
        parsed: &[(&str, &str)],
        line_buf: &[u8],
        raw_len: usize,
        parse_end: usize,
        cfg: &RunConfig,
        out: &mut W,
    ) -> std::io::Result<()> {
        // Capture path: redirect bytes into the sort buffer instead of `out`.
        // Aggregation modes without --raw-key produce no per-line bytes; skip
        // the capture in that case to avoid pointless allocations.
        if let Some(sort_buf) = self.sort_buf.as_mut() {
            if cfg.suppress_lines && self.raw.raw_key.is_none() {
                return Ok(());
            }
            let raw = &mut self.raw;
            let scratch = &mut self.scratch;
            return sort_buf.capture(parsed, |w| {
                emit_inner(parsed, line_buf, raw_len, parse_end, raw, scratch, cfg, w)
            });
        }

        emit_inner(
            parsed,
            line_buf,
            raw_len,
            parse_end,
            &mut self.raw,
            &mut self.scratch,
            cfg,
            out,
        )
    }

    /// Flush the sort buffer (if any) and the writer. Takes ownership of the
    /// buffer's contents, so subsequent calls are no-ops.
    pub(crate) fn flush_sort_buf<W: Write>(&mut self, out: &mut W) -> std::io::Result<()> {
        if let Some(sort_buf) = self.sort_buf.take() {
            sort_buf.emit(out)?;
            out.flush()?;
        }
        Ok(())
    }
}

/// Inner emit: writes `--raw-key` extraction (if any) and then the full line
/// through the configured `formatter`. Shared between the direct-emit and
/// sort-buf paths via a tiny indirection so both routes go through the same
/// logic.
///
/// `add_pairs` and `remove_keys` (from `cfg`) are pre-computed views over
/// the run-wide `LineTransform`; they're empty when no `--add`/`--rm` is set.
#[allow(clippy::too_many_arguments)]
#[inline]
fn emit_inner<W: Write + ?Sized>(
    parsed: &[(&str, &str)],
    line_buf: &[u8],
    raw_len: usize,
    parse_end: usize,
    raw: &mut RawExtractor,
    scratch: &mut EmitScratch,
    cfg: &RunConfig,
    out: &mut W,
) -> std::io::Result<()> {
    raw.emit(parsed, out)?;

    if cfg.suppress_lines {
        return Ok(());
    }

    // Passthrough copies bytes verbatim — no trait dispatch needed (and the
    // memcpy fast path is what makes this branch worth keeping separate).
    if cfg.passthrough {
        out.write_all(line_buf)?;
        if raw_len == parse_end {
            out.write_all(b"\n")?;
        }
        return Ok(());
    }

    let pairs: &[&[(&str, &str)]] = if cfg.add_pairs.is_empty() {
        &[parsed]
    } else {
        &[parsed, cfg.add_pairs]
    };

    // The trailer is the raw bytes between the parsed prefix and the end of
    // the line (whitespace + newline). Plain logfmt preserves it verbatim so
    // CRLF inputs stay CRLF; colored logfmt and jsonl ignore it.
    let trailer: &[u8] = if raw_len > parse_end {
        &line_buf[parse_end..raw_len]
    } else {
        &[]
    };

    // Buffer the formatted line into `scratch.buf`, then emit it with one
    // `write_all`. This is required for the parallel path: workers write
    // through `UnorderedSink` which flushes to the shared writer at
    // byte-count thresholds, so a multi-write `line` body would let
    // fragments of concurrent lines interleave. Per-line `Vec::clear` is
    // O(0); the allocation only grows once.
    scratch.buf.clear();
    cfg.formatter.line(
        &mut scratch.buf,
        pairs,
        cfg.remove_keys,
        trailer,
        &mut scratch.str_buf,
    )?;
    out.write_all(&scratch.buf)
}
