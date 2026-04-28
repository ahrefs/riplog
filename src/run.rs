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
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use crate::bisect::{self, Side};
use crate::cli::Cli;
use crate::filter::Filter;
use crate::logfmt;
use crate::timestamp::{self, Timestamp};

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
            if !self.keys.contains(*k) {
                let mut s = SmartString::new_const();
                s.push_str(k);
                self.keys.insert(s);
            }
        }
    }

    fn report<W: Write>(&self, out: &mut W) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut sorted: Vec<&SmartString> = self.keys.iter().collect();
        sorted.sort_unstable();
        for k in sorted {
            writeln!(out, "{k}")?;
        }
        Ok(())
    }
}

/// Collects every distinct value seen for each requested key, on matched
/// lines. Active when at least one `--list-values-for=<key>` is given;
/// suppresses line output.
#[derive(Default)]
struct ValueGather {
    keys: Vec<String>,
    values: Vec<RapidHashSet<SmartString>>,
    scratch: String,
}

impl ValueGather {
    fn new(keys: Vec<String>) -> Self {
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
            for (pk, pv) in pairs {
                if *pk == key.as_str() {
                    self.scratch.clear();
                    logfmt::unescape_value(pv.as_bytes(), &mut self.scratch);
                    if !self.values[i].contains(self.scratch.as_str()) {
                        let mut s = SmartString::new_const();
                        s.push_str(&self.scratch);
                        self.values[i].insert(s);
                    }
                    break;
                }
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
            let mut sorted: Vec<&SmartString> = set.iter().collect();
            sorted.sort_unstable();
            for v in sorted {
                writeln!(out, "{v}")?;
            }
        }
        Ok(())
    }
}

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

    fn report<W: Write>(&self, out: &mut W) -> std::io::Result<()> {
        if !self.is_active() || self.counts.is_empty() {
            return Ok(());
        }
        let mut entries: Vec<(&Combo, &usize)> = self.counts.iter().collect();
        // Descending by count, ties broken by combo for deterministic output.
        entries.sort_unstable_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let count_w = entries[0].1.to_string().len();
        for (combo, count) in entries {
            write!(out, "{count:>count_w$}")?;
            for (k, v) in self.keys.iter().zip(combo.iter()) {
                write!(out, " {k}={}", v.as_str())?;
            }
            writeln!(out)?;
        }
        Ok(())
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
    install_signal_handler();

    if cli.time_range && (cli.follow || cli.follow_reopen) {
        anyhow::bail!("`--time-range` cannot be combined with `-f` or `-F`");
    }

    let filter = Filter::parse(&cli.keys)?;
    let following = cli.follow || cli.follow_reopen;
    let suppress_lines =
        cli.list_keys || cli.count || !cli.list_values_for.is_empty();
    let tz = timestamp::resolve_tz(cli.tz.as_deref())?;

    let mut output: Box<dyn Write> = match &cli.output {
        Some(path) => Box::new(BufWriter::new(File::create(path)?)),
        None => Box::new(BufWriter::new(std::io::stdout().lock())),
    };

    let mut stats = Stats::default();
    let mut counter = Counter::new(cli.count_by.clone());
    let mut keys = KeyGather::new(cli.list_keys);
    let mut values = ValueGather::new(cli.list_values_for.clone());

    let need_seek = cli.from.is_some() || cli.to.is_some() || following || cli.time_range;
    let path = match &cli.file {
        Some(p) => p,
        None => {
            if need_seek {
                anyhow::bail!(
                    "`-f`, `-F`, `--from`, `--to`, `--time-range` require a file argument"
                );
            }
            stream_unbounded(
                &mut std::io::stdin().lock(),
                &filter,
                &mut output,
                &mut stats,
                &mut counter,
                &mut keys,
                &mut values,
                suppress_lines,
            )?;
            output.flush()?;
            emit_summaries(&stats, &counter, &keys, &values, cli.count, &mut output)?;
            return Ok(());
        }
    };

    let mut file = File::open(path)?;

    // Time-range mode: scan head/tail for min/max, print, and exit.
    if cli.time_range {
        let first = bisect::min_timestamp_in_head(&mut file)?;
        let last = bisect::max_timestamp_in_tail(&mut file)?;
        match (first, last) {
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
        None if following => file_len, // tail-from-EOF when no --from
        None => 0,
    };
    let end_byte: u64 = match cli.to.as_deref() {
        Some(to) if !following => {
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
    let file_for_reopen = if following {
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
        &mut keys,
        &mut values,
        suppress_lines,
    )?;
    output.flush()?;

    // Phase 2: follow. New lines are assumed monotone, so don't re-apply tf.
    if let Some(handle) = file_for_reopen
        && !interrupted()
    {
        follow_loop(
            path,
            handle,
            reader,
            cli.follow_reopen,
            &filter,
            &mut output,
            &mut stats,
            &mut counter,
            &mut keys,
            &mut values,
            suppress_lines,
        )?;
    }

    output.flush()?;
    emit_summaries(&stats, &counter, &keys, &values, cli.count, &mut output)?;

    Ok(())
}

fn emit_summaries<W: Write>(
    stats: &Stats,
    counter: &Counter,
    keys: &KeyGather,
    values: &ValueGather,
    count_only: bool,
    output: &mut W,
) -> anyhow::Result<()> {
    stats.report();
    counter.report(output)?;
    keys.report(output)?;
    values.report(output)?;
    if count_only {
        writeln!(output, "{}", stats.matched_lines)?;
    }
    output.flush()?;
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
#[allow(clippy::too_many_arguments)]
fn stream_bounded<R: BufRead, W: Write>(
    reader: &mut R,
    max_bytes: u64,
    filter: &Filter,
    tf: &TimeFilter,
    output: &mut W,
    stats: &mut Stats,
    counter: &mut Counter,
    keys: &mut KeyGather,
    values: &mut ValueGather,
    suppress_lines: bool,
) -> anyhow::Result<()> {
    let mut line_buf = Vec::new();
    let mut total_read: u64 = 0;
    stats.started.get_or_insert_with(Instant::now);

    while total_read < max_bytes && !interrupted() {
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            break;
        }
        total_read += n as u64;
        process_line(
            &mut line_buf,
            filter,
            tf,
            output,
            stats,
            counter,
            keys,
            values,
            suppress_lines,
            n,
        )?;
    }
    Ok(())
}

/// Read until EOF (e.g. stdin), write matching lines to `output`.
#[allow(clippy::too_many_arguments)]
fn stream_unbounded<R: Read, W: Write>(
    reader: &mut R,
    filter: &Filter,
    output: &mut W,
    stats: &mut Stats,
    counter: &mut Counter,
    keys: &mut KeyGather,
    values: &mut ValueGather,
    suppress_lines: bool,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(reader);
    let mut line_buf = Vec::new();
    stats.started.get_or_insert_with(Instant::now);

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
            stats,
            counter,
            keys,
            values,
            suppress_lines,
            n,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_line<W: Write>(
    line_buf: &mut Vec<u8>,
    filter: &Filter,
    tf: &TimeFilter,
    output: &mut W,
    stats: &mut Stats,
    counter: &mut Counter,
    keys: &mut KeyGather,
    values: &mut ValueGather,
    suppress_lines: bool,
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
        keys.record(parsed);
        values.record(parsed);
        if !suppress_lines {
            output.write_all(line_buf)?;
            if raw_len == parse_end {
                // No trailing newline in the source; add one for tidy output.
                output.write_all(b"\n")?;
            }
        }
    }
    line_buf.clear();
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
    stats: &mut Stats,
    counter: &mut Counter,
    keys: &mut KeyGather,
    values: &mut ValueGather,
    suppress_lines: bool,
) -> anyhow::Result<()> {
    let mut line_buf = Vec::new();
    let mut pos = reader.stream_position()?;

    while !interrupted() {
        let n = reader.read_until(b'\n', &mut line_buf)?;
        if n == 0 {
            output.flush()?;
            if reopen && let Some((new_handle, new_reader)) = check_rotation(path, &handle, pos)? {
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
            stats,
            counter,
            keys,
            values,
            suppress_lines,
            n,
        )?;
    }
    Ok(())
}

/// On EOF, decide whether the path now resolves to a different file (rotation)
/// or has shrunk below our position (truncation). Returns a fresh
/// `(File, BufReader)` if so.
fn check_rotation(
    path: &Path,
    current: &File,
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
        if !should_reopen
            && let Ok(cur_meta) = current.metadata()
            && (path_meta.ino() != cur_meta.ino() || path_meta.dev() != cur_meta.dev())
        {
            should_reopen = true;
        }
    }
    // `current` is only used on unix; silence the warning elsewhere.
    #[cfg(not(unix))]
    let _ = current;

    if !should_reopen {
        return Ok(None);
    }
    let f = File::open(path)?;
    let dup = f.try_clone()?;
    Ok(Some((f, BufReader::new(dup))))
}
