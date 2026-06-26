//! Per-line processing pipeline. Bundles the borrows that `process_line`
//! threaded as separate arguments (filter, time-filter, recorders, emitter,
//! config) into a single struct. The streaming helpers in `run.rs` hold
//! `&mut Pipeline` and call `process_line` for each line read.

use std::collections::VecDeque;
use std::io::Write;

use crate::filter::Filter;
use crate::logfmt;
use crate::run::TimeFilter;
use crate::sinks::{LineEmitter, Recorders, RunConfig};
use crate::timestamp;

/// Tracks before/after context lines around matched lines.
pub(crate) struct ContextState {
    before_buf: VecDeque<Vec<u8>>,
    before_n: usize,
    after_n: usize,
    remaining_after: usize,
}

impl ContextState {
    pub(crate) fn new(before_n: usize, after_n: usize) -> Self {
        Self {
            before_buf: VecDeque::with_capacity(before_n),
            before_n,
            after_n,
            remaining_after: 0,
        }
    }

    pub(crate) fn disabled() -> Self {
        Self::new(0, 0)
    }

    pub(crate) fn is_active(&self) -> bool {
        self.before_n > 0 || self.after_n > 0
    }

    pub(crate) fn reset(&mut self) {
        self.before_buf.clear();
        self.remaining_after = 0;
    }
}

/// Per-line state bundle: the borrows that every line-processing call needs.
///
/// One `Pipeline` is constructed per stream (per file, or once for stdin).
/// In the parallel path each worker builds its own, since `recorders` and
/// `emitter` are per-worker.
pub(crate) struct Pipeline<'a> {
    pub(crate) filter: &'a Filter,
    pub(crate) tf: &'a TimeFilter,
    pub(crate) cfg: &'a RunConfig<'a>,
    pub(crate) recorders: &'a mut Recorders,
    pub(crate) emitter: &'a mut LineEmitter,
    pub(crate) context: &'a mut ContextState,
}

impl<'a> Pipeline<'a> {
    pub(crate) fn new(
        filter: &'a Filter,
        tf: &'a TimeFilter,
        cfg: &'a RunConfig<'a>,
        recorders: &'a mut Recorders,
        emitter: &'a mut LineEmitter,
        context: &'a mut ContextState,
    ) -> Self {
        Self {
            filter,
            tf,
            cfg,
            recorders,
            emitter,
            context,
        }
    }

    /// True once the `--limit` matched-line count has been reached. Used by
    /// the streaming loops to break mid-chunk.
    #[inline]
    pub(crate) fn done(&self) -> bool {
        matches!(self.cfg.limit, Some(n) if self.recorders.stats.matched_lines >= n)
    }

    /// Per-chunk streaming-bucket flush. No-op when streaming is off (the
    /// common case); real work only when `-f`/`-F` + `--bucket`. Lives here
    /// so the borrow split (`&mut counter` + `&tz`) is encapsulated; `tz`
    /// now lives on the immutable `RunConfig`, so the borrow is trivial.
    #[inline]
    pub(crate) fn flush_closed_chunk<W: Write + ?Sized>(
        &mut self,
        output: &mut W,
    ) -> std::io::Result<()> {
        self.recorders
            .counter
            .flush_closed(output, self.cfg.formatter, &self.cfg.tz)
    }

    /// Process one line. `line` may include a trailing `\n` or `\r\n`; it is
    /// preserved verbatim in passthrough output. The caller owns the buffer
    /// (typically a slice into a `BufReader`'s internal buffer or a small
    /// carry-over `Vec`).
    #[inline]
    pub(crate) fn process_line<W: Write + ?Sized>(
        &mut self,
        line: &[u8],
        output: &mut W,
    ) -> anyhow::Result<()> {
        // Preserve the original bytes for output; trim a trailing newline for parsing.
        let raw_len = line.len();
        let mut parse_end = raw_len;
        while parse_end > 0 && matches!(line[parse_end - 1], b'\n' | b'\r') {
            parse_end -= 1;
        }

        self.recorders.stats.bytes += raw_len;
        self.recorders.stats.total_lines += 1;

        let parse_slice = &line[..parse_end];
        let line_str = match std::str::from_utf8(parse_slice) {
            Ok(s) => s,
            Err(_) => {
                self.recorders.stats.invalid_utf += 1;
                return Ok(());
            }
        };

        let mut pairs = logfmt::PairsBuffer::<256>::new();
        let (parsed, overflow) = pairs.parse(line_str);

        // Extract the timestamp at most once per line, only when something
        // downstream actually needs it (time-window filter or grouping counter).
        let ts = if !self.tf.is_empty() || self.recorders.counter.is_active() {
            timestamp::extract_timestamp(parsed)
        } else {
            None
        };

        let matched = self.tf.check(ts)
            && (self.filter.is_empty() || self.filter.matches(parsed))
            && self.cfg.sampler.as_ref().is_none_or(|s| s.keep(parsed));

        self.recorders.stats.pairs += parsed.len();
        self.recorders.stats.overflow += overflow as usize;
        if matched {
            self.recorders.stats.matched_lines += 1;
            self.recorders.counter.record(parsed, ts);
            // Streaming-bucket `flush_closed` used to live here; it now runs
            // once per chunk in the streaming loops (`run::stream_*` /
            // `follow_loop`), which is a real win when many lines match.
            self.recorders.keys.record(parsed);
            self.recorders.values.record(parsed);
            // Flush buffered before-context lines verbatim, then emit match.
            if self.context.is_active() {
                for ctx_line in self.context.before_buf.drain(..) {
                    output.write_all(&ctx_line)?;
                }
                self.context.remaining_after = self.context.after_n;
            }
            self.emitter
                .emit(parsed, line, raw_len, parse_end, self.cfg, output)?;
        } else if self.context.remaining_after > 0 {
            // Emit as after-context verbatim.
            output.write_all(line)?;
            self.context.remaining_after -= 1;
        } else if self.context.before_n > 0 {
            // Buffer for potential before-context, recycling evicted slots.
            let mut slot = if self.context.before_buf.len() == self.context.before_n {
                let mut v = self.context.before_buf.pop_front().unwrap();
                v.clear();
                v
            } else {
                Vec::with_capacity(line.len())
            };
            slot.extend_from_slice(line);
            self.context.before_buf.push_back(slot);
        }
        Ok(())
    }
}
