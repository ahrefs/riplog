//! Fast RFC 3339 timestamp parsing into nanoseconds since the Unix epoch.

use std::fmt::Write as _;

pub type Timestamp = i64;

/// Fold `t` into `slot`, keeping the minimum. `None` becomes `Some(t)`.
#[inline]
pub(crate) fn fold_min(slot: &mut Option<Timestamp>, t: Timestamp) {
    *slot = Some(slot.map_or(t, |cur| cur.min(t)));
}

/// Fold `t` into `slot`, keeping the maximum. `None` becomes `Some(t)`.
#[inline]
pub(crate) fn fold_max(slot: &mut Option<Timestamp>, t: Timestamp) {
    *slot = Some(slot.map_or(t, |cur| cur.max(t)));
}

/// Parse an RFC 3339 / ISO 8601 timestamp into nanoseconds since the Unix epoch.
///
/// Accepts:
/// - `YYYY-MM-DDTHH:MM:SS` (or space separator)
/// - optional `.frac` with 1..=9 fractional digits
/// - optional timezone suffix: `Z`, `±HH:MM`, `±HHMM`, or `±HH`
///
/// Returns `None` for any malformed input. Never panics.
pub fn parse_rfc3339_nanos(s: &str) -> Option<Timestamp> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }

    let year = parse_uint(&b[0..4])? as i32;
    if b[4] != b'-' {
        return None;
    }
    let month = parse_uint(&b[5..7])?;
    if b[7] != b'-' {
        return None;
    }
    let day = parse_uint(&b[8..10])?;
    if b[10] != b'T' && b[10] != b't' && b[10] != b' ' {
        return None;
    }
    let hour = parse_uint(&b[11..13])?;
    if b[13] != b':' {
        return None;
    }
    let minute = parse_uint(&b[14..16])?;
    if b[16] != b':' {
        return None;
    }
    let second = parse_uint(&b[17..19])?;

    if !(1..=12).contains(&month)
        || !valid_day(year, month, day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut idx = 19;
    let mut nanos_frac: i64 = 0;
    if idx < b.len() && b[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
        nanos_frac = parse_frac_nanos(&b[frac_start..idx])?;
    }

    let tz_offset_secs: i64 = if idx >= b.len() {
        0
    } else {
        match b[idx] {
            b'Z' | b'z' => {
                idx += 1;
                0
            }
            b'+' | b'-' => {
                let sign: i64 = if b[idx] == b'-' { -1 } else { 1 };
                idx += 1;
                if idx + 2 > b.len() {
                    return None;
                }
                let oh = parse_uint(&b[idx..idx + 2])? as i64;
                idx += 2;
                let om: i64 = if idx < b.len() && b[idx] == b':' {
                    idx += 1;
                    if idx + 2 > b.len() {
                        return None;
                    }
                    let v = parse_uint(&b[idx..idx + 2])? as i64;
                    idx += 2;
                    v
                } else if idx + 2 <= b.len() && b[idx].is_ascii_digit() {
                    let v = parse_uint(&b[idx..idx + 2])? as i64;
                    idx += 2;
                    v
                } else {
                    0
                };
                if oh > 23 || om > 59 {
                    return None;
                }
                sign * (oh * 3600 + om * 60)
            }
            _ => return None,
        }
    };

    if idx != b.len() {
        return None;
    }

    let epoch_days = days_from_civil(year, month as i32, day as i32);
    let secs_of_day = hour as i64 * 3600 + minute as i64 * 60 + second as i64;
    let utc_secs = epoch_days * 86_400 + secs_of_day - tz_offset_secs;

    utc_secs.checked_mul(1_000_000_000)?.checked_add(nanos_frac)
}

const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

/// Resolve a user-supplied time bound. Accepts:
/// - full RFC 3339 (`2026-04-24T18:09:03Z`)
/// - date-only (`2026-04-24` → midnight UTC)
/// - time-of-day (`18:00`, `18:00:00`, `18:00:00.5`) — anchored to the date of
///   `time_only_anchor` (UTC).
/// - `start` / `end` — file's first / last parseable timestamp
/// - `start±<n><unit>` / `end±<n><unit>` (whitespace optional). Units:
///   `s/sec/seconds`, `m/min/minutes`, `h/hour/hours`, `d/day/days`
///   (case-insensitive, plurals accepted).
pub fn resolve_bound(
    s: &str,
    file_first: Option<Timestamp>,
    file_last: Option<Timestamp>,
    time_only_anchor: Option<Timestamp>,
) -> anyhow::Result<Timestamp> {
    if let Some(t) = parse_rfc3339_nanos(s) {
        return Ok(t);
    }

    // Date-only YYYY-MM-DD.
    let b = s.as_bytes();
    if b.len() == 10 && b[4] == b'-' && b[7] == b'-' {
        let mut padded = String::with_capacity(20);
        padded.push_str(s);
        padded.push_str("T00:00:00Z");
        if let Some(t) = parse_rfc3339_nanos(&padded) {
            return Ok(t);
        }
    }

    // Symbolic anchors.
    if let Some(rest) = strip_anchor(s, "start") {
        let base = file_first.ok_or_else(|| {
            anyhow::anyhow!("`start` requires at least one parseable timestamp in the file")
        })?;
        return apply_offset(base, rest);
    }
    if let Some(rest) = strip_anchor(s, "end") {
        let base = file_last.ok_or_else(|| {
            anyhow::anyhow!("`end` requires at least one parseable timestamp in the file")
        })?;
        return apply_offset(base, rest);
    }

    // Time-of-day.
    if let Some(time_nanos) = parse_time_of_day(s) {
        let anchor = time_only_anchor.ok_or_else(|| {
            anyhow::anyhow!(
                "time-only bound `{s}` requires a parseable timestamp in the file to anchor the date"
            )
        })?;
        let day_start = anchor.div_euclid(NANOS_PER_DAY) * NANOS_PER_DAY;
        return Ok(day_start + time_nanos);
    }

    anyhow::bail!("unrecognized time bound: {s}")
}

fn strip_anchor<'a>(s: &'a str, anchor: &str) -> Option<&'a str> {
    let s = s.trim();
    if s == anchor {
        return Some("");
    }
    let rest = s.strip_prefix(anchor)?.trim_start();
    if rest.starts_with('+') || rest.starts_with('-') {
        Some(rest)
    } else {
        None
    }
}

fn parse_time_of_day(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 5 || b[2] != b':' {
        return None;
    }
    let h = parse_uint(&b[0..2])? as i64;
    let m = parse_uint(&b[3..5])? as i64;
    if h > 23 || m > 59 {
        return None;
    }
    let mut nanos = h * 3600 * 1_000_000_000 + m * 60 * 1_000_000_000;
    if b.len() == 5 {
        return Some(nanos);
    }
    if b[5] != b':' || b.len() < 8 {
        return None;
    }
    let sec = parse_uint(&b[6..8])? as i64;
    if sec > 60 {
        return None;
    }
    nanos += sec * 1_000_000_000;
    if b.len() == 8 {
        return Some(nanos);
    }
    if b[8] != b'.' {
        return None;
    }
    Some(nanos + parse_frac_nanos(&b[9..])?)
}

/// True iff `day` is a valid day-of-month for `(year, month)`. Caller has
/// already checked `month ∈ 1..=12`; this catches the silent rollover cases
/// in `days_from_civil` (e.g. `2026-02-30` would otherwise become March 2).
#[inline]
fn valid_day(year: i32, month: u32, day: u32) -> bool {
    if day == 0 {
        return false;
    }
    let max = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    day <= max
}

#[inline]
fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Parse 1..=9 ASCII-digit bytes as a fractional second, scaled to
/// nanoseconds (e.g. `b"5"` → 500_000_000). Returns `None` for empty input,
/// more than 9 digits, or any non-digit byte.
fn parse_frac_nanos(b: &[u8]) -> Option<i64> {
    if b.is_empty() || b.len() > 9 {
        return None;
    }
    let mut acc: i64 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        acc = acc * 10 + (c - b'0') as i64;
    }
    for _ in b.len()..9 {
        acc *= 10;
    }
    Some(acc)
}

/// Parse a positive duration string like `5m`, `30 secs`, `2 hours` into
/// nanoseconds. Accepts the same units as `--from start+<dur>`:
/// `s/sec/seconds`, `m/min/minutes`, `h/hour/hours`, `d/day/days`
/// (case-insensitive, plurals accepted, optional whitespace before the unit).
/// Rejects empty/whitespace input, missing unit, and non-positive numbers.
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
    let unit_lc = unit_raw.to_ascii_lowercase();
    let unit_nanos: i64 = match unit_lc.as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000_000_000,
        "m" | "min" | "mins" | "minute" | "minutes" => 60 * 1_000_000_000,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600 * 1_000_000_000,
        "d" | "day" | "days" => 86_400 * 1_000_000_000,
        _ => anyhow::bail!(
            "unknown duration unit `{unit_raw}` in `{s}`; expected one of \
             s/sec/seconds, m/min/minutes, h/hour/hours, d/day/days"
        ),
    };
    n.checked_mul(unit_nanos)
        .ok_or_else(|| anyhow::anyhow!("duration overflow in `{s}`"))
}

fn apply_offset(base: Timestamp, suffix: &str) -> anyhow::Result<Timestamp> {
    let suffix = suffix.trim();
    if suffix.is_empty() {
        return Ok(base);
    }
    let b = suffix.as_bytes();
    let sign: i64 = match b[0] {
        b'+' => 1,
        b'-' => -1,
        _ => anyhow::bail!("expected `+` or `-` after anchor: `{suffix}`"),
    };
    let delta = parse_duration_nanos(&suffix[1..])?;
    base.checked_add(sign * delta)
        .ok_or_else(|| anyhow::anyhow!("timestamp overflow applying `{suffix}`"))
}

/// Find a `time=…` (or `ts=…`) pair and parse it.
pub fn extract_timestamp(pairs: &[(&str, &str)]) -> Option<Timestamp> {
    for (k, v) in pairs {
        if *k == "time" || *k == "ts" {
            let raw = strip_quotes(v);
            return parse_rfc3339_nanos(raw);
        }
    }
    None
}

pub fn strip_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

fn parse_uint(b: &[u8]) -> Option<u32> {
    let mut acc: u32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return None;
        }
        acc = acc * 10 + (c - b'0') as u32;
    }
    Some(acc)
}

/// Resolve a `--tz` argument. Accepts `utc`, `local`, an IANA name like
/// `Europe/Paris`, or a fixed offset like `+02:00` / `-0530`.
pub fn resolve_tz(spec: Option<&str>) -> anyhow::Result<jiff::tz::TimeZone> {
    let spec = match spec {
        None => return Ok(jiff::tz::TimeZone::UTC),
        Some(s) => s,
    };
    if spec.eq_ignore_ascii_case("utc") {
        return Ok(jiff::tz::TimeZone::UTC);
    }
    if spec.eq_ignore_ascii_case("local") {
        return Ok(jiff::tz::TimeZone::system());
    }
    // Named (Europe/Paris, America/New_York, …).
    if let Ok(tz) = jiff::tz::TimeZone::get(spec) {
        return Ok(tz);
    }
    // Fixed offset like `+02:00`, `-05:30`, `+0200`, `+02`.
    if let Some(offset) = parse_fixed_offset(spec) {
        return Ok(jiff::tz::TimeZone::fixed(offset));
    }
    anyhow::bail!("unknown timezone `{spec}` (try utc, local, IANA name, or ±HH[:MM])")
}

fn parse_fixed_offset(spec: &str) -> Option<jiff::tz::Offset> {
    let b = spec.as_bytes();
    if b.is_empty() {
        return None;
    }
    let sign: i32 = match b[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &spec[1..];
    let (h, m) = match rest.len() {
        2 => (rest.parse::<u32>().ok()?, 0),
        4 => (
            rest[..2].parse::<u32>().ok()?,
            rest[2..].parse::<u32>().ok()?,
        ),
        5 if rest.as_bytes()[2] == b':' => (
            rest[..2].parse::<u32>().ok()?,
            rest[3..].parse::<u32>().ok()?,
        ),
        _ => return None,
    };
    if h > 23 || m > 59 {
        return None;
    }
    let total = sign * ((h * 3600 + m * 60) as i32);
    jiff::tz::Offset::from_seconds(total).ok()
}

/// Format a nanosecond timestamp in the given timezone as RFC 3339.
/// Trailing zeros in the fractional part are stripped.
pub fn format_rfc3339(ts: Timestamp, tz: &jiff::tz::TimeZone) -> String {
    let Ok(stamp) = jiff::Timestamp::from_nanosecond(ts as i128) else {
        return "<out-of-range>".to_string();
    };
    let zoned = stamp.to_zoned(tz.clone());
    let dt = zoned.datetime();
    let mut out = String::with_capacity(35);
    let _ = write!(
        out,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
    );
    let nanos = dt.subsec_nanosecond();
    if nanos != 0 {
        let _ = write!(out, ".{nanos:09}");
        while out.ends_with('0') {
            out.pop();
        }
    }
    let off = zoned.offset();
    if off.is_zero() {
        out.push('Z');
    } else {
        let total = off.seconds();
        let sign = if total < 0 { '-' } else { '+' };
        let abs = total.unsigned_abs();
        let _ = write!(out, "{sign}{:02}:{:02}", abs / 3600, (abs / 60) % 60);
    }
    out
}

/// Format a duration in nanoseconds as a compact `[Nd][Nh][Nm]Ns` string.
/// Always emits at least the seconds component.
pub fn format_duration(nanos: i64) -> String {
    let neg = nanos < 0;
    let mut secs = nanos.unsigned_abs() / 1_000_000_000;
    let days = secs / 86_400;
    secs %= 86_400;
    let h = secs / 3600;
    secs %= 3600;
    let m = secs / 60;
    let s = secs % 60;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if days > 0 {
        let _ = write!(out, "{days}d");
    }
    if h > 0 || days > 0 {
        let _ = write!(out, "{h}h");
    }
    if m > 0 || h > 0 || days > 0 {
        let _ = write!(out, "{m}m");
    }
    let _ = write!(out, "{s}s");
    out
}

/// Howard Hinnant's days_from_civil. Returns days since 1970-01-01 (negative
/// for dates before the epoch). Valid for any proleptic Gregorian date.
fn days_from_civil(y: i32, m: i32, d: i32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era as i64 * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch() {
        assert_eq!(parse_rfc3339_nanos("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_nanos("1970-01-01T00:00:00.000000000Z"),
            Some(0)
        );
    }

    #[test]
    fn one_second() {
        assert_eq!(
            parse_rfc3339_nanos("1970-01-01T00:00:01Z"),
            Some(1_000_000_000)
        );
    }

    #[test]
    fn fractional() {
        assert_eq!(
            parse_rfc3339_nanos("1970-01-01T00:00:00.5Z"),
            Some(500_000_000)
        );
        assert_eq!(
            parse_rfc3339_nanos("1970-01-01T00:00:00.000000001Z"),
            Some(1)
        );
    }

    #[test]
    fn no_tz_means_utc() {
        // No tz suffix is treated as UTC (for our purposes).
        assert_eq!(parse_rfc3339_nanos("1970-01-01T00:00:00"), Some(0));
    }

    #[test]
    fn positive_offset() {
        // 2026-04-24T20:09:03+02:00 = 2026-04-24T18:09:03Z
        let with_offset = parse_rfc3339_nanos("2026-04-24T20:09:03+02:00").unwrap();
        let utc = parse_rfc3339_nanos("2026-04-24T18:09:03Z").unwrap();
        assert_eq!(with_offset, utc);
    }

    #[test]
    fn negative_offset() {
        let with_offset = parse_rfc3339_nanos("2026-04-24T16:09:03-02:00").unwrap();
        let utc = parse_rfc3339_nanos("2026-04-24T18:09:03Z").unwrap();
        assert_eq!(with_offset, utc);
    }

    #[test]
    fn compact_offset() {
        let with_offset = parse_rfc3339_nanos("2026-04-24T20:09:03+0200").unwrap();
        let utc = parse_rfc3339_nanos("2026-04-24T18:09:03Z").unwrap();
        assert_eq!(with_offset, utc);
    }

    #[test]
    fn known_value_2000() {
        // 2000-01-01T00:00:00Z = 946684800 secs since epoch
        assert_eq!(
            parse_rfc3339_nanos("2000-01-01T00:00:00Z"),
            Some(946_684_800 * 1_000_000_000)
        );
    }

    #[test]
    fn known_value_test_fixture() {
        // From src/logfmt.rs:307 fixture.
        let t = parse_rfc3339_nanos("2026-04-24T18:09:03.0696Z").unwrap();
        let base = parse_rfc3339_nanos("2026-04-24T18:09:03Z").unwrap();
        assert_eq!(t - base, 69_600_000); // .0696 secs = 69.6ms
    }

    #[test]
    fn malformed() {
        assert_eq!(parse_rfc3339_nanos(""), None);
        assert_eq!(parse_rfc3339_nanos("not a date"), None);
        assert_eq!(parse_rfc3339_nanos("1970-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_nanos("1970-01-32T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_nanos("1970-01-01T25:00:00Z"), None);
        assert_eq!(parse_rfc3339_nanos("1970-01-01T00:60:00Z"), None);
        assert_eq!(parse_rfc3339_nanos("1970-01-01T00:00:00.Z"), None);
        assert_eq!(parse_rfc3339_nanos("1970-01-01T00:00:00.1234567890Z"), None);
        assert_eq!(parse_rfc3339_nanos("1970-01-01T00:00:00+25:00"), None);
        assert_eq!(parse_rfc3339_nanos("1970-01-01T00:00:00Zextra"), None);
    }

    #[test]
    fn rejects_invalid_calendar_dates() {
        // day=0 is never valid.
        assert_eq!(parse_rfc3339_nanos("2026-01-00T00:00:00Z"), None);
        // 30-day months reject day 31.
        assert_eq!(parse_rfc3339_nanos("2026-04-31T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_nanos("2026-06-31T00:00:00Z"), None);
        // February in a non-leap year tops out at 28.
        assert_eq!(parse_rfc3339_nanos("2026-02-29T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_nanos("2026-02-30T00:00:00Z"), None);
        // Year divisible by 100 but not 400 is not a leap year.
        assert_eq!(parse_rfc3339_nanos("1900-02-29T00:00:00Z"), None);
    }

    #[test]
    fn accepts_leap_day() {
        // Feb 29 only valid in leap years.
        assert!(parse_rfc3339_nanos("2024-02-29T00:00:00Z").is_some());
        // Year divisible by 400 is a leap year.
        assert!(parse_rfc3339_nanos("2000-02-29T00:00:00Z").is_some());
        // 31 January, 30 April, etc. are valid.
        assert!(parse_rfc3339_nanos("2026-01-31T00:00:00Z").is_some());
        assert!(parse_rfc3339_nanos("2026-04-30T00:00:00Z").is_some());
    }

    #[test]
    fn extract_from_pairs() {
        let pairs = vec![
            ("level", "info"),
            ("time", "2026-04-24T18:09:03Z"),
            ("msg", "hi"),
        ];
        let t = extract_timestamp(&pairs).unwrap();
        assert_eq!(t, parse_rfc3339_nanos("2026-04-24T18:09:03Z").unwrap());
    }

    #[test]
    fn extract_strips_quotes() {
        let pairs = vec![("time", "\"2026-04-24T18:09:03Z\"")];
        assert!(extract_timestamp(&pairs).is_some());
    }

    #[test]
    fn resolve_full_rfc3339() {
        let t = resolve_bound("2026-04-24T18:09:03Z", None, None, None).unwrap();
        assert_eq!(t, parse_rfc3339_nanos("2026-04-24T18:09:03Z").unwrap());
    }

    #[test]
    fn resolve_date_only() {
        let t = resolve_bound("2026-04-24", None, None, None).unwrap();
        assert_eq!(t, parse_rfc3339_nanos("2026-04-24T00:00:00Z").unwrap());
    }

    #[test]
    fn resolve_time_only_hhmm() {
        let anchor = parse_rfc3339_nanos("2026-04-24T05:30:11Z").unwrap();
        let t = resolve_bound("18:00", None, None, Some(anchor)).unwrap();
        assert_eq!(t, parse_rfc3339_nanos("2026-04-24T18:00:00Z").unwrap());
    }

    #[test]
    fn resolve_time_only_with_seconds_and_frac() {
        let anchor = parse_rfc3339_nanos("2026-04-24T00:00:00Z").unwrap();
        let t = resolve_bound("18:00:00.5", None, None, Some(anchor)).unwrap();
        assert_eq!(t, parse_rfc3339_nanos("2026-04-24T18:00:00.5Z").unwrap());
    }

    #[test]
    fn resolve_time_only_no_anchor_errors() {
        assert!(resolve_bound("18:00", None, None, None).is_err());
    }

    #[test]
    fn resolve_start_end_no_offset() {
        let first = parse_rfc3339_nanos("2026-04-24T10:00:00Z").unwrap();
        let last = parse_rfc3339_nanos("2026-04-24T20:00:00Z").unwrap();
        assert_eq!(
            resolve_bound("start", Some(first), Some(last), None).unwrap(),
            first
        );
        assert_eq!(
            resolve_bound("end", Some(first), Some(last), None).unwrap(),
            last
        );
    }

    #[test]
    fn resolve_start_plus_duration() {
        let first = parse_rfc3339_nanos("2026-04-24T10:00:00Z").unwrap();
        let t = resolve_bound("start+1h", Some(first), None, None).unwrap();
        assert_eq!(t, parse_rfc3339_nanos("2026-04-24T11:00:00Z").unwrap());
    }

    #[test]
    fn resolve_end_minus_duration() {
        let last = parse_rfc3339_nanos("2026-04-24T20:00:00Z").unwrap();
        let t = resolve_bound("end-30m", None, Some(last), None).unwrap();
        assert_eq!(t, parse_rfc3339_nanos("2026-04-24T19:30:00Z").unwrap());
    }

    #[test]
    fn resolve_start_minus_days() {
        let first = parse_rfc3339_nanos("2026-04-24T00:00:00Z").unwrap();
        let t = resolve_bound("start-2d", Some(first), None, None).unwrap();
        assert_eq!(t, parse_rfc3339_nanos("2026-04-22T00:00:00Z").unwrap());
    }

    #[test]
    fn resolve_unknown_unit_errors() {
        let first = parse_rfc3339_nanos("2026-04-24T10:00:00Z").unwrap();
        assert!(resolve_bound("start+1y", Some(first), None, None).is_err());
        // Two-letter near-misses also fail.
        assert!(resolve_bound("start+5 mn", Some(first), None, None).is_err());
        assert!(resolve_bound("start+1ms", Some(first), None, None).is_err());
    }

    #[test]
    fn resolve_word_units() {
        let first = parse_rfc3339_nanos("2026-04-24T10:00:00Z").unwrap();
        let last = parse_rfc3339_nanos("2026-04-24T20:00:00Z").unwrap();
        let cases: &[(&str, &str)] = &[
            ("start+5 min", "start+5m"),
            ("start+5min", "start+5m"),
            ("start+5mins", "start+5m"),
            ("start+5 minutes", "start+5m"),
            ("start+1 hour", "start+1h"),
            ("start+2 hours", "start+2h"),
            ("end-2 day", "end-2d"),
            ("end-2 days", "end-2d"),
            ("end-2days", "end-2d"),
            ("start+30 seconds", "start+30s"),
            ("start + 30 SECONDS", "start+30s"),
        ];
        for (got, want) in cases {
            let a = resolve_bound(got, Some(first), Some(last), None)
                .unwrap_or_else(|e| panic!("{got}: {e}"));
            let b = resolve_bound(want, Some(first), Some(last), None).unwrap();
            assert_eq!(a, b, "{got} should equal {want}");
        }
    }

    #[test]
    fn resolve_garbage_errors() {
        assert!(resolve_bound("hello", None, None, None).is_err());
    }

    #[test]
    fn resolve_anchor_missing_errors() {
        assert!(resolve_bound("start+1h", None, None, None).is_err());
        assert!(resolve_bound("end", None, None, None).is_err());
    }
}
