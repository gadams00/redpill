//! Decoder for RAPT Pill BLE advertisements.
//!
//! The Pill broadcasts manufacturer-specific data with company id 0x4152 ("RA", little-endian
//! on the wire). The payload that follows the company id is 23 bytes, big-endian:
//!
//! ```text
//! "PT"                       (together with the company id this spells "RAPT")
//! v1: version=1, mac[6], temp u16, gravity f32, x i16, y i16, z i16, battery i16
//! v2: version=2, reserved u8, gravity_velocity_valid u8, gravity_velocity f32,
//!     temp u16, gravity f32, x i16, y i16, z i16, battery i16
//! ```
//!
//! Layout taken from the Home Assistant `rapt-ble` parser. Verified against a real v2 Pill:
//! temperature, gravity and battery matched the RAPT app (24.6 C / SG 1.275 vs. 24.3-24.6 C / 1.2765).
//! The Pill also sends a 24-byte manufacturer packet (different content); it is ignored.

pub const COMPANY_ID: u16 = 0x4152;
const PAYLOAD_LEN: usize = 23;
/// Gravity outside this range is treated as junk. The Pill sends wild values (e.g. 86.4 or 55.2)
/// in its first packets after waking up, before the reading settles.
const SG_PLAUSIBLE: core::ops::RangeInclusive<f32> = 0.9..=1.3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub temp_c: f32,
    /// Specific gravity, e.g. 1.050.
    pub gravity: f32,
    /// Battery charge in percent (the Pill does not report volts).
    pub battery_pct: u8,
}

/// Parse the manufacturer-data payload (the bytes after the 2-byte company id).
/// Returns `None` if this isn't a RAPT Pill metrics advertisement.
pub fn parse(payload: &[u8]) -> Option<Reading> {
    if payload.len() != PAYLOAD_LEN || &payload[..2] != b"PT" {
        return None;
    }
    let body = &payload[2..];

    // v1 and v2 differ only in the bytes before the temperature (MAC vs. gravity velocity),
    // so temp/gravity/battery sit at the same offsets in both.
    if !matches!(body[0], 1 | 2) {
        return None;
    }
    let temp_raw = u16::from_be_bytes([body[7], body[8]]);
    let gravity_raw = f32::from_be_bytes([body[9], body[10], body[11], body[12]]);
    let battery_raw = i16::from_be_bytes([body[19], body[20]]);

    let gravity = gravity_raw / 1000.0;
    if !SG_PLAUSIBLE.contains(&gravity) {
        return None;
    }

    Some(Reading {
        temp_c: temp_raw as f32 / 128.0 - 273.15,
        gravity,
        battery_pct: (battery_raw as f32 / 256.0).round().clamp(0.0, 100.0) as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// Synthetic packet: temp 20.5 C, SG 1.050, 85%.
    fn synthetic(version: u8) -> Vec<u8> {
        let mut p = b"PT".to_vec();
        p.push(version);
        if version == 1 {
            p.extend([1, 2, 3, 4, 5, 6]); // MAC
        } else {
            p.extend([0, 1]); // reserved, gravity_velocity_valid
            p.extend(12.5f32.to_be_bytes()); // gravity velocity
        }
        p.extend(37589u16.to_be_bytes());
        p.extend(1050.0f32.to_be_bytes());
        for _ in 0..3 {
            p.extend(16i16.to_be_bytes());
        }
        p.extend(21760i16.to_be_bytes());
        p
    }

    #[test]
    fn real_v2_packet_from_a_pill_in_water() {
        // Captured from a real Pill; RAPT Brew showed 24.6 C and SG 1.275 at the time.
        let r = parse(&hex("50540200014456c95a94bb449f9069022320ad360c5233")).unwrap();
        assert!((r.temp_c - 24.31).abs() < 0.01, "{}", r.temp_c);
        assert!((r.gravity - 1.2765).abs() < 0.0005, "{}", r.gravity);
        assert_eq!(r.battery_pct, 82);
    }

    #[test]
    fn real_v2_packet_with_junk_gravity_is_rejected() {
        // First packet after waking: gravity decodes to 86.4, which is not a real reading.
        assert_eq!(parse(&hex("50540200014456c95a945347a8d169c39e040c04d8522d")), None);
    }

    #[test]
    fn real_24_byte_packet_is_ignored() {
        // The Pill also sends a different 24-byte manufacturer packet.
        assert_eq!(parse(&hex("5054b7fce8c05d747401fffe411b092220107fff02000013")), None);
    }

    #[test]
    fn synthetic_v1_and_v2_decode_the_same_values() {
        for v in [1, 2] {
            let r = parse(&synthetic(v)).unwrap_or_else(|| panic!("v{v} failed to parse"));
            assert!((r.temp_c - 20.514).abs() < 0.01);
            assert!((r.gravity - 1.050).abs() < 1e-6);
            assert_eq!(r.battery_pct, 85);
        }
    }

    #[test]
    fn rejects_unknown_version_wrong_prefix_and_wrong_length() {
        let mut bad_version = synthetic(2);
        bad_version[2] = 9;
        assert_eq!(parse(&bad_version), None);

        let mut bad_prefix = synthetic(2);
        bad_prefix[0] = b'X';
        assert_eq!(parse(&bad_prefix), None);

        assert_eq!(parse(&synthetic(2)[..22]), None);
        let mut long = synthetic(2);
        long.push(0);
        assert_eq!(parse(&long), None);
        assert_eq!(parse(&[]), None);
    }

    #[test]
    fn gravity_range_boundaries() {
        let with_sg = |sg: f32| {
            let mut p = synthetic(2);
            p[11..15].copy_from_slice(&(sg * 1000.0).to_be_bytes());
            parse(&p)
        };
        assert!(with_sg(1.000).is_some());
        assert!(with_sg(0.95).is_some());
        assert!(with_sg(1.25).is_some());
        assert!(with_sg(0.5).is_none());
        assert!(with_sg(1.5).is_none());
        assert!(with_sg(86.4).is_none());
        assert!(with_sg(f32::NAN).is_none());
    }

    #[test]
    fn battery_is_clamped_to_a_percentage() {
        let with_battery = |raw: i16| {
            let mut p = synthetic(2);
            let n = p.len();
            p[n - 2..].copy_from_slice(&raw.to_be_bytes());
            parse(&p).unwrap().battery_pct
        };
        assert_eq!(with_battery(25600), 100);
        assert_eq!(with_battery(32767), 100);
        assert_eq!(with_battery(-5), 0);
        assert_eq!(with_battery(0), 0);
    }
}
