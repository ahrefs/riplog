//! Output formatting for every shape `riplog` emits: per-line, aggregation
//! row, string-set summary, values summary, and bare count. Each output
//! "shape" is a method on the [`OutputFormat`] trait; the two impls
//! ([`crate::logfmt::LogfmtFormat`] and [`crate::json::JsonlFormat`]) carry
//! their own state and live next to their respective format's other code.
//!
//! Dispatch is static: the trait methods are generic over `W: Write + ?Sized`
//! (the trait is therefore NOT object-safe), and callers hold a
//! [`Formatter`] enum that wraps the two concrete impls. The enum's trait
//! impl matches on the variant and forwards to the inner impl; with the
//! `#[inline]` hints below the compiler monomorphizes per-`W` and inlines
//! the match arms into the per-line hot path, so per-key `write_all` calls
//! become direct stores rather than vtable indirections.

use rapidhash::RapidHashSet;
use smartstring::alias::String as SmartString;
use std::io::{self, Write};

use crate::aggregate::{AggregateSpec, Aggregates};
use crate::timestamp::Timestamp;

// -------- OutputFormat trait --------

/// Format-engine dispatcher. Picked once per run by `run::run` and stored
/// on `RunConfig::formatter` as `&Formatter`. Every output shape — per-line,
/// aggregation row, string-set summary, values summary, bare count — flows
/// through this trait. Methods are generic over the writer so that the
/// concrete formatter code (e.g. `LogfmtFormat::line`'s per-pair loop) can
/// be inlined into the caller's writer type, avoiding per-`write_all`
/// vtable indirections. The trait is therefore non-object-safe by design —
/// callers use [`Formatter`] (an enum dispatching via `match`) instead.
///
/// `Send + Sync` is kept on the supertrait for clarity: workers in the
/// parallel path share `&RunConfig` across threads. Both impls are trivial
/// data so the bound is free.
pub(crate) trait OutputFormat: Send + Sync {
    /// One matched line. `pairs` is a slice of `[(key, value)]` slices —
    /// the outer level lets the caller fold in `--add` pairs without
    /// per-call concatenation. `remove` lists keys to drop (`--rm`).
    ///
    /// `original_trailer` is the raw trailing bytes from the input line
    /// (whitespace + newline). Plain logfmt preserves it verbatim (so CRLF
    /// inputs stay CRLF on output); colored logfmt and jsonl emit their
    /// own `\n` and ignore it.
    ///
    /// `scratch` is a reusable `String` owned by the caller; the impl is
    /// free to clear and fill it and must not rely on its prior contents.
    fn line<W: Write + ?Sized>(
        &self,
        w: &mut W,
        pairs: &[&[(&str, &str)]],
        remove: &[&str],
        original_trailer: &[u8],
        scratch: &mut String,
    ) -> io::Result<()>;

    /// One aggregation row (`--group-by` / `--bucket` / `--n-buckets`
    /// and/or any numeric aggregate).
    #[allow(clippy::too_many_arguments)]
    fn agg_row<W: Write + ?Sized>(
        &self,
        w: &mut W,
        spec: &AggregateSpec,
        aggregates: &mut Aggregates,
        keys: &[SmartString],
        combo: &[SmartString],
        bucket: Option<(Timestamp, Timestamp)>,
        tz: &jiff::tz::TimeZone,
    ) -> io::Result<()>;

    /// `--list-keys` summary (or single-key `--list-values-for`).
    fn string_set<W: Write + ?Sized>(
        &self,
        w: &mut W,
        set: &RapidHashSet<SmartString>,
    ) -> io::Result<()>;

    /// `--list-values-for` summary, possibly multi-key.
    fn values_summary<W: Write + ?Sized>(
        &self,
        w: &mut W,
        keys: &[SmartString],
        values: &[RapidHashSet<SmartString>],
    ) -> io::Result<()>;

    /// Bare `--count` summary line.
    fn count_only<W: Write + ?Sized>(&self, w: &mut W, n: u64) -> io::Result<()>;
}

/// Enum wrapper that dispatches statically (via `match`) to the concrete
/// formatter impl. Lives next to [`OutputFormat`] because it's the only
/// way callers consume the trait — the trait itself is non-object-safe.
///
/// Each method is `#[inline]` so the compiler can fold the match into the
/// caller after monomorphising the generic `W`, restoring the pre-trait
/// fully-static dispatch shape on the hot per-line path.
pub(crate) enum Formatter {
    Logfmt(crate::logfmt::LogfmtFormat),
    Json(crate::json::JsonlFormat),
}

impl OutputFormat for Formatter {
    #[inline]
    fn line<W: Write + ?Sized>(
        &self,
        w: &mut W,
        pairs: &[&[(&str, &str)]],
        remove: &[&str],
        original_trailer: &[u8],
        scratch: &mut String,
    ) -> io::Result<()> {
        match self {
            Formatter::Logfmt(f) => f.line(w, pairs, remove, original_trailer, scratch),
            Formatter::Json(f) => f.line(w, pairs, remove, original_trailer, scratch),
        }
    }

    #[inline]
    fn agg_row<W: Write + ?Sized>(
        &self,
        w: &mut W,
        spec: &AggregateSpec,
        aggregates: &mut Aggregates,
        keys: &[SmartString],
        combo: &[SmartString],
        bucket: Option<(Timestamp, Timestamp)>,
        tz: &jiff::tz::TimeZone,
    ) -> io::Result<()> {
        match self {
            Formatter::Logfmt(f) => f.agg_row(w, spec, aggregates, keys, combo, bucket, tz),
            Formatter::Json(f) => f.agg_row(w, spec, aggregates, keys, combo, bucket, tz),
        }
    }

    #[inline]
    fn string_set<W: Write + ?Sized>(
        &self,
        w: &mut W,
        set: &RapidHashSet<SmartString>,
    ) -> io::Result<()> {
        match self {
            Formatter::Logfmt(f) => f.string_set(w, set),
            Formatter::Json(f) => f.string_set(w, set),
        }
    }

    #[inline]
    fn values_summary<W: Write + ?Sized>(
        &self,
        w: &mut W,
        keys: &[SmartString],
        values: &[RapidHashSet<SmartString>],
    ) -> io::Result<()> {
        match self {
            Formatter::Logfmt(f) => f.values_summary(w, keys, values),
            Formatter::Json(f) => f.values_summary(w, keys, values),
        }
    }

    #[inline]
    fn count_only<W: Write + ?Sized>(&self, w: &mut W, n: u64) -> io::Result<()> {
        match self {
            Formatter::Logfmt(f) => f.count_only(w, n),
            Formatter::Json(f) => f.count_only(w, n),
        }
    }
}

// -------- shared helpers for the format impls --------

#[inline]
pub(crate) fn is_removed(remove: &[&str], k: &str) -> bool {
    // Hot-path optimisation: when `--rm` is unused the slice is empty and we
    // skip the linear scan entirely.
    !remove.is_empty() && remove.contains(&k)
}

#[inline]
pub(crate) fn sorted(set: &RapidHashSet<SmartString>) -> Vec<&SmartString> {
    let mut v: Vec<&SmartString> = set.iter().collect();
    v.sort_unstable();
    v
}
