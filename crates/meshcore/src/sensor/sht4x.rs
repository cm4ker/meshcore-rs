//! Sensirion SHT4x temperature + humidity sensor (upstream `ENV_INCLUDE_SHT4X`).
//!
//! Same reply shape as the SHTC3 — two 16-bit words, temperature first, each with a Sensirion
//! CRC-8 — but a different scaling and a /65535 full scale (datasheet §4, as Adafruit_SHT4x uses),
//! and humidity is clamped to 0..100 because its raw-to-%RH line slightly overshoots at the ends.

use alloc::vec::Vec;

use crate::lpp;
use crate::sensor::sensirion_crc8;

/// Default I2C address (the `-A` parts; `-B` variants sit at `0x45`, probed too by the driver).
pub const ADDR: u8 = 0x44;

/// A reading in physical units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    pub temp_c: f32,
    pub humidity_pct: f32,
}

/// Decode the six reply bytes (`[t, t, crc, rh, rh, crc]`, temperature first). `None` if either
/// CRC fails.
pub fn decode(data: &[u8; 6]) -> Option<Reading> {
    if sensirion_crc8(&data[0..2]) != data[2] || sensirion_crc8(&data[3..5]) != data[5] {
        return None;
    }
    let raw_t = u16::from_be_bytes([data[0], data[1]]) as f32;
    let raw_h = u16::from_be_bytes([data[3], data[4]]) as f32;
    Some(Reading {
        temp_c: -45.0 + 175.0 * raw_t / 65535.0,
        humidity_pct: (-6.0 + 125.0 * raw_h / 65535.0).clamp(0.0, 100.0),
    })
}

/// Append the reading as upstream does: temperature then humidity.
pub fn push_lpp(reading: &Reading, channel: u8, out: &mut Vec<u8>) {
    lpp::push(out, channel, lpp::TEMPERATURE, reading.temp_c);
    lpp::push(out, channel, lpp::RELATIVE_HUMIDITY, reading.humidity_pct);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(raw_t: u16, raw_h: u16) -> [u8; 6] {
        let t = raw_t.to_be_bytes();
        let h = raw_h.to_be_bytes();
        [t[0], t[1], sensirion_crc8(&t), h[0], h[1], sensirion_crc8(&h)]
    }

    #[test]
    fn decodes_a_valid_reply() {
        // raw_t ~0x6666 -> ~25 °C; raw_h midscale -> ~56.5 %RH on the −6..119 line.
        let r = decode(&reply(0x6666, 0x8000)).expect("valid crc");
        assert!((r.temp_c - 25.0).abs() < 0.02, "t {}", r.temp_c);
        assert!((r.humidity_pct - 56.5).abs() < 0.1, "rh {}", r.humidity_pct);
    }

    #[test]
    fn humidity_clamps_at_the_ends() {
        let r = decode(&reply(0x6666, 0x0000)).unwrap();
        assert_eq!(r.humidity_pct, 0.0); // −6 %RH raw, clamped
        let r = decode(&reply(0x6666, 0xFFFF)).unwrap();
        assert_eq!(r.humidity_pct, 100.0); // 119 %RH raw, clamped
    }

    #[test]
    fn rejects_a_bad_crc() {
        let mut bad = reply(0x6666, 0x8000);
        bad[5] ^= 0xFF;
        assert!(decode(&bad).is_none());
    }
}
