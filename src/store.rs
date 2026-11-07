// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The store: where runs leave their bundles, and how runs that are going
//! say so to the commands and the runner page that follow them.
//!
//! ```text
//! runs/<date>/<run id>/     each run's results bundle (see bundle.rs)
//! running/<run id>/         while a run is going:
//!     lock                    held by the run's process; holds its pid
//!     stop                    `dukovo-lab stop` asks the run to stop
//!     done-<machine>          `dukovo-lab done` says a person did a step
//!     output.log              a detached run's own messages
//! machines/<name>.lock      held by the run using the machine
//! work/<run id>/            disks and other large files, while it runs
//! ```
//!
//! "Held" means an exclusive lock on the open file, which the system
//! drops when the process ends however it ends. So a run that crashed or
//! was killed never looks busy: its lock is free, and `status` reports it
//! as having died. The files in `running/<run id>/` are removed when the
//! run ends; a run that died, or broke before its end, leaves its
//! `output.log` there, for `status` and `run --detach` to show.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Error;
use crate::bundle::RunRecord;

/// Whether `id` is a run id as [`crate::bundle::new_run_id`] makes them:
/// `20261107-153012-4f2a`. Ids from outside (the command line, the
/// runner page) are checked before they name any file.
pub fn valid_run_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 20
        && bytes.iter().enumerate().all(|(index, &byte)| match index {
            8 | 15 => byte == b'-',
            0..=7 | 9..=14 => byte.is_ascii_digit(),
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// The bundle folder of run `id`: its date folder comes from the id.
pub fn bundle_dir(store: &Path, id: &str) -> PathBuf {
    let date = format!("{}-{}-{}", &id[0..4], &id[4..6], &id[6..8]);
    store.join("runs").join(date).join(id)
}

/// The run's files while it is going.
fn running_dir(store: &Path, id: &str) -> PathBuf {
    store.join("running").join(id)
}

/// Where a detached run's own messages go; see [`Running`].
pub fn output_log(store: &Path, id: &str) -> PathBuf {
    running_dir(store, id).join("output.log")
}

/// Makes the folder for a detached run's own messages before the run
/// starts, and opens its `output.log`.
pub fn open_output_log(store: &Path, id: &str) -> Result<File, Error> {
    let dir = running_dir(store, id);
    fs::create_dir_all(&dir)
        .map_err(|err| Error::Harness(format!("making {}: {err}", dir.display())))?;
    File::create(output_log(store, id))
        .map_err(|err| Error::Harness(format!("making {}/output.log: {err}", dir.display())))
}

/// Whether some process holds the lock file at `path`.
pub fn held(path: &Path) -> bool {
    let Ok(file) = File::open(path) else { return false };
    matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
}

/// How many times [`take_lock`] tries a lock that is taken, and how long
/// it waits between tries: [`held`] takes a lock for a moment to look,
/// and that must not make a free machine look busy.
const LOCK_TRIES: u32 = 20;
const LOCK_PAUSE: Duration = Duration::from_millis(5);

/// Opens `path` and takes its lock; `Ok(None)` when another process has it.
pub fn take_lock(path: &Path) -> Result<Option<File>, Error> {
    if let Some(folder) = path.parent() {
        fs::create_dir_all(folder)
            .map_err(|err| Error::Harness(format!("making {}: {err}", folder.display())))?;
    }
    // Not truncated on open: the holder's name inside stays readable to
    // others until the lock is ours.
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|err| Error::Harness(format!("opening {}: {err}", path.display())))?;
    // A run holds its locks for as long as it goes; someone looking (a
    // `status`, the runner page) holds one for microseconds. So a lock
    // still taken after a tenth of a second is a run's.
    let mut tries = 0;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if tries + 1 < LOCK_TRIES => {
                tries += 1;
                std::thread::sleep(LOCK_PAUSE);
            }
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(err)) => {
                return Err(Error::Harness(format!("locking {}: {err}", path.display())));
            }
        }
    }
    // Ours now: clear the last holder's name, if any, for ours.
    file.set_len(0).map_err(|err| Error::Harness(err.to_string()))?;
    Ok(Some(file))
}

/// Whether run `id` is going: its process holds its lock.
pub fn is_running(store: &Path, id: &str) -> bool {
    held(&running_dir(store, id).join("lock"))
}

/// The runs going now, newest first.
pub fn running_ids(store: &Path) -> Vec<String> {
    let mut ids: Vec<String> = fs::read_dir(store.join("running"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|id| valid_run_id(id) && is_running(store, id))
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// The run using machine `name` now, if one is.
pub fn machine_user(store: &Path, name: &str) -> Option<String> {
    let path = store.join("machines").join(format!("{name}.lock"));
    if !held(&path) {
        return None;
    }
    let id = fs::read_to_string(&path).unwrap_or_default().trim().to_string();
    Some(if id.is_empty() { "another run".into() } else { id })
}

/// A run that is going, as its own process sees it: it holds the run's
/// lock and its machines' locks until it is dropped.
pub struct Running {
    store: PathBuf,
    id: String,
    /// Whether the run reached its end; see [`Running::end`].
    ended: bool,
    _lock: File,
    _machines: Vec<File>,
}

impl Running {
    /// Takes run `id`'s lock and the locks of the machines it runs on.
    /// Fails with [`Error::Busy`] when another run uses one of them.
    pub fn start(store: &Path, id: &str, machines: &[&str]) -> Result<Running, Error> {
        let dir = running_dir(store, id);
        let mut lock = take_lock(&dir.join("lock"))?
            .ok_or_else(|| Error::Harness(format!("run {id} is already going")))?;
        let mut held_machines = Vec::new();
        for &name in machines {
            let path = store.join("machines").join(format!("{name}.lock"));
            let Some(mut file) = take_lock(&path)? else {
                let user = machine_user(store, name).unwrap_or_else(|| "another run".into());
                // Nothing ran, so nothing is left behind but what a
                // detached run says, which `run --detach` shows.
                clear(&dir);
                drop(lock);
                return Err(Error::Busy(format!(
                    "the {name} machine is busy with run {user}; wait for it (`dukovo-lab status --wait {user}`) or stop it (`dukovo-lab stop {user}`)"
                )));
            };
            let _ = write!(file, "{id}");
            held_machines.push(file);
        }
        let _ = write!(lock, "{}", std::process::id());
        Ok(Running {
            store: store.to_path_buf(),
            id: id.to_string(),
            ended: false,
            _lock: lock,
            _machines: held_machines,
        })
    }

    /// Whether `dukovo-lab stop` has asked the run to stop.
    pub fn stop_requested(&self) -> bool {
        running_dir(&self.store, &self.id).join("stop").exists()
    }

    /// Forgets an earlier `done` for `machine`, before a step asks a
    /// person to act, so only a `done` given after the ask counts.
    pub fn clear_done(&self, machine: &str) {
        let _ = fs::remove_file(done_file(&self.store, &self.id, machine));
    }

    /// Whether a person has said they did the step `machine` waits for;
    /// takes the answer, so it counts once.
    pub fn take_done(&self, machine: &str) -> bool {
        fs::remove_file(done_file(&self.store, &self.id, machine)).is_ok()
    }
}

impl Running {
    /// The run reached its end and run.json says so: its folder goes
    /// whole, and its locks with it.
    pub fn end(mut self) {
        self.ended = true;
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let dir = running_dir(&self.store, &self.id);
        if self.ended {
            let _ = fs::remove_dir_all(&dir);
        } else {
            // It broke on the way: keep what it said.
            clear(&dir);
        }
        // The locks go as the files close, after this.
    }
}

/// Removes a run's folder in `running/` but for its `output.log`, which
/// says why a detached run ended where it did; the folder goes too when
/// there is none.
fn clear(dir: &Path) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        if entry.file_name() != "output.log" {
            let _ = fs::remove_file(entry.path());
        }
    }
    let _ = fs::remove_dir(dir);
}

fn done_file(store: &Path, id: &str, machine: &str) -> PathBuf {
    running_dir(store, id).join(format!("done-{machine}"))
}

/// Asks run `id` to stop; it stops within a second or so, at the next
/// place a step can be cut short.
pub fn request_stop(store: &Path, id: &str) -> Result<(), Error> {
    let path = running_dir(store, id).join("stop");
    fs::write(&path, "").map_err(|err| Error::Harness(format!("writing {}: {err}", path.display())))
}

/// Tells run `id` that a person did the step `machine` waits for.
pub fn say_done(store: &Path, id: &str, machine: &str) -> Result<(), Error> {
    let path = done_file(store, id, machine);
    fs::write(&path, "").map_err(|err| Error::Harness(format!("writing {}: {err}", path.display())))
}

/// The newest runs in the store, newest first, at most `limit`, with
/// their bundle folders. Folders without a readable run.json are left out.
pub fn recent_runs(store: &Path, limit: usize) -> Vec<(PathBuf, RunRecord)> {
    let mut dates: Vec<PathBuf> = fs::read_dir(store.join("runs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dates.sort_unstable_by(|a, b| b.cmp(a));
    let mut runs = Vec::new();
    for date in dates {
        let mut bundles: Vec<PathBuf> =
            fs::read_dir(&date).into_iter().flatten().flatten().map(|entry| entry.path()).collect();
        bundles.sort_unstable_by(|a, b| b.cmp(a));
        for bundle in bundles {
            if runs.len() == limit {
                return runs;
            }
            if let Some(record) = read_run(&bundle) {
                runs.push((bundle, record));
            }
        }
    }
    runs
}

/// A bundle's run.json, if it reads.
pub fn read_run(bundle: &Path) -> Option<RunRecord> {
    serde_json::from_str(&fs::read_to_string(bundle.join("run.json")).ok()?).ok()
}

/// The run a command means: `given` when there is one (checked), else
/// the newest run going, else the newest run there is.
pub fn pick_run(store: &Path, given: Option<&str>) -> Result<String, Error> {
    if let Some(id) = given {
        if !valid_run_id(id) {
            return Err(Error::Usage(format!(
                "\"{id}\" is not a run id; run ids look like 20261107-153012-4f2a (`dukovo-lab runs` lists them)"
            )));
        }
        if !bundle_dir(store, id).join("run.json").is_file() {
            return Err(Error::Usage(format!(
                "there is no run {id} in {}; `dukovo-lab runs` lists the runs",
                store.display()
            )));
        }
        return Ok(id.to_string());
    }
    if let Some(id) = running_ids(store).into_iter().next() {
        return Ok(id);
    }
    recent_runs(store, 1)
        .into_iter()
        .next()
        .map(|(_, record)| record.id)
        .ok_or_else(|| Error::Usage("there are no runs yet; `dukovo-lab run` starts one".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn run_ids_are_checked_before_they_name_files() {
        assert!(valid_run_id("20261107-153012-4f2a"));
        for bad in ["20261107-153012-4F2A", "../../etc/passwd", "20261107-153012-4f2", ""] {
            assert!(!valid_run_id(bad), "{bad}");
        }
        let store = Path::new("/s");
        assert_eq!(
            bundle_dir(store, "20261107-153012-4f2a"),
            Path::new("/s/runs/2026-11-07/20261107-153012-4f2a")
        );
    }

    #[test]
    fn a_run_holds_its_machines_until_it_ends() {
        let store = TempDir::new("store-locks");
        let id = "20261107-153012-4f2a";
        let running = Running::start(store.path(), id, &["qemu"]).unwrap();
        assert!(is_running(store.path(), id));
        assert_eq!(running_ids(store.path()), [id]);
        assert_eq!(machine_user(store.path(), "qemu").as_deref(), Some(id));
        let other = Running::start(store.path(), "20261107-153013-0000", &["qemu"]);
        assert!(matches!(other, Err(Error::Busy(message)) if message.contains(id)));
        assert!(!is_running(store.path(), "20261107-153013-0000"), "the busy run left nothing");

        assert!(!running.stop_requested());
        request_stop(store.path(), id).unwrap();
        assert!(running.stop_requested());
        say_done(store.path(), id, "qemu").unwrap();
        assert!(running.take_done("qemu"));
        assert!(!running.take_done("qemu"), "a done counts once");

        running.end();
        assert!(!is_running(store.path(), id));
        assert_eq!(machine_user(store.path(), "qemu"), None);
        assert!(!store.path().join("running").join(id).exists());

        // A detached run that broke before its end keeps what it said.
        let id = "20261107-153014-0000";
        fs::create_dir_all(store.path().join("running").join(id)).unwrap();
        fs::write(output_log(store.path(), id), "dukovo-lab: it broke\n").unwrap();
        drop(Running::start(store.path(), id, &["qemu"]).unwrap());
        assert!(!is_running(store.path(), id));
        assert_eq!(
            fs::read_to_string(output_log(store.path(), id)).unwrap(),
            "dukovo-lab: it broke\n"
        );
    }

    #[test]
    fn someone_looking_does_not_make_a_machine_busy() {
        let store = TempDir::new("store-look");
        let path = store.path().join("machines/qemu.lock");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A `status` looking at the machine just as a run takes it.
        let looking = File::create(&path).unwrap();
        looking.lock_shared().unwrap();
        let looked = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            drop(looking);
        });
        let running = Running::start(store.path(), "20261107-153012-4f2a", &["qemu"]);
        looked.join().unwrap();
        assert!(running.is_ok(), "the run waited out the look");
    }
}
