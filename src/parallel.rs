//! Single-file parallel search. Splits a byte range across N worker threads
//! via `std::thread::scope`; each worker invokes the caller-supplied `work`
//! closure against a positioned reader, a byte budget, and a per-worker
//! writer. Output is **unordered**: workers append to a per-thread buffer
//! that flushes to the shared writer through a `Mutex`. The caller folds
//! the per-worker states returned from `run` into its own master state.

use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Mutex;

use crate::input::{FileInput, SeekTable};

/// Below this many bytes per worker, parallel mode falls back to sequential —
/// the fixed per-thread overhead would dominate the per-byte work.
pub(crate) const MIN_BYTES_PER_WORKER: u64 = 4 * 1024 * 1024;

/// Per-worker batch size before a flush to the shared writer.
const FLUSH_BYTES: usize = 64 * 1024;

/// Where to split the work. `path` is the only borrow; everything else is
/// plain data.
pub(crate) struct Job<'a> {
    pub path: &'a Path,
    pub start_byte: u64,
    pub max_bytes: u64,
    pub n_workers: usize,
    /// Reuse a parsed seek table across workers so each thread skips the
    /// per-open footer round-trip on seekable-zstd inputs. `None` for plain
    /// files (and always when the feature is off). Workers borrow it across
    /// `thread::scope`; the master keeps ownership.
    pub seek_table: Option<SeekTable>,
}

/// Spawn `job.n_workers` threads searching disjoint chunks of `job.path`
/// over `[start_byte, start_byte + max_bytes)`. The `work` closure is
/// invoked once per chunk with a positioned `BufReader`, the chunk's byte
/// budget, and a per-worker `Write` sink. Returns the vector of per-worker
/// states produced by `work` so the caller can merge them.
///
/// When the byte range is too small to split, falls back to a single
/// in-line invocation of `work` writing directly to `output` (no
/// `UnorderedSink` wrapper). The returned vector then contains exactly one
/// state. Callers can merge unconditionally.
pub(crate) fn run<S, F>(
    job: Job<'_>,
    output: &mut (dyn Write + Send),
    work: F,
) -> anyhow::Result<Vec<S>>
where
    F: Fn(BufReader<FileInput>, u64, &mut dyn Write) -> anyhow::Result<S> + Send + Sync,
    S: Send,
{
    let Job {
        path,
        start_byte,
        max_bytes,
        n_workers,
        seek_table,
    } = job;

    let end_byte = start_byte.saturating_add(max_bytes);
    let chunks = {
        let mut probe = FileInput::open_with_table(path, seek_table.as_ref())?;
        compute_chunks(&mut probe, start_byte, end_byte, n_workers)?
    };

    if chunks.len() <= 1 {
        let mut input = FileInput::open_with_table(path, seek_table.as_ref())?;
        input.seek(SeekFrom::Start(start_byte))?;
        let reader = BufReader::with_capacity(crate::run::STREAM_BUF_CAP, input);
        let state = work(reader, max_bytes, output)?;
        return Ok(vec![state]);
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
    let work_ref = &work;
    let st_ref = seek_table.as_ref();
    let worker_states: Vec<S> = std::thread::scope(|s| -> anyhow::Result<Vec<S>> {
        let handles: Vec<_> = chunks
            .iter()
            .map(|&(cs, ce)| {
                let shared = &shared_out;
                s.spawn(move || -> anyhow::Result<S> {
                    let mut input = FileInput::open_with_table(path, st_ref)?;
                    input.seek(SeekFrom::Start(cs))?;
                    let reader = BufReader::with_capacity(crate::run::STREAM_BUF_CAP, input);
                    let mut sink = UnorderedSink::new(shared);
                    let state = work_ref(reader, ce - cs, &mut sink)?;
                    sink.flush()?;
                    Ok(state)
                })
            })
            .collect();
        let mut all = Vec::with_capacity(handles.len());
        for h in handles {
            all.push(h.join().expect("worker panicked")?);
        }
        Ok(all)
    })?;

    Ok(worker_states)
}

/// Compute up to `n` non-overlapping byte ranges over `[start, end)` whose
/// boundaries lie at line starts (or at `start` / `end`). Each interior
/// boundary is forward-snapped to the byte after the next `\n`, so every
/// chunk except the first begins at a line boundary. Returns a single
/// `(start, end)` chunk if the span is too small to be worth splitting.
pub(crate) fn compute_chunks<R: Read + Seek>(
    reader: &mut R,
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
        bounds.push(snap_forward_to_newline(reader, approx, end)?);
    }
    bounds.push(end);
    Ok(bounds
        .windows(2)
        .filter_map(|w| (w[0] < w[1]).then_some((w[0], w[1])))
        .collect())
}

/// Read forward from `from` until just past the next `\n`, capped at `cap`.
/// Returns the new absolute position. Restores the reader's seek position to
/// its value on entry.
fn snap_forward_to_newline<R: Read + Seek>(
    reader: &mut R,
    from: u64,
    cap: u64,
) -> std::io::Result<u64> {
    let saved = reader.stream_position()?;
    reader.seek(SeekFrom::Start(from))?;
    let mut buf = [0u8; 8192];
    let mut pos = from;
    let result = loop {
        if pos >= cap {
            break cap;
        }
        let to_read = std::cmp::min(buf.len() as u64, cap - pos) as usize;
        let n = reader.read(&mut buf[..to_read])?;
        if n == 0 {
            break pos;
        }
        if let Some(idx) = memchr::memchr(b'\n', &buf[..n]) {
            break pos + idx as u64 + 1;
        }
        pos += n as u64;
    };
    reader.seek(SeekFrom::Start(saved))?;
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
    use std::fs::File;

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
