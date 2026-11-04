//! Aosong AHT10 / AHT20 temperature + humidity sensor (upstream `ENV_INCLUDE_AHTX0`).
//!
//! No chip-id register and no compensation: one trigger command yields a status byte and two
//! 20-bit ADC words, packed across six bytes, that scale linearly (datasheet §6, the same maths
//! Adafruit_AHTX0 uses). The sans-IO part is just that unpacking; the firmware drives the bus.

use alloc::vec::Vec;

use crate::lpp;

/// Fixed I2C address (there is no alternate).
pub const ADDR: u8 = 0x38;

/// A reading in physical units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    pub temp_c: f32,
    pub humidity_pct: f32,
}

/// Unpack the six status+data bytes (`[status, h, h, h|t, t, t]`) into physical units.
///
/// Humidity is the top 20 bits, temperature the bottom 20, sharing the middle byte's nibbles.
/// Each is scaled by its full-scale 2²⁰: `RH = raw·100/2²⁰`, `T = raw·200/2²⁰ − 50`.
pub fn decode(data: &[u8; 6]) -> Reading {
    let h = ((data[1] as u32) << 12) | ((data[2] as u32) << 4) | ((data[3] as u32) >> 4);
    let t = (((data[3] as u32) & 0x0F) << 16) | ((data[4] as u32) << 8) | (data[5] as u32);
    Reading {
        humidity_pct: h as f32 * 100.0 / 1_048_576.0,
        temp_c: t as f32 * 200.0 / 1_048_576.0 - 50.0,
    }
}

/// Append the reading as upstream does: temperature then humidity.
pub fn push_lpp(reading: &Reading, channel: u8, out: &mut Vec<u8>) {
    lpp::push(out, channel, lpp::TEMPERATURE, reading.temp_c);
    lpp::push(out, channel, lpp::RELATIVE_HUMIDITY, reading.humidity_pct);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midscale_words_decode_to_midscale_units() {
        // h20 = 0x80000 (half of 2^20) -> 50 %RH; t20 = 0x80000 -> 200·0.5 − 50 = 50 °C.
        // The two words share data[3]: high nibble is humidity's low bits, low nibble temp's high.
        let r = decode(&[0x00, 0x80, 0x00, 0x08, 0x00, 0x00]);
        assert!((r.humidity_pct - 50.0).abs() < 0.01, "rh {}", r.humidity_pct);
        assert!((r.temp_c - 50.0).abs() < 0.01, "t {}", r.temp_c);
    }

    #[test]
    fn zero_words_are_the_low_ends() {
        let r = decode(&[0x1C, 0, 0, 0, 0, 0]);
        assert_eq!(r.humidity_pct, 0.0);
        assert_eq!(r.temp_c, -50.0);
    }

    #[test]
    fn lpp_emits_temperature_then_humidity() {
        let mut out = Vec::new();
        push_lpp(&Reading { temp_c: 21.5, humidity_pct: 48.0 }, 2, &mut out);
        assert_eq!(out[0..2], [2, lpp::TEMPERATURE]);
        assert_eq!(out[4..6], [2, lpp::RELATIVE_HUMIDITY]);
    }
}
