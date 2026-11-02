# redpill

ESP32 firmware that listens for [RAPT Pill](https://rapt.io) hydrometer Bluetooth broadcasts and
forwards each Pill's temperature and gravity to [Brewfather](https://brewfather.app) over Wi-Fi,
using Brewfather's Custom Stream integration. It runs on its own: plug it into power near your
fermenter and it keeps posting.

```
RAPT Pill ──BLE──▶ ESP32 ──Wi-Fi──▶ Brewfather (Custom Stream)
```

- Supports one or more Pills. Each Pill shows up in Brewfather as its own device with a name you choose.
- Scans Bluetooth continuously and posts each Pill's latest reading every 15 minutes (Brewfather's limit).
- Sends temperature (°C) and gravity (SG). Battery is **not** sent: Brewfather's battery field is in
  volts and the Pill reports a percentage.

## What you need

- A **Pill** set to broadcast over **Bluetooth** (the mode used with a RAPT temperature controller),
  not Wi-Fi-only. In that mode it sends one short burst on its configured interval (15 minutes in our
  testing), so after the first reading the board posts about every 15 minutes.
- An **original ESP32** board (the Xtensa dual-core chip, e.g. ESP32-DevKitC / ESP-WROOM-32) with
  4 MB flash and a USB port. Developed and tested on a board with a CP210x USB-serial chip.
  Other chips (ESP32-S3, -C3, ...) are **not** supported as-is.
- A **2.4 GHz Wi-Fi network** (the ESP32 can't use 5 GHz).
- A **Brewfather** account with the Custom Stream integration enabled.
- A computer to build and flash from: Linux, or Windows with WSL2 (Ubuntu). macOS should work but is untested.

## 1. Get your Brewfather stream ID

In Brewfather, enable the Custom Stream integration (Settings → Integrations → Custom Stream; menu
names may differ slightly). It gives you a URL like:

```
http://log.brewfather.net/stream?id=AbCdEfGhIjKlMn
```

The part after `id=` is your **stream ID**.

## 2. One-time toolchain setup

Rust for the ESP32 needs a special toolchain. On Ubuntu/Debian:

```bash
# System packages (needs sudo)
sudo apt update && sudo apt install -y build-essential pkg-config libudev-dev libssl-dev \
  python3-pip python3-venv cmake ninja-build git curl wget flex bison gperf libffi-dev \
  dfu-util libusb-1.0-0

# Rust itself, if you don't have it
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# ESP tooling. Run these from your home directory, NOT inside this project: the project pins
# the `esp` toolchain, so cargo refuses to run in it until that toolchain exists.
cd ~
cargo install espup ldproxy espflash
espup install --toolchain-version 1.98.1.0
```

`espup install` writes `~/export-esp.sh`. **Run `. ~/export-esp.sh` in every new terminal** before
using `cargo` here (or add that line to `~/.bashrc`).

> The toolchain version matters. 1.97.0.0 has a broken `rust-src` (no `library/unwind`) and fails
> with `failed to load manifest ... unwind/Cargo.toml`.

## 3. Configure

```bash
cp cfg.toml.example cfg.toml
```

Edit `cfg.toml` (it is gitignored; it's compiled into the firmware at build time):

```toml
[redpill]
wifi_ssid = "your-network-name"
wifi_pass = "your-wifi-password"
brewfather_stream_id = "AbCdEfGhIjKlMn"
pills = ""
```

The section header must stay `[redpill]`. The build fails with a clear message if
`cfg.toml` is missing or incomplete.

### Finding your Pill's address

Leave `pills = ""` for your first flash. That is **discovery mode**: the board posts nothing and
logs every RAPT Pill it hears. After flashing (step 4), wake the Pill by **double-tapping** it and
watch the log for:

```
Found RAPT Pill FC:E8:C0:5D:74:76 (rssi -55, 24.56 C, SG 1.2771) but it is not listed; to post it add FC:E8:C0:5D:74:76=Name to `pills` in cfg.toml
```

Copy that address into `pills`, give it a name, then rebuild and reflash:

```toml
pills = "FC:E8:C0:5D:74:76=Yellow Pill 1"
```

For several Pills, separate entries with semicolons:

```toml
pills = "FC:E8:C0:5D:74:76=Yellow Pill 1;AA:BB:CC:DD:EE:FF=Red Pill 2"
```

- The **name is the Brewfather device name.** Each name must be unique, and each gets its own
  15-minute post limit.
- **Renaming creates a new Brewfather device.** Old readings stay under the old name.
- Pills **not listed** are ignored (their address is logged once), so a neighbour's Pill never ends up in your stream.
- You can also see a Pill's address with a phone BLE scanner such as nRF Connect, on Android.
  On iPhone it shows a random ID instead of the address, which won't work here.

## 4. Build and flash

Plug the ESP32 in over USB, then:

```bash
. ~/export-esp.sh
cargo build        # first build is slow: it downloads and compiles ESP-IDF
espflash flash --baud 115200 --port /dev/ttyUSB0 target/xtensa-esp32-espidf/debug/redpill
```

(`cargo run` builds, flashes and opens the log in one step, but uses a faster baud rate that
can fail; see Troubleshooting.) `cargo build --release` gives a smaller binary but has not been tested.

On Linux your user must be in the `dialout` group to open the port:
`sudo usermod -aG dialout $USER`, then log out and back in.

### Windows / WSL2: attach the USB port first

WSL2 can't see USB devices by default. Install [usbipd-win](https://github.com/dorssel/usbipd-win)
on Windows, then in an **Administrator PowerShell**:

```powershell
winget install --interactive --exact dorssel.usbipd-win   # once; reopen PowerShell after
usbipd list                                               # find the "CP210x" or "CH340" row
usbipd bind --busid <BUSID>                               # once per device
usbipd attach --wsl --busid <BUSID> --auto-attach         # leave this window open
```

Then `ls /dev/ttyUSB*` in WSL should show `/dev/ttyUSB0`. Restarting WSL (`wsl --shutdown`)
detaches the board; `--auto-attach` re-attaches it.

## 5. Watch it work

```bash
espflash monitor --baud 115200 --port /dev/ttyUSB0
```

Opening the monitor resets the board. A healthy start looks like:

```
Starting ESP32 RAPT Pill to Brewfather Bridge
Configured Pill FC:E8:C0:5D:74:76 -> Brewfather device "Yellow Pill 1"
Scanning continuously for RAPT Pill BLE advertisements...
Wi-Fi connected successfully! IP address: 192.168.x.x
RAPT Pill "Yellow Pill 1" [FC:E8:C0:5D:74:76] rssi -55: 24.56 C, SG 1.2771, battery 82%
Brewfather API response status: 200
Successfully pushed "Yellow Pill 1" to Brewfather: 24.56 C, SG 1.2771, battery 82%
```

Every 5 minutes a heartbeat line shows the board is alive and what it hears:
`Still scanning: 8393 Bluetooth advertisements in the last 5 min; RAPT Pill: 4 readings, 2 other packets`.
("Other packets" are RAPT-format packets the board can't use, such as the Pill's second packet type.)

After that, each Pill is posted at most once per 15 minutes. A reading that has gone 20 minutes
without an update is dropped rather than posted stale.

## How it behaves

- **The Pill only broadcasts on its own schedule.** Between bursts the board hears nothing from it, so a quiet log is normal; the periodic `Still scanning` line shows it is listening. Double-tapping the Pill makes it transmit for a while, which is handy for testing. Since the board remembers the latest reading, a burst that arrives just before the 15-minute post interval is posted a few seconds later, not lost.
- **RAPT Brew won't log by itself in Bluetooth-only mode.** That is expected: the Pill has no Wi-Fi/cloud path in that mode. This bridge is what carries its readings to Brewfather.
- **Keep the board in range.** Roughly a few metres. Liquid and the fermenter walls weaken the
  signal; put the board close to the fermenter.
- **Gravity is only meaningful with the Pill floating in liquid.** On a desk the Pill reports a
  tilt-derived value (about 1.27). Its first packets after waking can also carry wild gravity values
  (e.g. 86.4); anything outside 0.9-1.3 SG is discarded.
- **No Pill heard** means nothing is posted. There is no "keep last value" resend.
- **Wi-Fi/Brewfather failures** are logged (`Failed to push ... to Brewfather`) and retried after the next scan window.
- **Scanning never depends on Wi-Fi.** Bluetooth scanning runs on its own thread, so a slow or failed post, a Wi-Fi outage, or a router that boots slower than the board can't make it miss a Pill. The board checks Wi-Fi every second and reconnects every 10 seconds while it is down; after three failed posts in a row it also resets its Wi-Fi connection. (Written but not yet tested against a real outage.)

## Troubleshooting

| Symptom | Fix |
|---|---|
| `custom toolchain 'esp' ... is not installed` | Run `espup install --toolchain-version 1.98.1.0`. Install tools from outside the project dir. |
| `can't find crate for core` | Run `. ~/export-esp.sh`, then build again. |
| `cfg.toml is missing or incomplete` | `cp cfg.toml.example cfg.toml` and fill it in. |
| `Permission denied` opening `/dev/ttyUSB0` | Add yourself to `dialout`, then re-login (or `wsl --shutdown`). |
| No `/dev/ttyUSB0` in WSL | Attach it with `usbipd` (see above). |
| Flash fails partway: `FlashDeflData ... Other (0x0)` | Use `--baud 115200` and just retry; it sometimes needs a second try. |
| `Timeout while running ReadReg command` | Flaky USB link. Retry, or unplug/replug the board and re-attach. |
| `Found RAPT Pill ... but it is not listed` | Not a fault. Add `ADDRESS=Name` to `pills` to post it. |
| Pill never appears in the log | Double-tap it, move it closer, check it shows up in the RAPT app. |
| Nothing arrives in Brewfather | Check the stream ID, that the log shows `Successfully pushed`, and look for a **new device** under the name you configured. |
| Reboot loop with `A stack overflow in task ...` | Developer issue; see `CLAUDE.md` (stack sizes in `sdkconfig.defaults`). |

## Security

Your Wi-Fi password and stream ID are compiled into the firmware and stored in the ESP32's flash in
plain text. Don't share a built binary, and wipe the board before giving it away
(`espflash erase-flash`). `cfg.toml` is gitignored; keep it that way.

## Status

- Verified on hardware: Wi-Fi, continuous BLE scanning, decoding a **v2** Pill (temperature, gravity
  and battery matched the RAPT app, including SG 1.002 with the Pill floating in liquid), and posting
  to Brewfather under the configured device name. Wi-Fi (re)connect works at startup.
- Not verified: **more than one Pill**, v1-format Pills, release builds, and **Wi-Fi recovery after an
  outage while running**.

For how the code is organised, and how to run the host-side unit tests (`./scripts/host-test.sh`), see `CLAUDE.md`.
