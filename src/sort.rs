//! In-memory sort buffer for `--sort-by=KEY`. Captures matched-line output,
//! emits sorted by the value of a named field at end. Trades memory for
//! ordered output; callers are expected to narrow with `--if` first.

use smartstring::alias::String as SmartString;
use std::io::Write;

use crate::run::unescape_for_key;

pub struct SortBuffer {
    key: SmartString,
    rows: Vec<(SmartString, Box<[u8]>)>,
    scratch_bytes: Vec<u8>,
    scratch_str: String,
}

impl SortBuffer {
    pub fn new(key: &str) -> Self {
        Self {
            key: SmartString::from(key),
            rows: Vec::new(),
            scratch_bytes: Vec::new(),
            scratch_str: String::new(),
        }
    }

    /// Drive `fill` against a reusable scratch buffer; if it produced bytes,
    /// store them keyed by the value of `self.key` from `pairs`. The scratch
    /// keeps its capacity across calls, so the per-line cost is one `Box`
    /// allocation rather than `Vec` growth + shrink-to-fit.
    pub fn capture<F>(&mut self, pairs: &[(&str, &str)], fill: F) -> std::io::Result<()>
    where
        F: FnOnce(&mut Vec<u8>) -> std::io::Result<()>,
    {
        self.scratch_bytes.clear();
        fill(&mut self.scratch_bytes)?;
        if self.scratch_bytes.is_empty() {
            return Ok(());
        }
        let bytes: Box<[u8]> = Box::from(self.scratch_bytes.as_slice());
        let mut key_val = SmartString::new_const();
        if let Some(v) = unescape_for_key(pairs, &self.key, &mut self.scratch_str) {
            key_val.push_str(v);
        }
        self.rows.push((key_val, bytes));
        Ok(())
    }

    pub fn emit<W: Write>(mut self, out: &mut W) -> std::io::Result<()> {
        self.rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (_, bytes) in &self.rows {
            out.write_all(bytes)?;
        }
        Ok(())
    }
}
