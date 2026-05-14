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

use crate::output;
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
    enabled: bool,
    keys: RapidHashSet<SmartString>,
}

impl KeyGather {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            keys: RapidHashSet::default(),
        }
    }

    pub(crate) fn record(&mut self, pairs: &[(&str, &str)]) {
        if !self.enabled {
            return;
        }
        for (k, _) in pairs {
            intern_into_set(&mut self.keys, k);
        }
    }

    pub(crate) fn report<W: Write>(&self, out: &mut W, json: bool) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if json {
            crate::json::write_string_set_json(out, &self.keys)
        } else {
            output::write_string_set_logfmt(out, &self.keys)
        }
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

    pub(crate) fn report<W: Write>(&self, out: &mut W, json: bool) -> std::io::Result<()> {
        if !self.is_active() {
            return Ok(());
        }
        if json {
            crate::json::write_values_summary_json(out, &self.keys, &self.values)
        } else {
            output::write_values_summary_logfmt(out, &self.keys, &self.values)
        }
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
    sinks: &crate::sinks::Sinks,
    count_only: bool,
    tz: &jiff::tz::TimeZone,
    output: &mut W,
) -> anyhow::Result<()> {
    sinks.stats.report();
    let json = matches!(sinks.line_mode, crate::sinks::LineMode::Json);
    if sinks.counter.streaming {
        sinks.counter.flush_remaining(output, tz, json)?;
    } else {
        sinks.counter.report(output, tz, json)?;
    }
    sinks.keys.report(output, json)?;
    sinks.values.report(output, json)?;
    if count_only {
        if json {
            crate::json::write_count_json(output, sinks.stats.matched_lines as u64)?;
        } else {
            writeln!(output, "{}", sinks.stats.matched_lines)?;
        }
    }
    output.flush()?;
    Ok(())
}
