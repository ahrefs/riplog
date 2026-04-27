//! Top-level pipeline: open input, optionally bisect a time slice, stream
//! lines through the filter, write matches to the chosen output, optionally
//! follow.

use humanize_bytes::humanize_bytes_binary;
use rapidhash::RapidHashMap;
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;
use std::{
    fmt::Write as _,
    fs::File,
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    time::{Duration, Instant},
};

use crate::bisect::{self, Side};
use crate::cli::Cli;
use crate::filter::Filter;
use crate::logfmt;
use crate::timestamp::{self, Timestamp};

type Combo = SmallVec<[SmartString; 3]>;

/// Counts matched lines grouped by the value tuple of `keys`. Missing keys
/// produce an empty `SmartString` slot (rendered as `key=` in the report).
#[derive(Default)]
struct Counter {
    keys: Vec<String>,
    counts: RapidHashMap<Combo, usize>,
    scratch: String,
}

impl Counter {
    fn new(keys: Vec<String>) -> Self {
        Self {
            keys,
            counts: RapidHashMap::default(),
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
        let mut combo: Combo = SmallVec::with_capacity(self.keys.len());
        for k in &self.keys {
            let mut value = SmartString::new_const();
            for (pk, pv) in pairs {
                if *pk == k.as_str() {
                    self.scratch.clear();
                    logfmt::unescape_value(pv.as_bytes(), &mut self.scratch);
                    value.push_str(&self.scratch);
                    break;
                }
            }
            combo.push(value);
        }
        *self.counts.entry(combo).or_insert(0) += 1;
    }

    fn report(&self) {
        if !self.is_active() || self.counts.is_empty() {
            return;
        }
        let mut entries: Vec<(&Combo, &usize)> = self.counts.iter().collect();
        // Descending by count, ties broken by combo for deterministic output.
        entries.sort_unstable_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let count_w = entries[0].1.to_string().len();
        let mut out = String::from("count-by:\n");
        for (combo, count) in entries {
            let _ = write!(out, "  {:>w$}", count, w = count_w);
            for (k, v) in self.keys.iter().zip(combo.iter()) {
                let _ = write!(out, " {k}={}", v.as_str());
            }
            out.push('\n');
        }
        // Trim trailing newline so log formatter doesn't add a blank line.
        if out.ends_with('\n') {
            out.pop();
        }
        log::info!("{out}");
    }
}

/// Strict timestamp filter applied per-line on top of the bisected byte range.
/// The bisect is an over-approximation, so the byte range can include lines
/// outside `[from, to]`; this filter drops them.
///
/// Lines with no parseable timestamp are dropped when either bound is set
/// (we can't prove they're in range).
#[derive(Default, Clone, Copy)]
struct TimeFilter {
    from: Option<Timestamp>,
    to: Option<Timestamp>,
}

impl TimeFilter {
    fn is_empty(&self) -> bool {
        self.from.is_none() && self.to.is_none()
    }

    fn matches(&self, pairs: &[(&str, &str)]) -> bool {
        if self.is_empty() {
            return true;
        }
        let Some(ts) = timestamp::extract_timestamp(pairs) else {
            return false;
        };
        if let Some(t1) = self.from
            && ts < t1
        {
            return false;
        }
        if let Some(t2) = self.to
            && ts > t2
        {
            return false;
        }
        true
    }
}

const FOLLOW_POLL: Duration = Duration::from_millis(200);

pub fn run(cli: &Cli) -> anyhow::Result<()> {
    let filter = Filter::parse(&cli.keys)?;

    let mut output: Box<dyn Write> = match &cli.output {
        Some(path) => Box::new(BufWriter::new(File::create(path)?)),
        None => Box::new(BufWriter::new(std::io::stdout().lock())),
    };

    let mut stats = Stats::default();
    let mut counter = Counter::new(cli.count_by.clone());

    let need_seek = cli.from.is_some() || cli.to.is_some() || cli.follow;
    let path = match &cli.file {
        Some(p) => p,
        None => {
            if need_seek {
                anyhow::bail!("`-F`, `--from`, `--to` require a file argument");
            }
            stream_unbounded(
                &mut std::io::stdin().lock(),
                &filter,
                &mut output,
                &mut stats,
                &mut counter,
            )?;
            output.flush()?;
            stats.report();
            counter.report();
            return Ok(());
        }
    };

    let mut file = File::open(path)?;
    let file_len = file.seek(SeekFrom::End(0))?;
    let window = (cli.window_secs as i64).saturating_mul(1_000_000_000);

    // Resolve bounds (with shorthand) and bisect.
    let t_bisect = Instant::now();
    let (file_first, file_last) = if cli.from.is_some() || cli.to.is_some() {
        (
            bisect::peek_first_timestamp(&mut file)?,
            bisect::peek_last_timestamp(&mut file)?,
        )
    } else {
        (None, None)
    };

    let mut tf = TimeFilter::default();
    let start_byte: u64 = match cli.from.as_deref() {
        Some(from) => {
            let t1 = timestamp::resolve_bound(from, file_first, file_last, file_first)?;
            tf.from = Some(t1);
            bisect::bisect(&mut file, t1, window, Side::Lower)?
        }
        None if cli.follow => file_len, // tail-from-EOF when no --from
        None => 0,
    };
    let end_byte: u64 = match cli.to.as_deref() {
        Some(to) if !cli.follow => {
            let t2 = timestamp::resolve_bound(to, file_first, file_last, file_last)?;
            tf.to = Some(t2);
            bisect::bisect(&mut file, t2, window, Side::Upper)?
        }
        _ => file_len,
    };

    if cli.from.is_some() || cli.to.is_some() {
        log::info!(
            "bisect: [{}, {}] window={}s -> bytes [{start_byte}, {end_byte}) ({}) in {:.3}s",
            cli.from.as_deref().unwrap_or("-"),
            cli.to.as_deref().unwrap_or("-"),
            cli.window_secs,
            humanize_bytes_binary!(end_byte.saturating_sub(start_byte)),
            t_bisect.elapsed().as_secs_f64()
        );
    }

    file.seek(SeekFrom::Start(start_byte))?;
    let mut reader = BufReader::new(file);

    // Phase 1: stream up to end_byte.
    let max_bytes = end_byte.saturating_sub(start_byte);
    stream_bounded(
        &mut reader,
        max_bytes,
        &filter,
        &tf,
        &mut output,
        &mut stats,
        &mut counter,
    )?;
    output.flush()?;

    // Phase 2: follow. New lines are assumed monotone, so don't re-apply tf.
    if cli.follow {
        follow_loop(reader, &filter, &mut output, &mut stats, &mut counter)?;
    }

    stats.report();
    counter.report();

    Ok(())
}

#[derive(Default)]
struct Stats {
    bytes: usize,
    matched_lines: usize,
    total_lines: usize,
    invalid_utf: usize,
    pairs: usize,
    overflow: usize,
    started: Option<Instant>,
}

impl Stats {
    fn report(&self) {
        let elapsed = self
            .started
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        let rate = if elapsed > 0.0 {
            (self.bytes as f64 / elapsed) as u64
        } else {
            0
        };
        log::info!(
            "{} read, {}/{} lines matched ({} invalid utf8), {} pairs ({} overflow), {}/s",
            humanize_bytes_binary!(self.bytes),
            self.matched_lines,
            self.total_lines,
            self.invalid_utf,
            self.pairs,
            self.overflow,
            humanize_bytes_binary!(rate),
        );
    }
}

/// Read a fixed byte budget from `reader`, write matching lines to `output`.
fn stream_bounded<R: BufRead, W: Write>(
    reader: &mut R,
    max_bytes: u64,
    filter: &Filter,
    tf: &TimeFilter,
    output: &mut W,
    stats: &mut Stats,
    counter: &mut Counter,
) -> anyhow::Result<()> {
    let mut line_buf = Vec::new();
    let mut total_read: u64 = 0;
    stats.started.get_or_insert_with(Instant::now);

    while total_read < max_bytes {
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            break;
        }
        total_read += n as u64;
        process_line(&mut line_buf, filter, tf, output, stats, counter, n)?;
    }
    Ok(())
}

/// Read until EOF (e.g. stdin), write matching lines to `output`.
fn stream_unbounded<R: Read, W: Write>(
    reader: &mut R,
    filter: &Filter,
    output: &mut W,
    stats: &mut Stats,
    counter: &mut Counter,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut line_buf = Vec::new();
    stats.started.get_or_insert_with(Instant::now);

    loop {
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            break;
        }
        process_line(
            &mut line_buf,
            filter,
            &TimeFilter::default(),
            output,
            stats,
            counter,
            n,
        )?;
    }
    Ok(())
}

fn process_line<W: Write>(
    line_buf: &mut Vec<u8>,
    filter: &Filter,
    tf: &TimeFilter,
    output: &mut W,
    stats: &mut Stats,
    counter: &mut Counter,
    n_bytes: usize,
) -> anyhow::Result<()> {
    // Preserve the original bytes for output; trim a trailing newline for parsing.
    let raw_len = line_buf.len();
    let mut parse_end = raw_len;
    while parse_end > 0 && matches!(line_buf[parse_end - 1], b'\n' | b'\r') {
        parse_end -= 1;
    }

    let parse_slice = &line_buf[..parse_end];
    let line_str = match std::str::from_utf8(parse_slice) {
        Ok(s) => s,
        Err(_) => {
            stats.invalid_utf += 1;
            stats.bytes += n_bytes;
            stats.total_lines += 1;
            line_buf.clear();
            return Ok(());
        }
    };

    let mut pairs = logfmt::PairsBuffer::<256>::new();
    let (parsed, overflow) = pairs.parse(line_str);

    let matched = tf.matches(parsed) && (filter.is_empty() || filter.matches(parsed));

    stats.bytes += n_bytes;
    stats.total_lines += 1;
    stats.pairs += parsed.len();
    stats.overflow += overflow as usize;
    if matched {
        stats.matched_lines += 1;
        counter.record(parsed);
        output.write_all(line_buf)?;
        if raw_len == parse_end {
            // No trailing newline in the source; add one for tidy output.
            output.write_all(b"\n")?;
        }
    }
    line_buf.clear();
    Ok(())
}

fn follow_loop<W: Write>(
    mut reader: BufReader<File>,
    filter: &Filter,
    output: &mut W,
    stats: &mut Stats,
    counter: &mut Counter,
) -> anyhow::Result<()> {
    let mut line_buf = Vec::new();
    loop {
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            output.flush()?;
            std::thread::sleep(FOLLOW_POLL);
            continue;
        }
        // Only process complete lines (terminated by '\n'). If not, hold for
        // more bytes to arrive.
        if !line_buf.ends_with(b"\n") {
            // Partial — put back via short-circuit: keep accumulating.
            std::thread::sleep(FOLLOW_POLL);
            continue;
        }
        process_line(
            &mut line_buf,
            filter,
            &TimeFilter::default(),
            output,
            stats,
            counter,
            n,
        )?;
    }
}

