// SPDX-FileCopyrightText: 2026 The Dukovo contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A machine's screen as a picture from an HDMI capture stick on the lab
//! PC (an MS2130 or any UVC stick), for plug machines that have no screen
//! of their own to dump.
//!
//! A UVC device cannot be read with `read()` and its ioctls need `unsafe`,
//! so one picture is taken by running `v4l2-ctl` (Mint: the `v4l-utils`
//! package), which writes one raw YUYV frame to a file. The frame is
//! converted to RGB here and handed to screen.rs, which reads the console's
//! text from it allowing for the stick's [`NOISE`].
//!
//! When a picture fails, the error holds `v4l2-ctl`'s own last line, and
//! running the same command by hand shows the rest:
//! `v4l2-ctl -d /dev/video0 --list-formats-ext` lists what the stick
//! offers. Two sticks on one hub can refuse to stream at the same moment,
//! which is why a failed try is made once more.

use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::screen::{self, Image};

/// How far, in levels of brightness (0 to 255), a captured pixel may stray
/// from what the machine drew: the stick's conversion to YUYV and back
/// rounds, and its colour is shared by each pair of pixels.
pub const NOISE: u8 = 8;

/// How long `v4l2-ctl` is given for one picture.
const TRY_TIME: Duration =
    if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(15) };

/// How long a failed try waits before the second.
const RETRY_PAUSE: Duration =
    if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(1) };

/// How often a running `v4l2-ctl` is looked at.
const POLL: Duration = Duration::from_millis(10);

/// The lowest luma a pixel has to count as lit in [`lit_share`]: black on
/// a stick is not 0 but 16 in limited range, and rounds up from there.
const LIT_LUMA: u8 = 32;

/// A capture stick, as the lab PC sees it.
pub struct Capture {
    device: PathBuf,
    width: u32,
    height: u32,
    /// Where each picture is written before it is read.
    frame_file: PathBuf,
    /// The program run, `v4l2-ctl` (tests put a stand-in here).
    pub program: PathBuf,
}

impl Capture {
    /// A stick at `device` (`/dev/video0`) asked for pictures of `width` by
    /// `height` pixels, which pass through `frame_file`.
    pub fn new(device: &Path, (width, height): (u32, u32), frame_file: PathBuf) -> Capture {
        Capture {
            device: device.to_path_buf(),
            width,
            height,
            frame_file,
            program: "v4l2-ctl".into(),
        }
    }

    /// The video device the pictures come from.
    pub fn device(&self) -> &Path {
        &self.device
    }

    /// The size of the pictures, width and height in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The screen now. A failed try is made once more after a pause.
    ///
    /// # Errors
    ///
    /// The second try's error: `v4l2-ctl` missing, failing or too slow, or
    /// a picture of another size than was asked for.
    pub fn frame(&self) -> Result<Image, String> {
        // The first error is dropped: when both tries fail the second says
        // what is wrong now.
        self.try_frame().or_else(|_| {
            sleep(RETRY_PAUSE);
            self.try_frame()
        })
    }

    fn try_frame(&self) -> Result<Image, String> {
        self.run_program()?;
        let bytes = fs::read(&self.frame_file).map_err(|err| {
            format!(
                "reading the picture {}: {err}; v4l2-ctl said it was done, so the folder may be unwritable",
                self.frame_file.display()
            )
        })?;
        // A stale file would be read as the next picture if it stayed.
        let _ = fs::remove_file(&self.frame_file);
        let wanted = u64::from(self.width) * u64::from(self.height) * 2;
        if bytes.len() as u64 != wanted {
            return Err(format!(
                "the capture stick at {device} gave {got} bytes, not a {w}x{h} YUYV picture \
                 ({wanted} bytes): it may not offer {w}x{h} in YUYV on this port \
                 (v4l2-ctl -d {device} --list-formats-ext lists what it offers; 1920x1080 \
                 YUYV needs a USB 3 port)",
                device = self.device.display(),
                got = bytes.len(),
                w = self.width,
                h = self.height,
            ));
        }
        from_yuyv(self.width, self.height, &bytes)
    }

    /// Runs the program until it has written the frame, or stops it at
    /// `TRY_TIME`.
    fn run_program(&self) -> Result<(), String> {
        let device = self.device.display();
        let mut child = Command::new(&self.program)
            .arg(format!("--device={device}"))
            .arg(format!(
                "--set-fmt-video=width={},height={},pixelformat=YUYV",
                self.width, self.height
            ))
            // The first frames after streaming starts may be stale.
            .args(["--stream-mmap=3", "--stream-skip=2", "--stream-count=1"])
            .arg(format!("--stream-to={}", self.frame_file.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                if err.kind() == std::io::ErrorKind::NotFound {
                    "v4l2-ctl is not installed: sudo apt install v4l-utils".to_string()
                } else {
                    format!(
                        "starting {}: {err}; is it executable by this user?",
                        self.program.display()
                    )
                }
            })?;
        let deadline = Instant::now() + TRY_TIME;
        let status = loop {
            if let Some(status) =
                child.try_wait().map_err(|err| format!("waiting for v4l2-ctl: {err}"))?
            {
                break status;
            }
            if Instant::now() >= deadline {
                // Killing a child that already ended is not a failure to report.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "v4l2-ctl took no picture from {device} within {} s",
                    TRY_TIME.as_secs_f64()
                ));
            }
            sleep(POLL);
        };
        if status.success() {
            return Ok(());
        }
        let mut said = Vec::new();
        if let Some(mut stderr) = child.stderr.take() {
            // The child has ended, so its end of the pipe is closed and
            // this reads to the end of what it said without waiting.
            let _ = stderr.read_to_end(&mut said);
        }
        let said = String::from_utf8_lossy(&said);
        let last = said.lines().rev().map(str::trim).find(|line| !line.is_empty());
        let reason = last.map_or_else(|| format!("it exited with {status}"), str::to_string);
        Err(format!(
            "v4l2-ctl could not take a picture from {device}: {reason} (is the stick plugged \
             in, is this user in the video group, and is no other program showing its picture?)"
        ))
    }
}

/// `WIDTHxHEIGHT`, as `plug.capture_size` is written: width even (YUYV
/// holds pixels in pairs), 2 to 7680, height 1 to 4320.
///
/// # Errors
///
/// Says what `text` should look like when it is not a size in range.
pub fn parse_size(text: &str) -> Result<(u32, u32), String> {
    let number = |digits: &str| digits.parse::<u32>().ok();
    let size = text.split_once('x').and_then(|(w, h)| Some((number(w)?, number(h)?)));
    let Some((width, height)) = size else {
        return Err(format!("the capture size {text:?} is not WIDTHxHEIGHT, such as 1920x1080"));
    };
    if !width.is_multiple_of(2) || !(2..=7680).contains(&width) {
        return Err(format!(
            "the capture size {text:?} has a width of {width}: it must be even (YUYV holds \
             pixels in pairs) and from 2 to 7680"
        ));
    }
    if !(1..=4320).contains(&height) {
        return Err(format!(
            "the capture size {text:?} has a height of {height}: it must be from 1 to 4320"
        ));
    }
    Ok((width, height))
}

/// A YUYV picture (Y0 Cb Y1 Cr per pair) as RGB: BT.601, limited range.
///
/// # Errors
///
/// When `bytes` are not `width * height * 2`, or `width` is odd.
pub fn from_yuyv(width: u32, height: u32, bytes: &[u8]) -> Result<Image, String> {
    let wanted = u64::from(width) * u64::from(height) * 2;
    if !width.is_multiple_of(2) || bytes.len() as u64 != wanted {
        return Err(format!(
            "{} bytes are not a {width}x{height} YUYV picture ({wanted} bytes, and a width that is even)",
            bytes.len()
        ));
    }
    let mut pixels = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(4) {
        let (cb, cr) = (pair[1], pair[3]);
        pixels.push(rgb(pair[0], cb, cr));
        pixels.push(rgb(pair[2], cb, cr));
    }
    Image::new(width as usize, height as usize, pixels)
}

/// One pixel from its luma and its pair's chroma. A channel that the
/// chroma would push out of 0 to 255 is not clamped: the chroma part is
/// scaled down for all three channels alike, the most that fits, which
/// keeps the pixel's luma at `y`. The chroma of a pair is shared, so a
/// black pixel beside green text gets green chroma it cannot show;
/// clamping alone would raise its luma to about 30, and the text reader
/// compares luma.
fn rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let luma = ((f32::from(y) - 16.0) * 255.0 / 219.0).clamp(0.0, 255.0);
    let blue = (f32::from(cb) - 128.0) * 255.0 / 224.0;
    let red = (f32::from(cr) - 128.0) * 255.0 / 224.0;
    let chroma = [1.402 * red, -0.344_136 * blue - 0.714_136 * red, 1.772 * blue];
    let mut share = 1.0f32;
    for part in chroma {
        if part > 0.0 {
            share = share.min((255.0 - luma) / part);
        } else if part < 0.0 {
            share = share.min(luma / -part);
        }
    }
    // The share keeps each channel within 0 to 255; rounding may land on
    // the edge.
    chroma.map(|part| (luma + share * part).round().clamp(0.0, 255.0) as u8)
}

/// The share of pixels brighter than black (luma 32 and up), for
/// `machines check` to tell a picture from a dark one.
pub fn lit_share(image: &Image) -> f64 {
    let pixels = image.pixels();
    if pixels.is_empty() {
        return 0.0;
    }
    let lit = pixels.iter().filter(|&&pixel| screen::luma(pixel) >= LIT_LUMA).count();
    lit as f64 / pixels.len() as f64
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::testing::TempDir;

    /// A picture as a stick would send it: BT.601 limited range, the
    /// chroma of each pair the mean of its two pixels'.
    pub(crate) fn to_yuyv(image: &Image) -> Vec<u8> {
        let ycc = |[r, g, b]: [u8; 3]| {
            let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
            let y = 0.299 * r + 0.587 * g + 0.114 * b;
            [
                16.0 + y * 219.0 / 255.0,
                128.0 + (b - y) / 1.772 * 224.0 / 255.0,
                128.0 + (r - y) / 1.402 * 224.0 / 255.0,
            ]
        };
        let mut bytes = Vec::new();
        for pair in image.pixels().chunks_exact(2) {
            let (a, b) = (ycc(pair[0]), ycc(pair[1]));
            for value in [a[0], (a[1] + b[1]) / 2.0, b[0], (a[2] + b[2]) / 2.0] {
                bytes.push(value.round().clamp(0.0, 255.0) as u8);
            }
        }
        bytes
    }

    fn flat(width: usize, height: usize, colour: [u8; 3]) -> Image {
        Image::new(width, height, vec![colour; width * height]).unwrap()
    }

    /// A shell script in `dir` that stands in for `v4l2-ctl`, executable,
    /// and tried once with `--probe`, which it answers without running
    /// `body`, so that a test thread's fork still holding the file open for
    /// writing (ETXTBSY) is over before the test depends on it.
    pub(crate) fn stand_in(dir: &TempDir, body: &str) -> PathBuf {
        let script = format!("#!/bin/sh\n[ \"$1\" = --probe ] && exit 0\n{body}\n");
        let path = dir.write("v4l2-ctl", &script);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        for _ in 0..200 {
            match Command::new(&path).arg("--probe").output() {
                Err(err) if err.raw_os_error() == Some(26) => sleep(Duration::from_millis(10)),
                _ => break,
            }
        }
        path
    }

    /// A stick of 4x2 pixels whose program is `body`, and the folder.
    fn stick(body: impl Fn(&Path) -> String) -> (TempDir, Capture) {
        let dir = TempDir::new("capture");
        let program = stand_in(&dir, &body(dir.path()));
        let mut capture =
            Capture::new(Path::new("/dev/video9"), (4, 2), dir.path().join("frame.yuyv"));
        capture.program = program;
        (dir, capture)
    }

    /// The script text that copies the prepared picture to the
    /// `--stream-to=` path and records the arguments it was given.
    pub(crate) fn copying(dir: &Path) -> String {
        format!(
            "printf '%s\\n' \"$@\" > '{args}'\n\
             for arg in \"$@\"; do case \"$arg\" in --stream-to=*) cp '{frame}' \"${{arg#--stream-to=}}\";; esac; done",
            args = dir.join("args").display(),
            frame = dir.join("prepared.yuyv").display(),
        )
    }

    fn gray_picture(dir: &TempDir) {
        // Y 126 is (126 - 16) * 255 / 219 = 128 in full range.
        fs::write(dir.path().join("prepared.yuyv"), [126, 128, 126, 128].repeat(4)).unwrap();
    }

    #[test]
    fn a_frame_is_what_the_program_wrote_converted_to_rgb() {
        let (dir, capture) = stick(copying);
        gray_picture(&dir);
        let image = capture.frame().unwrap();
        assert_eq!((image.width, image.height), (4, 2));
        assert!(image.pixels().iter().all(|&pixel| pixel == [128, 128, 128]));
        assert_eq!(capture.size(), (4, 2));
        assert_eq!(capture.device(), Path::new("/dev/video9"));
        assert!(!dir.path().join("frame.yuyv").exists());
    }

    #[test]
    fn the_program_is_asked_for_one_yuyv_frame_after_skipping_two() {
        let (dir, capture) = stick(copying);
        gray_picture(&dir);
        capture.frame().unwrap();
        let args = fs::read_to_string(dir.path().join("args")).unwrap();
        let frame_file = dir.path().join("frame.yuyv");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "--device=/dev/video9",
                "--set-fmt-video=width=4,height=2,pixelformat=YUYV",
                "--stream-mmap=3",
                "--stream-skip=2",
                "--stream-count=1",
                &format!("--stream-to={}", frame_file.display()),
            ]
        );
    }

    #[test]
    fn a_failed_try_is_made_once_more() {
        let (dir, capture) = stick(|dir| {
            format!(
                "echo try >> '{tries}'\n\
                 if [ -f '{m}' ]; then {copy}; else touch '{m}'; echo 'busy' >&2; exit 1; fi",
                tries = dir.join("tries").display(),
                m = dir.join("tried").display(),
                copy = copying(dir).replace('\n', "; ")
            )
        });
        gray_picture(&dir);
        assert!(capture.frame().is_ok());
        assert_eq!(fs::read_to_string(dir.path().join("tries")).unwrap(), "try\ntry\n");
    }

    #[test]
    fn a_program_that_fails_twice_gives_its_last_line_of_complaint() {
        let (_dir, capture) =
            stick(|_| "echo 'first' >&2; echo 'cannot open device' >&2; exit 1".into());
        let err = capture.frame().err().unwrap();
        assert!(
            err.starts_with(
                "v4l2-ctl could not take a picture from /dev/video9: cannot open device ("
            ),
            "{err}"
        );
        assert!(err.contains("video group"), "{err}");
    }

    #[test]
    fn a_program_that_says_nothing_is_named_by_its_exit_status() {
        let (_dir, capture) = stick(|_| "exit 3".into());
        assert!(capture.frame().err().unwrap().contains("it exited with exit status: 3"));
    }

    #[test]
    fn a_picture_of_the_wrong_size_names_both_sizes() {
        let (dir, capture) = stick(copying);
        fs::write(dir.path().join("prepared.yuyv"), [0u8; 15]).unwrap();
        let err = capture.frame().err().unwrap();
        assert!(err.contains("gave 15 bytes, not a 4x2 YUYV picture (16 bytes)"), "{err}");
        assert!(err.contains("--list-formats-ext") && err.contains("USB 3"), "{err}");
    }

    #[test]
    fn a_program_that_hangs_is_stopped_at_the_time_limit() {
        let (_dir, capture) = stick(|_| "exec sleep 30".into());
        let started = Instant::now();
        let err = capture.frame().err().unwrap();
        assert_eq!(err, "v4l2-ctl took no picture from /dev/video9 within 0.5 s");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_says_how_to_install_it() {
        let (dir, mut capture) = stick(copying);
        capture.program = dir.path().join("no-such-program");
        assert_eq!(
            capture.frame().err().unwrap(),
            "v4l2-ctl is not installed: sudo apt install v4l-utils"
        );
    }

    #[test]
    fn a_size_is_two_numbers_with_an_even_width() {
        assert_eq!(parse_size("1920x1080"), Ok((1920, 1080)));
        assert_eq!(parse_size("2x1"), Ok((2, 1)));
        assert_eq!(parse_size("7680x4320"), Ok((7680, 4320)));
        for bad in [
            "1920",
            "1920x",
            "x1080",
            "1920X1080",
            "1921x1080",
            "0x1080",
            "7682x1080",
            "1920x0",
            "1920x4321",
            "-2x4",
        ] {
            assert!(parse_size(bad).unwrap_err().contains(bad), "{bad}");
        }
    }

    #[test]
    fn pure_colours_keep_their_luma_through_yuyv() {
        for colour in
            [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255], [0, 0, 0], [240, 230, 200]]
        {
            let image = flat(2, 1, colour);
            let back = from_yuyv(2, 1, &to_yuyv(&image)).unwrap();
            for &pixel in back.pixels() {
                let drift = screen::luma(pixel).abs_diff(screen::luma(colour));
                assert!(drift <= 2, "{colour:?} came back as {pixel:?}");
            }
        }
    }

    #[test]
    fn a_black_pixel_beside_green_keeps_black_luma() {
        let image = Image::new(2, 1, vec![[0, 0, 0], [0, 255, 0]]).unwrap();
        let back = from_yuyv(2, 1, &to_yuyv(&image)).unwrap();
        assert!(screen::luma(back.pixels()[0]) < 3, "{:?}", back.pixels());
        assert!(screen::luma(back.pixels()[1]).abs_diff(screen::luma([0, 255, 0])) <= 2);
    }

    #[test]
    fn an_odd_byte_count_or_width_is_no_picture() {
        assert!(from_yuyv(2, 1, &[16, 128, 16]).err().unwrap().contains("3 bytes"));
        assert!(from_yuyv(2, 1, &[16; 6]).is_err());
        assert!(from_yuyv(3, 1, &[16; 6]).is_err());
    }

    #[test]
    fn the_lit_share_counts_pixels_above_black() {
        let pixels = vec![[0, 0, 0], [16, 16, 16], [31, 31, 31], [32, 32, 32], [255, 255, 255]];
        let image = Image::new(5, 1, pixels).unwrap();
        assert!((lit_share(&image) - 0.4).abs() < 1e-9);
        assert_eq!(lit_share(&Image::new(0, 0, Vec::new()).unwrap()), 0.0);
    }
}
