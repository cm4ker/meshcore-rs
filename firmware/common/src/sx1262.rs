//! Minimal SX1262 LoRa driver: exactly what MeshCore needs, configured like upstream's
//! `CustomSX1262::std_init()` (RadioLib) for the Heltec V4.
//!
//! SPI transfers are blocking (a full 255-byte frame takes ~0.3 ms at 8 MHz); only waiting for
//! DIO1 is async.

use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::Blocking;
use esp_hal::gpio::{Event, Input, Output};
use esp_hal::spi::master::Spi;
use embedded_hal::spi::SpiBus;
use meshcore::radio::RadioParams;

use crate::radio::{Error, LoraRadio, RxEvent, RxFrame};

// commands
const CMD_SET_SLEEP: u8 = 0x84;
const CMD_SET_STANDBY: u8 = 0x80;
const CMD_SET_TX: u8 = 0x83;
const CMD_SET_RX: u8 = 0x82;
const CMD_SET_REGULATOR_MODE: u8 = 0x96;
const CMD_CALIBRATE: u8 = 0x89;
const CMD_CALIBRATE_IMAGE: u8 = 0x98;
const CMD_SET_PA_CONFIG: u8 = 0x95;
const CMD_WRITE_REGISTER: u8 = 0x0D;
const CMD_READ_REGISTER: u8 = 0x1D;
const CMD_WRITE_BUFFER: u8 = 0x0E;
const CMD_READ_BUFFER: u8 = 0x1E;
const CMD_SET_DIO_IRQ_PARAMS: u8 = 0x08;
const CMD_GET_IRQ_STATUS: u8 = 0x12;
const CMD_CLEAR_IRQ_STATUS: u8 = 0x02;
const CMD_SET_DIO2_AS_RF_SWITCH: u8 = 0x9D;
const CMD_SET_DIO3_AS_TCXO: u8 = 0x97;
const CMD_SET_RF_FREQUENCY: u8 = 0x86;
const CMD_SET_PACKET_TYPE: u8 = 0x8A;
const CMD_SET_TX_PARAMS: u8 = 0x8E;
const CMD_SET_MODULATION_PARAMS: u8 = 0x8B;
const CMD_SET_PACKET_PARAMS: u8 = 0x8C;
const CMD_SET_BUFFER_BASE: u8 = 0x8F;
const CMD_GET_RX_BUFFER_STATUS: u8 = 0x13;
const CMD_GET_PACKET_STATUS: u8 = 0x14;
const CMD_GET_RSSI_INST: u8 = 0x15;
const CMD_GET_DEVICE_ERRORS: u8 = 0x17;
const CMD_CLEAR_DEVICE_ERRORS: u8 = 0x07;
const CMD_SET_CAD_PARAMS: u8 = 0x88;
const CMD_SET_CAD: u8 = 0xC5;

// registers
const REG_SYNC_WORD: u16 = 0x0740;
const REG_IQ_POLARITY: u16 = 0x0736;
const REG_RANDOM: u16 = 0x0819;
const REG_RX_GAIN: u16 = 0x08AC;
const REG_OCP: u16 = 0x08E7;
const REG_TX_CLAMP: u16 = 0x08D8;
const REG_HELTEC_V4_PATCH: u16 = 0x08B5;

// IRQ bits
pub const IRQ_TX_DONE: u16 = 1 << 0;
pub const IRQ_RX_DONE: u16 = 1 << 1;
pub const IRQ_PREAMBLE_DETECTED: u16 = 1 << 2;
pub const IRQ_SYNC_WORD_VALID: u16 = 1 << 3;
pub const IRQ_HEADER_VALID: u16 = 1 << 4;
pub const IRQ_HEADER_ERR: u16 = 1 << 5;
pub const IRQ_CRC_ERR: u16 = 1 << 6;
pub const IRQ_CAD_DONE: u16 = 1 << 7;
pub const IRQ_CAD_DETECTED: u16 = 1 << 8;
pub const IRQ_TIMEOUT: u16 = 1 << 9;
const IRQ_ALL: u16 = 0x43FF;

const STDBY_RC: u8 = 0x00;
const PACKET_TYPE_LORA: u8 = 0x01;
const REGULATOR_DCDC: u8 = 0x01;
const TCXO_1V8: u8 = 0x02;
const PA_RAMP_200U: u8 = 0x04;
/// "Private" LoRa sync word 0x12 in SX126x register form.
const SYNC_WORD_PRIVATE: [u8; 2] = [0x14, 0x24];
const RX_GAIN_BOOSTED: u8 = 0x96;
const RX_GAIN_POWER_SAVING: u8 = 0x94;
/// 140 mA over-current limit (`SX126X_CURRENT_LIMIT`), 2.5 mA steps.
const OCP_140MA: u8 = 56;

pub struct Sx1262<'d> {
    spi: Spi<'d, Blocking>,
    nss: Output<'d>,
    reset: Output<'d>,
    busy: Input<'d>,
    dio1: Input<'d>,
    params: RadioParams,
    tx_power: i8,
    rx_boost: bool,
    use_tcxo: bool,
    // "is a packet coming in?" state (upstream `CustomSX1262::isReceiving`)
    activity_at: Option<Instant>,
    header_seen: bool,
    preamble_ms: u64,
    max_payload_ms: u64,
}

fn bw_code(bw_khz: f32) -> u8 {
    // (bandwidth, register value) - nearest match
    const TABLE: [(f32, u8); 10] = [
        (7.8, 0x00), (10.4, 0x08), (15.6, 0x01), (20.8, 0x09), (31.25, 0x02),
        (41.7, 0x0A), (62.5, 0x03), (125.0, 0x04), (250.0, 0x05), (500.0, 0x06),
    ];
    let mut best = TABLE[0];
    for e in TABLE {
        if libm::fabsf(e.0 - bw_khz) < libm::fabsf(best.0 - bw_khz) {
            best = e;
        }
    }
    best.1
}

/// Image calibration band for a frequency (RadioLib `calibrateImage`).
fn image_cal(freq_mhz: f32) -> [u8; 2] {
    if freq_mhz > 900.0 {
        [0xE1, 0xE9]
    } else if freq_mhz > 850.0 {
        [0xD7, 0xDB]
    } else if freq_mhz > 770.0 {
        [0xC1, 0xC5]
    } else if freq_mhz > 460.0 {
        [0x75, 0x81]
    } else {
        [0x6B, 0x6F]
    }
}

impl<'d> Sx1262<'d> {
    pub fn new(spi: Spi<'d, Blocking>, nss: Output<'d>, reset: Output<'d>, busy: Input<'d>, dio1: Input<'d>) -> Self {
        Self {
            spi,
            nss,
            reset,
            busy,
            dio1,
            params: RadioParams::default(),
            tx_power: 10,
            rx_boost: true,
            use_tcxo: true,
            activity_at: None,
            header_seen: false,
            preamble_ms: 66,
            max_payload_ms: 3934,
        }
    }

    // ---------------------------------------------------------------- SPI

    fn wait_busy(&mut self, timeout_ms: u64) -> Result<(), Error> {
        let start = Instant::now();
        while self.busy.is_high() {
            if start.elapsed() > Duration::from_millis(timeout_ms) {
                return Err(Error::Busy);
            }
        }
        Ok(())
    }

    fn command(&mut self, op: u8, args: &[u8]) -> Result<(), Error> {
        self.wait_busy(20)?;
        self.nss.set_low();
        let _ = self.spi.write(&[op]);
        let _ = self.spi.write(args);
        let _ = self.spi.flush();
        self.nss.set_high();
        Ok(())
    }

    /// Command with response: `op, args..., NOP(status), data...`.
    fn command_read(&mut self, op: u8, args: &[u8], out: &mut [u8]) -> Result<(), Error> {
        self.wait_busy(20)?;
        self.nss.set_low();
        let _ = self.spi.write(&[op]);
        let _ = self.spi.write(args);
        let mut status = [0u8];
        let _ = self.spi.transfer_in_place(&mut status);
        out.fill(0);
        let _ = self.spi.transfer_in_place(out);
        let _ = self.spi.flush();
        self.nss.set_high();
        Ok(())
    }

    fn write_register(&mut self, addr: u16, data: &[u8]) -> Result<(), Error> {
        self.wait_busy(20)?;
        self.nss.set_low();
        let _ = self.spi.write(&[CMD_WRITE_REGISTER, (addr >> 8) as u8, addr as u8]);
        let _ = self.spi.write(data);
        let _ = self.spi.flush();
        self.nss.set_high();
        Ok(())
    }

    fn read_register(&mut self, addr: u16, out: &mut [u8]) -> Result<(), Error> {
        self.command_read(CMD_READ_REGISTER, &[(addr >> 8) as u8, addr as u8], out)
    }

    fn modify_register(&mut self, addr: u16, set_bits: u8, clear_bits: u8) -> Result<(), Error> {
        let mut v = [0u8];
        self.read_register(addr, &mut v)?;
        v[0] = (v[0] & !clear_bits) | set_bits;
        self.write_register(addr, &v)
    }

    pub fn device_errors(&mut self) -> Result<u16, Error> {
        let mut e = [0u8; 2];
        self.command_read(CMD_GET_DEVICE_ERRORS, &[], &mut e)?;
        Ok(u16::from_be_bytes(e))
    }

    fn set_packet_params(&mut self, payload_len: u8) -> Result<(), Error> {
        let pre = self.params.preamble_len().to_be_bytes();
        // explicit header, CRC on, standard IQ
        self.command(CMD_SET_PACKET_PARAMS, &[pre[0], pre[1], 0x00, payload_len, 0x01, 0x00])
    }

    fn set_irq(&mut self, irq_mask: u16, dio1_mask: u16) -> Result<(), Error> {
        let i = irq_mask.to_be_bytes();
        let d = dio1_mask.to_be_bytes();
        self.command(CMD_SET_DIO_IRQ_PARAMS, &[i[0], i[1], d[0], d[1], 0, 0, 0, 0])
    }

    fn clear_irq(&mut self, mask: u16) -> Result<(), Error> {
        self.command(CMD_CLEAR_IRQ_STATUS, &mask.to_be_bytes())
    }

    pub fn irq_status(&mut self) -> Result<u16, Error> {
        let mut s = [0u8; 2];
        self.command_read(CMD_GET_IRQ_STATUS, &[], &mut s)?;
        Ok(u16::from_be_bytes(s))
    }
}

impl LoraRadio for Sx1262<'_> {
    /// Reset and configure the modem. Falls back to no-TCXO like upstream if the TCXO
    /// configuration is rejected.
    async fn init(&mut self, params: RadioParams, tx_power: i8, rx_boost: bool) -> Result<(), Error> {
        self.params = params;
        self.tx_power = tx_power;
        self.rx_boost = rx_boost;
        loop {
            self.reset.set_low();
            Timer::after_millis(2).await;
            self.reset.set_high();
            Timer::after_millis(10).await;
            self.wait_busy(100)?;
            self.command(CMD_SET_STANDBY, &[STDBY_RC])?;

            // the sync word register resets to 0x1424: proves SPI works and the chip is there
            let mut sw = [0u8; 2];
            self.read_register(REG_SYNC_WORD, &mut sw)?;
            if sw != SYNC_WORD_PRIVATE && sw != [0x34, 0x44] {
                return Err(Error::NotFound);
            }

            self.command(CMD_CLEAR_DEVICE_ERRORS, &[0, 0])?;
            if self.use_tcxo {
                // 1.8 V, 5 ms startup (320 * 15.625 us)
                self.command(CMD_SET_DIO3_AS_TCXO, &[TCXO_1V8, 0x00, 0x01, 0x40])?;
            }
            self.command(CMD_CALIBRATE, &[0x7F])?;
            Timer::after_millis(5).await;
            self.wait_busy(50)?;
            let errs = self.device_errors()?;
            if errs & (1 << 5) != 0 && self.use_tcxo {
                // XOSC start error: board without TCXO (upstream retries with tcxo = 0.0)
                log::warn!("SX1262: XOSC start error 0x{errs:04X}, retrying without TCXO");
                self.use_tcxo = false;
                continue;
            }
            break;
        }

        self.command(CMD_SET_REGULATOR_MODE, &[REGULATOR_DCDC])?;
        self.command(CMD_SET_DIO2_AS_RF_SWITCH, &[0x01])?;
        self.command(CMD_SET_PACKET_TYPE, &[PACKET_TYPE_LORA])?;
        self.command(CMD_SET_BUFFER_BASE, &[0x00, 0x00])?;
        self.apply_params(params)?;
        self.set_tx_power(tx_power)?;
        self.set_rx_boosted_gain(rx_boost)?;
        // Heltec V4: improved RX (`SX126X_REGISTER_PATCH`)
        self.modify_register(REG_HELTEC_V4_PATCH, 0x01, 0)?;
        self.start_rx()
    }

    /// Frequency / bandwidth / SF / CR, plus the derived preamble and "stuck receive" timeouts.
    fn apply_params(&mut self, p: RadioParams) -> Result<(), Error> {
        self.params = p;
        self.command(CMD_SET_STANDBY, &[STDBY_RC])?;
        self.command(CMD_CALIBRATE_IMAGE, &image_cal(p.freq_mhz))?;
        self.wait_busy(50)?;
        let frf = ((p.freq_mhz as f64 * 1_000_000.0) * (1u64 << 25) as f64 / 32_000_000.0) as u32;
        self.command(CMD_SET_RF_FREQUENCY, &frf.to_be_bytes())?;
        let ldro = if p.ldro() { 1 } else { 0 };
        self.command(CMD_SET_MODULATION_PARAMS, &[p.sf, bw_code(p.bw_khz), p.cr - 4, ldro])?;
        self.set_packet_params(255)?;
        // standard IQ: errata 15.4 says set bit 2 of 0x0736
        self.modify_register(REG_IQ_POLARITY, 0x04, 0)?;
        self.write_register(REG_SYNC_WORD, &SYNC_WORD_PRIVATE)?;
        let (pre, payload) = p.max_packet_millis();
        self.preamble_ms = pre as u64;
        self.max_payload_ms = payload as u64;
        Ok(())
    }

    /// Change only the coding rate (dynamic CR for one frame); `None` restores the configured one.
    fn set_frame_cr(&mut self, cr: Option<u8>) -> Result<(), Error> {
        let p = self.params;
        let cr = cr.unwrap_or(p.cr).clamp(5, 8);
        let ldro = if p.ldro() { 1 } else { 0 };
        self.command(CMD_SET_STANDBY, &[STDBY_RC])?;
        self.command(CMD_SET_MODULATION_PARAMS, &[p.sf, bw_code(p.bw_khz), cr - 4, ldro])
    }

    fn set_tx_power(&mut self, dbm: i8) -> Result<(), Error> {
        let dbm = dbm.clamp(-9, 22);
        self.tx_power = dbm;
        self.command(CMD_SET_PA_CONFIG, &[0x04, 0x07, 0x00, 0x01])?;
        self.command(CMD_SET_TX_PARAMS, &[dbm as u8, PA_RAMP_200U])?;
        self.write_register(REG_OCP, &[OCP_140MA])?;
        // errata 15.2: better PA clamping
        self.modify_register(REG_TX_CLAMP, 0x1E, 0)
    }

    fn set_rx_boosted_gain(&mut self, on: bool) -> Result<(), Error> {
        self.rx_boost = on;
        self.write_register(REG_RX_GAIN, &[if on { RX_GAIN_BOOSTED } else { RX_GAIN_POWER_SAVING }])
    }

    // ---------------------------------------------------------------- RX

    async fn wait_irq(&mut self) {
        self.dio1.wait_for_high().await
    }

    fn arm_wake(&mut self) -> bool {
        if self.dio1.is_high() {
            return false;
        }
        // a listening pin wakes the chip; the next wait_irq takes the interrupt over
        self.dio1.listen(Event::HighLevel);
        true
    }

    /// Continuous receive; DIO1 rises on RX-done.
    fn start_rx(&mut self) -> Result<(), Error> {
        self.command(CMD_SET_STANDBY, &[STDBY_RC])?;
        self.set_irq(
            IRQ_RX_DONE | IRQ_TIMEOUT | IRQ_CRC_ERR | IRQ_HEADER_ERR | IRQ_HEADER_VALID | IRQ_PREAMBLE_DETECTED
                | IRQ_SYNC_WORD_VALID,
            IRQ_RX_DONE | IRQ_TIMEOUT,
        )?;
        self.clear_irq(IRQ_ALL)?;
        self.set_packet_params(255)?;
        self.command(CMD_SET_RX, &[0xFF, 0xFF, 0xFF])?;
        self.activity_at = None;
        self.header_seen = false;
        Ok(())
    }

    /// Read whatever completed (call after DIO1 went high). The radio stays in RX.
    fn read_rx(&mut self) -> Result<RxEvent, Error> {
        let irq = self.irq_status()?;
        self.clear_irq(IRQ_ALL)?;
        self.activity_at = None;
        self.header_seen = false;
        if irq & IRQ_RX_DONE == 0 {
            if irq & IRQ_TIMEOUT != 0 {
                self.start_rx()?;
            }
            return Ok(RxEvent::Nothing);
        }
        if irq & (IRQ_CRC_ERR | IRQ_HEADER_ERR) != 0 {
            return Ok(RxEvent::CrcError);
        }
        let mut st = [0u8; 2];
        self.command_read(CMD_GET_RX_BUFFER_STATUS, &[], &mut st)?;
        let (len, ptr) = (st[0] as usize, st[1]);
        let mut frame = RxFrame { data: [0; 256], len, rssi: 0.0, snr: 0.0 };
        self.command_read(CMD_READ_BUFFER, &[ptr], &mut frame.data[..len])?;
        let mut ps = [0u8; 3];
        self.command_read(CMD_GET_PACKET_STATUS, &[], &mut ps)?;
        frame.rssi = -(ps[0] as f32) / 2.0;
        frame.snr = (ps[1] as i8) as f32 / 4.0;
        Ok(RxEvent::Frame(frame))
    }

    /// Instantaneous RSSI in dBm (noise floor sampling, interference threshold).
    fn rssi_inst(&mut self) -> Result<i16, Error> {
        let mut r = [0u8];
        self.command_read(CMD_GET_RSSI_INST, &[], &mut r)?;
        Ok(-(r[0] as i16) / 2)
    }

    /// Is a packet arriving right now? Preamble/header detection with the same staleness
    /// timeouts as upstream, so a missed RX-done cannot block TX forever.
    fn is_receiving(&mut self) -> Result<bool, Error> {
        let irq = self.irq_status()?;
        let preamble = irq & IRQ_PREAMBLE_DETECTED != 0;
        let header = irq & IRQ_HEADER_VALID != 0;
        let detect_bits = IRQ_PREAMBLE_DETECTED | IRQ_HEADER_VALID | IRQ_HEADER_ERR | IRQ_SYNC_WORD_VALID;
        let now = Instant::now();
        if irq & IRQ_HEADER_ERR != 0 {
            self.clear_irq(detect_bits)?;
            self.activity_at = None;
            self.header_seen = false;
            return Ok(false);
        }
        if !header && self.header_seen {
            self.activity_at = None;
            self.header_seen = false;
            return Ok(false);
        }
        if header {
            if !self.header_seen {
                self.header_seen = true;
                self.activity_at = Some(now);
            }
            if self.activity_at.is_some_and(|t| (now - t).as_millis() > self.max_payload_ms) {
                self.clear_irq(detect_bits)?;
                self.activity_at = None;
                self.header_seen = false;
                return Ok(false);
            }
            return Ok(true);
        }
        if preamble {
            let t = *self.activity_at.get_or_insert(now);
            if (now - t).as_millis() > self.preamble_ms {
                self.clear_irq(IRQ_PREAMBLE_DETECTED)?;
                self.activity_at = None;
                return Ok(false);
            }
            return Ok(true);
        }
        self.activity_at = None;
        self.header_seen = false;
        Ok(false)
    }

    /// Random bytes from the radio's wideband RSSI noise (must be in RX).
    async fn random_bytes(&mut self, out: &mut [u8]) -> Result<(), Error> {
        for chunk in out.chunks_mut(4) {
            Timer::after_micros(200).await;
            let mut r = [0u8; 4];
            self.read_register(REG_RANDOM, &mut r)?;
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
        Ok(())
    }

    // ---------------------------------------------------------------- TX

    /// Transmit one frame and wait for TX-done. Leaves the radio in standby; call
    /// [`Self::start_rx`] afterwards.
    async fn transmit(&mut self, frame: &[u8], timeout_ms: u32) -> Result<(), Error> {
        self.command(CMD_SET_STANDBY, &[STDBY_RC])?;
        self.set_packet_params(frame.len() as u8)?;
        self.wait_busy(20)?;
        self.nss.set_low();
        let _ = self.spi.write(&[CMD_WRITE_BUFFER, 0x00]);
        let _ = self.spi.write(frame);
        let _ = self.spi.flush();
        self.nss.set_high();
        self.set_irq(IRQ_TX_DONE | IRQ_TIMEOUT, IRQ_TX_DONE | IRQ_TIMEOUT)?;
        self.clear_irq(IRQ_ALL)?;
        self.command(CMD_SET_TX, &[0x00, 0x00, 0x00])?;

        let done = with_timeout(Duration::from_millis(timeout_ms as u64), self.dio1.wait_for_high()).await;
        let irq = self.irq_status()?;
        self.clear_irq(IRQ_ALL)?;
        if done.is_err() || irq & IRQ_TX_DONE == 0 {
            self.command(CMD_SET_STANDBY, &[STDBY_RC])?;
            return Err(Error::TxTimeout);
        }
        Ok(())
    }

    /// Channel activity detection (upstream `cad on`): is someone transmitting LoRa right now?
    /// Leaves the radio back in RX.
    async fn channel_activity(&mut self) -> Result<bool, Error> {
        self.command(CMD_SET_STANDBY, &[STDBY_RC])?;
        // RadioLib defaults: 2 symbols, detPeak = SF + 13, detMin = 10, CAD only
        let sf = self.params.sf;
        self.command(CMD_SET_CAD_PARAMS, &[0x01, sf + 13, 10, 0x00, 0, 0, 0])?;
        self.set_irq(IRQ_CAD_DONE | IRQ_CAD_DETECTED, IRQ_CAD_DONE)?;
        self.clear_irq(IRQ_ALL)?;
        self.command(CMD_SET_CAD, &[])?;
        let _ = with_timeout(Duration::from_millis(200), self.dio1.wait_for_high()).await;
        let irq = self.irq_status()?;
        self.start_rx()?;
        Ok(irq & IRQ_CAD_DETECTED != 0)
    }

    /// Full receiver reset (upstream `sx126xResetAGC`): warm sleep powers the analog front end
    /// down, a full calibration refreshes ADC/PLL/image, then RX settings are re-applied.
    async fn reset_agc(&mut self) -> Result<(), Error> {
        self.command(CMD_SET_STANDBY, &[STDBY_RC])?;
        self.command(CMD_SET_SLEEP, &[0x04])?; // warm start: configuration retained
        Timer::after_millis(2).await;
        self.nss.set_low(); // wake up
        Timer::after_micros(200).await;
        self.nss.set_high();
        self.wait_busy(10)?;
        self.command(CMD_SET_STANDBY, &[STDBY_RC])?;
        self.command(CMD_CALIBRATE, &[0x7F])?;
        Timer::after_millis(5).await;
        self.wait_busy(50)?;
        self.command(CMD_CALIBRATE_IMAGE, &image_cal(self.params.freq_mhz))?;
        self.wait_busy(50)?;
        self.command(CMD_SET_DIO2_AS_RF_SWITCH, &[0x01])?;
        self.set_rx_boosted_gain(self.rx_boost)?;
        self.modify_register(REG_HELTEC_V4_PATCH, 0x01, 0)?;
        self.start_rx()
    }

    /// Warm sleep (used before power-off).
    fn sleep(&mut self) -> Result<(), Error> {
        self.command(CMD_SET_STANDBY, &[STDBY_RC])?;
        self.command(CMD_SET_SLEEP, &[0x04])
    }

    fn params(&self) -> RadioParams {
        self.params
    }
}
