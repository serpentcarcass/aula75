//! Host-side live effects. Each produces one colour per entry of `KEYS`, streamed via `04 20`.

use crate::layout::KEYS;
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
    fn key_positions_normalised() {
        assert!(
            key_pos()
                .iter()
                .all(|&(x, y)| (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y))
        );
    }
}
