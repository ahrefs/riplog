//! `--add` / `--rm` line mutations and reusable scratch buffers for
//! reconstructed output. The actual emission lives in `output`.

use rapidhash::RapidHashSet;
use smartstring::alias::String as SmartString;

use crate::cli::Cli;
use crate::logfmt;

/// Output mutation from `--add` / `--rm`: removals applied first, then
/// appends. `add` values are stored **pre-escaped** in logfmt form so the
/// per-line emitters can treat user-supplied `--add` pairs uniformly with
/// the original parsed pairs.
#[derive(Clone, Debug)]
pub(crate) struct LineTransform {
    pub remove: RapidHashSet<SmartString>,
    /// `--add` pairs with the value re-encoded in logfmt form (e.g. quoted
    /// and escaped when needed). The `output::OutputFormat` impls read this.
    pub add_escaped: Vec<(SmartString, SmartString)>,
}

/// Reusable buffers for reconstructed line emission. `buf` collects the
/// fully-formatted line bytes so the slow path emits one `write_all` per
/// line (atomic w.r.t. the parallel-worker `UnorderedSink`, which flushes
/// at byte-count thresholds and would otherwise interleave fragments of
/// concurrent lines). `str_buf` is the unescape scratch used by the JSON
/// emitter.
#[derive(Debug, Default)]
pub(crate) struct EmitScratch {
    pub(crate) buf: Vec<u8>,
    pub(crate) str_buf: String,
}

/// Build the per-line `&str` views the `OutputFormat` impls expect. Returns
/// `(add_pairs, remove_keys)`; both are empty when `transform` is `None`.
/// Call this once per stream (or per worker), not per line — the per-line
/// hot path then borrows the slices without allocating.
pub(crate) fn transform_views(transform: Option<&LineTransform>) -> (Vec<(&str, &str)>, Vec<&str>) {
    match transform {
        Some(t) => (
            t.add_escaped
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect(),
            t.remove.iter().map(|s| s.as_str()).collect(),
        ),
        None => (Vec::new(), Vec::new()),
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
    let mut add_escaped = Vec::new();
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
        // Pre-encode the user-supplied value into logfmt so per-line output
        // doesn't have to. The `OutputFormat` trait sees uniform logfmt-shaped
        // pairs across both the parsed line and `--add`.
        let mut buf: Vec<u8> = Vec::new();
        logfmt::write_logfmt_value(&mut buf, val)?;
        let escaped = String::from_utf8(buf).map_err(|_| {
            anyhow::anyhow!("--add: value for `{key}` is not valid UTF-8 after escaping")
        })?;
        add_escaped.push((SmartString::from(key), SmartString::from(escaped)));
    }
    Ok(Some(LineTransform {
        remove,
        add_escaped,
    }))
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
