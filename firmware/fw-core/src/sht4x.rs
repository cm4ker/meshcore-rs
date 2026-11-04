//! SHT4x over an `embedded-hal-async` I2C bus (upstream `ENV_INCLUDE_SHT4X`).
//!
//! Decode + CRC are sans-IO in `meshcore::sensor::sht4x`; this is the bus traffic: one command
//! byte, a short wait, then a six-byte read. The `-A` parts answer at `0x44`, the `-B` parts at
//! `0x45`; both are probed. Borrows the bus per call.

use embassy_time::{Duration, Timer, with_timeout};
use embedded_hal_async::i2c::I2c;
use meshcore::sensor::sht4x::{Reading, decode};

/// The two SHT4x address variants.
const ADDR_A: u8 = 0x44;
const ADDR_B: u8 = 0x45;

/// Measure at high precision (the longest, most accurate conversion).
const CMD_MEAS_HIGH: u8 = 0xFD;
/// Soft reset.
const CMD_RESET: u8 = 0x94;

/// A transfer on a stuck bus must not hold up the repeater (as in the INA219 driver).
const TIMEOUT: Duration = Duration::from_millis(50);

pub struct Sht4x {
    addr: u8,
}

impl Sht4x {
    /// Soft-reset at each address and keep the one that yields a CRC-valid measurement.
    pub async fn probe<I: I2c>(i2c: &mut I) -> Option<Self> {
        for addr in [ADDR_A, ADDR_B] {
            if !matches!(with_timeout(TIMEOUT, i2c.write(addr, &[CMD_RESET])).await, Ok(Ok(()))) {
                continue;
            }
            Timer::after(Duration::from_millis(1)).await;
            let s = Self { addr };
            if s.read(i2c).await.is_some() {
                return Some(s);
            }
        }
        None
    }

    /// Take one high-precision measurement and decode it. `None` on a bus error or a failed CRC.
    pub async fn read<I: I2c>(&self, i2c: &mut I) -> Option<Reading> {
        if !matches!(with_timeout(TIMEOUT, i2c.write(self.addr, &[CMD_MEAS_HIGH])).await, Ok(Ok(()))) {
            return None;
        }
        // High precision takes ~9 ms.
        Timer::after(Duration::from_millis(10)).await;
        let mut data = [0u8; 6];
        matches!(with_timeout(TIMEOUT, i2c.read(self.addr, &mut data)).await, Ok(Ok(())))
            .then(|| decode(&data))
            .flatten()
    }
}
