// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Wall-clock times for run ids and bundles, always in UTC.
//!
//! Bundles store UTC so runs from machines in different places sort and
//! compare without a time zone database; the runner page shows them in the
//! viewer's local time. The date arithmetic is Howard Hinnant's
//! `civil_from_days` (public domain, <https://howardhinnant.github.io/date_algorithms.html>),
//! which saves a dependency for one function.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A moment, as whole seconds since the Unix epoch, in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Utc {
    seconds: u64,
}

impl Utc {
    /// The current time.
    pub fn now() -> Self {
        let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
        Self { seconds: since_epoch.as_secs() }
    }

    /// The time `seconds` after the Unix epoch.
    #[cfg(test)]
    pub fn from_seconds(seconds: u64) -> Self {
        Self { seconds }
    }

    /// Year, month, day, hour, minute and second.
    fn fields(self) -> (i64, u32, u32, u32, u32, u32) {
        let days = (self.seconds / 86_400) as i64;
        let in_day = (self.seconds % 86_400) as u32;
        let (year, month, day) = civil_from_days(days);
        (year, month, day, in_day / 3600, in_day / 60 % 60, in_day % 60)
    }

    /// RFC 3339, as `2026-11-07T15:30:12Z`.
    pub fn rfc3339(self) -> String {
        let (year, month, day, hour, minute, second) = self.fields();
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    }

    /// The date alone, as `2026-11-07`: a bundle's folder.
    pub fn date(self) -> String {
        let (year, month, day, ..) = self.fields();
        format!("{year:04}-{month:02}-{day:02}")
    }

    /// Compact, for run ids: `20261107-153012`.
    pub fn compact(self) -> String {
        let (year, month, day, hour, minute, second) = self.fields();
        format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
    }
}

/// The civil date `days` after 1970-01-01, in the proleptic Gregorian
/// calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153; // March is 0
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_moments() {
        assert_eq!(Utc::from_seconds(0).rfc3339(), "1970-01-01T00:00:00Z");
        // 2026-11-07 15:30:12 UTC, and the leap day of 2024.
        assert_eq!(Utc::from_seconds(1_794_065_412).rfc3339(), "2026-11-07T15:30:12Z");
        assert_eq!(Utc::from_seconds(1_794_065_412).compact(), "20261107-153012");
        assert_eq!(Utc::from_seconds(1_709_164_800).date(), "2024-02-29");
    }
}
