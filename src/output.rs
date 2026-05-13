//! Line and aggregation-row emitters. Logfmt-shaped writers (both colored
//! and plain) live here; JSON-shaped writers live in `json.rs`. Hot-path
//! per-line emission is dispatched through the [`OutputFormat`] trait so
//! `run::emit_match` doesn't branch on `LineMode` outside of choosing the
//! impl. Pure serialization — no parsing, no aggregation logic.

use rapidhash::RapidHashSet;
use smartstring::alias::String as SmartString;
use std::io::{self, Write};

use crate::json;
use crate::logfmt;
use crate::timestamp::{self, Timestamp};

// -------- ANSI escapes (colored logfmt only) --------

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const COL_BLUE: &str = "\x1b[34m";
const COL_RED: &str = "\x1b[31m";
const COL_YELLOW: &str = "\x1b[33m";
const COL_GRAY: &str = "\x1b[90m";
const COL_QUOTE: &str = "\x1b[1;34m";
// Bright white on red background — used for `critical`/`crit` so it really pops.
const COL_CRIT: &str = "\x1b[97;41m";

// -------- OutputFormat trait --------

/// Per-line emitter, picked once per run by `run::emit_match`. Implementors
/// serialise a parsed line (a sequence of `[(key, value)]` slices — outer
/// slice is to fold in `--add` pairs without per-call concatenation) into
/// `w`. Values are still in logfmt-escaped form; impls unescape if their
/// wire format demands it (jsonl) or pass through (logfmt).
pub trait OutputFormat {
    type Options;

    /// Write one line. `remove` lists keys to drop (`--rm`). `scratch` is a
    /// reusable `String` owned by the caller; the impl is free to clear and
    /// fill it as a working buffer (e.g. for logfmt unescape) and must not
    /// rely on its prior contents.
    fn output_line<W: Write + ?Sized>(
        w: &mut W,
        pairs: &[&[(&str, &str)]],
        remove: &[&str],
        opts: &Self::Options,
        scratch: &mut String,
    ) -> io::Result<()>;
}

#[inline]
fn is_removed(remove: &[&str], k: &str) -> bool {
    // Hot-path optimisation: when `--rm` is unused the slice is empty and we
    // skip the linear scan entirely.
    !remove.is_empty() && remove.contains(&k)
}

// -------- logfmt format (plain + colored, picked via `LogfmtOptions.color`) --------

pub struct LogfmtFormat;

#[derive(Debug, Clone, Copy)]
pub struct LogfmtOptions {
    /// When true, emit ANSI styling (bold keys, level-coloured values,
    /// quoted-value highlighting). When false, emit raw logfmt bytes.
    pub color: bool,
}

fn level_color(value: &str) -> &'static str {
    let v = value.trim_matches('"');
    match v {
        "critical" | "CRITICAL" | "crit" | "CRIT" => COL_CRIT,
        "error" | "fatal" | "ERROR" | "FATAL" => COL_RED,
        "warn" | "warning" | "WARN" | "WARNING" => COL_YELLOW,
        "info" | "INFO" => COL_BLUE,
        "debug" | "trace" | "DEBUG" | "TRACE" => COL_GRAY,
        _ => "",
    }
}

/// Emit a value with optional color and bold. If the value is wrapped in
/// double quotes, the quote characters are highlighted so the content
/// boundary is easy to spot.
fn write_value<W: Write + ?Sized>(out: &mut W, v: &str, color: &str, bold: bool) -> io::Result<()> {
    let b = v.as_bytes();
    let quoted = b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"';
    let inner = if quoted { &v[1..v.len() - 1] } else { v };
    let styled = bold || !color.is_empty();

    if quoted {
        write!(out, "{COL_QUOTE}\"{RESET}")?;
    }
    if bold {
        out.write_all(BOLD.as_bytes())?;
    }
    if !color.is_empty() {
        out.write_all(color.as_bytes())?;
    }
    out.write_all(inner.as_bytes())?;
    if styled {
        out.write_all(RESET.as_bytes())?;
    }
    if quoted {
        write!(out, "{COL_QUOTE}\"{RESET}")?;
    }
    Ok(())
}

impl OutputFormat for LogfmtFormat {
    type Options = LogfmtOptions;

    fn output_line<W: Write + ?Sized>(
        w: &mut W,
        pairs: &[&[(&str, &str)]],
        remove: &[&str],
        opts: &Self::Options,
        _scratch: &mut String,
    ) -> io::Result<()> {
        // For colored output, pick the level colour up front so the bold value
        // styling matches the level value (matters when `level` appears mid-line).
        let lvl_sgr = if opts.color {
            pairs
                .iter()
                .flat_map(|s| s.iter())
                .find_map(|(k, v)| {
                    if is_removed(remove, k) {
                        return None;
                    }
                    (*k == "level").then(|| level_color(v))
                })
                .unwrap_or("")
        } else {
            ""
        };

        let mut first = true;
        for slice in pairs {
            for (k, v) in *slice {
                if is_removed(remove, k) {
                    continue;
                }
                if !first {
                    w.write_all(b" ")?;
                }
                first = false;
                if opts.color {
                    write!(w, "{BOLD}{k}{RESET}=")?;
                    let color: &str = match *k {
                        "time" | "ts" => COL_BLUE,
                        "level" => lvl_sgr,
                        _ => "",
                    };
                    write_value(w, v, color, *k == "level")?;
                } else {
                    w.write_all(k.as_bytes())?;
                    w.write_all(b"=")?;
                    w.write_all(v.as_bytes())?;
                }
            }
        }
        if opts.color {
            w.write_all(b"\n")?;
        }
        Ok(())
    }
}

// -------- jsonl format --------

pub struct JsonlFormat;

#[derive(Debug, Clone, Copy, Default)]
pub struct JsonlOptions;

impl OutputFormat for JsonlFormat {
    type Options = JsonlOptions;

    fn output_line<W: Write + ?Sized>(
        w: &mut W,
        pairs: &[&[(&str, &str)]],
        remove: &[&str],
        _opts: &Self::Options,
        scratch: &mut String,
    ) -> io::Result<()> {
        let mut obj = json::JsonObj::open(w)?;
        for slice in pairs {
            for (k, v) in *slice {
                if is_removed(remove, k) {
                    continue;
                }
                obj.entry_logfmt_value(k, v, scratch)?;
            }
        }
        obj.finish()?;
        w.write_all(b"\n")
    }
}

// -------- aggregation row output (logfmt only; JSON variant in `json.rs`) --------

/// Logfmt aggregation row: `count=N key.<k>=… [bucket.start=… bucket.end=…]
/// [time.start=… time.end=…]`.
pub(crate) fn write_agg_row_logfmt<W: Write + ?Sized>(
    out: &mut W,
    count: u64,
    keys: &[SmartString],
    combo: &[SmartString],
    bucket: Option<(Timestamp, Timestamp)>,
    time_range: Option<(Timestamp, Timestamp)>,
    tz: &jiff::tz::TimeZone,
) -> io::Result<()> {
    write!(out, "count={count}")?;
    for (k, v) in keys.iter().zip(combo.iter()) {
        write!(out, " key.{k}=")?;
        logfmt::write_logfmt_value(out, v.as_str())?;
    }
    if let Some((start, end)) = bucket {
        write!(
            out,
            " bucket.start={}",
            timestamp::format_rfc3339(start, tz)
        )?;
        write!(out, " bucket.end={}", timestamp::format_rfc3339(end, tz))?;
    }
    if let Some((a, b)) = time_range {
        write!(out, " time.start={}", timestamp::format_rfc3339(a, tz))?;
        write!(out, " time.end={}", timestamp::format_rfc3339(b, tz))?;
    }
    writeln!(out)
}

// -------- summary outputs (`--list-keys`, `--list-values-for`) --------

fn sorted(set: &RapidHashSet<SmartString>) -> Vec<&SmartString> {
    let mut v: Vec<&SmartString> = set.iter().collect();
    v.sort_unstable();
    v
}

/// `--list-keys` (or single `--list-values-for`) logfmt: one value per line, sorted.
pub(crate) fn write_string_set_logfmt<W: Write + ?Sized>(
    out: &mut W,
    set: &RapidHashSet<SmartString>,
) -> io::Result<()> {
    for v in sorted(set) {
        writeln!(out, "{v}")?;
    }
    Ok(())
}

/// `--list-values-for` logfmt: multi-key prefixes each block with `# <key>`.
pub(crate) fn write_values_summary_logfmt<W: Write + ?Sized>(
    out: &mut W,
    keys: &[SmartString],
    values: &[RapidHashSet<SmartString>],
) -> io::Result<()> {
    let multi = keys.len() > 1;
    for (key, set) in keys.iter().zip(values.iter()) {
        if multi {
            writeln!(out, "# {key}")?;
        }
        write_string_set_logfmt(out, set)?;
    }
    Ok(())
}
