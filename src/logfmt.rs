//! Small zero-copy logfmt parser.

// Whitelisted: the parser uses `MaybeUninit` slot writes + `from_utf8_unchecked`
// on validated UTF-8 sub-ranges to keep the per-line hot path allocation- and
// validation-free. Each `unsafe` block carries its own `SAFETY:` comment.
#![allow(unsafe_code)]

use memchr::{memchr, memchr2};
use std::mem::MaybeUninit;

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
