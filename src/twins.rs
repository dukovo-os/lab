// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Each machine's Linux twin, boot after boot: the rows its latest boots
//! measured, kept from run to run.
//!
//! Linux's time for a row moves from one boot to the next on the same
//! machine, by more than Dukovo's does: on an OptiPlex 7040 the event
//! queue's wait among idle pipes took Linux 624 to 664 ns over six boots
//! of one kit, where Dukovo's stayed within 2%. Held to one boot, a row as
//! fast as Linux's usual time fails whenever that boot ran fast. So each
//! twin boot's rows are kept in the store, `twins/<machine>.json`, and a
//! bench step holds Dukovo to the median of the machine's latest
//! [`BOOTS`] boots of the same kit at the same clock, the run's own among
//! them. The twin still boots once a run, so this costs no time.
//!
//! A kit is known by the SHA-256 of its kernel and initramfs: a new kit
//! is another Linux, whose boots start afresh. A boot at another clock
//! (Dukovo's ADR 0557) is left out, as its times say nothing about this
//! one's.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::bench::{self, Kit};
use crate::hash::{self, Hasher, Sha256};

/// How many of a machine's latest twin boots a row is held to the median
/// of: enough that one boot that ran fast or slow does not move it.
pub const BOOTS: usize = 5;

/// How many boots a machine's file keeps, of every kit: the latest.
const KEPT: usize = 20;

/// One boot of a machine's Linux twin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Boot {
    /// The run it booted in.
    pub run: String,
    /// The kit, by [`kit_id`].
    pub kit: String,
    /// Each row's time in nanoseconds, the fastest the boot printed.
    pub rows: BTreeMap<String, f64>,
}

/// The rows a bench step holds Dukovo to, and the runs whose boots they
/// are the median of, the latest last.
#[derive(Debug, PartialEq)]
pub struct Held {
    pub rows: BTreeMap<String, f64>,
    pub runs: Vec<String>,
}

/// The kit's identity: the SHA-256 of its kernel, then its initramfs, in
/// hexadecimal.
pub fn kit_id(kit: &Kit) -> Result<String, String> {
    let mut sha = Sha256::new();
    for path in [&kit.kernel, &kit.initrd] {
        let bytes = fs::read(path).map_err(|err| format!("reading {}: {err}", path.display()))?;
        sha.update(&bytes);
    }
    Ok(hash::hex(&sha.finish()))
}

/// Keeps `boot` as the latest of `machine`'s, and gives the rows to hold
/// Dukovo to. A file that cannot be written is said in the error, and the
/// boot's own rows are all a step then has.
pub fn record(store: &Path, machine: &str, boot: Boot) -> Result<Held, String> {
    let path = file(store, machine);
    // A file that is missing or not ours starts afresh.
    let mut boots: Vec<Boot> = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    boots.push(boot);
    if boots.len() > KEPT {
        boots.drain(..boots.len() - KEPT);
    }
    let held = median(&boots);
    let text = serde_json::to_string_pretty(&boots).expect("boots always serialize");
    let written = path
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| {
            let partial = path.with_extension("json.partial");
            fs::write(&partial, text + "\n").and_then(|()| fs::rename(&partial, &path))
        })
        .map_err(|err| format!("keeping the twin's boot in {}: {err}", path.display()));
    written.map(|()| held)
}

fn file(store: &Path, machine: &str) -> PathBuf {
    store.join("twins").join(format!("{machine}.json"))
}

/// The median of each row over the latest [`BOOTS`] of `boots` (oldest
/// first) that share the last one's kit and clock, the last among them.
/// A row a boot lacks is the median of the boots that have it.
fn median(boots: &[Boot]) -> Held {
    let Some(last) = boots.last() else {
        return Held { rows: BTreeMap::new(), runs: Vec::new() };
    };
    let same_clock = |boot: &Boot| match bench::clocks(&last.rows, &boot.rows) {
        Some(clocks) => clocks.agree(),
        // Without a clock row on both, there is nothing to tell them apart.
        None => true,
    };
    let mut picked: Vec<&Boot> = boots
        .iter()
        .rev()
        .filter(|boot| boot.kit == last.kit && same_clock(boot))
        .take(BOOTS)
        .collect();
    picked.reverse();
    let rows = last
        .rows
        .keys()
        .map(|row| {
            let mut times: Vec<f64> =
                picked.iter().filter_map(|boot| boot.rows.get(row).copied()).collect();
            times.sort_by(f64::total_cmp);
            let middle = times.len() / 2;
            let time = if times.len().is_multiple_of(2) {
                (times[middle - 1] + times[middle]) / 2.0
            } else {
                times[middle]
            };
            (row.clone(), time)
        })
        .collect();
    Held { rows, runs: picked.iter().map(|boot| boot.run.clone()).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn boot(run: &str, kit: &str, rows: &[(&str, f64)]) -> Boot {
        Boot {
            run: run.into(),
            kit: kit.into(),
            rows: rows.iter().map(|&(name, ns)| (name.to_string(), ns)).collect(),
        }
    }

    #[test]
    fn a_row_is_held_to_the_median_of_the_latest_boots() {
        let dir = TempDir::new("twins");
        let store = dir.path();
        // The first boot is all there is.
        let held = record(store, "optiplex-1", boot("r1", "k", &[("events", 628.0)])).unwrap();
        assert_eq!(held.rows["events"], 628.0);
        assert_eq!(held.runs, ["r1"]);
        // Two boots: the mean of the two.
        let held = record(store, "optiplex-1", boot("r2", "k", &[("events", 664.0)])).unwrap();
        assert_eq!(held.rows["events"], 646.0);
        for (run, ns) in [("r3", 651.0), ("r4", 624.0), ("r5", 648.0)] {
            record(store, "optiplex-1", boot(run, "k", &[("events", ns)])).unwrap();
        }
        // Six boots: the latest five, 664 651 624 648 and 630, give 648.
        let held = record(store, "optiplex-1", boot("r6", "k", &[("events", 630.0)])).unwrap();
        assert_eq!(held.rows["events"], 648.0);
        assert_eq!(held.runs, ["r2", "r3", "r4", "r5", "r6"]);
        // Another machine's boots are its own.
        let held = record(store, "optiplex-2", boot("r6", "k", &[("events", 700.0)])).unwrap();
        assert_eq!(held.rows["events"], 700.0);
    }

    #[test]
    fn another_kit_or_clock_starts_afresh() {
        let dir = TempDir::new("twins-kit");
        let store = dir.path();
        let at = |ghz: f64, ns: f64| [("cpu_cycle", 1.0 / ghz), ("events", ns)];
        record(store, "m", boot("r1", "old", &at(3.19, 600.0))).unwrap();
        record(store, "m", boot("r2", "new", &at(3.19, 650.0))).unwrap();
        record(store, "m", boot("r3", "new", &at(2.91, 700.0))).unwrap();
        let held = record(store, "m", boot("r4", "new", &at(3.19, 660.0))).unwrap();
        assert_eq!(
            held.runs,
            ["r2", "r4"],
            "the old kit's and the slow clock's boots are left out"
        );
        assert_eq!(held.rows["events"], 655.0);
        // A row one boot lacks is the median of those that have it.
        let rows = [("cpu_cycle", 1.0 / 3.19), ("events", 640.0), ("pipe", 9.0)];
        let held = record(store, "m", boot("r5", "new", &rows)).unwrap();
        assert_eq!(held.rows["pipe"], 9.0);
        assert_eq!(held.rows["events"], 650.0);
    }

    #[test]
    fn the_file_keeps_the_latest_boots_and_survives_a_bad_one() {
        let dir = TempDir::new("twins-kept");
        let store = dir.path();
        fs::create_dir_all(store.join("twins")).unwrap();
        fs::write(store.join("twins/m.json"), "not json").unwrap();
        for run in 0..KEPT + 3 {
            record(store, "m", boot(&format!("r{run}"), "k", &[("events", 1.0)])).unwrap();
        }
        let text = fs::read_to_string(store.join("twins/m.json")).unwrap();
        let boots: Vec<Boot> = serde_json::from_str(&text).unwrap();
        assert_eq!(boots.len(), KEPT);
        assert_eq!(boots[0].run, "r3");
        assert!(!store.join("twins/m.json.partial").exists());
    }

    #[test]
    fn a_kit_is_known_by_its_files() {
        let dir = TempDir::new("twins-id");
        let kit = |initrd: &[u8]| {
            fs::write(dir.path().join("vmlinuz"), b"kernel").unwrap();
            fs::write(dir.path().join("initrd"), initrd).unwrap();
            Kit {
                kernel: dir.path().join("vmlinuz"),
                initrd: dir.path().join("initrd"),
                cmdline: "console=ttyS0".into(),
                done: "done".into(),
                time: std::time::Duration::ZERO,
            }
        };
        let one = kit_id(&kit(b"a")).unwrap();
        assert_eq!(one.len(), 64);
        assert_eq!(kit_id(&kit(b"a")).unwrap(), one);
        assert_ne!(kit_id(&kit(b"b")).unwrap(), one);
    }
}
