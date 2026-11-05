//! The board's network name (`redpill.local`, `redpill2.local`, ...): finding a free one on the
//! LAN, announcing it over mDNS, noticing a conflict, and listing the other redpill boards.
//! The choice itself is `settings::claim_hostname`; this file is the ESP/mDNS side of it.
//! None of this is needed for posting readings: every failure here is only logged.

use crate::settings;
use esp_idf_svc::mdns::{EspMdns, Interface, Protocol, QueryResult};
use log::warn;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

const QUERY_TIMEOUT: Duration = Duration::from_millis(1500);
/// A single mDNS question can go unanswered (packet loss, sleepy device), so ask twice.
const QUERY_TRIES: usize = 2;
/// Port of the settings page.
pub const HTTP_PORT: u16 = 80;

/// The address of another device that answers for `<name>.local`, or `None` if nobody does.
/// Answers from `own_ip` are ignored: that is this board.
pub fn holder_of(mdns: &EspMdns, name: &str, own_ip: Ipv4Addr) -> Option<Ipv4Addr> {
    for _ in 0..QUERY_TRIES {
        if let Ok(ip) = mdns.query_a(name, QUERY_TIMEOUT) {
            if ip != own_ip {
                return Some(ip);
            }
        }
    }
    None
}

/// The result of picking a name.
pub struct Chosen {
    pub name: String,
    /// Whether to save it. The MAC-based last resort is not saved, so the next boot tries the
    /// normal sequence again.
    pub persist: bool,
    /// Something to tell the user on the settings page (the old name was taken, ...).
    pub note: Option<String>,
}

/// Pick this board's name: the saved one if nobody else uses it, otherwise the next free one in
/// `redpill`, `redpill2`, ... `redpill9`. The board that already holds a name keeps it; only a
/// newcomer moves on.
pub fn choose(mdns: &EspMdns, stored: Option<&str>, own_ip: Ipv4Addr, mac: &[u8; 6]) -> Chosen {
    let mut first_holder: Option<(String, Ipv4Addr)> = None;
    let start = stored.unwrap_or(settings::BASE_HOSTNAME);
    let found = settings::claim_hostname(start, |candidate| match holder_of(mdns, candidate, own_ip) {
        Some(ip) => {
            first_holder.get_or_insert((candidate.to_string(), ip));
            true
        }
        None => false,
    });
    match found {
        Some(name) => {
            let note = match (&first_holder, stored) {
                (Some((was, ip)), Some(_)) => {
                    Some(format!("the name {was}.local is in use by {ip}, so this board is now {name}.local"))
                }
                _ => None,
            };
            Chosen { name, persist: true, note }
        }
        None => {
            let name = settings::fallback_hostname(mac);
            Chosen {
                note: Some(format!(
                    "every name from {} to {}{} is in use on this network; using {name}.local",
                    settings::BASE_HOSTNAME,
                    settings::BASE_HOSTNAME,
                    settings::MAX_HOSTNAME_NUMBER
                )),
                name,
                persist: false,
            }
        }
    }
}

/// Announce `name` and the two services (the settings page, and a marker other boards can find).
pub fn announce(mdns: &mut EspMdns, name: &str) {
    let version = env!("CARGO_PKG_VERSION");
    let results = [
        mdns.set_hostname(name),
        mdns.set_instance_name(name),
        mdns.add_service(Some(name), "_http", "_tcp", HTTP_PORT, &[]),
        mdns.add_service(Some(name), "_redpill", "_tcp", HTTP_PORT, &[("version", version)]),
    ];
    for r in results {
        if let Err(e) = r {
            warn!("mDNS setup step failed: {e:?}");
        }
    }
}

/// Switch an announced board to a new name.
pub fn rename(mdns: &mut EspMdns, name: &str) {
    let results = [
        mdns.set_hostname(name),
        mdns.set_instance_name(name),
        mdns.set_service_instance_name("_http", "_tcp", name),
        mdns.set_service_instance_name("_redpill", "_tcp", name),
    ];
    for r in results {
        if let Err(e) = r {
            warn!("mDNS rename step failed: {e:?}");
        }
    }
}

/// The host name the mDNS component is using right now. It differs from what we set if it lost a
/// name conflict and renamed itself.
pub fn current_hostname() -> Option<String> {
    let mut buf = [0 as core::ffi::c_char; 64];
    let err = unsafe { esp_idf_svc::sys::mdns_hostname_get(buf.as_mut_ptr()) };
    if err != 0 {
        return None;
    }
    let cstr = unsafe { core::ffi::CStr::from_ptr(buf.as_ptr()) };
    cstr.to_str().ok().map(str::to_string)
}

/// Other redpill boards on the network as `(host name, ip)`, without this one.
pub fn others(mdns: &EspMdns, own_name: &str) -> Vec<(String, String)> {
    let mut results: Vec<QueryResult> = (0..8)
        .map(|_| QueryResult {
            instance_name: None,
            hostname: None,
            port: 0,
            txt: Vec::new(),
            addr: Vec::new(),
            interface: Interface::STA,
            ip_protocol: Protocol::V4,
        })
        .collect();
    let n = match mdns.query_ptr("_redpill", "_tcp", QUERY_TIMEOUT, results.len(), &mut results) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for r in results.into_iter().take(n) {
        let Some(host) = r.hostname else { continue };
        let host = host.trim_end_matches(".local").to_string();
        if host == own_name || !settings::valid_hostname(&host) || out.iter().any(|(h, _)| *h == host) {
            continue;
        }
        let ip = r
            .addr
            .iter()
            .find(|a| matches!(a, IpAddr::V4(_)))
            .map(|a| a.to_string())
            .unwrap_or_default();
        out.push((host, ip));
    }
    out.sort();
    out
}
