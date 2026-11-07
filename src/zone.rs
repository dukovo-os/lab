// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Local time in a named time zone, for schedules: "every night at 1:00
//! AM" means 1:00 AM in the lab's zone (America/Denver unless the
//! schedules file says otherwise), summer and winter alike.
//!
//! The rules come from the system's own time zone database
//! (`/usr/share/zoneinfo`, or `$TZDIR`): the last line of a zone's file
//! is a POSIX TZ rule, such as `MST7MDT,M3.2.0,M11.1.0`, which says how
//! the zone keeps time from its last recorded change on, and so for now
//! and the future, which is all a schedule asks about. Reading that one
//! line saves a time zone dependency; when a country changes its rules,
//! the system's tzdata update brings the new line.

use std::path::PathBuf;

use crate::clock::Utc;

/// The zone schedules use unless the schedules file names another.
pub const DEFAULT: &str = "America/Denver";

/// A zone's rule: its standard offset and, if it keeps summer time,
/// when that starts and ends.
#[derive(Clone, Debug, PartialEq)]
pub struct Zone {
    /// The zone's name, such as `America/Denver`.
    pub name: String,
    /// The standard time's abbreviation, such as `MST`.
    standard_name: String,
    /// Seconds to add to UTC for standard time (negative west of
    /// Greenwich).
    standard: i64,
    summer: Option<Summer>,
}

/// Summer time: its abbreviation, offset, and the rules for its start
/// and end, each in the local time in force just before it.
#[derive(Clone, Debug, PartialEq)]
struct Summer {
    name: String,
    offset: i64,
    start: Change,
    end: Change,
}

/// A yearly change of offset: the `week`th `weekday` of `month` (week 5
/// means the last), at `seconds` after local midnight.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Change {
    month: u32,
    week: u32,
    /// 0 is Sunday, as POSIX counts.
    weekday: u32,
    seconds: i64,
}

impl Zone {
    /// Reads zone `name` from the system's time zone database.
    pub fn load(name: &str) -> Result<Zone, String> {
        // A name, never a path out of the database.
        let plain = !name.is_empty()
            && !name.starts_with('/')
            && name.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part.chars().all(|c| c.is_ascii_alphanumeric() || "_+-".contains(c))
            });
        if !plain {
            return Err(format!("\"{name}\" is not a time zone name, such as America/Denver"));
        }
        let folder = std::env::var_os("TZDIR")
            .map_or_else(|| PathBuf::from("/usr/share/zoneinfo"), PathBuf::from);
        let file = folder.join(name);
        let bytes = std::fs::read(&file).map_err(|err| {
            format!(
                "reading the time zone {name} ({}): {err}; is tzdata installed?",
                file.display()
            )
        })?;
        let rule = footer(&bytes).ok_or_else(|| {
            format!("{} has no TZ rule at its end; is it a time zone file?", file.display())
        })?;
        Zone::parse(name, rule)
    }

    /// Reads a POSIX TZ rule, such as `MST7MDT,M3.2.0,M11.1.0`.
    pub fn parse(name: &str, rule: &str) -> Result<Zone, String> {
        let bad = || format!("the TZ rule \"{rule}\" of {name} is not one this harness reads");
        let mut rest = rule;
        let standard_name = abbreviation(&mut rest).ok_or_else(bad)?;
        // POSIX offsets count west of Greenwich as positive: MST7 is UTC-7.
        let standard = -clock_time(&mut rest).ok_or_else(bad)?;
        if rest.is_empty() {
            return Ok(Zone { name: name.into(), standard_name, standard, summer: None });
        }
        let summer_name = abbreviation(&mut rest).ok_or_else(bad)?;
        // Summer time is an hour ahead unless the rule says otherwise.
        let offset = if rest.starts_with(',') {
            standard + 3600
        } else {
            -clock_time(&mut rest).ok_or_else(bad)?
        };
        let mut change = || -> Option<Change> {
            rest = rest.strip_prefix(',')?;
            let change = month_rule(&mut rest)?;
            Some(change)
        };
        let (start, end) = (change().ok_or_else(bad)?, change().ok_or_else(bad)?);
        if !rest.is_empty() {
            return Err(bad());
        }
        Ok(Zone {
            name: name.into(),
            standard_name,
            standard,
            summer: Some(Summer { name: summer_name, offset, start, end }),
        })
    }

    /// The zone's everyday short name: "MT" for MST and MDT, else the
    /// standard abbreviation.
    pub fn short_name(&self) -> String {
        let standard = &self.standard_name;
        match (&self.summer, standard.strip_suffix("ST")) {
            (Some(summer), Some(stem)) if summer.name == format!("{stem}DT") => format!("{stem}T"),
            _ => standard.clone(),
        }
    }

    /// Seconds to add to UTC to get local time at `at`.
    pub fn offset_at(&self, at: Utc) -> i64 {
        let Some(summer) = &self.summer else { return self.standard };
        let seconds = at.seconds() as i64;
        let (year, _, _) = civil(seconds + self.standard);
        // Each change happens at a local time, read on the clock in force
        // just before it: the start on standard time, the end on summer
        // time.
        let start = summer.start.utc_in(year, self.standard);
        let end = summer.end.utc_in(year, summer.offset);
        let in_summer = if start < end {
            (start..end).contains(&seconds)
        } else {
            // The southern hemisphere: summer spans the new year.
            seconds >= start || seconds < end
        };
        if in_summer { summer.offset } else { self.standard }
    }

    /// The UTC moment of local time `minute` (minutes after midnight) on
    /// the local day `day` (days since 1970-01-01). A time skipped when
    /// clocks go forward is read on the old clock, so it falls just after
    /// the jump; a time that happens twice when clocks go back is the
    /// first.
    pub fn moment(&self, day: i64, minute: u32) -> Utc {
        let local = day * 86_400 + i64::from(minute) * 60;
        let mut offsets = vec![self.standard];
        if let Some(summer) = &self.summer {
            offsets.push(summer.offset);
        }
        let fits = offsets
            .iter()
            .map(|offset| local - offset)
            .filter(|&utc| self.offset_at(Utc::from_seconds(utc.max(0) as u64)) == local - utc)
            .min();
        let utc = fits.unwrap_or(local - self.standard);
        Utc::from_seconds(utc.max(0) as u64)
    }

    /// The local day (days since 1970-01-01) at `at`.
    pub fn day_of(&self, at: Utc) -> i64 {
        (at.seconds() as i64 + self.offset_at(at)).div_euclid(86_400)
    }
}

/// The weekday of day `day` after 1970-01-01, a Thursday: 0 is Monday.
pub fn weekday(day: i64) -> u32 {
    (day + 3).rem_euclid(7) as u32
}

impl Change {
    /// The UTC second this change happens in `year`, on a clock `offset`
    /// seconds ahead of UTC.
    fn utc_in(self, year: i64, offset: i64) -> i64 {
        let first = days_from_civil(year, self.month, 1);
        // The first `weekday` of the month, then the week asked for; week
        // 5 is the last, so it steps back if it runs into the next month.
        let first_weekday = (first + 4).rem_euclid(7) as u32; // 0 is Sunday
        let mut day = first + i64::from((self.weekday + 7 - first_weekday) % 7);
        day += 7 * i64::from(self.week - 1);
        let next_month = if self.month == 12 {
            days_from_civil(year + 1, 1, 1)
        } else {
            days_from_civil(year, self.month + 1, 1)
        };
        while day >= next_month {
            day -= 7;
        }
        day * 86_400 + self.seconds - offset
    }
}

/// The TZ rule on the last line of a zone file (version 2 and later).
fn footer(bytes: &[u8]) -> Option<&str> {
    if !bytes.starts_with(b"TZif") {
        return None;
    }
    let body = bytes.strip_suffix(b"\n")?;
    let start = body.iter().rposition(|&byte| byte == b'\n')? + 1;
    let rule = std::str::from_utf8(&body[start..]).ok()?;
    (!rule.is_empty()).then_some(rule)
}

/// An abbreviation: letters (`MST`), or anything in angle brackets
/// (`<-03>`).
fn abbreviation(rest: &mut &str) -> Option<String> {
    let (name, after) = if let Some(quoted) = rest.strip_prefix('<') {
        let end = quoted.find('>')?;
        (&quoted[..end], &quoted[end + 1..])
    } else {
        let end = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
        (&rest[..end], &rest[end..])
    };
    if name.len() < 3 {
        return None;
    }
    *rest = after;
    Some(name.to_string())
}

/// `[+|-]hh[:mm[:ss]]`, in seconds.
fn clock_time(rest: &mut &str) -> Option<i64> {
    let sign = match rest.as_bytes().first() {
        Some(b'-') => {
            *rest = &rest[1..];
            -1
        }
        Some(b'+') => {
            *rest = &rest[1..];
            1
        }
        _ => 1,
    };
    let mut seconds = 0;
    for (index, scale) in [3600, 60, 1].into_iter().enumerate() {
        if index > 0 {
            match rest.strip_prefix(':') {
                Some(after) => *rest = after,
                None => break,
            }
        }
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if end == 0 || end > 3 {
            return None;
        }
        seconds += rest[..end].parse::<i64>().ok()? * scale;
        *rest = &rest[end..];
    }
    Some(sign * seconds)
}

/// `Mm.w.d[/time]`: the only form of change today's zone files use.
fn month_rule(rest: &mut &str) -> Option<Change> {
    *rest = rest.strip_prefix('M')?;
    let mut number = |stop: Option<char>| -> Option<u32> {
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let value = rest[..end].parse().ok()?;
        *rest = &rest[end..];
        if let Some(stop) = stop {
            *rest = rest.strip_prefix(stop)?;
        }
        Some(value)
    };
    let (month, week, weekday) = (number(Some('.'))?, number(Some('.'))?, number(None)?);
    if !(1..=12).contains(&month) || !(1..=5).contains(&week) || weekday > 6 {
        return None;
    }
    // At 2:00 AM unless the rule says otherwise.
    let seconds = match rest.strip_prefix('/') {
        Some(after) => {
            *rest = after;
            clock_time(rest)?
        }
        None => 7200,
    };
    Some(Change { month, week, weekday, seconds })
}

/// The civil date (year, month, day) of local second `seconds`.
fn civil(seconds: i64) -> (i64, u32, u32) {
    crate::clock::civil_from_days(seconds.div_euclid(86_400))
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    crate::clock::days_from_civil(year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn denver() -> Zone {
        Zone::parse("America/Denver", "MST7MDT,M3.2.0,M11.1.0").unwrap()
    }

    fn at(text: &str) -> Utc {
        Utc::parse_rfc3339(text).unwrap()
    }

    #[test]
    fn reads_rules_as_zone_files_write_them() {
        let zone = denver();
        assert_eq!(zone.standard, -7 * 3600);
        assert_eq!(zone.short_name(), "MT");
        let utc = Zone::parse("Etc/UTC", "UTC0").unwrap();
        assert_eq!((utc.standard, utc.short_name()), (0, "UTC".to_string()));
        let sydney = Zone::parse("Australia/Sydney", "AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(sydney.standard, 10 * 3600);
        let sao_paulo = Zone::parse("America/Sao_Paulo", "<-03>3").unwrap();
        assert_eq!(sao_paulo.standard, -3 * 3600);
        for rule in ["", "M7", "MST7MDT,M3.2.0", "MST7MDT,J60,M11.1.0", "MST7MDT,M13.2.0,M11.1.0"] {
            assert!(Zone::parse("x", rule).is_err(), "{rule}");
        }
        for name in ["../etc/passwd", "/etc/passwd", "America/../x", ""] {
            assert!(Zone::load(name).unwrap_err().contains("not a time zone name"), "{name}");
        }
    }

    #[test]
    fn keeps_summer_time_from_march_to_november() {
        let zone = denver();
        // 2026: summer time from 8 March 2:00 AM MST to 1 November 2:00 AM MDT.
        assert_eq!(zone.offset_at(at("2026-03-08T08:59:59Z")), -7 * 3600);
        assert_eq!(zone.offset_at(at("2026-03-08T09:00:00Z")), -6 * 3600);
        assert_eq!(zone.offset_at(at("2026-07-01T12:00:00Z")), -6 * 3600);
        assert_eq!(zone.offset_at(at("2026-11-01T07:59:59Z")), -6 * 3600);
        assert_eq!(zone.offset_at(at("2026-11-01T08:00:00Z")), -7 * 3600);
        assert_eq!(zone.offset_at(at("2026-12-25T12:00:00Z")), -7 * 3600);
        // Sydney's summer spans the new year.
        let sydney = Zone::parse("Australia/Sydney", "AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(sydney.offset_at(at("2027-01-15T00:00:00Z")), 11 * 3600);
        assert_eq!(sydney.offset_at(at("2026-07-15T00:00:00Z")), 10 * 3600);
    }

    #[test]
    fn finds_the_moment_of_a_local_time() {
        let zone = denver();
        let day = |text: &str| zone.day_of(at(text));
        // 1:00 AM on 7 November 2026, a Saturday, in winter time... not yet:
        // MDT ends on 1 November, so 7 November is MST.
        let saturday = day("2026-11-07T12:00:00Z");
        assert_eq!(weekday(saturday), 5);
        assert_eq!(zone.moment(saturday, 60), at("2026-11-07T08:00:00Z"));
        // In summer, 1:00 AM MDT is 7:00 UTC.
        assert_eq!(zone.moment(day("2026-07-01T12:00:00Z"), 60), at("2026-07-01T07:00:00Z"));
        // 2:30 AM does not happen on 8 March: it runs just after the jump.
        assert_eq!(zone.moment(day("2026-03-08T12:00:00Z"), 150), at("2026-03-08T09:30:00Z"));
        // 1:30 AM happens twice on 1 November: the first one.
        assert_eq!(zone.moment(day("2026-11-01T12:00:00Z"), 90), at("2026-11-01T07:30:00Z"));
    }

    #[test]
    fn reads_the_system_s_zone_file_when_there_is_one() {
        // Not every build machine has tzdata; where it does, Denver reads.
        if Path::new("/usr/share/zoneinfo/America/Denver").is_file()
            && std::env::var_os("TZDIR").is_none()
        {
            let zone = Zone::load(DEFAULT).unwrap();
            assert_eq!(zone.short_name(), "MT");
        }
    }
}
