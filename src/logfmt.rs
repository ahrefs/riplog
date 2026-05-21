//! Small zero-copy logfmt parser.

// Whitelisted: the parser uses `MaybeUninit` slot writes + `from_utf8_unchecked`
// on validated UTF-8 sub-ranges to keep the per-line hot path allocation- and
// validation-free. Each `unsafe` block carries its own `SAFETY:` comment.
#![allow(unsafe_code)]

use memchr::{memchr, memchr2};
use rapidhash::RapidHashSet;
use smartstring::alias::String as SmartString;
use std::io::{self, Write};
use std::mem::MaybeUninit;

use crate::aggregate::{AggregateSpec, Aggregates};
use crate::output::{is_removed, sorted, OutputFormat};
use crate::timestamp::{self, Timestamp};

/// Fixed-size stack-allocated scratch buffer for [`PairsBuffer::parse`].
///
/// Zero-cost to construct: [`PairsBuffer::new`] doesn't initialize the slots,
/// so there's no `memset` in the hot path.
pub struct PairsBuffer<'a, const N: usize> {
    slots: [MaybeUninit<(&'a str, &'a str)>; N],
}

impl<'a, const N: usize> PairsBuffer<'a, N> {
    /// Construct an uninitialized buffer. No allocation, no memset.
    #[inline]
    pub const fn new() -> Self {
        Self {
            slots: [const { MaybeUninit::uninit() }; N],
        }
    }

    /// Parse `line` into this buffer and return an initialized slice of
    /// `(key, value)` pairs plus a boolean indicating whether additional
    /// pairs were dropped because the buffer was too small.
    ///
    /// Quoted values are returned *including* the surrounding quotes;
    /// call [`unescape_value`] to decode them.
    ///
    /// Only ASCII space (`b' '`) separates pairs (matching brandur's
    /// `logfmt` crate). Callers must strip trailing `\n` / `\r\n` before
    /// invoking — newlines inside the input are treated as part of a token.
    #[inline]
    pub fn parse(&mut self, line: &'a str) -> (&[(&'a str, &'a str)], bool) {
        let (written, overflow) = parse_line_impl(line, &mut self.slots);
        debug_assert!(written <= N);
        // SAFETY: `parse_line_impl` initializes `self.slots[..written]` and
        // guarantees `written <= N` (asserted above in debug). `MaybeUninit<T>`
        // has the same layout and alignment as `T`, so the pointer cast is
        // valid. The output lifetime is bound to `&mut self` via elision, so
        // the caller cannot alias `self.slots` while the slice is live.
        let init: &[(&'a str, &'a str)] = unsafe {
            std::slice::from_raw_parts(self.slots.as_ptr().cast::<(&'a str, &'a str)>(), written)
        };
        (init, overflow)
    }
}

impl<'a, const N: usize> Default for PairsBuffer<'a, N> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn parse_line_impl<'a>(
    line: &'a str,
    out: &mut [MaybeUninit<(&'a str, &'a str)>],
) -> (usize, bool) {
    let cap = out.len();
    let mut written = 0usize;
    let bytes = line.as_bytes();
    let mut i = 0;
    let n = bytes.len();

    macro_rules! emit {
        ($k:expr, $v:expr) => {{
            let k = $k;
            let v = $v;
            if written < cap {
                // SAFETY: `written < cap == out.len()`.
                unsafe {
                    out.get_unchecked_mut(written).write((k, v));
                }
                written += 1;
            } else {
                return (written, true);
            }
        }};
    }

    while i < n {
        // Skip leading spaces (only ASCII space separates, per brandur).
        while i < n && bytes[i] == b' ' {
            i += 1;
        }
        if i >= n {
            break;
        }

        // Skip stray '=' at key position: consume until next space.
        if bytes[i] == b'=' {
            let end = memchr(b' ', &bytes[i..]).map(|p| i + p).unwrap_or(n);
            i = end;
            continue;
        }

        // Find end of key: '=' or space.
        let key_start = i;
        let rel = memchr2(b'=', b' ', &bytes[i..]);
        let (key_end, term) = match rel {
            Some(p) => (i + p, bytes[i + p]),
            None => (n, 0),
        };

        // SAFETY: key_start/key_end fall on ASCII boundaries (we stopped on
        // '=' or space, both single-byte UTF-8), so the slice is valid UTF-8
        // since the input is.
        let key = unsafe { std::str::from_utf8_unchecked(&bytes[key_start..key_end]) };

        if term != b'=' {
            // Bare key, no value.
            if !key.is_empty() {
                emit!(key, "");
            }
            i = key_end;
            continue;
        }

        // Consume '='.
        i = key_end + 1;

        // Value.
        let (val_start, val_end) = if i < n && bytes[i] == b'"' {
            let vs = i;
            i += 1; // past opening quote
            loop {
                match memchr2(b'"', b'\\', &bytes[i..]) {
                    Some(p) => {
                        let idx = i + p;
                        if bytes[idx] == b'"' {
                            i = idx + 1;
                            break (vs, i);
                        } else {
                            // Backslash: skip escape byte (if any).
                            i = (idx + 2).min(n);
                        }
                    }
                    None => {
                        // Unterminated quote: take the rest.
                        i = n;
                        break (vs, n);
                    }
                }
            }
        } else {
            let vs = i;
            let ve = memchr(b' ', &bytes[i..]).map(|p| i + p).unwrap_or(n);
            i = ve;
            (vs, ve)
        };

        // SAFETY: same reasoning as above; all split bytes are ASCII.
        let val = unsafe { std::str::from_utf8_unchecked(&bytes[val_start..val_end]) };

        if !key.is_empty() {
            emit!(key, val);
        }
    }

    (written, false)
}

/// Decode a raw value slice (as produced by [`parse_line`]) into `out`.
///
/// `out` is cleared first. Strips surrounding quotes if present and
/// processes `\"`, `\\`, `\n`, `\r`, `\t`. Unknown escapes drop the
/// backslash and keep the following byte. Invalid UTF-8 is replaced
/// with U+FFFD.
pub fn unescape_value(raw: &[u8], out: &mut String) {
    out.clear();

    // Unquoted values can't contain escapes — ship them as-is.
    if raw.first() != Some(&b'"') {
        push_bytes_lossy(out, raw);
        return;
    }

    // Strip the opening quote, and the matching closing quote if present.
    let raw = if raw.len() >= 2 && raw.last() == Some(&b'"') {
        &raw[1..raw.len() - 1]
    } else {
        &raw[1..]
    };

    // Fast path: no backslash inside the quoted body.
    if memchr(b'\\', raw).is_none() {
        push_bytes_lossy(out, raw);
        return;
    }

    let mut i = 0;
    let n = raw.len();
    while i < n {
        match memchr(b'\\', &raw[i..]) {
            Some(p) => {
                push_bytes_lossy(out, &raw[i..i + p]);
                let esc_at = i + p;
                if esc_at + 1 >= n {
                    // Trailing backslash: drop it.
                    break;
                }
                let c = raw[esc_at + 1];
                match c {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    // Unknown: keep the following byte verbatim.
                    other => push_bytes_lossy(out, &[other]),
                }
                i = esc_at + 2;
            }
            None => {
                push_bytes_lossy(out, &raw[i..]);
                break;
            }
        }
    }
}

fn push_bytes_lossy(out: &mut String, bytes: &[u8]) {
    out.push_str(&String::from_utf8_lossy(bytes));
}

/// Write `value` as a logfmt value to `out`. If `value` is empty, contains
/// any of space, `=`, `"`, or a control character, it is wrapped in double
/// quotes with `"`, `\`, `\n`, `\r`, `\t` escaped. Otherwise the bytes are
/// written verbatim.
///
/// Counterpart to [`unescape_value`]: parsing a key=value pair where the
/// value was emitted by this function and then calling `unescape_value`
/// recovers the original string.
pub fn write_logfmt_value<W: std::io::Write + ?Sized>(
    out: &mut W,
    value: &str,
) -> std::io::Result<()> {
    let bytes = value.as_bytes();
    let needs_quote = bytes.is_empty()
        || bytes
            .iter()
            .any(|&b| matches!(b, b' ' | b'=' | b'"' | b'\\') || b < 0x20);
    if !needs_quote {
        return out.write_all(bytes);
    }
    out.write_all(b"\"")?;
    let mut last = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            _ => continue,
        };
        if last < i {
            out.write_all(&bytes[last..i])?;
        }
        out.write_all(esc)?;
        last = i + 1;
    }
    if last < bytes.len() {
        out.write_all(&bytes[last..])?;
    }
    out.write_all(b"\"")
}

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

// -------- logfmt format (plain + colored, picked via `LogfmtFormat::color`) --------

/// Logfmt formatter. `color: true` enables ANSI styling for the per-line
/// path; the aggregation / summary paths are color-free either way (they
/// emit structured key=value rows that are already easy to parse).
pub(crate) struct LogfmtFormat {
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
#[inline]
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
    #[inline]
    fn line<W: Write + ?Sized>(
        &self,
        w: &mut W,
        pairs: &[&[(&str, &str)]],
        remove: &[&str],
        original_trailer: &[u8],
        _scratch: &mut String,
    ) -> io::Result<()> {
        // For colored output, pick the level colour up front so the bold value
        // styling matches the level value (matters when `level` appears mid-line).
        let lvl_sgr = if self.color {
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
                if self.color {
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
        // Colored output always terminates its own line; plain output
        // preserves the original trailer (e.g. CRLF) when present, falling
        // back to a single `\n` otherwise.
        if self.color || original_trailer.is_empty() {
            w.write_all(b"\n")
        } else {
            w.write_all(original_trailer)
        }
    }

    fn agg_row<W: Write + ?Sized>(
        &self,
        w: &mut W,
        spec: &AggregateSpec,
        aggregates: &mut Aggregates,
        keys: &[SmartString],
        combo: &[SmartString],
        bucket: Option<(Timestamp, Timestamp)>,
        tz: &jiff::tz::TimeZone,
    ) -> io::Result<()> {
        write!(w, "count={}", aggregates.count as u64)?;
        for key in &spec.averages {
            if let Some(v) = aggregates.get_mut(key).and_then(|a| a.average()) {
                write!(w, " avg.{key}={v}")?;
            }
        }
        for (label, p, keys_for) in [
            ("p50", 0.5, &spec.p50s),
            ("p90", 0.9, &spec.p90s),
            ("p99", 0.99, &spec.p99s),
        ] {
            for key in keys_for {
                if let Some(v) = aggregates.get_mut(key).and_then(|a| a.percentile(p)) {
                    write!(w, " {label}.{key}={v}")?;
                }
            }
        }
        for (k, v) in keys.iter().zip(combo.iter()) {
            write!(w, " key.{k}=")?;
            write_logfmt_value(w, v.as_str())?;
        }
        if let Some((start, end)) = bucket {
            write!(w, " bucket.start={}", timestamp::format_rfc3339(start, tz))?;
            write!(w, " bucket.end={}", timestamp::format_rfc3339(end, tz))?;
        }
        if let (Some(a), Some(b)) = (aggregates.min_ts, aggregates.max_ts) {
            write!(w, " time.start={}", timestamp::format_rfc3339(a, tz))?;
            write!(w, " time.end={}", timestamp::format_rfc3339(b, tz))?;
        }
        writeln!(w)
    }

    fn string_set<W: Write + ?Sized>(
        &self,
        w: &mut W,
        set: &RapidHashSet<SmartString>,
    ) -> io::Result<()> {
        for v in sorted(set) {
            writeln!(w, "{v}")?;
        }
        Ok(())
    }

    fn values_summary<W: Write + ?Sized>(
        &self,
        w: &mut W,
        keys: &[SmartString],
        values: &[RapidHashSet<SmartString>],
    ) -> io::Result<()> {
        let multi = keys.len() > 1;
        for (key, set) in keys.iter().zip(values.iter()) {
            if multi {
                writeln!(w, "# {key}")?;
            }
            self.string_set(w, set)?;
        }
        Ok(())
    }

    fn count_only<W: Write + ?Sized>(&self, w: &mut W, n: u64) -> io::Result<()> {
        writeln!(w, "{n}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::strategy::Strategy;

    fn parse(s: &str) -> Vec<(String, String)> {
        let mut buf = PairsBuffer::<32>::new();
        let (pairs, overflow) = buf.parse(s);
        assert!(!overflow, "test buffer too small");
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn overflow_reported() {
        let mut buf = PairsBuffer::<2>::new();
        let (pairs, overflow) = buf.parse("a=1 b=2 c=3 d=4");
        assert!(overflow);
        assert_eq!(pairs, &[("a", "1"), ("b", "2")]);
    }

    #[test]
    fn plain_pairs() {
        assert_eq!(
            parse("a=1 b=2"),
            vec![("a".into(), "1".into()), ("b".into(), "2".into())]
        );
    }

    #[test]
    fn quoted_value() {
        assert_eq!(
            parse(r#"msg="hello world""#),
            vec![("msg".into(), r#""hello world""#.into())]
        );
        let mut s = String::new();
        unescape_value(br#""hello world""#, &mut s);
        assert_eq!(s, "hello world");
    }

    #[test]
    fn escaped_quote() {
        let mut s = String::new();
        unescape_value(br#""he said \"hi\"""#, &mut s);
        assert_eq!(s, r#"he said "hi""#);
    }

    #[test]
    fn escapes_nrt() {
        let mut s = String::new();
        unescape_value(br#""a\nb\tc\\d""#, &mut s);
        assert_eq!(s, "a\nb\tc\\d");
    }

    #[test]
    fn unterminated_quote() {
        let got = parse(r#"msg="oops"#);
        assert_eq!(got, vec![("msg".into(), r#""oops"#.into())]);
    }

    #[test]
    fn bare_key() {
        assert_eq!(
            parse("flag other=x"),
            vec![("flag".into(), "".into()), ("other".into(), "x".into())]
        );
    }

    #[test]
    fn sample_line() {
        let line = r#"time=2026-04-24T18:09:03.0696 level=info facil=signal pid=543563 msg="Signal handler done.""#;
        let got = parse(line);
        assert_eq!(got.len(), 5);
        assert_eq!(got[0].0, "time");
        assert_eq!(got[0].1, "2026-04-24T18:09:03.0696");
        assert_eq!(got[4].0, "msg");
        assert_eq!(got[4].1, r#""Signal handler done.""#);
        let mut s = String::new();
        unescape_value(got[4].1.as_bytes(), &mut s);
        assert_eq!(s, "Signal handler done.");
    }

    #[test]
    fn stray_equals() {
        // Stray `=value` should be skipped.
        assert_eq!(parse("=junk a=1"), vec![("a".into(), "1".into())]);
    }

    #[test]
    fn empty_value() {
        assert_eq!(
            parse("a= b=2"),
            vec![("a".into(), "".into()), ("b".into(), "2".into())]
        );
    }

    #[test]
    fn rfc3339_dates() {
        // Unquoted RFC 3339 (with timezone).
        let got = parse("ts=2026-04-24T18:09:03.0696Z next=1");
        assert_eq!(
            got,
            vec![
                ("ts".into(), "2026-04-24T18:09:03.0696Z".into()),
                ("next".into(), "1".into()),
            ]
        );
        let mut s = String::new();
        unescape_value(got[0].1.as_bytes(), &mut s);
        assert_eq!(s, "2026-04-24T18:09:03.0696Z");

        // With numeric offset, unquoted.
        let got = parse("ts=2026-04-24T18:09:03+02:00");
        assert_eq!(got, vec![("ts".into(), "2026-04-24T18:09:03+02:00".into())]);

        // Quoted RFC 3339 — value includes the surrounding quotes.
        let got = parse(r#"ts="2026-04-24T18:09:03.0696Z""#);
        assert_eq!(
            got,
            vec![("ts".into(), r#""2026-04-24T18:09:03.0696Z""#.into())]
        );
        let mut s = String::new();
        unescape_value(got[0].1.as_bytes(), &mut s);
        assert_eq!(s, "2026-04-24T18:09:03.0696Z");
    }

    #[test]
    fn utf8_emojis() {
        // Unquoted: emojis are multi-byte UTF-8 but contain no whitespace.
        let got = parse("who=🦀 mood=🔥🎉");
        assert_eq!(
            got,
            vec![("who".into(), "🦀".into()), ("mood".into(), "🔥🎉".into())]
        );
        // Quoted emoji string with spaces.
        let got = parse(r#"msg="hello 🌍 from 🦀 rust!""#);
        assert_eq!(
            got,
            vec![("msg".into(), r#""hello 🌍 from 🦀 rust!""#.into())]
        );
        let mut s = String::new();
        unescape_value(got[0].1.as_bytes(), &mut s);
        assert_eq!(s, "hello 🌍 from 🦀 rust!");
    }

    /// Run our fast parser + `unescape_value` and produce a list shaped like
    /// brandur's `Vec<Pair>` output, so the two can be compared directly.
    fn fast_decoded(line: &str) -> (Vec<(String, String)>, bool) {
        let mut buf = PairsBuffer::<256>::new();
        let (pairs, overflow) = buf.parse(line);
        let mut out = Vec::with_capacity(pairs.len());
        let mut tmp = String::new();
        for (k, v) in pairs {
            tmp.clear();
            unescape_value(v.as_bytes(), &mut tmp);
            out.push((k.to_string(), tmp.clone()));
        }
        (out, overflow)
    }

    /// Brandur's `logfmt::parse`, normalized to `(key, value)` with
    /// `None` collapsed to `""` so it lines up with our representation.
    fn brandur_decoded(line: &str) -> Vec<(String, String)> {
        logfmt::parse(line)
            .into_iter()
            // brandur emits a trailing empty-key pair on EOF in many cases
            // (e.g. "", " ", "= "); our parser drops empty keys. Filter
            // them out so the two views line up.
            .filter(|p| !p.key.is_empty())
            .map(|p| (p.key, p.val.unwrap_or_default()))
            .collect()
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(2048))]

        /// Our fast parser (+ `unescape_value`) must agree with brandur's
        /// `logfmt` crate on well-formed inputs.
        ///
        /// Inputs are generated as a sequence of well-formed pairs joined by
        /// spaces. Constraints (so `unescape_value` and brandur stay in sync,
        /// and so brandur's stateful character-by-character behavior on
        /// malformed input doesn't bite us):
        ///   - no '\n' anywhere (caller invariant: line endings are stripped);
        ///   - keys and unquoted values exclude ' ', '=', '"', '\';
        ///   - inside quoted strings, the only escape is `\"` — no `\\`,
        ///     `\n`, `\r`, `\t`, etc. (brandur passes those through verbatim
        ///     while our `unescape_value` decodes them).
        ///
        /// `\r` and `\t` are exercised inside keys/values to confirm they're
        /// treated as ordinary token characters.
        #[test]
        fn fast_matches_brandur(
            pairs in proptest::collection::vec(
                proptest::prop_oneof![
                    // bare key
                    "[a-zA-Z0-9_.\\-\r\t]{1,8}",
                    // key=unquoted_value (non-empty value — brandur's `key=`
                    // form absorbs the next token as the value).
                    ("[a-zA-Z0-9_.\\-\r\t]{1,8}", "[a-zA-Z0-9_.\\-:/\r\t]{1,8}")
                        .prop_map(|(k, v)| format!("{k}={v}")),
                    // key="quoted no escapes" (non-empty body — brandur
                    // collapses empty quoted values into the next token).
                    ("[a-zA-Z0-9_.\\-\r\t]{1,8}", "[^\"\\\\\n]{1,12}")
                        .prop_map(|(k, v)| format!("{k}=\"{v}\"")),
                    // key="quoted with one \" escape"
                    (
                        "[a-zA-Z0-9_.\\-\r\t]{1,8}",
                        "[^\"\\\\\n]{1,4}",
                        "[^\"\\\\\n]{1,4}",
                    )
                        .prop_map(|(k, a, b)| format!(r#"{k}="{a}\"{b}""#)),
                ],
                0..64,
            ),
        ) {
            let line: String = pairs.as_slice().join(" ");
            let expected = brandur_decoded(&line);
            let (got, overflow) = fast_decoded(&line);
            let take = expected.len().min(256);

            if expected.len() <= 256 {
                proptest::prop_assert!(!overflow);
                proptest::prop_assert_eq!(got.len(), expected.len());
            } else {
                proptest::prop_assert!(overflow);
                proptest::prop_assert_eq!(got.len(), 256);
            }
            proptest::prop_assert_eq!(&got[..take], &expected[..take]);
        }
    }

    fn write_value(s: &str) -> String {
        let mut out = Vec::new();
        write_logfmt_value(&mut out, s).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn write_logfmt_value_unquoted_when_safe() {
        assert_eq!(write_value("info"), "info");
        assert_eq!(write_value("level=error"), "\"level=error\"");
        assert_eq!(write_value("hello world"), "\"hello world\"");
        assert_eq!(write_value(""), "\"\"");
        assert_eq!(write_value("a\"b"), "\"a\\\"b\"");
        assert_eq!(write_value("a\\b"), "\"a\\\\b\"");
        assert_eq!(write_value("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn write_logfmt_value_round_trips() {
        // Emit, parse back, unescape — must yield the original string.
        for original in [
            "info",
            "hello world",
            "level=error",
            "msg with \"quotes\"",
            "back\\slash",
            "tab\there",
            "",
        ] {
            let mut emitted = Vec::new();
            write_logfmt_value(&mut emitted, original).unwrap();
            let line = format!("k={}", String::from_utf8(emitted).unwrap());
            let pairs = parse(&line);
            assert_eq!(pairs.len(), 1);
            let mut decoded = String::new();
            unescape_value(pairs[0].1.as_bytes(), &mut decoded);
            assert_eq!(decoded, original, "round-trip failed for {original:?}");
        }
    }

    #[test]
    fn literal_backslash_n() {
        // The source contains a backslash followed by an 'n' — not a real LF.
        // `r#"..."#` ensures no Rust-level escaping.
        let got = parse(r#"msg="line1\nline2""#);
        assert_eq!(got, vec![("msg".into(), r#""line1\nline2""#.into())]);
        // Unescaping turns the two-byte `\n` sequence into an actual newline.
        let mut s = String::new();
        unescape_value(got[0].1.as_bytes(), &mut s);
        assert_eq!(s, "line1\nline2");
        assert!(s.contains('\n'));
        assert_eq!(s.len(), "line1".len() + 1 + "line2".len());
    }
}
