//! Synthetic logfmt log generator for the riplog bench harness.
//!
//! Emits lines of the form `time=<RFC3339> level=<lvl> msg=<msg>` with the
//! same shape as `tests/generate_logs.py`: ~1% `critical`, the remaining 99%
//! sampled uniformly from {debug, info, warn, error}, and `msg` chosen from
//! a fixed pool of 9 strings (quoted if it contains space/quote/equals).
//!
//! Deterministic given `--seed`. Same seed + same count + same start-time +
//! same rate produces byte-identical output.
//!
//! No real-time mode: this is for fixture generation. `--count` is required.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const LEVELS: [&str; 4] = ["debug", "info", "warn", "error"];

const MESSAGES: [&str; 9] = [
    "startup",
    "request handled",
    "cache miss",
    "cache hit",
    "connection reset",
    "user logged in",
    "task complete",
    "retry scheduled",
    "deadline exceeded",
];

/// Whether `msg` needs to be quoted under logfmt rules used by the Python
/// generator: space, double-quote, or equals.
fn needs_quote(s: &str) -> bool {
    s.bytes().any(|b| b == b' ' || b == b'"' || b == b'=')
}

/// Parse an RFC 3339 timestamp like `2026-01-01T00:00:00Z` or
/// `2026-01-01T00:00:00.123456Z` into (seconds-since-epoch, microseconds).
/// We accept only `Z`-terminated UTC inputs to keep the example dependency-free.
fn parse_start_time(s: &str) -> Result<(i64, u32), String> {
    // Trim trailing 'Z'.
    let s = s
        .strip_suffix('Z')
        .or_else(|| s.strip_suffix("+00:00"))
        .ok_or_else(|| format!("--start-time must end with Z or +00:00: {s:?}"))?;
    // Split on 'T'.
    let (date, time) = s
        .split_once('T')
        .ok_or_else(|| format!("--start-time missing 'T': {s:?}"))?;
    let mut dp = date.split('-');
    let y: i32 = dp
        .next()
        .ok_or("missing year")?
        .parse()
        .map_err(|e| format!("year: {e}"))?;
    let mo: u32 = dp
        .next()
        .ok_or("missing month")?
        .parse()
        .map_err(|e| format!("month: {e}"))?;
    let d: u32 = dp
        .next()
        .ok_or("missing day")?
        .parse()
        .map_err(|e| format!("day: {e}"))?;
    if dp.next().is_some() {
        return Err(format!("extra date component in {s:?}"));
    }
    // Time may be HH:MM:SS or HH:MM:SS.ffffff
    let (hms, micro) = match time.split_once('.') {
        Some((h, f)) => (h, f),
        None => (time, "0"),
    };
    let mut tp = hms.split(':');
    let h: u32 = tp
        .next()
        .ok_or("missing hour")?
        .parse()
        .map_err(|e| format!("hour: {e}"))?;
    let mi: u32 = tp
        .next()
        .ok_or("missing minute")?
        .parse()
        .map_err(|e| format!("min: {e}"))?;
    let sec: u32 = tp
        .next()
        .ok_or("missing second")?
        .parse()
        .map_err(|e| format!("sec: {e}"))?;
    // Parse fractional seconds, padding/truncating to microseconds.
    let mut micro_str = String::from(micro);
    while micro_str.len() < 6 {
        micro_str.push('0');
    }
    micro_str.truncate(6);
    let us: u32 = micro_str
        .parse()
        .map_err(|e| format!("microseconds: {e}"))?;

    let epoch = days_from_civil(y, mo, d) * 86_400 + h as i64 * 3600 + mi as i64 * 60 + sec as i64;
    Ok((epoch, us))
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 (proleptic
/// Gregorian). Works for any reasonable input year.
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let m = m as i64;
    let d = d as i64;
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe as i64 * 365 + (yoe / 4) as i64 - (yoe / 100) as i64 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse: epoch days -> (year, month, day).
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d)
}

/// Format `epoch_seconds` + `micros` as `YYYY-MM-DDTHH:MM:SS.uuuuuuZ` into
/// the given buffer.
fn write_rfc3339(buf: &mut Vec<u8>, epoch_secs: i64, micros: u32) {
    let days = epoch_secs.div_euclid(86_400);
    let sod = epoch_secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let h = (sod / 3600) as u32;
    let mi = ((sod % 3600) / 60) as u32;
    let s = (sod % 60) as u32;

    // Year: assume in [0, 9999]; pad to 4 digits.
    write_u32_pad(buf, y as u32, 4);
    buf.push(b'-');
    write_u32_pad(buf, mo, 2);
    buf.push(b'-');
    write_u32_pad(buf, d, 2);
    buf.push(b'T');
    write_u32_pad(buf, h, 2);
    buf.push(b':');
    write_u32_pad(buf, mi, 2);
    buf.push(b':');
    write_u32_pad(buf, s, 2);
    buf.push(b'.');
    write_u32_pad(buf, micros, 6);
    buf.push(b'Z');
}

fn write_u32_pad(buf: &mut Vec<u8>, n: u32, width: usize) {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    let mut x = n;
    if x == 0 {
        i -= 1;
        tmp[i] = b'0';
    } else {
        while x > 0 {
            i -= 1;
            tmp[i] = b'0' + (x % 10) as u8;
            x /= 10;
        }
    }
    let len = tmp.len() - i;
    for _ in len..width {
        buf.push(b'0');
    }
    buf.extend_from_slice(&tmp[i..]);
}

fn print_help(prog: &str) {
    eprintln!(
        "Usage: {prog} --count N --start-time RFC3339 [--seed S] [--rate R] [--out PATH]\n\
         \n\
         Generate N synthetic logfmt lines deterministically.\n\
         \n\
         Required:\n  \
           --count N         number of lines to emit\n  \
           --start-time T    RFC 3339 start (e.g. 2026-01-01T00:00:00Z)\n\
         Optional:\n  \
           --seed S          PRNG seed (default 0)\n  \
           --rate R          lines per second of synthetic time spacing (default 1000)\n  \
           --out PATH        output file (default: stdout)\n"
    );
}

struct Args {
    count: u64,
    seed: u64,
    start: String,
    rate: f64,
    out: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut count: Option<u64> = None;
    let mut seed: u64 = 0;
    let mut start: Option<String> = None;
    let mut rate: f64 = 1000.0;
    let mut out: Option<PathBuf> = None;

    let mut argv = std::env::args();
    let prog = argv.next().unwrap_or_else(|| "gen_logs".to_string());
    let mut it = argv.peekable();
    while let Some(a) = it.next() {
        let want =
            |it: &mut std::iter::Peekable<std::env::Args>, name: &str| -> Result<String, String> {
                it.next().ok_or_else(|| format!("{name} requires a value"))
            };
        match a.as_str() {
            "-h" | "--help" => {
                print_help(&prog);
                std::process::exit(0);
            }
            "--count" => {
                count = Some(
                    want(&mut it, "--count")?
                        .parse()
                        .map_err(|e| format!("--count: {e}"))?,
                )
            }
            "--seed" => {
                seed = want(&mut it, "--seed")?
                    .parse()
                    .map_err(|e| format!("--seed: {e}"))?
            }
            "--start-time" => start = Some(want(&mut it, "--start-time")?),
            "--rate" => {
                rate = want(&mut it, "--rate")?
                    .parse()
                    .map_err(|e| format!("--rate: {e}"))?
            }
            "--out" => out = Some(PathBuf::from(want(&mut it, "--out")?)),
            other => return Err(format!("unknown arg {other:?}")),
        }
    }
    let count = count.ok_or_else(|| "--count is required".to_string())?;
    let start = start.ok_or_else(|| "--start-time is required".to_string())?;
    if rate <= 0.0 {
        return Err("--rate must be positive".to_string());
    }
    Ok(Args {
        count,
        seed,
        start,
        rate,
        out,
    })
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let (epoch_secs, start_us) = parse_start_time(&args.start)?;

    // Interval per line, in microseconds (rounded to nearest).
    // We compute each line's timestamp from its index to avoid drift, using
    // 128-bit arithmetic: us(i) = start_us + round(i * 1e6 / rate).
    // For simplicity and determinism we use i*1_000_000 / rate as an integer.
    let interval_us_num: u128 = 1_000_000;
    // `rate` is f64; scale to keep precision: us(i) = (i * 1_000_000 / rate).
    // Use f64 here — for fixture generation any drift is < 1us per line over
    // realistic counts and the output is still strictly monotonic for
    // reasonable rates.

    let mut rng = fastrand::Rng::with_seed(args.seed);

    // Output sink: BufWriter with 1 MiB buffer.
    const BUF_CAP: usize = 1 << 20;
    let mut writer: Box<dyn Write> = match &args.out {
        Some(path) => Box::new(BufWriter::with_capacity(
            BUF_CAP,
            File::create(path).map_err(|e| format!("create {path:?}: {e}"))?,
        )),
        None => Box::new(BufWriter::with_capacity(BUF_CAP, io::stdout().lock())),
    };

    // Per-line scratch buffer.
    let mut line = Vec::with_capacity(128);

    for i in 0..args.count {
        // Synthetic time for line i.
        // us_offset = round(i * 1_000_000 / rate). Use f64 for division.
        let us_offset = ((i as f64) * (interval_us_num as f64) / args.rate) as u64;
        let total_us = start_us as u64 + us_offset;
        let extra_secs = (total_us / 1_000_000) as i64;
        let micros = (total_us % 1_000_000) as u32;
        let secs = epoch_secs + extra_secs;

        // Sample. Match the Python generator's RNG-call ordering:
        //   roll critical?  (always one call)
        //     if not critical: another call for level index
        //   one call for msg index
        // We use raw u32 / bounded helpers so behavior is deterministic.
        let crit_roll = rng.f64(); // [0, 1)
        let level: &str = if crit_roll < 0.01 {
            "critical"
        } else {
            let li = rng.usize(0..LEVELS.len());
            LEVELS[li]
        };
        let mi = rng.usize(0..MESSAGES.len());
        let msg = MESSAGES[mi];

        line.clear();
        line.extend_from_slice(b"time=");
        write_rfc3339(&mut line, secs, micros);
        line.extend_from_slice(b" level=");
        line.extend_from_slice(level.as_bytes());
        line.extend_from_slice(b" msg=");
        if needs_quote(msg) {
            line.push(b'"');
            // None of the messages contain `"` or `\`, so no escaping needed.
            line.extend_from_slice(msg.as_bytes());
            line.push(b'"');
        } else {
            line.extend_from_slice(msg.as_bytes());
        }
        line.push(b'\n');

        writer.write_all(&line).map_err(|e| format!("write: {e}"))?;
    }
    writer.flush().map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("gen_logs: {e}");
            ExitCode::FAILURE
        }
    }
}
