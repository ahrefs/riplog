//! Single-file parallel search. Splits a byte range across N worker threads
//! via `std::thread::scope`; each worker runs the same `stream_bounded`
//! core as sequential, into a thread-local `Sinks`. Output is **unordered**:
//! workers append to a per-thread buffer that flushes to the shared writer
//! through a `Mutex`. After join the master folds each worker's `Sinks`
//! into its own.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

use crate::bucket::BucketSpec;
use crate::cli::Cli;
use crate::filter::Filter;
use crate::run::{stream_bounded, TimeFilter};
use crate::sampler::Sampler;
use crate::sinks::{make_sinks, LineMode, Sinks};
use crate::transform::{transform_views, LineTransform};

/// Below this many bytes per worker, parallel mode falls back to sequential —
/// the fixed per-thread overhead would dominate the per-byte work.
pub(crate) const MIN_BYTES_PER_WORKER: u64 = 4 * 1024 * 1024;

/// Per-worker batch size before a flush to the shared writer.
const FLUSH_BYTES: usize = 64 * 1024;

/// Inputs for one parallel search invocation. All references share a single
/// lifetime since the call site (the file-plan loop in `run::run`) borrows
/// each from the same scope.
pub(crate) struct Job<'a> {
    pub path: &'a Path,
    pub start_byte: u64,
    pub max_bytes: u64,
    pub tf: TimeFilter,
    pub n_workers: usize,
    pub cli: &'a Cli,
    pub filter: &'a Filter,
    pub sampler: Option<Sampler>,
    pub suppress_lines: bool,
    pub line_mode: LineMode,
    pub bucket: Option<BucketSpec>,
    pub tz: jiff::tz::TimeZone,
    pub output: &'a mut (dyn Write + Send),
    pub master: &'a mut Sinks,
    pub line_transform: Option<LineTransform>,
}

/// Spawn `job.n_workers` threads searching disjoint chunks of `job.path`
/// over `[start_byte, start_byte + max_bytes)`. Falls back to a single
/// sequential pass on `job.master` if the range is too small to split.
pub(crate) fn run(job: Job<'_>) -> anyhow::Result<()> {
    let Job {
        path,
        start_byte,
        max_bytes,
        tf,
        n_workers,
        cli,
        filter,
        sampler,
        suppress_lines,
        line_mode,
        bucket,
        tz,
        output,
        master,
        line_transform,
    } = job;

    let end_byte = start_byte.saturating_add(max_bytes);
    let chunks = {
        let mut probe = File::open(path)?;
        compute_chunks(&mut probe, start_byte, end_byte, n_workers)?
    };

    if chunks.len() <= 1 {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(start_byte))?;
        let mut reader = BufReader::new(file);
        let (add_view, remove_view) = transform_views(line_transform.as_ref());
        return stream_bounded(
            &mut reader,
            max_bytes,
            filter,
            &tf,
            output,
            master,
            &add_view,
            &remove_view,
        );
    }

    log::info!(
        "parallel: {} workers over {} chunks, [{}..{}) ({} bytes)",
        n_workers,
        chunks.len(),
        start_byte,
        end_byte,
        end_byte - start_byte
    );

    let shared_out: Mutex<&mut (dyn Write + Send)> = Mutex::new(output);
    let worker_sinks: Vec<Sinks> = std::thread::scope(|s| -> anyhow::Result<Vec<Sinks>> {
        let handles: Vec<_> = chunks
            .iter()
            .map(|&(cs, ce)| {
                let sampler = sampler.clone();
                let shared = &shared_out;
                let tz = tz.clone();
                let worker_tf = line_transform.clone();
                s.spawn(move || -> anyhow::Result<Sinks> {
                    // Workers don't call `enable_streaming`: rows must batch
                    // into per-worker `Sinks` and merge into the master, or
                    // multi-writer output interleaves on the shared sink.
                    let mut sinks = make_sinks(cli, sampler, suppress_lines, line_mode, bucket, tz);
                    let (add_view, remove_view) = transform_views(worker_tf.as_ref());
                    let mut file = File::open(path)?;
                    file.seek(SeekFrom::Start(cs))?;
                    let mut reader = BufReader::new(file);
                    let mut sink = UnorderedSink::new(shared);
                    stream_bounded(
                        &mut reader,
                        ce - cs,
                        filter,
                        &tf,
                        &mut sink,
                        &mut sinks,
                        &add_view,
                        &remove_view,
                    )?;
                    sink.flush()?;
                    Ok(sinks)
                })
            })
            .collect();
        let mut all = Vec::with_capacity(handles.len());
        for h in handles {
            all.push(h.join().expect("worker panicked")?);
        }
        Ok(all)
    })?;

    for s in worker_sinks {
        master.merge(s);
    }
    Ok(())
}

/// Compute up to `n` non-overlapping byte ranges over `[start, end)` whose
/// boundaries lie at line starts (or at `start` / `end`). Each interior
/// boundary is forward-snapped to the byte after the next `\n`, so every
/// chunk except the first begins at a line boundary. Returns a single
/// `(start, end)` chunk if the span is too small to be worth splitting.
pub(crate) fn compute_chunks(
    file: &mut File,
    start: u64,
    end: u64,
    n: usize,
) -> std::io::Result<Vec<(u64, u64)>> {
    if n <= 1 || end <= start {
        return Ok(vec![(start, end)]);
    }
    let span = end - start;
    if span < MIN_BYTES_PER_WORKER.saturating_mul(n as u64) {
        return Ok(vec![(start, end)]);
    }
    let chunk = span / n as u64;
    let mut bounds = Vec::with_capacity(n + 1);
    bounds.push(start);
    for i in 1..n {
        let approx = start + chunk * i as u64;
        bounds.push(snap_forward_to_newline(file, approx, end)?);
    }
    bounds.push(end);
    Ok(bounds
        .windows(2)
        .filter_map(|w| (w[0] < w[1]).then_some((w[0], w[1])))
        .collect())
}

/// Read forward from `from` until just past the next `\n`, capped at `cap`.
/// Returns the new absolute position. Restores the file's seek position to
/// its value on entry.
fn snap_forward_to_newline(file: &mut File, from: u64, cap: u64) -> std::io::Result<u64> {
    let saved = file.stream_position()?;
    file.seek(SeekFrom::Start(from))?;
    let mut buf = [0u8; 8192];
    let mut pos = from;
    let result = loop {
        if pos >= cap {
            break cap;
        }
        let to_read = std::cmp::min(buf.len() as u64, cap - pos) as usize;
        let n = file.read(&mut buf[..to_read])?;
        if n == 0 {
            break pos;
        }
        if let Some(idx) = memchr::memchr(b'\n', &buf[..n]) {
            break pos + idx as u64 + 1;
        }
        pos += n as u64;
    };
    file.seek(SeekFrom::Start(saved))?;
    Ok(result)
}

/// Per-worker writer: appends to a thread-local `Vec<u8>`, flushes to the
/// shared writer behind a `Mutex` once the local buffer crosses
/// `FLUSH_BYTES`. Output across workers is unordered, but a single flush is
/// contiguous so individual lines are never split mid-byte.
struct UnorderedSink<'a> {
    local: Vec<u8>,
    shared: &'a Mutex<&'a mut (dyn Write + Send)>,
}

impl<'a> UnorderedSink<'a> {
    fn new(shared: &'a Mutex<&'a mut (dyn Write + Send)>) -> Self {
        Self {
            local: Vec::with_capacity(FLUSH_BYTES),
            shared,
        }
    }

    fn flush_to_shared(&mut self) -> std::io::Result<()> {
        if self.local.is_empty() {
            return Ok(());
        }
        let mut g = self.shared.lock().unwrap();
        g.write_all(&self.local)?;
        self.local.clear();
        Ok(())
    }
}

impl Write for UnorderedSink<'_> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.local.extend_from_slice(data);
        if self.local.len() >= FLUSH_BYTES {
            self.flush_to_shared()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_to_shared()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(name: &str, data: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("riplog-parallel-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(data).unwrap();
        p
    }

    #[test]
    fn compute_chunks_falls_back_for_small_inputs() {
        let p = write_tmp("small.log", b"a\nb\nc\n");
        let mut f = File::open(&p).unwrap();
        let chunks = compute_chunks(&mut f, 0, 6, 4).unwrap();
        assert_eq!(chunks, vec![(0, 6)]);
    }

    #[test]
    fn compute_chunks_snaps_to_line_boundaries() {
        // Build > MIN_BYTES_PER_WORKER * n bytes so the split actually fires.
        let mut data = Vec::new();
        for i in 0..900_000 {
            data.extend_from_slice(format!("line {i:08} hello world\n").as_bytes());
        }
        let p = write_tmp("big.log", &data);
        let mut f = File::open(&p).unwrap();
        let end = data.len() as u64;
        let chunks = compute_chunks(&mut f, 0, end, 4).unwrap();
        assert!(chunks.len() >= 2 && chunks.len() <= 4);
        // Every chunk except the first must start right after a `\n`.
        for &(cs, _) in chunks.iter().skip(1) {
            assert_eq!(data[cs as usize - 1], b'\n', "chunk start {cs} not aligned");
        }
        // Chunks cover exactly [0, end).
        assert_eq!(chunks.first().unwrap().0, 0);
        assert_eq!(chunks.last().unwrap().1, end);
        for w in chunks.windows(2) {
            assert_eq!(w[0].1, w[1].0);
        }
    }
}
