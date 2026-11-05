//! HTML for the setup portal and the settings page. Pure string building, so the escaping and
//! the content can be tested on the host. Every user- or air-supplied string (SSIDs, Pill names,
//! host names of other boards) goes through `escape`.

use crate::fleet::{PillStatus, UnlistedPill};
use crate::pills::format_addr;
use crate::rapt::Reading;
use std::time::Duration;

/// Escape text for use in HTML content or a double-quoted attribute.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// "just now", "5 min ago", "2 h ago".
pub fn fmt_age(age: Duration) -> String {
    let s = age.as_secs();
    if s < 60 {
        "just now".to_string()
    } else if s < 3600 {
        format!("{} min ago", s / 60)
    } else {
        format!("{} h ago", s / 3600)
    }
}

fn fmt_reading(r: &Reading, rssi: i8, age: Duration) -> String {
    format!(
        "{:.1} &deg;C &middot; SG {:.4} &middot; battery {}% &middot; {} dBm &middot; {}",
        r.temp_c,
        r.gravity,
        r.battery_pct,
        rssi,
        fmt_age(age)
    )
}

const STYLE: &str = "<style>\
body{font:16px system-ui,sans-serif;max-width:40rem;margin:0 auto;padding:1rem;line-height:1.4;color:#222;background:#fafafa}\
h1{font-size:1.4rem}h2{font-size:1.1rem;margin-top:1.8rem}\
input,select,button{font:inherit;padding:.4rem;margin:.15rem 0;box-sizing:border-box}\
input[type=text],input[type=password]{width:100%}\
button{cursor:pointer}\
table{width:100%;border-collapse:collapse}td{padding:.4rem .2rem;border-top:1px solid #ccc;vertical-align:top}\
.err{background:#fde;border:1px solid #c33;padding:.5rem;border-radius:4px}\
.ok{background:#efe;border:1px solid #3a3;padding:.5rem;border-radius:4px}\
.dim{color:#666;font-size:.9rem}code{background:#eee;padding:0 .2rem}\
@media(prefers-color-scheme:dark){body{color:#ddd;background:#181818}td{border-color:#444}code{background:#333}.dim{color:#999}input,select,button{background:#2a2a2a;color:#ddd;border:1px solid #555}}\
</style>";

fn page(title: &str, extra_head: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{}</title>{}{}</head><body>{}</body></html>",
        escape(title),
        STYLE,
        extra_head,
        body
    )
}

fn banner(notice: Option<&str>, error: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(e) = error {
        out.push_str(&format!("<p class=err>{}</p>", escape(e)));
    }
    if let Some(n) = notice {
        out.push_str(&format!("<p class=ok>{}</p>", escape(n)));
    }
    out
}

/// A plain page with a heading and one paragraph (restart screens, errors).
pub fn message_page(title: &str, message: &str) -> String {
    page(title, "", &format!("<h1>{}</h1><p>{}</p>", escape(title), escape(message)))
}

// ---- setup portal --------------------------------------------------------------------------

/// Progress of joining the home network from the portal.
#[derive(Debug, Clone, PartialEq)]
pub enum Join {
    Idle,
    Trying { ssid: String },
    Connected { hostname: String, ip: String },
    Failed { reason: String },
}

/// The Wi-Fi form. `networks` are the SSIDs found by a scan, offered as a list; the text field
/// below it takes a typed name (hidden networks, or a scan that found nothing). `next_name` is
/// what the board expects to be called afterwards.
pub fn portal_form(networks: &[String], error: Option<&str>, next_name: &str) -> String {
    let choose = if networks.is_empty() {
        "<p class=dim>No networks were found by the scan. Press Rescan, or type the name below.</p>".to_string()
    } else {
        let mut options = String::from("<option value=\"\">Choose a network&hellip;</option>");
        for n in networks {
            options.push_str(&format!("<option value=\"{0}\">{0}</option>", escape(n)));
        }
        format!("<label>Network<br><select name=ssid>{options}</select></label><br>")
    };
    let body = format!(
        "<h1>redpill setup</h1>{}\
         <p>Choose the 2.4 GHz Wi-Fi network this board should use to reach the internet.</p>\
         <form method=post action=/save>{}\
         <label>Or type the network name<br><input type=text name=ssid_other autocapitalize=off autocomplete=off></label><br>\
         <label>Password<br><input type=password name=pass autocomplete=off></label><br>\
         <button type=submit>Connect</button> <a href=/rescan>Rescan</a></form>\
         <p><b>Your phone will disconnect from this setup network when you press Connect.</b> \
         That is normal: the board has to switch to your Wi-Fi's channel. Wait about 30 seconds, \
         rejoin your home Wi-Fi, then open <code>http://{}.local</code>. \
         If you already have a redpill, this one may be <code>redpill2.local</code> instead; \
         every board's page lists the others.</p>",
        banner(None, error),
        choose,
        escape(next_name)
    );
    page("redpill setup", "", &body)
}

/// Shown after the form is submitted, and reloaded by the browser until the attempt finishes.
pub fn portal_join(join: &Join) -> String {
    match join {
        Join::Idle => page("redpill setup", "", "<p>Nothing to do. <a href=/>Back</a></p>"),
        Join::Trying { ssid } => page(
            "Connecting",
            "<meta http-equiv=refresh content=2>",
            &format!("<h1>Connecting&hellip;</h1><p>Joining <b>{}</b>. This takes a few seconds.</p>", escape(ssid)),
        ),
        Join::Connected { hostname, ip } => page(
            "Connected",
            "",
            &format!(
                "<h1>Connected</h1><p class=ok>This board is <a href=\"http://{h}.local\">http://{h}.local</a> \
                 (or <a href=\"http://{ip}\">http://{ip}</a>) on your home network.</p>\
                 <p>Join your home Wi-Fi again and open that address to name your Pills. \
                 This setup network closes in 30 seconds.</p>",
                h = escape(hostname),
                ip = escape(ip)
            ),
        ),
        Join::Failed { reason } => page(
            "Could not connect",
            "",
            &format!("<h1>Could not connect</h1><p class=err>{}</p><p><a href=/>Try again</a></p>", escape(reason)),
        ),
    }
}

// ---- settings page -------------------------------------------------------------------------

pub struct SettingsView<'a> {
    pub hostname: &'a str,
    pub ip: &'a str,
    pub stream_id: &'a str,
    pub pills: &'a [PillStatus],
    pub unlisted: &'a [UnlistedPill],
    /// `(host name, ip)` of other redpill boards found on the network.
    pub others: &'a [(String, String)],
    pub notice: Option<&'a str>,
    pub error: Option<&'a str>,
}

pub fn settings_page(v: &SettingsView) -> String {
    let mut b = String::new();
    b.push_str(&format!(
        "<h1>{h}.local</h1><p class=dim>{ip}</p>{}",
        banner(v.notice, v.error),
        h = escape(v.hostname),
        ip = escape(v.ip)
    ));

    // Configured Pills.
    b.push_str("<h2>Your Pills</h2>");
    if v.pills.is_empty() {
        b.push_str("<p>No Pills yet. Wake one by double-tapping it, then name it below.</p>");
    } else {
        b.push_str("<table>");
        for p in v.pills {
            let addr = format_addr(&p.addr);
            let last = match &p.last {
                Some((r, rssi, age)) => fmt_reading(r, *rssi, *age),
                None => "not heard since boot".to_string(),
            };
            b.push_str(&format!(
                "<tr><td><form method=post action=/pills>\
                 <input type=hidden name=addr value=\"{a}\">\
                 <input type=text name=name value=\"{n}\" maxlength=40 required>\
                 <button name=action value=save>Rename</button> \
                 <button name=action value=remove>Remove</button>\
                 <div class=dim>{a} &middot; {last}</div></form></td></tr>",
                a = escape(&addr),
                n = escape(&p.name),
                last = last
            ));
        }
        b.push_str("</table>");
    }

    // Heard but not configured.
    b.push_str("<h2>Heard nearby</h2>");
    if v.unlisted.is_empty() {
        b.push_str("<p class=dim>No other Pills heard in the last 20 minutes. Double-tap a Pill to wake it.</p>");
    } else {
        b.push_str(
            "<p class=dim>Name a Pill to start posting it to Brewfather. \
             Add each Pill to one board only, or Brewfather gets its readings twice.</p><table>",
        );
        for u in v.unlisted {
            let addr = format_addr(&u.addr);
            b.push_str(&format!(
                "<tr><td><form method=post action=/pills>\
                 <input type=hidden name=addr value=\"{a}\">\
                 <input type=text name=name placeholder=\"Name, e.g. Fermenter 1\" maxlength=40 required>\
                 <button name=action value=save>Add</button>\
                 <div class=dim>{a} &middot; {r}</div></form></td></tr>",
                a = escape(&addr),
                r = fmt_reading(&u.reading, u.rssi, u.age)
            ));
        }
        b.push_str("</table>");
    }

    // A Pill that has not been heard yet (asleep) can still be added by address.
    b.push_str(
        "<details><summary>Add a Pill by address</summary>\
         <form method=post action=/pills>\
         <input type=text name=addr placeholder=\"AA:BB:CC:DD:EE:FF\" required>\
         <input type=text name=name placeholder=\"Name\" maxlength=40 required>\
         <button name=action value=save>Add</button></form></details>",
    );

    // Brewfather.
    b.push_str(&format!(
        "<h2>Brewfather</h2><form method=post action=/stream>\
         <label>Stream id, or the whole Custom Stream URL<br>\
         <input type=text name=stream value=\"{}\" required autocomplete=off></label>\
         <button type=submit>Save</button></form>",
        escape(v.stream_id)
    ));

    // This board.
    b.push_str(&format!(
        "<h2>This board</h2><form method=post action=/hostname>\
         <label>Device name<br><input type=text name=hostname value=\"{}\" maxlength=24 required></label>\
         <button type=submit>Rename</button></form>\
         <form method=post action=/wifi/reset onsubmit=\"return confirm('Restart in setup mode to change Wi-Fi?')\">\
         <button type=submit>Change Wi-Fi network</button></form>",
        escape(v.hostname)
    ));

    if !v.others.is_empty() {
        b.push_str("<h2>Other redpill boards on this network</h2><ul>");
        for (host, ip) in v.others {
            b.push_str(&format!(
                "<li><a href=\"http://{h}.local\">{h}.local</a> <span class=dim>{ip}</span></li>",
                h = escape(host),
                ip = escape(ip)
            ));
        }
        b.push_str("</ul>");
    }

    b.push_str(
        "<p class=dim>Anyone on this network can open this page. Wi-Fi passwords are never shown.</p>",
    );
    page(&format!("{}.local", v.hostname), "", &b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: [u8; 6] = [0xFC, 0xE8, 0xC0, 0x5D, 0x74, 0x76];
    const B: [u8; 6] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];

    fn reading() -> Reading {
        Reading { temp_c: 19.46, gravity: 1.0123, battery_pct: 87 }
    }

    fn view<'a>(pills: &'a [PillStatus], unlisted: &'a [UnlistedPill], others: &'a [(String, String)]) -> SettingsView<'a> {
        SettingsView {
            hostname: "redpill",
            ip: "192.168.1.50",
            stream_id: "abc123",
            pills,
            unlisted,
            others,
            notice: None,
            error: None,
        }
    }

    #[test]
    fn escape_neutralises_html() {
        assert_eq!(escape("<script>alert('x')</script>"), "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;");
        assert_eq!(escape("\"quoted\" & more"), "&quot;quoted&quot; &amp; more");
        assert_eq!(escape("plain Ünïcode"), "plain Ünïcode");
    }

    #[test]
    fn ages_are_readable() {
        assert_eq!(fmt_age(Duration::from_secs(5)), "just now");
        assert_eq!(fmt_age(Duration::from_secs(59)), "just now");
        assert_eq!(fmt_age(Duration::from_secs(60)), "1 min ago");
        assert_eq!(fmt_age(Duration::from_secs(19 * 60 + 59)), "19 min ago");
        assert_eq!(fmt_age(Duration::from_secs(2 * 3600)), "2 h ago");
    }

    #[test]
    fn pill_names_ssids_and_stream_ids_cannot_inject_html() {
        let evil = "\"><script>alert(1)</script>";
        let pills = [PillStatus { addr: A, name: evil.into(), last: None }];
        let mut v = view(&pills, &[], &[]);
        v.stream_id = evil;
        let html = settings_page(&v);
        assert!(!html.contains("<script>"), "{html}");

        let form = portal_form(&[evil.to_string()], Some(evil), evil);
        assert!(!form.contains("<script>"));
        let joined = portal_join(&Join::Trying { ssid: evil.into() });
        assert!(!joined.contains("<script>"));
        let failed = portal_join(&Join::Failed { reason: evil.into() });
        assert!(!failed.contains("<script>"));
        let done = portal_join(&Join::Connected { hostname: evil.into(), ip: evil.into() });
        assert!(!done.contains("<script>"));

        let others = [(evil.to_string(), evil.to_string())];
        assert!(!settings_page(&view(&[], &[], &others)).contains("<script>"));
    }

    #[test]
    fn configured_pill_shows_name_address_and_last_reading() {
        let pills = [PillStatus { addr: A, name: "Yellow Pill 1".into(), last: Some((reading(), -64, Duration::from_secs(180))) }];
        let html = settings_page(&view(&pills, &[], &[]));
        assert!(html.contains("value=\"Yellow Pill 1\""));
        assert!(html.contains("FC:E8:C0:5D:74:76"));
        assert!(html.contains("19.5 &deg;C"), "temperature");
        assert!(html.contains("SG 1.0123"));
        assert!(html.contains("87%"));
        assert!(html.contains("-64 dBm"));
        assert!(html.contains("3 min ago"));
        assert!(html.contains("value=remove"));
    }

    #[test]
    fn unheard_pill_says_so() {
        let pills = [PillStatus { addr: A, name: "P".into(), last: None }];
        assert!(settings_page(&view(&pills, &[], &[])).contains("not heard since boot"));
    }

    #[test]
    fn unlisted_pill_gets_an_add_form_for_its_address() {
        let unlisted = [UnlistedPill { addr: B, rssi: -71, reading: reading(), age: Duration::from_secs(30) }];
        let html = settings_page(&view(&[], &unlisted, &[]));
        assert!(html.contains("name=addr value=\"AA:BB:CC:DD:EE:FF\""));
        assert!(html.contains("value=save>Add"));
        assert!(html.contains("-71 dBm"));
        assert!(html.contains("one board only"), "duplicate-posting warning");
    }

    #[test]
    fn empty_states_are_explained() {
        let html = settings_page(&view(&[], &[], &[]));
        assert!(html.contains("No Pills yet"));
        assert!(html.contains("No other Pills heard"));
        assert!(!html.contains("Other redpill boards"));
    }

    #[test]
    fn other_boards_are_linked() {
        let others = [("redpill2".to_string(), "192.168.1.51".to_string())];
        let html = settings_page(&view(&[], &[], &others));
        assert!(html.contains("Other redpill boards"));
        assert!(html.contains("href=\"http://redpill2.local\""));
        assert!(html.contains("192.168.1.51"));
    }

    #[test]
    fn page_shows_name_ip_and_messages_but_no_wifi_password_field() {
        let mut v = view(&[], &[], &[]);
        v.notice = Some("Saved");
        v.error = Some("Bad thing");
        let html = settings_page(&v);
        assert!(html.contains("<h1>redpill.local</h1>"));
        assert!(html.contains("192.168.1.50"));
        assert!(html.contains("class=ok>Saved"));
        assert!(html.contains("class=err>Bad thing"));
        assert!(!html.contains("<input type=password"), "the stylesheet mentions the selector, but no field");
        assert!(!html.contains("name=pass"));
    }

    #[test]
    fn portal_form_lists_scanned_networks_and_always_allows_typing_one() {
        let nets = ["Home".to_string(), "Neighbour 5G".to_string()];
        let html = portal_form(&nets, Some("Wrong password?"), "redpill");
        assert!(html.contains("<select name=ssid>"));
        assert!(html.contains("<option value=\"Home\">Home</option>"));
        assert!(html.contains("<option value=\"Neighbour 5G\">Neighbour 5G</option>"));
        assert!(html.contains("name=ssid_other"), "typed name field");
        assert!(html.contains("Wrong password?"));
        assert!(html.contains("<input type=password name=pass"));
        assert!(html.contains("href=/rescan"));
    }

    #[test]
    fn portal_form_without_scan_results_says_so_and_still_works() {
        let html = portal_form(&[], None, "redpill");
        assert!(!html.contains("<select"));
        assert!(html.contains("No networks were found"));
        assert!(html.contains("name=ssid_other"));
    }

    #[test]
    fn portal_form_warns_about_the_expected_disconnect_and_names_the_next_address() {
        let html = portal_form(&[], None, "redpill2");
        assert!(html.contains("will disconnect"));
        assert!(html.contains("http://redpill2.local"));
    }

    #[test]
    fn message_page_escapes_both_parts() {
        let html = message_page("<b>T</b>", "<i>m</i>");
        assert!(html.contains("&lt;b&gt;T&lt;/b&gt;") && html.contains("&lt;i&gt;m&lt;/i&gt;"));
        assert!(!html.contains("<b>T"));
    }

    #[test]
    fn join_progress_pages() {
        assert!(portal_join(&Join::Trying { ssid: "Home".into() }).contains("http-equiv=refresh"));
        let done = portal_join(&Join::Connected { hostname: "redpill2".into(), ip: "10.0.0.7".into() });
        assert!(done.contains("http://redpill2.local"));
        assert!(done.contains("http://10.0.0.7"));
        assert!(!done.contains("http-equiv=refresh"), "stops reloading once finished");
        let failed = portal_join(&Join::Failed { reason: "no such network".into() });
        assert!(failed.contains("no such network"));
        assert!(!failed.contains("http-equiv=refresh"));
    }
}
