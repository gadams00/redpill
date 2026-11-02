mod fleet;
mod pills;
mod rapt;

use anyhow::Result;
use embedded_svc::http::client::Client;
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration as WifiConfiguration};
use esp32_nimble::{BLEDevice, BLEScan};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::task::block_on;
use esp_idf_svc::http::client::{Configuration as HttpClientConfiguration, EspHttpConnection};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::EspWifi;
use log::{error, info, warn};
use serde::Serialize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
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

/// Length of one BLE scan. The scan thread restarts it immediately when it ends, so this only
/// sets how often (a few milliseconds of) restart gap occurs.
const SCAN_MS: i32 = 10 * 60 * 1000;
/// Stack for the scan thread. The scan callback itself runs on NimBLE's own task, not this one.
const SCAN_THREAD_STACK: usize = 8192;
/// How long to wait for the scan thread to bring up the Bluetooth stack.
const BLE_INIT_WAIT: Duration = Duration::from_secs(10);
/// Pause between passes of the main loop.
const LOOP_TICK: Duration = Duration::from_secs(1);
/// Minimum time between Wi-Fi (re)connect attempts.
const WIFI_RETRY: Duration = Duration::from_secs(10);
/// Log a "still scanning" line this often, so an unattended board shows it is alive and what it hears.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);
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

/// State shared between the Bluetooth scan thread (producer) and the main thread (consumer).
#[derive(Default)]
struct Shared {
    /// Newest decoded sighting per Pill, waiting for the main thread.
    pending: Mutex<fleet::SightingQueue>,
    /// Every Bluetooth advertisement heard from any device, since the last heartbeat.
    adverts: AtomicU32,
    /// RAPT readings decoded successfully, since the last heartbeat.
    pill_readings: AtomicU32,
    /// Packets with the RAPT company id that were not usable (other packet types, junk gravity).
    pill_other: AtomicU32,
}

fn main() -> Result<()> {
    // Patch patches for ESP-IDF runtime
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("Starting ESP32 RAPT Pill to Brewfather Bridge");

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

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // Wi-Fi is configured and started here, but not waited for: scanning must not depend on it.
    let mut wifi = EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?;
    wifi.set_configuration(&WifiConfiguration::Client(ClientConfiguration {
        ssid: CONFIG.wifi_ssid.try_into().unwrap(),
        password: CONFIG.wifi_pass.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))?;
    wifi.start()?;

    // Bluetooth scanning runs on its own thread, started before Wi-Fi even connects. Wait for it
    // to finish bringing up the Bluetooth stack before starting the Wi-Fi connection, so the two
    // radios are never initialised at the same moment.
    let shared = Arc::new(Shared::default());
    let (ready_tx, ready_rx) = mpsc::channel();
    {
        let shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("ble-scan".into())
            .stack_size(SCAN_THREAD_STACK)
            .spawn(move || scan_forever(shared, ready_tx))?;
    }
    if ready_rx.recv_timeout(BLE_INIT_WAIT).is_err() {
        warn!("Bluetooth did not report ready within {BLE_INIT_WAIT:?}; continuing anyway");
    }
    info!("Scanning continuously for RAPT Pill BLE advertisements...");

    // Main loop: keep Wi-Fi up (never blocking), feed new sightings to the fleet, and post
    // whatever is due. Everything slow happens here, so it can never make the scan thread miss
    // a Pill burst.
    info!("Connecting to Wi-Fi...");
    let mut wifi_was_up = false;
    let mut last_connect_try: Option<Instant> = None;
    let mut post_failures: u32 = 0;
    let mut last_heartbeat = Instant::now();
    loop {
        let now = Instant::now();

        // 1. Wi-Fi supervision. ESP-IDF does not reconnect by itself after a disconnect.
        let up = wifi_up(&wifi);
        if up != wifi_was_up {
            match (up, wifi.sta_netif().get_ip_info()) {
                (true, Ok(ip)) => info!("Wi-Fi connected successfully! IP address: {}", ip.ip),
                (true, Err(_)) => info!("Wi-Fi connected successfully!"),
                (false, _) => warn!("Wi-Fi connection lost"),
            }
            wifi_was_up = up;
        }
        if !up && last_connect_try.map_or(true, |t| now.duration_since(t) >= WIFI_RETRY) {
            if last_connect_try.is_some() {
                warn!("Wi-Fi is down; reconnecting...");
            }
            if let Err(e) = wifi.connect() {
                // Typically "already connecting"; the next pass checks again.
                warn!("wifi.connect() returned {e:?}");
            }
            last_connect_try = Some(now);
        }

        // 2. New sightings from the scan thread.
        let sightings = shared.pending.lock().unwrap_or_else(|e| e.into_inner()).take();
        for s in sightings {
            match fleet.on_heard(s.addr, s.reading, now) {
                // The Pill is heard at most once a minute or so; only the first sighting after
                // boot is shown by default, the rest are debug-level.
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

        // 3. Heartbeat.
        if now.duration_since(last_heartbeat) >= HEARTBEAT_INTERVAL {
            info!(
                "Still scanning: {} Bluetooth advertisements in the last {} min; RAPT Pill: {} readings, {} other packets",
                shared.adverts.swap(0, Ordering::Relaxed),
                HEARTBEAT_INTERVAL.as_secs() / 60,
                shared.pill_readings.swap(0, Ordering::Relaxed),
                shared.pill_other.swap(0, Ordering::Relaxed),
            );
            last_heartbeat = now;
        }

        // 4. Posting.
        for name in fleet.drop_stale(now) {
            warn!("Last reading from {name:?} is stale; waiting for a new one");
        }
        if up {
            for post in fleet.due(now) {
                match send_to_brewfather(&post.name, post.reading.temp_c, post.reading.gravity) {
                    Ok(status) if fleet::is_success_status(status) => {
                        info!(
                            "Successfully pushed {:?} to Brewfather: {:.2} C, SG {:.4}, battery {}%",
                            post.name, post.reading.temp_c, post.reading.gravity, post.reading.battery_pct
                        );
                        fleet.mark_posted(post.idx, Instant::now());
                        post_failures = 0;
                    }
                    Ok(status) => {
                        // We reached something, but it was not Brewfather accepting the reading
                        // (a router's block page, a captive portal, a rate limit, an error).
                        // Keep the reading and retry after the backoff. The network itself works,
                        // so this does not count towards resetting Wi-Fi.
                        error!(
                            "Brewfather did not accept {:?}: HTTP status {status}; will retry",
                            post.name
                        );
                        fleet.mark_failed(post.idx, Instant::now());
                        post_failures = 0;
                    }
                    Err(e) => {
                        error!("Failed to push {:?} to Brewfather: {e}", post.name);
                        fleet.mark_failed(post.idx, Instant::now());
                        post_failures += 1;
                        if post_failures >= FAILURES_BEFORE_RECONNECT {
                            warn!("{post_failures} posts failed in a row; resetting the Wi-Fi connection");
                            let _ = wifi.disconnect();
                            post_failures = 0;
                            break; // the Wi-Fi supervision above reconnects on the next pass
                        }
                    }
                }
            }
        }

        std::thread::sleep(LOOP_TICK);
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

fn fmt_addr(a: &pills::Addr) -> String {
    format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", a[0], a[1], a[2], a[3], a[4], a[5])
}

/// Scan for Bluetooth advertisements forever, queueing decoded RAPT Pill readings for the main
/// thread. Never returns. Runs on its own thread and shares nothing with Wi-Fi or HTTP, so
/// neither can make it miss an advertisement.
fn scan_forever(shared: Arc<Shared>, ready: mpsc::Sender<()>) {
    // First use of the global BLE device brings up the Bluetooth stack.
    let ble_device: &BLEDevice = BLEDevice::take();
    let _ = ready.send(());

    loop {
        let result = block_on(async {
            let mut scan = BLEScan::new();
            // Passive scan: all the data we need is in the advertisement itself.
            // Duplicates must not be filtered: the Pill sends several packet types from one address.
            scan.active_scan(false).filter_duplicates(false).interval(100).window(99);

            // This callback runs on NimBLE's small host task: no logging or formatting in here,
            // or it overflows the stack. Just count, decode and queue.
            scan.start(ble_device, SCAN_MS, |device, data| {
                shared.adverts.fetch_add(1, Ordering::Relaxed);
                let mfg = data.manufacture_data()?;
                if mfg.company_identifier == rapt::COMPANY_ID {
                    match rapt::parse(mfg.payload) {
                        Some(reading) => {
                            shared.pill_readings.fetch_add(1, Ordering::Relaxed);
                            shared.pending.lock().unwrap_or_else(|e| e.into_inner()).push(fleet::Sighting {
                                addr: device.addr().as_be_bytes(),
                                rssi: device.rssi(),
                                reading,
                            });
                        }
                        None => {
                            shared.pill_other.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                None::<()>
            })
            .await
        });

        // A scan only ends if the duration elapses (normal) or something went wrong.
        if let Err(e) = result {
            error!("BLE scan failed: {e:?}");
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

/// Post one reading. Returns the HTTP status of the response; an `Err` means the request never
/// completed (DNS, connection, timeout). The caller decides what counts as success.
fn send_to_brewfather(name: &str, temp: f32, gravity: f32) -> Result<u16> {
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

    Ok(status)
}
