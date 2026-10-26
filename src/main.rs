mod cli;
mod effects;
mod layout;
mod proto;

use anyhow::{Context, Result};
use effects::{Effect, Rgb};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, Rect, RichText, Sense, Shadow, Stroke,
    Vec2,
};
use image::{AnimationDecoder, ImageFormat, codecs::gif::GifDecoder, imageops};
use layout::KEYS;
use proto::{Animation, Direction, Keyboard, LedIndex, Level, Light, Mode, RtFrame, Sent};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::{
    io::Cursor,
    path::Path,
    thread,
    time::{Duration, Instant},
};

const ACCENT: Color32 = Color32::from_rgb(0xff, 0x9a, 0x1f);
const BG: Color32 = Color32::from_rgb(0x11, 0x12, 0x15);
const PANEL: Color32 = Color32::from_rgb(0x19, 0x1b, 0x1f);
const WIDGET: Color32 = Color32::from_rgb(0x26, 0x29, 0x2f);
const TEXT: Color32 = Color32::from_gray(232);
const MUTED: Color32 = Color32::from_gray(150);
const OK: Color32 = Color32::from_rgb(0x5e, 0xd3, 0x86);
const ERR: Color32 = Color32::from_rgb(0xff, 0x6b, 0x6b);
const SWATCHES: [[u8; 3]; 10] = [
    [0xff, 0x2d, 0x2d],
    [0xff, 0x7a, 0x00],
    [0xff, 0xc4, 0x00],
    [0x8d, 0xf0, 0x3c],
    [0x00, 0xe0, 0x8a],
    [0x00, 0xd4, 0xff],
    [0x2f, 0x6b, 0xff],
    [0x8b, 0x4d, 0xff],
    [0xff, 0x3d, 0xc8],
    [0xff, 0xff, 0xff],
];

enum Job {
    Light(Light),
    PerKey(Vec<(LedIndex, [u8; 3])>, Level),
    Clock,
    Screen(Animation),
    Live(LiveCfg),
    StopLive,
    /// Read the colours the keyboard shows right now into the per-key editor.
    ReadKeys,
    /// Keep polling what the keyboard shows (for the Lighting preview) while `true`.
    Mirror(bool),
}

#[derive(Clone)]
enum LiveCfg {
    Music { device: Option<String>, sensitivity: f32 },
    Pomodoro { work: u32, rest: u32 },
}

/// What the worker is currently streaming, for the on-screen preview.
#[derive(Default)]
struct LiveView {
    running: bool,
    colors: Vec<Rgb>,
    label: String,
    /// Last colours read back from the keyboard while mirroring; empty when not mirroring.
    mirror: Vec<Rgb>,
}

// ~30 fps, about what the driver's own music page manages.
const FRAME: Duration = Duration::from_millis(33);

enum Msg {
    Done(Result<&'static str, String>),
    Progress(u32),
    /// Live streaming stopped on its own; unrelated to the one-shot `busy` job.
    LiveStopped(String),
    /// Mirroring stopped after a failed read.
    MirrorStopped(String),
    /// Colours read from the keyboard, in `KEYS` order.
    Keys(Vec<Rgb>),
}

// The driver's Lighting page polls `04 F5` continuously; one read takes ~80 ms.
const MIRROR_EVERY: Duration = Duration::from_millis(100);

/// One worker owns the device, so transactions can never interleave: one-shot jobs run
/// between live frames, never inside one.
///
/// If any write fails after the device was opened, the firmware may hold a half-written
/// transaction, and the next `04 02` (from any job or live frame) could commit it. So every
/// write is refused until the keyboard has been physically replugged.
/// `vivid`: gamma-correct colours on the way to the keyboard (and undo it on the way back), so the
/// UI always works with the colours as picked.
fn spawn_worker(
    ctx: egui::Context,
    view: Arc<Mutex<LiveView>>,
    usb: Arc<Presence>,
    vivid: Arc<AtomicBool>,
) -> (mpsc::Sender<Job>, mpsc::Receiver<Msg>) {
    let (jtx, jrx) = mpsc::channel::<Job>();
    let (mtx, mrx) = mpsc::channel();
    thread::spawn(move || {
        let mut kb: Option<Keyboard> = None;
        let mut fx: Option<Box<dyn Effect>> = None;
        let mut busy_frames = 0u32; // consecutive live frames the keyboard declined
        let mut mirror = false;
        let mut next_poll = Instant::now();
        let mut mirror_misses = 0u32; // consecutive polls the keyboard declined
        let mut seen_plugs = usb.plugs.load(Relaxed);
        let leds: Vec<LedIndex> = (0..KEYS.len()).filter_map(LedIndex::of_key).collect();
        let mut next = Instant::now();
        let send = |m: Msg| {
            let _ = mtx.send(m);
            ctx.request_repaint();
        };
        let done = |r: Result<&'static str>| {
            if let Err(e) = &r {
                eprintln!("aula75: {e:#}"); // full chain in the terminal, for diagnosing
            }
            send(Msg::Done(r.map_err(|e| format!("{e:#}"))))
        };
        loop {
            let wait = if fx.is_some() {
                next.saturating_duration_since(Instant::now())
            } else if mirror {
                next_poll.saturating_duration_since(Instant::now())
            } else {
                Duration::from_secs(3600)
            };
            let job = match jrx.recv_timeout(wait) {
                Ok(j) => Some(j),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let plugs = usb.plugs.load(Relaxed);
            if plugs != seen_plugs {
                seen_plugs = plugs;
                kb = None; // replugged: the old handle is dead, reopen on next use
            }
            if let Some(Job::Mirror(on)) = job {
                mirror = on;
                if !on {
                    view.lock().unwrap().mirror.clear();
                }
                continue;
            }
            let out = |c: Rgb| if vivid.load(Relaxed) { effects::to_led(c) } else { c };
            let back = |c: Rgb| if vivid.load(Relaxed) { effects::from_led(c) } else { c };
            if let Some(job) = job {
                let res = match job {
                    Job::Live(cfg) => {
                        let made: Result<Box<dyn Effect>> = match cfg {
                            LiveCfg::Music { device, sensitivity } => {
                                effects::Music::new(device.as_deref(), sensitivity).map(|m| Box::new(m) as _)
                            }
                            LiveCfg::Pomodoro { work, rest } => Ok(Box::new(effects::Pomodoro::new(work, rest))),
                        };
                        made.map(|e| {
                            fx = Some(e);
                            busy_frames = 0;
                            "Live effect running"
                        })
                        .map_err(|e| e.context("could not start live effect"))
                    }
                    Job::StopLive => {
                        fx = None;
                        Ok("Live effect stopped")
                    }
                    Job::Light(l) => {
                        let l = Light { rgb: out(l.rgb), ..l };
                        write(&mut kb, |k| k.set_light(&l)).map(|_| "Lighting applied")
                    }
                    Job::PerKey(keys, br) => {
                        let keys: Vec<_> = keys.into_iter().map(|(l, c)| (l, out(c))).collect();
                        write(&mut kb, |k| k.set_per_key(&keys, br)).map(|_| "Per-key colours applied")
                    }
                    Job::Clock => write(&mut kb, |k| k.sync_clock()).map(|_| "Clock synced"),
                    Job::Screen(a) => write(&mut kb, |k| {
                        k.upload_screen(&a, |p| {
                            let _ = mtx.send(Msg::Progress(p));
                            ctx.request_repaint();
                        })
                    })
                    .map(|_| "Screen uploaded"),
                    Job::ReadKeys => device(&mut kb).and_then(|k| k.read_leds()).and_then(|c| {
                        let c = c.context("keyboard is busy, try again")?;
                        send(Msg::Keys(leds.iter().map(|&l| back(c.get(l))).collect()));
                        Ok("Loaded colours from the keyboard")
                    }),
                    Job::Mirror(_) => unreachable!("handled above"),
                };
                if res.as_ref().is_err_and(|e| e.is::<WriteFailed>()) {
                    // The write left its write-ahead marker: every process refuses this board until replug.
                    fx = None;
                    kb = None;
                    view.lock().unwrap().mirror.clear(); // no stale "live" preview while locked
                }
                view.lock().unwrap().running = fx.is_some();
                done(res);
                continue;
            }

            if fx.is_none() && mirror && Instant::now() >= next_poll {
                next_poll = Instant::now() + MIRROR_EVERY;
                // Read-only, like the driver's own polling: a failure stops mirroring but doesn't lock.
                match device(&mut kb).and_then(|k| k.read_leds()) {
                    Ok(Some(c)) => {
                        mirror_misses = 0;
                        view.lock().unwrap().mirror = leds.iter().map(|&l| back(c.get(l))).collect();
                    }
                    // Busy (e.g. right after an Apply): the driver just polls again; give up after ~1 s.
                    Ok(None) if mirror_misses < 10 => mirror_misses += 1,
                    Ok(None) => {
                        mirror = false;
                        view.lock().unwrap().mirror.clear();
                        send(Msg::MirrorStopped(
                            "Mirror stopped: keyboard kept declining reads".into(),
                        ));
                    }
                    Err(err) => {
                        mirror = false;
                        kb = None;
                        view.lock().unwrap().mirror.clear();
                        send(Msg::MirrorStopped(format!("Mirror stopped: {err:#}")));
                    }
                }
                ctx.request_repaint();
            }

            let Some(e) = fx.as_mut() else { continue };
            next = Instant::now() + FRAME;
            let colors = e.frame();
            let frame = RtFrame::new(
                &leds
                    .iter()
                    .copied()
                    .zip(colors.iter().map(|&c| out(c)))
                    .collect::<Vec<_>>(),
            );
            let res = write(&mut kb, |k| k.stream(&frame));
            {
                let mut v = view.lock().unwrap();
                v.colors = colors;
                v.label = e.label();
            }
            ctx.request_repaint();
            match res {
                Ok(Sent::Shown) => busy_frames = 0,
                // The driver just retries next frame; we give up after ~1 s of refusals.
                Ok(Sent::Busy) => {
                    busy_frames += 1;
                    if busy_frames >= 30 {
                        fx = None;
                        view.lock().unwrap().running = false;
                        send(Msg::LiveStopped(
                            "Keyboard kept declining live frames; live stopped (nothing was stored)".into(),
                        ));
                    }
                }
                Err(err) => {
                    // A real I/O failure mid-frame: stop, and lock writes until replug.
                    if err.is::<WriteFailed>() {
                        if let Some(k) = &kb {
                            k.mark_interrupted();
                        }
                        view.lock().unwrap().mirror.clear();
                    }
                    fx = None;
                    kb = None;
                    view.lock().unwrap().running = false;
                    send(Msg::LiveStopped(format!("{err:#}")));
                }
            }
        }
    });
    (jtx, mrx)
}

const SYSTEM_AUDIO: &str = "System audio (what's playing)";

/// Marks an error that happened after talking to the device (vs. failing to open it).
#[derive(Debug)]
struct WriteFailed;

impl std::fmt::Display for WriteFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("write to keyboard failed")
    }
}

impl std::error::Error for WriteFailed {}

/// Opens the keyboard if needed. Failing here touched nothing.
fn device(kb: &mut Option<Keyboard>) -> Result<&Keyboard> {
    if kb.is_none() {
        *kb = Some(Keyboard::open()?);
    }
    Ok(kb.as_ref().unwrap())
}

/// Runs a write, tagging its failure as `WriteFailed` (which locks writes until replug).
fn write<T>(kb: &mut Option<Keyboard>, op: impl FnOnce(&Keyboard) -> Result<T>) -> Result<T> {
    // `NotSent` means refused before anything was written: no lock, just try again.
    op(device(kb)?).map_err(|e| {
        if e.is::<proto::NotSent>() {
            e
        } else {
            e.context(WriteFailed)
        }
    })
}

/// Read-only USB enumeration every 2 s; never opens or writes to the device.
#[derive(Default)]
struct Presence {
    present: AtomicBool,
    plugs: AtomicU32, // incremented on every absent → present transition
}

fn spawn_presence(ctx: egui::Context) -> Arc<Presence> {
    let usb = Arc::new(Presence::default());
    let u = usb.clone();
    thread::spawn(move || {
        loop {
            let now = hidapi::HidApi::new()
                .map(|a| {
                    a.device_list()
                        .any(|d| d.vendor_id() == proto::VID && d.product_id() == proto::PID)
                })
                .unwrap_or(false);
            if u.present.load(Relaxed) != now {
                if now {
                    // Bump the plug counter before publishing `present`, so the worker never
                    // acts on a reconnect while still holding the old handle.
                    u.plugs.fetch_add(1, Relaxed);
                }
                u.present.store(now, Relaxed);
                ctx.request_repaint();
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
    usb
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Lighting,
    PerKey,
    Live,
    Screen,
    Clock,
}

struct App {
    jobs: mpsc::Sender<Job>,
    msgs: mpsc::Receiver<Msg>,
    usb: Arc<Presence>,
    busy: bool,
    progress: Option<u32>,
    status: Result<&'static str, String>,
    tab: Tab,
    light: Light,
    keys: Vec<[u8; 3]>,
    brush: [u8; 3],
    key_brightness: Level,
    screen_path: String,
    frames: Vec<(Vec<u8>, u32)>,
    preview: Option<egui::TextureHandle>,
    screen_confirm: bool,
    view: Arc<Mutex<LiveView>>,
    live_kind: LiveKind,
    audio_devices: Vec<String>,
    audio_device: Option<String>,
    sensitivity: f32,
    work_min: u32,
    rest_min: u32,
    /// Lighting preview shows what the keyboard reports instead of a simulation.
    mirror_on: bool,
    mirror_sent: bool,
    /// Shared with the worker: gamma-correct colours sent to the keyboard.
    vivid: Arc<AtomicBool>,
}

#[derive(PartialEq, Clone, Copy)]
enum LiveKind {
    Music,
    Pomodoro,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        style(&cc.egui_ctx);
        let view = Arc::new(Mutex::new(LiveView::default()));
        let usb = spawn_presence(cc.egui_ctx.clone());
        let vivid = Arc::new(AtomicBool::new(true));
        let (jobs, msgs) = spawn_worker(cc.egui_ctx.clone(), view.clone(), usb.clone(), vivid.clone());
        let mut app = Self::with(jobs, msgs, usb, view);
        app.vivid = vivid;
        app.audio_devices = effects::input_devices();
        if let Some(s) = cc
            .storage
            .and_then(|st| eframe::get_value::<Saved>(st, eframe::APP_KEY))
        {
            app.restore(s);
        }
        app
    }

    /// Default state around already-running worker channels.
    fn with(
        jobs: mpsc::Sender<Job>,
        msgs: mpsc::Receiver<Msg>,
        usb: Arc<Presence>,
        view: Arc<Mutex<LiveView>>,
    ) -> Self {
        let mut app = Self {
            jobs,
            msgs,
            usb,
            busy: false,
            progress: None,
            status: Ok("Ready"),
            tab: Tab::Lighting,
            light: Light {
                mode: Mode::all().next().unwrap(),
                rgb: [0; 3],
                rainbow: false,
                brightness: Level::new(0),
                speed: Level::new(0),
                direction: Direction::Left,
            },
            keys: vec![],
            brush: [0; 3],
            key_brightness: Level::new(0),
            screen_path: String::new(),
            frames: vec![],
            preview: None,
            screen_confirm: false,
            view,
            live_kind: LiveKind::Music,
            audio_devices: vec![],
            audio_device: None,
            sensitivity: 0.0,
            work_min: 0,
            rest_min: 0,
            mirror_on: false, // opt-in until `04 F5` has been confirmed on hardware
            mirror_sent: false,
            vivid: Default::default(),
        };
        app.restore(Saved::default()); // single source of defaults
        app
    }

    fn saved(&self) -> Saved {
        Saved {
            mode: self.light.mode.name().into(),
            rgb: self.light.rgb,
            rainbow: self.light.rainbow,
            brightness: self.light.brightness.get(),
            speed: self.light.speed.get(),
            direction: self.light.direction as u8,
            keys: self.keys.clone(),
            brush: self.brush,
            key_brightness: self.key_brightness.get(),
            pomodoro: self.live_kind == LiveKind::Pomodoro,
            audio_device: self.audio_device.clone(),
            sensitivity: self.sensitivity,
            work_min: self.work_min,
            rest_min: self.rest_min,
            vivid: self.vivid.load(Relaxed),
        }
    }

    /// The settings file is user-editable, so every value goes back through the same
    /// validating constructors as the UI; anything unknown or out of range keeps the default.
    fn restore(&mut self, s: Saved) {
        if let Some(m) = Mode::all().find(|m| m.name() == s.mode) {
            self.light.mode = m;
        }
        self.light.rgb = s.rgb;
        self.light.rainbow = s.rainbow;
        self.light.brightness = Level::new(s.brightness);
        self.light.speed = Level::new(s.speed);
        if let Some(d) = Direction::ALL.get(s.direction as usize) {
            self.light.direction = *d;
        }
        if s.keys.len() == KEYS.len() {
            self.keys = s.keys;
        }
        self.brush = s.brush;
        self.key_brightness = Level::new(s.key_brightness);
        self.live_kind = if s.pomodoro {
            LiveKind::Pomodoro
        } else {
            LiveKind::Music
        };
        self.audio_device = s.audio_device;
        if s.sensitivity.is_finite() {
            self.sensitivity = s.sensitivity.clamp(*SENSITIVITY.start(), *SENSITIVITY.end());
        }
        self.work_min = s.work_min.clamp(*FOCUS_MIN.start(), *FOCUS_MIN.end());
        self.rest_min = s.rest_min.clamp(*BREAK_MIN.start(), *BREAK_MIN.end());
        self.vivid.store(s.vivid, Relaxed);
    }

    fn run(&mut self, job: Job) {
        if !self.busy && self.jobs.send(job).is_ok() {
            self.busy = true;
            self.progress = None;
            self.status = Ok("Working…");
        }
    }

    fn lighting(&mut self, ui: &mut egui::Ui) {
        header(ui, "Lighting", "Hardware effect, stored on the keyboard");
        let t = ui.input(|i| i.time) as f32;
        ui.ctx().request_repaint_after(Duration::from_millis(33));
        let l = self.light;
        egui::Frame::new()
            .fill(PANEL)
            .corner_radius(14)
            .inner_margin(16)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let mirror = self.view.lock().unwrap().mirror.clone();
                let live = self.mirror_on && mirror.len() == KEYS.len();
                ui.horizontal(|ui| {
                    label(ui, "Preview");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        toggle(ui, &mut self.mirror_on, "Mirror keyboard");
                        ui.add_space(12.0);
                        ui.label(if live {
                            RichText::new("● live from the keyboard").color(OK).size(12.0)
                        } else {
                            RichText::new("simulated · approximate").color(MUTED).size(12.0)
                        });
                    });
                });
                ui.add_space(6.0);
                if live {
                    keyboard(ui, false, |i, _| rgb(mirror[i]));
                } else {
                    keyboard(ui, false, |i, p| preview(&l, i, p, t));
                }
            });
        ui.add_space(12.0);

        let l = &mut self.light;
        ui.columns(2, |c| {
            card(&mut c[0], "Effect", |ui| {
                let cols = 4;
                let gap = 6.0;
                let w = (ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32;
                ui.spacing_mut().item_spacing = Vec2::splat(gap);
                let modes: Vec<Mode> = Mode::all().collect();
                for row in modes.chunks(cols) {
                    ui.horizontal(|ui| {
                        for &m in row {
                            if tile(ui, m.name(), l.mode == m, Vec2::new(w, 34.0)).clicked() {
                                l.mode = m;
                            }
                        }
                    });
                }
            });
            card(&mut c[1], "Settings", |ui| {
                let m = l.mode;
                if m.has_color() || m.has_rainbow() {
                    label(ui, "Colour");
                    ui.add_enabled_ui(!(l.rainbow && m.has_rainbow()) && m.has_color(), |ui| {
                        swatches(ui, &mut l.rgb)
                    });
                    if m.has_rainbow() {
                        ui.add_space(4.0);
                        toggle(ui, &mut l.rainbow, "Rainbow");
                    }
                    ui.add_space(10.0);
                }
                if m.has_brightness() {
                    level(ui, "Brightness", &mut l.brightness);
                }
                if m.has_speed() {
                    level(ui, "Speed", &mut l.speed);
                }
                if m.has_horizontal() || m.has_vertical() {
                    ui.add_space(6.0);
                    label(ui, "Direction");
                    let opts: &[(Direction, &str)] = if m.has_horizontal() {
                        &[(Direction::Left, "←  Left"), (Direction::Right, "Right  →")]
                    } else {
                        &[(Direction::Up, "↑  Up"), (Direction::Down, "Down  ↓")]
                    };
                    if !opts.iter().any(|o| o.0 == l.direction) {
                        l.direction = opts[0].0;
                    }
                    ui.horizontal(|ui| {
                        for &(d, name) in opts {
                            if tile(ui, name, l.direction == d, Vec2::new(110.0, 32.0)).clicked() {
                                l.direction = d;
                            }
                        }
                    });
                }
                if m.name() == "Off" {
                    ui.label(RichText::new("Backlight will be turned off.").color(MUTED));
                }
            });
        });
    }

    fn per_key(&mut self, ui: &mut egui::Ui) {
        header(
            ui,
            "Per-key",
            "Paint keys, then apply. Overwrites the keyboard's user-lighting slot.",
        );
        card(ui, "Brush", |ui| {
            ui.horizontal(|ui| {
                swatches(ui, &mut self.brush);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Clear").clicked() {
                        self.keys.fill([0; 3]);
                    }
                    if ui.button("Fill all").clicked() {
                        self.keys.fill(self.brush);
                    }
                    // Not while live runs: it would copy the streamed frame.
                    let can_read = !self.busy && self.usb.present.load(Relaxed) && !self.view.lock().unwrap().running;
                    if ui
                        .add_enabled(can_read, egui::Button::new("Load from keyboard"))
                        .on_hover_text("Copy what the keyboard shows right now")
                        .clicked()
                    {
                        self.run(Job::ReadKeys);
                    }
                });
            });
            ui.label(
                RichText::new("Drag to paint · right-click a key to pick its colour")
                    .color(MUTED)
                    .size(12.0),
            );
        });

        let keys = self.keys.clone();
        let (resp, rects) = egui::Frame::new()
            .fill(PANEL)
            .corner_radius(14)
            .inner_margin(16)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                keyboard(ui, true, |i, _| rgb(keys[i]))
            })
            .inner;
        if let Some(p) = resp.interact_pointer_pos()
            && let Some(i) = rects.iter().position(|r| r.contains(p))
        {
            if resp.secondary_clicked() {
                self.brush = self.keys[i];
            } else if resp.dragged_by(egui::PointerButton::Primary) || resp.clicked() {
                self.keys[i] = self.brush;
            }
        }
        ui.add_space(12.0);
        card(ui, "Output", |ui| level(ui, "Brightness", &mut self.key_brightness));
    }

    fn screen(&mut self, ui: &mut egui::Ui) {
        header(
            ui,
            "Screen",
            "128×128 TFT · PNG, JPG or animated GIF (up to 255 frames)",
        );
        let (dropped, hovering) = ui.ctx().input(|i| {
            (
                i.raw.dropped_files.first().map(|f| f.path().to_path_buf()),
                !i.raw.hovered_files.is_empty(),
            )
        });
        if let Some(p) = dropped {
            self.screen_path = p.display().to_string();
            self.load_image(ui.ctx());
        }

        ui.columns(2, |c| {
            let ui = &mut c[0];
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 150.0), Sense::hover());
            let border = if hovering { ACCENT } else { Color32::from_gray(70) };
            ui.painter().rect(
                rect,
                14,
                if hovering { ACCENT.gamma_multiply(0.08) } else { PANEL },
                Stroke::new(1.5, border),
                egui::StrokeKind::Inside,
            );
            draw_icon(ui.painter(), Icon::Drop, rect.center() - Vec2::new(0.0, 12.0), border);
            ui.painter().text(
                rect.center() + Vec2::new(0.0, 22.0),
                Align2::CENTER_CENTER,
                "Drop an image here",
                FontId::proportional(14.0),
                TEXT,
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.screen_path)
                        .hint_text("or paste a path…")
                        .desired_width(ui.available_width() - 70.0),
                );
                if ui.button("Load").clicked() {
                    self.load_image(ui.ctx());
                }
            });

            let ui = &mut c[1];
            card(ui, "Preview", |ui| {
                let size = Vec2::splat(192.0);
                match &self.preview {
                    Some(tex) => {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(size).corner_radius(10));
                        let n = self.frames.len();
                        ui.label(RichText::new(format!("{n} frame{}", if n == 1 { "" } else { "s" })).color(MUTED));
                        ui.add_space(6.0);
                        toggle(ui, &mut self.screen_confirm, "Replace the animation on the keyboard");
                    }
                    None => {
                        let (r, _) = ui.allocate_exact_size(size, Sense::hover());
                        ui.painter().rect_filled(r, 10, BG);
                        ui.painter().text(
                            r.center(),
                            Align2::CENTER_CENTER,
                            "No image",
                            FontId::proportional(13.0),
                            MUTED,
                        );
                    }
                }
            });
        });
    }

    fn live(&mut self, ui: &mut egui::Ui) {
        header(
            ui,
            "Live",
            "Computed on this computer and streamed in real time · nothing is stored on the keyboard",
        );
        let (running, colors, text) = {
            let v = self.view.lock().unwrap();
            (v.running, v.colors.clone(), v.label.clone())
        };
        Frame::new()
            .fill(PANEL)
            .corner_radius(14)
            .inner_margin(16)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let tag = if running {
                    if text.is_empty() {
                        "● Live".to_string()
                    } else {
                        format!("● Live · {text}")
                    }
                } else {
                    "Stopped".to_string()
                };
                title_row(ui, "Output", RichText::new(tag).color(if running { OK } else { MUTED }));
                keyboard(ui, false, |i, _| {
                    colors
                        .get(i)
                        .filter(|_| running)
                        .map_or(Color32::from_gray(26), |&c| rgb(c))
                });
            });
        ui.add_space(12.0);
        ui.columns(2, |c| {
            card(&mut c[0], "Effect", |ui| {
                ui.horizontal(|ui| {
                    for (k, name) in [(LiveKind::Music, "Music"), (LiveKind::Pomodoro, "Pomodoro")] {
                        if tile(ui, name, self.live_kind == k, Vec2::new(150.0, 40.0)).clicked() {
                            self.live_kind = k;
                        }
                    }
                });
                ui.add_space(6.0);
                ui.label(RichText::new(match self.live_kind {
                    LiveKind::Music => "Spectrum bars across the columns, height across the rows.",
                    LiveKind::Pomodoro => "F1–F12 burn down as a timer; the whole board pulses when the phase changes.",
                }).color(MUTED).size(12.0));
            });
            card(&mut c[1], "Settings · applied on start", |ui| match self.live_kind {
                LiveKind::Music => {
                    label(ui, "Audio source");
                    ui.horizontal(|ui| {
                        let current = self.audio_device.clone().unwrap_or_else(|| SYSTEM_AUDIO.into());
                        egui::ComboBox::from_id_salt("audio").width(260.0).selected_text(current).show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.audio_device, None, SYSTEM_AUDIO);
                            for d in &self.audio_devices {
                                ui.selectable_value(&mut self.audio_device, Some(d.clone()), d);
                            }
                        });
                        if ui.button("Rescan").on_hover_text("Rescan audio devices").clicked() {
                            self.audio_devices = effects::input_devices();
                        }
                    });
                    ui.label(RichText::new("System audio records what's playing (macOS asks for permission once). Or pick a microphone.").color(MUTED).size(12.0));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.add_sized([86.0, 20.0], egui::Label::new("Sensitivity"));
                        ui.add(egui::Slider::new(&mut self.sensitivity, SENSITIVITY).fixed_decimals(1));
                    });
                }
                LiveKind::Pomodoro => {
                    ui.horizontal(|ui| {
                        ui.add_sized([86.0, 20.0], egui::Label::new("Focus"));
                        ui.add(egui::Slider::new(&mut self.work_min, FOCUS_MIN).suffix(" min"));
                    });
                    ui.horizontal(|ui| {
                        ui.add_sized([86.0, 20.0], egui::Label::new("Break"));
                        ui.add(egui::Slider::new(&mut self.rest_min, BREAK_MIN).suffix(" min"));
                    });
                }
            });
        });
        if running {
            ui.ctx().request_repaint_after(FRAME);
        }
    }

    fn clock(&mut self, ui: &mut egui::Ui) {
        header(ui, "Clock", "Set the screen clock to this computer's local time");
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        let now = local_now();
        egui::Frame::new()
            .fill(PANEL)
            .corner_radius(14)
            .inner_margin(28)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(&now.0).size(64.0).color(TEXT).monospace());
                    ui.label(RichText::new(&now.1).size(15.0).color(MUTED));
                });
            });
    }

    /// Bottom bar: connection, status, and the current tab's primary action.
    fn action_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            let present = self.usb.present.load(Relaxed);
            dot(ui, if present { OK } else { ERR });
            ui.label(
                RichText::new(if present {
                    "F75 Max connected"
                } else {
                    "Keyboard not found"
                })
                .color(TEXT),
            );
            ui.add_space(16.0);
            let (txt, col) = match &self.status {
                Ok(s) => (s.to_string(), if self.busy { ACCENT } else { MUTED }),
                Err(e) => (e.clone(), ERR),
            };
            ui.add(egui::Label::new(RichText::new(txt).color(col).size(13.0)).truncate());

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let ready = present && !self.busy;
                match self.tab {
                    Tab::Lighting => {
                        if primary(ui, "Apply lighting", ready) {
                            self.run(Job::Light(self.light));
                        }
                    }
                    Tab::PerKey => {
                        if primary(ui, "Apply colours", ready) {
                            let keys = (0..KEYS.len())
                                .filter_map(|i| Some((LedIndex::of_key(i)?, self.keys[i])))
                                .collect();
                            self.run(Job::PerKey(keys, self.key_brightness));
                        }
                    }
                    Tab::Screen => {
                        let can = ready && self.screen_confirm && !self.frames.is_empty();
                        if primary(ui, "Upload to screen", can) {
                            let n = self.frames.len().min(proto::MAX_FRAMES);
                            match Animation::from_rgba(&self.frames[..n]) {
                                Ok(a) => self.run(Job::Screen(a)),
                                Err(e) => self.status = Err(format!("{e:#}")),
                            }
                        }
                        if let Some(p) = self.progress.filter(|_| self.busy) {
                            ui.add(
                                egui::ProgressBar::new(p as f32 / 100.0)
                                    .desired_width(160.0)
                                    .show_percentage(),
                            );
                        }
                    }
                    Tab::Clock => {
                        if primary(ui, "Sync clock", ready) {
                            self.run(Job::Clock);
                        }
                    }
                    Tab::Live => {
                        if self.view.lock().unwrap().running {
                            if primary(ui, "Stop", !self.busy) {
                                self.run(Job::StopLive);
                            }
                        } else if primary(ui, "Start live", ready) {
                            self.run(Job::Live(match self.live_kind {
                                LiveKind::Music => LiveCfg::Music {
                                    device: self.audio_device.clone(),
                                    sensitivity: self.sensitivity,
                                },
                                LiveKind::Pomodoro => LiveCfg::Pomodoro {
                                    work: self.work_min,
                                    rest: self.rest_min,
                                },
                            }));
                        }
                    }
                }
            });
        });
    }

    fn load_image(&mut self, ctx: &egui::Context) {
        match load_frames(Path::new(self.screen_path.trim())) {
            Ok(f) => {
                let img = egui::ColorImage::from_rgba_unmultiplied([proto::SCREEN; 2], &f[0].0);
                self.preview = Some(ctx.load_texture("screen", img, egui::TextureOptions::NEAREST));
                self.frames = f;
                self.screen_confirm = false;
                self.status = Ok("Image loaded");
            }
            Err(e) => self.status = Err(format!("{e:#}")),
        }
    }
}

/// Decodes an image/GIF into 128×128 RGBA frames. Untrusted input: only regular files up to
/// 32 MiB, decoder limits on canvas size and memory, frames resized one at a time.
fn load_frames(path: &Path) -> Result<Vec<(Vec<u8>, u32)>> {
    let meta = std::fs::metadata(path)?;
    anyhow::ensure!(
        meta.is_file() && meta.len() <= 32 << 20,
        "not a regular file under 32 MiB"
    );
    let bytes = std::fs::read(path)?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(256 << 20);
    let n = proto::SCREEN as u32;
    let shrink = |f: image::Frame| {
        let (num, den) = f.delay().numer_denom_ms();
        let ms = num / den.max(1);
        let px = imageops::resize(f.buffer(), n, n, imageops::FilterType::Triangle).into_raw();
        (px, if ms <= 10 { 100 } else { ms }) // browsers treat 0–10 ms as 100 ms
    };
    let frames: Vec<_> = if image::guess_format(&bytes)? == ImageFormat::Gif {
        let mut dec = GifDecoder::new(Cursor::new(&bytes))?;
        image::ImageDecoder::set_limits(&mut dec, limits)?;
        dec.into_frames()
            .take(proto::MAX_FRAMES)
            .map(|f| f.map(shrink))
            .collect::<Result<_, _>>()?
    } else {
        let mut r = image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
        r.limits(limits);
        vec![shrink(image::Frame::new(r.decode()?.to_rgba8()))]
    };
    anyhow::ensure!(!frames.is_empty(), "image has no frames");
    Ok(frames)
}

fn local_now() -> (String, String) {
    let Ok(tm) = proto::local_time() else {
        return Default::default();
    };
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    (
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
        format!(
            "{} · {:02}.{:02}.{}",
            DAYS[tm.tm_wday.clamp(0, 6) as usize],
            tm.tm_mday,
            tm.tm_mon + 1,
            tm.tm_year + 1900
        ),
    )
}

/// Rough on-screen imitation of each firmware effect; the real one runs on the keyboard.
fn preview(l: &Light, key: usize, p: Vec2, t: f32) -> Color32 {
    use std::f32::consts::TAU;
    let name = l.mode.name();
    if name == "Off" {
        return Color32::from_gray(18);
    }
    let t = t * (0.3 + l.speed.get() as f32 * 0.3);
    let along = match l.direction {
        Direction::Left => 1.0 - p.x,
        Direction::Right => p.x,
        Direction::Up => 1.0 - p.y,
        Direction::Down => p.y,
    };
    let d = (p - Vec2::splat(0.5)).length();
    let rnd = |n: u32| ((key as u32).wrapping_mul(2654435761) ^ n.wrapping_mul(40503)).wrapping_mul(2246822519) >> 24;
    let (hue, v) = match name {
        "Static" => (p.x * 0.9, 1.0),
        "Breath" | "Pulsating" => (p.x * 0.9, 0.12 + 0.88 * (0.5 + 0.5 * (t * 2.0).sin())),
        "Spectrum" => (t * 0.12, 1.0),
        "Colourful" => (p.x * 0.8 + t * 0.12, 1.0),
        "Scrolling" | "Rolling" | "Flowing" | "Tilt" => (
            along - t * 0.25,
            0.35 + 0.65 * (0.5 + 0.5 * (along * TAU * 1.2 - t * 3.0).sin()),
        ),
        "Rotating" => ((p.y - 0.5).atan2(p.x - 0.5) / TAU + t * 0.2, 1.0),
        "Outward" | "Explode" | "Ripples" | "Launch" => (d - t * 0.1, (0.5 + 0.5 * (d * 16.0 - t * 4.0).sin()).powi(3)),
        _ => {
            let phase = (t * 2.5 + (key % 7) as f32 * 0.37) as u32;
            (
                rnd(phase) as f32 / 255.0,
                if rnd(phase ^ 0x9e37) < 60 { 1.0 } else { 0.06 },
            )
        }
    };
    let base = if l.mode.has_color() && !(l.rainbow && l.mode.has_rainbow()) {
        rgb(l.rgb)
    } else {
        Color32::from(egui::ecolor::Hsva::new(hue.rem_euclid(1.0), 0.85, 1.0, 1.0))
    };
    let level = if l.mode.has_brightness() {
        l.brightness.get() as f32 / Level::MAX as f32
    } else {
        1.0
    };
    Color32::from_gray(18).lerp_to_gamma(base, (v * level).clamp(0.0, 1.0))
}

fn rgb([r, g, b]: [u8; 3]) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// Draws the board with glowing keys. Returns its response and the on-screen rect of every key.
fn keyboard(
    ui: &mut egui::Ui,
    interactive: bool,
    color: impl Fn(usize, Vec2) -> Color32,
) -> (egui::Response, Vec<Rect>) {
    let (x0, y0, x1, y1) = crate::layout::bounds();
    let pad = 18.0;
    // Fit width, but never take more than ~36% of the visible height.
    let scale = ((ui.available_width() - pad * 2.0) / (x1 - x0))
        .min(ui.clip_rect().height() * 0.36 / (y1 - y0))
        .min(1.7);
    let size = Vec2::new(ui.available_width(), (y1 - y0) * scale + pad * 2.0);
    let sense = if interactive {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let (resp, painter) = ui.allocate_painter(size, sense);
    let plate = Rect::from_center_size(resp.rect.center(), Vec2::new((x1 - x0) * scale + pad * 2.0, size.y));
    painter.rect(
        plate,
        16,
        Color32::from_rgb(0x0c, 0x0d, 0x0f),
        Stroke::new(1.0, Color32::from_gray(38)),
        egui::StrokeKind::Inside,
    );
    let origin = plate.min + Vec2::splat(pad);

    let keys: Vec<(Rect, Color32)> = KEYS
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let r = Rect::from_min_size(
                origin + Vec2::new((k.2 - x0) * scale, (k.3 - y0) * scale),
                Vec2::new(k.4 * scale, k.5 * scale),
            )
            .shrink(2.0);
            let p = Vec2::new((k.2 + k.4 / 2.0 - x0) / (x1 - x0), (k.3 + k.5 / 2.0 - y0) / (y1 - y0));
            (r, color(i, p))
        })
        .collect();

    // Glow first so it never covers neighbouring caps.
    for &(r, c) in &keys {
        let lit = c.r().max(c.g()).max(c.b()) as f32 / 255.0;
        if lit > 0.15 {
            let glow = Shadow {
                offset: [0, 0],
                blur: 18,
                spread: 1,
                color: c.gamma_multiply(0.30 * lit),
            };
            painter.add(glow.as_shape(r, 7));
        }
    }
    let hovered = resp.hover_pos();
    for (i, &(r, c)) in keys.iter().enumerate() {
        painter.rect_filled(r, 7, Color32::from_rgb(0x22, 0x24, 0x29));
        painter.rect_filled(r.shrink(2.5), 5, c);
        let shine = Rect::from_min_size(r.shrink(2.5).min, Vec2::new(r.width() - 5.0, (r.height() - 5.0) * 0.45));
        painter.rect_filled(
            shine,
            CornerRadius {
                nw: 5,
                ne: 5,
                sw: 0,
                se: 0,
            },
            Color32::from_white_alpha(14),
        );
        if interactive && hovered.is_some_and(|p| r.contains(p)) {
            painter.rect_stroke(r, 7, Stroke::new(2.0, TEXT), egui::StrokeKind::Outside);
        }
        let luma = 0.3 * c.r() as f32 + 0.59 * c.g() as f32 + 0.11 * c.b() as f32;
        let fg = if luma > 150.0 {
            Color32::from_gray(20)
        } else {
            Color32::from_gray(225)
        };
        painter.text(
            r.center(),
            Align2::CENTER_CENTER,
            KEYS[i].0,
            FontId::proportional((9.5 * scale).clamp(9.0, 14.0)),
            fg,
        );
    }
    (resp, keys.into_iter().map(|k| k.0).collect())
}

fn header(ui: &mut egui::Ui, title: &str, sub: &str) {
    ui.label(RichText::new(title).size(24.0).strong().color(TEXT));
    ui.label(RichText::new(sub).size(13.0).color(MUTED));
    ui.add_space(14.0);
}

fn label(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).size(11.0).strong().color(MUTED));
}

fn card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .fill(PANEL)
        .corner_radius(14)
        .inner_margin(16)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            label(ui, title);
            ui.add_space(8.0);
            body(ui);
        });
    ui.add_space(12.0);
}

/// Selectable tile with animated fill; accent only marks the selection.
fn tile(ui: &mut egui::Ui, text: &str, selected: bool, size: Vec2) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let a = ui.ctx().animate_bool_with_time(resp.id, selected, 0.15);
    let h = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
    let fill = WIDGET
        .lerp_to_gamma(Color32::from_rgb(0x30, 0x34, 0x3c), h)
        .lerp_to_gamma(ACCENT.gamma_multiply(0.22), a);
    ui.painter().rect(
        rect,
        9,
        fill,
        Stroke::new(1.5, ACCENT.gamma_multiply(a)),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::proportional(13.0),
        TEXT.lerp_to_gamma(ACCENT, a),
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn swatches(ui: &mut egui::Ui, value: &mut [u8; 3]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        for s in SWATCHES {
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
            let sel = *value == s;
            if sel {
                ui.painter().circle_stroke(rect.center(), 13.0, Stroke::new(2.0, TEXT));
            }
            ui.painter()
                .circle_filled(rect.center(), if sel { 9.0 } else { 11.0 }, rgb(s));
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                *value = s;
            }
        }
        ui.add_space(6.0);
        ui.color_edit_button_srgb(value).on_hover_text("Custom colour");
    });
}

fn toggle(ui: &mut egui::Ui, on: &mut bool, text: &str) {
    ui.horizontal(|ui| {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(38.0, 22.0), Sense::click());
        if resp.clicked() {
            *on = !*on;
        }
        let a = ui.ctx().animate_bool_with_time(resp.id, *on, 0.12);
        ui.painter().rect_filled(rect, 11, WIDGET.lerp_to_gamma(ACCENT, a));
        let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, a);
        ui.painter().circle_filled(egui::pos2(x, rect.center().y), 8.0, TEXT);
        if ui
            .add(egui::Label::new(RichText::new(text).color(TEXT)).sense(Sense::click()))
            .clicked()
        {
            *on = !*on;
        }
    });
}

fn dot(ui: &mut egui::Ui, c: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 6.0, c.gamma_multiply(0.25));
    ui.painter().circle_filled(rect.center(), 3.5, c);
}

fn level(ui: &mut egui::Ui, name: &str, v: &mut Level) {
    let mut x = v.get();
    ui.horizontal(|ui| {
        ui.add_sized([86.0, 20.0], egui::Label::new(RichText::new(name).color(TEXT)));
        ui.add(egui::Slider::new(&mut x, 0..=Level::MAX).show_value(true));
    });
    *v = Level::new(x);
}

fn primary(ui: &mut egui::Ui, text: &str, enabled: bool) -> bool {
    let b = egui::Button::new(RichText::new(text).size(15.0).color(Color32::from_gray(15)).strong())
        .fill(ACCENT)
        .corner_radius(10)
        .min_size(Vec2::new(180.0, 40.0));
    ui.add_enabled(enabled, b)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

#[derive(Clone, Copy)]
enum Icon {
    Bulb,
    Keys,
    Bars,
    Display,
    Clock,
    Drop,
}

/// Line icons drawn with the painter so they share the UI's colour and stroke weight.
fn draw_icon(p: &egui::Painter, icon: Icon, c: egui::Pos2, col: Color32) {
    let st = Stroke::new(1.6, col);
    let at = |x: f32, y: f32| c + Vec2::new(x, y);
    let line = |a: egui::Pos2, b: egui::Pos2| p.line_segment([a, b], st);
    let outline = |r: Rect, rad: u8| p.rect_stroke(r, rad, st, egui::StrokeKind::Middle);
    match icon {
        Icon::Bulb => {
            p.circle_stroke(at(0.0, -2.5), 5.5, st);
            line(at(-2.5, 4.5), at(2.5, 4.5));
            line(at(-1.6, 7.0), at(1.6, 7.0));
        }
        Icon::Keys => {
            outline(Rect::from_center_size(c, Vec2::new(18.0, 12.0)), 3);
            for x in [-5.0, 0.0, 5.0] {
                p.rect_filled(Rect::from_center_size(at(x, -1.8), Vec2::splat(2.4)), 1, col);
            }
            line(at(-4.0, 2.6), at(4.0, 2.6));
        }
        Icon::Bars => {
            for (i, h) in [7.0, 13.0, 9.0, 15.0].into_iter().enumerate() {
                let x = -6.0 + i as f32 * 4.0;
                p.rect_filled(Rect::from_min_max(at(x - 1.2, 7.5 - h), at(x + 1.2, 7.5)), 1, col);
            }
        }
        Icon::Display => {
            outline(Rect::from_center_size(at(0.0, -1.5), Vec2::new(16.0, 12.0)), 2);
            line(at(-3.5, 7.0), at(3.5, 7.0));
        }
        Icon::Clock => {
            p.circle_stroke(c, 7.5, st);
            line(c, at(0.0, -4.5));
            line(c, at(3.5, 0.0));
        }
        Icon::Drop => {
            let st = Stroke::new(2.0, col);
            p.line_segment([at(0.0, -9.0), at(0.0, 6.0)], st);
            p.line_segment([at(-6.0, 0.0), at(0.0, 6.0)], st);
            p.line_segment([at(6.0, 0.0), at(0.0, 6.0)], st);
            p.line_segment([at(-9.0, 10.0), at(9.0, 10.0)], st);
        }
    }
}

/// Card title with a right-aligned note, kept on its own row so nothing overlaps content.
fn title_row(ui: &mut egui::Ui, title: &str, note: RichText) {
    ui.horizontal(|ui| {
        label(ui, title);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(note.size(12.0)));
    });
    ui.add_space(6.0);
}

fn nav(ui: &mut egui::Ui, icon: Icon, text: &str, selected: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
    let a = ui.ctx().animate_bool_with_time(resp.id, selected, 0.15);
    let h = ui.ctx().animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
    let p = ui.painter();
    p.rect_filled(rect, 10, Color32::TRANSPARENT.lerp_to_gamma(WIDGET, h.max(a)));
    let bar = Rect::from_center_size(egui::pos2(rect.left() + 2.0, rect.center().y), Vec2::new(3.0, 20.0 * a));
    p.rect_filled(bar, 2, ACCENT);
    let col = MUTED.lerp_to_gamma(TEXT, h.max(a)).lerp_to_gamma(ACCENT, a * 0.6);
    draw_icon(p, icon, egui::pos2(rect.left() + 26.0, rect.center().y), col);
    p.text(
        egui::pos2(rect.left() + 46.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(15.0),
        col,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

// Shared by the sliders and `App::restore`, so the two can't drift apart.
const SENSITIVITY: std::ops::RangeInclusive<f32> = 0.5..=3.0;
const FOCUS_MIN: std::ops::RangeInclusive<u32> = 5..=90;
const BREAK_MIN: std::ops::RangeInclusive<u32> = 1..=30;

/// What survives a restart, and the app's defaults. Plain values only; `App::restore`
/// re-validates them. Fields missing from an older file take these defaults, not zero.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Saved {
    mode: String,
    rgb: [u8; 3],
    rainbow: bool,
    brightness: u8,
    speed: u8,
    direction: u8,
    keys: Vec<[u8; 3]>,
    brush: [u8; 3],
    key_brightness: u8,
    pomodoro: bool,
    audio_device: Option<String>,
    sensitivity: f32,
    work_min: u32,
    rest_min: u32,
    vivid: bool,
}

impl Default for Saved {
    fn default() -> Self {
        Saved {
            mode: "Rolling".into(),
            rgb: SWATCHES[1],
            rainbow: true,
            brightness: 5,
            speed: 3,
            direction: Direction::Right as u8,
            keys: vec![[0xe6, 0xe8, 0xf0]; KEYS.len()],
            brush: SWATCHES[5],
            key_brightness: 5,
            pomodoro: false,
            audio_device: None,
            sensitivity: 1.2,
            work_min: 25,
            rest_min: 5,
            vivid: true,
        }
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.saved());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(m) = self.msgs.try_recv() {
            match m {
                Msg::Progress(p) => self.progress = Some(p),
                Msg::LiveStopped(e) => self.status = Err(e),
                Msg::MirrorStopped(e) => {
                    self.status = Err(e);
                    self.mirror_on = false; // don't immediately restart a mirror that just failed
                }
                Msg::Keys(k) => self.keys = k,
                Msg::Done(r) => {
                    self.busy = false;
                    self.status = r;
                }
            }
        }
        let want = self.tab == Tab::Lighting && self.mirror_on && self.usb.present.load(Relaxed);
        if want != self.mirror_sent && self.jobs.send(Job::Mirror(want)).is_ok() {
            self.mirror_sent = want;
        }

        egui::Panel::left("nav")
            .exact_size(200.0)
            .frame(Frame::new().fill(PANEL).inner_margin(16))
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("AULA").size(24.0).strong().color(ACCENT));
                    ui.label(RichText::new("F75 MAX").size(12.0).strong().color(MUTED));
                });
                ui.add_space(22.0);
                for (t, icon, name) in [
                    (Tab::Lighting, Icon::Bulb, "Lighting"),
                    (Tab::PerKey, Icon::Keys, "Per-key"),
                    (Tab::Live, Icon::Bars, "Live"),
                    (Tab::Screen, Icon::Display, "Screen"),
                    (Tab::Clock, Icon::Clock, "Clock"),
                ] {
                    if nav(ui, icon, name, self.tab == t) {
                        self.tab = t;
                    }
                }
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    let mut v = self.vivid.load(Relaxed);
                    toggle(ui, &mut v, "Vivid colours");
                    self.vivid.store(v, Relaxed);
                    ui.label(
                        RichText::new("Gamma-corrects colours sent to the keyboard")
                            .color(MUTED)
                            .size(11.0),
                    );
                });
            });

        egui::Panel::bottom("bar")
            .exact_size(68.0)
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(20, 12))
                    .stroke(Stroke::new(1.0, Color32::from_gray(34))),
            )
            .show(ui, |ui| self.action_bar(ui));

        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(24))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                    Tab::Lighting => self.lighting(ui),
                    Tab::PerKey => self.per_key(ui),
                    Tab::Live => self.live(ui),
                    Tab::Screen => self.screen(ui),
                    Tab::Clock => self.clock(ui),
                });
            });
    }
}

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = PANEL;
    v.extreme_bg_color = Color32::from_rgb(0x0c, 0x0d, 0x0f);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.override_text_color = Some(TEXT);
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(8);
    }
    v.widgets.inactive.weak_bg_fill = WIDGET;
    v.widgets.inactive.bg_fill = WIDGET;
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x30, 0x34, 0x3c);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x30, 0x34, 0x3c);
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.active.weak_bg_fill = ACCENT.gamma_multiply(0.5);
    v.slider_trailing_fill = true;
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals(v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(10.0, 8.0);
        s.spacing.button_padding = Vec2::new(14.0, 7.0);
        s.spacing.slider_width = 200.0;
        s.spacing.interact_size.y = 26.0;
    });
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        if let Err(e) = cli::run(&args) {
            eprintln!("aula-control: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AULA F75 Max")
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([900.0, 620.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("aula-control", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_validates_untrusted_settings() {
        let (jobs, _) = mpsc::channel();
        let (_, msgs) = mpsc::channel();
        let mut app = App::with(jobs, msgs, Default::default(), Default::default());
        let defaults = (app.light.mode, app.keys.clone());
        app.restore(Saved {
            mode: "Definitely Not A Mode".into(),
            brightness: 200,
            speed: 99,
            direction: 7,
            keys: vec![[1, 2, 3]; 3], // wrong length
            key_brightness: 255,
            sensitivity: f32::NAN,
            work_min: 0,
            rest_min: 10_000,
            ..Default::default()
        });
        assert_eq!(app.light.mode, defaults.0);
        assert_eq!(app.keys, defaults.1);
        assert_eq!(
            (
                app.light.brightness.get(),
                app.light.speed.get(),
                app.key_brightness.get()
            ),
            (5, 5, 5)
        );
        assert_eq!(app.light.direction, Direction::Right);
        assert_eq!((app.sensitivity, app.work_min, app.rest_min), (1.2, 5, 30));

        // And a good file round-trips.
        let mut saved = app.saved();
        saved.mode = "Breath".into();
        saved.direction = 2;
        app.restore(saved);
        assert_eq!((app.light.mode.name(), app.light.direction), ("Breath", Direction::Up));
    }

    #[test]
    fn missing_settings_fields_take_defaults_not_zero() {
        let s: Saved = ron::from_str("(mode: \"Breath\")").unwrap();
        assert_eq!(
            (s.mode.as_str(), s.brightness, s.work_min, s.keys.len()),
            ("Breath", 5, 25, KEYS.len())
        );
    }

    #[test]
    fn write_failures_are_tagged_for_the_replug_lock() {
        let failed = anyhow::anyhow!("Apply: keyboard reported busy/error").context(WriteFailed);
        assert!(failed.is::<WriteFailed>());
        assert!(format!("{failed:#}").contains("busy"));
        assert!(!anyhow::anyhow!("AULA F75 Max not found").is::<WriteFailed>());
    }
}
