//! Bosch BME280 temperature / humidity / pressure sensor (upstream `ENV_INCLUDE_BME280`).
//!
//! The chip ships raw 20-bit (T, P) and 16-bit (H) ADC words that only become physical values
//! through Bosch's fixed-point compensation formulas, keyed by per-chip calibration read once at
//! start-up. These are the integer reference formulas from the datasheet (§8.2), the same ones
//! Adafruit_BME280 uses, so our telemetry matches upstream byte for byte. All maths is pure and
//! host-tested; the firmware supplies the calibration block and each reading's data bytes.

use alloc::vec::Vec;

use crate::lpp;

/// `0xD0` chip-id register reads this on a genuine BME280 (a BMP280 reads `0x58`).
pub const CHIP_ID: u8 = 0x60;

/// Sea-level pressure upstream assumes for the altitude estimate (`TELEM_BME280_SEALEVELPRESSURE_HPA`).
const SEALEVEL_HPA: f32 = 1013.25;

/// Per-chip calibration, as laid out in the trimming registers.
///
/// Built from the two register runs the firmware reads once after reset: `0x88..=0xA1` (26 bytes)
/// and `0xE1..=0xE7` (7 bytes). Field sizes and signedness, and the split 12-bit `h4`/`h5`, follow
/// Adafruit_BME280's `readCoefficients()`.
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
    h1: u8,
    h2: i16,
    h3: u8,
    h4: i16,
    h5: i16,
    h6: i8,
}

/// A compensated reading in physical units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reading {
    pub temp_c: f32,
    pub humidity_pct: f32,
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
    /// Parse the two trimming-register runs: `tp` is `0x88..=0xA1`, `hum` is `0xE1..=0xE7`.
    pub fn from_regs(tp: &[u8; 26], hum: &[u8; 7]) -> Self {
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
            h1: tp[25],
            h2: le_i16(hum, 0),
            h3: hum[2],
            // 12-bit signed, sign carried by the high byte's bit 7 ((int8_t)E4 << 4).
            h4: (((hum[3] as i8 as i32) << 4) | (hum[4] as i32 & 0x0F)) as i16,
            h5: (((hum[5] as i8 as i32) << 4) | ((hum[4] as i32) >> 4)) as i16,
            h6: hum[6] as i8,
        }
    }

    /// Compensate one set of raw ADC words into physical units.
    pub fn compensate(&self, adc_t: i32, adc_p: i32, adc_h: i32) -> Reading {
        let t_fine = self.t_fine(adc_t);
        let temp_c = ((t_fine * 5 + 128) >> 8) as f32 / 100.0;
        let pressure_hpa = self.pressure_q24_8(adc_p, t_fine) as f32 / 256.0 / 100.0;
        let humidity_pct = self.humidity_q22_10(adc_h, t_fine) as f32 / 1024.0;
        let altitude_m = 44330.0 * (1.0 - libm::powf(pressure_hpa / SEALEVEL_HPA, 0.1903));
        Reading { temp_c, humidity_pct, pressure_hpa, altitude_m }
    }

    /// Compensate the 8 data-register bytes (`0xF7..=0xFE`: press, temp, hum, each MSB first).
    pub fn read_data(&self, data: &[u8; 8]) -> Reading {
        let adc_p = ((data[0] as i32) << 12) | ((data[1] as i32) << 4) | ((data[2] as i32) >> 4);
        let adc_t = ((data[3] as i32) << 12) | ((data[4] as i32) << 4) | ((data[5] as i32) >> 4);
        let adc_h = ((data[6] as i32) << 8) | (data[7] as i32);
        self.compensate(adc_t, adc_p, adc_h)
    }

    /// `t_fine`, the shared fine-temperature term the pressure and humidity formulas also read.
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

    /// Relative humidity in Q22.10 percent (divide by 1024 for %RH), via the 32-bit reference formula.
    fn humidity_q22_10(&self, adc_h: i32, t_fine: i32) -> u32 {
        let mut v = t_fine - 76800;
        let term_a = (((adc_h << 14) - ((self.h4 as i32) << 20) - ((self.h5 as i32) * v)) + 16384) >> 15;
        let inner = ((((v * (self.h6 as i32)) >> 10) * (((v * (self.h3 as i32)) >> 11) + 32768)) >> 10) + 2097152;
        let term_b = (inner * (self.h2 as i32) + 8192) >> 14;
        v = term_a * term_b;
        v -= ((((v >> 15) * (v >> 15)) >> 7) * (self.h1 as i32)) >> 4;
        v = v.clamp(0, 419430400);
        (v >> 12) as u32
    }
}

/// Append the reading as upstream's `query_bme280` does: temperature, humidity, pressure, altitude.
pub fn push_lpp(reading: &Reading, channel: u8, out: &mut Vec<u8>) {
    lpp::push(out, channel, lpp::TEMPERATURE, reading.temp_c);
    lpp::push(out, channel, lpp::RELATIVE_HUMIDITY, reading.humidity_pct);
    lpp::push(out, channel, lpp::BAROMETRIC_PRESSURE, reading.pressure_hpa);
    lpp::push(out, channel, lpp::ALTITUDE, reading.altitude_m);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Datasheet §8.2 worked example for temperature, with plausible pressure and humidity
    // trimming added so the other two formulas run on real-shaped coefficients.
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
            h1: 75,
            h2: 362,
            h3: 0,
            h4: 317,
            h5: 0,
            h6: 30,
        }
    }

    // Reference `double` formulas from the datasheet, used only to cross-check the integer ones:
    // a porting slip in the fixed-point maths would diverge by far more than the tolerances here.
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

    fn humidity_double(c: &Calib, adc_h: i32, t_fine: i32) -> f64 {
        let mut h = (t_fine as f64) - 76800.0;
        h = ((adc_h as f64) - ((c.h4 as f64) * 64.0 + (c.h5 as f64) / 16384.0 * h))
            * ((c.h2 as f64) / 65536.0
                * (1.0 + (c.h6 as f64) / 67_108_864.0 * h * (1.0 + (c.h3 as f64) / 67_108_864.0 * h)));
        h *= 1.0 - (c.h1 as f64) * h / 524288.0;
        h.clamp(0.0, 100.0)
    }

    #[test]
    fn temperature_matches_datasheet_example() {
        let c = sample();
        // adc_T = 519888 is the datasheet's worked value; t_fine and the 0.01 °C output are exact.
        assert_eq!(c.t_fine(519888), 128422);
        let r = c.compensate(519888, 415148, 32768);
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
    fn humidity_tracks_the_double_formula() {
        let c = sample();
        let t_fine = c.t_fine(519888);
        for adc_h in [16384, 32768, 50000] {
            let int_pct = c.humidity_q22_10(adc_h, t_fine) as f64 / 1024.0;
            let dbl_pct = humidity_double(&c, adc_h, t_fine);
            assert!((int_pct - dbl_pct).abs() < 1.0, "adc_h {adc_h}: int {int_pct} dbl {dbl_pct}");
        }
    }

    #[test]
    fn data_bytes_split_into_adc_words() {
        let c = sample();
        // press 0x6D3A0, temp 0x7EED0 (=519888), hum 0x8000: packed MSB-first across 8 bytes.
        let data = [0x6D, 0x3A, 0x00, 0x7E, 0xED, 0x00, 0x80, 0x00];
        let from_bytes = c.read_data(&data);
        let direct = c.compensate(519888, 0x6D3A0, 0x8000);
        assert_eq!(from_bytes, direct);
        assert!((from_bytes.temp_c - 25.08).abs() < 0.001);
    }

    #[test]
    fn lpp_emits_four_environment_records() {
        let r = Reading { temp_c: 25.09, humidity_pct: 46.3, pressure_hpa: 1013.2, altitude_m: 1.0 };
        let mut out = Vec::new();
        push_lpp(&r, 2, &mut out);
        // channel+type pairs: 103 temp, 104 humidity, 115 pressure, 121 altitude
        assert_eq!(out[0..2], [2, lpp::TEMPERATURE]);
        assert_eq!(out[4..6], [2, lpp::RELATIVE_HUMIDITY]);
        assert_eq!(out[7..9], [2, lpp::BAROMETRIC_PRESSURE]);
        assert_eq!(out[11..13], [2, lpp::ALTITUDE]);
    }
}
