//! Time-bucket configuration: user-input spec + resolved grid + CLI/window
//! resolver for `--bucket` / `--n-buckets`.

use crate::cli::Cli;
use crate::timestamp::{self, Timestamp};

/// User-supplied bucketing input, parsed from CLI flags. One step removed
/// from a concrete grid: `NBuckets` still needs the active time window to
/// compute its width, while `Duration` is already self-contained.
#[derive(Clone, Copy)]
pub(crate) enum BucketSpec {
    /// `--n-buckets=N`: split the resolved window into N equal slices.
    NBuckets(u32),
    /// `--bucket=DURATION`: epoch-aligned grid with this width (nanoseconds).
    Duration(i64),
}

/// Resolved bucket grid: ready to consume per-line. `--bucket=DURATION` uses
/// `start_nanos=0` (epoch-aligned, so 5-minute buckets fall on `:00`, `:05`,
/// ...). `--n-buckets=N` uses `start_nanos=window-start` *and*
/// `n_buckets=Some(N)`, which clamps the bucket index to `[0, N-1]` so a
/// line at the inclusive `end` boundary lands in the last bucket instead of
/// overflowing into an N+1-th one.
#[derive(Clone, Copy)]
pub(crate) struct ResolvedBucket {
    pub(crate) start_nanos: i64,
    pub(crate) dur_nanos: i64,
    /// When set, clamps the bucket index to `[0, n_buckets-1]`.
    pub(crate) n_buckets: Option<u32>,
}

impl ResolvedBucket {
    #[inline]
    pub(crate) fn floor(&self, ts: Timestamp) -> i64 {
        let mut idx = (ts - self.start_nanos).div_euclid(self.dur_nanos);
        if let Some(n) = self.n_buckets {
            let max = (n as i64) - 1;
            if idx < 0 {
                idx = 0;
            } else if idx > max {
                idx = max;
            }
        }
        self.start_nanos + idx * self.dur_nanos
    }
}

impl BucketSpec {
    /// Read `--bucket` / `--n-buckets` off the CLI. `--bucket` wins when both
    /// are set (clap rejects the combo today via `conflicts_with`, but the
    /// precedence is preserved verbatim if that ever loosens). Returns `None`
    /// when neither flag is set.
    pub(crate) fn from_cli(cli: &Cli) -> anyhow::Result<Option<BucketSpec>> {
        if let Some(s) = cli.bucket.as_deref() {
            let nanos = timestamp::parse_duration_nanos(s)?;
            return Ok(Some(BucketSpec::Duration(nanos)));
        }
        if let Some(n) = cli.n_buckets {
            if n == 0 {
                anyhow::bail!("--n-buckets must be > 0");
            }
            return Ok(Some(BucketSpec::NBuckets(n as u32)));
        }
        Ok(None)
    }
}

/// Resolve a user-supplied `BucketSpec` against the active time window.
/// `Duration` is already self-contained; `NBuckets` needs the window so it
/// can compute the per-slice width. Caller has already resolved `tf`;
/// `global_first`/`global_last` are the file-side bounds returned by
/// `peek_global_window` (or `None` if it wasn't run).
pub(crate) fn resolve(
    spec: BucketSpec,
    tf: &crate::run::TimeFilter,
    global_first: Option<Timestamp>,
    global_last: Option<Timestamp>,
) -> anyhow::Result<ResolvedBucket> {
    match spec {
        BucketSpec::Duration(nanos) => Ok(ResolvedBucket {
            start_nanos: 0,
            dur_nanos: nanos,
            n_buckets: None,
        }),
        BucketSpec::NBuckets(n) => {
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
            let dur_nanos = span / n as i64;
            if dur_nanos == 0 {
                anyhow::bail!(
                    "--n-buckets={n}: span {span}ns is too small to split into {n} buckets"
                );
            }
            Ok(ResolvedBucket {
                start_nanos: start,
                dur_nanos,
                n_buckets: Some(n),
            })
        }
    }
}
