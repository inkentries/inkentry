use anyhow::{Context, Result};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};

/// Parse an optional `--as-of` argument into a Unix timestamp.
pub fn parse_as_of(s: Option<&str>) -> Result<Option<i64>> {
    match s {
        None => Ok(None),
        Some(v) => parse_iso8601_to_epoch(v)
            .with_context(|| {
                format!(
                    "parsing --as-of '{v}': expected ISO 8601 (e.g. 2026-03-15 or 2026-03-15T10:00:00)"
                )
            })
            .map(Some),
    }
}

/// Parse an ISO 8601 date or datetime string to a Unix epoch (seconds, UTC).
///
/// Accepted forms:
///   - `YYYY-MM-DD`
///   - `YYYY-MM-DDTHH:MM[:SS]`  (optional trailing `Z`)
///   - `YYYY-MM-DD HH:MM[:SS]`  (space-separated variant)
fn parse_iso8601_to_epoch(s: &str) -> Result<i64> {
    let trimmed = s.trim_end_matches('Z');
    let normalized = trimmed.replacen(' ', "T", 1);

    let dt: NaiveDateTime =
        if let Ok(dt) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S") {
            dt
        } else if let Ok(dt) = NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M") {
            dt
        } else if let Ok(d) = NaiveDate::parse_from_str(&normalized, "%Y-%m-%d") {
            NaiveDateTime::new(d, NaiveTime::MIN)
        } else {
            anyhow::bail!("expected ISO 8601 (YYYY-MM-DD or YYYY-MM-DDTHH:MM[:SS]) in '{s}'");
        };

    Ok(Utc.from_utc_datetime(&dt).timestamp())
}
