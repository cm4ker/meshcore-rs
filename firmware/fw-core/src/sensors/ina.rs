//! Register transfers and readiness polling shared by INA219 and INA226.

use embassy_time::{Duration, Instant, Timer, with_timeout};
use embedded_hal_async::i2c::I2c;

/// Bound each transaction on a stuck bus.
const I2C_TIMEOUT: Duration = Duration::from_millis(50);

pub(super) async fn write<I: I2c>(i2c: &mut I, address: u8, reg: u8, value: u16) -> bool {
    let [hi, lo] = value.to_be_bytes();
    matches!(with_timeout(I2C_TIMEOUT, i2c.write(address, &[reg, hi, lo])).await, Ok(Ok(())))
}

pub(super) async fn register<I: I2c>(i2c: &mut I, address: u8, reg: u8) -> Option<u16> {
    let mut buf = [0u8; 2];
    matches!(with_timeout(I2C_TIMEOUT, i2c.write_read(address, &[reg], &mut buf)).await, Ok(Ok(())))
        .then(|| u16::from_be_bytes(buf))
}

/// Let the ADC run before checking the flag, then allow a bounded grace period.
pub(super) async fn wait_ready<I: I2c>(
    i2c: &mut I, address: u8, reg: u8, mask: u16, ready_at: Instant,
) -> Option<u16> {
    Timer::at(ready_at).await;
    for attempt in 0..3 {
        if attempt != 0 {
            Timer::after_millis(1).await;
        }
        let value = register(i2c, address, reg).await?;
        if value & mask != 0 {
            return Some(value);
        }
    }
    None
}
