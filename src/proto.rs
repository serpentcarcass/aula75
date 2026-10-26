//! AULA F75 Max (SONiX 0c45:800a) protocol, reversed from the vendor's DeviceDriver.exe 1.0.0.5.
//!
//! Commands are 64-byte feature reports on the 0xFF13 interface: `04 <cmd> .. [8]=arg`.
//! Writes are wrapped in `04 18` (begin) .. `04 02` (apply) [.. `04 F0` (finish)].
//! Screen frames go as 4096-byte output reports on the 0xFF68 interface.
//!
//! Safety rules (don't brick the board):
//! - Only commands in `Cmd` can be sent. Firmware update, factory reset, keymap (04 11 / 04 27)
//!   and macro writes are deliberately not implemented.
//! - Every value that reaches the wire comes from a validated type (`Mode`, `Level`, `LedIndex`, ...).
//! - Every acknowledged step is checked; on the first bad reply we stop and never send `Apply`,
//!   so a half-written transaction is not committed. Replug the keyboard to clear it.
//! - We only talk to the exact VID/PID/firmware this protocol was verified against.
//! - Live animation uses only `04 20` (real-time), the same path the driver streams for its
//!   music / real-time pages; never `04 23`, which stores to the keyboard.

use crate::layout::KEYS;
use anyhow::{Context, Result, bail, ensure};
use hidapi::{HidApi, HidDevice};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::{
    thread::sleep,
    time::{Duration, Instant},
};

pub const VID: u16 = 0x0c45;
pub const PID: u16 = 0x800a;
/// bcdDevice values seen on boards where this protocol was verified. Add yours only after checking.
const KNOWN_FIRMWARE: &[u16] = &[0x0110];
const CMD_PAGE: u16 = 0xff13;
const BULK_PAGE: u16 = 0xff68;
// layouts/rgb-keyboard.xml: <cmd_delaytime value="35"/>
const DELAY: Duration = Duration::from_millis(35);

pub const SCREEN: usize = 128;
pub const FRAME_BYTES: usize = SCREEN * SCREEN * 2;
pub const MAX_FRAMES: usize = 255; // <screen gif_maxframes="255"/>
const GIF_HEADER: usize = 256; // <screen gif_headlength="256"/>
const PAGE: usize = 4096;

/// The only commands this program can send.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Cmd {
    Begin = 0x18,
    Apply = 0x02,
    Finish = 0xf0,
    Light = 0x13,
    PerKey = 0x23,
    Clock = 0x28,
    Screen = 0x72,
    Realtime = 0x20,
    /// Read-only: the colours the LEDs show right now.
    ReadLeds = 0xf5,
}

impl Cmd {
    fn packet(self, arg: u8) -> [u8; 64] {
        let mut p = [0; 64];
        p[0] = 0x04;
        p[1] = self as u8;
        p[8] = arg;
        p
    }
}

/// A lighting effect from the driver's own table (FUN_0042b380). Cannot be built from a raw byte.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Mode(usize);

/// (name, wire id, config bits). Bits: 1 brightness, 2 speed, 4 direction L/R,
/// 8 direction U/D, 0x10 rainbow toggle, 0x20 colour.
const MODES: &[(&str, u8, u8)] = &[
    ("Off", 0, 0),
    ("Static", 1, 0x31),
    ("Single On", 2, 0x33),
    ("Single Off", 3, 0x33),
    ("Glittering", 4, 0x33),
    ("Falling", 5, 0x33),
    ("Colourful", 6, 0x03),
    ("Breath", 7, 0x33),
    ("Spectrum", 8, 0x03),
    ("Outward", 9, 0x33),
    ("Scrolling", 10, 0x3b),
    ("Rolling", 11, 0x37),
    ("Rotating", 12, 0x37),
    ("Explode", 13, 0x33),
    ("Launch", 14, 0x33),
    ("Ripples", 15, 0x33),
    ("Flowing", 16, 0x37),
    ("Pulsating", 17, 0x33),
    ("Tilt", 18, 0x37),
    ("Shuttle", 19, 0x33),
];
/// "User lighting" mode the driver selects before uploading per-key colours.
const CUSTOM_MODE_ID: u8 = 0x80;

impl Mode {
    pub fn all() -> impl Iterator<Item = Mode> {
        (0..MODES.len()).map(Mode)
    }
    pub fn name(self) -> &'static str {
        MODES[self.0].0
    }
    fn id(self) -> u8 {
        MODES[self.0].1
    }
    fn has(self, bit: u8) -> bool {
        MODES[self.0].2 & bit != 0
    }
    pub fn has_brightness(self) -> bool {
        self.has(0x01)
    }
    pub fn has_speed(self) -> bool {
        self.has(0x02)
    }
    pub fn has_horizontal(self) -> bool {
        self.has(0x04)
    }
    pub fn has_vertical(self) -> bool {
        self.has(0x08)
    }
    pub fn has_rainbow(self) -> bool {
        self.has(0x10)
    }
    pub fn has_color(self) -> bool {
        self.has(0x20)
    }
}

/// 0..=5, the range the driver's sliders allow (`brightness_max`/`speed_max`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Level(u8);

impl Level {
    pub const MAX: u8 = 5;
    pub fn new(v: u8) -> Self {
        Self(v.min(Self::MAX))
    }
    pub fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Direction {
    Left = 0,
    Right = 1,
    Up = 2,
    Down = 3,
}

impl Direction {
    /// Indexed by the wire value.
    pub const ALL: [Direction; 4] = [Direction::Left, Direction::Right, Direction::Up, Direction::Down];
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Light {
    pub mode: Mode,
    pub rgb: [u8; 3],
    pub rainbow: bool,
    pub brightness: Level,
    pub speed: Level,
    pub direction: Direction,
}

/// LED slot that exists on this board's layout. Only obtainable from `KEYS`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LedIndex(u8);

impl LedIndex {
    pub fn of_key(key: usize) -> Option<Self> {
        KEYS.get(key).map(|k| Self(k.1))
    }
}

/// Validated screen animation: 1..=255 frames, each exactly 128x128 RGB565-LE.
pub struct Animation(Vec<(Vec<u8>, u32)>);

impl Animation {
    /// `frames`: RGBA8 128x128 images with delay in ms (driver allows 30..=500).
    pub fn from_rgba(frames: &[(Vec<u8>, u32)]) -> Result<Self> {
        ensure!(
            (1..=MAX_FRAMES).contains(&frames.len()),
            "need 1..={MAX_FRAMES} frames, got {}",
            frames.len()
        );
        let mut out = Vec::with_capacity(frames.len());
        for (i, (rgba, delay)) in frames.iter().enumerate() {
            ensure!(rgba.len() == SCREEN * SCREEN * 4, "frame {i} is not {SCREEN}x{SCREEN}");
            out.push((rgb565(rgba), (*delay).clamp(30, 500)));
        }
        Ok(Self(out))
    }
}

/// What every LED shows right now, as returned by `04 F5`: `[led, R, G, B]` at `led * 4`,
/// the same layout as `04 23`.
pub struct LedColors([u8; 576]);

impl LedColors {
    pub fn get(&self, LedIndex(i): LedIndex) -> [u8; 3] {
        let o = i as usize * 4; // LedIndex comes from KEYS, max 121 -> 484..488
        [self.0[o + 1], self.0[o + 2], self.0[o + 3]]
    }
}

/// Outcome of a live frame. `Busy` = the keyboard declined it; nothing is stored, just skip it.
#[derive(Debug, PartialEq)]
pub enum Sent {
    Shown,
    Busy,
}

/// One real-time frame: up to 128 `[led, r, g, b]` records packed back to back (FUN_004329c0),
/// zero padded, no trailer. Unlike `04 23`, records are not placed at `led * 4`.
pub struct RtFrame([u8; 512]);

impl RtFrame {
    pub fn new(keys: &[(LedIndex, [u8; 3])]) -> Self {
        let mut d = [0; 512];
        for (n, &(LedIndex(i), [r, g, b])) in keys.iter().take(128).enumerate() {
            d[n * 4..n * 4 + 4].copy_from_slice(&[i, r, g, b]);
        }
        Self(d)
    }
}

fn magic(p: &mut [u8]) {
    let n = p.len();
    p[n - 2] = 0xaa;
    p[n - 1] = 0x55;
}

fn light_packet(l: &Light) -> [u8; 64] {
    light_raw(l.mode.id(), l.rgb, l.rainbow, l.brightness, l.speed, l.direction)
}

fn light_raw(mode: u8, rgb: [u8; 3], rainbow: bool, br: Level, sp: Level, dir: Direction) -> [u8; 64] {
    let mut p = [0; 64];
    p[0] = mode;
    if mode != 0 {
        p[1..4].copy_from_slice(&rgb);
        p[8] = rainbow as u8;
        p[9] = br.0;
        p[10] = sp.0;
        p[11] = dir as u8;
    }
    magic(&mut p[..16]);
    p
}

/// 144 slots of [led, r, g, b]; the last two bytes are the AA 55 trailer.
fn per_key_packet(keys: &[(LedIndex, [u8; 3])]) -> [u8; 576] {
    let mut d = [0; 576];
    for &(LedIndex(i), [r, g, b]) in keys {
        let o = i as usize * 4; // LedIndex comes from KEYS, max 121 -> 484..488, never hits the trailer
        d[o..o + 4].copy_from_slice(&[i, r, g, b]);
    }
    magic(&mut d);
    d
}

/// Header (frame count + per-frame delay/2) followed by frames, padded with 0xFF to 4K pages.
fn screen_blob(a: &Animation) -> Vec<u8> {
    let len = GIF_HEADER + a.0.len() * FRAME_BYTES;
    let mut b = vec![0xff; len.div_ceil(PAGE) * PAGE];
    b[0] = a.0.len() as u8;
    for (i, (px, delay_ms)) in a.0.iter().enumerate() {
        b[1 + i] = (delay_ms / 2) as u8; // 15..=250
        let o = GIF_HEADER + i * FRAME_BYTES;
        b[o..o + FRAME_BYTES].copy_from_slice(px);
    }
    b
}

fn rgb565(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .flat_map(|p| ((p[0] as u16 >> 3) << 11 | (p[1] as u16 >> 2) << 5 | p[2] as u16 >> 3).to_le_bytes())
        .collect()
}

/// Reply must echo `04 <cmd>`; byte 3 = 0xFF means the firmware rejected/was busy.
fn check_ack(cmd: Cmd, r: &[u8; 64]) -> Result<()> {
    ensure!(
        r[0] == 0x04 && r[1] == cmd as u8,
        "{cmd:?}: unexpected reply {:02x?}",
        &r[..4]
    );
    ensure!(r[3] != 0xff, "{cmd:?}: keyboard reported busy/error");
    Ok(())
}

pub struct Keyboard {
    cmd: HidDevice,
    bulk: Option<HidDevice>,
    /// Identifies this plug-in of the board; changes when it is replugged.
    instance: String,
    dir: PathBuf,
    _lock: File,
}

pub const INTERRUPTED: &str =
    "A previous write was interrupted. Unplug the keyboard for a few seconds and plug it back in.";

/// Per-user runtime directory for the process lock and the interrupted-write marker.
///
/// Derived from the uid, not environment variables, so a window and a command (or an ssh
/// session) always agree. Root is refused: `sudo` would silently use a different directory.
fn runtime_dir() -> Result<PathBuf> {
    // SAFETY: geteuid/getuid cannot fail.
    ensure!(
        unsafe { libc::geteuid() } != 0,
        "don't run aula-control as root (on Linux, install the udev rule instead)"
    );
    #[cfg(target_os = "linux")]
    let dir = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
    #[cfg(target_os = "macos")]
    let dir = {
        let mut buf = [0 as libc::c_char; 1024];
        // SAFETY: the buffer and its length match; confstr NUL-terminates within it.
        let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
        ensure!(n > 0 && n <= buf.len(), "cannot find the per-user temp directory");
        // SAFETY: confstr wrote a NUL-terminated string into buf.
        PathBuf::from(
            unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        )
    };
    ensure!(dir.is_dir(), "runtime directory {} is missing", dir.display());
    Ok(dir)
}

const MARKER: &str = "aula75.interrupted";

/// Atomically records `instance` as "write in progress" (temp file + rename, no symlinks).
fn set_marker(dir: &Path, instance: &str) -> Result<()> {
    let tmp = dir.join(format!("{MARKER}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    f.write_all(instance.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, dir.join(MARKER))?;
    Ok(())
}

/// Opens `path` without following a symlink (the fallback dir may be a shared /tmp).
fn open_private(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Only one process (GUI window or CLI command) may hold the keyboard at a time.
fn take_lock(dir: &Path) -> Result<File> {
    let f = open_private(&dir.join("aula75.lock"))?;
    // SAFETY: flock on a file descriptor we own.
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    ensure!(
        r == 0,
        "the keyboard is in use by another aula75 window or command; close it first"
    );
    Ok(f)
}

/// Refuses while the marker names this same plug-in of the board; a marker for an older plug-in
/// means it has been replugged since, so it's cleared. Only "no marker" passes: any other error
/// (unreadable, symlink, ...) refuses, failing safe.
fn check_interrupted(dir: &Path, instance: &str) -> Result<()> {
    let marker = dir.join(MARKER);
    let mut s = String::new();
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&marker)
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => bail!("cannot check {}: {e}; refusing to touch the keyboard", marker.display()),
        Ok(mut f) => f.read_to_string(&mut s)?,
    };
    ensure!(s != instance, "{INTERRUPTED} (marker: {})", marker.display());
    std::fs::remove_file(&marker)?;
    Ok(())
}

/// A string that changes whenever the board re-enumerates: on macOS hidapi's path carries the
/// IORegistry entry id; on Linux the sysfs HID device name has a per-plug instance counter.
fn instance_id(info: &hidapi::DeviceInfo) -> String {
    let path = info.path().to_string_lossy().into_owned();
    #[cfg(target_os = "linux")]
    if let Some(name) = Path::new(&path).file_name() {
        if let Ok(p) = std::fs::canonicalize(Path::new("/sys/class/hidraw").join(name).join("device")) {
            return p.display().to_string();
        }
    }
    path
}

impl Keyboard {
    pub fn open() -> Result<Self> {
        let api = HidApi::new()?;
        let find = |page| {
            api.device_list()
                .find(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == page)
        };
        let boards = api
            .device_list()
            .filter(|d| d.vendor_id() == VID && d.product_id() == PID && d.usage_page() == CMD_PAGE)
            .count();
        ensure!(boards <= 1, "more than one F75 Max connected; plug in only one");
        let info = find(CMD_PAGE).context("AULA F75 Max not found (wired USB only)")?;
        let fw = info.release_number();
        if !KNOWN_FIRMWARE.contains(&fw) {
            bail!("firmware {fw:#06x} not verified with this tool, refusing to write");
        }
        let dir = runtime_dir()?;
        let lock = take_lock(&dir)?;
        let instance = instance_id(info);
        check_interrupted(&dir, &instance)?;
        let cmd = info
            .open_device(&api)
            .context("cannot open command interface (permissions?)")?;
        let bulk = find(BULK_PAGE).and_then(|d| d.open_device(&api).ok());
        Ok(Self {
            cmd,
            bulk,
            instance,
            dir,
            _lock: lock,
        })
    }

    /// Call after a write failed mid-transaction: every process refuses this board until it is
    /// replugged, so a later `04 02` can't commit the half-written state.
    pub fn mark_interrupted(&self) {
        if let Err(e) = set_marker(&self.dir, &self.instance) {
            eprintln!("aula75: could not record the interrupted write: {e:#}");
        }
    }

    /// Write-ahead guard for stored writes: the marker exists for the whole transaction, so a
    /// failure, panic, kill or closed window all leave it behind, and every process refuses
    /// this board until it is replugged. Without a marker on disk, nothing is written.
    fn guarded(&self, op: impl FnOnce() -> Result<()>) -> Result<()> {
        set_marker(&self.dir, &self.instance).context("cannot record the write in progress; refusing to write")?;
        op()?;
        std::fs::remove_file(self.dir.join(MARKER)).context("write applied, but its safety marker could not be cleared")
    }

    fn send(&self, p: &[u8; 64]) -> Result<()> {
        self.send_wait(p, DELAY)
    }

    fn send_wait(&self, p: &[u8; 64], wait: Duration) -> Result<()> {
        let mut r = [0u8; 65];
        r[1..].copy_from_slice(p);
        self.cmd.send_feature_report(&r)?;
        sleep(wait);
        Ok(())
    }

    fn reply(&self, cmd: Cmd) -> Result<[u8; 64]> {
        let mut r = [0u8; 65];
        let n = self.cmd.get_feature_report(&mut r)?;
        ensure!(n == 65, "{cmd:?}: short reply ({n} bytes)");
        Ok(r[1..].try_into().unwrap())
    }

    /// set_feature + get_feature + ack check, like the driver's FUN_0044eeb0 with readback on.
    fn step(&self, cmd: Cmd, p: &[u8; 64]) -> Result<()> {
        self.send(p)?;
        check_ack(cmd, &self.reply(cmd)?)
    }

    /// One live frame, mirroring FUN_004329c0 / FUN_0044f0a0:
    /// header `04 20 08` must be acked with status 01 (one resend on FF) or the frame is skipped
    /// with nothing else sent; then 8 chunks 2 ms apart within 100 ms; then `04 02`, whose busy
    /// reply the driver ignores, so it only marks the frame as not shown.
    /// I/O errors and malformed replies are real errors.
    pub fn stream(&self, f: &RtFrame) -> Result<Sent> {
        if self.fast_step(Cmd::Realtime, &Cmd::Realtime.packet(8))?[3] != 0x01 {
            return Ok(Sent::Busy);
        }
        let t0 = Instant::now();
        for c in f.0.chunks(64) {
            self.send_wait(c.try_into().unwrap(), Duration::from_millis(2))?;
            ensure!(
                t0.elapsed() <= Duration::from_millis(100),
                "real-time frame too slow, aborted"
            );
        }
        Ok(if self.fast_step(Cmd::Apply, &Cmd::Apply.packet(0))?[3] == 0xff {
            Sent::Busy
        } else {
            Sent::Shown
        })
    }

    /// Reads what the LEDs show right now, the way the driver's Lighting page mirrors the board
    /// (FUN_0042aeb0 polling FUN_0044f380): `04 F5 09` (5 ms, ack status 01), then 9 feature-report
    /// reads 3 ms apart (each within 360 ms) carrying 64 bytes each, then `04 02` like the driver.
    /// Writes nothing. The driver first tries a 9-byte read that always fails on Windows before
    /// any USB traffic; it is skipped here since on other OSes it could consume the reply.
    /// `Ok(None)`: the keyboard declined (busy); like the driver, nothing else is sent.
    pub fn read_leds(&self) -> Result<Option<LedColors>> {
        self.send_wait(&Cmd::ReadLeds.packet(9), Duration::from_millis(5))?;
        let r = self.reply(Cmd::ReadLeds)?;
        ensure!(
            r[0] == 0x04 && r[1] == Cmd::ReadLeds as u8,
            "LED read: unexpected reply {:02x?}",
            &r[..4]
        );
        if r[3] != 0x01 {
            return Ok(None);
        }
        let mut d = [0u8; 576];
        for chunk in d.chunks_mut(64) {
            sleep(Duration::from_millis(3));
            let mut buf = [0u8; 65];
            let t0 = Instant::now();
            let n = self.cmd.get_feature_report(&mut buf)?;
            ensure!(n == 65, "LED read: short chunk ({n} bytes)");
            ensure!(t0.elapsed() <= Duration::from_millis(360), "LED read too slow");
            chunk.copy_from_slice(&buf[1..]);
        }
        self.command(Cmd::Apply, 0)?;
        Ok(Some(LedColors(d)))
    }

    /// set, 5 ms, get; on a busy (FF) reply resend once, like FUN_0044f0a0. Returns the last reply.
    fn fast_step(&self, cmd: Cmd, p: &[u8; 64]) -> Result<[u8; 64]> {
        let mut r = [0; 64];
        for _ in 0..2 {
            self.send_wait(p, Duration::from_millis(5))?;
            r = self.reply(cmd)?;
            ensure!(
                r[0] == 0x04 && r[1] == cmd as u8,
                "{cmd:?}: unexpected reply {:02x?}",
                &r[..4]
            );
            if r[3] != 0xff {
                break;
            }
        }
        Ok(r)
    }

    fn command(&self, cmd: Cmd, arg: u8) -> Result<()> {
        self.step(cmd, &cmd.packet(arg))
    }

    fn data(&self, d: &[u8]) -> Result<()> {
        for c in d.chunks(64) {
            self.send(c.try_into().context("data not 64-byte aligned")?)?;
        }
        Ok(())
    }

    fn write_light(&self, packet: &[u8; 64]) -> Result<()> {
        self.command(Cmd::Begin, 0)?;
        self.command(Cmd::Light, 1)?;
        self.send(packet)?;
        self.command(Cmd::Apply, 0)?;
        self.send(&Cmd::Finish.packet(0))
    }

    pub fn set_light(&self, l: &Light) -> Result<()> {
        self.guarded(|| self.write_light(&light_packet(l)))
    }

    pub fn set_per_key(&self, keys: &[(LedIndex, [u8; 3])], brightness: Level) -> Result<()> {
        self.guarded(|| {
            self.write_light(&light_raw(
                CUSTOM_MODE_ID,
                [0; 3],
                false,
                brightness,
                Level(0),
                Direction::Left,
            ))?;
            self.command(Cmd::Begin, 0)?;
            self.command(Cmd::PerKey, 9)?;
            self.data(&per_key_packet(keys))?;
            self.command(Cmd::Apply, 0)?;
            self.send(&Cmd::Finish.packet(0))
        })
    }

    /// Clock on the TFT, from the local time of this machine.
    pub fn sync_clock(&self) -> Result<()> {
        self.guarded(|| {
            let t = local_time()?;
            let mut p = [0; 64];
            p[1] = 1;
            p[2] = 0x5a;
            p[3] = (t.tm_year % 100) as u8;
            p[4] = (t.tm_mon + 1) as u8;
            p[5] = t.tm_mday as u8;
            p[6] = t.tm_hour as u8;
            p[7] = t.tm_min as u8;
            p[8] = t.tm_sec.min(59) as u8;
            p[10] = t.tm_wday as u8;
            magic(&mut p);
            self.command(Cmd::Begin, 0)?;
            self.command(Cmd::Clock, 1)?;
            self.send(&p)?;
            // The driver reads back after the clock data packet too (FUN_004238f0); the reply
            // has no `04 xx` header, so only its length is checked.
            self.reply(Cmd::Clock)?;
            self.command(Cmd::Apply, 0)
        })
    }

    /// Upload to the single screen slot (`gif_count="1"`). `progress` gets 0..=100.
    pub fn upload_screen(&self, a: &Animation, mut progress: impl FnMut(u32)) -> Result<()> {
        // Checked before the guard: refusing here sends nothing, so it must not leave a marker.
        let Some(bulk) = &self.bulk else {
            bail!("screen interface (0xFF68) not available")
        };
        self.guarded(|| {
            let blob = screen_blob(a);
            let pages = (blob.len() / PAGE) as u16;
            let mut p = Cmd::Screen.packet(0);
            p[2] = 1;
            p[8..10].copy_from_slice(&pages.to_le_bytes());
            self.command(Cmd::Begin, 0)?;
            self.step(Cmd::Screen, &p)?;
            let mut buf = vec![0u8; PAGE + 1];
            for (i, page) in blob.chunks(PAGE).enumerate() {
                buf[0] = 0;
                buf[1..].copy_from_slice(page);
                let n = bulk.write(&buf)?;
                ensure!(n == buf.len(), "screen page {i}: short write ({n})");
                // The driver waits up to 300 ms for an ack and ignores its content and a timeout
                // (FUN_0044f2b0); an I/O error, however, aborts before Apply.
                bulk.read_timeout(&mut buf, 300)?;
                progress((i as u32 + 1) * 100 / pages as u32);
            }
            self.command(Cmd::Apply, 0)
        })
    }
}

pub fn local_time() -> Result<libc::tm> {
    // SAFETY: time(NULL) and localtime_r with valid out-pointer are always sound.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        ensure!(!libc::localtime_r(&now, &mut tm).is_null(), "localtime failed");
        Ok(tm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_lock_and_write_ahead_marker() {
        let dir = std::env::temp_dir().join(format!("aula75-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let held = take_lock(&dir).unwrap();
        assert!(take_lock(&dir).is_err(), "second holder must be refused");
        drop(held);
        assert!(take_lock(&dir).is_ok());

        assert!(check_interrupted(&dir, "plug-1").is_ok(), "no marker passes");
        set_marker(&dir, "plug-1").unwrap();
        assert!(check_interrupted(&dir, "plug-1").is_err(), "same plug-in stays locked");
        assert!(check_interrupted(&dir, "plug-2").is_ok(), "replug clears it");
        assert!(!dir.join(MARKER).exists());

        // Anything but "not found" fails safe.
        std::os::unix::fs::symlink("/etc/hosts", dir.join(MARKER)).unwrap();
        assert!(check_interrupted(&dir, "plug-1").is_err(), "symlinked marker refuses");
        std::fs::remove_file(dir.join(MARKER)).unwrap();
        std::fs::create_dir(dir.join(MARKER)).unwrap();
        assert!(check_interrupted(&dir, "plug-1").is_err(), "unreadable marker refuses");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn packets() {
        let mode = Mode::all().find(|m| m.name() == "Static").unwrap();
        let l = Light {
            mode,
            rgb: [1, 2, 3],
            rainbow: true,
            brightness: Level::new(9),
            speed: Level::new(3),
            direction: Direction::Up,
        };
        assert_eq!(
            &light_packet(&l)[..16],
            &[1, 1, 2, 3, 0, 0, 0, 0, 1, 5, 3, 2, 0, 0, 0xaa, 0x55]
        );
        assert_eq!(light_packet(&Light { mode: Mode(0), ..l })[..4], [0; 4]);

        let leds: Vec<_> = (0..KEYS.len())
            .map(|k| (LedIndex::of_key(k).unwrap(), [7, 7, 7]))
            .collect();
        let d = per_key_packet(&leds);
        assert_eq!(&d[4..8], &[1, 7, 7, 7]); // Esc
        assert_eq!(&d[574..], &[0xaa, 0x55]);
        assert!(LedIndex::of_key(KEYS.len()).is_none());

        assert_eq!(rgb565(&[255, 0, 0, 255, 0, 0, 255, 255]), [0x00, 0xf8, 0x1f, 0x00]);

        let px = vec![0u8; SCREEN * SCREEN * 4];
        assert!(Animation::from_rgba(&[]).is_err());
        assert!(Animation::from_rgba(&[(vec![0; 10], 100)]).is_err());
        let a = Animation::from_rgba(&[(px.clone(), 100), (px, 0)]).unwrap();
        let b = screen_blob(&a);
        assert_eq!(b.len(), 17 * PAGE); // ceil((256 + 2*32768) / 4096)
        assert_eq!(&b[..4], &[2, 50, 15, 0xff]);
        assert_eq!(*b.last().unwrap(), 0xff);

        let rt = RtFrame::new(&[
            (LedIndex::of_key(5).unwrap(), [1, 2, 3]),
            (LedIndex::of_key(0).unwrap(), [4, 5, 6]),
        ]);
        assert_eq!(&rt.0[..8], &[KEYS[5].1, 1, 2, 3, 1, 4, 5, 6]); // packed, not at led*4
        assert!(rt.0[8..].iter().all(|&b| b == 0));
        assert_eq!(Cmd::Realtime.packet(8)[..9], [4, 0x20, 0, 0, 0, 0, 0, 0, 8]);

        assert_eq!(Cmd::ReadLeds.packet(9)[..9], [4, 0xf5, 0, 0, 0, 0, 0, 0, 9]);
        let esc = LedIndex::of_key(0).unwrap(); // Esc = LED 1
        let mut raw = [0u8; 576];
        raw[4..8].copy_from_slice(&[1, 10, 20, 30]);
        assert_eq!(LedColors(raw).get(esc), [10, 20, 30]);

        assert!(
            check_ack(
                Cmd::Begin,
                &[4, 0x18, 0, 1]
                    .iter()
                    .chain(&[0; 60])
                    .copied()
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap()
            )
            .is_ok()
        );
        assert!(
            check_ack(
                Cmd::Begin,
                &[4, 0x13, 0, 1]
                    .iter()
                    .chain(&[0; 60])
                    .copied()
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap()
            )
            .is_err()
        );
        assert!(
            check_ack(
                Cmd::Apply,
                &[4, 0x02, 0, 0xff]
                    .iter()
                    .chain(&[0; 60])
                    .copied()
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap()
            )
            .is_err()
        );
    }
}
