//! Time without a chrono dependency: ISO timestamps for storage and logs,
//! and enough civil-date math to answer "how many days ago was that".

use std::time::{SystemTime, UNIX_EPOCH};

pub fn timestamp() -> String {
    format_epoch_seconds(now_epoch_seconds())
}

pub fn timestamp_days_ago(days: u64) -> String {
    format_epoch_seconds(now_epoch_seconds().saturating_sub(days * 86_400))
}

/// Whole days between a stored ISO timestamp and now; unparseable or future
/// timestamps count as today.
pub fn days_since(timestamp: &str) -> u64 {
    let Some(then) = epoch_days_of(timestamp) else {
        return 0;
    };
    let today = now_epoch_seconds() / 86_400;
    today.saturating_sub(then)
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs()
}

fn format_epoch_seconds(seconds: u64) -> String {
    let (year, month, day) = civil_date(seconds / 86_400);
    let hour = seconds / 3600 % 24;
    let minute = seconds / 60 % 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn epoch_days_of(timestamp: &str) -> Option<u64> {
    let date = timestamp.get(0..10)?;
    let mut parts = date.split('-');
    let year: u64 = parts.next()?.parse().ok()?;
    let month: u64 = parts.next()?.parse().ok()?;
    let day: u64 = parts.next()?.parse().ok()?;
    days_from_civil(year, month, day)
}

fn civil_date(days_since_epoch: u64) -> (u64, u64, u64) {
    let days = days_since_epoch + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || year < 1970 {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let year_of_era = year % 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_round_trips_through_epoch_days() {
        for days in [0, 19_723, 20_700, 25_000] {
            let (year, month, day) = civil_date(days);
            assert_eq!(days_from_civil(year, month, day), Some(days));
        }
    }

    #[test]
    fn days_since_handles_the_expected_shapes() {
        assert_eq!(days_since(&timestamp()), 0);
        assert!(days_since("2020-01-01T00:00:00Z") > 365);
        assert_eq!(days_since("not-a-date"), 0);
    }
}
