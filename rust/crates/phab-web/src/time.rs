//! Dates on the pages, in UTC until user time zones are ported.

/// `epoch` as `2026-11-13 15:04 UTC`.
pub fn format_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3600,
        seconds % 3600 / 60
    )
}

/// `date('F jS, g:i:s A', $epoch)` in UTC, like "November 14th, 10:13:20
/// PM".
pub fn format_long_utc(epoch: i64) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let (_, month, day) = civil_from_days(epoch.div_euclid(86_400));
    let seconds = epoch.rem_euclid(86_400);
    let suffix = match (day % 10, day) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    let hour = seconds / 3600;
    let twelve = match hour % 12 {
        0 => 12,
        other => other,
    };
    format!(
        "{} {day}{suffix}, {twelve}:{:02}:{:02} {}",
        MONTHS[usize::try_from(month - 1).unwrap_or_default()],
        seconds % 3600 / 60,
        seconds % 60,
        if hour < 12 { "AM" } else { "PM" }
    )
}

/// The proleptic Gregorian date of a day count since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc() {
        assert_eq!(format_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(format_long_utc(1_700_000_000), "November 14th, 10:13:20 PM");
        assert_eq!(format_long_utc(951_782_400), "February 29th, 12:00:00 AM");
        assert_eq!(format_long_utc(86_400 + 3_600), "January 2nd, 1:00:00 AM");
    }
}
