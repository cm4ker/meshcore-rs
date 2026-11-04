//! Sensirion SHTC3 temperature + humidity sensor (upstream `ENV_INCLUDE_SHTC3`).
//!
//! The reply is two 16-bit words (temperature first), each followed by a Sensirion CRC-8 check
//! byte. The scaling is linear (datasheet §4, as Adafruit_SHTC3 uses). The sans-IO part decodes
//! and validates those six bytes; the firmware wakes the chip, sends the measure command and
//! reads the reply.

use alloc::vec::Vec;

use crate::lpp;
use crate::sensor::sensirion_crc8;

/// Fixed I2C address.
pub const ADDR: u8 = 0x70;

/// A reading in physical units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    pub temp_c: f32,
    pub humidity_pct: f32,
}

/// Decode the six reply bytes (`[t, t, crc, rh, rh, crc]`, temperature first). `None` if either
/// CRC fails, so a garbled transfer drops out rather than reporting a wild value.
pub fn decode(data: &[u8; 6]) -> Option<Reading> {
    if sensirion_crc8(&data[0..2]) != data[2] || sensirion_crc8(&data[3..5]) != data[5] {
        return None;
    }
    let raw_t = u16::from_be_bytes([data[0], data[1]]) as f32;
    let raw_h = u16::from_be_bytes([data[3], data[4]]) as f32;
    Some(Reading {
        temp_c: -45.0 + 175.0 * raw_t / 65536.0,
        humidity_pct: 100.0 * raw_h / 65536.0,
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

    // Build a valid reply from two raw words, appending the CRC the sensor would.
    fn reply(raw_t: u16, raw_h: u16) -> [u8; 6] {
        let t = raw_t.to_be_bytes();
        let h = raw_h.to_be_bytes();
        [t[0], t[1], sensirion_crc8(&t), h[0], h[1], sensirion_crc8(&h)]
    }

    #[test]
    fn decodes_a_valid_reply() {
        // raw_t = 0x6666 -> −45 + 175·0.4 = 25 °C; raw_h = 0x6666 -> 40 %RH.
        let r = decode(&reply(0x6666, 0x6666)).expect("valid crc");
        assert!((r.temp_c - 25.0).abs() < 0.01, "t {}", r.temp_c);
        assert!((r.humidity_pct - 40.0).abs() < 0.01, "rh {}", r.humidity_pct);
    }

    #[test]
    fn rejects_a_bad_crc() {
        let mut bad = reply(0x6666, 0x6666);
        bad[2] ^= 0xFF;
        assert!(decode(&bad).is_none());
    }

    #[test]
    fn lpp_emits_temperature_then_humidity() {
        let mut out = Vec::new();
        push_lpp(&Reading { temp_c: 25.0, humidity_pct: 40.0 }, 2, &mut out);
        assert_eq!(out[0..2], [2, lpp::TEMPERATURE]);
        assert_eq!(out[4..6], [2, lpp::RELATIVE_HUMIDITY]);
    }
}
