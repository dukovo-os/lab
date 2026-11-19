// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The health check of a real machine after each fresh boot.
//!
//! A machine with a bad setup fails every test it is given: a root too
//! small for the bundles tests install, a clock years off, a console that
//! shows nothing. Each of those would cost a test's whole time and be
//! filed as a failure of Dukovo's. So once per fresh boot, after the root
//! has grown and the clock is set, the runner asks the machine one cheap
//! question over SSH ([`COMMAND`]) and judges the answer here. A machine
//! that fails is a lab error for that test, and one that fails
//! [`FAILURES_TO_BENCH`] fresh boots in a row is benched for the rest of
//! the run (see run.rs).
//!
//! The console is not checked: the runner waits for the ready line on the
//! console before it asks anything, so a boot that reaches the check has
//! shown its console lines already.

/// The command that prints the root disk's free space and the machine's
/// clock in seconds since the epoch (`clock=<seconds>`). It needs no root.
/// Dukovo's `/` is a small tmpfs; the root disk's filesystem is mounted at
/// /system/state, where bundles are installed, so that is the one asked
/// about, in POSIX's one-line form (`-P`).
pub const COMMAND: &str = "sh -c 'df -Pk /system/state; echo \"clock=$(date +%s)\"'";

/// The least free space a root may have, in KiB: 1 GiB, which the largest
/// bundles a test installs fit in.
pub const MIN_FREE_KIB: u64 = 1024 * 1024;

/// The most the machine's clock may differ from the lab PC's, in seconds.
pub const CLOCK_TOLERANCE: u64 = 5;

/// How many fresh boots in a row may fail before the machine is benched.
pub const FAILURES_TO_BENCH: u32 = 2;

/// What the command printed, each part none when it is not in the text.
#[derive(Debug, PartialEq, Eq)]
pub struct Reading {
    pub free_kib: Option<u64>,
    pub clock: Option<u64>,
}

/// One check that failed: what was found, and what a person can do.
#[derive(Debug, PartialEq, Eq)]
pub struct Fault {
    pub found: String,
    pub advice: String,
}

/// How the checks came out: what each found, for the test's log, and the
/// ones that failed.
#[derive(Debug, PartialEq, Eq)]
pub struct Judgement {
    pub summary: String,
    pub faults: Vec<Fault>,
}

/// Reads [`COMMAND`]'s output: the line after df's header gives the
/// filesystem, blocks, used and available, and a long device name may sit
/// on a line of its own, so the columns are counted across the lines.
pub fn read(text: &str) -> Reading {
    let clock = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("clock="))
        .and_then(|seconds| seconds.trim().parse().ok());
    let columns: Vec<&str> = text
        .lines()
        .skip(1)
        .take_while(|line| !line.trim_start().starts_with("clock="))
        .flat_map(str::split_whitespace)
        .collect();
    let free_kib = columns.get(3).and_then(|available| available.parse().ok());
    Reading { free_kib, clock }
}

/// Holds `reading` to the checks, `lab_now` being the lab PC's clock in
/// seconds since the epoch. The root's space is held to [`MIN_FREE_KIB`]
/// only when `root_grows`: the lab image's own root is small by design, so
/// a machine whose root is not grown would fail every time. A part the
/// command did not print is not judged, and the summary says so.
pub fn judge(reading: &Reading, lab_now: u64, root_grows: bool) -> Judgement {
    let mut found = Vec::new();
    let mut faults = Vec::new();
    match (reading.free_kib, root_grows) {
        (_, false) => found.push("root space not checked (grow_root is off)".to_string()),
        (None, true) => found.push("root space not read".to_string()),
        (Some(free), true) => {
            let mib = free / 1024;
            found.push(format!("root {mib} MiB free"));
            if free < MIN_FREE_KIB {
                faults.push(Fault {
                    found: format!(
                        "root has {mib} MiB free, under the {} MiB needed",
                        MIN_FREE_KIB / 1024
                    ),
                    advice: "the root should have grown to the end of the disk: check that \
                             `storage grow` works on this machine and that its disk is the size \
                             the machine file says"
                        .into(),
                });
            }
        }
    }
    match reading.clock {
        None => found.push("clock not read".to_string()),
        Some(clock) => {
            let off = clock.abs_diff(lab_now);
            found.push(format!("clock {off} s off the lab PC's"));
            if off > CLOCK_TOLERANCE {
                faults.push(Fault {
                    found: format!(
                        "clock {off} s off the lab PC's, over the {CLOCK_TOLERANCE} s allowed"
                    ),
                    advice: "`timedatectl set-time` did not take, or the lab PC's own clock is \
                             wrong: check the lab PC's with `timedatectl`, and set the machine's \
                             by hand"
                        .into(),
                });
            }
        }
    }
    Judgement { summary: found.join("; "), faults }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DF: &str = "Filesystem     1024-blocks   Used Available Capacity Mounted on\n\
                      sda3               7861228 239048   7622180       4% /system/state\n";

    #[test]
    fn the_free_space_and_the_clock_are_read_from_the_output() {
        let text = format!("{DF}clock=1794065412\n");
        assert_eq!(read(&text), Reading { free_kib: Some(7_622_180), clock: Some(1_794_065_412) });
    }

    #[test]
    fn a_device_name_wrapped_onto_its_own_line_is_read() {
        let text = "Filesystem 1K-blocks Used Available Use% Mounted on\n\
                    /dev/disk/by-id/nvme-a-very-long-name\n\
                    7000000 1000000 6000000 15% /\nclock=5\n";
        assert_eq!(read(text), Reading { free_kib: Some(6_000_000), clock: Some(5) });
    }

    #[test]
    fn output_that_is_not_there_is_not_read() {
        assert_eq!(read("df: not found\nclock=\n"), Reading { free_kib: None, clock: None });
    }

    #[test]
    fn a_healthy_machine_has_no_faults() {
        let reading = Reading { free_kib: Some(MIN_FREE_KIB), clock: Some(1_000_000 + 5) };
        let judged = judge(&reading, 1_000_000, true);
        assert_eq!(judged.faults, []);
        assert_eq!(judged.summary, "root 1024 MiB free; clock 5 s off the lab PC's");
    }

    #[test]
    fn a_clock_off_either_way_is_a_fault_that_says_how_far() {
        for clock in [1_000_000 + 41, 1_000_000 - 41] {
            let judged = judge(&Reading { free_kib: None, clock: Some(clock) }, 1_000_000, true);
            assert_eq!(judged.faults.len(), 1);
            assert_eq!(judged.faults[0].found, "clock 41 s off the lab PC's, over the 5 s allowed");
            assert!(judged.faults[0].advice.contains("timedatectl"));
        }
    }

    #[test]
    fn a_small_root_is_a_fault_only_when_the_root_is_grown() {
        let reading = Reading { free_kib: Some(300 * 1024), clock: Some(7) };
        let grown = judge(&reading, 7, true);
        assert_eq!(grown.faults.len(), 1);
        assert_eq!(grown.faults[0].found, "root has 300 MiB free, under the 1024 MiB needed");
        let small = judge(&reading, 7, false);
        assert_eq!(small.faults, []);
        assert!(small.summary.contains("root space not checked"), "{}", small.summary);
    }

    #[test]
    fn a_part_that_was_not_read_is_not_judged() {
        let judged = judge(&Reading { free_kib: None, clock: None }, 7, true);
        assert_eq!(judged.faults, []);
        assert_eq!(judged.summary, "root space not read; clock not read");
    }
}
