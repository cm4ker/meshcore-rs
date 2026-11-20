//! An INA226 current sensor on the board's I2C (upstream `ENV_INCLUDE_INA226`): bus voltage,
//! current and power for telemetry, set up as Adafruit's INA226 driver's `begin` and
//! `setMaxCurrentShunt(0.8, 0.1)` do (0.8 A over the 0.1 Ω shunt of the usual breakouts, which is
//! what a small solar panel's output takes).
//!
//! The current is signed, as the chip reports it: positive when it flows into the shunt at VIN+
//! and out at VIN-. Which way is "charging" depends on how the shunt is wired; the bus voltage is
//! measured at VIN-.
//!
//! The INA226 sits at 0x44, where the INA219's driver would talk to the same register numbers but
//! a different chip, so the probe reads the die ID (0x2260) rather than trusting a NACK.

use alloc::vec::Vec;

use embassy_time::{Duration, with_timeout};
use embedded_hal_async::i2c::I2c;
use meshcore::lpp;

/// Upstream `TELEM_INA226_ADDRESS`.
pub const ADDRESS: u8 = 0x44;

const REG_CONFIG: u8 = 0x00;
const REG_BUS_VOLTAGE: u8 = 0x02;
const REG_POWER: u8 = 0x03;
const REG_CURRENT: u8 = 0x04;
const REG_CALIBRATION: u8 = 0x05;
const REG_DIE_ID: u8 = 0xFF;
/// The INA226's answer in [`REG_DIE_ID`], which an INA219 at the same address does not give.
const DIE_ID: u16 = 0x2260;
/// 16 averages of both bus and shunt, 1.1 ms each, continuous.
const CONFIG: u16 = 0x4527;
/// 0.8 A full scale, 0.1 Ω shunt: `trunc(0.00512 / (current_lsb * 0.1))` with
/// `current_lsb = 0.8 / 2^15`.
const CALIBRATION: u16 = 2097;
/// Current per step, in amperes (`current_lsb`).
const CURRENT_LSB: f32 = 0.8 / 32768.0;
/// Power per step, in watts: 25 times the current's.
const POWER_LSB: f32 = 25.0 * CURRENT_LSB;

/// A transfer on a stuck bus must not hold up the repeater.
const TIMEOUT: Duration = Duration::from_millis(50);

pub struct Reading {
    pub bus_v: f32,
    pub current_ma: f32,
    pub power_mw: f32,
}

impl Reading {
    /// As upstream's `query_ina226`: voltage, current, power on one channel.
    pub fn lpp(&self, channel: u8, out: &mut Vec<u8>) {
        lpp::push(out, channel, lpp::VOLTAGE, self.bus_v);
        lpp::push(out, channel, lpp::CURRENT, self.current_ma / 1000.0);
        lpp::push(out, channel, lpp::POWER, self.power_mw / 1000.0);
    }
}

/// The driver is stateless (the calibration lives on the chip); it borrows the shared I2C bus per
/// call so other sensors can sit on the same bus.
pub struct Ina226;

impl Ina226 {
    /// The sensor set up, or `None` if nothing answers at its address or it is not an INA226.
    pub async fn probe<I: I2c>(i2c: &mut I) -> Option<Self> {
        (register(i2c, REG_DIE_ID).await? == DIE_ID && write(i2c, REG_CONFIG, CONFIG).await && write(i2c, REG_CALIBRATION, CALIBRATION).await)
            .then_some(Self)
    }

    pub async fn read<I: I2c>(&self, i2c: &mut I) -> Option<Reading> {
        let bus = register(i2c, REG_BUS_VOLTAGE).await?;
        let current = register(i2c, REG_CURRENT).await? as i16;
        let power = register(i2c, REG_POWER).await? as i16;
        Some(Reading {
            bus_v: bus as f32 * 0.00125,
            current_ma: current as f32 * CURRENT_LSB * 1000.0,
            power_mw: power.max(0) as f32 * POWER_LSB * 1000.0,
        })
    }
}

async fn write<I: I2c>(i2c: &mut I, reg: u8, value: u16) -> bool {
    let [hi, lo] = value.to_be_bytes();
    matches!(with_timeout(TIMEOUT, i2c.write(ADDRESS, &[reg, hi, lo])).await, Ok(Ok(())))
}

async fn register<I: I2c>(i2c: &mut I, reg: u8) -> Option<u16> {
    let mut buf = [0u8; 2];
    matches!(with_timeout(TIMEOUT, i2c.write_read(ADDRESS, &[reg], &mut buf)).await, Ok(Ok(())))
        .then(|| u16::from_be_bytes(buf))
}
