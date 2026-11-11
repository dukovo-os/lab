// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Net boot: how a real machine starts each test from a fresh copy of
//! the image under test, and boots its Linux twin.
//!
//! The machine boots from the lab network once (AMT asks its firmware
//! to): dnsmasq on the lab PC's lab card hands it an address and the
//! network GRUB, and GRUB reads a short script the driver wrote for this
//! one boot, `<tftp>/dukovo-lab/boot/<mac>.cfg`. The script loads a
//! kernel and initrd from this module's HTTP server: the writer's, or the
//! Linux twin's. Every path on the server starts with a token new to the
//! boot, so a machine still running a boot before can never act on this
//! one's.
//!
//! The writer asks for its plan (`plan`: which disk, by serial number,
//! and where the image has data), streams the data (`data`), writes the
//! image to the disk and zeroes the rest of it, reads it all back, and
//! says what it read (`done`, with the SHA-256 of the image's bytes on
//! the disk) or why it could not (`failed`). Only a `done` whose hash is
//! the image's own is answered with 200, the writer's word to restart
//! into the disk. docs/net-boot.md is the contract `ob lab`'s writer and
//! GRUB keep to.

use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::bench::Kit;
use crate::hash::{Hasher as _, Sha256, hex};
use crate::http::{self, Response};

/// The plan's format, which the writer checks it knows.
pub const PLAN_VERSION: u32 = 1;

/// The network GRUB in the TFTP folder, which dnsmasq hands UEFI
/// machines as their boot file. `ob lab` builds and installs it.
pub const GRUB_FILE: &str = "dukovo-lab/grubx64.efi";

/// How finely the image is split into data and zeros: runs of this many
/// zero bytes are left out of the data and zeroed by the writer.
const GRAIN: usize = 64 * 1024;

/// The image under test as the writer sees it: its size, its hash, and
/// where it holds anything but zeros.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub path: PathBuf,
    pub size: u64,
    /// SHA-256 of all of it, zeros too, in hexadecimal.
    pub sha256: String,
    /// (offset, length) of each run of data, in order, never touching.
    pub extents: Vec<(u64, u64)>,
}

impl Image {
    /// Reads the image once, hashing it and finding its data. A run of
    /// zeros shorter than [`GRAIN`], or not aligned to it, stays in the
    /// data.
    pub fn scan(path: &Path) -> Result<Image, String> {
        let mut file = File::open(path).map_err(|err| format!("{}: {err}", path.display()))?;
        let mut sha = Sha256::new();
        let mut extents: Vec<(u64, u64)> = Vec::new();
        let mut buffer = vec![0u8; GRAIN];
        let mut offset = 0u64;
        loop {
            let read = read_full(&mut file, &mut buffer)
                .map_err(|err| format!("reading {}: {err}", path.display()))?;
            if read == 0 {
                break;
            }
            let chunk = &buffer[..read];
            sha.update(chunk);
            if chunk.iter().any(|&byte| byte != 0) {
                match extents.last_mut() {
                    Some((start, length)) if *start + *length == offset => *length += read as u64,
                    _ => extents.push((offset, read as u64)),
                }
            }
            offset += read as u64;
        }
        Ok(Image { path: path.to_path_buf(), size: offset, sha256: hex(&sha.finish()), extents })
    }

    /// How many bytes the data holds.
    pub fn data_bytes(&self) -> u64 {
        self.extents.iter().map(|(_, length)| length).sum()
    }
}

/// Reads until `buffer` is full or the file ends.
fn read_full(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

/// What the server hands out for the boot going now.
pub enum Offer {
    /// Nothing: every path is 404.
    Nothing,
    /// The writer, its plan and the image's data.
    Writer {
        kit: Kit,
        image: Arc<Image>,
        /// The serial number of the only disk the writer may write.
        disk: String,
        /// Called once the writer said it wrote the image and the hash is
        /// right, before it is told to restart: the driver moves the
        /// serial log to the next boot's file here.
        written: Box<dyn Fn() + Send>,
    },
    /// The Linux twin's kernel and initrd.
    Twin { kit: Kit },
}

/// How the writer's boot ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The disk holds the image, checked.
    Written,
    /// The writer could not write it, or what it read back was not the
    /// image: why.
    Failed(String),
}

/// What the server shares with its connections.
struct State {
    token: String,
    offer: Offer,
    outcome: Option<Outcome>,
    /// What the server did, for the driver's log.
    events: Vec<String>,
}

/// The net-boot server: one per real machine, for the whole run.
pub struct Server {
    address: SocketAddr,
    state: Arc<Mutex<State>>,
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Listens on `address`, port `port` (0 for any free one).
    pub fn start(address: IpAddr, port: u16) -> Result<Server, String> {
        let listener = TcpListener::bind((address, port)).map_err(|err| {
            format!(
                "the net-boot server cannot listen on {address} port {port}: {err} (is the lab \
                 card up with that address, and is no other run using the port?)"
            )
        })?;
        let address = listener.local_addr().map_err(|err| err.to_string())?;
        listener.set_nonblocking(true).map_err(|err| err.to_string())?;
        let state = Arc::new(Mutex::new(State {
            token: String::new(),
            offer: Offer::Nothing,
            outcome: None,
            events: Vec::new(),
        }));
        let done = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, done) = (Arc::clone(&state), Arc::clone(&done));
            std::thread::spawn(move || accept(&listener, &state, &done))
        };
        Ok(Server { address, state, done, thread: Some(thread) })
    }

    /// Where the machine reaches the server.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Offers `offer` under a new token, which the boot's URLs start
    /// with, and forgets the last boot's outcome. Gives the boot's base
    /// URL, `http://<address>/<token>/`.
    pub fn offer(&self, offer: Offer) -> Result<String, String> {
        let token = random_hex(16)?;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.token.clone_from(&token);
        state.offer = offer;
        state.outcome = None;
        Ok(format!("http://{}/{token}/", self.address))
    }

    /// Takes the offer back: what is asked for after is 404.
    pub fn withdraw(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.token.clear();
        state.offer = Offer::Nothing;
    }

    /// The writer's outcome, once it has said.
    pub fn outcome(&self) -> Option<Outcome> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).outcome.clone()
    }

    /// What the server did since the last call.
    pub fn take_events(&self) -> Vec<String> {
        std::mem::take(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner).events)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Takes connections until `done`, each in a thread of its own: the
/// data's may take a while.
fn accept(listener: &TcpListener, state: &Arc<Mutex<State>>, done: &AtomicBool) {
    while !done.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let state = Arc::clone(state);
                std::thread::spawn(move || answer(&stream, &state));
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// What the writer says when it is done.
#[derive(Deserialize)]
struct Done {
    sha256: String,
}

/// Answers one request.
fn answer(stream: &TcpStream, state: &Mutex<State>) {
    // Connections are blocking, whatever the listener is.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    let Ok(request) = http::read_request(stream) else { return };
    let mut parts = request.path.trim_start_matches('/').splitn(2, '/');
    let (token, what) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
    if guard.token.is_empty() || token != guard.token {
        drop(guard);
        http::write_response(stream, &Response::text(404, "no such boot\n"));
        return;
    }
    guard.events.push(format!("{} {what}", request.method));
    match (request.method.as_str(), what, &guard.offer) {
        ("GET", "kernel" | "initrd", Offer::Writer { kit, .. } | Offer::Twin { kit }) => {
            let file = if what == "kernel" { kit.kernel.clone() } else { kit.initrd.clone() };
            drop(guard);
            send_file(stream, &file);
        }
        ("GET", "plan", Offer::Writer { image, disk, .. }) => {
            let plan = json!({
                "version": PLAN_VERSION,
                "disk": disk,
                "size": image.size,
                "sha256": image.sha256,
                "extents": image.extents,
            });
            drop(guard);
            http::write_response(stream, &Response::json(200, &plan));
        }
        ("GET", "data", Offer::Writer { image, .. }) => {
            let image = Arc::clone(image);
            drop(guard);
            if let Err(err) = send_data(stream, &image) {
                let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
                guard.events.push(format!("sending the data broke off: {err}"));
            }
        }
        ("POST", "log", _) => {
            let line = String::from_utf8_lossy(&request.body).trim_end().to_string();
            guard.events.push(format!("writer: {line}"));
            drop(guard);
            http::write_response(stream, &Response::text(200, "ok\n"));
        }
        ("POST", "failed", Offer::Writer { .. }) => {
            let why = String::from_utf8_lossy(&request.body).trim().to_string();
            guard.outcome = Some(Outcome::Failed(format!("the writer failed: {why}")));
            drop(guard);
            http::write_response(stream, &Response::text(200, "power off\n"));
        }
        ("POST", "done", Offer::Writer { image, written, .. }) => {
            let said = serde_json::from_slice::<Done>(&request.body)
                .map(|done| done.sha256.to_ascii_lowercase());
            match said {
                Ok(sha256) if sha256 == image.sha256 => {
                    written();
                    guard.outcome = Some(Outcome::Written);
                    drop(guard);
                    http::write_response(stream, &Response::text(200, "restart\n"));
                }
                Ok(sha256) => {
                    guard.outcome = Some(Outcome::Failed(format!(
                        "the disk did not hold the image once written: it read back as \
                         SHA-256 {sha256}, the image is {}",
                        image.sha256
                    )));
                    drop(guard);
                    http::write_response(stream, &Response::text(409, "wrong hash\n"));
                }
                Err(err) => {
                    drop(guard);
                    http::write_response(stream, &Response::text(400, &format!("{err}\n")));
                }
            }
        }
        _ => {
            drop(guard);
            http::write_response(stream, &Response::text(404, "not here\n"));
        }
    }
}

/// Sends a whole file.
fn send_file(stream: &TcpStream, path: &Path) {
    let Ok(mut file) = File::open(path) else {
        http::write_response(stream, &Response::text(404, "missing\n"));
        return;
    };
    let length = file.metadata().map(|meta| meta.len()).unwrap_or_default();
    let mut out = stream;
    let _ = http::write_head(stream, 200, "application/octet-stream", length)
        .and_then(|()| io::copy(&mut file, &mut out).map(drop));
}

/// Sends the image's extents, one after another.
fn send_data(stream: &TcpStream, image: &Image) -> io::Result<()> {
    let mut file = File::open(&image.path)?;
    http::write_head(stream, 200, "application/octet-stream", image.data_bytes())?;
    let mut out = stream;
    for &(offset, length) in &image.extents {
        file.seek(SeekFrom::Start(offset))?;
        let copied = io::copy(&mut (&mut file).take(length), &mut out)?;
        if copied != length {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the image got shorter"));
        }
    }
    out.flush()
}

/// The GRUB script for one boot: load `kit`'s kernel and initrd from
/// `base` (the boot's URL), with `cmdline`.
pub fn grub_script(what: &str, base: &str, cmdline: &str) -> String {
    // GRUB names an HTTP server as (http,<host:port>) and takes paths on it.
    let rest = base.trim_start_matches("http://");
    let (server, path) = rest.split_once('/').unwrap_or((rest, ""));
    format!(
        "# Written by dukovo-lab for one boot, and removed once it has begun.\n\
         set timeout=0\n\
         insmod http\n\
         echo 'dukovo-lab: loading {what}'\n\
         linux (http,{server})/{path}kernel {cmdline}\n\
         initrd (http,{server})/{path}initrd\n\
         boot\n"
    )
}

/// Where the boot script for the machine with network card `mac` goes in
/// the TFTP folder.
pub fn script_path(tftp: &Path, mac: &str) -> PathBuf {
    tftp.join("dukovo-lab").join("boot").join(format!("{}.cfg", mac.to_ascii_lowercase()))
}

/// Writes the boot script whole (a half-written one is never read), so
/// that dnsmasq, which runs as its own user, can read it.
pub fn write_script(path: &Path, script: &str) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let folder = path.parent().ok_or("the boot script has no folder")?;
    fs::create_dir_all(folder).map_err(|err| {
        format!(
            "making {}: {err} (`ob lab` sets the TFTP folder up for the lab PC's user)",
            folder.display()
        )
    })?;
    let temporary = path.with_extension("cfg.new");
    fs::write(&temporary, script)
        .and_then(|()| fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644)))
        .and_then(|()| fs::rename(&temporary, path))
        .map_err(|err| format!("writing {}: {err}", path.display()))
}

/// Removes the boot script, so the machine's next net boot goes on to
/// its disk.
pub fn remove_script(path: &Path) {
    let _ = fs::remove_file(path);
}

/// `bytes` random bytes from the system, in hexadecimal.
pub fn random_hex(bytes: usize) -> Result<String, String> {
    let mut random = vec![0u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|err| format!("/dev/urandom: {err}"))?;
    Ok(hex(&random))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::net::Ipv4Addr;
    use std::time::Instant;

    /// A pretend writer: what `ob lab`'s does, over HTTP, with a file as
    /// the disk. Gives what the lab answered to `done`.
    pub fn pretend_writer(base: &str, disk: &Path, tamper: bool) -> Result<String, String> {
        let rest = base.trim_start_matches("http://");
        let (server, path) = rest.split_once('/').unwrap();
        let get = |what: &str| {
            let answer = http::send(
                server,
                "GET",
                &format!("/{path}{what}"),
                &[],
                &[],
                Duration::from_secs(10),
            )?;
            if answer.status != 200 {
                return Err(format!("GET {what}: {}", answer.status));
            }
            Ok(answer.body)
        };
        let plan: serde_json::Value =
            serde_json::from_slice(&get("plan")?).map_err(|err| err.to_string())?;
        assert_eq!(plan["version"], PLAN_VERSION);
        let size = plan["size"].as_u64().unwrap();
        let data = get("data")?;
        // Zeros where the image has none of its data, then the data.
        let mut written = vec![0u8; usize::try_from(size).unwrap()];
        let mut at = 0;
        for extent in plan["extents"].as_array().unwrap() {
            let (offset, length) = (extent[0].as_u64().unwrap(), extent[1].as_u64().unwrap());
            let (offset, length) = (offset as usize, length as usize);
            written[offset..offset + length].copy_from_slice(&data[at..at + length]);
            at += length;
        }
        assert_eq!(at, data.len());
        if tamper {
            written[0] ^= 1;
        }
        fs::write(disk, &written).unwrap();
        let mut sha = Sha256::new();
        sha.update(&written);
        let body = json!({ "sha256": hex(&sha.finish()) }).to_string();
        let answer = http::send(
            server,
            "POST",
            &format!("/{path}done"),
            &[],
            body.as_bytes(),
            Duration::from_secs(10),
        )?;
        Ok(String::from_utf8_lossy(&answer.body).trim().to_string())
    }

    /// An image of `size` bytes with data at a few places.
    pub fn make_image(dir: &TempDir, size: usize) -> PathBuf {
        let mut bytes = vec![0u8; size];
        for (index, byte) in bytes.iter_mut().enumerate().take(5000) {
            *byte = (index % 251) as u8 + 1;
        }
        let middle = size / 2 + 17;
        bytes[middle..middle + 3].copy_from_slice(b"mid");
        let end = bytes.len() - 1;
        bytes[end] = 0xee;
        let path = dir.path().join("image.img");
        fs::write(&path, &bytes).unwrap();
        path
    }

    fn kit(dir: &TempDir) -> Kit {
        dir.write("kit/kernel", "KERNEL");
        dir.write("kit/initrd", "INITRD");
        dir.write(
            "kit/kit.toml",
            "kernel = \"kernel\"\ninitrd = \"initrd\"\ncmdline = \"console=ttyS0 rdinit=/writer\"\ndone = \"writer: done\"\n",
        );
        Kit::read(&dir.path().join("kit")).unwrap()
    }

    #[test]
    fn finds_where_the_image_has_data() {
        let dir = TempDir::new("netboot-scan");
        let image = Image::scan(&make_image(&dir, 1 << 20)).unwrap();
        assert_eq!(image.size, 1 << 20);
        let grain = GRAIN as u64;
        let middle = (1u64 << 19) / grain * grain;
        assert_eq!(image.extents, [(0, grain), (middle, grain), ((1 << 20) - grain, grain)]);
        assert_eq!(image.data_bytes(), 3 * grain);
        let mut sha = Sha256::new();
        sha.update(&fs::read(&image.path).unwrap());
        assert_eq!(image.sha256, hex(&sha.finish()));
    }

    #[test]
    fn hands_a_writer_its_plan_and_data_and_checks_what_it_wrote() {
        let dir = TempDir::new("netboot-serve");
        let image = Arc::new(Image::scan(&make_image(&dir, 300_000)).unwrap());
        let server = Server::start(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).unwrap();
        let told = Arc::new(AtomicBool::new(false));
        let offer = |told: &Arc<AtomicBool>| Offer::Writer {
            kit: kit(&dir),
            image: Arc::clone(&image),
            disk: "S3Z5NB0K123456".into(),
            written: {
                let told = Arc::clone(told);
                Box::new(move || told.store(true, Ordering::Relaxed))
            },
        };
        let base = server.offer(offer(&told)).unwrap();
        let disk = dir.path().join("disk");
        assert_eq!(pretend_writer(&base, &disk, false).unwrap(), "restart");
        assert_eq!(server.outcome(), Some(Outcome::Written));
        assert!(told.load(Ordering::Relaxed));
        assert_eq!(fs::read(&disk).unwrap(), fs::read(&image.path).unwrap());
        let events = server.take_events();
        assert_eq!(events, ["GET plan", "GET data", "POST done"]);

        // The kernel and initrd are the kit's.
        let rest = base.trim_start_matches("http://");
        let (host, path) = rest.split_once('/').unwrap();
        let kernel =
            http::send(host, "GET", &format!("/{path}kernel"), &[], &[], Duration::from_secs(5))
                .unwrap();
        assert_eq!(kernel.body, b"KERNEL");

        // A disk that reads back wrong fails the boot, and the old token
        // is gone with the new offer.
        let told_again = Arc::new(AtomicBool::new(false));
        let next = server.offer(offer(&told_again)).unwrap();
        assert!(pretend_writer(&base, &disk, false).is_err());
        assert_eq!(pretend_writer(&next, &disk, true).unwrap(), "wrong hash");
        assert!(
            matches!(server.outcome(), Some(Outcome::Failed(why)) if why.contains("read back"))
        );
        assert!(!told_again.load(Ordering::Relaxed));

        server.withdraw();
        assert!(pretend_writer(&next, &disk, false).is_err());
    }

    #[test]
    fn the_writer_s_failure_is_kept() {
        let server = Server::start(IpAddr::V4(Ipv4Addr::LOCALHOST), 0).unwrap();
        let dir = TempDir::new("netboot-fail");
        let image = Arc::new(Image::scan(&make_image(&dir, 100_000)).unwrap());
        let base = server
            .offer(Offer::Writer {
                kit: kit(&dir),
                image,
                disk: "X".into(),
                written: Box::new(|| {}),
            })
            .unwrap();
        let rest = base.trim_start_matches("http://");
        let (host, path) = rest.split_once('/').unwrap();
        let started = Instant::now();
        http::send(
            host,
            "POST",
            &format!("/{path}failed"),
            &[],
            b"no disk X",
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(server.outcome(), Some(Outcome::Failed("the writer failed: no disk X".into())));
    }

    #[test]
    fn boot_scripts_load_from_the_lab_pc() {
        let script =
            grub_script("the writer", "http://10.77.0.1:7431/abc/", "console=ttyS4,115200");
        assert!(script.contains("linux (http,10.77.0.1:7431)/abc/kernel console=ttyS4,115200\n"));
        assert!(script.contains("initrd (http,10.77.0.1:7431)/abc/initrd\n"));
        let dir = TempDir::new("netboot-script");
        let path = script_path(dir.path(), "AA:BB:CC:00:11:22");
        assert!(path.ends_with("dukovo-lab/boot/aa:bb:cc:00:11:22.cfg"));
        write_script(&path, &script).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), script);
        remove_script(&path);
        assert!(!path.exists());
    }
}
