//! Per-line processing pipeline. Bundles the borrows that `process_line`
//! threaded as separate arguments (filter, time-filter, sinks, transform
//! views) plus a reusable line buffer, into a single struct. The streaming
//! helpers in `run.rs` hold `&mut Pipeline` and call `process_line` for each
//! line read.

use std::io::Write;

use crate::filter::Filter;
use crate::logfmt;
use crate::output::{JsonlFormat, JsonlOptions, LogfmtFormat, LogfmtOptions, OutputFormat};
use crate::raw_extractor::RawExtractor;
use crate::run::TimeFilter;
use crate::sinks::{LineMode, Sinks};
use crate::timestamp;
use crate::transform::EmitScratch;

/// Per-line state bundle: the borrows that every line-processing call needs,
/// plus the reusable byte buffer that `read_until` fills.
///
/// One `Pipeline` is constructed per stream (per file, or once for stdin).
/// In the parallel path each worker builds its own, since `sinks` is
/// per-worker.
pub(crate) struct Pipeline<'a> {
    pub(crate) filter: &'a Filter,
    pub(crate) tf: &'a TimeFilter,
    pub(crate) sinks: &'a mut Sinks,
    pub(crate) add_pairs: &'a [(&'a str, &'a str)],
    pub(crate) remove_keys: &'a [&'a str],
    /// Reused across calls: `read_until` appends into it, `process_line`
    /// clears at the end.
    pub(crate) line_buf: Vec<u8>,
}

impl<'a> Pipeline<'a> {
    pub(crate) fn new(
        filter: &'a Filter,
        tf: &'a TimeFilter,
        sinks: &'a mut Sinks,
        add_pairs: &'a [(&'a str, &'a str)],
        remove_keys: &'a [&'a str],
    ) -> Self {
        Self {
            filter,
            tf,
            sinks,
            add_pairs,
            remove_keys,
            line_buf: Vec::new(),
        }
    }

    /// Process the bytes currently in `self.line_buf` (the caller just
    /// appended `n_bytes` to it via `read_until`). Writes any matched
    /// output to `output`. Clears `self.line_buf` on return.
    pub(crate) fn process_line<W: Write + ?Sized>(
        &mut self,
        n_bytes: usize,
        output: &mut W,
    ) -> anyhow::Result<()> {
        // Preserve the original bytes for output; trim a trailing newline for parsing.
        let raw_len = self.line_buf.len();
        let mut parse_end = raw_len;
        while parse_end > 0 && matches!(self.line_buf[parse_end - 1], b'\n' | b'\r') {
            parse_end -= 1;
        }

        self.sinks.stats.bytes += n_bytes;
        self.sinks.stats.total_lines += 1;

        let parse_slice = &self.line_buf[..parse_end];
        let line_str = match std::str::from_utf8(parse_slice) {
            Ok(s) => s,
            Err(_) => {
                self.sinks.stats.invalid_utf += 1;
                self.line_buf.clear();
                return Ok(());
            }
        };

        let mut pairs = logfmt::PairsBuffer::<256>::new();
        let (parsed, overflow) = pairs.parse(line_str);

        // Extract the timestamp at most once per line, only when something
        // downstream actually needs it (time-window filter or grouping counter).
        let ts = if !self.tf.is_empty() || self.sinks.counter.is_active() {
            timestamp::extract_timestamp(parsed)
        } else {
            None
        };

        let matched = self.tf.check(ts)
            && (self.filter.is_empty() || self.filter.matches(parsed))
            && self.sinks.sampler.as_ref().is_none_or(|s| s.keep(parsed));

        self.sinks.stats.pairs += parsed.len();
        self.sinks.stats.overflow += overflow as usize;
        if matched {
            self.sinks.stats.matched_lines += 1;
            self.sinks.counter.record(parsed, ts);
            // Streaming mode (`-f`/`-F` + `--bucket`): emit any buckets that
            // have passed the close threshold. Cheap when nothing is closeable;
            // a no-op when streaming is off.
            self.sinks.counter.flush_closed(
                output,
                &self.sinks.tz,
                matches!(self.sinks.line_mode, LineMode::Json),
            )?;
            self.sinks.keys.record(parsed);
            self.sinks.values.record(parsed);
            if let Some(sort_buf) = self.sinks.sort_buf.as_mut() {
                // Aggregation modes without --raw-key produce no per-line bytes;
                // skip the capture in that case (counters above already recorded).
                if !self.sinks.suppress_lines || self.sinks.raw.raw_key.is_some() {
                    let raw = &mut self.sinks.raw;
                    let scratch = &mut self.sinks.emit_scratch;
                    let suppress = self.sinks.suppress_lines;
                    let mode = self.sinks.line_mode;
                    let line_buf = &self.line_buf;
                    let add_pairs = self.add_pairs;
                    let remove_keys = self.remove_keys;
                    sort_buf.capture(parsed, |w| {
                        emit_match(
                            parsed,
                            line_buf,
                            raw_len,
                            parse_end,
                            raw,
                            suppress,
                            mode,
                            add_pairs,
                            remove_keys,
                            scratch,
                            w,
                        )
                    })?;
                }
            } else {
                emit_match(
                    parsed,
                    &self.line_buf,
                    raw_len,
                    parse_end,
                    &mut self.sinks.raw,
                    self.sinks.suppress_lines,
                    self.sinks.line_mode,
                    self.add_pairs,
                    self.remove_keys,
                    &mut self.sinks.emit_scratch,
                    output,
                )?;
            }
        }
        self.line_buf.clear();
        Ok(())
    }
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
/// followed by the full line in the configured `LineMode` unless line output
/// is suppressed by an aggregation/raw-key mode.
///
/// `add_pairs` and `remove_keys` are pre-computed views over the run-wide
/// `LineTransform`; they're empty when no `--add`/`--rm` is set. They live
/// in the caller's frame for the whole stream, so per-line work is just two
/// slice borrows.
#[allow(clippy::too_many_arguments)]
fn emit_match<W: Write + ?Sized>(
    parsed: &[(&str, &str)],
    line_buf: &[u8],
    raw_len: usize,
    parse_end: usize,
    raw: &mut RawExtractor,
    suppress_lines: bool,
    mode: LineMode,
    add_pairs: &[(&str, &str)],
    remove_keys: &[&str],
    scratch: &mut EmitScratch,
    out: &mut W,
) -> std::io::Result<()> {
    raw.emit(parsed, out)?;

    if suppress_lines {
        return Ok(());
    }

    // Passthrough copies bytes verbatim — no trait dispatch needed (and the
    // memcpy fast path is what makes this mode worth keeping separate).
    if let LineMode::Passthrough = mode {
        out.write_all(line_buf)?;
        if raw_len == parse_end {
            out.write_all(b"\n")?;
        }
        return Ok(());
    }

    let pairs: &[&[(&str, &str)]] = if add_pairs.is_empty() {
        &[parsed]
    } else {
        &[parsed, add_pairs]
    };

    match mode {
        LineMode::Passthrough => unreachable!("handled above"),
        LineMode::Json => {
            JsonlFormat::output_line(out, pairs, remove_keys, &JsonlOptions, &mut scratch.str_buf)
        }
        LineMode::Colored => {
            let EmitScratch { buf, str_buf } = scratch;
            buf.clear();
            LogfmtFormat::output_line(
                buf,
                pairs,
                remove_keys,
                &LogfmtOptions { color: true },
                str_buf,
            )?;
            out.write_all(buf)
        }
        LineMode::Plain => {
            let EmitScratch { buf, str_buf } = scratch;
            buf.clear();
            LogfmtFormat::output_line(
                buf,
                pairs,
                remove_keys,
                &LogfmtOptions { color: false },
                str_buf,
            )?;
            append_reconstructed_plain_tail(buf, line_buf, raw_len, parse_end);
            out.write_all(buf)
        }
    }
}
