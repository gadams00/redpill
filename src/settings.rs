//! Device settings and the pure logic around them: form parsing and validation, the
//! `redpill`, `redpill2`, ... hostname sequence, and the "open the fallback portal?" decision.
//! No ESP dependencies and the clock is passed in, so it is tested on the host
//! (see `scripts/host-test.sh`). Reading and writing NVS lives in `store.rs`.

use crate::pills::Addr;
use std::time::{Duration, Instant};

/// Everything the user can configure. Stored in NVS; `cfg.toml` only seeds it on first boot.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Settings {
    pub wifi_ssid: String,
    pub wifi_pass: String,
    /// The `id=` value of the Brewfather Custom Stream URL.
    pub stream_id: String,
    pub pills: Vec<(Addr, String)>,
}

impl Settings {
    /// True once Wi-Fi credentials exist, i.e. the board can leave setup mode.
    pub fn has_wifi(&self) -> bool {
        !self.wifi_ssid.is_empty()
    }
}

/// If the board has not connected to Wi-Fi even once this long after boot, the setup portal
/// opens alongside normal operation (wrong password, new router).
pub const WIFI_BOOT_GRACE: Duration = Duration::from_secs(5 * 60);
/// A portal opened on purpose (BOOT button) closes by itself after this long when credentials exist.
pub const PORTAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Whether to open the fallback portal: never connected since boot, and the grace period is over.
/// Once the board has connected, later outages never open it.
pub fn fallback_portal_due(boot: Instant, ever_connected: bool, now: Instant) -> bool {
    !ever_connected && now.saturating_duration_since(boot) >= WIFI_BOOT_GRACE
}

// ---- hostnames ----------------------------------------------------------------------------

pub const BASE_HOSTNAME: &str = "redpill";
/// `redpill`, `redpill2`, ... up to this number.
pub const MAX_HOSTNAME_NUMBER: u32 = 9;
/// Longest accepted host name. Short enough that `<name>-setup` fits in a 32-byte SSID.
pub const MAX_HOSTNAME_LEN: usize = 24;

/// The name for position `n` in the sequence: 1 is `redpill`, 2 is `redpill2`, and so on.
fn hostname_for(n: u32) -> String {
    if n <= 1 {
        BASE_HOSTNAME.to_string()
    } else {
        format!("{BASE_HOSTNAME}{n}")
    }
}

/// Position of `name` in the sequence, or `None` if it is not one of the sequence names.
fn hostname_number(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(BASE_HOSTNAME)?;
    if rest.is_empty() {
        return Some(1);
    }
    // `redpill1` and `redpill02` are not sequence names: only 2..=MAX, written plainly.
    if rest.starts_with('0') || rest.len() > 2 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = rest.parse().ok()?;
    (2..=MAX_HOSTNAME_NUMBER).contains(&n).then_some(n)
}

/// The sequence name after `name` (`redpill` -> `redpill2`), or `None` after the last one.
/// A name outside the sequence (a hand-chosen one) continues from the start.
pub fn next_hostname(name: &str) -> Option<String> {
    let n = hostname_number(name).unwrap_or(0) + 1;
    (n <= MAX_HOSTNAME_NUMBER).then(|| hostname_for(n))
}

/// Walk the sequence from `start` (inclusive) and return the first name for which `is_taken`
/// is false. `None` means every name up to `redpill9` is in use.
pub fn claim_hostname(start: &str, mut is_taken: impl FnMut(&str) -> bool) -> Option<String> {
    let mut candidate = Some(start.to_string());
    while let Some(name) = candidate {
        if !is_taken(&name) {
            return Some(name);
        }
        candidate = next_hostname(&name);
    }
    None
}

/// Whether `s` can be used as an mDNS / DHCP host name: one DNS label, lower-case letters,
/// digits and hyphens, not starting or ending with a hyphen.
pub fn valid_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_HOSTNAME_LEN
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Normalise and check a host name typed by the user.
pub fn validate_hostname(input: &str) -> Result<String, String> {
    let name = input.trim().trim_end_matches(".local").to_ascii_lowercase();
    if valid_hostname(&name) {
        Ok(name)
    } else {
        Err(format!(
            "{input:?} is not a valid device name: use 1-24 letters, digits or hyphens (no spaces)"
        ))
    }
}

/// Last two bytes of the MAC in lower-case hex, e.g. `a1b2`.
pub fn mac_suffix(mac: &[u8; 6]) -> String {
    format!("{:02x}{:02x}", mac[4], mac[5])
}

/// Name used when the whole sequence is taken: unique because the MAC is. Never saved.
pub fn fallback_hostname(mac: &[u8; 6]) -> String {
    format!("{BASE_HOSTNAME}-{}", mac_suffix(mac))
}

/// SSID of the setup access point. A board that already has a name uses it (`redpill2-setup`);
/// an unnamed one appends its MAC suffix so two unconfigured boards can't be confused.
pub fn setup_ssid(hostname: Option<&str>, mac: &[u8; 6]) -> String {
    match hostname {
        Some(h) => format!("{h}-setup"),
        None => format!("{BASE_HOSTNAME}-setup-{}", mac_suffix(mac)),
    }
}

// ---- web forms ----------------------------------------------------------------------------

/// Largest form body the handlers read.
pub const MAX_FORM_BYTES: usize = 2048;

/// Decode an `application/x-www-form-urlencoded` body into `(key, value)` pairs.
pub fn parse_form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (url_decode(k), url_decode(v))
        })
        .collect()
}

/// First value for `key`.
pub fn form_get<'a>(form: &'a [(String, String)], key: &str) -> Option<&'a str> {
    form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// Percent-decoding with `+` as space. Malformed escapes are kept literally; invalid UTF-8 is
/// replaced rather than rejected.
pub fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() && hex(bytes[i + 1]).is_some() && hex(bytes[i + 2]).is_some() => {
                out.push(hex(bytes[i + 1]).unwrap() << 4 | hex(bytes[i + 2]).unwrap());
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// Check Wi-Fi credentials from the portal form. An empty password means an open network;
/// otherwise WPA2 needs 8-63 characters.
pub fn validate_wifi(ssid: &str, pass: &str) -> Result<(String, String), String> {
    if ssid.is_empty() {
        return Err("choose or type a Wi-Fi network name".into());
    }
    if ssid.len() > 32 {
        return Err("a Wi-Fi network name is at most 32 bytes".into());
    }
    if !pass.is_empty() && !(8..=63).contains(&pass.len()) {
        return Err("a Wi-Fi password is 8 to 63 characters (leave it empty for an open network)".into());
    }
    Ok((ssid.to_string(), pass.to_string()))
}

/// Accept either the bare Brewfather stream id or the whole Custom Stream URL, and return the id.
/// The id goes into a URL, so only characters that are safe there are allowed.
pub fn validate_stream_id(input: &str) -> Result<String, String> {
    let input = input.trim();
    let id = match input.split_once("id=") {
        Some((_, rest)) => rest.split('&').next().unwrap_or(""),
        None => input,
    };
    if id.is_empty() {
        return Err("the Brewfather stream id is empty".into());
    }
    if id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("the Brewfather stream id should be only letters, digits, - and _ (paste the id= value)".into());
    }
    Ok(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn fallback_portal_opens_only_when_never_connected_after_the_grace_period() {
        let t0 = Instant::now();
        assert!(!fallback_portal_due(t0, false, t0));
        assert!(!fallback_portal_due(t0, false, t0 + WIFI_BOOT_GRACE - secs(1)));
        assert!(fallback_portal_due(t0, false, t0 + WIFI_BOOT_GRACE));
        assert!(fallback_portal_due(t0, false, t0 + secs(3600)));
    }

    #[test]
    fn fallback_portal_never_opens_after_a_successful_connection() {
        let t0 = Instant::now();
        assert!(!fallback_portal_due(t0, true, t0 + secs(3600)));
        assert!(!fallback_portal_due(t0, true, t0 + WIFI_BOOT_GRACE));
    }

    #[test]
    fn hostname_sequence_runs_redpill_then_2_to_9() {
        assert_eq!(next_hostname("redpill").as_deref(), Some("redpill2"));
        assert_eq!(next_hostname("redpill2").as_deref(), Some("redpill3"));
        assert_eq!(next_hostname("redpill8").as_deref(), Some("redpill9"));
        assert_eq!(next_hostname("redpill9"), None);
    }

    #[test]
    fn a_hand_chosen_name_continues_from_the_start_of_the_sequence() {
        assert_eq!(next_hostname("cellar").as_deref(), Some("redpill"));
        assert_eq!(next_hostname("redpill1").as_deref(), Some("redpill"));
        assert_eq!(next_hostname("redpill02").as_deref(), Some("redpill"));
        assert_eq!(next_hostname("redpill10").as_deref(), Some("redpill"));
    }

    #[test]
    fn the_first_board_gets_redpill() {
        assert_eq!(claim_hostname("redpill", |_| false).as_deref(), Some("redpill"));
    }

    #[test]
    fn a_second_board_gets_redpill2() {
        let taken = ["redpill"];
        assert_eq!(claim_hostname("redpill", |n| taken.contains(&n)).as_deref(), Some("redpill2"));
    }

    #[test]
    fn claiming_skips_every_taken_name() {
        let taken = ["redpill", "redpill2", "redpill3"];
        assert_eq!(claim_hostname("redpill", |n| taken.contains(&n)).as_deref(), Some("redpill4"));
    }

    #[test]
    fn claiming_can_fill_a_gap_and_moves_on_from_a_stored_name() {
        // A board that stored `redpill3` finds it taken: it moves on, not back.
        let taken = ["redpill3"];
        assert_eq!(claim_hostname("redpill3", |n| taken.contains(&n)).as_deref(), Some("redpill4"));
        // A first-time board takes the lowest free name.
        let taken = ["redpill", "redpill3"];
        assert_eq!(claim_hostname("redpill", |n| taken.contains(&n)).as_deref(), Some("redpill2"));
    }

    #[test]
    fn claiming_gives_up_when_the_whole_sequence_is_taken() {
        assert_eq!(claim_hostname("redpill", |_| true), None);
    }

    #[test]
    fn a_stored_name_that_is_free_is_kept() {
        let mut asked = Vec::new();
        let got = claim_hostname("redpill2", |n| {
            asked.push(n.to_string());
            false
        });
        assert_eq!(got.as_deref(), Some("redpill2"));
        assert_eq!(asked, ["redpill2"]);
    }

    #[test]
    fn hostname_validation() {
        assert_eq!(validate_hostname(" Cellar-1 "), Ok("cellar-1".to_string()));
        assert_eq!(validate_hostname("redpill.local"), Ok("redpill".to_string()));
        for bad in ["", "-a", "a-", "has space", "under_score", "dot.ted", "ünï", &"x".repeat(MAX_HOSTNAME_LEN + 1)] {
            assert!(validate_hostname(bad).is_err(), "{bad:?}");
        }
        assert!(validate_hostname(&"x".repeat(MAX_HOSTNAME_LEN)).is_ok());
        // The setup network name built from the longest host name must still fit an SSID.
        assert!(setup_ssid(Some(&"x".repeat(MAX_HOSTNAME_LEN)), &[0; 6]).len() <= 32);
        assert!(setup_ssid(None, &[0; 6]).len() <= 32);
    }

    #[test]
    fn names_derived_from_the_mac() {
        let mac = [0x24, 0x6F, 0x28, 0x01, 0xA1, 0x0B];
        assert_eq!(mac_suffix(&mac), "a10b");
        assert_eq!(fallback_hostname(&mac), "redpill-a10b");
        assert!(valid_hostname(&fallback_hostname(&mac)));
        assert_eq!(setup_ssid(None, &mac), "redpill-setup-a10b");
        assert_eq!(setup_ssid(Some("redpill2"), &mac), "redpill2-setup");
    }

    #[test]
    fn form_decoding() {
        let f = parse_form("ssid=My+Home%20Wi-Fi&pass=p%26ss%3Dword%25&empty=&flag");
        assert_eq!(form_get(&f, "ssid"), Some("My Home Wi-Fi"));
        assert_eq!(form_get(&f, "pass"), Some("p&ss=word%"));
        assert_eq!(form_get(&f, "empty"), Some(""));
        assert_eq!(form_get(&f, "flag"), Some(""));
        assert_eq!(form_get(&f, "missing"), None);
    }

    #[test]
    fn form_decoding_handles_utf8_and_malformed_escapes() {
        assert_eq!(url_decode("caf%C3%A9"), "café");
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%zz%4"), "%zz%4");
        assert_eq!(url_decode("%41%"), "A%");
        assert_eq!(url_decode("%FF"), "\u{FFFD}");
    }

    #[test]
    fn wifi_validation() {
        assert_eq!(validate_wifi("Home", "password1"), Ok(("Home".into(), "password1".into())));
        assert_eq!(validate_wifi("Open", ""), Ok(("Open".into(), "".into())));
        assert!(validate_wifi("", "password1").is_err());
        assert!(validate_wifi(&"s".repeat(33), "password1").is_err());
        assert!(validate_wifi("Home", "short").is_err());
        assert!(validate_wifi("Home", &"p".repeat(64)).is_err());
        assert!(validate_wifi("Home", &"p".repeat(63)).is_ok());
    }

    #[test]
    fn stream_id_accepts_the_id_or_the_whole_url() {
        assert_eq!(validate_stream_id(" abc123XYZ "), Ok("abc123XYZ".into()));
        assert_eq!(validate_stream_id("http://log.brewfather.net/stream?id=abc123XYZ"), Ok("abc123XYZ".into()));
        assert_eq!(validate_stream_id("http://log.brewfather.net/stream?id=abc123&x=1"), Ok("abc123".into()));
        for bad in ["", "   ", "has space", "a/b", "a?b", "x&id=", "http://log.brewfather.net/stream?id=", &"a".repeat(65)] {
            assert!(validate_stream_id(bad).is_err(), "{bad:?}");
        }
    }
}
