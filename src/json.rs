//! Small streaming JSON emitter used by `--json` output. Delegates string
//! escaping and number formatting to `serde_json`; provides `JsonObj` /
//! `JsonArr` builders so callers can interleave control flow (e.g.
//! `--rm`/`--add`, optional fields) without materialising a `serde_json::Value`.

use rapidhash::RapidHashSet;
use smartstring::alias::String as SmartString;
use std::io::{self, Write};

use crate::logfmt;
use crate::output::{is_removed, sorted, OutputFormat};
use crate::timestamp::{self, Timestamp};

#[inline]
fn jerr(e: serde_json::Error) -> io::Error {
    io::Error::other(e)
}

/// Largest integer that round-trips through an IEEE 754 double — the type
/// most JSON parsers (JavaScript's `JSON.parse`, jq, Python's `json` with
/// default float parsing) use for numbers. Values above this silently lose
/// precision on the consumer side, so we emit them as JSON strings instead.
pub(crate) const MAX_SAFE_JSON_INTEGER: u64 = (1u64 << 53) - 1;

/// Write `s` as a JSON string (quoted, properly escaped) via serde_json.
#[inline]
pub fn write_json_string<W: Write + ?Sized>(out: &mut W, s: &str) -> io::Result<()> {
    serde_json::to_writer(out, s).map_err(jerr)
}

/// Unescape a raw logfmt value (which may be surrounded by quotes and contain
/// `\"` / `\\` / `\n` / `\r` / `\t`) into `scratch`, then emit as a JSON string.
#[inline]
pub fn write_json_decoded_logfmt_value<W: Write + ?Sized>(
    out: &mut W,
    raw: &str,
    scratch: &mut String,
) -> io::Result<()> {
    logfmt::unescape_value(raw.as_bytes(), scratch);
    write_json_string(out, scratch.as_str())
}

/// Streaming JSON object writer. Borrows a `&mut W` for its lifetime, tracks
/// comma separators between entries, and closes on `.finish()`.
pub struct JsonObj<'w, W: Write + ?Sized> {
    out: &'w mut W,
    first: bool,
}

impl<'w, W: Write + ?Sized> JsonObj<'w, W> {
    pub fn open(out: &'w mut W) -> io::Result<Self> {
        out.write_all(b"{")?;
        Ok(Self { out, first: true })
    }

    #[inline]
    fn sep(&mut self) -> io::Result<()> {
        if !self.first {
            self.out.write_all(b",")?;
        }
        self.first = false;
        Ok(())
    }

    #[inline]
    fn write_key(&mut self, k: &str) -> io::Result<()> {
        self.sep()?;
        write_json_string(self.out, k)?;
        self.out.write_all(b":")
    }

    pub fn entry_str(&mut self, k: &str, v: &str) -> io::Result<()> {
        self.write_key(k)?;
        write_json_string(self.out, v)
    }

    pub fn entry_u64(&mut self, k: &str, n: u64) -> io::Result<()> {
        self.write_key(k)?;
        if n <= MAX_SAFE_JSON_INTEGER {
            serde_json::to_writer(&mut *self.out, &n).map_err(jerr)
        } else {
            // Switch to a string so the consumer doesn't truncate.
            write_json_string(self.out, &n.to_string())
        }
    }

    /// Emit `"k": "<decoded value>"`, unescaping the raw logfmt value first.
    pub fn entry_logfmt_value(
        &mut self,
        k: &str,
        raw: &str,
        scratch: &mut String,
    ) -> io::Result<()> {
        self.write_key(k)?;
        write_json_decoded_logfmt_value(self.out, raw, scratch)
    }

    /// Begin a nested object value for key `k`. Caller drives it through
    /// the returned `JsonObj` and calls `.finish()` when done.
    pub fn start_obj<'a>(&'a mut self, k: &str) -> io::Result<JsonObj<'a, W>> {
        self.write_key(k)?;
        JsonObj::open(self.out)
    }

    pub fn finish(self) -> io::Result<()> {
        self.out.write_all(b"}")
    }
}

/// Streaming JSON array writer. Mirrors `JsonObj`. Supports nested objects
/// via `start_obj`, which re-borrows the underlying writer.
pub struct JsonArr<'w, W: Write + ?Sized> {
    out: &'w mut W,
    first: bool,
}

impl<'w, W: Write + ?Sized> JsonArr<'w, W> {
    pub fn open(out: &'w mut W) -> io::Result<Self> {
        out.write_all(b"[")?;
        Ok(Self { out, first: true })
    }

    #[inline]
    fn sep(&mut self) -> io::Result<()> {
        if !self.first {
            self.out.write_all(b",")?;
        }
        self.first = false;
        Ok(())
    }

    pub fn push_str(&mut self, v: &str) -> io::Result<()> {
        self.sep()?;
        write_json_string(self.out, v)
    }

    /// Begin a nested object inside the array; the caller drives it through
    /// the returned `JsonObj` and calls `.finish()` when done.
    pub fn start_obj<'a>(&'a mut self) -> io::Result<JsonObj<'a, W>> {
        self.sep()?;
        JsonObj::open(self.out)
    }

    pub fn finish(self) -> io::Result<()> {
        self.out.write_all(b"]")
    }
}

// -------- jsonl format --------

pub(crate) struct JsonlFormat;

impl OutputFormat for JsonlFormat {
    #[inline]
    fn line<W: Write + ?Sized>(
        &self,
        w: &mut W,
        pairs: &[&[(&str, &str)]],
        remove: &[&str],
        _original_trailer: &[u8],
        scratch: &mut String,
    ) -> io::Result<()> {
        let mut obj = JsonObj::open(w)?;
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

    fn agg_row<W: Write + ?Sized>(
        &self,
        w: &mut W,
        count: u64,
        keys: &[SmartString],
        combo: &[SmartString],
        bucket: Option<(Timestamp, Timestamp)>,
        time_range: Option<(Timestamp, Timestamp)>,
        tz: &jiff::tz::TimeZone,
    ) -> io::Result<()> {
        let mut obj = JsonObj::open(w)?;
        obj.entry_u64("count", count)?;
        if !keys.is_empty() {
            let mut k_obj = obj.start_obj("keys")?;
            for (k, v) in keys.iter().zip(combo.iter()) {
                k_obj.entry_str(k.as_str(), v.as_str())?;
            }
            k_obj.finish()?;
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
        w.write_all(b"\n")
    }

    fn string_set<W: Write + ?Sized>(
        &self,
        w: &mut W,
        set: &RapidHashSet<SmartString>,
    ) -> io::Result<()> {
        let mut arr = JsonArr::open(w)?;
        for v in sorted(set) {
            arr.push_str(v.as_str())?;
        }
        arr.finish()?;
        w.write_all(b"\n")
    }

    fn values_summary<W: Write + ?Sized>(
        &self,
        w: &mut W,
        keys: &[SmartString],
        values: &[RapidHashSet<SmartString>],
    ) -> io::Result<()> {
        // Single key: shape matches `--list-keys` (one JSON array).
        // Multiple keys: array of `{"key": K, "value": V}` so the consumer
        // doesn't have to disambiguate by position.
        if keys.len() == 1 {
            return self.string_set(w, &values[0]);
        }
        let mut arr = JsonArr::open(w)?;
        for (key, set) in keys.iter().zip(values.iter()) {
            for v in sorted(set) {
                let mut o = arr.start_obj()?;
                o.entry_str("key", key.as_str())?;
                o.entry_str("value", v.as_str())?;
                o.finish()?;
            }
        }
        arr.finish()?;
        w.write_all(b"\n")
    }

    fn count_only<W: Write + ?Sized>(&self, w: &mut W, n: u64) -> io::Result<()> {
        let mut obj = JsonObj::open(w)?;
        obj.entry_u64("count", n)?;
        obj.finish()?;
        w.write_all(b"\n")
    }
}

// -------- tests --------

#[cfg(test)]
mod tests {
    use super::*;

    fn s<F: FnOnce(&mut Vec<u8>) -> io::Result<()>>(f: F) -> String {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn json_string_escapes() {
        assert_eq!(s(|b| write_json_string(b, "hello")), r#""hello""#);
        assert_eq!(s(|b| write_json_string(b, "a\"b")), r#""a\"b""#);
        assert_eq!(s(|b| write_json_string(b, "a\\b")), r#""a\\b""#);
        assert_eq!(s(|b| write_json_string(b, "a\nb")), r#""a\nb""#);
        assert_eq!(s(|b| write_json_string(b, "a\tb")), r#""a\tb""#);
        assert_eq!(s(|b| write_json_string(b, "")), r#""""#);
    }

    #[test]
    fn json_string_control_chars() {
        // 0x01 is a control char; serde_json escapes it as ``.
        let got = s(|b| write_json_string(b, "\x01"));
        assert_eq!(got, r#""\u0001""#);
    }

    #[test]
    fn json_string_utf8_passthrough() {
        assert_eq!(s(|b| write_json_string(b, "🦀")), "\"🦀\"");
        assert_eq!(s(|b| write_json_string(b, "héllo")), "\"héllo\"");
    }

    #[test]
    fn decoded_logfmt_value_quoted() {
        // Quoted logfmt value with escapes: should be decoded and re-emitted.
        let mut scratch = String::new();
        assert_eq!(
            s(|b| write_json_decoded_logfmt_value(b, r#""hello\nworld""#, &mut scratch)),
            r#""hello\nworld""#
        );
    }

    #[test]
    fn decoded_logfmt_value_unquoted() {
        let mut scratch = String::new();
        assert_eq!(
            s(|b| write_json_decoded_logfmt_value(b, "info", &mut scratch)),
            r#""info""#
        );
    }

    #[test]
    fn obj_simple() {
        let got = s(|b| {
            let mut o = JsonObj::open(b)?;
            o.entry_str("k", "v")?;
            o.entry_u64("n", 42)?;
            o.finish()
        });
        assert_eq!(got, r#"{"k":"v","n":42}"#);
    }

    #[test]
    fn entry_u64_emits_number_at_safe_boundary() {
        let got = s(|b| {
            let mut o = JsonObj::open(b)?;
            o.entry_u64("n", MAX_SAFE_JSON_INTEGER)?;
            o.finish()
        });
        assert_eq!(got, r#"{"n":9007199254740991}"#);
    }

    #[test]
    fn entry_u64_stringifies_above_safe_boundary() {
        let got = s(|b| {
            let mut o = JsonObj::open(b)?;
            o.entry_u64("n", MAX_SAFE_JSON_INTEGER + 1)?;
            o.finish()
        });
        assert_eq!(got, r#"{"n":"9007199254740992"}"#);
    }

    #[test]
    fn entry_u64_stringifies_u64_max() {
        let got = s(|b| {
            let mut o = JsonObj::open(b)?;
            o.entry_u64("n", u64::MAX)?;
            o.finish()
        });
        assert_eq!(got, r#"{"n":"18446744073709551615"}"#);
    }

    #[test]
    fn obj_nested() {
        let got = s(|b| {
            let mut o = JsonObj::open(b)?;
            o.entry_u64("a", 1)?;
            {
                let mut sub = o.start_obj("sub")?;
                sub.entry_str("x", "y")?;
                sub.finish()?;
            }
            o.finish()
        });
        assert_eq!(got, r#"{"a":1,"sub":{"x":"y"}}"#);
    }

    #[test]
    fn obj_empty() {
        let got = s(|b| JsonObj::open(b)?.finish());
        assert_eq!(got, "{}");
    }

    #[test]
    fn arr_strings() {
        let got = s(|b| {
            let mut a = JsonArr::open(b)?;
            a.push_str("x")?;
            a.push_str("y")?;
            a.finish()
        });
        assert_eq!(got, r#"["x","y"]"#);
    }

    #[test]
    fn arr_of_objects() {
        let got = s(|b| {
            let mut a = JsonArr::open(b)?;
            {
                let mut o = a.start_obj()?;
                o.entry_str("key", "level")?;
                o.entry_str("value", "info")?;
                o.finish()?;
            }
            {
                let mut o = a.start_obj()?;
                o.entry_str("key", "facil")?;
                o.entry_str("value", "net")?;
                o.finish()?;
            }
            a.finish()
        });
        assert_eq!(
            got,
            r#"[{"key":"level","value":"info"},{"key":"facil","value":"net"}]"#
        );
    }
}
