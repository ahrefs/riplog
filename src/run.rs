//! Top-level pipeline: open input, optionally bisect a time slice, stream
//! lines through the filter, write matches to the chosen output, optionally
//! follow.

use anyhow::Context as _;
use std::{
    fs::File,
    io::{BufRead, BufReader, BufWriter, IsTerminal, Read, Seek, SeekFrom, Write},
    path::Path,
    time::{Duration, Instant},
};

use crate::bucket::{resolve_bucket_spec, BucketSpec};
use crate::cli::{Cli, ColorMode};
use crate::file_plan::{peek_global_window, plan_file, FilePlan};
use crate::filter::Filter;
use crate::pipeline::Pipeline;
use crate::sampler::build_sampler;
use crate::signal_handling::{install_signal_handler, interrupted};
use crate::sinks::{flush_sort_buf, make_sinks, LineMode, Sinks};
use crate::stats::emit_summaries;
use crate::time_bisect;
use crate::timestamp::{self, Timestamp};
use crate::transform::{parse_line_transform, transform_views, validate_rm_vs_features};

/// `-` in the file list is the Unix idiom for "read from stdin in position".
#[inline]
pub(crate) fn is_stdin_path(p: &Path) -> bool {
    p == Path::new("-")
}

/// Strict timestamp filter applied per-line on top of the bisected byte range.
/// The bisect is an over-approximation, so the byte range can include lines
/// outside `[from, to]`; this filter drops them.
///
/// Lines with no parseable timestamp are dropped when either bound is set
/// (we can't prove they're in range).
#[derive(Default, Clone, Copy)]
pub(crate) struct TimeFilter {
    pub(crate) from: Option<Timestamp>,
    pub(crate) to: Option<Timestamp>,
}

impl TimeFilter {
    pub(crate) fn is_empty(&self) -> bool {
        self.from.is_none() && self.to.is_none()
    }

    /// Test the (already-parsed) timestamp against the bounds. `None` means
    /// the line had no parseable timestamp; with bounds set, that's a drop.
    pub(crate) fn check(&self, ts: Option<Timestamp>) -> bool {
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

/// What to do once CLI parsing and validation are done. Classified by
/// `ExecutionMode::classify` from a `(Cli, …)`-shaped input; the dispatch in
/// `run()` matches on this once and runs the corresponding arm.
enum ExecutionMode<'a> {
    /// `--time-range`: probe per-file head+tail, print the union span, exit.
    /// No filter pipeline, no sinks.
    TimeRange,
    /// No file arguments: stream stdin to the chosen output. `bucket` carries
    /// the epoch-aligned `--bucket=DURATION` config, or `None` when not set.
    StdinOnly { bucket: Option<BucketSpec> },
    /// One or more file arguments (possibly including `-` as stdin). Phase 1
    /// has already produced one `FilePlan` per file; phase 2 streams each in
    /// order. `bucket` here may be `--bucket=DURATION` *or* `--n-buckets=N`
    /// resolved against the global window.
    Files {
        plans: Vec<FilePlan<'a>>,
        bucket: Option<BucketSpec>,
        /// True when `-` appears in `cli.files` (at most once).
        has_stdin: bool,
    },
}

impl<'a> ExecutionMode<'a> {
    /// Tag for `log::debug!` so a `RUST_LOG=debug` run shows the chosen path
    /// at a glance without dragging in `Debug` impls for `FilePlan`/etc.
    fn tag(&self) -> &'static str {
        match self {
            ExecutionMode::TimeRange => "time-range",
            ExecutionMode::StdinOnly { bucket: None } => "stdin",
            ExecutionMode::StdinOnly { bucket: Some(_) } => "stdin+bucket",
            ExecutionMode::Files { .. } => "files",
        }
    }
}

/// Classify the run into one of the `ExecutionMode` arms. All CLI validations
/// that don't depend on output state happen here (and in the same order they
/// did pre-refactor), so the error messages and short-circuit behaviour are
/// preserved.
///
/// For `Files`, this also performs phase 1 (`peek_global_window` + `plan_file`
/// over every input) so the dispatch arm in `run()` is purely phase 2.
fn classify<'a>(cli: &'a Cli, following: bool) -> anyhow::Result<ExecutionMode<'a>> {
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
        return Ok(ExecutionMode::StdinOnly { bucket });
    }

    let n_stdin = cli.files.iter().filter(|p| is_stdin_path(p)).count();
    if n_stdin > 1 {
        anyhow::bail!("`-` (stdin) cannot appear more than once in the file list");
    }
    let has_stdin = n_stdin == 1;
    if has_stdin && (following || cli.time_range) {
        anyhow::bail!("`-` (stdin) cannot be combined with `-f`, `-F`, or `--time-range`");
    }

    if cli.time_range {
        return Ok(ExecutionMode::TimeRange);
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

    // Phase 1: bisect every file up front against the resolved absolute
    // window. Output is suppressed during planning — only summaries and
    // matched lines are written, in file order, in phase 2.
    let last_idx = cli.files.len() - 1;
    // `tail -F`-style start-at-EOF only applies to the classic single-file
    // case. With multiple files (e.g. `foo.log.1 foo.log -F`), the last
    // file is read fully — completing the rotated → current → tail story.
    let single_file = cli.files.len() == 1;
    let plans: Vec<FilePlan<'a>> = cli
        .files
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let last = i == last_idx;
            plan_file(
                path,
                cli,
                tf,
                following && last,
                following && last && single_file,
            )
        })
        .collect::<anyhow::Result<_>>()?;

    Ok(ExecutionMode::Files {
        plans,
        bucket,
        has_stdin,
    })
}

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
    // Borrow once into `&str` slices for the per-line emit hot path; built
    // here so `process_line`/`emit_match` don't re-walk `SmartString`s per
    // matched line. Lifetime is tied to `line_transform`, which outlives all
    // uses below.
    let (add_view, remove_view) = transform_views(line_transform.as_ref());
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

    if cli.json && matches!(cli.color, ColorMode::Always) {
        anyhow::bail!("`--json` cannot be combined with `--color=always`");
    }
    let colorize = if cli.json {
        false
    } else {
        match cli.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => cli.output.is_none() && std::io::stdout().is_terminal(),
        }
    };

    // Pick the line emitter once. Order matters: passthrough is the memcpy
    // fast path, only eligible when no transform/filter/color/json applies.
    let line_mode = if cli.json {
        LineMode::Json
    } else if colorize {
        LineMode::Colored
    } else if line_transform.is_none() && filter.is_empty() {
        LineMode::Passthrough
    } else {
        LineMode::Plain
    };

    // `Send` so the parallel path can hand `&mut output` to its workers
    // through a shared `Mutex`. Using the unlocked `Stdout` (rather than
    // `stdout().lock()`) makes this cross-thread-safe; the per-call lock
    // inside `Stdout::write` is amortised by `BufWriter` batching.
    let mut output: Box<dyn Write + Send> = match &cli.output {
        Some(path) => Box::new(BufWriter::new(File::create(path)?)),
        None => Box::new(BufWriter::new(std::io::stdout())),
    };

    let mode = classify(cli, following)?;
    log::debug!("execution mode: {}", mode.tag());

    match mode {
        ExecutionMode::TimeRange => {
            // Min of per-file firsts, max of per-file lasts — the union span.
            let mut overall_first: Option<Timestamp> = None;
            let mut overall_last: Option<Timestamp> = None;
            for path in &cli.files {
                let mut file = File::open(path)?;
                let t0 = Instant::now();
                let (first, last) = time_bisect::time_range(&mut file)?;
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
        }

        ExecutionMode::StdinOnly { bucket } => {
            // stdin can't follow (rejected earlier), but `--bucket` still
            // enables streaming output: the per-line `flush_closed` hook in
            // `process_line` emits closed buckets in time order as we go,
            // without any seek (pipes can't seek). At EOF, `emit_summaries`
            // calls `flush_remaining` for the still-open buckets.
            let mut sinks = make_sinks(
                cli,
                sampler.clone(),
                suppress_lines,
                line_mode,
                bucket,
                tz.clone(),
            );
            if bucket.is_some() {
                sinks.enable_streaming(cli.window_nanos());
            }
            let tf_default = TimeFilter::default();
            let mut pipeline =
                Pipeline::new(&filter, &tf_default, &mut sinks, &add_view, &remove_view);
            stream_unbounded(&mut std::io::stdin().lock(), &mut pipeline, &mut output)?;
            output.flush()?;
            flush_sort_buf(&mut sinks, &mut output)?;
            emit_summaries(&sinks, bare_count, &tz, &mut output)?;
        }

        ExecutionMode::Files {
            plans,
            bucket,
            has_stdin,
        } => {
            let mut sinks = make_sinks(
                cli,
                sampler.clone(),
                suppress_lines,
                line_mode,
                bucket,
                tz.clone(),
            );
            // Master streams under follow or when stdin (`-`) is in the file list;
            // workers always batch (their output would interleave on the shared
            // writer otherwise) and merge into the master.
            if cli.bucket.is_some() && (following || has_stdin) {
                sinks.enable_streaming(cli.window_nanos());
            }

            // Phase 2: stream each planned range in order. Only the last file
            // may attach the follow loop (set during planning).
            let n_workers = resolve_parallelism(cli);
            for plan in plans {
                if interrupted() || sinks.done() {
                    break;
                }
                if n_workers > 1 && !plan.follow_this_file && !is_stdin_path(plan.path) {
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
                        line_mode,
                        bucket,
                        tz: tz.clone(),
                        output: &mut *output,
                        master: &mut sinks,
                        line_transform: line_transform.clone(),
                    })?;
                    output.flush()?;
                } else {
                    stream_plan(
                        plan,
                        cli,
                        &filter,
                        &mut output,
                        &mut sinks,
                        &add_view,
                        &remove_view,
                    )?;
                }
            }

            output.flush()?;
            flush_sort_buf(&mut sinks, &mut output)?;
            emit_summaries(&sinks, bare_count, &tz, &mut output)?;
        }
    }

    Ok(())
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
    add_pairs: &[(&str, &str)],
    remove_keys: &[&str],
) -> anyhow::Result<()> {
    let FilePlan {
        path,
        start_byte,
        max_bytes,
        tf,
        follow_this_file,
    } = plan;

    if is_stdin_path(path) {
        // No bisect, no follow loop. Per-line `tf` still applies (resolved
        // against the real files' span by the caller).
        let mut pipeline = Pipeline::new(filter, &tf, sinks, add_pairs, remove_keys);
        return stream_unbounded(&mut std::io::stdin().lock(), &mut pipeline, output);
    }

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

    let mut pipeline = Pipeline::new(filter, &tf, sinks, add_pairs, remove_keys);
    stream_bounded(&mut reader, max_bytes, &mut pipeline, output)?;
    output.flush()?;

    if let Some(handle) = file_for_reopen {
        // Re-borrow check: the original `sinks.done()` call after stream_bounded
        // is gated through the pipeline's borrow. Drop the pipeline so we can
        // re-check `sinks.done()` and build a fresh one for the follow loop.
        drop(pipeline);
        if !interrupted() && !sinks.done() {
            // Follow mode ignores the per-file `tf` (newly arrived lines have
            // no resolved time bound), matching the previous behavior where
            // `follow_loop` passed `TimeFilter::default()`.
            let tf_follow = TimeFilter::default();
            let mut pipeline = Pipeline::new(filter, &tf_follow, sinks, add_pairs, remove_keys);
            follow_loop(
                path,
                handle,
                reader,
                cli.follow_reopen,
                &mut pipeline,
                output,
            )?;
        }
    }

    Ok(())
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

/// Read a fixed byte budget from `reader`, write matching lines to `output`.
pub(crate) fn stream_bounded<R: BufRead, W: Write + ?Sized>(
    reader: &mut R,
    max_bytes: u64,
    pipeline: &mut Pipeline<'_>,
    output: &mut W,
) -> anyhow::Result<()> {
    let mut total_read: u64 = 0;

    while total_read < max_bytes && !interrupted() {
        let n = reader.read_until(b'\n', &mut pipeline.line_buf)?;
        if n == 0 {
            break;
        }
        total_read += n as u64;
        pipeline.process_line(n, output)?;
        if pipeline.sinks.done() {
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
    pipeline: &mut Pipeline<'_>,
    output: &mut W,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(reader);

    while !interrupted() {
        let n = reader.read_until(b'\n', &mut pipeline.line_buf)?;
        if n == 0 {
            break;
        }
        pipeline.process_line(n, output)?;
        output.flush()?;
        if pipeline.sinks.done() {
            break;
        }
    }
    Ok(())
}

fn follow_loop<W: Write>(
    path: &Path,
    mut handle: File,
    mut reader: BufReader<File>,
    reopen: bool,
    pipeline: &mut Pipeline<'_>,
    output: &mut W,
) -> anyhow::Result<()> {
    let mut pos = reader.stream_position()?;

    while !interrupted() {
        let n = reader.read_until(b'\n', &mut pipeline.line_buf)?;
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
                    pipeline.line_buf.clear();
                    continue;
                }
            }
            std::thread::sleep(FOLLOW_POLL);
            continue;
        }
        if !pipeline.line_buf.ends_with(b"\n") {
            // Partial line — wait for the rest.
            std::thread::sleep(FOLLOW_POLL);
            continue;
        }
        pos += n as u64;
        pipeline.process_line(n, output)?;
        if pipeline.sinks.done() {
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
