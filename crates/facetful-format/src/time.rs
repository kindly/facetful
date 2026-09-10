//! Calendar math and ISO-8601 parsing for the temporal column types.
//! Encoding decision (design doc): `Date` = days since 1970-01-01 (i32 on
//! disk), `Timestamp` = milliseconds since the epoch (i64, UTC) — the same
//! value JS `Date.getTime()` produces, so the wasm boundary needs no
//! conversion. Algorithms are Howard Hinnant's civil-days routines.

pub const MS_PER_DAY: i64 = 86_400_000;

/// days since epoch -> (year, month 1-12, day 1-31), proleptic Gregorian.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + (m <= 2) as i64, m, d)
}

/// (year, month 1-12, day 1-31) -> days since epoch.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - (m <= 2) as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (if m > 2 { m - 3 } else { m + 9 }) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn digits(s: &[u8], n: usize) -> Option<i64> {
    if s.len() < n {
        return None;
    }
    let mut v = 0i64;
    for &b in &s[..n] {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as i64;
    }
    Some(v)
}

/// Strict "YYYY-MM-DD" -> days since epoch.
pub fn parse_date(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, m, d) = (digits(b, 4)?, digits(&b[5..], 2)?, digits(&b[8..], 2)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m as u32, d as u32))
}

/// "YYYY-MM-DD[ T]HH:MM[:SS[.fff]]" (or a bare date) -> ms since epoch, UTC.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() == 10 {
        return Some(parse_date(s)? * MS_PER_DAY);
    }
    if b.len() < 16 || (b[10] != b' ' && b[10] != b'T') {
        return None;
    }
    let days = parse_date(&s[..10])?;
    let (h, min) = (digits(&b[11..], 2)?, digits(&b[14..], 2)?);
    if b[13] != b':' || h > 23 || min > 59 {
        return None;
    }
    let mut ms = (h * 3600 + min * 60) * 1000;
    if b.len() >= 19 && b[16] == b':' {
        let sec = digits(&b[17..], 2)?;
        if sec > 59 {
            return None;
        }
        ms += sec * 1000;
        if b.len() >= 23 && b[19] == b'.' {
            ms += digits(&b[20..], 3)?;
        } else if b.len() != 19 {
            return None;
        }
    } else if b.len() != 16 {
        return None;
    }
    Some(days * MS_PER_DAY + ms)
}

/// days -> "YYYY-MM-DD"
pub fn format_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// ms -> "YYYY-MM-DD HH:MM:SS" (fractional ms dropped)
pub fn format_timestamp(ms: i64) -> String {
    let days = ms.div_euclid(MS_PER_DAY);
    let t = ms.rem_euclid(MS_PER_DAY) / 1000;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip_and_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2020, 1, 15), 18276);
        assert_eq!(civil_from_days(18276), (2020, 1, 15));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // leap years incl. century rules
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
        for days in [-1_000_000i64, -1, 0, 1, 18276, 1_000_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    #[test]
    fn parsing() {
        assert_eq!(parse_date("2020-01-15"), Some(18276));
        assert_eq!(parse_date("2020-13-01"), None);
        assert_eq!(parse_date("2020-1-15"), None);
        assert_eq!(parse_timestamp("2020-01-15 10:30"), Some(18276 * MS_PER_DAY + 37_800_000));
        assert_eq!(parse_timestamp("2020-01-15T10:30:05"), Some(18276 * MS_PER_DAY + 37_805_000));
        assert_eq!(
            parse_timestamp("2020-01-15 10:30:05.250"),
            Some(18276 * MS_PER_DAY + 37_805_250)
        );
        assert_eq!(parse_timestamp("2020-01-15"), Some(18276 * MS_PER_DAY));
        assert_eq!(parse_timestamp("2020-01-15 25:00"), None);
        assert_eq!(format_date(18276), "2020-01-15");
        assert_eq!(format_timestamp(18276 * MS_PER_DAY + 37_805_000), "2020-01-15 10:30:05");
    }
}
