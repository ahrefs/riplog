//! Time-bucket configuration: bucket grid + resolver for `--bucket` /
//! `--n-buckets`.

use crate::cli::Cli;
use crate::timestamp::{self, Timestamp};

/// Time bucketing config: width in nanoseconds, plus the origin the bucket
/// grid is aligned to. `--bucket=DURATION` uses origin=0 (epoch-aligned, so
/// 5-minute buckets fall on `:00`, `:05`, ...). `--n-buckets=N` uses
/// origin=window-start *and* `n_buckets=Some(N)`, which clamps the bucket
/// index to `[0, N-1]` so a line at the inclusive `end` boundary lands in
/// the last bucket instead of overflowing into an N+1-th one.
#[derive(Clone, Copy)]
pub(crate) struct BucketSpec {
    pub(crate) nanos: i64,
    pub(crate) origin: i64,
    /// When set, clamps the bucket index to `[0, n_buckets-1]`.
    pub(crate) n_buckets: Option<usize>,
}

impl BucketSpec {
    #[inline]
    pub(crate) fn floor(&self, ts: Timestamp) -> i64 {
        let mut idx = (ts - self.origin).div_euclid(self.nanos);
        if let Some(n) = self.n_buckets {
            let max = (n as i64) - 1;
            if idx < 0 {
                idx = 0;
            } else if idx > max {
                idx = max;
            }
        }
        self.origin + idx * self.nanos
    }
}

/// Compute the bucket spec from `--bucket` / `--n-buckets`. Caller has
/// already resolved `tf`; `global_first`/`global_last` are the file-side
/// bounds returned by `peek_global_window` (or `None` if it wasn't run).
/// Returns `None` when neither flag is set. Errors when `--n-buckets`
/// cannot be sized (no resolvable window) or the resulting width is zero.
pub(crate) fn resolve_bucket_spec(
    cli: &Cli,
    tf: &crate::run::TimeFilter,
    global_first: Option<Timestamp>,
    global_last: Option<Timestamp>,
) -> anyhow::Result<Option<BucketSpec>> {
    if let Some(s) = cli.bucket.as_deref() {
        let nanos = timestamp::parse_duration_nanos(s)?;
        return Ok(Some(BucketSpec {
            nanos,
            origin: 0,
            n_buckets: None,
        }));
    }
    if let Some(n) = cli.n_buckets {
        if n == 0 {
            anyhow::bail!("--n-buckets must be > 0");
        }
        let start = tf.from.or(global_first).ok_or_else(|| {
            anyhow::anyhow!(
                "--n-buckets needs a window start: pass --from, or use a file with parseable timestamps"
            )
        })?;
        let end = tf.to.or(global_last).ok_or_else(|| {
            anyhow::anyhow!(
                "--n-buckets needs a window end: pass --to, or use a file with parseable timestamps"
            )
        })?;
        let span = end - start;
        if span <= 0 {
            anyhow::bail!("--n-buckets: time range is empty (end <= start)");
        }
        let nanos = span / n as i64;
        if nanos == 0 {
            anyhow::bail!("--n-buckets={n}: span {span}ns is too small to split into {n} buckets");
        }
        return Ok(Some(BucketSpec {
            nanos,
            origin: start,
            n_buckets: Some(n),
        }));
    }
    Ok(None)
}
