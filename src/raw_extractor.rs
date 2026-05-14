//! Per-line value extractor used by `--raw-key`, plus the small
//! `unescape_for_key` helper shared with the counter / value-gather paths.

use smartstring::alias::String as SmartString;
use std::io::Write;

use crate::logfmt;

/// Find the first pair with key `key`, unescape its value into `scratch`,
/// and return a borrow of the unescaped string. Returns `None` if no pair
/// matches; in that case `scratch` is unspecified.
pub(crate) fn unescape_for_key<'s>(
    pairs: &[(&str, &str)],
    key: &str,
    scratch: &'s mut String,
) -> Option<&'s str> {
    for (pk, pv) in pairs {
        if *pk == key {
            scratch.clear();
            logfmt::unescape_value(pv.as_bytes(), scratch);
            return Some(scratch.as_str());
        }
    }
    None
}

/// Per-line value extractor: for each matched line, emit the unquoted,
/// unescaped value of `key`. Active when `--raw-key=<key>` is given;
/// suppresses the normal full-line output. Lines lacking the key are
/// silently skipped.
pub(crate) struct RawExtractor {
    pub(crate) raw_key: Option<SmartString>,
    scratch: String,
}

impl RawExtractor {
    pub(crate) fn new(raw_key: Option<&str>) -> Self {
        Self {
            raw_key: raw_key.map(SmartString::from),
            scratch: String::new(),
        }
    }

    pub(crate) fn emit<W: Write + ?Sized>(
        &mut self,
        pairs: &[(&str, &str)],
        out: &mut W,
    ) -> std::io::Result<()> {
        let Some(key) = self.raw_key.as_deref() else {
            return Ok(());
        };
        if let Some(v) = unescape_for_key(pairs, key, &mut self.scratch) {
            out.write_all(v.as_bytes())?;
            out.write_all(b"\n")?;
        }
        Ok(())
    }
}
