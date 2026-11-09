//! AHT10 / AHT20 over an `embedded-hal-async` I2C bus (upstream `ENV_INCLUDE_AHTX0`).
//!
//! The unpacking maths is sans-IO in `meshcore::sensor::ahtx0`; this is the bus traffic: a soft
//! reset and calibrate at probe, then a trigger-measure command, a wait, and a six-byte read per
//! reading. Borrows the bus per call so several sensors share one I2C.

use embassy_time::{Duration, Timer, with_timeout};
use embedded_hal_async::i2c::I2c;
use meshcore::sensor::ahtx0::{ADDR, Reading, decode};

/// Trigger a measurement, then read status + data.
const CMD_TRIGGER: [u8; 3] = [0xAC, 0x33, 0x00];
/// Calibrate / init (Adafruit_AHTX0 uses `0xE1`; an AHT20 datasheet lists `0xBE` — flip here if a
/// bare AHT20 fails to calibrate).
const CMD_CALIBRATE: [u8; 3] = [0xE1, 0x08, 0x00];
/// Soft reset.
const CMD_RESET: u8 = 0xBA;
/// Status bit 7: still busy; bit 3: calibrated.
const STATUS_BUSY: u8 = 0x80;
const STATUS_CALIBRATED: u8 = 0x08;

/// A transfer on a stuck bus must not hold up the repeater (as in the INA219 driver).
const TIMEOUT: Duration = Duration::from_millis(50);

pub struct Ahtx0;

impl Ahtx0 {
    /// Soft-reset, calibrate, and confirm the chip reports calibrated. `None` if it never does.
    pub async fn probe<I: I2c>(i2c: &mut I) -> Option<Self> {
        if !write(i2c, &[CMD_RESET]).await {
            return None;
        }
        Timer::after(Duration::from_millis(20)).await;
        if !write(i2c, &CMD_CALIBRATE).await {
            return None;
        }
        Timer::after(Duration::from_millis(10)).await;
        let st = status(i2c).await?;
        (st & STATUS_CALIBRATED != 0).then_some(Self)
    }

    /// Trigger one measurement and decode it, or `None` on a bus error or an unfinished conversion.
    pub async fn read<I: I2c>(&self, i2c: &mut I) -> Option<Reading> {
        if !write(i2c, &CMD_TRIGGER).await {
            return None;
        }
        // The measurement takes ~75 ms; the datasheet says to wait rather than clock-stretch.
        Timer::after(Duration::from_millis(80)).await;
        let mut data = [0u8; 6];
        if !matches!(with_timeout(TIMEOUT, i2c.read(ADDR, &mut data)).await, Ok(Ok(()))) {
            return None;
        }
        (data[0] & STATUS_BUSY == 0).then(|| decode(&data))
    }
}

async fn write<I: I2c>(i2c: &mut I, bytes: &[u8]) -> bool {
    matches!(with_timeout(TIMEOUT, i2c.write(ADDR, bytes)).await, Ok(Ok(())))
}

async fn status<I: I2c>(i2c: &mut I) -> Option<u8> {
    let mut st = [0u8; 1];
    matches!(with_timeout(TIMEOUT, i2c.read(ADDR, &mut st)).await, Ok(Ok(()))).then_some(st[0])
}
