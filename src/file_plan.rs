//! Phase 1 of the file pipeline: peek the global time window across all
//! input files, and bisect each file to the byte range corresponding to the
//! resolved `--from`/`--to` bounds. Output is suppressed during planning —
//! phase 2 (in `run.rs`) reopens each file and streams its planned range in
//! order.

use humanize_bytes::humanize_bytes_binary;
use std::{
    io::{Seek, SeekFrom},
    path::Path,
    time::Instant,
};

use crate::cli::Cli;
use crate::input::{FileInput, SeekTable};
use crate::run::{is_stdin_path, TimeFilter};
use crate::time_bisect::{self, Side};
use crate::timestamp::Timestamp;

/// Resolved per-file plan produced by phase 1: a byte slice (start, len),
/// the strict time-filter to apply on top of it, and whether this file
/// should be followed after EOF. The file is reopened in phase 2 so phase 1
/// doesn't hold N file descriptors simultaneously.
pub(crate) struct FilePlan<'a> {
    pub(crate) path: &'a Path,
    pub(crate) start_byte: u64,
    /// `end_byte - start_byte`. Phase 2 reads exactly this many bytes.
    pub(crate) max_bytes: u64,
    pub(crate) tf: TimeFilter,
    pub(crate) follow_this_file: bool,
    /// Forward-only input (stdin, streaming-zstd): phase 2 routes through
    /// `stream_unbounded` and `TimeFilter::check` does the time filtering.
    pub(crate) streaming: bool,
    /// Set for seekable-zstd inputs so `-j` workers can build their decoder
    /// without re-parsing the footer. Always `None` when the `zeekstd`
    /// feature is off (the type is uninhabited then).
    pub(crate) seek_table: Option<SeekTable>,
}

/// Compute the union span across all real files (min of per-file first
/// timestamps, max of per-file lasts) used as the anchor for symbolic
/// `--from`/`--to` bounds. One head + one tail seek per file. Stdin (`-`)
/// entries are skipped — pipes can't be peeked at both ends.
pub(crate) fn peek_global_window(
    files: &[std::path::PathBuf],
) -> anyhow::Result<(Option<Timestamp>, Option<Timestamp>)> {
    let mut first: Option<Timestamp> = None;
    let mut last: Option<Timestamp> = None;
    for path in files {
        if is_stdin_path(path) {
            continue;
        }
        let mut input = FileInput::open(path)?;
        // Streaming inputs can't seek to the tail; skip them like stdin.
        if !input.supports_seek() {
            continue;
        }
        if let Some(t) = time_bisect::peek_first_timestamp(&mut input)? {
            first = Some(first.map_or(t, |cur| cur.min(t)));
        }
        if let Some(t) = time_bisect::peek_last_timestamp(&mut input)? {
            last = Some(last.map_or(t, |cur| cur.max(t)));
        }
    }
    Ok((first, last))
}

/// Phase 1: open `path`, bisect to the absolute byte slice corresponding to
/// `tf`, and return the resolved range. The file is dropped on return so
/// phase 1 doesn't pin a file descriptor; phase 2 reopens it. Independent
/// across files (so it parallelizes cleanly).
pub(crate) fn plan_file<'a>(
    path: &'a Path,
    cli: &Cli,
    tf: TimeFilter,
    follow_this_file: bool,
    tail_from_eof: bool,
) -> anyhow::Result<FilePlan<'a>> {
    let streaming_plan = || FilePlan {
        path,
        start_byte: 0,
        max_bytes: 0,
        tf,
        follow_this_file: false,
        streaming: true,
        seek_table: None,
    };
    // Stdin and streaming-zstd both go through `stream_unbounded` in phase 2
    // with the per-line `TimeFilter::check` enforcing the window.
    if is_stdin_path(path) {
        return Ok(streaming_plan());
    }
    let mut input = FileInput::open(path)?;
    if !input.supports_seek() {
        return Ok(streaming_plan());
    }
    let file_len = input.seek(SeekFrom::End(0))?;
    let window = cli.window_nanos();

    let t_bisect = Instant::now();
    let start_byte: u64 = match tf.from {
        Some(t1) => time_bisect::bisect(&mut input, t1, window, Side::Lower)?,
        None if tail_from_eof => file_len, // `tail -F`-style start at EOF
        None => 0,
    };
    let end_byte: u64 = match tf.to {
        Some(t2) if !follow_this_file => time_bisect::bisect(&mut input, t2, window, Side::Upper)?,
        _ => file_len,
    };

    if cli.from.is_some() || cli.to.is_some() {
        log::info!(
            "bisect {}: [{}, {}] window={}s -> bytes [{start_byte}, {end_byte}) ({}) in {:.3}s",
            path.display(),
            cli.from.as_deref().unwrap_or("-"),
            cli.to.as_deref().unwrap_or("-"),
            cli.window_secs,
            humanize_bytes_binary!(end_byte.saturating_sub(start_byte)),
            t_bisect.elapsed().as_secs_f64()
        );
    }

    let seek_table = input.seek_table();

    Ok(FilePlan {
        path,
        start_byte,
        max_bytes: end_byte.saturating_sub(start_byte),
        tf,
        follow_this_file,
        streaming: false,
        seek_table,
    })
}
