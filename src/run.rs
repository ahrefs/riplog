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

use crate::bucket::{self, BucketSpec, ResolvedBucket};
use crate::cli::{Cli, ColorMode};
use crate::file_plan::{peek_global_window, plan_file, FilePlan};
use crate::filter::Filter;
use crate::json::JsonlFormat;
use crate::logfmt::LogfmtFormat;
use crate::output::Formatter;
use crate::pipeline::Pipeline;
use crate::sampler::build_sampler;
use crate::signal_handling::{install_signal_handler, interrupted};
use crate::sinks::{LineEmitter, Recorders, RunConfig};
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
    StdinOnly { bucket: Option<ResolvedBucket> },
    /// One or more file arguments (possibly including `-` as stdin). Phase 1
    /// has already produced one `FilePlan` per file; phase 2 streams each in
    /// order. `bucket` here may be `--bucket=DURATION` *or* `--n-buckets=N`
    /// resolved against the global window.
    Files {
        plans: Vec<FilePlan<'a>>,
        bucket: Option<ResolvedBucket>,
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
        // Epoch-aligned grid for `--bucket=DURATION` on stdin. Stdin can't
        // use `--n-buckets` (rejected just above), so inline-build the
        // resolved form rather than going through `BucketSpec::from_cli`.
        let bucket = cli
            .bucket
            .as_deref()
            .map(timestamp::parse_duration_nanos)
            .transpose()?
            .map(|nanos| ResolvedBucket {
                start_nanos: 0,
                dur_nanos: nanos,
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
    let bucket = match BucketSpec::from_cli(cli)? {
        Some(spec) => Some(bucket::resolve(spec, &tf, global_first, global_last)?),
        None => None,
    };

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

    // Pick the format engine and the memcpy fast-path flag independently.
    // When `passthrough` is true, the emit path writes the input bytes
    // verbatim and `formatter` is only used for end-of-run summaries
    // (agg rows, list-keys, list-values-for, bare count).
    //
    // `Formatter` is a plain enum (no heap allocation); workers and master
    // share the same `&formatter` borrow. Static dispatch via `match` on
    // the enum lets the compiler inline `LogfmtFormat::line`'s per-pair
    // `write_all` loop directly into the caller's writer type, because this
    // is in the hot path.
    let formatter = if cli.json {
        Formatter::Json(JsonlFormat)
    } else {
        Formatter::Logfmt(LogfmtFormat { color: colorize })
    };
    let passthrough = !cli.json && !colorize && line_transform.is_none() && filter.is_empty();

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
            let cfg = RunConfig {
                formatter: &formatter,
                passthrough,
                suppress_lines,
                limit: cli.limit,
                tz: tz.clone(),
                sampler: sampler.clone(),
                add_pairs: &add_view,
                remove_keys: &remove_view,
            };
            // stdin always streams when bucketed (no seek; mid-run flush is
            // the only way to emit closed buckets in order).
            let streaming_grace = bucket.is_some().then(|| cli.window_nanos());
            let mut recorders = Recorders::new(cli, bucket, streaming_grace);
            let mut emitter = LineEmitter::new(cli.raw_key.as_deref(), cli.sort_by.as_deref());
            let tf_default = TimeFilter::default();
            let mut pipeline =
                Pipeline::new(&filter, &tf_default, &cfg, &mut recorders, &mut emitter);
            stream_unbounded(&mut std::io::stdin().lock(), &mut pipeline, &mut output)?;
            output.flush()?;
            emitter.flush_sort_buf(&mut output)?;
            emit_summaries(&recorders, &cfg, bare_count, &mut output)?;
        }

        ExecutionMode::Files {
            plans,
            bucket,
            has_stdin,
        } => {
            let cfg = RunConfig {
                formatter: &formatter,
                passthrough,
                suppress_lines,
                limit: cli.limit,
                tz: tz.clone(),
                sampler: sampler.clone(),
                add_pairs: &add_view,
                remove_keys: &remove_view,
            };
            // Master streams under follow or when stdin (`-`) is in the file list;
            // workers always batch (their output would interleave on the shared
            // writer otherwise) and merge into the master.
            let master_streaming_grace =
                (cli.bucket.is_some() && (following || has_stdin)).then(|| cli.window_nanos());
            let mut recorders = Recorders::new(cli, bucket, master_streaming_grace);
            let mut emitter = LineEmitter::new(cli.raw_key.as_deref(), cli.sort_by.as_deref());

            // Phase 2: stream each planned range in order. Only the last file
            // may attach the follow loop (set during planning).
            let n_workers = resolve_parallelism(cli);
            for plan in plans {
                if interrupted() || pipeline_done(&cfg, &recorders) {
                    break;
                }
                if n_workers > 1 && !plan.follow_this_file && !is_stdin_path(plan.path) {
                    let plan_tf = plan.tf;
                    let filter_ref = &filter;
                    let line_transform_ref = &line_transform;
                    let cfg_ref = &cfg;
                    let worker_results = crate::parallel::run(
                        crate::parallel::Job {
                            path: plan.path,
                            start_byte: plan.start_byte,
                            max_bytes: plan.max_bytes,
                            n_workers,
                        },
                        &mut *output,
                        |mut reader, byte_budget, sink| {
                            // Workers always batch; the master is the only
                            // counter that may stream.
                            let mut worker_recorders = Recorders::new(cli, bucket, None);
                            let mut worker_emitter =
                                LineEmitter::new(cli.raw_key.as_deref(), cli.sort_by.as_deref());
                            let worker_tf = line_transform_ref.clone();
                            let (add_view, remove_view) = transform_views(worker_tf.as_ref());
                            // Workers borrow shared `cfg` for the immutable parts
                            // (formatter, limit, tz, sampler) but get their own
                            // add_pairs/remove_keys views into their own clone of
                            // the line transform.
                            let worker_cfg = RunConfig {
                                formatter: cfg_ref.formatter,
                                passthrough: cfg_ref.passthrough,
                                suppress_lines: cfg_ref.suppress_lines,
                                limit: cfg_ref.limit,
                                tz: cfg_ref.tz.clone(),
                                sampler: cfg_ref.sampler.clone(),
                                add_pairs: &add_view,
                                remove_keys: &remove_view,
                            };
                            {
                                let mut pipeline = Pipeline::new(
                                    filter_ref,
                                    &plan_tf,
                                    &worker_cfg,
                                    &mut worker_recorders,
                                    &mut worker_emitter,
                                );
                                stream_bounded(&mut reader, byte_budget, &mut pipeline, sink)?;
                            }
                            Ok((worker_recorders, worker_emitter))
                        },
                    )?;
                    for (r, e) in worker_results {
                        recorders.merge(r);
                        emitter.merge_sort_buf(e.sort_buf);
                    }
                    output.flush()?;
                } else {
                    stream_plan(
                        plan,
                        cli,
                        &filter,
                        &mut output,
                        &cfg,
                        &mut recorders,
                        &mut emitter,
                    )?;
                }
            }

            output.flush()?;
            emitter.flush_sort_buf(&mut output)?;
            emit_summaries(&recorders, &cfg, bare_count, &mut output)?;
        }
    }

    Ok(())
}

/// `--limit` check without needing to construct a `Pipeline`. Used at the
/// top of the per-file loop in the Files arm where the pipeline doesn't
/// exist yet.
#[inline]
fn pipeline_done(cfg: &RunConfig<'_>, recorders: &Recorders) -> bool {
    matches!(cfg.limit, Some(n) if recorders.stats.matched_lines >= n)
}

/// Phase 2: open `plan.path`, seek to the planned start, stream the bounded
/// byte range through filters and emitter, then optionally attach the follow
/// loop.
fn stream_plan<W: Write>(
    plan: FilePlan<'_>,
    cli: &Cli,
    filter: &Filter,
    output: &mut W,
    cfg: &RunConfig<'_>,
    recorders: &mut Recorders,
    emitter: &mut LineEmitter,
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
        let mut pipeline = Pipeline::new(filter, &tf, cfg, recorders, emitter);
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
    let mut reader = BufReader::with_capacity(STREAM_BUF_CAP, file);

    {
        let mut pipeline = Pipeline::new(filter, &tf, cfg, &mut *recorders, &mut *emitter);
        stream_bounded(&mut reader, max_bytes, &mut pipeline, output)?;
    }
    output.flush()?;

    if let Some(handle) = file_for_reopen {
        if !interrupted() && !pipeline_done(cfg, recorders) {
            // Follow mode ignores the per-file `tf` (newly arrived lines have
            // no resolved time bound), matching the previous behavior where
            // `follow_loop` passed `TimeFilter::default()`.
            let tf_follow = TimeFilter::default();
            let mut pipeline = Pipeline::new(filter, &tf_follow, cfg, recorders, emitter);
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

/// BufReader capacity for streaming reads. 128 KB is the sweet spot from a
/// sweep on `foo.log` (2.8 GB): 8 KB is clearly bad, 32 KB recovers most of
/// it, 128 KB wins marginally on aggregate/parallel modes, sizes above are
/// noise. Big enough to fit pathologically long log lines without spilling
/// into the carryover `tail`.
pub(crate) const STREAM_BUF_CAP: usize = 128 * 1024;

/// Outcome of scanning one filled buffer.
enum ChunkOutcome {
    /// Full chunk processed; caller should keep going.
    Full,
    /// `--limit` reached mid-chunk: `usize` is the byte count to `consume()`.
    /// Caller has already had `flush_closed_chunk` invoked and should break.
    HitLimit,
}

/// Owns the per-stream carry-over buffer (`tail`) for a partial line that
/// straddles two `fill_buf` chunks, plus the cumulative byte count used by
/// `stream_bounded` to honour its byte budget. Shared by all three streaming
/// loops (bounded byte range, unbounded stdin/pipe, follow mode); the
/// loop-specific behaviours (byte cap, per-chunk `output.flush`, empty-chunk
/// action) live in the callers.
struct LineScanner {
    tail: Vec<u8>,
    total_read: u64,
}

impl LineScanner {
    fn new() -> Self {
        Self {
            tail: Vec::new(),
            total_read: 0,
        }
    }

    /// Scan one `fill_buf` chunk: emit one line per `\n` (carrying over the
    /// `tail` partial-line buffer), check `sinks.done()` after each line for
    /// mid-chunk `--limit` exit, then `reader.consume()` the bytes processed
    /// and run `flush_closed_chunk` for the per-chunk streaming-bucket flush.
    ///
    /// `usable` is the caller-trimmed view of the chunk (bounded readers
    /// shrink it to fit the remaining byte budget; unbounded uses the full
    /// chunk). The returned `ChunkOutcome` tells the caller whether to keep
    /// looping or break for `--limit`.
    fn scan_chunk<R: BufRead, W: Write + ?Sized>(
        &mut self,
        reader: &mut R,
        usable_len: usize,
        pipeline: &mut Pipeline<'_>,
        output: &mut W,
    ) -> anyhow::Result<ChunkOutcome> {
        // Re-borrow the chunk here: callers passed us the trimmed length, not
        // the slice, because the buffer is borrowed from `reader` and we need
        // a fresh borrow scope to also call `reader.consume`.
        let chunk = reader.fill_buf()?;
        let usable = &chunk[..usable_len];

        let mut start = 0;
        let mut hit_limit_at: Option<usize> = None;
        for nl in memchr::memchr_iter(b'\n', usable) {
            if self.tail.is_empty() {
                pipeline.process_line(&usable[start..=nl], output)?;
            } else {
                // add the beginning of the line, saved from previous call
                self.tail.extend_from_slice(&usable[start..=nl]);
                pipeline.process_line(&self.tail, output)?;
                self.tail.clear();
            }
            start = nl + 1;
            if pipeline.done() {
                hit_limit_at = Some(nl + 1);
                break;
            }
        }
        if let Some(consumed) = hit_limit_at {
            reader.consume(consumed);
            // Final per-chunk flush for buckets that may have just closed.
            pipeline.flush_closed_chunk(output)?;
            return Ok(ChunkOutcome::HitLimit);
        }
        if start < usable_len {
            self.tail.extend_from_slice(&usable[start..usable_len]);
        }
        reader.consume(usable_len);
        self.total_read += usable_len as u64;
        // Per-chunk streaming-bucket flush: no-op when streaming is off
        // (the common case), real work only when `-f`/`-F` + `--bucket`.
        pipeline.flush_closed_chunk(output)?;
        Ok(ChunkOutcome::Full)
    }

    /// Process the trailing partial line at EOF (no terminating `\n`). Used
    /// by the non-follow loops; `follow_loop` deliberately leaves a non-empty
    /// `tail` in place between polls so more bytes can complete the line.
    /// Returns `true` if it actually emitted the partial line (i.e. tail was
    /// non-empty AND `sinks.done()` was false), so the unbounded caller knows
    /// whether to chase it with an `output.flush()`.
    fn finish_eof<W: Write + ?Sized>(
        &mut self,
        pipeline: &mut Pipeline<'_>,
        output: &mut W,
    ) -> anyhow::Result<bool> {
        if !self.tail.is_empty() && !pipeline.done() {
            pipeline.process_line(&self.tail, output)?;
            pipeline.flush_closed_chunk(output)?;
            return Ok(true);
        }
        Ok(false)
    }
}

/// Read a fixed byte budget from `reader`, write matching lines to `output`.
///
/// Uses `fill_buf` + `memchr::memchr_iter` so newline scanning is one SIMD
/// pass per chunk and per-line bookkeeping (interrupt + `sinks.done`) only
/// runs at chunk granularity, except for the `--limit` `done` check which
/// still fires per matched line to stop mid-chunk.
pub(crate) fn stream_bounded<R: BufRead, W: Write + ?Sized>(
    reader: &mut R,
    max_bytes: u64,
    pipeline: &mut Pipeline<'_>,
    output: &mut W,
) -> anyhow::Result<()> {
    let mut scan = LineScanner::new();

    while scan.total_read < max_bytes && !interrupted() {
        let chunk_len = {
            let chunk = reader.fill_buf()?;
            chunk.len()
        };
        if chunk_len == 0 {
            break;
        }
        let remaining = (max_bytes - scan.total_read) as usize;
        let usable_len = chunk_len.min(remaining);
        if matches!(
            scan.scan_chunk(reader, usable_len, pipeline, output)?,
            ChunkOutcome::HitLimit
        ) {
            break;
        }
    }
    let _ = scan.finish_eof(pipeline, output)?;
    Ok(())
}

/// Read until EOF (e.g. stdin), write matching lines to `output`.
fn stream_unbounded<R: Read, W: Write>(
    reader: &mut R,
    pipeline: &mut Pipeline<'_>,
    output: &mut W,
) -> anyhow::Result<()> {
    let mut reader = BufReader::with_capacity(STREAM_BUF_CAP, reader);
    let mut scan = LineScanner::new();

    // Per-chunk `output.flush()` instead of per-line: interactive
    // `tail -f | riplog` accepts batch-latency (bounded by chunk size
    // ~128 KB) for higher throughput. If a user reports lag on interactive
    // pipes, gate this on `stdout().is_terminal()` and revert to per-line
    // flush in that case.
    //
    // NOTE: `stream_bounded` does NOT do a per-chunk `output.flush()` (it
    // flushes once at the end of `stream_plan`). That asymmetry is
    // deliberate-for-now — the bounded path is the bulk-throughput one and a
    // single trailing flush is cheaper. If it ever becomes a bug, surface it
    // here rather than fixing it silently.
    while !interrupted() {
        let chunk_len = {
            let chunk = reader.fill_buf()?;
            chunk.len()
        };
        if chunk_len == 0 {
            break;
        }
        let outcome = scan.scan_chunk(&mut reader, chunk_len, pipeline, output)?;
        output.flush()?;
        if matches!(outcome, ChunkOutcome::HitLimit) {
            break;
        }
    }
    if scan.finish_eof(pipeline, output)? {
        // Mirror the original unbounded behaviour of flushing `output` after
        // emitting a trailing partial line; `finish_eof` only flushes the
        // `Counter`.
        output.flush()?;
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
    let mut scan = LineScanner::new();

    while !interrupted() {
        let chunk_len = {
            let chunk = reader.fill_buf()?;
            chunk.len()
        };
        if chunk_len == 0 {
            // No bytes available right now. If we have a buffered partial line,
            // just wait — don't process it yet (more bytes may complete it).
            output.flush()?;
            if scan.tail.is_empty() && reopen {
                if let Some((new_handle, new_reader)) = check_rotation(path, &handle, pos)? {
                    log::info!(
                        "follow: file rotated/truncated; reopening {}",
                        path.display()
                    );
                    handle = new_handle;
                    reader = new_reader;
                    pos = 0;
                    continue;
                }
            }
            std::thread::sleep(FOLLOW_POLL);
            continue;
        }
        let outcome = scan.scan_chunk(&mut reader, chunk_len, pipeline, output)?;
        // `scan_chunk` advances `total_read` by `usable_len`; mirror that
        // into the follow-specific `pos` used by `check_rotation`.
        pos += chunk_len as u64;
        if matches!(outcome, ChunkOutcome::HitLimit) {
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
