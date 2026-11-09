//! BME280 over an `embedded-hal-async` I2C bus (upstream `ENV_INCLUDE_BME280`).
//!
//! The sans-IO maths — calibration parsing and Bosch compensation — lives in
//! `meshcore::sensor::bme280`; this is only the bus traffic: probe the chip id, read the
//! trimming registers once, then take a forced measurement per reading (the low-power mode
//! upstream uses: the chip sleeps between reads). The driver borrows the bus per call rather
//! than owning it, so several sensors can share one I2C.

use embassy_time::{Duration, Timer, with_timeout};
use embedded_hal_async::i2c::I2c;
use meshcore::sensor::bme280::{CHIP_ID, Calib, Reading};

/// SDO low / high select these; probe both, as upstream does for Bosch parts.
pub const ADDR_PRIMARY: u8 = 0x76;
pub const ADDR_SECONDARY: u8 = 0x77;

const REG_ID: u8 = 0xD0;
const REG_CTRL_HUM: u8 = 0xF2;
const REG_STATUS: u8 = 0xF3;
const REG_CTRL_MEAS: u8 = 0xF4;
const REG_DATA: u8 = 0xF7; // press/temp/hum, 8 bytes
const REG_CALIB_TP: u8 = 0x88; // dig_T1..dig_P9, dig_H1 (26 bytes)
const REG_CALIB_H: u8 = 0xE1; // dig_H2..dig_H6 (7 bytes)

/// Humidity oversampling ×1 (`ctrl_hum`); must be written before `ctrl_meas` to take effect.
const CTRL_HUM_X1: u8 = 0x01;
/// `ctrl_meas`: temperature ×1, pressure ×1, forced mode (one measurement, then sleep).
const CTRL_MEAS_FORCED_X1: u8 = (0b001 << 5) | (0b001 << 2) | 0b01;
/// `status` bit 3: a conversion is running.
const STATUS_MEASURING: u8 = 0x08;

/// A transfer on a stuck bus must not hold up the repeater (as in the INA219 driver).
const TIMEOUT: Duration = Duration::from_millis(50);

pub struct Bme280 {
    addr: u8,
    calib: Calib,
}

impl Bme280 {
    /// Look for the chip at either address, read its calibration and set humidity oversampling.
    /// Returns `None` if nothing on the bus identifies as a BME280.
    pub async fn probe<I: I2c>(i2c: &mut I) -> Option<Self> {
        for addr in [ADDR_PRIMARY, ADDR_SECONDARY] {
            let mut id = [0u8; 1];
            if read_regs(i2c, addr, REG_ID, &mut id).await && id[0] == CHIP_ID {
                let mut tp = [0u8; 26];
                let mut h = [0u8; 7];
                if !read_regs(i2c, addr, REG_CALIB_TP, &mut tp).await
                    || !read_regs(i2c, addr, REG_CALIB_H, &mut h).await
                    || !write_reg(i2c, addr, REG_CTRL_HUM, CTRL_HUM_X1).await
                {
                    return None;
                }
                return Some(Self { addr, calib: Calib::from_regs(&tp, &h) });
            }
        }
        None
    }

    pub fn addr(&self) -> u8 {
        self.addr
    }

    /// Take one forced measurement and compensate it, or `None` if the bus misbehaves.
    pub async fn read<I: I2c>(&self, i2c: &mut I) -> Option<Reading> {
        if !write_reg(i2c, self.addr, REG_CTRL_MEAS, CTRL_MEAS_FORCED_X1).await {
            return None;
        }
        // ×1 of all three finishes in under ~10 ms; poll, but never wait forever.
        for _ in 0..10 {
            let mut st = [0u8; 1];
            if !read_regs(i2c, self.addr, REG_STATUS, &mut st).await {
                return None;
            }
            if st[0] & STATUS_MEASURING == 0 {
                break;
            }
            Timer::after(Duration::from_millis(2)).await;
        }
        let mut data = [0u8; 8];
        read_regs(i2c, self.addr, REG_DATA, &mut data).await.then(|| self.calib.read_data(&data))
    }
}

async fn read_regs<I: I2c>(i2c: &mut I, addr: u8, reg: u8, buf: &mut [u8]) -> bool {
    matches!(with_timeout(TIMEOUT, i2c.write_read(addr, &[reg], buf)).await, Ok(Ok(())))
}

async fn write_reg<I: I2c>(i2c: &mut I, addr: u8, reg: u8, value: u8) -> bool {
    matches!(with_timeout(TIMEOUT, i2c.write(addr, &[reg, value])).await, Ok(Ok(())))
}
