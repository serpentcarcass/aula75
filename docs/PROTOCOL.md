# AULA F75 Max USB protocol

Reverse engineered with Ghidra from the vendor's Windows driver, `AULA F75 Max Gasket Mechanical Keyboard Driver 1.0.0.5` (`DeviceDriver.exe` + `mui.dll`). Function addresses refer to that build. Everything below is implemented in [`src/proto.rs`](../src/proto.rs) and was checked against the decompiled code, and against disassembly where Ghidra's decompiler dropped code.

## Device

| | |
|---|---|
| USB ID | `0c45:800a` (SONiX), product string `AULA F75Max`, firmware (bcdDevice) `0x0110` |
| Interface 0 | Boot keyboard |
| Interface 1 | Consumer / system controls |
| Interface 2 | Vendor usage page **`0xFF68`**: 4096-byte output reports, used for **screen data** |
| Interface 3 | Vendor usage page **`0xFF13`**: 64-byte feature reports, used for **commands** |

The driver bundles hidapi (`hid_send_feature_report` with a fixed length of 65 = report id 0 + 64 bytes).

## Command packets

All commands are 64-byte feature reports on `0xFF13`:

```
04 <cmd> 00 00 00 00 00 00 <arg> 00 … 00
```

A **step** is: set the feature report, wait (35 ms, from the vendor layout file's `cmd_delaytime`), then get the feature report. The reply echoes `04 <cmd>`. Byte 3 of the reply is a status:

| Status | Meaning |
|---|---|
| `01` | OK |
| `00` | OK for `04 02` |
| `FF` | Busy or error |

Data packets that follow a command are sent with "set" only.

Writes are wrapped in a transaction:

```
04 18            begin
04 <cmd> <arg>   command header
<data packets>   (set only)
04 02            apply / commit
04 F0            finish (lighting and per-key only)
```

## Commands

### `04 13`: lighting effect (FUN_0042b0d0)

The header arg is `01`. It is followed by one data packet:

| Byte | Value |
|---|---|
| 0 | Mode: `0` off, `1`–`19` effects (see below), `0x80` user lighting |
| 1–3 | R, G, B |
| 8 | Rainbow (0/1) |
| 9 | Brightness 0–5 |
| 10 | Speed 0–5 |
| 11 | Direction: 0 left, 1 right, 2 up, 3 down |
| 14–15 | `AA 55` |

Modes, as defined in the driver's default table (FUN_0042b380). Feature bits: `01` brightness, `02` speed, `04` left/right, `08` up/down, `10` rainbow toggle, `20` colour.

| id | name | bits | id | name | bits |
|---|---|---|---|---|---|
| 1 | Static | `31` | 11 | Rolling | `37` |
| 2 | Single On | `33` | 12 | Rotating | `37` |
| 3 | Single Off | `33` | 13 | Explode | `33` |
| 4 | Glittering | `33` | 14 | Launch | `33` |
| 5 | Falling | `33` | 15 | Ripples | `33` |
| 6 | Colourful | `03` | 16 | Flowing | `37` |
| 7 | Breath | `33` | 17 | Pulsating | `33` |
| 8 | Spectrum | `03` | 18 | Tilt | `37` |
| 9 | Outward | `33` | 19 | Shuttle | `33` |
| 10 | Scrolling | `3b` | | | |

### `04 23`: stored per-key colours (FUN_0044ba00)

This is two transactions:

1. A lighting write with mode `0x80` and the brightness.
2. `04 23` with arg `09`, followed by 576 bytes (9 packets).

In the 576 bytes, the record for LED `i` sits **at offset `i*4`** as `[i, R, G, B]`, and bytes 574–575 are `AA 55`. LED indices come from the vendor layout (`light_index`, at most 121). Colours are COLORREF (R in the low byte), which was verified in `mui.dll` `DrawControl`.

### `04 20`: real-time frame (FUN_004329c0, transport FUN_0044f0a0)

This is used for streaming. The vendor streams it continuously for its "Real-time Lighting" and "Music Rhythm" pages, with no `04 18` / `04 F0` and no mode switch beforehand.

1. Header `04 20` with arg `08`: set, wait 5 ms, get. On `FF`, resend once. Data is sent only if the status is `01`.
2. 512 bytes in 8 packets, 2 ms apart. The driver aborts if this takes longer than 100 ms. The records are `[led, R, G, B]` **packed back to back** (not at `led*4`), zero padded, with no trailer.
3. `04 02`: set, wait 5 ms, get. On `FF`, resend once. The driver ignores the final status.

### `04 28`: screen clock (FUN_004238f0)

The header arg is `01`. It is followed by one data packet, which **is** read back:

| Byte | Value |
|---|---|
| 0 | `00` |
| 1 | `01` (slot) |
| 2 | `5A` |
| 3 | Year % 100 |
| 4–8 | Month, day, hour, minute, second |
| 10 | Weekday (0 = Sunday) |
| 62–63 | `AA 55` |

The packet ends with `04 02`. There is no `04 F0`.

### `04 72`: screen animation upload (FUN_00422930, FUN_0044f2b0)

1. Header `04 72`: byte 2 = slot (`01`), bytes 8–9 = page count, u16 little-endian.
2. The pages, each 4096 bytes, sent as output reports on interface `0xFF68` (report id 0, so 4097 bytes on the wire). After each page the driver waits up to 300 ms for an input report and ignores it.
3. `04 02`.

Blob layout:

```
[0]        frame count (1..255)
[1+i]      frame i delay in ms / 2
[256..]    frames, 128×128 RGB565 little-endian, top-down rows
padding    0xFF up to a multiple of 4096
```

## Seen but not used

The following appear in the driver but are **not implemented**. Their meaning is unconfirmed.

| Command | Where | Likely purpose |
|---|---|---|
| `04 11` / `04 27` | FUN_004183d0 | Key remapping for the base layer and the Fn layer |
| `04 F5` arg `09` | FUN_0042aeb0 | Reads stored per-key colours back |
| `04 15`, `04 19` | FUN_0042d6d0 | Unknown, probably macros |
| `04 17` | FUN_00414020 | Unknown |
| `04 2b` | FUN_00418020 | Unknown |

Firmware update and factory reset paths exist in the driver too. They were deliberately not examined for use.
