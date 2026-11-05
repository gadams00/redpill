//! Persistent settings in the ESP32's NVS flash. Keys: `ssid`, `pass`, `stream`, `pills`,
//! `hostname` and `portal` (a one-shot "start in setup mode" flag). The Pill list is stored in the
//! same `ADDRESS=Name;...` text as before, so `pills::parse` stays the only parser.

use crate::pills;
use crate::settings::Settings;
use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use log::warn;

const NAMESPACE: &str = "redpill";
/// Big enough for 16 Pills of `AA:BB:CC:DD:EE:FF=` plus a 40-character name.
const MAX_VALUE: usize = 1536;

pub struct Store {
    nvs: EspNvs<NvsDefault>,
}

impl Store {
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        Ok(Store { nvs: EspNvs::new(partition, NAMESPACE, true)? })
    }

    fn get(&self, key: &str) -> Option<String> {
        let mut buf = vec![0u8; MAX_VALUE];
        match self.nvs.get_str(key, &mut buf) {
            Ok(Some(s)) => Some(s.to_string()),
            Ok(None) => None,
            Err(e) => {
                warn!("could not read {key:?} from NVS: {e:?}");
                None
            }
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.nvs.set_str(key, value).map_err(|e| anyhow!("could not write {key:?} to NVS: {e:?}"))
    }

    /// Everything stored. A Pill list that no longer parses is ignored (and logged) rather than
    /// stopping the board from starting.
    pub fn load(&self) -> Settings {
        let pills = match self.get("pills") {
            Some(text) => pills::parse(&text).unwrap_or_else(|e| {
                warn!("stored Pill list is invalid ({e}); starting with none");
                Vec::new()
            }),
            None => Vec::new(),
        };
        Settings {
            wifi_ssid: self.get("ssid").unwrap_or_default(),
            wifi_pass: self.get("pass").unwrap_or_default(),
            stream_id: self.get("stream").unwrap_or_default(),
            pills,
        }
    }

    pub fn save(&self, s: &Settings) -> Result<()> {
        self.set("ssid", &s.wifi_ssid)?;
        self.set("pass", &s.wifi_pass)?;
        self.set("stream", &s.stream_id)?;
        self.set("pills", &pills::format(&s.pills))
    }

    /// Save only the parts the Wi-Fi form changes.
    pub fn save_wifi(&self, ssid: &str, pass: &str) -> Result<()> {
        self.set("ssid", ssid)?;
        self.set("pass", pass)
    }

    pub fn hostname(&self) -> Option<String> {
        self.get("hostname").filter(|h| crate::settings::valid_hostname(h))
    }

    pub fn set_hostname(&self, name: &str) -> Result<()> {
        self.set("hostname", name)
    }

    /// True if the user asked (web page button) for the next boot to be in setup mode.
    pub fn portal_requested(&self) -> bool {
        self.get("portal").as_deref() == Some("1")
    }

    pub fn set_portal_requested(&self, requested: bool) -> Result<()> {
        self.set("portal", if requested { "1" } else { "0" })
    }
}
