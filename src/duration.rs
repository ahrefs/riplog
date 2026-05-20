//! Duration parsing for `--from`/`--to`/`--bucket`. Two grammars:
//!
//! - `parse_duration_nanos` — a **single positive** term `<int><unit>`
//!   (e.g. `5m`, `30 seconds`, `2 days`). Used by `--bucket` and the
//!   stdin-bucket path.
//! - `parse_signed_chain` — a **signed sum** of terms
//!   (`+1d + 3h - 5min`). Used by `--from start±…` / `--to end±…` to let
//!   users compose offsets without precomputing seconds.
//!
//! Both return `i64` nanoseconds. All multiplications and accumulations are
//! checked — overflow is a parse error, never a wrap.

/// Parse a positive duration string like `5m`, `30 secs`, `2 hours` into
/// nanoseconds. Units (case-insensitive, plurals accepted, whitespace
/// optional): `s/sec/seconds`, `m/min/minutes`, `h/hour/hours`,
/// `d/day/days`. Rejects empty input, missing unit, and non-positive
/// numbers.
pub fn parse_duration_nanos(s: &str) -> anyhow::Result<i64> {
    let inner = s.trim();
    if inner.is_empty() {
        anyhow::bail!("empty duration");
    }
    let split = inner
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(inner.len());
    if split == 0 || split == inner.len() {
        anyhow::bail!("invalid duration `{s}`: expected `<int><unit>`");
    }
    let num_str = &inner[..split];
    let unit_raw = inner[split..].trim();
    let n: i64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration number in `{s}`"))?;
    if n <= 0 {
        anyhow::bail!("duration must be positive, got `{s}`");
    }
    let unit_nanos = unit_nanos(unit_raw).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown duration unit `{unit_raw}` in `{s}`; expected one of \
             s/sec/seconds, m/min/minutes, h/hour/hours, d/day/days"
        )
    })?;
    n.checked_mul(unit_nanos)
        .ok_or_else(|| anyhow::anyhow!("duration overflow in `{s}`"))
}

/// Parse a signed sum of duration terms, e.g. `+1h`, `+1 day + 3h - 5min`,
/// or `-30m -30m`. The input **must** start with `+` or `-` (after
/// trimming); a leading bare term is rejected so the grammar mirrors the
/// `start±…` / `end±…` form exactly. Returns the signed total in
/// nanoseconds.
pub fn parse_signed_chain(s: &str) -> anyhow::Result<i64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty duration chain");
    }
    let bytes = trimmed.as_bytes();
    if bytes[0] != b'+' && bytes[0] != b'-' {
        anyhow::bail!("duration chain must start with `+` or `-`: `{s}`");
    }
    let mut total: i64 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b' ' || c == b'\t' {
            i += 1;
            continue;
        }
        let sign: i64 = match c {
            b'+' => 1,
            b'-' => -1,
            _ => anyhow::bail!("expected `+` or `-` at position {i} in `{s}`"),
        };
        i += 1;
        // Numbers are unsigned, units are letters/whitespace: any `+`/`-`
        // we hit is a top-level term boundary.
        let rest_start = i;
        let rest = &trimmed[rest_start..];
        let end = rest.find(['+', '-']).unwrap_or(rest.len());
        let term = rest[..end].trim();
        if term.is_empty() {
            anyhow::bail!("missing duration after sign in `{s}`");
        }
        let delta = parse_duration_nanos(term)?;
        let signed = sign
            .checked_mul(delta)
            .ok_or_else(|| anyhow::anyhow!("duration overflow in `{s}`"))?;
        total = total
            .checked_add(signed)
            .ok_or_else(|| anyhow::anyhow!("duration chain overflow in `{s}`"))?;
        i = rest_start + end;
    }
    Ok(total)
}

fn unit_nanos(unit_raw: &str) -> Option<i64> {
    let lc = unit_raw.to_ascii_lowercase();
    Some(match lc.as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000_000_000,
        "m" | "min" | "mins" | "minute" | "minutes" => 60 * 1_000_000_000,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600 * 1_000_000_000,
        "d" | "day" | "days" => 86_400 * 1_000_000_000,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;
    const MIN: i64 = 60 * S;
    const H: i64 = 60 * MIN;
    const D: i64 = 24 * H;

    // ---------- parse_duration_nanos: happy paths ----------

    #[test]
    fn single_term_basic_units() {
        assert_eq!(parse_duration_nanos("1s").unwrap(), S);
        assert_eq!(parse_duration_nanos("5m").unwrap(), 5 * MIN);
        assert_eq!(parse_duration_nanos("2h").unwrap(), 2 * H);
        assert_eq!(parse_duration_nanos("3d").unwrap(), 3 * D);
    }

    #[test]
    fn single_term_word_units() {
        let cases: &[(&str, i64)] = &[
            ("30 seconds", 30 * S),
            ("30 SECONDS", 30 * S),
            ("1 minute", MIN),
            ("5 mins", 5 * MIN),
            ("5 Minutes", 5 * MIN),
            ("1 hour", H),
            ("2 hours", 2 * H),
            ("1 day", D),
            ("3 days", 3 * D),
        ];
        for (s, want) in cases {
            assert_eq!(parse_duration_nanos(s).unwrap(), *want, "{s}");
        }
    }

    #[test]
    fn single_term_no_space_between_num_and_unit() {
        assert_eq!(parse_duration_nanos("90s").unwrap(), 90 * S);
        assert_eq!(parse_duration_nanos("90sec").unwrap(), 90 * S);
        assert_eq!(parse_duration_nanos("90seconds").unwrap(), 90 * S);
    }

    #[test]
    fn single_term_trims_outer_whitespace() {
        assert_eq!(parse_duration_nanos("  5m  ").unwrap(), 5 * MIN);
        assert_eq!(parse_duration_nanos("\t1 hour\n").unwrap(), H);
    }

    // ---------- parse_duration_nanos: error paths ----------

    #[test]
    fn single_term_rejects_empty() {
        assert!(parse_duration_nanos("").is_err());
        assert!(parse_duration_nanos("   ").is_err());
    }

    #[test]
    fn single_term_rejects_missing_number() {
        assert!(parse_duration_nanos("h").is_err());
        assert!(parse_duration_nanos("minutes").is_err());
    }

    #[test]
    fn single_term_rejects_missing_unit() {
        assert!(parse_duration_nanos("5").is_err());
        assert!(parse_duration_nanos("100").is_err());
    }

    #[test]
    fn single_term_rejects_zero_and_negative() {
        assert!(parse_duration_nanos("0s").is_err());
        // Negative sign is not an ASCII digit, so it splits to an empty
        // number part — caught by the `split == 0` check.
        assert!(parse_duration_nanos("-5s").is_err());
    }

    #[test]
    fn single_term_rejects_unknown_units() {
        assert!(parse_duration_nanos("1y").is_err());
        assert!(parse_duration_nanos("1ms").is_err());
        assert!(parse_duration_nanos("1us").is_err());
        assert!(parse_duration_nanos("1ns").is_err());
        assert!(parse_duration_nanos("5 mn").is_err());
        assert!(parse_duration_nanos("5 weeks").is_err());
    }

    #[test]
    fn single_term_rejects_overflow() {
        // i64::MAX nanoseconds ≈ 292 years; 10_000 days fits, 10^18 days
        // doesn't.
        assert!(parse_duration_nanos("1000000000000d").is_err());
    }

    #[test]
    fn single_term_rejects_garbage() {
        assert!(parse_duration_nanos("abc").is_err());
        assert!(parse_duration_nanos("5m extra").is_err());
        assert!(parse_duration_nanos("5 5m").is_err());
    }

    // ---------- parse_signed_chain: happy paths ----------

    #[test]
    fn chain_single_term_matches_signed_duration() {
        assert_eq!(parse_signed_chain("+1h").unwrap(), H);
        assert_eq!(parse_signed_chain("-30m").unwrap(), -30 * MIN);
        assert_eq!(parse_signed_chain("+5 minutes").unwrap(), 5 * MIN);
    }

    #[test]
    fn chain_multi_term_plus_plus_minus() {
        let got = parse_signed_chain("+ 1 day + 3h - 5min").unwrap();
        assert_eq!(got, D + 3 * H - 5 * MIN);
    }

    #[test]
    fn chain_no_spaces_equals_spaced() {
        let a = parse_signed_chain("+1d+3h-5m").unwrap();
        let b = parse_signed_chain("+1 day + 3h - 5min").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn chain_repeated_signs_accumulate() {
        assert_eq!(
            parse_signed_chain("-30m -30m").unwrap(),
            parse_signed_chain("-1h").unwrap()
        );
        assert_eq!(
            parse_signed_chain("+15m +15m +15m +15m").unwrap(),
            parse_signed_chain("+1h").unwrap()
        );
    }

    #[test]
    fn chain_cancels_to_zero() {
        assert_eq!(parse_signed_chain("+1h -1h").unwrap(), 0);
        assert_eq!(parse_signed_chain("+1 day - 24 hours").unwrap(), 0);
    }

    #[test]
    fn chain_tolerates_arbitrary_whitespace() {
        assert_eq!(
            parse_signed_chain("   +   1h   +   30m   ").unwrap(),
            H + 30 * MIN
        );
        assert_eq!(parse_signed_chain("+1h\t+30m").unwrap(), H + 30 * MIN);
    }

    // ---------- parse_signed_chain: error paths ----------

    #[test]
    fn chain_rejects_empty() {
        assert!(parse_signed_chain("").is_err());
        assert!(parse_signed_chain("   ").is_err());
    }

    #[test]
    fn chain_rejects_missing_leading_sign() {
        assert!(parse_signed_chain("1h").is_err());
        assert!(parse_signed_chain("1h + 30m").is_err());
        assert!(parse_signed_chain("hello").is_err());
    }

    #[test]
    fn chain_rejects_trailing_sign() {
        let err = parse_signed_chain("+1h+").unwrap_err().to_string();
        assert!(
            err.contains("missing duration after sign"),
            "unexpected error: {err}"
        );
        assert!(parse_signed_chain("+1h-").is_err());
        assert!(parse_signed_chain("+").is_err());
        assert!(parse_signed_chain("-").is_err());
    }

    #[test]
    fn chain_rejects_double_sign() {
        assert!(parse_signed_chain("++1h").is_err());
        assert!(parse_signed_chain("+-1h").is_err());
        assert!(parse_signed_chain("--1h").is_err());
    }

    #[test]
    fn chain_propagates_term_errors() {
        assert!(parse_signed_chain("+1h + 5x").is_err());
        assert!(parse_signed_chain("+1h + 0s").is_err());
        assert!(parse_signed_chain("+1h + abc").is_err());
    }

    #[test]
    fn chain_overflow_is_caught() {
        // Two terms each near i64::MAX/2 nanos — should overflow on add.
        let huge = format!("+{}d", i64::MAX / D / 2);
        let chain = format!("{huge} {huge} {huge}");
        assert!(parse_signed_chain(&chain).is_err());
    }

    // ---------- cross-checks: chain vs single ----------

    #[test]
    fn chain_with_one_positive_term_matches_parse_duration_nanos() {
        for s in ["1s", "30 seconds", "5 min", "2h", "1 day"] {
            let chain = parse_signed_chain(&format!("+{s}")).unwrap();
            let single = parse_duration_nanos(s).unwrap();
            assert_eq!(chain, single, "for `{s}`");
        }
    }

    #[test]
    fn chain_negation_is_inverse() {
        for s in ["1s", "5 min", "2h", "1 day", "30 seconds"] {
            let pos = parse_signed_chain(&format!("+{s}")).unwrap();
            let neg = parse_signed_chain(&format!("-{s}")).unwrap();
            assert_eq!(pos, -neg);
        }
    }
}
