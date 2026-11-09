//! SHT3x (SHT30, SHT31, SHT35) over an `embedded-hal-async` I2C bus; not in upstream.
//!
//! Decode + CRC are sans-IO in `meshcore::sensor::sht3x`; this is the bus traffic: a two-byte
//! single-shot command, a wait for the conversion, then a six-byte read. The chip answers at
//! `0x44` (ADDR low) or `0x45`; both are probed. An SHT4x at the same addresses takes one-byte
//! commands and refuses these, so neither driver mistakes the other's chip. Borrows the bus per
//! call.

use embassy_time::{Duration, Timer, with_timeout};
use embedded_hal_async::i2c::I2c;
use meshcore::sensor::sht3x::{Reading, decode};

/// The two SHT3x address variants.
const ADDR_A: u8 = 0x44;
const ADDR_B: u8 = 0x45;

/// Single shot, high repeatability, clock stretching off.
const CMD_MEAS_HIGH: [u8; 2] = [0x24, 0x00];
/// Soft reset.
const CMD_RESET: [u8; 2] = [0x30, 0xA2];

/// A transfer on a stuck bus must not hold up the repeater (as in the INA219 driver).
const TIMEOUT: Duration = Duration::from_millis(50);

pub struct Sht3x {
    addr: u8,
}

impl Sht3x {
    /// Soft-reset at each address and keep the one that yields a CRC-valid measurement.
    pub async fn probe<I: I2c>(i2c: &mut I) -> Option<Self> {
        for addr in [ADDR_A, ADDR_B] {
            if !matches!(with_timeout(TIMEOUT, i2c.write(addr, &CMD_RESET)).await, Ok(Ok(()))) {
                continue;
            }
            Timer::after(Duration::from_millis(2)).await;
            let s = Self { addr };
            if s.read(i2c).await.is_some() {
                return Some(s);
            }
        }
        None
    }

    pub fn addr(&self) -> u8 {
        self.addr
    }

    /// Take one high-repeatability measurement and decode it. `None` on a bus error or a failed
    /// CRC.
    pub async fn read<I: I2c>(&self, i2c: &mut I) -> Option<Reading> {
        if !matches!(with_timeout(TIMEOUT, i2c.write(self.addr, &CMD_MEAS_HIGH)).await, Ok(Ok(()))) {
            return None;
        }
        // high repeatability takes up to 15.5 ms
        Timer::after(Duration::from_millis(16)).await;
        let mut data = [0u8; 6];
        matches!(with_timeout(TIMEOUT, i2c.read(self.addr, &mut data)).await, Ok(Ok(())))
            .then(|| decode(&data))
            .flatten()
    }
}
