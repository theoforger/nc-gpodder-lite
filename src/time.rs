//! Timestamp handling compatible with nextcloud-gpodder, which parses client
//! timestamps with `new DateTime($ts, new DateTimeZone("UTC"))` and renders them
//! with `format("c")`.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};

/// Parses a client-supplied timestamp into a unix epoch. Times without an
/// explicit offset are treated as UTC.
pub fn parse_timestamp(input: &str) -> Option<i64> {
    let s = input.trim();
    if let Some(epoch) = s.strip_prefix('@') {
        return epoch.parse().ok();
    }

    let s = s.replacen(' ', "T", 1);
    let s = s.strip_suffix("UTC").map(str::trim_end).unwrap_or(&s);

    const WITH_OFFSET: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M%:z",
    ];
    for fmt in WITH_OFFSET {
        if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
            return Some(dt.timestamp());
        }
    }

    let naive = s.strip_suffix(['Z', 'z']).unwrap_or(s);
    const NAIVE: &[&str] = &["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"];
    for fmt in NAIVE {
        if let Ok(dt) = NaiveDateTime::parse_from_str(naive, fmt) {
            return Some(dt.and_utc().timestamp());
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(naive, "%Y-%m-%d") {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp());
    }
    None
}

/// Formats an epoch like PHP's `DateTime::createFromFormat("U", $epoch)->format("c")`.
pub fn format_timestamp(epoch: i64) -> String {
    DateTime::<Utc>::from_timestamp(epoch, 0)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S+00:00")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_client_formats() {
        assert_eq!(parse_timestamp("2009-12-12T09:00:00"), Some(1260608400));
        assert_eq!(parse_timestamp("2009-12-12T09:00:00Z"), Some(1260608400));
        assert_eq!(parse_timestamp("2009-12-12T09:00:00.123"), Some(1260608400));
        assert_eq!(parse_timestamp("2009-12-12 09:00:00"), Some(1260608400));
        assert_eq!(
            parse_timestamp("2009-12-12T10:00:00+01:00"),
            Some(1260608400)
        );
        assert_eq!(
            parse_timestamp("2009-12-12T10:00:00+0100"),
            Some(1260608400)
        );
        assert_eq!(parse_timestamp("2009-12-12"), Some(1260576000));
        assert_eq!(parse_timestamp("@1260608400"), Some(1260608400));
        assert_eq!(parse_timestamp("yesterday-ish"), None);
    }

    #[test]
    fn formats_like_php_c() {
        assert_eq!(format_timestamp(1260608400), "2009-12-12T09:00:00+00:00");
    }
}
