mod fleet;
mod pills;
mod rapt;

use anyhow::Result;
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration as WifiConfiguration};
use esp32_nimble::{BLEDevice, BLEScan};
use esp_idf_svc::hal::task::block_on;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::client::{EspHttpConnection, Configuration as HttpClientConfiguration};
use embedded_svc::http::client::Client;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::EspWifi;
use log::{error, info, warn};
use serde::Serialize;
use std::time::{Duration, Instant};

/// Secrets and per-install settings. Read from `cfg.toml` at build time (see `cfg.toml.example`).
#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_pass: &'static str,
    #[default("")]
    brewfather_stream_id: &'static str,
    /// `ADDRESS=Name;ADDRESS=Name`. The name is the Brewfather device name for that Pill.
    /// Leave empty for discovery mode: nothing is posted, and every Pill heard is logged.
    #[default("")]
    pills: &'static str,
}

// Fail the build, not the device, if cfg.toml is missing or incomplete.
const _: () = assert!(
    !CONFIG.wifi_ssid.is_empty() && !CONFIG.brewfather_stream_id.is_empty(),
    "cfg.toml is missing or incomplete: copy cfg.toml.example to cfg.toml and fill it in"
);

/// Length of one BLE scan window. Scanning is continuous: windows run back to back, and the
/// main loop only gets control between them (to log and to post).
const SCAN_MS: i32 = 30_000;
/// Log a "still scanning" line every this many scan windows (10 x 30 s = 5 min), so an unattended
/// board shows it is alive and whether it hears anything at all.
const HEARTBEAT_WINDOWS: u32 = 10;
/// How long to wait for Wi-Fi (link + IP address) after asking it to (re)connect.
const WIFI_WAIT: Duration = Duration::from_secs(15);
/// After this many posts fail in a row, assume the Wi-Fi/DNS state is stuck and reconnect.
const FAILURES_BEFORE_RECONNECT: u32 = 3;

#[derive(Serialize, Debug)]
struct BrewfatherPayload {
    name: String,
    temp: f32,
    gravity: f32,
    temp_unit: String,
    gravity_unit: String,
}

fn main() -> Result<()> {
    // Patch patches for ESP-IDF runtime
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("Starting ESP32 RAPT Pill to Brewfather Bridge");

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // 1. Initialize and connect Wi-Fi
    let mut wifi = EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?;
    wifi.set_configuration(&WifiConfiguration::Client(ClientConfiguration {
        ssid: CONFIG.wifi_ssid.try_into().unwrap(),
        password: CONFIG.wifi_pass.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))?;

    wifi.start()?;
    info!("Connecting to Wi-Fi...");
    // Keep trying until the network is up (e.g. the router may boot slower than we do).
    while !ensure_wifi(&mut wifi) {
        warn!("Wi-Fi not up yet; retrying");
    }

    // 2. Main loop: scan continuously, remember the newest reading from each configured Pill,
    // and post each Pill's reading whenever fleet::POST_INTERVAL has passed since its last successful
    // post. Scanning never pauses for the 15-minute wait, so a Pill burst can't fall into a gap
    // between scans.
    let mut fleet = fleet::Fleet::new(
        pills::parse(CONFIG.pills).map_err(|e| anyhow::anyhow!("invalid `pills` in cfg.toml: {e}"))?,
    );
    if fleet.is_empty() {
        warn!(
            "DISCOVERY MODE: no pills configured, so nothing will be posted. Wake your Pill \
             (double-tap it) and copy its address from the log into `pills` in cfg.toml."
        );
    }
    for (addr, name) in fleet.configured() {
        info!("Configured Pill {} -> Brewfather device {:?}", fmt_addr(addr), name);
    }
    let mut post_failures: u32 = 0;
    let (mut windows, mut adverts_total, mut pill_sightings) = (0u32, 0u32, 0u32);

    let ble_device = BLEDevice::take();
    info!("Scanning continuously for RAPT Pill BLE advertisements...");
    loop {
        let (sightings, adverts) = scan_window(ble_device);
        windows += 1;
        adverts_total += adverts;
        pill_sightings += sightings.len() as u32;
        if windows % HEARTBEAT_WINDOWS == 0 {
            info!(
                "Still scanning: {adverts_total} Bluetooth advertisements heard in the last {} min, \
                 {pill_sightings} RAPT Pill sightings",
                HEARTBEAT_WINDOWS as i32 * SCAN_MS / 60_000
            );
            (adverts_total, pill_sightings) = (0, 0);
        }
        for s in sightings {
            match fleet.on_heard(s.addr, s.reading, Instant::now()) {
                // Every sighting (once per 30 s window per Pill) is routine, so only the first one
                // after boot is shown by default; the rest are debug-level.
                fleet::Heard::Listed { name, first } => {
                    let level = if first { log::Level::Info } else { log::Level::Debug };
                    log::log!(
                        level,
                        "RAPT Pill {:?} [{}] rssi {}: {:.2} C, SG {:.4}, battery {}%",
                        name,
                        fmt_addr(&s.addr),
                        s.rssi,
                        s.reading.temp_c,
                        s.reading.gravity,
                        s.reading.battery_pct
                    );
                }
                fleet::Heard::UnlistedNew => warn!(
                    "Found RAPT Pill {} (rssi {}, {:.2} C, SG {:.4}) but it is not listed; \
                     to post it add `{}=Name` to `pills` in cfg.toml",
                    fmt_addr(&s.addr),
                    s.rssi,
                    s.reading.temp_c,
                    s.reading.gravity,
                    fmt_addr(&s.addr)
                ),
                fleet::Heard::UnlistedAgain => {}
            }
        }

        for name in fleet.drop_stale(Instant::now()) {
            warn!("Last reading from {name:?} is stale; waiting for a new one");
        }

        // Only touch Wi-Fi when a post is actually due, so a reconnect never delays scanning
        // needlessly. If the link is down, try to bring it back and retry after the next window.
        let due = fleet.due(Instant::now());
        if due.is_empty() || !ensure_wifi(&mut wifi) {
            continue;
        }

        for post in due {
            match send_to_brewfather(&post.name, post.reading.temp_c, post.reading.gravity) {
                Ok(_) => {
                    info!(
                        "Successfully pushed {:?} to Brewfather: {:.2} C, SG {:.4}, battery {}%",
                        post.name, post.reading.temp_c, post.reading.gravity, post.reading.battery_pct
                    );
                    fleet.mark_posted(post.idx, Instant::now());
                    post_failures = 0;
                }
                Err(e) => {
                    error!("Failed to push {:?} to Brewfather: {e}", post.name);
                    post_failures += 1;
                    if post_failures >= FAILURES_BEFORE_RECONNECT {
                        warn!("{post_failures} posts failed in a row; resetting the Wi-Fi connection");
                        let _ = wifi.disconnect();
                        post_failures = 0;
                        break; // ensure_wifi() reconnects before the next attempt
                    }
                }
            }
        }
    }
}

/// True if the station is associated and has an IP address.
fn wifi_up(wifi: &EspWifi<'_>) -> bool {
    wifi.is_connected().unwrap_or(false)
        && wifi
            .sta_netif()
            .get_ip_info()
            .map(|i| i.ip != core::net::Ipv4Addr::UNSPECIFIED)
            .unwrap_or(false)
}

/// Make sure Wi-Fi is up, (re)connecting if it isn't. Returns false if it still isn't up after
/// `WIFI_WAIT`; the caller should try again later. ESP-IDF does not reconnect by itself after a
/// disconnect, so without this a dropped link would leave the board offline until power-cycled.
fn ensure_wifi(wifi: &mut EspWifi<'_>) -> bool {
    if wifi_up(wifi) {
        return true;
    }
    warn!("Wi-Fi is down; (re)connecting...");
    if let Err(e) = wifi.connect() {
        // Typically "already connecting"; keep waiting below.
        warn!("wifi.connect() returned {e:?}");
    }
    let deadline = Instant::now() + WIFI_WAIT;
    while Instant::now() < deadline {
        if wifi_up(wifi) {
            if let Ok(ip) = wifi.sta_netif().get_ip_info() {
                info!("Wi-Fi connected successfully! IP address: {}", ip.ip);
            }
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// One Pill heard during a scan window.
struct Sighting {
    addr: pills::Addr,
    rssi: i8,
    reading: rapt::Reading,
}

fn fmt_addr(a: &pills::Addr) -> String {
    format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", a[0], a[1], a[2], a[3], a[4], a[5])
}

/// Listen for one scan window and return the newest reading from each RAPT Pill heard in it.
fn scan_window(ble_device: &BLEDevice) -> (Vec<Sighting>, u32) {
    let mut heard: Vec<Sighting> = Vec::new();
    let mut total_adverts: u32 = 0;

    let result = block_on(async {
        let mut scan = BLEScan::new();
        // Passive scan: all the data we need is in the advertisement itself.
        // Duplicates must not be filtered: the Pill sends several packet types from one address.
        scan.active_scan(false).filter_duplicates(false).interval(100).window(99);

        // This callback runs on the small NimBLE host task: no logging or formatting in here,
        // or it overflows the stack. Just decode and store. Returning None keeps the scan
        // running for the full window.
        scan.start(ble_device, SCAN_MS, |device, data| {
            total_adverts += 1;
            let mfg = data.manufacture_data()?;
            if mfg.company_identifier == rapt::COMPANY_ID {
                if let Some(reading) = rapt::parse(mfg.payload) {
                    let entry = Sighting { addr: device.addr().as_be_bytes(), rssi: device.rssi(), reading };
                    match heard.iter_mut().find(|h| h.addr == entry.addr) {
                        Some(existing) => *existing = entry,
                        None => heard.push(entry),
                    }
                }
            }
            None::<()>
        })
        .await
    });

    if let Err(e) = result {
        error!("BLE scan failed: {e:?}");
        std::thread::sleep(Duration::from_secs(1));
    }
    (heard, total_adverts)
}

fn send_to_brewfather(name: &str, temp: f32, gravity: f32) -> Result<()> {
    let payload = BrewfatherPayload {
        name: name.to_string(),
        temp,
        gravity,
        temp_unit: "C".into(),
        gravity_unit: "G".into(),
    };

    let json_body = serde_json::to_string(&payload)?;

    let settings = HttpClientConfiguration {
        use_global_ca_store: true,
        ..Default::default()
    };
    
    // Initialize the client using the defined HTTP configuration settings
    let mut client = Client::wrap(EspHttpConnection::new(&settings)?);
    let headers = [("Content-Type", "application/json")];
    
    let url = format!("http://log.brewfather.net/stream?id={}", CONFIG.brewfather_stream_id);
    let mut request = client.post(&url, &headers)?;
    request.write(json_body.as_bytes())?;
    let response = request.submit()?;

    let status = response.status();
    info!("Brewfather API response status: {status}");

    if status != 200 {
        warn!("Non-200 response received from Brewfather Custom Stream endpoint");
    }

    Ok(())
}