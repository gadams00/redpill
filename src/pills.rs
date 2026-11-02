//! Parsing of the configured Pill list.
//!
//! The list comes from `cfg.toml` as one string: `ADDRESS=Name;ADDRESS=Name`, e.g.
//! `FC:E8:C0:5D:74:76=Fermenter 1;AA:BB:CC:DD:EE:FF=Fermenter 2`. The name is what Brewfather
//! sees as the device name, so each Pill gets its own device (and its own 15-minute post limit).
//! An empty list is valid: the firmware then runs in discovery mode and only logs the Pills it hears.

/// Bluetooth address in display order (the order it is written and logged in).
pub type Addr = [u8; 6];

pub fn parse(spec: &str) -> Result<Vec<(Addr, String)>, String> {
    let mut pills: Vec<(Addr, String)> = Vec::new();

    for entry in spec.split(';').map(str::trim).filter(|e| !e.is_empty()) {
        let (addr_str, name) = entry
            .split_once('=')
            .ok_or_else(|| format!("pill entry {entry:?} must look like ADDRESS=Name"))?;
        let addr = parse_addr(addr_str.trim())?;
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("pill {addr_str:?} has an empty name"));
        }
        if pills.iter().any(|(a, _)| *a == addr) {
            return Err(format!("pill address {addr_str:?} is listed twice"));
        }
        if pills.iter().any(|(_, n)| n == name) {
            return Err(format!("pill name {name:?} is used twice; Brewfather needs unique names"));
        }
        pills.push((addr, name.to_string()));
    }

    Ok(pills)
}

fn parse_addr(s: &str) -> Result<Addr, String> {
    let mut out = [0u8; 6];
    let mut parts = s.split([':', '-']);
    for byte in out.iter_mut() {
        let part = parts.next().ok_or_else(|| format!("bad address {s:?}: expected 6 bytes"))?;
        if part.len() != 2 {
            return Err(format!("bad address {s:?}: each byte must be two hex digits"));
        }
        *byte = u8::from_str_radix(part, 16).map_err(|_| format!("bad address {s:?}: not hex"))?;
    }
    if parts.next().is_some() {
        return Err(format!("bad address {s:?}: more than 6 bytes"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Addr = [0xFC, 0xE8, 0xC0, 0x5D, 0x74, 0x76];

    #[test]
    fn single_pill() {
        assert_eq!(parse("FC:E8:C0:5D:74:76=Yellow Pill 1"), Ok(vec![(A, "Yellow Pill 1".to_string())]));
    }

    #[test]
    fn several_pills_with_spaces_dashes_lowercase_and_trailing_semicolon() {
        let got = parse("FC:E8:C0:5D:74:76=Fermenter 1; aa-bb-cc-dd-ee-ff = Fermenter 2 ;").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (A, "Fermenter 1".to_string()));
        assert_eq!(got[1], ([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF], "Fermenter 2".to_string()));
    }

    #[test]
    fn empty_is_valid_discovery_mode() {
        assert_eq!(parse(""), Ok(vec![]));
        assert_eq!(parse("  ;  ; "), Ok(vec![]));
    }

    #[test]
    fn bad_entries_are_rejected_with_a_message() {
        for bad in [
            "FC:E8:C0:5D:74:76",       // no name
            "FC:E8:C0:5D:74:76=",      // empty name
            "FC:E8:C0:5D:74:76=  ",    // blank name
            "FC:E8:C0:5D:74=Short",    // 5 bytes
            "FC:E8:C0:5D:74:76:99=Long", // 7 bytes
            "FC:E8:C0:5D:74:ZZ=Nothex",
            "FC:E8:C0:5D:74:7=OneDigit",
            "FCE8C05D7476=NoColons",
            "=NoAddress",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn duplicate_address_or_name_is_rejected() {
        assert!(parse("FC:E8:C0:5D:74:76=A;FC:E8:C0:5D:74:76=B").unwrap_err().contains("twice"));
        assert!(parse("FC:E8:C0:5D:74:76=A;11:22:33:44:55:66=A").unwrap_err().contains("twice"));
    }

    #[test]
    fn name_may_contain_equals_and_punctuation() {
        let got = parse("FC:E8:C0:5D:74:76=Batch #4 (IPA) = good").unwrap();
        assert_eq!(got[0].1, "Batch #4 (IPA) = good");
    }
}
