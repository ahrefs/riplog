//! Small zero-copy logfmt parser.

use memchr::{memchr, memchr2, memchr3};

/// Parse a single logfmt line into `(key, value)` slices of the input.
///
/// Fills `out` in order. Returns `(written, overflow)` — the number of pairs
/// actually stored (≤ `out.len()`) and whether some pairs remain.
/// Quoted values are returned *including* the
/// surrounding quotes; call [`unescape_value`] to decode them. Malformed
/// input is consumed best-effort.
pub fn parse_line<'a>(line: &'a str, out: &mut [(&'a str, &'a str)]) -> (usize, bool) {
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
                out[written] = (k, v);
                written += 1;
            } else {
                return (written, true);
            }
        }};
    }

    while i < n {
        // Skip whitespace (including \n, \r from BufRead).
        while i < n {
            let b = bytes[i];
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                i += 1;
            } else {
                break;
            }
        }
        if i >= n {
            break;
        }

        // Skip stray '=' at key position.
        if bytes[i] == b'=' {
            // Consume until whitespace.
            let end = memchr3(b' ', b'\t', b'\n', &bytes[i..])
                .map(|p| i + p)
                .unwrap_or(n);
            i = end;
            continue;
        }

        // Find end of key: '=', whitespace, or EOL.
        let key_start = i;
        let rel = memchr3(b'=', b' ', b'\t', &bytes[i..]);
        let (key_end, term) = match rel {
            Some(p) => (i + p, bytes[i + p]),
            None => (n, 0),
        };
        // Also stop at \n / \r (rare; handle explicitly).
        let (key_end, term) = {
            let mut ke = key_end;
            let mut t = term;
            if let Some(p) = memchr2(b'\n', b'\r', &bytes[key_start..key_end]) {
                ke = key_start + p;
                t = bytes[ke];
            }
            (ke, t)
        };

        // SAFETY: key_start/key_end fall on ASCII boundaries (we stopped on
        // '=', space, tab, \n, or \r, all single-byte UTF-8), so the slice is
        // valid UTF-8 since the input is.
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
            let ve = memchr3(b' ', b'\t', b'\n', &bytes[i..])
                .map(|p| i + p)
                .unwrap_or(n);
            // Also stop at \r.
            let ve = memchr(b'\r', &bytes[vs..ve]).map(|p| vs + p).unwrap_or(ve);
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

    let raw = if raw.len() >= 2 && raw.first() == Some(&b'"') && raw.last() == Some(&b'"') {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };

    // Fast path: no backslash.
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

#[allow(dead_code)]
fn push_bytes_lossy(out: &mut String, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(s) => out.push_str(s),
        Err(_) => out.push_str(&String::from_utf8_lossy(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Vec<(String, String)> {
        let mut buf: [(&str, &str); 32] = [("", ""); 32];
        let (n, overflow) = parse_line(s, &mut buf);
        assert_eq!(overflow, false, "test buffer too small");
        buf[..n]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn overflow_reported() {
        let mut buf: [(&str, &str); 2] = [("", ""); 2];
        let (n, overflow) = parse_line("a=1 b=2 c=3 d=4", &mut buf);
        assert_eq!(n, 2);
        assert_eq!(overflow, true);
        assert_eq!(buf[0], ("a", "1"));
        assert_eq!(buf[1], ("b", "2"));
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
    fn trailing_newline() {
        assert_eq!(parse("a=1 b=2\n"), parse("a=1 b=2"));
        assert_eq!(parse("a=1 b=2\r\n"), parse("a=1 b=2"));
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
}
