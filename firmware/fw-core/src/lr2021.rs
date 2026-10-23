//! Minimal Semtech LR2021 LoRa driver: exactly what MeshCore needs, configured like upstream's
//! `CustomLR2021::std_init()` (RadioLib 7.7).
//!
//! The LR2021 talks in 16-bit commands. A write is one transaction (`cmd, args`); a read is the
//! command followed by a second transaction of NOPs that returns `stat1, stat2, data`. Every
//! transaction also clocks out the IRQ flags after the two status bytes, which is how the IRQ
//! state is read. BUSY must be low before each transaction.

use embassy_time::{Duration, Instant, Timer, with_timeout};
use embedded_hal::spi::SpiBus;
use embedded_hal::digital::{InputPin, OutputPin};
use meshcore::radio::RadioParams;

use crate::radio::{Error, IrqPin, LoraRadio, RxEvent, RxFrame};

const CMD_NOP: u16 = 0x0000;
const CMD_READ_RX_FIFO: u16 = 0x0001;
const CMD_WRITE_TX_FIFO: u16 = 0x0002;
const CMD_WRITE_REG_MEM_MASK_32: u16 = 0x0105;
const CMD_SET_SLEEP: u16 = 0x0127;
const CMD_SET_STANDBY: u16 = 0x0128;
const CMD_SET_RX: u16 = 0x020C;
const CMD_SET_TX: u16 = 0x020D;
const CMD_SET_RX_TX_FALLBACK_MODE: u16 = 0x0206;
const CMD_GET_RX_PKT_LENGTH: u16 = 0x0212;
const CMD_CALIBRATE: u16 = 0x0122;
const CMD_CALIB_FRONT_END: u16 = 0x0123;
const CMD_GET_RANDOM_NUMBER: u16 = 0x0126;
const CMD_GET_VERSION: u16 = 0x0101;
const CMD_CLEAR_ERRORS: u16 = 0x0111;
const CMD_GET_ERRORS: u16 = 0x0110;
const CMD_SET_DIO_FUNCTION: u16 = 0x0112;
const CMD_SET_DIO_IRQ_CONFIG: u16 = 0x0115;
const CMD_CLEAR_IRQ: u16 = 0x0116;
const CMD_CLEAR_RX_FIFO: u16 = 0x011E;
const CMD_SET_TCXO_MODE: u16 = 0x0120;
const CMD_SET_RF_FREQUENCY: u16 = 0x0200;
const CMD_SET_RX_PATH: u16 = 0x0201;
const CMD_GET_RSSI_INST: u16 = 0x020B;
const CMD_SET_PA_CONFIG: u16 = 0x0202;
const CMD_SET_TX_PARAMS: u16 = 0x0203;
const CMD_SET_PACKET_TYPE: u16 = 0x0207;
const CMD_SET_LORA_MODULATION_PARAMS: u16 = 0x0220;
const CMD_SET_LORA_PACKET_PARAMS: u16 = 0x0221;
const CMD_SET_LORA_SYNCWORD: u16 = 0x0223;
const CMD_SET_LORA_CAD_PARAMS: u16 = 0x0227;
const CMD_SET_LORA_CAD: u16 = 0x0228;
const CMD_GET_LORA_PACKET_STATUS: u16 = 0x022A;

const REG_LORA_MODEM_TXRX_CFG0: u32 = 0xF30A14;

const IRQ_PREAMBLE_DETECTED: u32 = 1 << 5;
const IRQ_LORA_HEADER_VALID: u32 = 1 << 6;
const IRQ_CAD_DETECTED: u32 = 1 << 7;
const IRQ_LORA_HDR_CRC_ERROR: u32 = 1 << 9;
const IRQ_RX_DONE: u32 = 1 << 18;
const IRQ_TX_DONE: u32 = 1 << 19;
const IRQ_CAD_DONE: u32 = 1 << 20;
const IRQ_TIMEOUT: u32 = 1 << 21;
const IRQ_CRC_ERROR: u32 = 1 << 22;
const IRQ_ALL: u32 = 0xFFFF_FFFF;

const STANDBY_RC: u8 = 0x00;
const FALLBACK_STBY_RC: u8 = 0x01;
const PACKET_TYPE_LORA: u8 = 0x00;
const DIO_FUNCTION_IRQ: u8 = 0x10;
const DIO_SLEEP_PULL_DOWN: u8 = 0x01;
const DIO_SLEEP_PULL_UP: u8 = 0x02;
const CALIBRATE_ALL: u8 = 0x6F;
const SYNC_WORD_PRIVATE: u8 = 0x12;
const PA_RAMP_48U: u8 = 0x05;
const PA_HF_DUTY_CYCLE_UNUSED: u8 = 0x10;
/// RadioLib's boost level for "boosted gain" (`LR2021_RX_BOOST_LEVEL`).
const RX_BOOST_LEVEL: u8 = 7;
const CAL_IMG_FREQ_TRIG_MHZ: f32 = 20.0;
const LF_CUTOFF_FREQ_MHZ: f32 = 1500.0;
/// Device error bits: calibration failures (front end, PLL, image, ...).
const ERR_CALIBRATION: u16 = 0xFCF8;
const ERR_SRC_SATURATION: u16 = 1 << 14;

/// Sub-GHz PA settings for -9..=22 dBm: (duty cycle, slices, PA value) (RadioLib `paOptTableLf`).
const PA_TABLE_LF: [(u8, u8, i8); 32] = [
    (1, 1, 8), (2, 2, 1), (2, 2, 3), (2, 2, 5), (1, 2, 13), (2, 1, 13), (2, 2, 11), (2, 2, 13),
    (3, 1, 12), (1, 1, 18), (1, 1, 20), (1, 1, 23), (1, 1, 27), (1, 1, 33), (1, 2, 26), (1, 2, 31),
    (1, 3, 27), (1, 1, 37), (1, 2, 40), (2, 1, 38), (2, 2, 39), (2, 4, 40), (2, 7, 41), (3, 2, 39),
    (3, 3, 39), (3, 6, 38), (4, 3, 37), (4, 5, 37), (4, 7, 38), (5, 3, 37), (5, 6, 37), (6, 7, 35),
];

/// CAD detection peak by SF for 2 symbols (RadioLib reference values).
const CAD_DET_PEAK: [u8; 8] = [56, 56, 56, 58, 58, 60, 64, 68];

fn bw_code(bw_khz: f32) -> u8 {
    // (bandwidth, LoRa modulation code) - nearest match
    const TABLE: [(f32, u8); 12] = [
        (31.25, 0x02), (41.67, 0x0A), (62.5, 0x03), (83.34, 0x0B), (101.5625, 0x0C), (125.0, 0x04),
        (203.125, 0x0D), (250.0, 0x05), (406.25, 0x0E), (500.0, 0x06), (812.5, 0x0F), (1000.0, 0x07),
    ];
    let mut best = TABLE[0];
    for e in TABLE {
        if libm::fabsf(e.0 - bw_khz) < libm::fabsf(best.0 - bw_khz) {
            best = e;
        }
    }
    best.1
}

pub struct Lr2021<SPI: SpiBus, OUT: OutputPin, IN: InputPin, IRQ: IrqPin> {
    spi: SPI,
    nss: OUT,
    reset: OUT,
    busy: IN,
    irq: IRQ,
    /// The DIO (5..=11) wired to `irq`.
    irq_dio: u8,
    /// TCXO supply code (`RADIOLIB_LRXXXX_TCXO_VOLTAGE_*`); `None` for a crystal.
    tcxo: Option<u8>,
    params: RadioParams,
    rx_boost: bool,
    /// Frequency of the last front-end (image) calibration.
    cal_freq_mhz: f32,
    // "is a packet coming in?" state (upstream `CustomLR2021::isReceiving`)
    activity_at: Option<Instant>,
    header_seen: bool,
    preamble_ms: u64,
    max_payload_ms: u64,
}

impl<SPI: SpiBus, OUT: OutputPin, IN: InputPin, IRQ: IrqPin> Lr2021<SPI, OUT, IN, IRQ> {
    pub fn new(
        spi: SPI, nss: OUT, reset: OUT, busy: IN, irq: IRQ, irq_dio: u8,
        tcxo: Option<u8>,
    ) -> Self {
        Self {
            spi,
            nss,
            reset,
            busy,
            irq,
            irq_dio,
            tcxo,
            params: RadioParams::default(),
            rx_boost: false,
            cal_freq_mhz: 0.0,
            activity_at: None,
            header_seen: false,
            preamble_ms: 66,
            max_payload_ms: 3934,
        }
    }

    // ---------------------------------------------------------------- SPI

    fn wait_busy(&mut self, timeout_ms: u64) -> Result<(), Error> {
        let start = Instant::now();
        while self.busy.is_high().unwrap_or(false) {
            if start.elapsed() > Duration::from_millis(timeout_ms) {
                return Err(Error::Busy);
            }
        }
        Ok(())
    }

    fn write(&mut self, cmd: u16, args: &[u8]) -> Result<(), Error> {
        self.wait_busy(50)?;
        let _ = self.nss.set_low();
        let _ = self.spi.write(&cmd.to_be_bytes());
        let _ = self.spi.write(args);
        let _ = self.spi.flush();
        let _ = self.nss.set_high();
        Ok(())
    }

    /// The command, then a NOP transaction answering `stat1, stat2, out...`.
    fn read(&mut self, cmd: u16, args: &[u8], out: &mut [u8]) -> Result<(), Error> {
        self.write(cmd, args)?;
        self.wait_busy(50)?;
        let mut status = [0u8; 2];
        let _ = self.nss.set_low();
        let _ = self.spi.transfer_in_place(&mut status);
        out.fill(0);
        let _ = self.spi.transfer_in_place(out);
        let _ = self.spi.flush();
        let _ = self.nss.set_high();
        if status[0] == 0x00 || status[0] == 0xFF {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    fn irq_status(&mut self) -> Result<u32, Error> {
        self.wait_busy(50)?;
        let mut buf = [0u8; 6];
        let _ = self.nss.set_low();
        let _ = self.spi.transfer_in_place(&mut buf);
        let _ = self.spi.flush();
        let _ = self.nss.set_high();
        Ok(u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]))
    }

    fn clear_irq(&mut self, mask: u32) -> Result<(), Error> {
        self.write(CMD_CLEAR_IRQ, &mask.to_be_bytes())
    }

    fn set_irq(&mut self, mask: u32) -> Result<(), Error> {
        let m = mask.to_be_bytes();
        self.write(CMD_SET_DIO_IRQ_CONFIG, &[self.irq_dio, m[0], m[1], m[2], m[3]])
    }

    fn standby(&mut self) -> Result<(), Error> {
        // a NOP first pulls NSS low, which wakes the chip from sleep
        let _ = self.nss.set_low();
        let _ = self.spi.write(&CMD_NOP.to_be_bytes());
        let _ = self.spi.flush();
        let _ = self.nss.set_high();
        self.write(CMD_SET_STANDBY, &[STANDBY_RC])
    }

    pub fn errors(&mut self) -> Result<u16, Error> {
        let mut e = [0u8; 2];
        self.read(CMD_GET_ERRORS, &[], &mut e)?;
        Ok(u16::from_be_bytes(e))
    }

    pub fn version(&mut self) -> Result<(u8, u8), Error> {
        let mut v = [0u8; 2];
        self.read(CMD_GET_VERSION, &[], &mut v)?;
        Ok((v[0], v[1]))
    }

    // ---------------------------------------------------------------- setup

    /// Image calibration of the front end near `freq_mhz`; repeated while a strong signal
    /// saturates the receiver.
    fn calibrate_front_end(&mut self, freq_mhz: f32) -> Result<(), Error> {
        let path = if freq_mhz > LF_CUTOFF_FREQ_MHZ { 0x8000 } else { 0 };
        let f = ((freq_mhz / 4.0 + 0.5) as u16 | path).to_be_bytes();
        for _ in 0..10 {
            self.write(CMD_CALIB_FRONT_END, &[f[0], f[1], 0, 0, 0, 0])?;
            self.wait_busy(200)?;
            let errs = self.errors()?;
            if errs & ERR_CALIBRATION == 0 {
                self.cal_freq_mhz = freq_mhz;
                return Ok(());
            }
            self.write(CMD_CLEAR_ERRORS, &[])?;
            if errs & ERR_SRC_SATURATION == 0 {
                log::warn!("LR2021: front-end calibration failed: 0x{errs:04X}");
                return Err(Error::Command);
            }
        }
        Err(Error::Command)
    }

    fn set_packet_params(&mut self, payload_len: u8) -> Result<(), Error> {
        let pre = self.params.preamble_len().to_be_bytes();
        // explicit header (0 << 2), CRC on (1 << 1), standard IQ
        self.write(CMD_SET_LORA_PACKET_PARAMS, &[pre[0], pre[1], payload_len, 0x02])
    }

    fn set_modulation(&mut self, cr: u8) -> Result<(), Error> {
        let p = self.params;
        let ldro = if p.ldro() { 1 } else { 0 };
        let args = [(p.sf << 4) | bw_code(p.bw_khz), ((cr.clamp(5, 8) - 4) << 4) | ldro];
        self.write(CMD_SET_LORA_MODULATION_PARAMS, &args)
    }

    fn set_rx_path(&mut self) -> Result<(), Error> {
        let boost = if self.rx_boost { RX_BOOST_LEVEL } else { 0 };
        self.write(CMD_SET_RX_PATH, &[0x00, boost]) // sub-GHz path
    }
}

impl<SPI: SpiBus, OUT: OutputPin, IN: InputPin, IRQ: IrqPin> LoraRadio for Lr2021<SPI, OUT, IN, IRQ> {
    async fn init(&mut self, params: RadioParams, tx_power: i8, rx_boost: bool) -> Result<(), Error> {
        self.params = params;
        self.rx_boost = rx_boost;
        let _ = self.reset.set_low();
        Timer::after_millis(10).await;
        let _ = self.reset.set_high();
        Timer::after_millis(300).await; // typical boot 273 ms
        self.wait_busy(3000)?;

        let (major, minor) = self.version()?;
        log::info!("LR2021: firmware {major}.{minor:02X}");
        if major != 0x01 {
            return Err(Error::NotFound);
        }
        self.standby()?;
        if let Some(tune) = self.tcxo {
            // 5 ms start-up, in 30.52 us steps
            self.write(CMD_SET_TCXO_MODE, &[tune & 0x07, 0, 0, 0, 164])?;
        }
        self.write(CMD_SET_RX_TX_FALLBACK_MODE, &[FALLBACK_STBY_RC])?;
        self.clear_irq(IRQ_ALL)?;
        let pull = if self.irq_dio == 5 { DIO_SLEEP_PULL_UP } else { DIO_SLEEP_PULL_DOWN };
        self.write(CMD_SET_DIO_FUNCTION, &[self.irq_dio, DIO_FUNCTION_IRQ | pull])?;
        self.write(CMD_CALIBRATE, &[CALIBRATE_ALL])?;
        Timer::after_millis(5).await;
        self.wait_busy(200)?;
        let errs = self.errors()?;
        if errs != 0 {
            log::warn!("LR2021: errors after calibration 0x{errs:04X}");
            self.write(CMD_CLEAR_ERRORS, &[])?;
        }
        self.write(CMD_SET_PACKET_TYPE, &[PACKET_TYPE_LORA])?;
        // SF6 legacy mode off
        let mut reg = [0u8; 11];
        reg[..3].copy_from_slice(&REG_LORA_MODEM_TXRX_CFG0.to_be_bytes()[1..]);
        reg[3..7].copy_from_slice(&(3u32 << 18).to_be_bytes());
        self.write(CMD_WRITE_REG_MEM_MASK_32, &reg)?;
        self.apply_params(params)?;
        self.set_tx_power(tx_power)?;
        self.start_rx()
    }

    fn apply_params(&mut self, p: RadioParams) -> Result<(), Error> {
        self.params = p;
        self.standby()?;
        if libm::fabsf(p.freq_mhz - self.cal_freq_mhz) >= CAL_IMG_FREQ_TRIG_MHZ {
            self.calibrate_front_end(p.freq_mhz)?;
        }
        self.write(CMD_SET_RF_FREQUENCY, &((p.freq_mhz as f64 * 1_000_000.0) as u32).to_be_bytes())?;
        self.set_modulation(p.cr)?;
        self.write(CMD_SET_LORA_SYNCWORD, &[SYNC_WORD_PRIVATE])?;
        self.set_packet_params(255)?;
        let (pre, payload) = p.max_packet_millis();
        self.preamble_ms = pre as u64;
        self.max_payload_ms = payload as u64;
        Ok(())
    }

    fn set_frame_cr(&mut self, cr: Option<u8>) -> Result<(), Error> {
        self.standby()?;
        self.set_modulation(cr.unwrap_or(self.params.cr))
    }

    fn set_tx_power(&mut self, dbm: i8) -> Result<(), Error> {
        let (duty, slices, pa_val) = PA_TABLE_LF[(dbm.clamp(-9, 22) + 9) as usize];
        // low-frequency PA, full single-ended mode
        self.write(CMD_SET_PA_CONFIG, &[0x00, (duty << 4) | (slices & 0x0F), PA_HF_DUTY_CYCLE_UNUSED])?;
        self.write(CMD_SET_TX_PARAMS, &[pa_val as u8, PA_RAMP_48U])
    }

    fn set_rx_boosted_gain(&mut self, on: bool) -> Result<(), Error> {
        self.rx_boost = on;
        self.standby()?; // the RX path is only accepted in standby
        self.start_rx()
    }

    fn start_rx(&mut self) -> Result<(), Error> {
        self.standby()?;
        self.set_rx_path()?;
        self.set_irq(IRQ_RX_DONE)?;
        self.clear_irq(IRQ_ALL)?;
        self.set_packet_params(255)?;
        self.write(CMD_SET_RX, &[0xFF, 0xFF, 0xFF])?; // continuous
        self.activity_at = None;
        self.header_seen = false;
        Ok(())
    }

    async fn wait_irq(&mut self) {
        self.irq.wait_high().await
    }

    fn arm_wake(&mut self) -> bool {
        self.irq.arm_wake()
    }

    fn read_rx(&mut self) -> Result<RxEvent, Error> {
        let irq = self.irq_status()?;
        if irq & IRQ_RX_DONE == 0 {
            self.clear_irq(IRQ_ALL)?;
            if irq & IRQ_TIMEOUT != 0 {
                self.start_rx()?;
            }
            return Ok(RxEvent::Nothing);
        }
        let event = if irq & (IRQ_CRC_ERROR | IRQ_LORA_HDR_CRC_ERROR) != 0 || irq & IRQ_LORA_HEADER_VALID == 0 {
            RxEvent::CrcError
        } else {
            let mut l = [0u8; 2];
            self.read(CMD_GET_RX_PKT_LENGTH, &[], &mut l)?;
            let len = (u16::from_be_bytes(l) as usize).min(255);
            let mut frame = RxFrame { data: [0; 256], len, rssi: 0.0, snr: 0.0 };
            // the FIFO read is a single transaction without status bytes
            self.wait_busy(50)?;
            let _ = self.nss.set_low();
            let _ = self.spi.write(&CMD_READ_RX_FIFO.to_be_bytes());
            let _ = self.spi.transfer_in_place(&mut frame.data[..len]);
            let _ = self.spi.flush();
            let _ = self.nss.set_high();
            let mut st = [0u8; 6];
            self.read(CMD_GET_LORA_PACKET_STATUS, &[], &mut st)?;
            frame.snr = (st[2] as i8) as f32 / 4.0;
            frame.rssi = -((((st[3] as u16) << 1) | ((st[5] as u16 & 0x02) >> 1)) as f32) / 2.0;
            RxEvent::Frame(frame)
        };
        self.write(CMD_CLEAR_RX_FIFO, &[])?;
        self.start_rx()?;
        Ok(event)
    }

    fn rssi_inst(&mut self) -> Result<i16, Error> {
        let mut r = [0u8; 2];
        self.read(CMD_GET_RSSI_INST, &[], &mut r)?;
        let raw = ((r[0] as i16) << 1) | ((r[1] >> 7) & 0x01) as i16;
        Ok(-raw / 2)
    }

    fn is_receiving(&mut self) -> Result<bool, Error> {
        let irq = self.irq_status()?;
        let preamble = irq & IRQ_PREAMBLE_DETECTED != 0;
        let header = irq & IRQ_LORA_HEADER_VALID != 0;
        let detect_bits = IRQ_PREAMBLE_DETECTED | IRQ_LORA_HEADER_VALID | IRQ_LORA_HDR_CRC_ERROR;
        let now = Instant::now();
        if irq & IRQ_LORA_HDR_CRC_ERROR != 0 {
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

    async fn random_bytes(&mut self, out: &mut [u8]) -> Result<(), Error> {
        for chunk in out.chunks_mut(4) {
            let mut r = [0u8; 4];
            self.read(CMD_GET_RANDOM_NUMBER, &[], &mut r)?;
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
        Ok(())
    }

    async fn transmit(&mut self, frame: &[u8], timeout_ms: u32) -> Result<(), Error> {
        self.standby()?;
        self.set_packet_params(frame.len() as u8)?;
        self.set_irq(IRQ_TX_DONE | IRQ_TIMEOUT)?;
        self.write(CMD_WRITE_TX_FIFO, frame)?;
        self.clear_irq(IRQ_ALL)?;
        self.write(CMD_SET_TX, &[0, 0, 0])?;
        self.wait_busy(50)?; // PA ramp-up done

        let done = with_timeout(Duration::from_millis(timeout_ms as u64), self.irq.wait_high()).await;
        let irq = self.irq_status()?;
        self.clear_irq(IRQ_ALL)?;
        self.standby()?;
        if done.is_err() || irq & IRQ_TX_DONE == 0 {
            return Err(Error::TxTimeout);
        }
        Ok(())
    }

    async fn channel_activity(&mut self) -> Result<bool, Error> {
        self.standby()?;
        let peak = CAD_DET_PEAK[(self.params.sf.clamp(5, 12) - 5) as usize];
        // 2 symbols, standard PNR delta, exit to the fallback mode, no timeout
        self.write(CMD_SET_LORA_CAD_PARAMS, &[2, 0x00, 0x00, 0, 0, 0, peak & 0x7F])?;
        self.set_irq(IRQ_CAD_DETECTED | IRQ_CAD_DONE)?;
        self.clear_irq(IRQ_ALL)?;
        self.write(CMD_SET_LORA_CAD, &[])?;
        let _ = with_timeout(Duration::from_millis(200), self.irq.wait_high()).await;
        let irq = self.irq_status()?;
        self.start_rx()?;
        Ok(irq & IRQ_CAD_DETECTED != 0)
    }

    async fn reset_agc(&mut self) -> Result<(), Error> {
        self.standby()?;
        self.write(CMD_CALIBRATE, &[CALIBRATE_ALL])?;
        Timer::after_millis(5).await;
        self.wait_busy(200)?;
        self.calibrate_front_end(self.params.freq_mhz)?;
        self.start_rx()
    }

    fn sleep(&mut self) -> Result<(), Error> {
        self.standby()?;
        // configuration retained, no wake-up timer; BUSY stays high in sleep, so no wait after
        let _ = self.nss.set_low();
        let _ = self.spi.write(&CMD_SET_SLEEP.to_be_bytes());
        let _ = self.spi.write(&[0x02, 0, 0, 0, 0]);
        let _ = self.spi.flush();
        let _ = self.nss.set_high();
        Ok(())
    }

    fn params(&self) -> RadioParams {
        self.params
    }
}
