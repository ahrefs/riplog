//! Line and aggregation-row emitters in both `logfmt` (the default) and JSON
//! (`--json`) formats. Pure serialization — no parsing, no aggregation logic.
//! Callers in `run::Sinks` / `Counter` pick the format based on `output_json`.

use rapidhash::RapidHashSet;
use smartstring::alias::String as SmartString;
use std::io::{self, Write};

use crate::json::{JsonArr, JsonObj};
use crate::logfmt;
use crate::timestamp::{self, Timestamp};
use crate::transform::LineTransform;

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

// -------- logfmt line output --------

#[inline]
fn plain_field_sep(buf: &mut Vec<u8>, first: &mut bool) {
    if !*first {
        buf.push(b' ');
    }
    *first = false;
}

/// Plain reconstruction: optional `--rm` / `--add`. Without a transform, copies
/// original pair tokens verbatim (hot path when `transform` is `None`).
pub(crate) fn write_plain_reconstructed(
    buf: &mut Vec<u8>,
    parsed: &[(&str, &str)],
    transform: Option<&LineTransform>,
) -> io::Result<()> {
    let mut first = true;
    match transform {
        None => {
            for (k, v) in parsed {
                plain_field_sep(buf, &mut first);
                buf.extend_from_slice(k.as_bytes());
                buf.push(b'=');
                buf.extend_from_slice(v.as_bytes());
            }
        }
        Some(tf) => {
            for (k, v) in parsed {
                if tf.key_removed(k) {
                    continue;
                }
                plain_field_sep(buf, &mut first);
                buf.extend_from_slice(k.as_bytes());
                buf.push(b'=');
                buf.extend_from_slice(v.as_bytes());
            }
            for (k, v) in &tf.add {
                plain_field_sep(buf, &mut first);
                buf.extend_from_slice(k.as_bytes());
                buf.push(b'=');
                logfmt::write_logfmt_value(buf, v.as_str())?;
            }
        }
    }
    Ok(())
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

/// Colored logfmt line. With `transform`, skips `--rm` keys and appends `--add`
/// pairs without ANSI styling.
pub(crate) fn write_colored_line<W: Write + ?Sized>(
    out: &mut W,
    parsed: &[(&str, &str)],
    transform: Option<&LineTransform>,
) -> io::Result<()> {
    let lvl_sgr = parsed
        .iter()
        .find_map(|(k, v)| {
            if transform.is_some_and(|tf| tf.key_removed(k)) {
                return None;
            }
            (*k == "level").then(|| level_color(v))
        })
        .unwrap_or("");

    let mut first = true;
    for (k, v) in parsed {
        if transform.is_some_and(|tf| tf.key_removed(k)) {
            continue;
        }
        if !first {
            out.write_all(b" ")?;
        }
        first = false;
        write!(out, "{BOLD}{k}{RESET}=")?;
        let color: &str = match *k {
            "time" | "ts" => COL_BLUE,
            "level" => lvl_sgr,
            _ => "",
        };
        write_value(out, v, color, *k == "level")?;
    }
    if let Some(tf) = transform {
        for (k, v) in &tf.add {
            if !first {
                out.write_all(b" ")?;
            }
            first = false;
            write!(out, "{k}=")?;
            logfmt::write_logfmt_value(out, v.as_str())?;
        }
    }
    out.write_all(b"\n")?;
    Ok(())
}

// -------- JSON line output --------

/// One matched line as a JSON object, in logfmt parse order. Logfmt values are
/// unescaped into `scratch` then re-emitted as JSON strings. `--rm` keys are
/// dropped; `--add` pairs are appended.
pub(crate) fn write_json_line<W: Write + ?Sized>(
    out: &mut W,
    parsed: &[(&str, &str)],
    transform: Option<&LineTransform>,
    scratch: &mut String,
) -> io::Result<()> {
    let mut obj = JsonObj::open(out)?;
    for (k, v) in parsed {
        if transform.is_some_and(|tf| tf.key_removed(k)) {
            continue;
        }
        obj.entry_logfmt_value(k, v, scratch)?;
    }
    if let Some(tf) = transform {
        for (k, v) in &tf.add {
            // `--add` values are plaintext (not raw logfmt), so emit directly.
            obj.entry_str(k.as_str(), v.as_str())?;
        }
    }
    obj.finish()?;
    out.write_all(b"\n")
}

// -------- aggregation row output --------

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

/// JSON aggregation row, flat keys: `{"count": N, "key.<k>": "...", ...}`.
pub(crate) fn write_agg_row_json<W: Write + ?Sized>(
    out: &mut W,
    count: u64,
    keys: &[SmartString],
    combo: &[SmartString],
    bucket: Option<(Timestamp, Timestamp)>,
    time_range: Option<(Timestamp, Timestamp)>,
    tz: &jiff::tz::TimeZone,
) -> io::Result<()> {
    let mut obj = JsonObj::open(out)?;
    obj.entry_u64("count", count)?;
    for (k, v) in keys.iter().zip(combo.iter()) {
        obj.entry_str(&format!("key.{k}"), v.as_str())?;
    }
    if let Some((start, end)) = bucket {
        obj.entry_str("bucket.start", &timestamp::format_rfc3339(start, tz))?;
        obj.entry_str("bucket.end", &timestamp::format_rfc3339(end, tz))?;
    }
    if let Some((a, b)) = time_range {
        obj.entry_str("time.start", &timestamp::format_rfc3339(a, tz))?;
        obj.entry_str("time.end", &timestamp::format_rfc3339(b, tz))?;
    }
    obj.finish()?;
    out.write_all(b"\n")
}

/// One-shot `{"count": N}\n` line. Used for the bare `--count` summary
/// under `--json`.
pub(crate) fn write_count_json<W: Write + ?Sized>(out: &mut W, count: u64) -> io::Result<()> {
    let mut obj = JsonObj::open(out)?;
    obj.entry_u64("count", count)?;
    obj.finish()?;
    out.write_all(b"\n")
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

/// `--list-keys` JSON: one JSON array on a single line, sorted.
pub(crate) fn write_string_set_json<W: Write + ?Sized>(
    out: &mut W,
    set: &RapidHashSet<SmartString>,
) -> io::Result<()> {
    let mut arr = JsonArr::open(out)?;
    for v in sorted(set) {
        arr.push_str(v.as_str())?;
    }
    arr.finish()?;
    out.write_all(b"\n")
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

/// `--list-values-for` JSON. Single key → array of strings; multiple keys →
/// array of `{"key": K, "value": V}` objects. One line either way.
pub(crate) fn write_values_summary_json<W: Write + ?Sized>(
    out: &mut W,
    keys: &[SmartString],
    values: &[RapidHashSet<SmartString>],
) -> io::Result<()> {
    if keys.len() == 1 {
        write_string_set_json(out, &values[0])?;
        return Ok(());
    }
    let mut arr = JsonArr::open(out)?;
    for (key, set) in keys.iter().zip(values.iter()) {
        for v in sorted(set) {
            let mut o = arr.start_obj()?;
            o.entry_str("key", key.as_str())?;
            o.entry_str("value", v.as_str())?;
            o.finish()?;
        }
    }
    arr.finish()?;
    out.write_all(b"\n")
}
