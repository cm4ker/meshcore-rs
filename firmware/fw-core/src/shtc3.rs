//! SHTC3 over an `embedded-hal-async` I2C bus (upstream `ENV_INCLUDE_SHTC3`).
//!
//! Decode + CRC are sans-IO in `meshcore::sensor::shtc3`; this is the bus traffic. The chip sleeps
//! between reads, so each cycle wakes it, measures in no-clock-stretch mode (embassy's `Twim` does
//! not clock-stretch), reads six bytes, then puts it back to sleep. Borrows the bus per call.

use embassy_time::{Duration, Timer, with_timeout};
use embedded_hal_async::i2c::I2c;
use meshcore::sensor::shtc3::{ADDR, Reading, decode};

const CMD_WAKEUP: [u8; 2] = [0x35, 0x17];
const CMD_SLEEP: [u8; 2] = [0xB0, 0x98];
/// Normal mode, temperature first, clock stretching disabled.
const CMD_MEAS_T_FIRST: [u8; 2] = [0x78, 0x66];
/// Read-id command; the id has a fixed pattern that identifies an SHTC3.
const CMD_READ_ID: [u8; 2] = [0xEF, 0xC8];

/// A transfer on a stuck bus must not hold up the repeater (as in the INA219 driver).
const TIMEOUT: Duration = Duration::from_millis(50);

pub struct Shtc3;

impl Shtc3 {
    /// Wake the chip and check its id register for the SHTC3 pattern. `None` otherwise.
    pub async fn probe<I: I2c>(i2c: &mut I) -> Option<Self> {
        if !write(i2c, &CMD_WAKEUP).await {
            return None;
        }
        Timer::after(Duration::from_millis(1)).await;
        let mut id = [0u8; 3];
        let got = matches!(with_timeout(TIMEOUT, i2c.write_read(ADDR, &CMD_READ_ID, &mut id)).await, Ok(Ok(())));
        let _ = write(i2c, &CMD_SLEEP).await;
        // Datasheet: bits 11 and 5..0 are fixed; (id & 0x083F) == 0x0807 marks an SHTC3.
        let id = u16::from_be_bytes([id[0], id[1]]);
        (got && id & 0x083F == 0x0807).then_some(Self)
    }

    /// Wake, measure, sleep. `None` on a bus error or a failed CRC.
    pub async fn read<I: I2c>(&self, i2c: &mut I) -> Option<Reading> {
        if !write(i2c, &CMD_WAKEUP).await {
            return None;
        }
        Timer::after(Duration::from_millis(1)).await;
        if !write(i2c, &CMD_MEAS_T_FIRST).await {
            return None;
        }
        // Normal-mode conversion is ~12 ms; wait, since clock stretching is off.
        Timer::after(Duration::from_millis(13)).await;
        let mut data = [0u8; 6];
        let got = matches!(with_timeout(TIMEOUT, i2c.read(ADDR, &mut data)).await, Ok(Ok(())));
        let _ = write(i2c, &CMD_SLEEP).await;
        got.then(|| decode(&data)).flatten()
    }
}

async fn write<I: I2c>(i2c: &mut I, bytes: &[u8]) -> bool {
    matches!(with_timeout(TIMEOUT, i2c.write(ADDR, bytes)).await, Ok(Ok(())))
}
