//! Command-line interface: `aula-control <command> [key=value ...]`. Without arguments the GUI starts.
//!
//! Same safety rules as the GUI: only the validated protocol calls, one holder of the keyboard
//! at a time (see `Keyboard::open`), and a failed write marks the board as interrupted so no
//! process touches it again before a replug. Ctrl-C never cuts a transaction in half.

use crate::effects::{self, Effect};
use crate::layout::KEYS;
use crate::proto::{Animation, Direction, Keyboard, LedIndex, Level, Light, Mode, RtFrame, Sent};
use crate::{BREAK_MIN, FOCUS_MIN, LIVE_BRIGHTNESS, SENSITIVITY, Saved};
use anyhow::{Context, Result, bail, ensure};
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: aula-control [command]          (no command: open the window)

  modes                                list lighting effects
  light <effect> [color=RRGGBB] [brightness=0-5] [speed=0-5] [dir=left|right|up|down] [rainbow]
  clock                                set the screen clock to local time
  screen <image> --yes                 upload a PNG/JPG/GIF to the screen (replaces the stored one)
  live music [device=NAME] [sensitivity=0.5-3] [low=RRGGBB] [high=RRGGBB] [bg=RRGGBB]
  live pomodoro [focus=5-90] [break=1-30] [focus-color=RRGGBB] [break-color=RRGGBB]
  live <star|rain|flower|breath|spectrum|spring|vertical|roll|rotate> [speed=1-15]
                                       stream until Ctrl-C (nothing is stored on the keyboard);
                                       every live effect also takes brightness=10-100 (percent)";

/// Builds the effect once the keyboard is ours.
type Make = Box<dyn FnOnce() -> Result<Box<dyn Effect>>>;

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, SeqCst); // async-signal-safe
}

pub fn run(args: &[String]) -> Result<()> {
    // Ctrl-C / SIGTERM only set a flag: a write in progress always completes.
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: installing a handler that only stores to an atomic.
        unsafe { libc::signal(sig, on_signal as *const () as libc::sighandler_t) };
    }
    dispatch(args)
}

fn dispatch(args: &[String]) -> Result<()> {
    let (cmd, rest) = args.split_first().context(USAGE)?;
    match cmd.as_str() {
        "modes" => {
            Mode::all().for_each(|m| println!("{}", m.name()));
            Ok(())
        }
        "light" => {
            let (effect, opts) = rest.split_first().context("light: which effect? (see `modes`)")?;
            let mut light = parse_light(effect, opts)?;
            light.rgb = effects::curve(light.rgb, saved_curve());
            write(|k| k.set_light(&light))?;
            println!("lighting applied");
            Ok(())
        }
        "clock" => {
            ensure!(rest.is_empty(), "clock takes no arguments");
            write(|k| k.sync_clock())?;
            println!("clock synced");
            Ok(())
        }
        "screen" => {
            let [path, yes] = rest else {
                bail!("screen: usage `screen <image> --yes`")
            };
            ensure!(
                yes == "--yes",
                "screen replaces the animation stored on the keyboard; add --yes"
            );
            let frames = crate::screen::load_frames(std::path::Path::new(path))?;
            let anim = Animation::from_rgba(&frames)?;
            ensure!(!STOP.load(SeqCst), "cancelled"); // decoding can take a while
            // Progress must never panic mid-upload (e.g. stderr is a closed pipe).
            write(|k| k.upload_screen(&anim, |p| drop(write!(std::io::stderr(), "\ruploading {p:3}%"))))?;
            let _ = writeln!(std::io::stderr());
            println!("screen uploaded ({} frames)", frames.len());
            Ok(())
        }
        "live" => {
            let (kind, opts) = rest.split_first().context("live: music or pomodoro?")?;
            let opts = kv(opts)?;
            let d = Saved::default();
            // Built only after the keyboard is ours, so a busy keyboard never starts audio capture.
            let (make, percent): (Make, u8) = match kind.as_str() {
                "music" => {
                    known(&opts, &["sensitivity", "device", "low", "high", "bg", "brightness"])?;
                    let sens = opt(&opts, "sensitivity", d.sensitivity, |v| parse_in(v, SENSITIVITY))?;
                    let device = opts.iter().find(|(k, _)| k == "device").map(|(_, v)| v.clone());
                    let colours = effects::MusicColours {
                        low: opt(&opts, "low", d.music_low, parse_hex)?,
                        high: opt(&opts, "high", d.music_high, parse_hex)?,
                        bg: opt(&opts, "bg", d.music_bg, parse_hex)?,
                    };
                    let make =
                        Box::new(move || Ok(Box::new(effects::Music::new(device.as_deref(), sens, colours)?) as _));
                    (make, brightness(&opts, &d)?)
                }
                "pomodoro" => {
                    known(&opts, &["focus", "break", "focus-color", "break-color", "brightness"])?;
                    let focus = opt(&opts, "focus", d.work_min, |v| parse_in(v, FOCUS_MIN))?;
                    let rest = opt(&opts, "break", d.rest_min, |v| parse_in(v, BREAK_MIN))?;
                    let (work_colour, rest_colour) = (
                        opt(&opts, "focus-color", d.work_colour, parse_hex)?,
                        opt(&opts, "break-color", d.rest_colour, parse_hex)?,
                    );
                    let make: Make = Box::new(move || {
                        Ok(Box::new(effects::Pomodoro::new(focus, rest, work_colour, rest_colour)) as _)
                    });
                    (make, brightness(&opts, &d)?)
                }
                other => {
                    let fx = effects::VendorFx::from_name(other)
                        .with_context(|| format!("live: unknown effect `{other}`"))?;
                    known(&opts, &["speed", "brightness"])?;
                    let speed = opt(&opts, "speed", d.vendor_speed, |v| parse_in(v, effects::VENDOR_SPEED))?;
                    let make: Make = Box::new(move || Ok(Box::new(effects::Vendor::new(fx, speed)) as _));
                    (make, brightness(&opts, &d)?)
                }
            };
            stream(make, percent)
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

/// Opens the keyboard and runs one stored write (which guards itself with the write-ahead marker).
fn write(op: impl FnOnce(&Keyboard) -> Result<()>) -> Result<()> {
    op(&Keyboard::open()?)
}

/// Streams a live effect at ~30 fps until Ctrl-C, mirroring the GUI worker's rules.
fn stream(make: Make, percent: u8) -> Result<()> {
    let k = Keyboard::open()?;
    let mut fx = make()?;
    let gamma = saved_curve();
    let leds: Vec<LedIndex> = (0..KEYS.len()).filter_map(LedIndex::of_key).collect();
    let mut busy = 0;
    let _ = writeln!(std::io::stderr(), "streaming, Ctrl-C to stop");
    while !STOP.load(SeqCst) {
        let next = Instant::now() + Duration::from_millis(33);
        let colors = fx.frame();
        let frame = RtFrame::new(
            &leds
                .iter()
                .copied()
                .zip(colors.into_iter().map(|c| effects::shown(c, fx.raw(), gamma, percent)))
                .collect::<Vec<_>>(),
        );
        match k.stream(&frame) {
            Ok(Sent::Shown) => busy = 0,
            Ok(Sent::Busy) => {
                busy += 1;
                ensure!(busy < 30, "keyboard kept declining live frames (nothing was stored)");
            }
            Err(e) => {
                k.mark_interrupted();
                return Err(e);
            }
        }
        let label = fx.label();
        if !label.is_empty() {
            let _ = write!(std::io::stderr(), "\r{label}   ");
        }
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    let _ = writeln!(std::io::stderr(), "\nstopped");
    Ok(())
}

/// The window's "Colour curve", read from its saved settings so the CLI looks the same; 1.0 if
/// there are none. The file is user-editable: anything odd falls back to 1.0.
fn saved_curve() -> f32 {
    let saved = eframe::storage_dir("aula-control")
        .and_then(|d| std::fs::read_to_string(d.join("app.ron")).ok())
        .and_then(|s| ron::from_str::<std::collections::HashMap<String, String>>(&s).ok())
        .and_then(|kv| ron::from_str::<Saved>(kv.get(eframe::APP_KEY)?).ok());
    saved.map_or(1.0, |s| effects::valid_curve(s.curve))
}

fn parse_light(effect: &str, opts: &[String]) -> Result<Light> {
    let norm = |s: &str| s.to_lowercase().replace([' ', '-', '_'], "");
    let mode = Mode::all()
        .find(|m| norm(m.name()) == norm(effect))
        .with_context(|| format!("unknown effect `{effect}` (see `modes`)"))?;
    let mut rainbow = false;
    let mut pairs = vec![];
    for o in opts {
        if o == "rainbow" {
            rainbow = true;
        } else {
            pairs.push(o.clone());
        }
    }
    let opts = kv(&pairs)?;
    known(&opts, &["color", "brightness", "speed", "dir"])?;
    let has = |k: &str| opts.iter().any(|(o, _)| o == k);
    ensure!(!rainbow || mode.has_rainbow(), "{} has no rainbow option", mode.name());
    ensure!(
        !has("color") || mode.has_color(),
        "{} doesn't take a colour",
        mode.name()
    );
    ensure!(!has("speed") || mode.has_speed(), "{} has no speed", mode.name());
    ensure!(
        !has("dir") || mode.has_horizontal() || mode.has_vertical(),
        "{} has no direction",
        mode.name()
    );
    let d = Saved::default();
    let dir_default = Direction::ALL[d.direction as usize];
    Ok(Light {
        mode,
        rgb: opt(&opts, "color", d.rgb, parse_hex)?,
        rainbow,
        brightness: Level::new(opt(&opts, "brightness", d.brightness, |v| parse_in(v, 0..=Level::MAX))?),
        speed: Level::new(opt(&opts, "speed", d.speed, |v| parse_in(v, 0..=Level::MAX))?),
        direction: opt(&opts, "dir", dir_default, |v| match v {
            "left" => Ok(Direction::Left),
            "right" => Ok(Direction::Right),
            "up" => Ok(Direction::Up),
            "down" => Ok(Direction::Down),
            _ => bail!("dir must be left, right, up or down"),
        })?,
    })
}

/// The `brightness=` option shared by every live effect (percent).
fn brightness(opts: &[(String, String)], d: &Saved) -> Result<u8> {
    opt(opts, "brightness", d.live_brightness, |v| parse_in(v, LIVE_BRIGHTNESS))
}

fn kv(opts: &[String]) -> Result<Vec<(String, String)>> {
    let mut seen = std::collections::HashSet::new();
    opts.iter()
        .map(|o| {
            let (k, v) = o
                .split_once('=')
                .with_context(|| format!("expected key=value, got `{o}`"))?;
            ensure!(seen.insert(k.to_string()), "`{k}` given twice");
            Ok((k.to_string(), v.to_string()))
        })
        .collect()
}

fn known(opts: &[(String, String)], allowed: &[&str]) -> Result<()> {
    match opts.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        Some((k, _)) => bail!("unknown option `{k}` (allowed: {})", allowed.join(", ")),
        None => Ok(()),
    }
}

fn opt<T>(opts: &[(String, String)], key: &str, default: T, parse: impl Fn(&str) -> Result<T>) -> Result<T> {
    match opts.iter().find(|(k, _)| k == key) {
        Some((_, v)) => parse(v).with_context(|| format!("bad {key}=`{v}`")),
        None => Ok(default),
    }
}

/// Rejects (rather than clamps) out-of-range values: a script should fail loudly.
fn parse_in<T: std::str::FromStr + PartialOrd + std::fmt::Debug>(
    v: &str,
    range: std::ops::RangeInclusive<T>,
) -> Result<T> {
    let x: T = v.parse().ok().context("not a number")?;
    ensure!(range.contains(&x), "must be in {range:?}");
    Ok(x)
}

fn parse_hex(v: &str) -> Result<[u8; 3]> {
    let v = v.strip_prefix('#').unwrap_or(v);
    ensure!(
        v.len() == 6 && v.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected RRGGBB"
    );
    let n = u32::from_str_radix(v, 16)?;
    Ok([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn light_args() {
        let l = parse_light("rolling", &args("color=#00ff80 brightness=2 dir=left rainbow")).unwrap();
        assert_eq!(
            (l.mode.name(), l.rgb, l.brightness.get(), l.direction, l.rainbow),
            ("Rolling", [0, 255, 128], 2, Direction::Left, true)
        );
        assert!(
            parse_light("single-on", &args("dir=up")).is_err(),
            "Single On has no direction"
        );
        assert!(parse_light("nope", &[]).is_err());
        assert!(
            parse_light("static", &args("brightness=6")).is_err(),
            "out of range is rejected, not clamped"
        );
        assert!(
            parse_light("static", &args("colour=ff0000")).is_err(),
            "typos are caught"
        );
        assert!(parse_light("static", &args("color=12345")).is_err());
        assert!(parse_light("static", &args("color=+12345")).is_err(), "no sign tricks");
        assert!(
            parse_light("static", &args("color=ff0000 color=00ff00")).is_err(),
            "duplicates rejected"
        );
        assert!(
            parse_light("spectrum", &args("color=ff0000")).is_err(),
            "Spectrum takes no colour"
        );
        assert!(parse_light("static", &args("speed=2")).is_err(), "Static has no speed");
        assert_eq!(
            parse_light("static", &[]).unwrap().rgb,
            Saved::default().rgb,
            "same defaults as the GUI"
        );
        // These fail on argument checks, before any device access.
        assert!(dispatch(&args("screen x.gif")).is_err(), "screen needs --yes");
        assert!(dispatch(&args("light")).is_err());
        assert!(dispatch(&args("live disco")).is_err());
        assert!(dispatch(&args("live spring speed=16")).is_err(), "vendor speed range");
        assert!(dispatch(&args("live spring brightness=5")).is_err(), "brightness range");
        assert!(dispatch(&args("live music low=red")).is_err(), "colours are RRGGBB");
        assert!(
            dispatch(&args("live pomodoro low=ff0000")).is_err(),
            "music option on pomodoro"
        );
    }
}
