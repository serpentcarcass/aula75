# aula75

**Unofficial control app for the AULA F75 Max keyboard on macOS and Linux.**
Written in Rust with an [egui](https://github.com/emilk/egui) interface. The vendor only ships a Windows driver, so the USB protocol was reverse engineered from it with Ghidra ([protocol notes](docs/PROTOCOL.md)).

![Lighting tab](docs/screenshots/lighting.png)

## Features

| | |
|---|---|
| **Lighting** | All 19 built-in effects plus "off", with colour, rainbow, brightness, speed and direction. Each effect only shows the settings it supports, and an animated preview approximates it on screen. |
| **Per-key** | Paint individual keys and store them in the keyboard's user-lighting slot. |
| **Live** | Effects computed on your computer and streamed in real time. Nothing is written to the keyboard's memory. <br>• **Music**: a spectrum visualiser driven by system audio (whatever is playing) or a microphone <br>• **Pomodoro**: F1–F12 burn down as a timer, and the board pulses when the phase changes |
| **Screen** | Upload a PNG, JPG or animated GIF (up to 255 frames) to the 128×128 TFT. |
| **Clock** | Set the TFT clock to your computer's local time. |

<p>
  <img src="docs/screenshots/per-key.png" width="49%" alt="Per-key tab">
  <img src="docs/screenshots/live.png" width="49%" alt="Live tab">
</p>

## Built to not brick your keyboard

The tool only knows the commands it needs, and every byte it sends was checked against the vendor driver's code.

- **Allowlist only.** Firmware update, factory reset, key remapping and macros are deliberately **not implemented**, and there is no way to send a raw command byte.
- **Validated values.** Effects, levels, directions, LED indices and screen frames can only be built from checked types, never from arbitrary numbers.
- **No half-written settings.** Every step must be acknowledged. If one fails, the write stops before the final "apply". Writes then stay **locked until you unplug and replug** the keyboard, so a later command can't commit the leftovers.
- **Live effects never touch flash.** They use only the real-time command the vendor's own music mode streams, never the command that stores colours.
- **Known hardware only.** The app refuses to write unless the device is a wired F75 Max (`0c45:800a`) with firmware it was verified against (`0x0110`). It also refuses if two F75s are connected.
- **One writer.** A single worker thread owns the device, so commands never interleave.

> [!WARNING]
> This is a reverse-engineered, unofficial tool. It is careful, but you use it at your own risk.

## Install

You need Rust **1.88+** ([rustup](https://rustup.rs)).

```sh
git clone https://github.com/serpentcarcass/aula75.git
cd aula75
cargo run --release
```

### macOS

No extra setup is needed. For **Music → System audio**, macOS 14.6 or newer asks once for permission to record system audio.

### Linux

> [!NOTE]
> Linux support is written but **has not been tested on real hardware yet**. Reports are welcome.

Build dependencies (Debian/Ubuntu names):

```sh
sudo apt install build-essential pkg-config clang libudev-dev libpipewire-0.3-dev \
  libxkbcommon-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

Give your user access to the keyboard's two vendor interfaces. The typing interface stays root-only, so other programs can't read your keystrokes:

```sh
sudo cp 70-aula-f75max.rules /etc/udev/rules.d/
sudo udevadm control --reload && sudo udevadm trigger
```

**System audio** capture uses PipeWire and records what your default output is playing.

## Usage

The keyboard has to be connected **by USB cable**. The 2.4 GHz dongle and Bluetooth are not supported. The bottom bar shows whether the keyboard is connected, plus the current tab's main action.

- **Lighting**: pick an effect, adjust it, then press **Apply lighting**.
- **Per-key**: drag to paint and right-click a key to pick up its colour, then press **Apply colours**.
- **Live**: choose Music or Pomodoro, then **Start live** and **Stop**. Settings take effect when you start.
- **Screen**: drop an image onto the window, confirm the checkbox, then press **Upload to screen**.
- **Clock**: press **Sync clock**.

If you see *"A previous write was interrupted"*, unplug the keyboard for a few seconds and plug it back in.

## Development

```sh
cargo test                                         # packet layouts, effects, safety checks
cargo test -- --ignored system_audio --nocapture   # manual: prints live spectrum while audio plays
cargo clippy --all-targets
```

| File | What's in it |
|---|---|
| `src/proto.rs` | USB/HID protocol, validated types and the command allowlist |
| `src/main.rs` | UI and the device worker thread |
| `src/effects.rs` | Live effects (spectrum analysis, Pomodoro) |
| `src/layout.rs` | Key positions and LED indices, taken from the vendor layout file |

## Not affiliated

AULA and Epomaker are trademarks of their respective owners. This project is not affiliated with or endorsed by them, and it contains none of their software.
