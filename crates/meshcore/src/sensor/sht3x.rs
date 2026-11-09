//! Sensirion SHT3x (SHT30, SHT31, SHT35) temperature + humidity sensor; upstream has no driver for
//! it (its SHT4X one, at the same address, does not read it).
//!
//! The reply is shaped as the SHT4x's — two 16-bit words, temperature first, each with a
//! Sensirion CRC-8 — with the SHT3x scaling (datasheet §4.13): T = -45 + 175·S/65535,
//! RH = 100·S/65535.

use alloc::vec::Vec;

use crate::lpp;
use crate::sensor::sensirion_crc8;

/// Default I2C address (ADDR pin low; `0x45` with it high, probed too by the driver).
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
    Some(Reading { temp_c: -45.0 + 175.0 * raw_t / 65535.0, humidity_pct: 100.0 * raw_h / 65535.0 })
}

/// Append the reading as the other humidity sensors do: temperature then humidity.
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
        // raw_t 0x6666 -> 25 °C; raw_h midscale -> 50 %RH
        let r = decode(&reply(0x6666, 0x8000)).expect("valid crc");
        assert!((r.temp_c - 25.0).abs() < 0.02, "t {}", r.temp_c);
        assert!((r.humidity_pct - 50.0).abs() < 0.01, "rh {}", r.humidity_pct);
        // the datasheet's CRC example: 0xBEEF -> 0x92
        assert_eq!(sensirion_crc8(&[0xBE, 0xEF]), 0x92);
    }

    #[test]
    fn rejects_a_bad_crc() {
        let mut bad = reply(0x6666, 0x8000);
        bad[2] ^= 0x01;
        assert!(decode(&bad).is_none());
    }
}
