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

## 3. Configure (optional)

Nothing has to be configured before flashing: Wi-Fi, the Brewfather stream ID and your Pills are all
set on the board itself afterwards (step 5). If you would rather bake in defaults for a development
board, copy `cfg.toml.example` to `cfg.toml` (gitignored) and fill it in:

```toml
[redpill]
wifi_ssid = "your-network-name"
wifi_pass = "your-wifi-password"
brewfather_stream_id = "AbCdEfGhIjKlMn"
pills = "FC:E8:C0:5D:74:76=Yellow Pill 1;AA:BB:CC:DD:EE:FF=Red Pill 2"
setup_pass = "redpill1"
```

These values only **seed an empty board**: they are copied into the board's flash on first boot,
after which whatever is saved on the board wins. To start over with new `cfg.toml` values, erase
the saved settings with `espflash erase-flash` and flash again.

`setup_pass` is the password of the setup Wi-Fi network (8-63 characters, default `redpill1`).

## 4. Build and flash

Plug the ESP32 in over USB, then:

```bash
. ~/export-esp.sh
cargo build        # first build is slow: it downloads and compiles ESP-IDF
espflash flash --baud 115200 --port /dev/ttyUSB0 target/xtensa-esp32-espidf/debug/redpill
```

(`cargo run` builds, flashes and opens the log in one step, but uses a faster baud rate that
can fail; see Troubleshooting.) `cargo build --release` gives a smaller binary (about 1.7 MB) and builds; it has not been run on a board.

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

## 5. First-time setup (Wi-Fi, Brewfather, Pills)

Everything is set up from a phone or computer. No `cfg.toml` and no reflashing.

### Step 1: connect the board to your Wi-Fi

A board with no saved Wi-Fi starts in **setup mode** and creates its own Wi-Fi network:

- Name: **`redpill-setup-XXXX`** (the last digits of the board's address; a board that already has a
  name uses it, e.g. `redpill-setup`).
- Password: **`redpill1`** (set `setup_pass` in `cfg.toml` before flashing to change it).

On your phone:

1. Open Wi-Fi settings and join the setup network. It will say "no internet", which is expected.
2. A "sign in to network" page may open by itself. If it does not, open a browser and go to
   **`http://192.168.71.1`**.
3. Pick your home network from the list. If it is missing (or hidden), press **Rescan** or type its
   name in the box below. Use a **2.4 GHz** network; the ESP32 cannot use 5 GHz. Enter the password
   and press **Connect**.
4. **Your phone will disconnect from the setup network at this point. That is normal**: the board
   switches to your Wi-Fi's channel while it joins. It saves your Wi-Fi and its name, then restarts
   about 30 seconds later. If it cannot join (wrong password), nothing is saved and the page tells
   you why; reconnect to the setup network and try again.
5. Rejoin your home Wi-Fi on the phone. The board is now on your network.

With nothing saved, setup mode stays open until you finish. (On a board that already has Wi-Fi, it
closes by itself after 10 minutes; see "Changing Wi-Fi later".)

> **Android: allow local network access.** Newer Android versions stop apps from reaching devices
> on your home network unless you allow it. If the board's page never loads on your Android phone
> (Chrome shows `ERR_TOO_MANY_RETRIES`), look in your phone's settings for the **local network** or
> **nearby devices** permission for your browser and allow it. On a Pixel it was in the phone's app
> permission settings, not inside Chrome's own menu. A computer on the same network is not affected.
> The setup network itself worked without this.

### Step 2: open the board's settings page

Browse to **`http://redpill.local`** from a computer, iPhone or Mac. On an Android phone use the
board's **IP address** instead (see below), because `.local` names often don't work on Android.

To find the IP address: look for the board in your router's app (it appears as `redpill`, made by
Espressif), or read the `This board is http://redpill.local/ (http://192.168.x.x/)` line in the
serial log (step 6).

### Step 3: Brewfather and your Pills

On the settings page:

1. Under **Brewfather**, paste your stream ID (or the whole Custom Stream URL from step 1) and
   press **Save**. Nothing is posted until this is set.
2. Wake your Pill by **double-tapping** it. Within a minute or so it appears under **Heard nearby**,
   with its address, signal strength and current reading.
3. Type a name for it and press **Add**. That name is the **Brewfather device name**. The board
   posts that Pill's reading right away, then every 15 minutes.

Everything on this page applies immediately, with no restart, and is kept across reboots and power
cuts. Come back whenever you add a Pill or want to rename or remove one.

- **Each name must be unique, and renaming creates a new Brewfather device.** Old readings stay
  under the old name. Each device gets its own 15-minute post limit.
- Pills **not named** here are ignored, so a neighbour's Pill never ends up in your stream.
- A Pill that is asleep can still be added by its address under **Add a Pill by address**.
- Add each Pill to **one** board only, or Brewfather receives its readings twice.

### The board's name: redpill.local, redpill2.local, ...

The first board on a network is `redpill.local`. A second board, set up later, finds that name
taken and becomes `redpill2.local`, and so on up to `redpill9`. The name is saved, so it is the same
after every reboot and power cut. Each board's page lists the other redpill boards it can see, and
you can rename a board on its settings page. If you see a message that a board's name changed,
another device had claimed it first.

`.local` names work on iPhone, Mac, Windows 10+ (tested) and Linux (with Avahi). They often do not
work in Android browsers; use the IP address there.

### Changing Wi-Fi later

- On the settings page, press **Change Wi-Fi network**, or
- hold the board's **BOOT** button for 3 seconds while it is running.

Either restarts it in setup mode, with the setup network named after the board (e.g.
`redpill2-setup`). Follow step 1 again. Your Pills and Brewfather settings are kept. Because the
board already has Wi-Fi saved, setup mode closes by itself after 10 minutes and the board carries on
with the old network.

If the board **cannot join Wi-Fi at all** within 5 minutes of power-up (wrong password, new
router), it opens the setup network automatically while it keeps trying the saved one. A router
outage while the board is already running does not do this.

## 6. Watch it work

The serial log is optional now, but handy:

```bash
espflash monitor --baud 115200 --port /dev/ttyUSB0
```

Opening the monitor resets the board. A healthy start looks like:

```
Starting ESP32 RAPT Pill to Brewfather Bridge
Configured Pill FC:E8:C0:5D:74:76 -> Brewfather device "Yellow Pill 1"
Scanning continuously for RAPT Pill BLE advertisements...
Wi-Fi connected successfully! IP address: 192.168.x.x
This board is http://redpill.local/ (http://192.168.x.x/)
RAPT Pill "Yellow Pill 1" [FC:E8:C0:5D:74:76] rssi -55: 24.56 C, SG 1.2771, battery 82%
Brewfather API response status: 200
Successfully pushed "Yellow Pill 1" to Brewfather: 24.56 C, SG 1.2771, battery 82%
```

Every 5 minutes a heartbeat line shows the board is alive and what it hears:
`Still scanning: 8393 Bluetooth advertisements in the last 5 min; RAPT Pill: 4 readings, 2 other packets; free heap ...`.
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
| `Permission denied` opening `/dev/ttyUSB0` | Add yourself to `dialout`, then re-login (or `wsl --shutdown`). |
| No `/dev/ttyUSB0` in WSL | Attach it with `usbipd` (see above). |
| Flash fails partway: `FlashDeflData ... Other (0x0)` | Use `--baud 115200` and just retry; it sometimes needs a second try. |
| `Timeout while running ReadReg command` | Flaky USB link. Retry, or unplug/replug the board and re-attach. |
| `Found RAPT Pill ... but it is not named yet` | Not a fault. Name it on the settings page to post it. |
| Pill never appears under *Heard nearby* | Double-tap it, move it closer, check it shows up in the RAPT app. |
| Phone: the page never loads (`ERR_TOO_MANY_RETRIES`), but a PC can open it | Android can block apps from reaching devices on your home network. In your phone's settings, allow the **local network / nearby devices** permission for your browser (on a Pixel it is in the app permission settings, outside Chrome's own menu), or try another browser. The board is not at fault. |
| `redpill.local` does not open | Use the IP address (shown in the log and on the setup "Connected" page). Check your phone/PC is on the same network, not a guest network that isolates devices. |
| The setup network never appears | A board with saved Wi-Fi only opens it on request (BOOT held 3 s, or the settings page button) or after 5 minutes without ever connecting. |
| Nothing posted, log says no stream id | Enter the Brewfather stream ID on the settings page. |
| Nothing arrives in Brewfather | Check the stream ID, that the log shows `Successfully pushed`, and look for a **new device** under the name you configured. |
| Reboot loop with `A stack overflow in task ...` | Developer issue; see `CLAUDE.md` (stack sizes in `sdkconfig.defaults`). |

## Security

Your Wi-Fi password and stream ID are stored in the ESP32's flash in plain text (and compiled into
the firmware if you used `cfg.toml`). Don't share a built binary, and wipe the board before giving it
away (`espflash erase-flash`). `cfg.toml` is gitignored; keep it that way.

The settings page has **no login**: anyone on your home network can open it and change the Pill
names or the stream ID. It never shows or changes the Wi-Fi password except through setup mode,
which is protected by the setup network's password. Change `setup_pass` if you do not want the
default.

## Status

- Verified on hardware: Wi-Fi, continuous BLE scanning on its own thread, decoding a **v2** Pill
  (temperature, gravity and battery matched the RAPT app, including SG 1.002 with the Pill floating
  in liquid), and posting to Brewfather under the configured device name. Also verified with the
  internet blocked while Wi-Fi stayed up (the router answered with its own page, HTTP 405): the board
  kept the reading, retried every 30 seconds, and posted as soon as the internet came back, without
  a reboot.
- **Setup and settings page, verified on hardware:** first-boot seeding, joining the setup network
  from a phone, picking a network and joining it (the board saved its Wi-Fi and name and restarted),
  the settings page from a PC and from a phone (after allowing Android's local-network permission),
  the `redpill.local` name from Windows, adding / renaming / removing Pills, renaming the board,
  and persistence across reboots. **Not yet tried:** the automatic setup network after 5 minutes
  without Wi-Fi, the BOOT-button restart, two boards on one network, and a live Pill reading
  through the new settings page.
- Not verified: **more than one Pill**, v1-format Pills, and **recovery when the Wi-Fi connection
  itself drops while running** (the router tests only blocked internet access).

For how the code is organised, and how to run the host-side unit tests (`./scripts/host-test.sh`), see `CLAUDE.md`.
