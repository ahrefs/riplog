//! `--add` / `--rm` line mutations and a reusable `Vec<u8>` scratch for
//! reconstructed output. The actual emission lives in `output`.

use rapidhash::RapidHashSet;
use smartstring::alias::String as SmartString;
use std::io::Write;

use crate::cli::Cli;

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

/// Reusable buffers for reconstructed line emission. `buf` collects bytes
/// for the logfmt slow path; `str_buf` is the unescape scratch used by the
/// JSON emitter.
#[derive(Debug, Default)]
pub(crate) struct EmitScratch {
    buf: Vec<u8>,
    pub(crate) str_buf: String,
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
        if cli.raw_key.as_deref() == Some(key) {
            anyhow::bail!("`--add` key `{key}` conflicts with `--raw-key`");
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
