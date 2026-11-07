// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Lengths of time as test files write them: `90s`, `3m`, `1h`.

use std::time::Duration;

/// Reads a whole number followed by `s`, `m` or `h`.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let bad = || {
        format!(
            "\"{text}\" is not a length of time: write a whole number and s, m or h, such as 90s or 3m"
        )
    };
    let unit_at = text.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (number, unit) = text.split_at(unit_at);
    let number: u64 = number.parse().map_err(|_| bad())?;
    let seconds = match unit {
        "s" => number,
        "m" => number * 60,
        "h" => number * 3600,
        _ => return Err(bad()),
    };
    Ok(Duration::from_secs(seconds))
}

/// Writes a length of time the way test files do, in its largest whole unit.
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 && seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds >= 60 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_writes_lengths_of_time() {
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("3m"), Ok(Duration::from_secs(180)));
        assert_eq!(parse_duration("2h"), Ok(Duration::from_secs(7200)));
        for bad in ["", "3", "m", "3 m", "1.5m", "3d", "-3m"] {
            assert!(parse_duration(bad).is_err(), "{bad:?} should not parse");
        }
        assert_eq!(format_duration(Duration::from_secs(90)), "90s");
        assert_eq!(format_duration(Duration::from_secs(180)), "3m");
        assert_eq!(format_duration(Duration::from_secs(7200)), "2h");
    }
}
