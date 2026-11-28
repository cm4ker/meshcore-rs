//! An INA219 current sensor on the board's I2C (upstream `ENV_INCLUDE_INA219`): bus voltage,
//! current and power for telemetry, set up as Adafruit_INA219's `begin()` does (32 V / 2 A
//! calibration, for the 0.1 Ω shunt of the usual breakout boards).
//!
//! The current is signed, as the chip and upstream report it: positive when it flows into the
//! shunt at VIN+ and out at VIN-, negative the other way. Which way is "charging" depends on how
//! the shunt is wired; the bus voltage is measured at VIN-.

use alloc::vec::Vec;

use embassy_time::{Duration, Instant};
use embedded_hal_async::i2c::I2c;
use meshcore::lpp;

use super::ina::{register, wait_ready, write};

/// Upstream `TELEM_INA219_ADDRESS`.
pub const ADDRESS: u8 = 0x40;

const REG_CONFIG: u8 = 0x00;
const REG_BUS_VOLTAGE: u8 = 0x02;
const REG_POWER: u8 = 0x03;
const REG_CURRENT: u8 = 0x04;
const REG_CALIBRATION: u8 = 0x05;
/// 32 V bus range, ±320 mV across the shunt, 12-bit samples of both (0x39FF's
/// 128-sample averaging breaks some INA219 clones).
const CONFIG_POWER_DOWN: u16 = 0x3998;
const CONFIG_TRIGGERED: u16 = CONFIG_POWER_DOWN | 0b011;
const CONVERSION_READY: u16 = 1 << 1; // in the bus voltage register
// Two 12-bit conversions (586 µs max each) and 40 µs to recover from power-down.
const CONVERSION_TIME: Duration = Duration::from_micros(1_250);
/// 0.1 mA per current step and 2 mW per power step with a 0.1 Ω shunt.
const CALIBRATION_32V_2A: u16 = 4096;
const CURRENT_STEPS_PER_MA: f32 = 10.0;
const POWER_MW_PER_STEP: f32 = 2.0;

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
    pub async fn probe<I: I2c>(i2c: &mut I) -> Option<Self> {
        (write(i2c, ADDRESS, REG_CONFIG, CONFIG_POWER_DOWN).await
            && write(i2c, ADDRESS, REG_CALIBRATION, CALIBRATION_32V_2A).await)
            .then_some(Self)
    }

    pub async fn read<I: I2c>(&self, i2c: &mut I) -> Option<Reading> {
        let reading = async {
            // Restore calibration before the trigger in case the chip reset since the last poll.
            if !write(i2c, ADDRESS, REG_CALIBRATION, CALIBRATION_32V_2A).await
                || !write(i2c, ADDRESS, REG_CONFIG, CONFIG_TRIGGERED).await
            {
                return None;
            }
            let ready_at = Instant::now() + CONVERSION_TIME;
            let bus = wait_ready(i2c, ADDRESS, REG_BUS_VOLTAGE, CONVERSION_READY, ready_at).await?;
            let current = register(i2c, ADDRESS, REG_CURRENT).await? as i16;
            let power = register(i2c, ADDRESS, REG_POWER).await? as i16;
            Some(Reading {
                bus_v: (bus >> 3) as f32 * 0.004,
                current_ma: current as f32 / CURRENT_STEPS_PER_MA,
                power_mw: power as f32 * POWER_MW_PER_STEP,
            })
        }.await;
        // Best effort even on timeout or I2C error, including an ambiguous trigger write.
        write(i2c, ADDRESS, REG_CONFIG, CONFIG_POWER_DOWN).await;
        reading
    }
}
