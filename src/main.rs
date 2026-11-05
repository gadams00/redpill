mod dns;
mod fleet;
mod names;
mod pages;
mod pills;
mod rapt;
mod settings;
mod store;
mod web;

use anyhow::{anyhow, Result};
use embedded_svc::http::client::Client;
use embedded_svc::ipv4;
use embedded_svc::wifi::{AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration as WifiConfiguration};
use esp32_nimble::{BLEDevice, BLEScan};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::hal::gpio::{PinDriver, Pull};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::reset::restart;
use esp_idf_svc::hal::task::block_on;
use esp_idf_svc::http::client::{Configuration as HttpClientConfiguration, EspHttpConnection};
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::netif::{EspNetif, NetifConfiguration, NetifStack};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{EspWifi, WifiDeviceId, WifiDriver};
use log::{error, info, warn};
use pages::Join;
use serde::Serialize;
use settings::Settings;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use store::Store;
use web::{lock, LanState, Portal, PortalShared};

/// First-boot defaults. Read from `cfg.toml` at build time if it exists (see `cfg.toml.example`).
/// They only seed an empty device: once settings are saved on the board (setup portal / settings
/// page), those win. `setup_pass` is the password of the setup Wi-Fi network.
#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_pass: &'static str,
    #[default("")]
    brewfather_stream_id: &'static str,
    /// `ADDRESS=Name;ADDRESS=Name`. The name is the Brewfather device name for that Pill.
    #[default("")]
    pills: &'static str,
    /// Password of the setup access point (8-63 characters).
    #[default("redpill1")]
    setup_pass: &'static str,
}

/// Length of one BLE scan. The scan thread restarts it immediately when it ends, so this only
/// sets how often (a few milliseconds of) restart gap occurs, and how long it takes to pause
/// scanning (see `Shared::scan_paused`).
const SCAN_MS: i32 = 30 * 1000;
/// Stack for the scan thread. The scan callback itself runs on NimBLE's own task, not this one.
const SCAN_THREAD_STACK: usize = 8192;
/// How long to wait for the scan thread to bring up the Bluetooth stack.
const BLE_INIT_WAIT: Duration = Duration::from_secs(10);
/// Pause between passes of the main loop.
const LOOP_TICK: Duration = Duration::from_secs(1);
/// Minimum time between Wi-Fi (re)connect attempts.
const WIFI_RETRY: Duration = Duration::from_secs(10);
/// Reconnect interval while the setup portal is open. Every attempt makes the setup network follow
/// the router's channel for several seconds (it hops between channels when the router has several
/// nodes), which drops a phone that is using the portal, so attempts are rare.
const WIFI_RETRY_WITH_PORTAL: Duration = Duration::from_secs(5 * 60);
/// Log a "still scanning" line this often, so an unattended board shows it is alive and what it hears.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// After this many posts fail in a row, assume the Wi-Fi/DNS state is stuck and reconnect.
const FAILURES_BEFORE_RECONNECT: u32 = 3;
/// How long to wait for the home network when the user submits the portal form.
const JOIN_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the portal stays up after a successful join, so the phone can read the new address.
const PORTAL_LINGER: Duration = Duration::from_secs(30);
/// Holding the BOOT button this long restarts the board in setup mode.
const BUTTON_HOLD: Duration = Duration::from_secs(3);
/// Stack for the LAN thread, which starts the HTTP server and registers its handlers. 8 KB
/// overflowed in debug builds (symptom: `A stack overflow in task pthread` right after the name is
/// announced).
const LAN_THREAD_STACK: usize = 16 * 1024;
/// How often the LAN thread looks for other redpill boards and re-checks its own name.
const LAN_REFRESH: Duration = Duration::from_secs(60);

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
    /// While the fallback setup network is open, Bluetooth scanning is paused: it runs at nearly
    /// 100% duty and starves the access point's Wi-Fi beacons on the ESP32's single radio, so phones
    /// could not see the setup network. Nothing can be posted without Wi-Fi anyway.
    scan_paused: AtomicBool,
}

/// A running fallback setup portal, opened on top of normal operation.
struct OpenPortal {
    _portal: Portal,
    shared: Arc<PortalShared>,
    ap: AccessPointConfiguration,
}

fn main() -> Result<()> {
    // Patch patches for ESP-IDF runtime
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("Starting ESP32 RAPT Pill to Brewfather Bridge");

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let store = Arc::new(Mutex::new(Store::open(nvs.clone())?));
    let mut settings = lock(&store).load();
    seed_from_build_config(&store, &mut settings);
    // The "start in setup mode" request is one-shot: clear it now so the next boot is normal.
    let portal_requested = {
        let s = lock(&store);
        let requested = s.portal_requested();
        if requested {
            if let Err(e) = s.set_portal_requested(false) {
                warn!("could not clear the setup-mode request: {e}");
            }
        }
        requested
    };

    // Same as `EspWifi::new`, but with our own access-point interface (see `portal_ap_netif`).
    let driver = WifiDriver::new(peripherals.modem, sys_loop, Some(nvs))?;
    let wifi = EspWifi::wrap_all(driver, EspNetif::new(NetifStack::Sta)?, portal_ap_netif()?)?;
    let mac = wifi.get_mac(WifiDeviceId::Sta)?;
    let stored_name = lock(&store).hostname();

    if !settings.has_wifi() || portal_requested {
        if portal_requested {
            info!("Setup mode requested");
        } else {
            info!("No Wi-Fi network is saved yet");
        }
        return run_setup(wifi, &store, settings.has_wifi(), mac, stored_name);
    }

    // Holding BOOT (GPIO0) for a few seconds while running asks for setup mode.
    let button = PinDriver::input(peripherals.pins.gpio0, Pull::Up)?;
    {
        let store = Arc::clone(&store);
        std::thread::Builder::new().name("button".into()).stack_size(4096).spawn(move || {
            let mut pressed_since: Option<Instant> = None;
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if button.is_low() {
                    let since = *pressed_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= BUTTON_HOLD {
                        warn!("BOOT button held: restarting in setup mode");
                        if let Err(e) = lock(&store).set_portal_requested(true) {
                            error!("could not save the setup-mode request: {e}");
                        }
                        restart();
                    }
                } else {
                    pressed_since = None;
                }
            }
        })?;
    }

    run_normal(wifi, store, settings, mac, stored_name)
}

/// On a device with nothing saved, copy the values compiled in from `cfg.toml` (if any) into NVS,
/// so a developer's flash-and-go workflow keeps working.
fn seed_from_build_config(store: &Mutex<Store>, settings: &mut Settings) {
    if settings.has_wifi() || CONFIG.wifi_ssid.is_empty() {
        return;
    }
    let pills = pills::parse(CONFIG.pills).unwrap_or_else(|e| {
        warn!("ignoring `pills` in cfg.toml: {e}");
        Vec::new()
    });
    let stream_id = if CONFIG.brewfather_stream_id.is_empty() {
        String::new()
    } else {
        settings::validate_stream_id(CONFIG.brewfather_stream_id).unwrap_or_else(|e| {
            warn!("ignoring `brewfather_stream_id` in cfg.toml: {e}");
            String::new()
        })
    };
    let seeded = Settings {
        wifi_ssid: CONFIG.wifi_ssid.to_string(),
        wifi_pass: CONFIG.wifi_pass.to_string(),
        stream_id,
        pills,
    };
    match lock(store).save(&seeded) {
        Ok(()) => {
            info!("Saved the settings compiled in from cfg.toml to the board");
            *settings = seeded;
        }
        Err(e) => warn!("could not save the cfg.toml settings: {e}"),
    }
}

// ---- Wi-Fi helpers ----------------------------------------------------------------------------

fn client_config(ssid: &str, pass: &str) -> Result<ClientConfiguration> {
    Ok(ClientConfiguration {
        ssid: ssid.try_into().map_err(|_| anyhow!("Wi-Fi network name {ssid:?} is too long"))?,
        password: pass.try_into().map_err(|_| anyhow!("Wi-Fi password is too long"))?,
        auth_method: if pass.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    })
}

fn setup_password() -> &'static str {
    if (8..=63).contains(&CONFIG.setup_pass.len()) {
        CONFIG.setup_pass
    } else {
        "redpill1"
    }
}

fn ap_config(ssid: &str) -> Result<AccessPointConfiguration> {
    Ok(AccessPointConfiguration {
        ssid: ssid.try_into().map_err(|_| anyhow!("setup network name {ssid:?} is too long"))?,
        password: setup_password().try_into().map_err(|_| anyhow!("setup password is too long"))?,
        auth_method: AuthMethod::WPA2Personal,
        channel: 1,
        max_connections: 4,
        ..Default::default()
    })
}

/// The station's IP address, if it is associated and has one.
fn station_ip(wifi: &EspWifi<'_>) -> Option<Ipv4Addr> {
    if !wifi.is_connected().unwrap_or(false) {
        return None;
    }
    wifi.sta_netif().get_ip_info().ok().map(|i| i.ip).filter(|ip| *ip != Ipv4Addr::UNSPECIFIED)
}

/// Host name the router sees in DHCP. Must be set before the station connects.
fn set_dhcp_hostname(wifi: &EspWifi<'_>, name: &str) {
    let Ok(c_name) = std::ffi::CString::new(name) else { return };
    let err = unsafe { esp_idf_svc::sys::esp_netif_set_hostname(wifi.sta_netif().handle(), c_name.as_ptr()) };
    if err != 0 {
        warn!("could not set the DHCP host name (error {err})");
    }
}

/// The setup network's interface: `192.168.71.1/24`, with a DHCP server that tells clients to use
/// the board itself as their DNS server. Without that offer a phone has no DNS on the setup network,
/// so Android cannot resolve its connectivity-check host and never shows the "sign in" prompt.
fn portal_ap_netif() -> Result<EspNetif> {
    let gateway = Ipv4Addr::new(192, 168, 71, 1);
    let mut conf = NetifConfiguration::wifi_default_router();
    conf.ip_configuration = Some(ipv4::Configuration::Router(ipv4::RouterConfiguration {
        subnet: ipv4::Subnet { gateway, mask: ipv4::Mask(24) },
        dhcp_enabled: true,
        dns: Some(gateway),
        secondary_dns: None,
    }));
    Ok(EspNetif::new_with_conf(&conf)?)
}

/// The board's own address on the setup network (the access point's interface).
fn ap_ip(wifi: &EspWifi<'_>) -> Ipv4Addr {
    wifi.ap_netif().get_ip_info().map(|i| i.ip).unwrap_or(Ipv4Addr::new(192, 168, 71, 1))
}

/// Names of nearby networks, strongest first, for the portal's drop-down.
fn scan_networks(wifi: &mut EspWifi<'_>) -> Vec<String> {
    match wifi.scan() {
        Ok(mut aps) => {
            aps.sort_by_key(|ap| std::cmp::Reverse(ap.signal_strength));
            let mut out: Vec<String> = Vec::new();
            for ap in aps {
                let name = ap.ssid.as_str();
                if !name.is_empty() && !out.iter().any(|o| o == name) {
                    out.push(name.to_string());
                }
                if out.len() >= 20 {
                    break;
                }
            }
            info!("Wi-Fi scan found {} network(s): {:?}", out.len(), out);
            out
        }
        Err(e) => {
            warn!("Wi-Fi scan failed: {e:?}");
            Vec::new()
        }
    }
}

/// Try to join the network from the portal form while the setup network stays up.
fn try_join(wifi: &mut EspWifi<'_>, ap: &AccessPointConfiguration, ssid: &str, pass: &str) -> Result<Ipv4Addr, String> {
    let client = client_config(ssid, pass).map_err(|e| e.to_string())?;
    wifi.set_configuration(&WifiConfiguration::Mixed(client, ap.clone())).map_err(|e| format!("{e}"))?;
    let _ = wifi.disconnect(); // forget an earlier attempt; fails harmlessly if there was none
    if let Err(e) = wifi.connect() {
        warn!("wifi.connect() returned {e:?}");
    }
    let deadline = Instant::now() + JOIN_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(ip) = station_ip(wifi) {
            return Ok(ip);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = wifi.disconnect();
    Err(format!(
        "Could not connect to {ssid:?} within {} seconds. Check the network name and password.",
        JOIN_TIMEOUT.as_secs()
    ))
}

/// Handle a submitted portal form. On success the new Wi-Fi settings and the board's name are
/// saved, the portal shows the address for a while, and the board restarts into normal mode
/// (this function does not return). On failure the portal reports the reason and the previous
/// Wi-Fi configuration is restored.
#[allow(clippy::too_many_arguments)]
fn attempt_join(
    wifi: &mut EspWifi<'_>,
    ap: &AccessPointConfiguration,
    previous_client: &ClientConfiguration,
    shared: &PortalShared,
    store: &Mutex<Store>,
    mac: &[u8; 6],
    stored_name: Option<&str>,
    ssid: &str,
    pass: &str,
) {
    match try_join(wifi, ap, ssid, pass) {
        Ok(ip) => {
            info!("Portal: joined {ssid:?}, IP address {ip}");
            let chosen = match EspMdns::take() {
                Ok(mdns) => names::choose(&mdns, stored_name, ip, mac),
                Err(e) => {
                    warn!("mDNS unavailable ({e:?}); using the default name");
                    names::Chosen {
                        name: stored_name.unwrap_or(settings::BASE_HOSTNAME).to_string(),
                        persist: stored_name.is_some(),
                        note: None,
                    }
                }
            };
            let saved = {
                let s = lock(store);
                s.save_wifi(ssid, pass).and_then(|()| if chosen.persist { s.set_hostname(&chosen.name) } else { Ok(()) })
            };
            if let Err(e) = saved {
                error!("Portal: could not save settings: {e}");
                *lock(&shared.join) = Join::Failed { reason: format!("Connected, but saving the settings failed: {e}") };
                return;
            }
            info!("Portal: this board is {}.local; restarting in {} s", chosen.name, PORTAL_LINGER.as_secs());
            *lock(&shared.join) = Join::Connected { hostname: chosen.name, ip: ip.to_string() };
            std::thread::sleep(PORTAL_LINGER);
            restart();
        }
        Err(reason) => {
            warn!("Portal: {reason}");
            *lock(&shared.join) = Join::Failed { reason };
            let _ = wifi.disconnect();
            if let Err(e) = wifi.set_configuration(&WifiConfiguration::Mixed(previous_client.clone(), ap.clone())) {
                warn!("could not restore the previous Wi-Fi configuration: {e:?}");
            }
        }
    }
}

// ---- setup mode (nothing saved, or requested) ---------------------------------------------------

/// Setup mode: only the setup network and the portal run (no Bluetooth, no posting). Never returns
/// except on error: it ends by restarting the board.
fn run_setup(
    mut wifi: EspWifi<'static>,
    store: &Arc<Mutex<Store>>,
    had_wifi: bool,
    mac: [u8; 6],
    stored_name: Option<String>,
) -> Result<()> {
    let ssid = settings::setup_ssid(stored_name.as_deref(), &mac);
    let ap = ap_config(&ssid)?;
    let idle_client = ClientConfiguration::default();
    wifi.set_configuration(&WifiConfiguration::Mixed(idle_client.clone(), ap.clone()))?;
    wifi.start()?;
    let networks = scan_networks(&mut wifi);

    info!("SETUP MODE: join the Wi-Fi network {ssid:?} (password {:?}) and a page opens to choose your Wi-Fi", setup_password());
    let shared = Arc::new(PortalShared::new(
        networks,
        stored_name.clone().unwrap_or_else(|| settings::BASE_HOSTNAME.to_string()),
    ));
    let _portal = Portal::start(Arc::clone(&shared), ap_ip(&wifi))?;

    let started = Instant::now();
    loop {
        if shared.rescan.swap(false, Ordering::AcqRel) {
            *lock(&shared.networks) = scan_networks(&mut wifi);
        }
        let request = lock(&shared.request).take();
        if let Some((ssid_in, pass_in)) = request {
            attempt_join(&mut wifi, &ap, &idle_client, &shared, store, &mac, stored_name.as_deref(), &ssid_in, &pass_in);
        }
        // A portal opened on purpose (button) on a board that already has Wi-Fi closes by itself.
        if had_wifi && started.elapsed() >= settings::PORTAL_TIMEOUT {
            info!("Setup mode timed out; restarting with the saved Wi-Fi");
            restart();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---- normal mode ----------------------------------------------------------------------------------

fn run_normal(
    mut wifi: EspWifi<'static>,
    store: Arc<Mutex<Store>>,
    settings: Settings,
    mac: [u8; 6],
    stored_name: Option<String>,
) -> Result<()> {
    let mut fleet = fleet::Fleet::new(settings.pills.clone());
    if fleet.is_empty() {
        warn!(
            "No Pills are configured yet, so nothing will be posted. Wake your Pill (double-tap it), \
             then open the board's settings page and name it."
        );
    }
    for (addr, name) in fleet.configured() {
        info!("Configured Pill {} -> Brewfather device {:?}", pills::format_addr(addr), name);
    }

    let client = client_config(&settings.wifi_ssid, &settings.wifi_pass)?;
    let lan = Arc::new(LanState::new(Arc::clone(&store), settings));
    {
        let mut view = lock(&lan.view);
        view.hostname = stored_name.clone().unwrap_or_else(|| settings::BASE_HOSTNAME.to_string());
        view.setup_ssid = settings::setup_ssid(stored_name.as_deref(), &mac);
    }

    // Wi-Fi is configured and started here, but not waited for: scanning must not depend on it.
    if let Some(name) = &stored_name {
        set_dhcp_hostname(&wifi, name);
    }
    wifi.set_configuration(&WifiConfiguration::Client(client.clone()))?;
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
    let boot = Instant::now();
    let mut wifi_was_up = false;
    let mut ever_connected = false;
    let mut lan_started = false;
    let mut portal: Option<OpenPortal> = None;
    let mut last_portal_try: Option<Instant> = None;
    let mut last_connect_try: Option<Instant> = None;
    let mut post_failures: u32 = 0;
    let mut last_heartbeat = Instant::now();
    loop {
        let now = Instant::now();

        // 1. Wi-Fi supervision. ESP-IDF does not reconnect by itself after a disconnect.
        let ip = station_ip(&wifi);
        let up = ip.is_some();
        if up != wifi_was_up {
            match ip {
                Some(ip) => info!("Wi-Fi connected successfully! IP address: {ip}"),
                None => warn!("Wi-Fi connection lost"),
            }
            wifi_was_up = up;
        }
        if let Some(ip) = ip {
            ever_connected = true;
            lock(&lan.view).ip = ip.to_string();
            if portal.take().is_some() {
                shared.scan_paused.store(false, Ordering::Release);
                info!("Wi-Fi connected; closing the setup network");
                if let Err(e) = wifi.set_configuration(&WifiConfiguration::Client(client.clone())) {
                    warn!("could not leave setup-network mode: {e:?}");
                }
            }
            if !lan_started {
                lan_started = true;
                spawn_lan(Arc::clone(&lan), mac, ip, stored_name.clone());
            }
        }
        let retry = if portal.is_some() { WIFI_RETRY_WITH_PORTAL } else { WIFI_RETRY };
        if !up && last_connect_try.map_or(true, |t| now.duration_since(t) >= retry) {
            if last_connect_try.is_some() {
                warn!("Wi-Fi is down; reconnecting...");
            }
            if let Err(e) = wifi.connect() {
                // Typically "already connecting"; the next pass checks again.
                warn!("wifi.connect() returned {e:?}");
            }
            last_connect_try = Some(now);
        }

        // 1b. Fallback setup portal: never connected since boot, e.g. a wrong password or a new
        // router. Opens next to normal operation; scanning carries on.
        if portal.is_none()
            && settings::fallback_portal_due(boot, ever_connected, now)
            && last_portal_try.is_none_or(|t| now.duration_since(t) >= WIFI_RETRY_WITH_PORTAL)
        {
            last_portal_try = Some(now);
            match open_fallback_portal(&mut wifi, &client, &mac, stored_name.as_deref()) {
                Ok(p) => {
                    shared.scan_paused.store(true, Ordering::Release);
                    portal = Some(p);
                }
                Err(e) => error!("could not open the setup portal: {e}"),
            }
        }
        if let Some(p) = &portal {
            if p.shared.rescan.swap(false, Ordering::AcqRel) {
                *lock(&p.shared.networks) = scan_networks(&mut wifi);
            }
            let request = lock(&p.shared.request).take();
            if let Some((ssid_in, pass_in)) = request {
                attempt_join(&mut wifi, &p.ap, &client, &p.shared, &store, &mac, stored_name.as_deref(), &ssid_in, &pass_in);
            }
        }

        // 2. New sightings from the scan thread.
        let sightings = lock(&shared.pending).take();
        for s in sightings {
            match fleet.on_heard(s.addr, s.rssi, s.reading, now) {
                // The Pill is heard at most once a minute or so; only the first sighting after
                // boot is shown by default, the rest are debug-level.
                fleet::Heard::Listed { name, first } => {
                    let level = if first { log::Level::Info } else { log::Level::Debug };
                    log::log!(
                        level,
                        "RAPT Pill {:?} [{}] rssi {}: {:.2} C, SG {:.4}, battery {}%",
                        name,
                        pills::format_addr(&s.addr),
                        s.rssi,
                        s.reading.temp_c,
                        s.reading.gravity,
                        s.reading.battery_pct
                    );
                }
                fleet::Heard::UnlistedNew => warn!(
                    "Found RAPT Pill {} (rssi {}, {:.2} C, SG {:.4}) but it is not named yet; \
                     open the board's settings page to add it",
                    pills::format_addr(&s.addr),
                    s.rssi,
                    s.reading.temp_c,
                    s.reading.gravity,
                ),
                fleet::Heard::UnlistedAgain => {}
            }
        }

        // 2b. The Pill list was edited on the settings page.
        if lan.dirty.load(Ordering::Acquire) {
            let pills = lock(&lan.settings).pills.clone();
            for (addr, name) in &pills {
                info!("Configured Pill {} -> Brewfather device {:?}", pills::format_addr(addr), name);
            }
            fleet.set_configured(pills);
            refresh_view(&lan, &fleet, now);
            lan.dirty.store(false, Ordering::Release);
        }

        // 3. Heartbeat.
        if now.duration_since(last_heartbeat) >= HEARTBEAT_INTERVAL {
            info!(
                "Still scanning: {} Bluetooth advertisements in the last {} min; RAPT Pill: {} readings, {} other packets; free heap {} bytes (lowest {})",
                shared.adverts.swap(0, Ordering::Relaxed),
                HEARTBEAT_INTERVAL.as_secs() / 60,
                shared.pill_readings.swap(0, Ordering::Relaxed),
                shared.pill_other.swap(0, Ordering::Relaxed),
                unsafe { esp_idf_svc::sys::esp_get_free_heap_size() },
                unsafe { esp_idf_svc::sys::esp_get_minimum_free_heap_size() },
            );
            if lock(&lan.settings).stream_id.is_empty() {
                warn!("No Brewfather stream id is set, so nothing is posted. Enter it on the settings page.");
            }
            last_heartbeat = now;
        }

        // 4. Posting.
        for name in fleet.drop_stale(now) {
            warn!("Last reading from {name:?} is stale; waiting for a new one");
        }
        refresh_view(&lan, &fleet, now);
        let stream_id = lock(&lan.settings).stream_id.clone();
        if up && !stream_id.is_empty() {
            for post in fleet.due(now) {
                match send_to_brewfather(&stream_id, &post.name, post.reading.temp_c, post.reading.gravity) {
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

/// Copy the fleet's view of the world into what the settings page shows.
fn refresh_view(lan: &LanState, fleet: &fleet::Fleet, now: Instant) {
    let mut view = lock(&lan.view);
    view.status = fleet.status(now);
    view.unlisted = fleet.unlisted(now);
}

/// Open the setup network and portal alongside the (not yet working) saved Wi-Fi.
fn open_fallback_portal(
    wifi: &mut EspWifi<'_>,
    client: &ClientConfiguration,
    mac: &[u8; 6],
    stored_name: Option<&str>,
) -> Result<OpenPortal> {
    let ssid = settings::setup_ssid(stored_name, mac);
    let ap = ap_config(&ssid)?;
    wifi.set_configuration(&WifiConfiguration::Mixed(client.clone(), ap.clone()))?;
    let networks = scan_networks(wifi);
    warn!(
        "Wi-Fi has not connected since boot. Opening the setup network {ssid:?} (password {:?}); \
         Bluetooth scanning is paused until Wi-Fi connects",
        setup_password()
    );
    let shared = Arc::new(PortalShared::new(
        networks,
        stored_name.unwrap_or(settings::BASE_HOSTNAME).to_string(),
    ));
    let portal = Portal::start(Arc::clone(&shared), ap_ip(wifi))?;
    Ok(OpenPortal { _portal: portal, shared, ap })
}

// ---- settings page and network name -----------------------------------------------------------------

fn spawn_lan(lan: Arc<LanState>, mac: [u8; 6], ip: Ipv4Addr, stored_name: Option<String>) {
    let spawned = std::thread::Builder::new()
        .name("lan".into())
        .stack_size(LAN_THREAD_STACK)
        .spawn(move || lan_services(lan, mac, ip, stored_name));
    if let Err(e) = spawned {
        error!("could not start the settings page thread: {e}");
    }
}

/// Runs once Wi-Fi has connected for the first time: pick and announce the board's name, serve the
/// settings page, then keep the name healthy and look for other boards. Nothing here is needed
/// for posting readings, so every failure is just logged.
fn lan_services(lan: Arc<LanState>, mac: [u8; 6], ip: Ipv4Addr, stored_name: Option<String>) {
    let mut mdns = EspMdns::take().map_err(|e| warn!("mDNS is unavailable: {e:?}")).ok();

    let (name, note, persisted) = match mdns.as_mut() {
        Some(m) => {
            let chosen = names::choose(m, stored_name.as_deref(), ip, &mac);
            if chosen.persist && stored_name.as_deref() != Some(chosen.name.as_str()) {
                if let Err(e) = lock(&lan.store).set_hostname(&chosen.name) {
                    warn!("could not save the board name: {e}");
                }
            }
            names::announce(m, &chosen.name);
            (chosen.name, chosen.note, chosen.persist)
        }
        None => (
            stored_name.clone().unwrap_or_else(|| settings::BASE_HOSTNAME.to_string()),
            None,
            stored_name.is_some(),
        ),
    };
    if let Some(note) = &note {
        warn!("Board name: {note}");
    }
    {
        let mut view = lock(&lan.view);
        view.hostname = name.clone();
        view.notice = note.map(|n| capitalize(&n));
        view.setup_ssid = settings::setup_ssid(persisted.then_some(name.as_str()), &mac);
    }
    info!("This board is http://{name}.local/ (http://{ip}/)");

    let mdns = Arc::new(Mutex::new(mdns));
    let _server = match web::start_lan_server(Arc::clone(&lan), Arc::clone(&mdns)) {
        Ok(s) => s,
        Err(e) => {
            error!("could not start the settings page: {e}");
            return;
        }
    };

    let mut first = true;
    loop {
        std::thread::sleep(if first { Duration::from_secs(5) } else { LAN_REFRESH });
        first = false;

        let mut guard = lock(&mdns);
        let Some(m) = guard.as_mut() else { continue };
        let (wanted, own_ip) = {
            let view = lock(&lan.view);
            (view.hostname.clone(), view.ip.parse::<Ipv4Addr>().unwrap_or(ip))
        };

        // If another device won the name while we were announcing it, the mDNS component renames
        // itself (e.g. "redpill-2"). Move on to the next free name and remember it.
        if names::current_hostname().is_some_and(|actual| actual != wanted) {
            warn!("Another device is using {wanted}.local; choosing a new name");
            let start = settings::next_hostname(&wanted);
            let chosen = names::choose(m, start.as_deref(), own_ip, &mac);
            names::rename(m, &chosen.name);
            if chosen.persist {
                if let Err(e) = lock(&lan.store).set_hostname(&chosen.name) {
                    warn!("could not save the board name: {e}");
                }
            }
            let mut view = lock(&lan.view);
            view.notice = Some(format!(
                "The name {wanted}.local was taken by another device, so this board is now {}.local.",
                chosen.name
            ));
            view.setup_ssid = settings::setup_ssid(chosen.persist.then_some(chosen.name.as_str()), &mac);
            view.hostname = chosen.name;
        }

        let current = lock(&lan.view).hostname.clone();
        let others = names::others(m, &current);
        lock(&lan.view).others = others;
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str() + ".",
        None => String::new(),
    }
}

/// Scan for Bluetooth advertisements forever, queueing decoded RAPT Pill readings for the main
/// thread. Never returns. Runs on its own thread and shares nothing with Wi-Fi or HTTP, so
/// neither can make it miss an advertisement.
fn scan_forever(shared: Arc<Shared>, ready: mpsc::Sender<()>) {
    // First use of the global BLE device brings up the Bluetooth stack.
    let ble_device: &BLEDevice = BLEDevice::take();
    let _ = ready.send(());

    loop {
        // Paused while the fallback setup network is open (see `Shared::scan_paused`).
        while shared.scan_paused.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(500));
        }
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
fn send_to_brewfather(stream_id: &str, name: &str, temp: f32, gravity: f32) -> Result<u16> {
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

    let url = format!("http://log.brewfather.net/stream?id={stream_id}");
    let mut request = client.post(&url, &headers)?;
    request.write(json_body.as_bytes())?;
    let response = request.submit()?;

    let status = response.status();
    info!("Brewfather API response status: {status}");

    Ok(status)
}
