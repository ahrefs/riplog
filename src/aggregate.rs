//! Per-(group, bucket) aggregates: count, min/max timestamps, plus
//! numeric aggregates (`--average`, `--p50`, `--p90`, `--p99`) maintained
//! per user-named numeric key.
//!
//! Numeric storage strategy:
//! - While `samples_seen <= cap`, every value is stored exactly. Percentiles
//!   are computed by sorting `samples` in place (lazy, gated by `sorted`)
//!   and linearly interpolating.
//! - Once `samples_seen > cap`, the accumulator switches to reservoir
//!   sampling (Algorithm L, Li 1994) keeping `cap` samples uniformly drawn
//!   from the full stream. `samples.len()` is pinned at `cap`;
//!   `samples_seen` keeps incrementing as the true denominator.
//! - `average()` reads from the exact running `sum`, so it stays exact
//!   even past the cap.
//!
//! Merging two `NumericAccum`s preserves the uniform-sample property —
//! see `merge()` for the case split (both-exact, mixed, both-reservoir).

use rapidhash::RapidHashMap;
use smallvec::SmallVec;
use smartstring::alias::String as SmartString;

use crate::cli::Cli;
use crate::raw_extractor::unescape_for_key;
use crate::timestamp::{fold_max, fold_min, Timestamp};

/// User-resolved aggregate request. Built once from `Cli` and shared
/// (read-only) by every `Aggregates` instance.
#[derive(Clone, Debug, Default)]
pub(crate) struct AggregateSpec {
    pub count: bool,
    pub averages: Vec<SmartString>,
    pub p50s: Vec<SmartString>,
    pub p90s: Vec<SmartString>,
    pub p99s: Vec<SmartString>,
    /// Per-(group, key) cap before reservoir sampling kicks in. 0 means
    /// "no numeric aggregates", but still legal.
    pub sample_cap: u64,
}

impl AggregateSpec {
    pub fn from_cli(cli: &Cli) -> Self {
        let conv = |xs: &[String]| -> Vec<SmartString> {
            let mut out: Vec<SmartString> = Vec::with_capacity(xs.len());
            for s in xs {
                let k = SmartString::from(s.as_str());
                if !out.iter().any(|existing| existing == &k) {
                    out.push(k);
                }
            }
            out
        };
        Self {
            count: cli.count,
            averages: conv(&cli.average),
            p50s: conv(&cli.p50),
            p90s: conv(&cli.p90),
            p99s: conv(&cli.p99),
            sample_cap: cli.sample_cap,
        }
    }

    pub fn is_active(&self) -> bool {
        self.count || self.has_numeric()
    }

    /// True if any *numeric* aggregate (average/percentile) is requested.
    /// The bare `--count` flag alone doesn't activate the per-(group,
    /// bucket) row machinery — `run.rs` routes it through `count_only`
    /// instead, so the counter only "is_active" when there's grouping,
    /// bucketing, or a numeric aggregate.
    pub fn has_numeric(&self) -> bool {
        !self.averages.is_empty()
            || !self.p50s.is_empty()
            || !self.p90s.is_empty()
            || !self.p99s.is_empty()
    }

    /// Deduped union of every key needing numeric parsing, in first-seen
    /// CLI order (averages → p50s → p90s → p99s). Drives the parse loop
    /// in `Aggregates::record` and the per-key accumulator layout.
    pub fn numeric_keys(&self) -> SmallVec<[SmartString; 2]> {
        let mut out: SmallVec<[SmartString; 2]> = SmallVec::new();
        for src in [&self.averages, &self.p50s, &self.p90s, &self.p99s] {
            for k in src {
                if !out.iter().any(|existing| existing == k) {
                    out.push(k.clone());
                }
            }
        }
        out
    }
}

/// Per-key numeric accumulator. See module header for the
/// exact-then-reservoir strategy.
#[derive(Clone, Debug)]
pub(crate) struct NumericAccum {
    pub key: SmartString,
    /// Cap copied from `AggregateSpec` so single-accum methods don't need
    /// the spec borrow.
    cap: u64,
    /// Exact samples while `samples_seen <= cap`; reservoir of size `cap`
    /// drawn uniformly from the stream past that.
    pub samples: Vec<f64>,
    /// Exact running sum — drives `average()` even past the cap.
    pub sum: f64,
    /// True count of *valid numeric* samples seen across the stream.
    /// Differs from `samples.len()` once the reservoir is active.
    pub samples_seen: u64,
    /// Lines that lacked this key entirely.
    pub missing: u64,
    /// Lines where the value was present but did not parse as f64.
    pub non_numeric: u64,
    pub reservoir_active: bool,
    /// Algorithm L's running random "skip this many" before next
    /// replacement. Unused while exact.
    next_skip: u64,
    /// Algorithm L's running multiplier (the `w` variable). Unused while
    /// exact. Stored as f64 — kept across record calls.
    w: f64,
    /// `true` once `samples` is sorted ascending. Cleared on any
    /// mutation (push, reservoir replace, merge).
    sorted: bool,
}

impl NumericAccum {
    fn new(key: SmartString, cap: u64) -> Self {
        Self {
            key,
            cap,
            samples: Vec::new(),
            sum: 0.0,
            samples_seen: 0,
            missing: 0,
            non_numeric: 0,
            reservoir_active: false,
            next_skip: 0,
            w: 1.0,
            sorted: true, // vacuously sorted
        }
    }

    /// Record one numeric value. Decides exact-push vs reservoir-replace
    /// based on `samples_seen` and `cap`.
    #[inline]
    fn record_value(&mut self, v: f64) {
        self.sum += v;
        self.samples_seen += 1;
        self.sorted = false;

        let cap = self.cap;
        if !self.reservoir_active {
            if self.samples_seen <= cap {
                // Exact mode: push and keep going.
                self.samples.push(v);
                return;
            }
            // We just crossed the threshold; the value that pushed us over
            // is the (cap + 1)th observation. The first `cap` are already
            // in `samples` (they're our initial reservoir). Switch modes
            // and seed Algorithm L's state, then process this value as
            // the first post-cap observation.
            self.reservoir_active = true;
            // Algorithm L seeding (per Li 1994):
            //   w = exp(ln(random) / cap)
            //   next index to replace = cap + floor(ln(random) / ln(1 - w)) + 1
            // We track `next_skip` as the number of subsequent samples to
            // skip before the next replacement (0 = replace this one).
            self.w = (fastrand::f64().ln() / cap as f64).exp();
            self.next_skip = (fastrand::f64().ln() / (1.0 - self.w).ln()).floor() as u64;
        }

        // Reservoir mode. `next_skip` counts down; when it hits zero, the
        // current value replaces a random slot and we re-seed.
        if cap == 0 {
            return; // pathological; nothing to sample into
        }
        if self.next_skip == 0 {
            let slot = fastrand::usize(0..cap as usize);
            self.samples[slot] = v;
            self.w *= (fastrand::f64().ln() / cap as f64).exp();
            self.next_skip = (fastrand::f64().ln() / (1.0 - self.w).ln()).floor() as u64;
        } else {
            self.next_skip -= 1;
        }
    }

    /// Average across all valid numeric samples (exact even past the cap).
    pub fn average(&self) -> Option<f64> {
        if self.samples_seen == 0 {
            None
        } else {
            Some(self.sum / self.samples_seen as f64)
        }
    }

    /// Linear-interpolation percentile (`p` in `[0, 1]`). Sorts `samples`
    /// in place on first call after a mutation. Returns `None` when
    /// there are no samples (either nothing seen, or all
    /// missing/non-numeric).
    pub fn percentile(&mut self, p: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        if !self.sorted {
            self.samples
                .sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            self.sorted = true;
        }
        let n = self.samples.len();
        if n == 1 {
            return Some(self.samples[0]);
        }
        let rank = p.clamp(0.0, 1.0) * (n - 1) as f64;
        let lo = rank.floor() as usize;
        let hi = rank.ceil() as usize;
        if lo == hi {
            Some(self.samples[lo])
        } else {
            let frac = rank - lo as f64;
            Some(self.samples[lo] + (self.samples[hi] - self.samples[lo]) * frac)
        }
    }

    /// Fold `other` into `self`, preserving the uniform-sample property.
    /// See module header. `cap` is taken from `self` (both sides should
    /// share a spec).
    fn merge(&mut self, mut other: NumericAccum) {
        debug_assert_eq!(self.key, other.key);
        debug_assert_eq!(self.cap, other.cap);

        self.sum += other.sum;
        self.missing += other.missing;
        self.non_numeric += other.non_numeric;

        let count_a = self.samples_seen;
        let count_b = other.samples_seen;
        self.samples_seen = count_a.saturating_add(count_b);
        self.sorted = false;

        if count_b == 0 {
            return;
        }
        if count_a == 0 {
            self.samples = std::mem::take(&mut other.samples);
            self.reservoir_active = other.reservoir_active;
            self.w = other.w;
            self.next_skip = other.next_skip;
            return;
        }

        let cap = self.cap;
        let total = self.samples_seen;

        if !self.reservoir_active && !other.reservoir_active && total <= cap {
            // Both exact, fits exact — just concat.
            self.samples.append(&mut other.samples);
            return;
        }

        // From here on we will produce a reservoir of size `cap`. The
        // merged accumulator is reservoir-active.
        self.reservoir_active = true;

        // For the merge math, treat each side as a uniform sample drawn
        // from its own population (size = `samples_seen` on that side). To
        // produce a uniform sample of size `cap` from the union of both
        // populations, draw `k` from the binomial with `p =
        // count_a / (count_a + count_b)` and take `k` random items from
        // `self.samples` and `cap - k` from `other.samples`.
        //
        // Why binomial-with-replacement rather than hypergeometric
        // without-replacement: simpler, no extra distribution code, and
        // the bias scales as O(N / total) which vanishes for the regimes
        // where the reservoir matters at all (total ≫ cap). Documented
        // approximation.
        let cap_usize = cap as usize;
        let p_self = count_a as f64 / total as f64;
        let mut k: usize = 0;
        for _ in 0..cap_usize {
            if fastrand::f64() < p_self {
                k += 1;
            }
        }
        let from_other = cap_usize - k;

        let mut merged: Vec<f64> = Vec::with_capacity(cap_usize);
        // Pick k uniformly with replacement from self.samples and the
        // rest from other.samples. With-replacement is consistent with
        // how a reservoir is itself a sampled view of its population.
        if !self.samples.is_empty() {
            for _ in 0..k {
                let idx = fastrand::usize(0..self.samples.len());
                merged.push(self.samples[idx]);
            }
        }
        if !other.samples.is_empty() {
            for _ in 0..from_other {
                let idx = fastrand::usize(0..other.samples.len());
                merged.push(other.samples[idx]);
            }
        }
        self.samples = merged;

        // Reset Algorithm L state — we treat the merged reservoir as a
        // fresh starting point; subsequent record_value calls (rare in
        // practice, since merges happen at end-of-run) will re-seed `w`
        // when the next sample lands past cap. Setting next_skip to 0
        // forces re-seeding on the next record_value.
        self.w = 1.0;
        self.next_skip = 0;
    }
}

/// Per-(group, bucket) aggregate state. Replaces the old `GroupStats`;
/// always carries `count` + timestamp range, plus an optional boxed
/// slice of `NumericAccum`s — one per unique numeric key declared in
/// the `AggregateSpec`.
///
/// `per_key` is `Option<Box<[NumericAccum]>>` (not an inline `SmallVec`)
/// to keep `Aggregates` small in the no-numeric-aggregate hot path
/// (just `--group-by` / `--bucket`). With niche optimization `Option<Box<[T]>>`
/// is two words: total `Aggregates` size is roughly 56 B vs the ~296 B
/// an inline-2 `SmallVec` would force. The boxed slice has a fixed
/// length set at construction (spec is known up front), so we never
/// need to grow it.
#[derive(Default, Clone, Debug)]
pub(crate) struct Aggregates {
    pub count: usize,
    pub min_ts: Option<Timestamp>,
    pub max_ts: Option<Timestamp>,
    pub per_key: Option<Box<[NumericAccum]>>,
}

impl Aggregates {
    /// Pre-seed `per_key` from the spec so `record` can walk by index
    /// without re-deduping per line. Returns `None` when the spec has
    /// no numeric aggregates — keeps `Aggregates` two-words slim in the
    /// hot `--group-by` / `--bucket`-only path.
    pub fn new(spec: &AggregateSpec) -> Self {
        let per_key = if spec.has_numeric() {
            let v: Vec<NumericAccum> = spec
                .numeric_keys()
                .into_iter()
                .map(|k| NumericAccum::new(k, spec.sample_cap))
                .collect();
            Some(v.into_boxed_slice())
        } else {
            None
        };
        Self {
            count: 0,
            min_ts: None,
            max_ts: None,
            per_key,
        }
    }

    pub fn record(&mut self, pairs: &[(&str, &str)], ts: Option<Timestamp>, scratch: &mut String) {
        self.count += 1;
        if let Some(t) = ts {
            fold_min(&mut self.min_ts, t);
            fold_max(&mut self.max_ts, t);
        }
        if let Some(pk) = self.per_key.as_deref_mut() {
            for accum in pk.iter_mut() {
                match unescape_for_key(pairs, &accum.key, scratch) {
                    None => accum.missing += 1,
                    Some(s) => match s.trim().parse::<f64>() {
                        Ok(v) if v.is_finite() => accum.record_value(v),
                        _ => accum.non_numeric += 1,
                    },
                }
            }
        }
    }

    pub fn merge(&mut self, other: Aggregates) {
        self.count += other.count;
        if let Some(t) = other.min_ts {
            fold_min(&mut self.min_ts, t);
        }
        if let Some(t) = other.max_ts {
            fold_max(&mut self.max_ts, t);
        }
        let Some(other_pk) = other.per_key else {
            return;
        };
        if self.per_key.is_none() {
            // Spec mismatch (self lacks numeric); adopt other's slice
            // wholesale so its samples aren't lost.
            self.per_key = Some(other_pk);
            return;
        }
        let self_pk = self.per_key.as_deref_mut().unwrap();
        // Both sides built from the same spec → same order. Fall back
        // to find-by-key on mismatch.
        for (i, other_accum) in Vec::from(other_pk).into_iter().enumerate() {
            let same_at_i = self_pk
                .get(i)
                .map(|a| a.key == other_accum.key)
                .unwrap_or(false);
            if same_at_i {
                self_pk[i].merge(other_accum);
            } else if let Some(found) = self_pk.iter_mut().find(|a| a.key == other_accum.key) {
                found.merge(other_accum);
            }
            // else: foreign accum has no matching slot here. The boxed
            // slice is fixed-length so we can't append; drop it. In
            // practice both sides come from the same `AggregateSpec`,
            // so this branch is unreachable.
        }
    }

    /// Look up the accumulator for `key` (mutable so callers can compute
    /// percentiles, which sorts lazily). `None` if the spec didn't
    /// declare that key.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut NumericAccum> {
        self.per_key
            .as_deref_mut()?
            .iter_mut()
            .find(|a| a.key.as_str() == key)
    }
}

/// Sum non-numeric counts and detect reservoir-activation across every
/// `(group, bucket)` accumulator in a counter's `counts` map. Used by
/// `emit_summaries` to print end-of-run stderr warnings.
pub(crate) struct WarningTotals {
    /// Per-key total of `non_numeric` across every group.
    pub non_numeric: RapidHashMap<SmartString, u64>,
    /// True if any accumulator ever flipped `reservoir_active`.
    pub reservoir_engaged: bool,
}

impl WarningTotals {
    pub fn collect<'a>(groups: impl Iterator<Item = &'a Aggregates>) -> Self {
        let mut non_numeric: RapidHashMap<SmartString, u64> = RapidHashMap::default();
        let mut reservoir_engaged = false;
        for g in groups {
            let Some(pk) = g.per_key.as_deref() else {
                continue;
            };
            for accum in pk {
                if accum.non_numeric > 0 {
                    *non_numeric.entry(accum.key.clone()).or_insert(0) += accum.non_numeric;
                }
                if accum.reservoir_active {
                    reservoir_engaged = true;
                }
            }
        }
        Self {
            non_numeric,
            reservoir_engaged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_spec(p50s: &[&str]) -> AggregateSpec {
        AggregateSpec {
            count: false,
            averages: vec![],
            p50s: p50s.iter().map(|s| SmartString::from(*s)).collect(),
            p90s: vec![],
            p99s: vec![],
            sample_cap: 1024,
        }
    }

    #[test]
    fn percentile_exact_uniform() {
        let spec = build_spec(&["x"]);
        let mut a = Aggregates::new(&spec);
        let mut scratch = String::new();
        for i in 1..=100 {
            let s = i.to_string();
            let pairs: &[(&str, &str)] = &[("x", &s)];
            a.record(pairs, None, &mut scratch);
        }
        let accum = a.get_mut("x").unwrap();
        assert!((accum.percentile(0.5).unwrap() - 50.5).abs() < 0.001);
        assert!((accum.percentile(0.9).unwrap() - 90.1).abs() < 0.001);
        assert!((accum.percentile(0.99).unwrap() - 99.01).abs() < 0.001);
        assert!((accum.average().unwrap() - 50.5).abs() < 0.001);
    }

    #[test]
    fn percentile_empty_returns_none() {
        let spec = build_spec(&["x"]);
        let mut a = Aggregates::new(&spec);
        assert!(a.get_mut("x").unwrap().percentile(0.5).is_none());
        assert!(a.get_mut("x").unwrap().average().is_none());
    }

    #[test]
    fn missing_and_non_numeric_tracked_separately() {
        let spec = build_spec(&["x"]);
        let mut a = Aggregates::new(&spec);
        let mut scratch = String::new();
        a.record(&[("y", "ignored")], None, &mut scratch);
        a.record(&[("x", "not a number")], None, &mut scratch);
        a.record(&[("x", "42")], None, &mut scratch);
        let accum = a.get_mut("x").unwrap();
        assert_eq!(accum.missing, 1);
        assert_eq!(accum.non_numeric, 1);
        assert_eq!(accum.samples_seen, 1);
        assert_eq!(accum.samples.len(), 1);
    }

    #[test]
    fn average_exact_past_reservoir_cap() {
        let spec = AggregateSpec {
            count: false,
            averages: vec![SmartString::from("x")],
            p50s: vec![SmartString::from("x")],
            p90s: vec![],
            p99s: vec![],
            sample_cap: 100,
        };
        let mut a = Aggregates::new(&spec);
        let mut scratch = String::new();
        let n: u64 = 10_000;
        let mut true_sum: f64 = 0.0;
        for i in 1..=n {
            let s = i.to_string();
            a.record(&[("x", s.as_str())], None, &mut scratch);
            true_sum += i as f64;
        }
        let accum = a.get_mut("x").unwrap();
        assert!(accum.reservoir_active);
        assert_eq!(accum.samples_seen, n);
        assert_eq!(accum.samples.len(), 100);
        // average is exact even past the cap
        let expected = true_sum / n as f64;
        assert!((accum.average().unwrap() - expected).abs() < 1e-6);
        // p50 from the reservoir is approximate; on a uniform [1, n]
        // stream it should still land near n/2.
        let p50 = accum.percentile(0.5).unwrap();
        let tol = 0.2 * n as f64; // within 20% of true (reservoir noise)
        assert!(
            (p50 - expected).abs() < tol,
            "p50={p50} expected≈{expected}"
        );
    }

    #[test]
    fn merge_exact_below_cap_concats() {
        let spec = build_spec(&["x"]);
        let mut a = Aggregates::new(&spec);
        let mut b = Aggregates::new(&spec);
        let mut scratch = String::new();
        for i in 1..=50 {
            let s = i.to_string();
            a.record(&[("x", s.as_str())], None, &mut scratch);
        }
        for i in 51..=100 {
            let s = i.to_string();
            b.record(&[("x", s.as_str())], None, &mut scratch);
        }
        a.merge(b);
        let accum = a.get_mut("x").unwrap();
        assert_eq!(accum.samples_seen, 100);
        assert_eq!(accum.samples.len(), 100);
        assert!(!accum.reservoir_active);
        assert!((accum.percentile(0.5).unwrap() - 50.5).abs() < 0.001);
    }
}
