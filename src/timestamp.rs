//! Fast RFC 3339 timestamp parsing into nanoseconds since the Unix epoch.

pub type Timestamp = i64;

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

    if month < 1 || month > 12 || day < 1 || day > 31 || hour > 23 || minute > 59 || second > 60 {
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
        let digits = idx - frac_start;
        if digits == 0 || digits > 9 {
            return None;
        }
        let mut acc: i64 = 0;
        for &c in &b[frac_start..idx] {
            acc = acc * 10 + (c - b'0') as i64;
        }
        // Scale to nanoseconds: pad with zeros to 9 digits.
        for _ in digits..9 {
            acc *= 10;
        }
        nanos_frac = acc;
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

    Some(utc_secs.checked_mul(1_000_000_000)?.checked_add(nanos_frac)?)
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

fn strip_quotes(s: &str) -> &str {
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
        assert_eq!(parse_rfc3339_nanos("1970-01-01T00:00:00.000000000Z"), Some(0));
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
}
