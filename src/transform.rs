//! `--add` / `--rm` line mutations, a reusable `Vec<u8>` scratch for
//! reconstructed output, and helpers for plain or colored emission.

use rapidhash::RapidHashSet;
use smartstring::alias::String as SmartString;
use std::io::Write;

use crate::cli::Cli;
use crate::logfmt;

// ANSI escapes for colorized output.
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const COL_BLUE: &str = "\x1b[34m";
const COL_RED: &str = "\x1b[31m";
const COL_YELLOW: &str = "\x1b[33m";
const COL_GRAY: &str = "\x1b[90m";
const COL_QUOTE: &str = "\x1b[1;34m";
// Bright white on red background — used for `critical`/`crit` so it really pops.
const COL_CRIT: &str = "\x1b[97;41m";

/// Output mutation from `--add` / `--rm`: removals applied first, then
/// appends (plain logfmt, not ANSI-colored).
#[derive(Clone, Debug)]
pub(crate) struct LineTransform {
    pub remove: RapidHashSet<SmartString>,
    pub add: Vec<(SmartString, SmartString)>,
}

impl LineTransform {
    #[inline]
    pub(crate) fn key_removed(&self, k: &str) -> bool {
        self.remove.contains(k)
    }
}

/// Reusable buffer for reconstructed lines (everything except the memcpy fast path).
#[derive(Debug, Default)]
pub(crate) struct EmitScratch {
    buf: Vec<u8>,
}

impl EmitScratch {
    pub(crate) fn write_slow<W: Write + ?Sized, F>(
        &mut self,
        out: &mut W,
        f: F,
    ) -> std::io::Result<()>
    where
        F: FnOnce(&mut Vec<u8>) -> std::io::Result<()>,
    {
        self.buf.clear();
        f(&mut self.buf)?;
        out.write_all(&self.buf)?;
        Ok(())
    }
}

pub(crate) fn parse_line_transform(cli: &Cli) -> anyhow::Result<Option<LineTransform>> {
    if cli.add.is_empty() && cli.rm.is_empty() {
        return Ok(None);
    }
    let mut remove = RapidHashSet::default();
    for s in &cli.rm {
        let k = SmartString::from(s.trim());
        if k.is_empty() {
            anyhow::bail!("--rm: empty key");
        }
        remove.insert(k);
    }
    let mut add = Vec::new();
    for s in &cli.add {
        let s = s.trim();
        let eq = s
            .find('=')
            .ok_or_else(|| anyhow::anyhow!("--add expects key=value, got `{s}`"))?;
        let (key, rest) = s.split_at(eq);
        let key = key.trim();
        let val = rest[1..].trim();
        if key.is_empty() {
            anyhow::bail!("--add: empty key in `{s}`");
        }
        add.push((SmartString::from(key), SmartString::from(val)));
    }
    Ok(Some(LineTransform { remove, add }))
}

fn rm_key_conflicts(cli: &Cli, ks: &str) -> anyhow::Result<()> {
    if cli.group_by.iter().any(|g| g == ks) {
        anyhow::bail!(
            "`--rm` key `{ks}` conflicts with `--group-by` (removed keys are unsupported for grouping)"
        );
    }
    if cli.sort_by.as_deref() == Some(ks) {
        anyhow::bail!(
            "`--rm` key `{ks}` conflicts with `--sort-by` (removed keys cannot be used for sorting)"
        );
    }
    if cli.list_values_for.iter().any(|x| x == ks) {
        anyhow::bail!("`--rm` key `{ks}` conflicts with `--list-values-for`");
    }
    if cli.raw_key.as_deref() == Some(ks) {
        anyhow::bail!("`--rm` key `{ks}` conflicts with `--raw-key`");
    }
    Ok(())
}

/// Check that `--rm` keys do not conflict with other features.
pub(crate) fn validate_rm_vs_features(
    cli: &Cli,
    remove: &RapidHashSet<SmartString>,
) -> anyhow::Result<()> {
    for k in remove {
        rm_key_conflicts(cli, k.as_str())?;
    }
    Ok(())
}

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
) -> std::io::Result<()> {
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
fn write_value<W: Write + ?Sized>(
    out: &mut W,
    v: &str,
    color: &str,
    bold: bool,
) -> std::io::Result<()> {
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
) -> std::io::Result<()> {
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
