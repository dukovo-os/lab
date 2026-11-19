// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! How busy the lab PC is, recorded beside each step.
//!
//! Browser and performance steps run their side of the test on the lab PC,
//! so a busy lab PC makes their times slow whatever Dukovo does. Each
//! step's record in results.json holds the lab PC's load (`lab_pc_load`),
//! and a browser or bench step that ends while the load is over [`BUSY`]
//! says so in its note and in the test's log, so a slow result can be told
//! from a slow Dukovo. The load never changes a result. Only Linux has
//! `/proc/loadavg`; elsewhere no load is recorded.

use std::fs;

/// The load per CPU above which the lab PC counts as busy: three quarters
/// of its CPUs' time taken over the last minute.
pub const BUSY: f64 = 0.75;

/// The lab PC's one-minute load average per CPU now, to two decimals, or
/// none where `/proc/loadavg` is not there.
pub fn now() -> Option<f64> {
    let text = fs::read_to_string("/proc/loadavg").ok()?;
    let cpus = std::thread::available_parallelism().ok()?;
    per_cpu(&text, cpus.get())
}

/// The one-minute load average in `loadavg`, the text of `/proc/loadavg`,
/// divided by `cpus` and rounded to two decimals. None when the text does
/// not begin with a number.
fn per_cpu(loadavg: &str, cpus: usize) -> Option<f64> {
    let one_minute: f64 = loadavg.split_whitespace().next()?.parse().ok()?;
    if !one_minute.is_finite() || one_minute < 0.0 || cpus == 0 {
        return None;
    }
    // CPU counts are small, so the conversion is exact.
    Some((one_minute / cpus as f64 * 100.0).round() / 100.0)
}

/// The line a step's note and the test's log hold when `load` is over
/// [`BUSY`], and nothing otherwise.
pub fn busy_line(load: Option<f64>) -> Option<String> {
    let load = load.filter(|&load| load > BUSY)?;
    Some(format!("the lab PC was busy (load {load:.2} per CPU), so this step's times may be slow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_load_is_the_first_number_per_cpu() {
        let text = "1.24 0.90 0.80 2/512 12345\n";
        assert_eq!(per_cpu(text, 4), Some(0.31));
        assert_eq!(per_cpu(text, 1), Some(1.24));
        assert_eq!(per_cpu("0.00 0.00 0.00 1/100 9", 8), Some(0.0));
    }

    #[test]
    fn text_that_is_not_a_load_gives_none() {
        assert_eq!(per_cpu("", 4), None);
        assert_eq!(per_cpu("busy 1 2", 4), None);
        assert_eq!(per_cpu("-1.00 0 0", 4), None);
        assert_eq!(per_cpu("1.00 0 0", 0), None);
    }

    #[test]
    fn only_a_load_over_the_threshold_is_busy() {
        assert_eq!(busy_line(None), None);
        assert_eq!(busy_line(Some(0.31)), None);
        assert_eq!(busy_line(Some(BUSY)), None);
        assert_eq!(
            busy_line(Some(0.92)).as_deref(),
            Some("the lab PC was busy (load 0.92 per CPU), so this step's times may be slow")
        );
        let load = per_cpu("3.70 1.00 1.00 1/100 9", 4);
        assert!(busy_line(load).is_some_and(|line| line.contains("load 0.93 per CPU")));
    }
}
