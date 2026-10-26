//! Host-side live effects. Each produces one colour per entry of `KEYS`, streamed via `04 20`.

use crate::layout::KEYS;
use crate::vendor_tables as vt;
use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::collections::VecDeque;
use std::f32::consts::{PI, TAU};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub type Rgb = [u8; 3];

pub trait Effect {
    fn frame(&mut self) -> Vec<Rgb>;
    /// Short text for the UI (e.g. remaining time).
    fn label(&self) -> String {
        String::new()
    }
    /// The colours are already final (skip the colour curve), e.g. the calibration pattern.
    fn raw(&self) -> bool {
        false
    }
}

/// Curves shown on the six matrix rows during calibration (F row first).
pub const CALIBRATION: [f32; 6] = [1.0, 1.6, 2.2, 2.8, 3.4, 4.0];
/// Reference mixes: orange on the left half, purple on the right.
pub const CAL_COLOURS: [Rgb; 2] = [[255, 128, 0], [128, 0, 255]];

/// Each matrix row shows the reference mixes through a different curve; the user picks the row
/// that looks most like the colours on screen. Nothing is stored on the keyboard.
pub struct Calibration;

impl Effect for Calibration {
    fn frame(&mut self) -> Vec<Rgb> {
        vt::KEY_MATRIX
            .iter()
            .map(|&(_, row, col)| curve(CAL_COLOURS[(col > 7) as usize], CALIBRATION[(row as usize).min(5)]))
            .collect()
    }
    fn raw(&self) -> bool {
        true
    }
}

/// Key centres normalised to 0..1 (x left→right, y top→bottom).
pub fn key_pos() -> Vec<(f32, f32)> {
    let (x0, y0, x1, y1) = crate::layout::bounds();
    KEYS.iter()
        .map(|k| ((k.2 + k.4 / 2.0 - x0) / (x1 - x0), (k.3 + k.5 / 2.0 - y0) / (y1 - y0)))
        .collect()
}

fn hsv(h: f32, s: f32, v: f32) -> Rgb {
    eframe::egui::ecolor::rgb_from_hsv((h, s, v)).map(|c| (c * 255.0) as u8)
}

/// Colour curve: these LEDs are very non-linear, so even small amounts of a second channel wash
/// a colour out towards white (pure primaries look right). Raising each channel to `gamma` before
/// sending keeps mixes saturated; 1.0 = unchanged. `uncurve` undoes it for colours read back.
pub fn curve(c: Rgb, gamma: f32) -> Rgb {
    if gamma <= 1.0 {
        return c;
    }
    c.map(|x| ((x as f32 / 255.0).powf(gamma) * 255.0).round() as u8)
}

pub fn uncurve(c: Rgb, gamma: f32) -> Rgb {
    if gamma <= 1.0 {
        return c;
    }
    c.map(|x| ((x as f32 / 255.0).powf(1.0 / gamma) * 255.0).round() as u8)
}

pub const CURVE: std::ops::RangeInclusive<f32> = 1.0..=4.0;

fn scale([r, g, b]: Rgb, k: f32) -> Rgb {
    let k = k.clamp(0.0, 1.0);
    [(r as f32 * k) as u8, (g as f32 * k) as u8, (b as f32 * k) as u8]
}

// ---------------------------------------------------------------- music

pub fn input_devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|d| d.map(|d| d.to_string()).collect())
        .unwrap_or_default()
}

const FFT_LEN: usize = 2048;

/// Spectrum bars: one band per keyboard column, bar height across the rows.
pub struct Music {
    _stream: cpal::Stream,
    ring: Arc<Mutex<VecDeque<f32>>>,
    rate: f32,
    pos: Vec<(f32, f32)>,
    bands: Vec<f32>,
    peak: f32,
    pub sensitivity: f32,
}

impl Music {
    pub const BANDS: usize = 15;

    /// `device: None` = what the system is playing: recording from the default *output* device,
    /// which cpal turns into a loopback capture (Core Audio process tap on macOS 14.6+,
    /// sink capture on PipeWire). `Some(name)` = that input device (microphone, BlackHole, ...).
    pub fn new(device: Option<&str>, sensitivity: f32) -> Result<Self> {
        let host = cpal::default_host();
        let (dev, cfg) = match device {
            Some(name) => {
                let d = host
                    .input_devices()?
                    .find(|d| d.to_string() == name)
                    .context("audio device not found")?;
                let c = d.default_input_config()?;
                (d, c)
            }
            None => {
                let d: cpal::Device = host.default_output_device().context("no default audio output")?;
                let c = if d.supports_input() {
                    d.default_input_config()?
                } else {
                    d.default_output_config()?
                };
                (d, c)
            }
        };
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(FFT_LEN * 2)));
        let ch = cfg.channels() as usize;
        anyhow::ensure!(ch > 0, "audio device reports 0 channels");
        let stream = match cfg.sample_format() {
            cpal::SampleFormat::F32 => build::<f32>(&dev, &cfg.config(), ch, ring.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&dev, &cfg.config(), ch, ring.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&dev, &cfg.config(), ch, ring.clone()),
            cpal::SampleFormat::I32 => build::<i32>(&dev, &cfg.config(), ch, ring.clone()),
            f => bail!("unsupported sample format {f:?}"),
        }?;
        stream.play()?;
        Ok(Self {
            _stream: stream,
            ring,
            rate: cfg.sample_rate() as f32,
            pos: key_pos(),
            bands: vec![0.0; Self::BANDS],
            peak: 1e-3,
            sensitivity,
        })
    }

    /// Band levels 0..1, log-spaced 40 Hz .. 14 kHz.
    fn analyse(&mut self) {
        let buf: Vec<f32> = {
            let r = self.ring.lock().unwrap();
            r.iter().copied().collect()
        };
        if buf.len() < FFT_LEN {
            return;
        }
        let n = FFT_LEN as f32;
        let win: Vec<f32> = buf
            .iter()
            .enumerate()
            .map(|(i, s)| s * (0.5 - 0.5 * (TAU * i as f32 / n).cos()))
            .collect();
        let (lo, hi) = (40f32, 14_000f32.min(self.rate / 2.0));
        let mut raw = [0f32; Self::BANDS];
        for (b, v) in raw.iter_mut().enumerate() {
            let f0 = lo * (hi / lo).powf(b as f32 / Self::BANDS as f32);
            let f1 = lo * (hi / lo).powf((b + 1) as f32 / Self::BANDS as f32);
            // Power summed over every DFT bin in the band, so no frequency falls between probes.
            let bin = self.rate / n;
            let (k0, k1) = (
                (f0 / bin).round() as usize,
                ((f1 / bin).round() as usize).max((f0 / bin).round() as usize + 1),
            );
            *v = (k0..k1)
                .map(|k| goertzel(&win, k as f32 * bin, self.rate).powi(2))
                .sum::<f32>()
                .sqrt();
        }
        let loudest = raw.iter().copied().fold(0.0, f32::max);
        self.peak = (self.peak * 0.995).max(loudest).max(1e-3); // slow auto-gain
        for (b, v) in raw.iter().enumerate() {
            let level = ((v / self.peak).powf(0.6) * self.sensitivity).min(1.0);
            let old = self.bands[b];
            self.bands[b] = if level > old { level } else { old * 0.82 + level * 0.18 }; // fast attack, slow fall
        }
    }
}

fn goertzel(x: &[f32], freq: f32, rate: f32) -> f32 {
    let w = 2.0 * PI * freq / rate;
    let c = 2.0 * w.cos();
    let (mut s1, mut s2) = (0f32, 0f32);
    for &v in x {
        let s0 = v + c * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - c * s1 * s2).max(0.0).sqrt() / x.len() as f32
}

fn build<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    ch: usize,
    ring: Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    Ok(dev.build_input_stream::<T, _, _>(
        *cfg,
        move |data: &[T], _| {
            // Real-time audio thread: never block. If the analyser holds the lock, drop this buffer.
            let Ok(mut r) = ring.try_lock() else { return };
            for frame in data.chunks(ch) {
                if r.len() == FFT_LEN {
                    r.pop_front();
                }
                r.push_back(frame.iter().map(|&s| s.to_sample::<f32>()).sum::<f32>() / ch as f32);
            }
        },
        |e| eprintln!("audio: {e}"),
        None,
    )?)
}

impl Effect for Music {
    fn frame(&mut self) -> Vec<Rgb> {
        self.analyse();
        self.pos
            .iter()
            .map(|&(x, y)| {
                let band = ((x * Self::BANDS as f32) as usize).min(Self::BANDS - 1);
                let h = 1.0 - y; // bottom row = 0, F-row = 1
                let level = self.bands[band];
                let color = hsv(0.33 - 0.33 * h, 1.0, 1.0); // green → yellow → red with height
                if level >= h * 0.92 + 0.04 {
                    color
                } else {
                    scale(hsv(0.6 + x * 0.25, 0.9, 1.0), 0.06) // dim blue-violet bed
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------- pomodoro

pub struct Pomodoro {
    work: Duration,
    rest: Duration,
    started: Instant,
    working: bool,
    fkeys: Vec<usize>, // F1..F12 in left-to-right order
}

const WORK: Rgb = [255, 70, 20];
const REST: Rgb = [30, 230, 120];
const FLASH: Duration = Duration::from_secs(4);

impl Pomodoro {
    pub fn new(work_min: u32, rest_min: u32) -> Self {
        let mut fkeys: Vec<usize> = (0..KEYS.len())
            .filter(|&i| KEYS[i].0.len() > 1 && KEYS[i].0.starts_with('F') && KEYS[i].0[1..].parse::<u8>().is_ok())
            .collect();
        fkeys.sort_by(|&a, &b| KEYS[a].2.total_cmp(&KEYS[b].2));
        Self {
            work: Duration::from_secs(work_min.max(1) as u64 * 60),
            rest: Duration::from_secs(rest_min.max(1) as u64 * 60),
            started: Instant::now(),
            working: true,
            fkeys,
        }
    }

    fn phase(&self) -> (Duration, Rgb) {
        if self.working {
            (self.work, WORK)
        } else {
            (self.rest, REST)
        }
    }

    fn tick(&mut self) -> f32 {
        let (len, _) = self.phase();
        if self.started.elapsed() >= len {
            self.working = !self.working;
            self.started = Instant::now();
        }
        self.started.elapsed().as_secs_f32() / self.phase().0.as_secs_f32()
    }
}

impl Effect for Pomodoro {
    fn frame(&mut self) -> Vec<Rgb> {
        let done = self.tick();
        let (_, color) = self.phase();
        let since = self.started.elapsed();
        let mut out = vec![scale(color, 0.05); KEYS.len()];
        if since < FLASH {
            // Phase change: whole board pulses.
            let k = 0.5 + 0.5 * (since.as_secs_f32() * TAU * 1.5).cos();
            return out.iter().map(|_| scale(color, 0.15 + 0.85 * k)).collect();
        }
        let n = self.fkeys.len() as f32;
        let remaining = (1.0 - done) * n; // e.g. 7.4 of 12 segments left
        for (s, &key) in self.fkeys.iter().enumerate() {
            let fill = (remaining - s as f32).clamp(0.0, 1.0);
            out[key] = scale(color, 0.08 + 0.92 * fill);
        }
        out
    }

    fn label(&self) -> String {
        let (len, _) = self.phase();
        let left = len.saturating_sub(self.started.elapsed()).as_secs();
        format!(
            "{} · {:02}:{:02} left",
            if self.working { "Focus" } else { "Break" },
            left / 60,
            left % 60
        )
    }
}

// ---------------------------------------------------------------- vendor effects

/// The driver's own real-time effects (mui.dll `CustomLightMode::Update*Effect`), applied to the
/// whole board. Step logic, tables, key groups and the `rgb * alpha >> 8` output (FUN_00427dc0)
/// follow the driver; initial phases and the 10 ms base tick are approximations.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum VendorFx {
    Star,
    Rain,
    Flower,
    Breath,
    Spectrum,
    Spring,
    Vertical,
    Roll,
    Rotate,
}

impl VendorFx {
    pub const ALL: [VendorFx; 9] = [
        VendorFx::Star,
        VendorFx::Rain,
        VendorFx::Flower,
        VendorFx::Breath,
        VendorFx::Spectrum,
        VendorFx::Spring,
        VendorFx::Vertical,
        VendorFx::Roll,
        VendorFx::Rotate,
    ];

    /// Case-insensitive lookup by `name()` (CLI and saved settings).
    pub fn from_name(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.name().eq_ignore_ascii_case(s))
    }

    pub fn name(self) -> &'static str {
        match self {
            VendorFx::Star => "Star",
            VendorFx::Rain => "Rain",
            VendorFx::Flower => "Flower",
            VendorFx::Breath => "Breath",
            VendorFx::Spectrum => "Spectrum",
            VendorFx::Spring => "Spring",
            VendorFx::Vertical => "Vertical",
            VendorFx::Roll => "Roll",
            VendorFx::Rotate => "Rotate",
        }
    }
}

/// Vendor speed scale; the driver updates once every `15 - speed` timer ticks (SetSpeed).
pub const VENDOR_SPEED: std::ops::RangeInclusive<u8> = 1..=15;
const TICK: Duration = Duration::from_millis(10);

pub struct Vendor {
    fx: VendorFx,
    period: Duration,
    last: Instant,
    groups: Vec<Vec<usize>>, // key indices per row / column / ring / sector, as the effect needs
    phase: Vec<usize>,       // per key (Star, Flower) or per group (waves)
    bright: Vec<usize>,      // per group brightness index into BRIGHT
    rgb: Vec<Rgb>,           // per key colour before alpha
    alpha: Vec<u8>,          // per key alpha
    mask: Vec<u32>,          // Rain: per column drop bits
    counter: i32,            // palette walk (0xf5c)
    t: usize,                // Breath / Spectrum position
}

impl Vendor {
    pub fn new(fx: VendorFx, speed: u8) -> Self {
        // div 0 in the driver also means "every tick", hence max(1).
        let div = (15 - speed.clamp(*VENDOR_SPEED.start(), *VENDOR_SPEED.end()) as u32).max(1);
        let groups = match fx {
            VendorFx::Rain | VendorFx::Roll | VendorFx::Breath => by_matrix(|_, c| c),
            VendorFx::Vertical => by_matrix(|r, _| r),
            VendorFx::Spring => by_hid(&vt::SPRING_RINGS, 12),
            VendorFx::Rotate => by_hid(&vt::ROTATE_SECTORS, 3),
            _ => vec![],
        };
        let n = KEYS.len();
        let g = groups.len().max(1);
        // Approximation: the driver's initial phases weren't reversed; spread them evenly.
        let phase = match fx {
            VendorFx::Star => (0..n).map(|k| k * 97 % 256).collect(),
            VendorFx::Flower => (0..n).map(|k| k * 53 % 192).collect(),
            _ => (0..g).map(|i| i * 192 / g).collect(),
        };
        Self {
            fx,
            period: TICK * div,
            last: Instant::now(),
            bright: (0..g).map(|i| i * 96 / g).collect(),
            rgb: vec![[0; 3]; n],
            alpha: vec![255; n],
            mask: (0..g).map(|i| 1 << (i * 3 % 7)).collect(),
            groups,
            phase,
            counter: 0,
            t: 0,
        }
    }

    fn step(&mut self) {
        let sin = |i: usize| vt::SIN[i % 192];
        let wave = |p: usize| [sin(p), sin(p + 64), sin(p + 128)];
        match self.fx {
            VendorFx::Star => {
                for k in 0..self.rgb.len() {
                    self.alpha[k] = self.phase[k] as u8;
                    if self.phase[k] == 0 {
                        self.phase[k] = 255;
                        let i = next_palette(&mut self.counter) * 3;
                        self.rgb[k] = [vt::STAR_PALETTE[i], vt::STAR_PALETTE[i + 1], vt::STAR_PALETTE[i + 2]];
                    } else {
                        self.phase[k] -= 1;
                    }
                }
            }
            VendorFx::Rain => {
                for col in 0..self.groups.len() {
                    self.mask[col] <<= 1;
                    if self.mask[col] & 0x80 != 0 {
                        self.mask[col] = 1;
                    }
                    for &k in &self.groups[col] {
                        if self.mask[col] & (1 << vt::KEY_MATRIX[k].1) != 0 {
                            let i = next_palette(&mut self.counter) * 3;
                            self.rgb[k] = [vt::RAIN_PALETTE[i], vt::RAIN_PALETTE[i + 1], vt::RAIN_PALETTE[i + 2]];
                        }
                    }
                }
            }
            VendorFx::Flower => {
                for k in 0..self.rgb.len() {
                    self.phase[k] = (self.phase[k] + 1) % 192;
                    self.rgb[k] = wave(self.phase[k]);
                }
            }
            VendorFx::Breath => {
                let a = vt::BREATH[self.t];
                self.t = (self.t + 1) % 128;
                for (col, keys) in self.groups.iter().enumerate() {
                    let c = &vt::BREATH_COLUMNS[(col % 15) * 3..];
                    for &k in keys {
                        self.rgb[k] = [c[1], c[0], c[2]]; // stored G, R, B
                        self.alpha[k] = a;
                    }
                }
            }
            VendorFx::Spectrum => {
                self.t = (self.t + 1) % 192;
                let c = [
                    vt::SPEC[self.t],
                    vt::SPEC[(self.t + 64) % 192],
                    vt::SPEC[(self.t + 128) % 192],
                ];
                self.rgb.fill(c);
            }
            VendorFx::Spring | VendorFx::Rotate | VendorFx::Vertical | VendorFx::Roll => {
                // Spring/Rotate run their waves forwards, Vertical/Roll backwards.
                let fwd = matches!(self.fx, VendorFx::Spring | VendorFx::Rotate);
                for (g, keys) in self.groups.iter().enumerate() {
                    self.phase[g] = if fwd {
                        (self.phase[g] + 1) % 192
                    } else {
                        (self.phase[g] + 191) % 192
                    };
                    self.bright[g] = if fwd {
                        (self.bright[g] + 1) % 96
                    } else {
                        (self.bright[g] + 95) % 96
                    };
                    for &k in keys {
                        self.rgb[k] = wave(self.phase[g]);
                        self.alpha[k] = vt::BRIGHT[self.bright[g]];
                    }
                }
            }
        }
    }
}

impl Effect for Vendor {
    fn frame(&mut self) -> Vec<Rgb> {
        // Catch up on the driver's update rate, but never spin after a long stall.
        let due = (self.last.elapsed().as_micros() / self.period.as_micros().max(1)).min(20) as u32;
        for _ in 0..due {
            self.step();
        }
        // After a long stall drop the backlog instead of fast-forwarding through it.
        self.last = if due == 20 {
            Instant::now()
        } else {
            self.last + self.period * due
        };
        self.rgb
            .iter()
            .zip(&self.alpha)
            .map(|(c, &a)| c.map(|x| ((x as u32 * a as u32) >> 8) as u8))
            .collect()
    }
}

/// The driver's pseudo-random palette walk: srand(0xe8) before every rand() makes rand()
/// always 796, so it's just a counter over the 107-colour palette.
fn next_palette(counter: &mut i32) -> usize {
    *counter += 1;
    if *counter > 100 {
        *counter -= 100;
    }
    let i = (796 + *counter as usize) & 0x7f;
    if i < 0x6b { i } else { i - 0x6a }
}

/// Key indices grouped by matrix row or column (vendor `row_col`).
fn by_matrix(key: impl Fn(usize, usize) -> usize) -> Vec<Vec<usize>> {
    let mut g: Vec<Vec<usize>> = vec![];
    for (k, &(_, r, c)) in vt::KEY_MATRIX.iter().enumerate() {
        let i = key(r as usize, c as usize);
        if g.len() <= i {
            g.resize(i + 1, vec![]);
        }
        g[i].push(k);
    }
    g
}

/// Key indices for the driver's HID-code groups (`width` codes per group, 0 = empty slot).
fn by_hid(table: &[u8], width: usize) -> Vec<Vec<usize>> {
    table
        .chunks(width)
        .map(|codes| {
            codes
                .iter()
                .filter(|&&c| c != 0)
                .filter_map(|&c| vt::KEY_MATRIX.iter().position(|&(h, _, _)| h == c))
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goertzel_finds_tone() {
        let rate = 48_000.0;
        let x: Vec<f32> = (0..FFT_LEN).map(|i| (TAU * 1000.0 * i as f32 / rate).sin()).collect();
        assert!(goertzel(&x, 1000.0, rate) > 10.0 * goertzel(&x, 4000.0, rate));
    }

    #[test]
    fn pomodoro_bar() {
        let mut p = Pomodoro::new(25, 5);
        assert_eq!(p.fkeys.len(), 12);
        assert_eq!(KEYS[p.fkeys[0]].0, "F1");
        p.started -= FLASH + Duration::from_secs(60 * 25 / 2); // halfway through focus
        let f = p.frame();
        assert_eq!(f[p.fkeys[0]], WORK); // F1 still full
        assert!(f[p.fkeys[11]][0] < 40); // F12 burnt down
        assert!(p.label().starts_with("Focus"));
    }

    /// Manual: `cargo test -- --ignored system_audio --nocapture` while something plays.
    #[test]
    #[ignore]
    fn system_audio_loopback() {
        let mut m = Music::new(None, 1.0).expect("open system audio");
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            m.analyse();
            let (n, rms) = {
                let r = m.ring.lock().unwrap();
                (
                    r.len(),
                    (r.iter().map(|x| x * x).sum::<f32>() / r.len().max(1) as f32).sqrt(),
                )
            };
            print!("samples {n:4} rms {rms:.5} | ");
            println!(
                "{}",
                m.bands
                    .iter()
                    .map(|b| format!("{:3.0}", b * 100.0))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }

    #[test]
    fn vendor_effects() {
        assert_eq!(vt::KEY_MATRIX.len(), KEYS.len());
        for fx in VendorFx::ALL {
            let mut v = Vendor::new(fx, 15);
            v.last -= Duration::from_millis(500); // 50 steps due
            let f = v.frame();
            assert_eq!(f.len(), KEYS.len(), "{fx:?}");
            assert!(f.iter().any(|c| c.iter().any(|&x| x > 0)), "{fx:?} lights something");
        }
        // Key groups: every key belongs to some row/column; Spring rings cover most of the board.
        assert_eq!(by_matrix(|r, _| r).iter().map(Vec::len).sum::<usize>(), KEYS.len());
        let ringed: usize = by_hid(&vt::SPRING_RINGS, 12).iter().map(Vec::len).sum();
        assert!(ringed >= 70, "spring covers {ringed} keys");
        // The palette walk stays inside the 107-colour palette.
        let mut c = 0;
        assert!((0..500).all(|_| next_palette(&mut c) < 107));
        assert_eq!(VendorFx::from_name("spring"), Some(VendorFx::Spring));
    }

    #[test]
    fn calibration_rows() {
        let f = Calibration.frame();
        let esc = f[0]; // row 0: curve 1.0, left half
        let space = f[KEYS.iter().position(|k| k.0 == "Space").unwrap()]; // row 5: curve 4.0
        assert_eq!(esc, CAL_COLOURS[0]);
        assert!(space[1] < esc[1], "steeper curve thins the green in orange");
    }

    #[test]
    fn colour_curve() {
        assert_eq!(curve([255, 45, 45], 1.0), [255, 45, 45], "1.0 is off");
        assert_eq!(curve([255, 45, 45], 3.0)[1], 1, "a small mix almost vanishes");
        assert_eq!(curve([255, 0, 0], 3.0), [255, 0, 0], "primaries stay pure");
        for x in [64u8, 128, 200, 255] {
            assert!(
                uncurve(curve([x; 3], 2.5), 2.5)[0].abs_diff(x) <= 4,
                "round trip for {x}"
            );
        }
    }

    #[test]
    fn key_positions_normalised() {
        assert!(
            key_pos()
                .iter()
                .all(|&(x, y)| (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y))
        );
    }
}
