# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`redpill`: ESP32 firmware in Rust (std, on ESP-IDF via `esp-idf-svc`). It reads RAPT Pill hydrometer BLE advertisements and forwards temperature and gravity readings to a Brewfather Custom Stream endpoint over Wi-Fi. Wi-Fi, the stream id and the Pill names are configured on the board itself (setup portal + settings web page), not in the firmware image.

**Current state:** Wi-Fi, BLE scanning (NimBLE) and the Brewfather HTTP POST all run on hardware. The setup portal, settings page and `redpill.local` naming (see Architecture) compile, pass clippy, have host unit tests, and were exercised on a board, including a phone joining the setup network and a real Wi-Fi join (see "Tested on hardware" below). The RAPT payload decoder (`src/rapt.rs`) is verified against a real v2 Pill (temp/SG/battery matched the RAPT app). SG was also confirmed in liquid (1.002 matched the app). A Pill sitting out of liquid reports tilt-derived SG (about 1.27 on a desk), and its first packets after waking carry junk gravity (e.g. 86.4), which `rapt::parse` rejects outside 0.9-1.3.

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

`cargo test` doesn't work: this crate always builds for the xtensa target. The hardware-independent modules (`rapt`, `pills`, `fleet`, `settings`, `dns`, `pages`) have unit tests that run on the host instead:

```bash
./scripts/host-test.sh          # compiles those files with stable rustc --test and runs them
```

`main.rs`, `store.rs`, `names.rs` and `web.rs` (Wi-Fi, BLE, NVS, mDNS, HTTP) can only be tested on the board. Keep logic that doesn't need ESP APIs in the testable modules, and take the clock as a parameter (`Instant`) rather than calling `Instant::now()` inside them.

## Architecture

Boot decides a mode from NVS (`store.rs`): **setup mode** when no Wi-Fi network is saved or setup was requested (web page button, or BOOT held 3 s while running), otherwise **normal mode**.

**Normal mode** (`run_normal` in `src/main.rs`): threads that share small structs behind mutexes.
- **Scan thread** (`scan_forever`, 8 KB stack): brings up Bluetooth via `BLEDevice::take()` and runs `esp32_nimble::BLEScan` in `SCAN_MS` (10 min) scans that restart immediately. The callback runs on NimBLE's own host task: it only counts, decodes with `rapt::parse` and pushes a `Sighting` (keyed by BLE address, `as_be_bytes()`, the order it is printed in). It never touches Wi-Fi or HTTP and never logs.
- **Main thread** (16 KB stack, `LOOP_TICK` 1 s): starts Wi-Fi without waiting for it, then loops: non-blocking Wi-Fi supervision (`station_ip()`, reconnect every `WIFI_RETRY` when down, because ESP-IDF does not reconnect on its own), drain the queue into a `fleet::Fleet`, apply Pill-list edits from the settings page (`LanState::dirty` -> `Fleet::set_configured`), refresh what the settings page shows, the 5-minute "Still scanning" heartbeat (includes free heap), drop stale readings, and post whatever `Fleet::due` returns with `send_to_brewfather(stream_id, name, ...)` (only while Wi-Fi is up and a stream id is set). After `FAILURES_BEFORE_RECONNECT` consecutive failed posts it calls `wifi.disconnect()` so the supervision rebuilds the connection (a DNS failure, `getaddrinfo() returns 202`, was seen once with the link up).
- **LAN thread** (`lan_services`, 16 KB stack: 8 KB overflowed in debug builds while starting the HTTP server, started the first time Wi-Fi gets an IP): picks and announces the board name over mDNS, serves the settings page (`web::start_lan_server`), then every 60 s re-checks the name and lists other boards. Nothing here is needed for posting; failures are only logged.
- **Button thread**: polls GPIO0; held for 3 s it saves a one-shot `portal` flag in NVS and restarts into setup mode.
- Because scanning is independent of Wi-Fi, a slow or failed post, a reconnect, or an offline router cannot make the board miss a Pill burst, and scanning starts about 2 s after boot. The Bluetooth stack is initialised before `wifi.connect()` so the two radios never initialise at once.
- **Fallback portal**: if Wi-Fi has not connected even once within `settings::WIFI_BOOT_GRACE` (5 min) of boot (`settings::fallback_portal_due`), the setup network and portal open alongside normal mode (Wi-Fi in AP+STA, reconnect interval slows to 60 s) and close when the station gets an IP. Outages after a first successful connection never open it.

**Phone gotcha (cost hours):** a phone's browser that cannot load the settings page or portal with `ERR_TOO_MANY_RETRIES`, even for the router's address, is Android's local-network permission, not the board. Proved by a bare TCP listener on the board that never saw the phone's connection while a PC reached it. Check the permission before debugging the firmware.

**Tested on hardware so far:** seeding NVS from `cfg.toml`, mDNS name claim (`redpill.local`), the settings page over the LAN (add / rename / remove / validation / HTML escaping, hostname rename, persistence across reboot), 5+ minutes of normal scanning with the heartbeat, and entering setup mode from the page (AP `redpill-setup` up at `192.168.71.1`, DNS and HTTP servers running). Also verified: a phone joining the setup network, the Wi-Fi form with a scanned network list, the join (credentials and name saved, restart into normal mode), and the settings page from a phone. Not tried: a second board, the fallback portal (5 min without Wi-Fi), the BOOT button, a live Pill reading with the new code.

**Setup mode** (`run_setup`): no Bluetooth, no posting. Wi-Fi in AP+STA; the AP is `<hostname>-setup` (or `redpill-setup-<mac suffix>` before a name exists), WPA2 with `setup_pass`. `web::Portal` runs a catch-all DNS responder (UDP 53, `dns::answer`) and an HTTP server whose `/*` redirects to the AP's own address (`192.168.71.1`, the esp-idf-svc default, read from the AP netif by `ap_ip()`; do not hard-code it). The form handler only records the request in `PortalShared`; the main thread owns the Wi-Fi driver, so it does the join (`attempt_join`): it tries the network for `JOIN_TIMEOUT`, and only on success saves the credentials and the claimed name to NVS, shows the address for `PORTAL_LINGER`, then restarts. A failed join saves nothing and restores the previous Wi-Fi config. A portal opened on purpose on a board that already has Wi-Fi restarts itself after 10 min.

**Modules**
- `src/fleet.rs`: all per-Pill bookkeeping, no ESP dependencies, clock passed in. `Fleet::on_heard` stores the newest reading for a configured Pill and remembers unlisted Pills (capped at 16, expiring with `MAX_READING_AGE`) so the settings page can offer to name them; `set_configured` replaces the list live (kept Pills keep their timers; a renamed Pill is due at once because it is a new Brewfather device; an unlisted Pill that gets added posts its remembered reading immediately); `status`/`unlisted` feed the page; `drop_stale`, `due` (`POST_INTERVAL` 15 min since that Pill's last successful post, per Pill), `mark_posted`. `is_success_status` decides what counts as delivered: only 2xx. Anything else (a router's "device paused" page answers 405, a captive portal, a rate limit, an error) keeps the reading and is retried; treating any response as success once dropped a reading and stalled posting for 15 minutes. A failed post (`mark_failed`) is not offered again for `POST_RETRY_DELAY` (30 s). `SightingQueue` is the cross-thread hand-off: newest sighting per address, at most 16 addresses.
- `src/settings.rs`: pure. `Settings`, form decoding (`parse_form`), validators (Wi-Fi, Brewfather stream id: accepts the bare id or the whole URL, host names), the `redpill`, `redpill2`..`redpill9` sequence (`next_hostname`, `claim_hostname` with an injected "is it taken" callback), setup SSID naming, `fallback_portal_due`.
- `src/pills.rs`: `pills::parse`/`format` for the `ADDRESS=Name;ADDRESS=Name` text (the stored format), `validate_name` (no `;`, no control characters, max 40), `parse_address`. Rejects bad addresses, empty names, duplicate addresses or names. Pills not in the list are ignored so a neighbour's Pill can't end up in your stream.
- `src/rapt.rs`: pure decoder for the RAPT Pill manufacturer payload (23 bytes, big-endian, v1 and v2 share the temp/gravity/battery offsets). Battery is a percentage, so it is not sent to Brewfather, whose `battery` field is volts.
- `src/dns.rs`, `src/pages.rs`: pure packet building and HTML. Everything user- or air-supplied goes through `pages::escape`.
- `src/store.rs` (NVS namespace `redpill`; keys `ssid`, `pass`, `stream`, `pills`, `hostname`, `portal`), `src/names.rs` (mDNS name claiming and announcing), `src/web.rs` (portal and settings handlers): ESP-dependent glue. HTTP handlers run on the httpd task and never touch the `Fleet`: they validate, save to NVS, update `LanState.settings` and set `dirty`, then wait (up to 2 s) for the main loop to apply it so the page they return is fresh.

**Board name**: first board on a LAN is `redpill.local`, the next `redpill2.local`, ... up to `redpill9`; the choice is saved in NVS and reused every boot. A board queries a name (`mdns_query_a`, twice) before claiming it, so the one that already holds a name keeps it and only a newcomer moves on (with a banner on its page). The LAN thread re-checks that the mDNS component did not rename itself after losing a probe. A hand-chosen name is validated by `settings::validate_hostname` and checked free on the LAN. The DHCP host name is set from the stored name before connecting. Max length 24 so `<name>-setup` fits a 32-byte SSID.

`send_to_brewfather()` serializes `BrewfatherPayload` with serde_json and POSTs it via `EspHttpConnection` wrapped in `embedded_svc::http::client::Client`.

## Configuration and secrets

Everything is stored in NVS on the board and edited through the setup portal (Wi-Fi) and the settings page (stream id, Pill names, board name); there is no authentication on the settings page (home-LAN device). Wi-Fi password and stream id are in flash in plain text. `cfg.toml` (gitignored, optional, section `[redpill]`, read at build time by `toml-cfg` into `CONFIG` in `main.rs`) only **seeds an empty device** on first boot (`seed_from_build_config`): `wifi_ssid`, `wifi_pass`, `brewfather_stream_id`, `pills`, plus `setup_pass` (password of the setup network, default `redpill1`). The build no longer fails without it. To re-seed, erase NVS (`espflash erase-flash`). `cfg.toml.example` is the template.

## ESP-IDF configuration

`sdkconfig.defaults` raises the main task stack to 16384 bytes (8192 overflowed once BLE scanning and the HTTP post were combined; symptom: `A stack overflow in task main` right after a Pill reading) and enables BLE with the NimBLE host (Bluedroid off), which `esp32-nimble` requires. `CONFIG_BT_NIMBLE_HOST_TASK_STACK_SIZE=8192` is needed because the `BLEScan` callback runs on the `nimble_host` task. Logging or formatting inside the callback overflows even that stack (symptom: `A stack overflow in task nimble_host` and a reboot loop), so the callback only counts, decodes and queues, and the main thread does all logging. To debug what is on the air, collect data in the callback and print it from the main thread. `CONFIG_LWIP_MAX_SOCKETS=16` with the HTTP server's `max_open_sockets: 8` (`web::http_config`): with the default 4, Chrome's parallel connections made the server drop live keep-alive sockets and the browser failed with `ERR_TOO_MANY_RETRIES` (curl never showed it; reproduce by opening 6 keep-alive sockets at once). `CONFIG_HTTPD_MAX_REQ_HDR_LEN=1024` / `CONFIG_HTTPD_MAX_URI_LEN=512` are for phones' long captive-portal probe requests. The HTTP server task gets a 10 KB stack (`web::HTTP_STACK`), the portal DNS thread 8 KB (4 KB overflowed in debug builds), the LAN thread 16 KB; pages are built on the heap. Std threads show up as `pthread` in a stack-overflow message. Changes to `sdkconfig.defaults` only take effect after `cargo clean -p esp-idf-sys`, which rebuilds ESP-IDF and is slow.

mDNS is an external ESP-IDF 5 component: `Cargo.toml` pulls `espressif/mdns` via `[[package.metadata.esp-idf-sys.extra_components]]` (esp-idf-svc's `mdns` module is only compiled when it is present). The first build downloads it and writes `components_esp32.lock`, which should be committed like `Cargo.lock`.

Crate versions are tightly coupled: `esp-idf-svc` 0.53 (re-exports `hal` and `sys`, so there are no separate `esp-idf-hal` or `esp-idf-sys` deps), `embedded-svc` 0.29, `esp32-nimble` 0.13 and `embuild` 0.33. Upgrade them together. Older `esp32-nimble` (0.9) does not compile on the current Rust toolchain.
