//! The two web servers: the captive setup portal (Wi-Fi only, with a catch-all DNS responder) and
//! the settings page on the home network. Handlers only validate, save to NVS and hand work to
//! the main thread through `LanState` / `PortalShared`; they never touch the `Fleet`.
//! Page content comes from `pages.rs`, parsing and validation from `settings.rs` / `pills.rs`.

use crate::fleet::{PillStatus, SightingQueue, UnlistedPill};
use crate::pages::{self, Join, SettingsView};
use crate::settings::{self, Settings};
use crate::store::Store;
use crate::{dns, names, pills};
use anyhow::{anyhow, Result};
use embedded_svc::http::Headers;
use esp_idf_svc::http::server::{Configuration as HttpConfig, EspHttpConnection, EspHttpServer, Request};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::{Read, Write};
use esp_idf_svc::mdns::EspMdns;
use log::{info, warn};
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// Stack for the HTTP server task. Pages are built on the heap; this only has to hold the handler
/// frames (earlier stack overflows are described in CLAUDE.md).
const HTTP_STACK: usize = 10 * 1024;
const DNS_STACK: usize = 8192;

type Req<'a, 'r> = Request<&'a mut EspHttpConnection<'r>>;

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn http_config(wildcard: bool) -> HttpConfig {
    HttpConfig {
        stack_size: HTTP_STACK,
        uri_match_wildcard: wildcard,
        max_uri_handlers: 12,
        // Chrome opens up to 6 parallel connections (plus speculative ones). With the default of 4 the
        // server closed live keep-alive connections to make room, and Chrome gave up with
        // ERR_TOO_MANY_RETRIES. Needs CONFIG_LWIP_MAX_SOCKETS >= this + 3 (sdkconfig.defaults).
        max_open_sockets: 8,
        ..Default::default()
    }
}

fn send_html(req: Req<'_, '_>, status: u16, html: &str) -> Result<()> {
    let mut resp = req.into_response(
        status,
        None,
        &[("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store")],
    )?;
    resp.write_all(html.as_bytes())?;
    Ok(())
}

fn redirect(req: Req<'_, '_>, location: &str) -> Result<()> {
    req.into_response(302, Some("Found"), &[("Location", location), ("Content-Length", "0"), ("Cache-Control", "no-store")])?;
    Ok(())
}

fn read_form(req: &mut Req<'_, '_>) -> Result<Vec<(String, String)>> {
    let len = req.content_len().unwrap_or(0) as usize;
    if len > settings::MAX_FORM_BYTES {
        return Err(anyhow!("form of {len} bytes is too large"));
    }
    let mut buf = vec![0u8; len];
    req.read_exact(&mut buf).map_err(|e| anyhow!("reading form: {e:?}"))?;
    Ok(settings::parse_form(&String::from_utf8_lossy(&buf)))
}

// ---- captive portal -------------------------------------------------------------------------

/// State shared between the portal handlers and the main thread (which owns the Wi-Fi driver).
pub struct PortalShared {
    /// SSIDs found by the scan, for the drop-down.
    pub networks: Mutex<Vec<String>>,
    /// The name this board expects to get, shown as a hint on the form.
    pub next_name: Mutex<String>,
    pub join: Mutex<Join>,
    /// Set by the form handler, taken by the main thread, which tries to join.
    pub request: Mutex<Option<(String, String)>>,
    /// Set by `/rescan`; the main thread (which owns the Wi-Fi driver) scans and clears it.
    pub rescan: AtomicBool,
}

impl PortalShared {
    pub fn new(networks: Vec<String>, next_name: String) -> Self {
        PortalShared {
            networks: Mutex::new(networks),
            next_name: Mutex::new(next_name),
            join: Mutex::new(Join::Idle),
            request: Mutex::new(None),
            rescan: AtomicBool::new(false),
        }
    }
}

/// A running portal: the web server plus the catch-all DNS responder. Dropping it stops both.
pub struct Portal {
    _server: EspHttpServer<'static>,
    stop: Arc<AtomicBool>,
    dns: Option<JoinHandle<()>>,
}

impl Drop for Portal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.dns.take() {
            let _ = h.join();
        }
    }
}

impl Portal {
    /// `ap_ip` is the board's own address on the setup network (read from the access point's
    /// network interface: the DNS answers and the redirects must point at it).
    pub fn start(shared: Arc<PortalShared>, ap_ip: Ipv4Addr) -> Result<Portal> {
        let stop = Arc::new(AtomicBool::new(false));
        let dns = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("dns".into())
                .stack_size(DNS_STACK)
                .spawn(move || dns_loop(stop, ap_ip.octets()))?
        };

        let mut server = EspHttpServer::new(&http_config(true))?;
        let home = format!("http://{ap_ip}/");

        {
            let shared = Arc::clone(&shared);
            server.fn_handler("/", Method::Get, move |req: Req<'_, '_>| -> Result<()> {
                let join = lock(&shared.join).clone();
                let html = match &join {
                    Join::Idle => portal_form(&shared, None),
                    Join::Failed { reason } => portal_form(&shared, Some(reason)),
                    other => pages::portal_join(other),
                };
                send_html(req, 200, &html)
            })?;
        }
        {
            let shared = Arc::clone(&shared);
            server.fn_handler("/save", Method::Post, move |mut req: Req<'_, '_>| -> Result<()> {
                let form = read_form(&mut req)?;
                // A typed name wins over the list.
                let typed = settings::form_get(&form, "ssid_other").unwrap_or("");
                let ssid = if typed.is_empty() { settings::form_get(&form, "ssid").unwrap_or("") } else { typed };
                let pass = settings::form_get(&form, "pass").unwrap_or("");
                match settings::validate_wifi(ssid, pass) {
                    Err(e) => send_html(req, 200, &portal_form(&shared, Some(&e))),
                    Ok((ssid, pass)) => {
                        info!("Portal: asked to join {ssid:?}");
                        *lock(&shared.join) = Join::Trying { ssid: ssid.clone() };
                        *lock(&shared.request) = Some((ssid, pass));
                        redirect(req, "/status")
                    }
                }
            })?;
        }
        {
            let shared = Arc::clone(&shared);
            server.fn_handler("/status", Method::Get, move |req: Req<'_, '_>| -> Result<()> {
                let join = lock(&shared.join).clone();
                if join == Join::Idle {
                    return redirect(req, "/");
                }
                send_html(req, 200, &pages::portal_join(&join))
            })?;
        }
        {
            let shared = Arc::clone(&shared);
            server.fn_handler("/rescan", Method::Get, move |req: Req<'_, '_>| -> Result<()> {
                shared.rescan.store(true, Ordering::Release);
                // The scan runs on the main thread; give it a few seconds so the list is fresh.
                for _ in 0..80 {
                    if !shared.rescan.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                redirect(req, "/")
            })?;
        }
        // Everything else (the phone's connectivity probes, any site the user types) lands here.
        server.fn_handler("/*", Method::Get, move |req: Req<'_, '_>| -> Result<()> { redirect(req, &home) })?;

        info!("Setup portal is up at http://{ap_ip}/");
        Ok(Portal { _server: server, stop, dns: Some(dns) })
    }
}

fn portal_form(shared: &PortalShared, error: Option<&str>) -> String {
    pages::portal_form(&lock(&shared.networks), error, &lock(&shared.next_name))
}

/// Answer every DNS question with the board's own address until `stop` is set.
fn dns_loop(stop: Arc<AtomicBool>, ip: [u8; 4]) {
    let sock = match UdpSocket::bind(("0.0.0.0", 53)) {
        Ok(s) => s,
        Err(e) => {
            warn!("Portal DNS: could not bind port 53: {e}");
            return;
        }
    };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    let mut buf = [0u8; 512];
    while !stop.load(Ordering::Relaxed) {
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            if let Some(reply) = dns::answer(&buf[..n], ip) {
                let _ = sock.send_to(&reply, from);
            }
        }
    }
}

// ---- settings page on the home network ------------------------------------------------------

/// What the settings page shows, written by the main loop and the LAN thread.
#[derive(Default)]
pub struct ViewData {
    pub hostname: String,
    pub ip: String,
    /// SSID the setup network will have if the user asks for setup mode.
    pub setup_ssid: String,
    /// A standing message (the old name was taken, ...).
    pub notice: Option<String>,
    pub status: Vec<PillStatus>,
    pub unlisted: Vec<UnlistedPill>,
    pub others: Vec<(String, String)>,
}

/// State shared between the settings page handlers and the main loop.
pub struct LanState {
    pub store: Arc<Mutex<Store>>,
    /// The current settings, the single copy in RAM.
    pub settings: Mutex<Settings>,
    /// Set by a handler after it changed the Pill list; the main loop applies it to the `Fleet`,
    /// refreshes `view`, then clears it.
    pub dirty: AtomicBool,
    pub view: Mutex<ViewData>,
}

impl LanState {
    pub fn new(store: Arc<Mutex<Store>>, settings: Settings) -> Self {
        LanState { store, settings: Mutex::new(settings), dirty: AtomicBool::new(false), view: Mutex::new(ViewData::default()) }
    }
}

fn render(state: &LanState, notice: Option<&str>, error: Option<&str>) -> String {
    let view = lock(&state.view);
    let settings = lock(&state.settings);
    pages::settings_page(&SettingsView {
        hostname: &view.hostname,
        ip: &view.ip,
        stream_id: &settings.stream_id,
        pills: &view.status,
        unlisted: &view.unlisted,
        others: &view.others,
        notice: notice.or(view.notice.as_deref()),
        error,
    })
}

/// Wait (briefly) until the main loop has applied a change, so the page we send back shows it.
fn wait_applied(state: &LanState) {
    for _ in 0..40 {
        if !state.dirty.load(Ordering::Acquire) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Apply one add / rename / remove of a Pill. Returns the message for the user.
fn edit_pills(state: &LanState, form: &[(String, String)]) -> Result<&'static str, String> {
    let addr = pills::parse_address(settings::form_get(form, "addr").unwrap_or(""))?;
    let mut list = lock(&state.settings).pills.clone();
    let message = match settings::form_get(form, "action").unwrap_or("") {
        "remove" => {
            list.retain(|(a, _)| *a != addr);
            "Pill removed."
        }
        "save" => {
            let name = pills::validate_name(settings::form_get(form, "name").unwrap_or(""))?;
            if list.iter().any(|(a, n)| *a != addr && *n == name) {
                return Err(format!("{name:?} is already used by another Pill; Brewfather needs unique names"));
            }
            match list.iter().position(|(a, _)| *a == addr) {
                Some(i) => list[i].1 = name,
                None if list.len() >= SightingQueue::MAX_PILLS => {
                    return Err(format!("at most {} Pills are supported", SightingQueue::MAX_PILLS))
                }
                None => list.push((addr, name)),
            }
            "Saved. The next reading will be posted to Brewfather."
        }
        other => return Err(format!("unknown action {other:?}")),
    };
    let mut settings = lock(&state.settings).clone();
    settings.pills = list;
    // Save first: if flash fails, nothing changes in RAM either.
    lock(&state.store).save(&settings).map_err(|e| e.to_string())?;
    *lock(&state.settings) = settings;
    state.dirty.store(true, Ordering::Release);
    Ok(message)
}

fn edit_stream(state: &LanState, form: &[(String, String)]) -> Result<&'static str, String> {
    let id = settings::validate_stream_id(settings::form_get(form, "stream").unwrap_or(""))?;
    let mut settings = lock(&state.settings).clone();
    settings.stream_id = id;
    lock(&state.store).save(&settings).map_err(|e| e.to_string())?;
    *lock(&state.settings) = settings;
    Ok("Brewfather stream id saved.")
}

fn edit_hostname(state: &LanState, mdns: &Mutex<Option<EspMdns>>, form: &[(String, String)]) -> Result<String, String> {
    let name = settings::validate_hostname(settings::form_get(form, "hostname").unwrap_or(""))?;
    let (current, ip) = {
        let view = lock(&state.view);
        (view.hostname.clone(), view.ip.parse::<Ipv4Addr>().unwrap_or(Ipv4Addr::UNSPECIFIED))
    };
    if name == current {
        return Ok(format!("This board is already {name}.local."));
    }
    let mut guard = lock(mdns);
    let Some(mdns) = guard.as_mut() else {
        return Err("mDNS is not running on this board, so it cannot be renamed".into());
    };
    if let Some(holder) = names::holder_of(mdns, &name, ip) {
        return Err(format!("{name}.local is already used by {holder}"));
    }
    lock(&state.store).set_hostname(&name).map_err(|e| e.to_string())?;
    names::rename(mdns, &name);
    let mut view = lock(&state.view);
    view.hostname = name.clone();
    view.notice = None;
    Ok(format!("Renamed. From now on this board is http://{name}.local (this page moves there)."))
}

/// Start the settings page. The returned server must be kept alive.
pub fn start_lan_server(state: Arc<LanState>, mdns: Arc<Mutex<Option<EspMdns>>>) -> Result<EspHttpServer<'static>> {
    let mut server = EspHttpServer::new(&http_config(false))?;

    {
        let state = Arc::clone(&state);
        server.fn_handler("/", Method::Get, move |req: Req<'_, '_>| -> Result<()> {
            send_html(req, 200, &render(&state, None, None))
        })?;
    }
    {
        let state = Arc::clone(&state);
        server.fn_handler("/pills", Method::Post, move |mut req: Req<'_, '_>| -> Result<()> {
            let form = read_form(&mut req)?;
            let result = edit_pills(&state, &form);
            wait_applied(&state);
            let (notice, error) = split(result.map(str::to_string));
            send_html(req, 200, &render(&state, notice.as_deref(), error.as_deref()))
        })?;
    }
    {
        let state = Arc::clone(&state);
        server.fn_handler("/stream", Method::Post, move |mut req: Req<'_, '_>| -> Result<()> {
            let form = read_form(&mut req)?;
            let (notice, error) = split(edit_stream(&state, &form).map(str::to_string));
            send_html(req, 200, &render(&state, notice.as_deref(), error.as_deref()))
        })?;
    }
    {
        let state = Arc::clone(&state);
        let mdns = Arc::clone(&mdns);
        server.fn_handler("/hostname", Method::Post, move |mut req: Req<'_, '_>| -> Result<()> {
            let form = read_form(&mut req)?;
            let (notice, error) = split(edit_hostname(&state, &mdns, &form));
            send_html(req, 200, &render(&state, notice.as_deref(), error.as_deref()))
        })?;
    }
    {
        let state = Arc::clone(&state);
        server.fn_handler("/wifi/reset", Method::Post, move |req: Req<'_, '_>| -> Result<()> {
            if let Err(e) = lock(&state.store).set_portal_requested(true) {
                let msg = format!("Could not save the request: {e}");
                return send_html(req, 200, &render(&state, None, Some(&msg)));
            }
            let ssid = lock(&state.view).setup_ssid.clone();
            let msg = format!(
                "The board is restarting in setup mode. Join the Wi-Fi network {ssid} (the setup password is \
                 redpill1 unless you changed it), and a page opens to choose your Wi-Fi. \
                 Your Pills and Brewfather settings are kept."
            );
            send_html(req, 200, &pages::message_page("Restarting", &msg))?;
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(800));
                esp_idf_svc::hal::reset::restart();
            });
            Ok(())
        })?;
    }

    info!("Settings page is up");
    Ok(server)
}

fn split(r: Result<String, String>) -> (Option<String>, Option<String>) {
    match r {
        Ok(n) => (Some(n), None),
        Err(e) => (None, Some(e)),
    }
}
