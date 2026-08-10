//! Minimal civil-date <-> unix-epoch conversion, so profiles can express date
//! cutoffs and envelopes can render timestamps without pulling in a calendar
//! crate. Algorithms are Howard Hinnant's `days_from_civil` / `civil_from_days`.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DateError {
    #[error("`{0}` is not a YYYY-MM-DD date")]
    Unparseable(String),
    #[error("`{0}` has an out-of-range month or day")]
    OutOfRange(String),
}

const SECS_PER_DAY: i64 = 86_400;

/// Days since 1970-01-01 for a civil date. Valid for all i64-representable dates.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = i64::from((m + 9) % 12); // [0, 11], March = 0
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Civil date for days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse a `YYYY-MM-DD` string into unix-epoch seconds at midnight UTC.
pub fn parse_ymd_epoch(s: &str) -> Result<i64, DateError> {
    let mut parts = s.splitn(3, '-');
    let (y, m, d) = match (parts.next(), parts.next(), parts.next()) {
        (Some(y), Some(m), Some(d)) => (
            y.parse::<i64>()
                .map_err(|_| DateError::Unparseable(s.to_string()))?,
            m.parse::<u32>()
                .map_err(|_| DateError::Unparseable(s.to_string()))?,
            d.parse::<u32>()
                .map_err(|_| DateError::Unparseable(s.to_string()))?,
        ),
        _ => return Err(DateError::Unparseable(s.to_string())),
    };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(DateError::OutOfRange(s.to_string()));
    }
    Ok(days_from_civil(y, m, d) * SECS_PER_DAY)
}

/// Render unix-epoch seconds as a `YYYY-MM-DD` string (UTC).
pub fn epoch_to_ymd(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(SECS_PER_DAY));
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero_is_1970() {
        assert_eq!(parse_ymd_epoch("1970-01-01"), Ok(0));
        assert_eq!(epoch_to_ymd(0), "1970-01-01");
    }

    #[test]
    fn known_dates_roundtrip() {
        let cases = [
            ("2000-03-01", 951_868_800),
            ("2015-01-01", 1_420_070_400),
            ("2026-08-09", 1_786_233_600),
            ("1969-12-31", -86_400),
        ];
        for (s, epoch) in cases {
            assert_eq!(parse_ymd_epoch(s), Ok(epoch), "parsing {s}");
            assert_eq!(epoch_to_ymd(epoch), s, "rendering {epoch}");
        }
    }

    #[test]
    fn renders_mid_day_times_as_their_date() {
        assert_eq!(epoch_to_ymd(1_420_070_400 + 3600 * 13), "2015-01-01");
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(
            parse_ymd_epoch("yesterday"),
            Err(DateError::Unparseable(_))
        ));
        assert!(matches!(
            parse_ymd_epoch("2015-13-01"),
            Err(DateError::OutOfRange(_))
        ));
        assert!(matches!(parse_ymd_epoch("2015"), Err(DateError::Unparseable(_))));
    }
}
