//! LoRa modem parameters and the air-time / reception-score math the router relies on.

use crate::MAX_TRANS_UNIT;

/// Link margins (dB above the demodulation floor) for dynamic coding rate.
pub const DYN_CR_STRONG_MARGIN_DB: f32 = 15.0;
pub const DYN_CR_WEAK_MARGIN_DB: f32 = 7.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadioParams {
    pub freq_mhz: f32,
    pub bw_khz: f32,
    pub sf: u8,
    /// Coding rate denominator, 5..=8 (4/5 .. 4/8).
    pub cr: u8,
}

impl Default for RadioParams {
    /// Upstream build defaults (`LORA_FREQ=869.618 LORA_BW=62.5 LORA_SF=8`, CR 4/5).
    fn default() -> Self {
        Self { freq_mhz: 869.618, bw_khz: 62.5, sf: 8, cr: 5 }
    }
}

impl RadioParams {
    pub fn is_valid(&self) -> bool {
        (150.0..=2500.0).contains(&self.freq_mhz)
            && (7.0..=500.0).contains(&self.bw_khz)
            && (5..=12).contains(&self.sf)
            && (5..=8).contains(&self.cr)
    }

    /// Longer preamble at low SF improves reliability (`preambleLengthForSF`).
    pub fn preamble_len(&self) -> u16 {
        if self.sf <= 8 { 32 } else { 16 }
    }

    /// Symbol time in milliseconds.
    pub fn symbol_ms(&self) -> f32 {
        (1u32 << self.sf) as f32 / self.bw_khz
    }

    /// Low data-rate optimisation, enabled automatically for symbols >= 16 ms (RadioLib SX126x).
    pub fn ldro(&self) -> bool {
        self.symbol_ms() >= 16.0
    }

    /// Time on air in microseconds (RadioLib `calculateTimeOnAir`, explicit header, CRC on).
    pub fn time_on_air_us(&self, len: usize) -> u32 {
        let sf = self.sf as i32;
        let (coeff1, coeff2) = if sf == 5 || sf == 6 { (6.25f32, 0f32) } else { (4.25, 8.0) };
        let sf_divisor = if self.ldro() { 4 * (sf - 2) } else { 4 * sf };
        let bits_per_crc = 16;
        let header_syms = 20;
        let bit_count = (8 * len as i32 + bits_per_crc - 4 * sf + header_syms).max(0);
        let pre_coded = (bit_count + sf_divisor - 1) / sf_divisor;
        let n_symbols = (self.preamble_len() as f32 + coeff1) + (pre_coded * self.cr as i32) as f32 + coeff2;
        ((1u32 << self.sf) as f32 / self.bw_khz * n_symbols * 1000.0) as u32
    }

    /// Estimated transmit air-time in milliseconds (`getEstAirtimeFor`).
    pub fn est_airtime_ms(&self, len: usize) -> u32 {
        self.time_on_air_us(len) / 1000
    }

    /// Deadlines used to clear a stuck "receiving" state: (preamble->header ms, header->done ms)
    /// (`calcMaxPacketMillis`).
    pub fn max_packet_millis(&self) -> (u32, u32) {
        let tsym_us = (10_000u32 << self.sf) / (self.bw_khz * 10.0) as u32;
        let coeff1_x4 = if self.sf == 5 || self.sf == 6 { 25 } else { 17 };
        let preamble_us = (((self.preamble_len() as u32 + 8) * 4 + coeff1_x4) * tsym_us) / 4;
        let total_us = self.time_on_air_us(MAX_TRANS_UNIT);
        let mut payload_us = if total_us > preamble_us { total_us - preamble_us } else { 4000u32.saturating_sub(preamble_us) };
        if (5..8).contains(&self.cr) {
            payload_us = payload_us * 8 / self.cr as u32;
        }
        (preamble_us.div_ceil(1000), payload_us.div_ceil(1000))
    }

    /// Demodulation SNR floor for the spreading factor (Semtech datasheet figures).
    pub fn snr_threshold(&self) -> f32 {
        const SNR_THRESHOLD: [f32; 8] = [-2.5, -5.0, -7.5, -10.0, -12.5, -15.0, -17.5, -20.0];
        SNR_THRESHOLD[(self.sf.clamp(5, 12) - 5) as usize]
    }

    /// Dynamic coding rate for one hop with the given link SNR (roadmap "dynamic CR"):
    /// a strong link gets 4/5 (shorter airtime), a weak one 4/8 (more redundancy).
    /// Receivers need no configuration: the CR travels in the explicit LoRa header.
    pub fn dynamic_cr(&self, link_snr: f32) -> u8 {
        let margin = link_snr - self.snr_threshold();
        if margin >= DYN_CR_STRONG_MARGIN_DB {
            5
        } else if margin < DYN_CR_WEAK_MARGIN_DB {
            8
        } else {
            self.cr
        }
    }

    pub fn with_cr(&self, cr: u8) -> Self {
        Self { cr, ..*self }
    }

    /// Probability-like score of successful reception (`packetScoreInt`).
    pub fn packet_score(&self, snr: f32, packet_len: usize) -> f32 {
        const SNR_THRESHOLD: [f32; 6] = [-7.5, -10.0, -12.5, -15.0, -17.5, -20.0];
        if self.sf < 7 {
            return 0.0;
        }
        let thr = SNR_THRESHOLD[(self.sf.min(12) - 7) as usize];
        if snr < thr {
            return 0.0;
        }
        let success = (snr - thr) / 10.0;
        let collision_penalty = 1.0 - (packet_len as f32 / 256.0);
        (success * collision_penalty).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn airtime_reference_values() {
        // SF8/BW62.5/CR5, preamble 32: symbol = 4.096 ms
        let p = RadioParams::default();
        assert!(!p.ldro());
        // 10-byte frame: bits = 80+16-32+20 = 84 -> ceil(84/32)=3 -> 3*5=15 syms
        // total = 32+4.25+15+8 = 59.25 syms -> 242.688 ms
        assert_eq!(p.est_airtime_ms(10), 242);
        // SF12/BW125 enables LDRO
        let q = RadioParams { freq_mhz: 868.0, bw_khz: 125.0, sf: 12, cr: 5 };
        assert!(q.ldro());
        assert!(q.est_airtime_ms(255) > 8000);
    }

    #[test]
    fn score_bounds() {
        let p = RadioParams::default();
        assert_eq!(p.packet_score(-20.0, 50), 0.0);
        assert!(p.packet_score(10.0, 10) > 0.9);
    }
}
