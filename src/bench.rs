// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Performance tests: Dukovo's numbers held to Linux's.
//!
//! A `bench` step runs a benchmark program on the machine under test and
//! reads the lines it prints in the format of Dukovo's benchmark programs
//! and `ob bench`:
//!
//! ```text
//! BENCH name=<row> ns=<nanoseconds per operation> iterations=<count>
//! ```
//!
//! Linux's numbers come from the machine's Linux twin: Linux booted from a
//! kit, on a machine set up the same way, whose first program runs the same
//! benchmarks built for Linux and prints the same lines on the serial
//! console, then a line that says it is done. `ob lab` builds the kit from
//! the Linux kernel `ob bench` holds Dukovo to (Dukovo's ADR 0290), so the
//! lab and CI measure against the same Linux. The twin boots once per run
//! on each machine; its numbers serve every bench step after.
//!
//! Each row the step names passes when Dukovo takes at most `limit` times
//! Linux's time per operation: 1.00 by default, parity, the bar every row
//! of `ob bench` is held to.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The file in a kit's folder that describes it.
pub const KIT_FILE: &str = "kit.toml";

/// Linux to measure against: a kernel and an initramfs whose first
/// program runs the benchmarks and prints their lines.
#[derive(Clone, Debug, PartialEq)]
pub struct Kit {
    /// The kernel, booted directly by the driver.
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    /// The kernel's command line, which must send its console to the
    /// first serial port.
    pub cmdline: String,
    /// What the serial console says once every benchmark has run.
    pub done: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KitFile {
    kernel: String,
    initrd: String,
    cmdline: String,
    done: String,
}

impl Kit {
    /// Reads `dir/kit.toml`. Its `kernel` and `initrd` are files in `dir`.
    pub fn read(dir: &Path) -> Result<Kit, String> {
        let path = dir.join(KIT_FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("reading the Linux kit {}: {err}", path.display()))?;
        let file: KitFile =
            toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        let in_dir = |name: &str, what: &str| -> Result<PathBuf, String> {
            let file = dir.join(name);
            if name.is_empty() || name.contains('/') || !file.is_file() {
                return Err(format!(
                    "{}: its {what} is a file beside it, and {name:?} is not",
                    path.display()
                ));
            }
            Ok(file)
        };
        let kit = Kit {
            kernel: in_dir(&file.kernel, "kernel")?,
            initrd: in_dir(&file.initrd, "initrd")?,
            cmdline: file.cmdline,
            done: file.done,
        };
        if !kit.cmdline.contains("console=ttyS0") {
            return Err(format!(
                "{}: the kernel's command line needs console=ttyS0, for its numbers",
                path.display()
            ));
        }
        if kit.done.trim().is_empty() {
            return Err(format!("{}: done needs the text Linux prints at the end", path.display()));
        }
        Ok(kit)
    }
}

/// The rows in a benchmark program's output, by name: nanoseconds per
/// operation. A row printed twice keeps its fastest, as `ob bench` does
/// across runs. Lines of anything else are left alone.
pub fn parse(text: &str) -> BTreeMap<String, f64> {
    let mut rows = BTreeMap::new();
    for line in text.lines() {
        let Some(fields) = line.split_once("BENCH ").map(|(_, fields)| fields) else { continue };
        let (mut name, mut ns) = (None, None);
        for field in fields.split_whitespace() {
            match field.split_once('=') {
                Some(("name", value)) if !value.is_empty() => name = Some(value),
                // A time is a finite number above 0: a ratio needs both.
                Some(("ns", value)) => {
                    ns = value.parse::<f64>().ok().filter(|ns| ns.is_finite() && *ns > 0.0);
                }
                _ => {}
            }
        }
        if let (Some(name), Some(ns)) = (name, ns) {
            let best = rows.entry(name.to_string()).or_insert(ns);
            *best = best.min(ns);
        }
    }
    rows
}

/// One row held to Linux's.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub name: String,
    pub dukovo_ns: f64,
    pub linux_ns: f64,
    pub limit: f64,
}

impl Row {
    /// Dukovo's time over Linux's: under 1 is faster.
    pub fn ratio(&self) -> f64 {
        self.dukovo_ns / self.linux_ns
    }

    pub fn passes(&self) -> bool {
        self.ratio() <= self.limit
    }
}

/// The rows `names` from both systems' numbers, or what is missing.
pub fn compare(
    names: &[String],
    limit: f64,
    dukovo: &BTreeMap<String, f64>,
    linux: &BTreeMap<String, f64>,
) -> Result<Vec<Row>, String> {
    let missing = |numbers: &BTreeMap<String, f64>| -> Vec<&str> {
        names.iter().filter(|name| !numbers.contains_key(*name)).map(String::as_str).collect()
    };
    let (on_dukovo, on_linux) = (missing(dukovo), missing(linux));
    if !on_dukovo.is_empty() {
        return Err(format!("Dukovo's output has no row {}", on_dukovo.join(", ")));
    }
    if !on_linux.is_empty() {
        return Err(format!("Linux's output has no row {}", on_linux.join(", ")));
    }
    Ok(names
        .iter()
        .map(|name| Row {
            name: name.clone(),
            dukovo_ns: dukovo[name],
            linux_ns: linux[name],
            limit,
        })
        .collect())
}

/// The rows as a table for the log, a finding and the runner page.
pub fn table(rows: &[Row]) -> String {
    let width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0).max(3);
    let mut out = format!(
        "{:<width$}  {:>12}  {:>12}  {:>6}  {:>5}\n",
        "row", "Dukovo", "Linux", "ratio", "limit"
    );
    for row in rows {
        let mark = if row.passes() { "" } else { "  over" };
        let _ = writeln!(
            out,
            "{:<width$}  {:>12}  {:>12}  {:>6.2}  {:>5.2}{mark}",
            row.name,
            show(row.dukovo_ns),
            show(row.linux_ns),
            row.ratio(),
            row.limit
        );
    }
    out
}

/// A time per operation as a person reads it.
pub fn show(ns: f64) -> String {
    if ns >= 1_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else if ns >= 10_000.0 {
        format!("{:.1} µs", ns / 1000.0)
    } else {
        format!("{ns:.0} ns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bench_lines_and_keeps_the_fastest() {
        let text = "boot noise\n\
                    BENCH name=posix_fork_wait ns=153856.4 iterations=2000\n\
                    BENCHCOUNT group=posix_fork_wait rounds=2000 switches=8\n\
                    [  3.1] BENCH name=posix_poll ns=1092 iterations=100000\n\
                    BENCH name=posix_fork_wait ns=150000 iterations=2000\n\
                    BENCH name=broken ns=nan iterations=1\n\
                    BENCH name=zero ns=0 iterations=1\n";
        let rows = parse(text);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows["posix_fork_wait"], 150000.0);
        assert_eq!(rows["posix_poll"], 1092.0);
    }

    #[test]
    fn holds_each_row_to_linux_and_names_what_is_missing() {
        let dukovo = parse("BENCH name=a ns=90 iterations=1\nBENCH name=b ns=120 iterations=1\n");
        let linux = parse("BENCH name=a ns=100 iterations=1\nBENCH name=b ns=100 iterations=1\n");
        let names = vec!["a".to_string(), "b".to_string()];
        let rows = compare(&names, 1.0, &dukovo, &linux).unwrap();
        assert!(rows[0].passes());
        assert!(!rows[1].passes());
        assert!((rows[1].ratio() - 1.2).abs() < 1e-9);
        let shown = table(&rows);
        assert!(shown.contains("b          120 ns        100 ns    1.20   1.00  over"), "{shown}");
        assert!(compare(&names, 1.25, &dukovo, &linux).unwrap().iter().all(Row::passes));

        let names = vec!["a".to_string(), "c".to_string()];
        assert_eq!(
            compare(&names, 1.0, &dukovo, &linux),
            Err("Dukovo's output has no row c".into())
        );
        let more = parse("BENCH name=c ns=1 iterations=1\n");
        assert_eq!(compare(&names, 1.0, &more, &linux), Err("Dukovo's output has no row a".into()));
    }

    #[test]
    fn a_kit_is_its_file_and_the_two_files_beside_it() {
        let dir = crate::testing::TempDir::new("kit");
        dir.write("vmlinuz", "kernel");
        dir.write("initramfs.img", "initrd");
        let good = "kernel = \"vmlinuz\"\ninitrd = \"initramfs.img\"\n\
                    cmdline = \"console=ttyS0 panic=-1 quiet\"\ndone = \"linux-bench: done\"\n";
        dir.write(KIT_FILE, good);
        let kit = Kit::read(dir.path()).unwrap();
        assert_eq!(kit.kernel, dir.path().join("vmlinuz"));
        assert_eq!(kit.done, "linux-bench: done");

        for bad in [
            good.replace("vmlinuz", "../vmlinuz"),
            good.replace("vmlinuz", "missing"),
            good.replace("console=ttyS0 ", ""),
            good.replace("linux-bench: done", " "),
            format!("{good}extra = 1\n"),
        ] {
            dir.write(KIT_FILE, &bad);
            assert!(Kit::read(dir.path()).is_err(), "{bad}");
        }
        assert!(Kit::read(&dir.path().join("nowhere")).is_err());
    }

    #[test]
    fn shows_times_as_a_person_reads_them() {
        assert_eq!(show(1092.4), "1092 ns");
        assert_eq!(show(153_856.0), "153.9 µs");
        assert_eq!(show(2_188_725.0), "2.19 ms");
    }
}
