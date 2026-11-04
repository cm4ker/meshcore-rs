//! An INA219 current sensor on the board's I2C (upstream `ENV_INCLUDE_INA219`): bus voltage,
//! current and power for telemetry, set up as Adafruit_INA219's `begin()` does (32 V / 2 A
//! calibration, for the 0.1 Ω shunt of the usual breakout boards).

use alloc::vec::Vec;

use embassy_nrf::twim::Twim;
use embassy_time::{Duration, with_timeout};
use meshcore::lpp;

/// Upstream `TELEM_INA219_ADDRESS`.
pub const ADDRESS: u8 = 0x40;

const REG_CONFIG: u8 = 0x00;
const REG_BUS_VOLTAGE: u8 = 0x02;
const REG_POWER: u8 = 0x03;
const REG_CURRENT: u8 = 0x04;
const REG_CALIBRATION: u8 = 0x05;
/// 32 V bus range, ±320 mV across the shunt, 12-bit samples of both, continuous.
const CONFIG_32V_2A: u16 = 0x399F;
/// 0.1 mA per current step and 2 mW per power step with a 0.1 Ω shunt.
const CALIBRATION_32V_2A: u16 = 4096;
const CURRENT_STEPS_PER_MA: f32 = 10.0;
const POWER_MW_PER_STEP: f32 = 2.0;

/// A transfer on a stuck bus must not hold up the repeater.
const TIMEOUT: Duration = Duration::from_millis(50);

pub struct Reading {
    pub bus_v: f32,
    pub current_ma: f32,
    pub power_mw: f32,
}

impl Reading {
    /// As upstream's `query_ina219`: voltage, current, power on one channel.
    pub fn lpp(&self, channel: u8, out: &mut Vec<u8>) {
        lpp::push(out, channel, lpp::VOLTAGE, self.bus_v);
        lpp::push(out, channel, lpp::CURRENT, self.current_ma / 1000.0);
        lpp::push(out, channel, lpp::POWER, self.power_mw / 1000.0);
    }
}

/// The driver is stateless (the calibration lives on the chip); it borrows the shared I2C bus per
/// call so other sensors can sit on the same bus.
pub struct Ina219;

impl Ina219 {
    /// The sensor set up, or `None` if nothing answers at its address.
    pub async fn probe(i2c: &mut Twim<'static>) -> Option<Self> {
        (write(i2c, REG_CALIBRATION, CALIBRATION_32V_2A).await && write(i2c, REG_CONFIG, CONFIG_32V_2A).await)
            .then_some(Self)
    }

    pub async fn read(&self, i2c: &mut Twim<'static>) -> Option<Reading> {
        let bus = register(i2c, REG_BUS_VOLTAGE).await?;
        // a sharp load can reset the chip and its calibration with it: Adafruit's driver writes
        // the calibration again before reading current or power
        write(i2c, REG_CALIBRATION, CALIBRATION_32V_2A).await;
        let current = register(i2c, REG_CURRENT).await? as i16;
        write(i2c, REG_CALIBRATION, CALIBRATION_32V_2A).await;
        let power = register(i2c, REG_POWER).await? as i16;
        Some(Reading {
            bus_v: (bus >> 3) as f32 * 0.004,
            current_ma: current as f32 / CURRENT_STEPS_PER_MA,
            power_mw: power as f32 * POWER_MW_PER_STEP,
        })
    }
}

async fn write(i2c: &mut Twim<'static>, reg: u8, value: u16) -> bool {
    let [hi, lo] = value.to_be_bytes();
    matches!(with_timeout(TIMEOUT, i2c.write(ADDRESS, &[reg, hi, lo])).await, Ok(Ok(())))
}

async fn register(i2c: &mut Twim<'static>, reg: u8) -> Option<u16> {
    let mut buf = [0u8; 2];
    matches!(with_timeout(TIMEOUT, i2c.write_read(ADDRESS, &[reg], &mut buf)).await, Ok(Ok(())))
        .then(|| u16::from_be_bytes(buf))
}
