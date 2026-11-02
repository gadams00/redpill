# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`redpill`: ESP32 firmware in Rust (std, on ESP-IDF via `esp-idf-svc`). It is meant to read RAPT Pill hydrometer BLE advertisements and forward temperature, gravity and battery readings to a Brewfather Custom Stream endpoint over Wi-Fi.

**Current state:** Wi-Fi, BLE scanning (NimBLE) and the Brewfather HTTP POST all run on hardware. The RAPT payload decoder (`src/rapt.rs`) is verified against a real v2 Pill (temp/SG/battery matched the RAPT app). SG was also confirmed in liquid (1.002 matched the app). A Pill sitting out of liquid reports tilt-derived SG (about 1.27 on a desk), and its first packets after waking carry junk gravity (e.g. 86.4), which `rapt::parse` rejects outside 0.9-1.3.

## Toolchain and commands

- Target: `xtensa-esp32-espidf` (original ESP32, Xtensa). This is the default build target in `.cargo/config.toml`, so you don't pass `--target`.
- Needs the `esp` Rust toolchain (from `espup`; see `rust-toolchain.toml`), plus `ldproxy` and `espflash` on PATH.
- Run `. ~/export-esp.sh` in every new shell before `cargo` (it sets up the Xtensa GCC/LLVM paths).
- Install tools (`cargo install ...`, `espup install`) from outside this directory. `rust-toolchain.toml` forces the `esp` toolchain here, so those commands fail until it exists.
- `.cargo/config.toml` sets `build-std = ["std", "panic_abort"]`, because Xtensa has no prebuilt `std`. It also pins `MCU=esp32` and `ESP_IDF_VERSION=v5.5.1`. `esp-idf-svc` 0.53's BLE module needs ESP-IDF 5.4 or newer, so don't lower this.
- The `esp` toolchain is 1.98.1.0 (`espup install --toolchain-version 1.98.1.0`). The 1.97.0.0 release has a `rust-src` without `library/unwind`, which breaks `build-std`.
- Flashing: the default 460800 baud fails partway through over usbipd/WSL2 (`FlashDeflData ... Other (0x0)`). Use `espflash flash --baud 115200 --port /dev/ttyUSB0 <elf>`. The user must be in the `dialout` group, and the board must be attached with `usbipd attach --wsl --busid 1-10`.
- `embuild` (in `build.rs`) downloads and builds ESP-IDF on the first build. That first build is slow and needs network access.

```bash
cargo build            # build firmware
cargo build --release
cargo run              # build, flash, and open serial monitor (runner = espflash flash --monitor)
espflash monitor       # serial monitor only
cargo clippy
cargo fmt
```

`cargo test` doesn't work: this crate always builds for the xtensa target. The hardware-independent modules (`rapt`, `pills`, `fleet`) have unit tests that run on the host instead:

```bash
./scripts/host-test.sh          # compiles those three files with stable rustc --test and runs them
```

`main.rs` (Wi-Fi, BLE, HTTP) can only be tested on the board. Keep logic that doesn't need ESP APIs in the testable modules, and take the clock as a parameter (`Instant`) rather than calling `Instant::now()` inside them.

## Architecture

Two source files:

- `src/main.rs`: ESP-IDF setup, Wi-Fi station connect (WPA2, then polls `sta_netif().get_ip_info()` for a DHCP IP), then an infinite loop that scans continuously. `scan_window()` runs a passive `esp32_nimble::BLEScan` (via `block_on`) for `SCAN_MS` (30 s), filters on manufacturer data with company id `0x4152`, and returns the newest valid reading from each Pill heard, keyed by BLE address (`as_be_bytes()`, the same order it is printed in). The loop feeds each reading to a `fleet::Fleet` and posts whatever `Fleet::due` returns with `send_to_brewfather(name, ...)` (15 minutes between posts per Pill, Brewfather's limit of 1 post per device name). Scanning never sleeps, because a Pill may transmit only briefly about every 15 minutes and a sleeping board would miss it. ESP-IDF does not reconnect Wi-Fi on its own, so `ensure_wifi()` runs at startup (retries until the network is up) and before any due post (reconnects if the link or IP is gone, waiting up to `WIFI_WAIT`), and after `FAILURES_BEFORE_RECONNECT` consecutive failed posts the loop calls `wifi.disconnect()` so the next `ensure_wifi()` rebuilds the connection. A DNS failure (`getaddrinfo() returns 202`, `ESP_ERR_HTTP_CONNECT`) was seen once with the link apparently still up. The startup reconnect works on hardware; recovery from an outage while running has not been tested.
- `src/fleet.rs`: all per-Pill bookkeeping, with no ESP dependencies and the clock passed in. `Fleet::on_heard` stores the newest reading for a configured Pill (and reports an unlisted Pill once), `drop_stale` forgets readings older than `MAX_READING_AGE` (20 min), `due` returns the readings to post now (`POST_INTERVAL` of 15 min since that Pill's last successful post, judged per Pill), and `mark_posted` records a success. A failed post is simply offered again next time. Covered by host tests that include mutation-checked cases for per-Pill timers, names, stale and re-post handling.
- `src/pills.rs`: parses the `pills` config string (`ADDRESS=Name;ADDRESS=Name`) and rejects bad addresses, empty names, and duplicate addresses or names. No ESP dependencies, so it can be tested on the host with plain `rustc`. An empty list is valid (discovery mode). Pills not in the list are ignored (their address is logged once) so a neighbour's Pill can't end up in your stream.
- `src/rapt.rs`: pure decoder for the RAPT Pill manufacturer payload (23 bytes, big-endian, v1 and v2 share the temp/gravity/battery offsets). It has no ESP dependencies, so it can be compiled and tested on the host with plain `rustc`. Battery is reported as a percentage, so it is not sent to Brewfather, whose `battery` field is volts.

`send_to_brewfather()` serializes `BrewfatherPayload` with serde_json and POSTs it via `EspHttpConnection` wrapped in `embedded_svc::http::client::Client`. A non-200 response only logs a warning.

## Configuration and secrets

Wi-Fi SSID/password, the Brewfather stream id, and the Pill list live in `cfg.toml` (gitignored), read at build time by the `toml-cfg` crate into `CONFIG` in `main.rs`. `cfg.toml.example` is the committed template. The values are compiled into the firmware. The section header must match the package name (`[redpill]`). The build fails with a message if `cfg.toml` is missing or incomplete, and `build.rs` reruns when it changes. The Brewfather device name for each Pill comes from the `pills` entry. To find a Pill's address, run the firmware (an empty `pills` is valid and means discovery mode: nothing is posted), double-tap the Pill, read the address from the "Found RAPT Pill ... but it is not listed" log line, and add `ADDRESS=Name` to `pills`.

## ESP-IDF configuration

`sdkconfig.defaults` raises the main task stack to 16384 bytes (8192 overflowed once BLE scanning and the HTTP post were combined; symptom: `A stack overflow in task main` right after a Pill reading) and enables BLE with the NimBLE host (Bluedroid off), which `esp32-nimble` requires. `CONFIG_BT_NIMBLE_HOST_TASK_STACK_SIZE=8192` is needed because the `BLEScan` callback runs on the `nimble_host` task. Logging or formatting inside the callback overflows even that stack (symptom: `A stack overflow in task nimble_host` and a reboot loop), so `scan_for_pill()` only decodes inside the callback and logs after the scan returns. To debug what is on the air, collect data in the callback and print it outside. Changes to `sdkconfig.defaults` only take effect after `cargo clean -p esp-idf-sys`, which rebuilds ESP-IDF and is slow.

Crate versions are tightly coupled: `esp-idf-svc` 0.53 (re-exports `hal` and `sys`, so there are no separate `esp-idf-hal` or `esp-idf-sys` deps), `embedded-svc` 0.29, `esp32-nimble` 0.13 and `embuild` 0.33. Upgrade them together. Older `esp32-nimble` (0.9) does not compile on the current Rust toolchain.
