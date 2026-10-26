//! Command-line interface: `aula-control <command> [key=value ...]`. Without arguments the GUI starts.
//!
//! Same safety rules as the GUI: only the validated protocol calls, one holder of the keyboard
//! at a time (see `Keyboard::open`), and a failed write marks the board as interrupted so no
//! process touches it again before a replug. Ctrl-C never cuts a transaction in half.

use crate::effects::{self, Effect};
use crate::layout::KEYS;
use crate::proto::{Animation, Direction, Keyboard, LedIndex, Level, Light, Mode, RtFrame, Sent};
use anyhow::{Context, Result, bail, ensure};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: aula-control [command]          (no command: open the window)

  modes                                list lighting effects
  light <effect> [color=RRGGBB] [brightness=0-5] [speed=0-5] [dir=left|right|up|down] [rainbow]
  clock                                set the screen clock to local time
  screen <image> --yes                 upload a PNG/JPG/GIF to the screen (replaces the stored one)
  live music [device=NAME] [sensitivity=0.5-3]
  live pomodoro [focus=5-90] [break=1-30]
                                       stream until Ctrl-C (nothing is stored on the keyboard)";

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
            let light = parse_light(effect, opts)?;
            write(|k| k.set_light(&light))?;
            println!("lighting applied");
            Ok(())
        }
        "clock" => {
            no_extra(rest)?;
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
            let frames = crate::load_frames(std::path::Path::new(path))?;
            let anim = Animation::from_rgba(&frames)?;
            write(|k| k.upload_screen(&anim, |p| eprint!("\ruploading {p:3}%")))?;
            eprintln!();
            println!("screen uploaded ({} frames)", frames.len());
            Ok(())
        }
        "live" => {
            let (kind, opts) = rest.split_first().context("live: music or pomodoro?")?;
            let opts = kv(opts)?;
            let mut fx: Box<dyn Effect> = match kind.as_str() {
                "music" => {
                    let sens = opt(&opts, "sensitivity", 1.2, |v| parse_in(v, 0.5..=3.0))?;
                    let device = opts.iter().find(|(k, _)| k == "device").map(|(_, v)| v.as_str());
                    known(&opts, &["sensitivity", "device"])?;
                    Box::new(effects::Music::new(device, sens)?)
                }
                "pomodoro" => {
                    let focus = opt(&opts, "focus", 25, |v| parse_in(v, 5..=90))?;
                    let rest = opt(&opts, "break", 5, |v| parse_in(v, 1..=30))?;
                    known(&opts, &["focus", "break"])?;
                    Box::new(effects::Pomodoro::new(focus, rest))
                }
                other => bail!("live: unknown effect `{other}` (music or pomodoro)"),
            };
            stream(fx.as_mut())
        }
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

/// Opens the keyboard, runs one write; on failure marks the board interrupted (until replug).
fn write(op: impl FnOnce(&Keyboard) -> Result<()>) -> Result<()> {
    let k = Keyboard::open()?;
    op(&k).inspect_err(|_| k.mark_interrupted())
}

/// Streams a live effect at ~30 fps until Ctrl-C, mirroring the GUI worker's rules.
fn stream(fx: &mut dyn Effect) -> Result<()> {
    let k = Keyboard::open()?;
    let leds: Vec<LedIndex> = (0..KEYS.len()).filter_map(LedIndex::of_key).collect();
    let mut busy = 0;
    eprintln!("streaming, Ctrl-C to stop");
    while !STOP.load(SeqCst) {
        let next = Instant::now() + Duration::from_millis(33);
        let colors = fx.frame();
        let frame = RtFrame::new(&leds.iter().copied().zip(colors).collect::<Vec<_>>());
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
            eprint!("\r{label}   ");
        }
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    eprintln!("\nstopped");
    Ok(())
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
    Ok(Light {
        mode,
        rgb: opt(&opts, "color", [0xff, 0x9a, 0x1f], parse_hex)?,
        rainbow,
        brightness: Level::new(opt(&opts, "brightness", Level::MAX, |v| parse_in(v, 0..=Level::MAX))?),
        speed: Level::new(opt(&opts, "speed", 3, |v| parse_in(v, 0..=Level::MAX))?),
        direction: opt(&opts, "dir", Direction::Right, |v| match v {
            "left" => Ok(Direction::Left),
            "right" => Ok(Direction::Right),
            "up" => Ok(Direction::Up),
            "down" => Ok(Direction::Down),
            _ => bail!("dir must be left, right, up or down"),
        })?,
    })
}

fn kv(opts: &[String]) -> Result<Vec<(String, String)>> {
    opts.iter()
        .map(|o| {
            let (k, v) = o
                .split_once('=')
                .with_context(|| format!("expected key=value, got `{o}`"))?;
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
    let v = v.trim_start_matches('#');
    ensure!(v.len() == 6, "expected RRGGBB");
    let n = u32::from_str_radix(v, 16)?;
    Ok([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

fn no_extra(rest: &[String]) -> Result<()> {
    ensure!(rest.is_empty(), "unexpected arguments: {}", rest.join(" "));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn light_args() {
        let l = parse_light("single-on", &args("color=#00ff80 brightness=2 dir=up rainbow")).unwrap();
        assert_eq!(
            (l.mode.name(), l.rgb, l.brightness.get(), l.direction, l.rainbow),
            ("Single On", [0, 255, 128], 2, Direction::Up, true)
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
        // These fail on argument checks, before any device access.
        assert!(dispatch(&args("screen x.gif")).is_err(), "screen needs --yes");
        assert!(dispatch(&args("light")).is_err());
        assert!(dispatch(&args("live disco")).is_err());
    }
}
