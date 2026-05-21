//! Per-run bookkeeping + the `--list-keys` / `--list-values-for` gathers.
//!
//! `Stats` accumulates byte / line / pair counters for the final summary
//! log line. `KeyGather` records every distinct key seen on matched lines
//! (suppresses normal line output while active). `ValueGather` does the
//! same per key value, for one or more user-requested keys.

use humanize_bytes::humanize_bytes_binary;
use rapidhash::RapidHashSet;
use smartstring::alias::String as SmartString;
use std::io::Write;
use std::time::Instant;

use crate::output::{Formatter, OutputFormat};
use crate::raw_extractor::unescape_for_key;

/// Insert `s` into `set` only if not already present, allocating a
/// `SmartString` lazily.
pub(crate) fn intern_into_set(set: &mut RapidHashSet<SmartString>, s: &str) {
    if !set.contains(s) {
        let mut x = SmartString::new_const();
        x.push_str(s);
        set.insert(x);
    }
}

/// Collects every distinct key seen on matched lines. Active only when
/// `--list-keys` is set; in that mode line output is suppressed.
#[derive(Default)]
pub(crate) struct KeyGather {
    is_active: bool,
    keys: RapidHashSet<SmartString>,
}

impl KeyGather {
    pub(crate) fn new(is_active: bool) -> Self {
        Self {
            is_active,
            keys: RapidHashSet::default(),
        }
    }

    #[inline]
    pub(crate) fn record(&mut self, pairs: &[(&str, &str)]) {
        if !self.is_active {
            return;
        }
        for (k, _) in pairs {
            intern_into_set(&mut self.keys, k);
        }
    }

    pub(crate) fn report<W: Write>(
        &self,
        out: &mut W,
        formatter: &Formatter,
    ) -> std::io::Result<()> {
        if !self.is_active {
            return Ok(());
        }
        formatter.string_set(out, &self.keys)
    }

    pub(crate) fn merge(&mut self, other: Self) {
        self.keys.extend(other.keys);
    }
}

/// Collects every distinct value seen for each requested key, on matched
/// lines. Active when at least one `--list-values-for=<key>` is given;
/// suppresses line output.
#[derive(Default)]
pub(crate) struct ValueGather {
    keys: Vec<SmartString>,
    values: Vec<RapidHashSet<SmartString>>,
    scratch: String,
}

impl ValueGather {
    pub(crate) fn new(keys: Vec<SmartString>) -> Self {
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

    #[inline]
    pub(crate) fn record(&mut self, pairs: &[(&str, &str)]) {
        if !self.is_active() {
            return;
        }
        for (i, key) in self.keys.iter().enumerate() {
            if unescape_for_key(pairs, key, &mut self.scratch).is_some() {
                intern_into_set(&mut self.values[i], &self.scratch);
            }
        }
    }

    pub(crate) fn report<W: Write>(
        &self,
        out: &mut W,
        formatter: &Formatter,
    ) -> std::io::Result<()> {
        if !self.is_active() {
            return Ok(());
        }
        formatter.values_summary(out, &self.keys, &self.values)
    }

    pub(crate) fn merge(&mut self, other: Self) {
        debug_assert_eq!(self.values.len(), other.values.len());
        for (a, b) in self.values.iter_mut().zip(other.values) {
            a.extend(b);
        }
    }
}

pub(crate) struct Stats {
    pub(crate) bytes: usize,
    pub(crate) matched_lines: usize,
    pub(crate) total_lines: usize,
    pub(crate) invalid_utf: usize,
    pub(crate) pairs: usize,
    pub(crate) overflow: usize,
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
    pub(crate) fn merge(&mut self, other: Self) {
        self.bytes += other.bytes;
        self.matched_lines += other.matched_lines;
        self.total_lines += other.total_lines;
        self.invalid_utf += other.invalid_utf;
        self.pairs += other.pairs;
        self.overflow += other.overflow;
        // `started` stays as the master's earliest start time.
    }

    pub(crate) fn report(&self) {
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

/// End-of-run summary block: stats, counter rows, key/value gathers,
/// optional bare `--count` number. Pulled out so the `run::run`
/// happy-path doesn't have to thread `bare_count` through everywhere.
pub(crate) fn emit_summaries<W: Write>(
    recorders: &mut crate::sinks::Recorders,
    cfg: &crate::sinks::RunConfig<'_>,
    count_only: bool,
    output: &mut W,
) -> anyhow::Result<()> {
    recorders.stats.report();
    recorders
        .counter
        .emit_final(output, cfg.formatter, &cfg.tz)?;
    recorders.keys.report(output, cfg.formatter)?;
    recorders.values.report(output, cfg.formatter)?;
    if count_only {
        cfg.formatter
            .count_only(output, recorders.stats.matched_lines as u64)?;
    }
    output.flush()?;
    emit_aggregate_warnings(&recorders.counter);
    Ok(())
}

/// Walk the merged counter and emit one stderr warning per (key, kind)
/// that fired during the run:
/// - per numeric key with `non_numeric > 0`: "N lines skipped".
/// - once, if any accumulator hit the sample cap and switched to
///   reservoir sampling.
///
/// Warnings go to stderr (not the `output` writer) so they don't
/// interleave with structured agg rows piped into downstream tools.
fn emit_aggregate_warnings(counter: &crate::counter::Counter) {
    use std::collections::BTreeMap;
    let totals = crate::aggregate::WarningTotals::collect(counter.iter_aggregates());
    // BTreeMap for deterministic key order in the warnings.
    let ordered: BTreeMap<&str, u64> = totals
        .non_numeric
        .iter()
        .map(|(k, n)| (k.as_str(), *n))
        .collect();
    for (key, n) in ordered {
        eprintln!("warning: non-numeric values for '{key}': {n} lines skipped");
    }
    if totals.reservoir_engaged {
        let cap = counter.spec().sample_cap;
        eprintln!(
            "warning: sample cap ({cap}) exceeded for at least one (group, key); \
             percentiles are approximate (raise --sample-cap to keep them exact)"
        );
    }
}
