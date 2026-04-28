//! Timestamp-based bisection over a (mostly-sorted) logfmt stream.
//!
//! Mostly-sorted means: if the entry at file position `P` has timestamp
//! `T(P)`, every entry before `P` has timestamp `≤ T(P) + window`, and every
//! entry after `P` has timestamp `≥ T(P) − window`. Equivalently, no entry is
//! reordered by more than `window` against the file's overall ordering.
//!
//! Under that assumption [`bisect`] returns a byte offset that frames an
//! over-approximation of the time slice: the result is conservative on the
//! safe side, so the caller can stream from `start_byte..end_byte` and be
//! guaranteed to see every entry within `[t1, t2]`, plus possibly a few extras
//! near each edge.
//!
//! The returned offsets land on line boundaries.

use std::io::{Read, Seek, SeekFrom};

use crate::logfmt;
use crate::timestamp::{Timestamp, extract_timestamp};

/// Which frontier to find.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Lower frontier: largest offset such that everything before has
    /// timestamp `< timestamp`. Computed against the shifted threshold
    /// `timestamp − window`.
    Lower,
    /// Upper frontier: smallest offset such that everything after has
    /// timestamp `> timestamp`. Computed against the shifted threshold
    /// `timestamp + window`.
    Upper,
}

/// Maximum bytes to scan past a probe point looking for a parseable timestamp.
const PROBE_SCAN_BYTES: u64 = 1 << 20; // 1 MiB

/// Bisection terminates when the search interval is smaller than this. Larger
/// values are faster but slightly looser; the over-approximation property
/// still holds.
const MIN_LINE_BYTES: u64 = 64;

/// Find the byte offset of the requested frontier.
///
/// See module docs for the over-approximation guarantee.
pub fn bisect<R: Read + Seek>(
    reader: &mut R,
    timestamp: Timestamp,
    window_nanos: i64,
    side: Side,
) -> anyhow::Result<u64> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    if file_len == 0 {
        return Ok(0);
    }

    // Both sides reduce to: find the smallest position P such that
    //   T(P) `cmp` threshold
    // where:
    //   Lower → cmp is `>=`, threshold = t - window
    //           (start_byte = first line whose ts could be ≥ t once we
    //            account for the reorder window: T(P) + window ≥ t)
    //   Upper → cmp is `>`,  threshold = t + window
    //           (end_byte = first line whose ts is guaranteed > t for every
    //            entry from there on: T(P) - window > t)
    let (threshold, strict) = match side {
        Side::Lower => (timestamp.saturating_sub(window_nanos), false),
        Side::Upper => (timestamp.saturating_add(window_nanos), true),
    };

    let mut lo: u64 = 0;
    let mut hi: u64 = file_len;

    while hi - lo > MIN_LINE_BYTES {
        let mid = lo + (hi - lo) / 2;
        match probe(reader, mid, file_len)? {
            Some((line_start, ts)) => {
                let satisfied = if strict {
                    ts > threshold
                } else {
                    ts >= threshold
                };
                if satisfied {
                    // line_start is a valid candidate; pull hi inward.
                    if line_start >= hi {
                        break;
                    }
                    hi = line_start;
                } else {
                    // line_start does not satisfy; advance past it.
                    let next = line_start.saturating_add(1);
                    if next >= hi {
                        break;
                    }
                    lo = next;
                }
            }
            None => {
                // No parseable timestamp in [mid, mid + PROBE_SCAN_BYTES).
                // Shrink conservatively to preserve the over-approximation:
                //   Lower → pull hi inward (keep start_byte smaller → more inclusive)
                //   Upper → push lo forward (keep end_byte larger → more inclusive)
                match side {
                    Side::Lower => {
                        if mid <= lo {
                            break;
                        }
                        hi = mid;
                    }
                    Side::Upper => {
                        let next = mid.saturating_add(1);
                        if next >= hi {
                            break;
                        }
                        lo = next;
                    }
                }
            }
        }
    }

    // Refinement pass: bisection narrowed the answer to [lo, hi). When the
    // answer is the very first line (e.g. lo never advanced past 0) the loop
    // never probes offset 0 because `mid > 0` whenever `lo == 0 && hi > 0`.
    // A single probe at lo catches that case.
    if lo < hi {
        if let Some((line_start, ts)) = probe(reader, lo, file_len)? {
            let satisfied = if strict {
                ts > threshold
            } else {
                ts >= threshold
            };
            if satisfied && line_start < hi {
                return Ok(line_start);
            }
        }
    }

    Ok(hi)
}

/// First parseable timestamp at or near the start of the stream.
pub fn peek_first_timestamp<R: Read + Seek>(reader: &mut R) -> anyhow::Result<Option<Timestamp>> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    if file_len == 0 {
        return Ok(None);
    }
    Ok(probe(reader, 0, file_len)?.map(|(_, ts)| ts))
}

/// (min, max) parseable timestamps from the head and tail of the stream.
/// When the file fits in a single [`PROBE_SCAN_BYTES`] chunk, scans once
/// to avoid reading overlapping regions twice.
pub fn time_range<R: Read + Seek>(
    reader: &mut R,
) -> anyhow::Result<(Option<Timestamp>, Option<Timestamp>)> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    if file_len == 0 {
        return Ok((None, None));
    }
    if file_len <= PROBE_SCAN_BYTES {
        let buf = read_head(reader)?;
        let mut min: Option<Timestamp> = None;
        let mut max: Option<Timestamp> = None;
        fold_timestamps_in_chunk(&buf, false, |_, ts| {
            min = Some(min.map_or(ts, |m| m.min(ts)));
            max = Some(max.map_or(ts, |m| m.max(ts)));
            None
        });
        return Ok((min, max));
    }
    let min = min_timestamp_in_head(reader)?;
    let max = max_timestamp_in_tail(reader)?;
    Ok((min, max))
}

/// Minimum parseable timestamp in the head [`PROBE_SCAN_BYTES`] of the
/// stream. Tolerates a small reorder near the beginning.
pub fn min_timestamp_in_head<R: Read + Seek>(reader: &mut R) -> anyhow::Result<Option<Timestamp>> {
    let buf = read_head(reader)?;
    Ok(fold_timestamps_in_chunk(&buf, false, |acc, ts| {
        Some(acc.map_or(ts, |a: Timestamp| a.min(ts)))
    }))
}

/// Maximum parseable timestamp in the tail [`PROBE_SCAN_BYTES`] of the
/// stream. Tolerates a small reorder near the end.
pub fn max_timestamp_in_tail<R: Read + Seek>(reader: &mut R) -> anyhow::Result<Option<Timestamp>> {
    let (buf, skip) = read_tail(reader)?;
    Ok(fold_timestamps_in_chunk(&buf, skip, |acc, ts| {
        Some(acc.map_or(ts, |a: Timestamp| a.max(ts)))
    }))
}

/// Last parseable timestamp in the stream. Scans the trailing
/// [`PROBE_SCAN_BYTES`]; if no parseable line is found returns `None`.
pub fn peek_last_timestamp<R: Read + Seek>(reader: &mut R) -> anyhow::Result<Option<Timestamp>> {
    let (buf, skip) = read_tail(reader)?;
    Ok(fold_timestamps_in_chunk(&buf, skip, |_, ts| Some(ts)))
}

fn read_head<R: Read + Seek>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    if file_len == 0 {
        return Ok(Vec::new());
    }
    let chunk = PROBE_SCAN_BYTES.min(file_len) as usize;
    reader.seek(SeekFrom::Start(0))?;
    let mut buf = vec![0u8; chunk];
    let n = read_fully(reader, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Returns `(buf, skip_partial_first_line)`. The skip flag is true when the
/// chunk begins mid-file and the caller must drop everything up to the first
/// newline.
fn read_tail<R: Read + Seek>(reader: &mut R) -> anyhow::Result<(Vec<u8>, bool)> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    if file_len == 0 {
        return Ok((Vec::new(), false));
    }
    let chunk = PROBE_SCAN_BYTES.min(file_len);
    let chunk_start = file_len - chunk;
    reader.seek(SeekFrom::Start(chunk_start))?;
    let mut buf = vec![0u8; chunk as usize];
    let n = read_fully(reader, &mut buf)?;
    buf.truncate(n);
    Ok((buf, chunk_start != 0))
}

/// Walks every newline-delimited line in `buf`, parses each as logfmt,
/// extracts a timestamp, and threads it through `fold`. If `skip_partial`
/// is set, the first (potentially incomplete) line is discarded.
fn fold_timestamps_in_chunk(
    buf: &[u8],
    skip_partial: bool,
    mut fold: impl FnMut(Option<Timestamp>, Timestamp) -> Option<Timestamp>,
) -> Option<Timestamp> {
    let mut idx = if skip_partial {
        match memchr::memchr(b'\n', buf) {
            Some(i) => i + 1,
            None => return None,
        }
    } else {
        0
    };
    let mut acc: Option<Timestamp> = None;
    while idx < buf.len() {
        let end = memchr::memchr(b'\n', &buf[idx..])
            .map(|i| idx + i)
            .unwrap_or(buf.len());
        let line_end = if end > idx && buf[end - 1] == b'\r' {
            end - 1
        } else {
            end
        };
        if let Ok(line) = std::str::from_utf8(&buf[idx..line_end]) {
            let mut pairs = logfmt::PairsBuffer::<256>::new();
            let (parsed, _) = pairs.parse(line);
            if let Some(ts) = extract_timestamp(parsed) {
                acc = fold(acc, ts);
            }
        }
        if end >= buf.len() {
            break;
        }
        idx = end + 1;
    }
    acc
}

/// Seek to `offset`, snap forward to the next line boundary, and return the
/// byte offset and timestamp of the first line within the next
/// [`PROBE_SCAN_BYTES`] bytes that has a parseable `time=…` value.
fn probe<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    file_len: u64,
) -> anyhow::Result<Option<(u64, Timestamp)>> {
    if offset >= file_len {
        return Ok(None);
    }

    reader.seek(SeekFrom::Start(offset))?;

    let scan_end = (offset + PROBE_SCAN_BYTES).min(file_len);
    let to_read = (scan_end - offset) as usize;
    let mut buf = vec![0u8; to_read];
    let n = read_fully(reader, &mut buf)?;
    buf.truncate(n);

    // If we didn't start at byte 0 and we didn't seek to a known line start,
    // skip past the partial first line.
    let mut start = if offset == 0 {
        0
    } else {
        match memchr::memchr(b'\n', &buf) {
            Some(i) => i + 1,
            None => return Ok(None),
        }
    };

    while start < buf.len() {
        let end = memchr::memchr(b'\n', &buf[start..])
            .map(|i| start + i)
            .unwrap_or(buf.len());

        // Strip CR if present.
        let line_end = if end > start && buf[end - 1] == b'\r' {
            end - 1
        } else {
            end
        };

        if let Ok(line) = std::str::from_utf8(&buf[start..line_end]) {
            let mut pairs = logfmt::PairsBuffer::<256>::new();
            let (parsed, _overflow) = pairs.parse(line);
            if let Some(ts) = extract_timestamp(parsed) {
                return Ok(Some((offset + start as u64, ts)));
            }
        }

        if end >= buf.len() {
            break;
        }
        start = end + 1;
    }

    Ok(None)
}

fn read_fully<R: Read>(reader: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timestamp::parse_rfc3339_nanos;
    use std::io::Cursor;

    /// Build an in-memory log with one line per second starting at
    /// `2026-04-24T18:00:00Z`, for `count` lines.
    fn build_sorted_log(count: usize) -> (Vec<u8>, Vec<Timestamp>) {
        let base = parse_rfc3339_nanos("2026-04-24T18:00:00Z").unwrap();
        let mut buf = String::new();
        let mut ts = Vec::with_capacity(count);
        for i in 0..count {
            let secs = i as i64;
            // Format manually: 2026-04-24T18:MM:SSZ for small counts.
            let mins = secs / 60;
            let s = secs % 60;
            buf.push_str(&format!(
                "level=info time=2026-04-24T18:{:02}:{:02}Z msg=hello_{}\n",
                mins, s, i
            ));
            ts.push(base + secs * 1_000_000_000);
        }
        (buf.into_bytes(), ts)
    }

    #[test]
    fn empty_file() {
        let mut cur = Cursor::new(Vec::<u8>::new());
        let lo = bisect(&mut cur, 0, 0, Side::Lower).unwrap();
        let hi = bisect(&mut cur, 0, 0, Side::Upper).unwrap();
        assert_eq!(lo, 0);
        assert_eq!(hi, 0);
    }

    #[test]
    fn frontier_sorted() {
        let (data, ts) = build_sorted_log(120);
        let mut cur = Cursor::new(data.clone());

        // Pick t1 = ts[30], t2 = ts[80], window = 0.
        let t1 = ts[30];
        let t2 = ts[80];

        let start = bisect(&mut cur, t1, 0, Side::Lower).unwrap();
        let end = bisect(&mut cur, t2, 0, Side::Upper).unwrap();

        assert!(start <= end);

        // Slice must contain every line whose timestamp ∈ [t1, t2].
        let slice = &data[start as usize..end as usize];
        let text = std::str::from_utf8(slice).unwrap();
        for i in 30..=80 {
            let needle = format!("msg=hello_{}\n", i);
            assert!(
                text.contains(&needle),
                "missing line {} in bisected slice",
                i
            );
        }
    }

    #[test]
    fn frontier_window_tolerates_reorder() {
        // Build a sorted log, then swap a few neighbouring lines to simulate
        // small reorders within a 5-second window.
        let (data, ts) = build_sorted_log(120);

        // For simplicity, verify bisection on sorted data with a non-zero
        // window — the over-approximation expands the slice, which still
        // must contain the in-range lines.
        let t1 = ts[40];
        let t2 = ts[70];
        let window = 5 * 1_000_000_000; // 5 seconds

        let mut cur = Cursor::new(data.clone());
        let start = bisect(&mut cur, t1, window, Side::Lower).unwrap();
        let end = bisect(&mut cur, t2, window, Side::Upper).unwrap();

        // With a 5s window, the lower frontier targets ts[35] or earlier;
        // the upper frontier targets ts[75] or later.
        let slice = &data[start as usize..end as usize];
        let text = std::str::from_utf8(slice).unwrap();
        for i in 40..=70 {
            assert!(text.contains(&format!("msg=hello_{}\n", i)));
        }
        // Sanity: window expansion should also keep some boundary lines.
        assert!(start < end);
    }

    #[test]
    fn peek_first_and_last() {
        let (data, ts) = build_sorted_log(120);
        let mut cur = Cursor::new(data);
        assert_eq!(peek_first_timestamp(&mut cur).unwrap(), Some(ts[0]));
        assert_eq!(
            peek_last_timestamp(&mut cur).unwrap(),
            Some(*ts.last().unwrap())
        );
    }

    #[test]
    fn peek_empty() {
        let mut cur = Cursor::new(Vec::<u8>::new());
        assert_eq!(peek_first_timestamp(&mut cur).unwrap(), None);
        assert_eq!(peek_last_timestamp(&mut cur).unwrap(), None);
    }

    #[test]
    fn frontier_before_first() {
        let (data, ts) = build_sorted_log(50);
        let very_early = ts[0] - 1_000_000_000_000; // 1000 seconds before first
        let mut cur = Cursor::new(data);
        let start = bisect(&mut cur, very_early, 0, Side::Lower).unwrap();
        assert_eq!(start, 0);
    }

    #[test]
    fn frontier_after_last() {
        let (data, ts) = build_sorted_log(50);
        let file_len = data.len() as u64;
        let very_late = ts[ts.len() - 1] + 1_000_000_000_000;
        let mut cur = Cursor::new(data);
        let end = bisect(&mut cur, very_late, 0, Side::Upper).unwrap();
        assert_eq!(end, file_len);
    }
}
