//! Per-line random sampling.
//!
//! When `--sample-rate` is set, matched lines pass through a dice roll;
//! `--sample-if` narrows the dice roll to a sub-predicate so e.g. only
//! `level=info` lines are sampled and everything else passes through
//! untouched.

use std::sync::Arc;

use crate::cli::Cli;
use crate::filter::Filter;

/// Random per-line sampling. When `sample_if` is set, only lines matching it
/// are subject to the dice roll; all other matched lines pass through.
#[derive(Clone)]
pub(crate) struct Sampler {
    rate: f64,
    sample_if: Option<Arc<Filter>>,
}

impl Sampler {
    pub(crate) fn keep(&self, pairs: &[(&str, &str)]) -> bool {
        let subject = self.sample_if.as_ref().is_none_or(|f| f.matches(pairs));
        !subject || fastrand::f64() < self.rate
    }
}

/// Parse the sampler config out of `--sample-rate` / `--sample-if`. Done
/// once up front so workers can clone it cheaply (the inner `Filter` is
/// shared via `Arc`).
pub(crate) fn build_sampler(cli: &Cli) -> anyhow::Result<Option<Sampler>> {
    match (cli.sample_rate, cli.sample_if.as_deref()) {
        (None, Some(_)) => anyhow::bail!("--sample-if requires --sample-rate"),
        (None, None) => Ok(None),
        (Some(rate), _) if !(0.0..=1.0).contains(&rate) => {
            anyhow::bail!("--sample-rate must be in [0, 1], got {rate}")
        }
        (Some(rate), sample_if) => Ok(Some(Sampler {
            rate,
            sample_if: sample_if.map(Filter::parse_one).transpose()?.map(Arc::new),
        })),
    }
}
