//! Bosch BMP280 temperature / pressure sensor (upstream `ENV_INCLUDE_BMP280`).
//!
//! The pressure/humidity sibling of the BME280: the same chip family, the same temperature and
//! pressure compensation to the bit, but no humidity channel and a shorter (24-byte) calibration
//! block. These are the datasheet §3.11.3 integer formulas, the same ones Adafruit_BMP280 uses, so
//! the telemetry matches upstream byte for byte. All maths is pure and host-tested; the firmware
//! supplies the calibration block and each reading's data bytes.

use alloc::vec::Vec;

use crate::lpp;

/// `0xD0` chip-id register reads this on a production BMP280 (a BME280 reads `0x60`).
pub const CHIP_ID: u8 = 0x58;

/// Sea-level pressure upstream assumes for the altitude estimate (`TELEM_BME280_SEALEVELPRESSURE_HPA`).
const SEALEVEL_HPA: f32 = 1013.25;

/// Per-chip calibration, as laid out in the trimming registers `0x88..=0x9F` (24 bytes):
/// `dig_T1..dig_T3` then `dig_P1..dig_P9`. Field sizes and signedness follow Adafruit_BMP280.
#[derive(Clone, Copy, Debug)]
pub struct Calib {
    t1: u16,
    t2: i16,
    t3: i16,
    p1: u16,
    p2: i16,
    p3: i16,
    p4: i16,
    p5: i16,
    p6: i16,
    p7: i16,
    p8: i16,
    p9: i16,
}

/// A compensated reading in physical units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    pub temp_c: f32,
    pub pressure_hpa: f32,
    pub altitude_m: f32,
}

fn le_u16(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn le_i16(b: &[u8], i: usize) -> i16 {
    i16::from_le_bytes([b[i], b[i + 1]])
}

impl Calib {
    /// Parse the trimming-register run `0x88..=0x9F` (24 bytes).
    pub fn from_regs(tp: &[u8; 24]) -> Self {
        Self {
            t1: le_u16(tp, 0),
            t2: le_i16(tp, 2),
            t3: le_i16(tp, 4),
            p1: le_u16(tp, 6),
            p2: le_i16(tp, 8),
            p3: le_i16(tp, 10),
            p4: le_i16(tp, 12),
            p5: le_i16(tp, 14),
            p6: le_i16(tp, 16),
            p7: le_i16(tp, 18),
            p8: le_i16(tp, 20),
            p9: le_i16(tp, 22),
        }
    }

    /// Compensate one set of raw ADC words into physical units.
    pub fn compensate(&self, adc_t: i32, adc_p: i32) -> Reading {
        let t_fine = self.t_fine(adc_t);
        let temp_c = ((t_fine * 5 + 128) >> 8) as f32 / 100.0;
        let pressure_hpa = self.pressure_q24_8(adc_p, t_fine) as f32 / 256.0 / 100.0;
        let altitude_m = 44330.0 * (1.0 - libm::powf(pressure_hpa / SEALEVEL_HPA, 0.1903));
        Reading { temp_c, pressure_hpa, altitude_m }
    }

    /// Compensate the 6 data-register bytes (`0xF7..=0xFC`: press then temp, each MSB first).
    pub fn read_data(&self, data: &[u8; 6]) -> Reading {
        let adc_p = ((data[0] as i32) << 12) | ((data[1] as i32) << 4) | ((data[2] as i32) >> 4);
        let adc_t = ((data[3] as i32) << 12) | ((data[4] as i32) << 4) | ((data[5] as i32) >> 4);
        self.compensate(adc_t, adc_p)
    }

    /// `t_fine`, the shared fine-temperature term the pressure formula also reads.
    fn t_fine(&self, adc_t: i32) -> i32 {
        let var1 = (((adc_t >> 3) - ((self.t1 as i32) << 1)) * (self.t2 as i32)) >> 11;
        let d = (adc_t >> 4) - (self.t1 as i32);
        let var2 = (((d * d) >> 12) * (self.t3 as i32)) >> 14;
        var1 + var2
    }

    /// Pressure in Q24.8 pascals (divide by 256 for Pa), via the 64-bit reference formula.
    fn pressure_q24_8(&self, adc_p: i32, t_fine: i32) -> u32 {
        let mut var1 = (t_fine as i64) - 128000;
        let mut var2 = var1 * var1 * (self.p6 as i64);
        var2 += (var1 * (self.p5 as i64)) << 17;
        var2 += (self.p4 as i64) << 35;
        var1 = ((var1 * var1 * (self.p3 as i64)) >> 8) + ((var1 * (self.p2 as i64)) << 12);
        var1 = (((1i64 << 47) + var1) * (self.p1 as i64)) >> 33;
        if var1 == 0 {
            return 0; // avoid a divide-by-zero on an impossible calibration
        }
        let mut p = 1_048_576 - (adc_p as i64);
        p = (((p << 31) - var2) * 3125) / var1;
        var1 = ((self.p9 as i64) * (p >> 13) * (p >> 13)) >> 25;
        var2 = ((self.p8 as i64) * p) >> 19;
        p = ((p + var1 + var2) >> 8) + ((self.p7 as i64) << 4);
        p as u32
    }
}

/// Append the reading as upstream's `query_bmp280` does: temperature, pressure, altitude.
pub fn push_lpp(reading: &Reading, channel: u8, out: &mut Vec<u8>) {
    lpp::push(out, channel, lpp::TEMPERATURE, reading.temp_c);
    lpp::push(out, channel, lpp::BAROMETRIC_PRESSURE, reading.pressure_hpa);
    lpp::push(out, channel, lpp::ALTITUDE, reading.altitude_m);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Datasheet temperature worked example (shared with the BME280), with plausible pressure
    // trimming so the pressure formula runs on real-shaped coefficients.
    fn sample() -> Calib {
        Calib {
            t1: 27504,
            t2: 26435,
            t3: -1000,
            p1: 36477,
            p2: -10685,
            p3: 3024,
            p4: 2855,
            p5: 140,
            p6: -7,
            p7: 15500,
            p8: -14600,
            p9: 6000,
        }
    }

    // Reference `double` pressure formula from the datasheet, to cross-check the integer one.
    fn pressure_double(c: &Calib, adc_p: i32, t_fine: i32) -> f64 {
        let mut var1 = (t_fine as f64) / 2.0 - 64000.0;
        let mut var2 = var1 * var1 * (c.p6 as f64) / 32768.0;
        var2 += var1 * (c.p5 as f64) * 2.0;
        var2 = var2 / 4.0 + (c.p4 as f64) * 65536.0;
        var1 = ((c.p3 as f64) * var1 * var1 / 524288.0 + (c.p2 as f64) * var1) / 524288.0;
        var1 = (1.0 + var1 / 32768.0) * (c.p1 as f64);
        if var1 == 0.0 {
            return 0.0;
        }
        let mut p = 1_048_576.0 - (adc_p as f64);
        p = (p - var2 / 4096.0) * 6250.0 / var1;
        var1 = (c.p9 as f64) * p * p / 2_147_483_648.0;
        var2 = p * (c.p8 as f64) / 32768.0;
        p + (var1 + var2 + (c.p7 as f64)) / 16.0 // Pa
    }

    #[test]
    fn temperature_matches_datasheet_example() {
        let c = sample();
        assert_eq!(c.t_fine(519888), 128422);
        let r = c.compensate(519888, 415148);
        assert!((r.temp_c - 25.08).abs() < 0.001, "temp {}", r.temp_c);
    }

    #[test]
    fn pressure_tracks_the_double_formula() {
        let c = sample();
        let t_fine = c.t_fine(519888);
        for adc_p in [300_000, 415148, 520_000] {
            let int_pa = c.pressure_q24_8(adc_p, t_fine) as f64 / 256.0;
            let dbl_pa = pressure_double(&c, adc_p, t_fine);
            assert!((int_pa - dbl_pa).abs() < 5.0, "adc_p {adc_p}: int {int_pa} dbl {dbl_pa}");
        }
    }

    #[test]
    fn data_bytes_split_into_adc_words() {
        let c = sample();
        // press 0x6D3A0, temp 0x7EED0 (=519888): packed MSB-first across 6 bytes.
        let data = [0x6D, 0x3A, 0x00, 0x7E, 0xED, 0x00];
        let from_bytes = c.read_data(&data);
        let direct = c.compensate(519888, 0x6D3A0);
        assert_eq!(from_bytes, direct);
        assert!((from_bytes.temp_c - 25.08).abs() < 0.001);
    }

    #[test]
    fn lpp_emits_three_environment_records() {
        let r = Reading { temp_c: 25.09, pressure_hpa: 1013.2, altitude_m: 1.0 };
        let mut out = Vec::new();
        push_lpp(&r, 2, &mut out);
        // channel+type pairs: 103 temp, 115 pressure, 121 altitude
        assert_eq!(out[0..2], [2, lpp::TEMPERATURE]);
        assert_eq!(out[4..6], [2, lpp::BAROMETRIC_PRESSURE]);
        assert_eq!(out[8..10], [2, lpp::ALTITUDE]);
    }
}
