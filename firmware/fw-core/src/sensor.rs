//! Drive a text-described sensor (`meshcore::sensor::descriptor`) over an `embedded-hal-async`
//! I2C bus: apply its init writes, read each field's register and append the CayenneLPP records.
//! The parsing and scaling are sans-IO in the core crate; this is only the bus traffic.

use alloc::vec::Vec;

use embassy_time::{Duration, with_timeout};
use embedded_hal_async::i2c::I2c;
use meshcore::lpp;
use meshcore::sensor::descriptor::Descriptor;

/// A transfer on a stuck bus must not hold up the repeater (as in the other I2C drivers).
const TIMEOUT: Duration = Duration::from_millis(50);

/// Read one descriptor's fields onto `channel` (all of a sensor's records share its channel, as
/// upstream numbers them). Appends nothing and returns `false` if the device is absent or the bus
/// stalls, so a disconnected sensor simply drops out of the telemetry.
pub async fn read_descriptor<I: I2c>(desc: &Descriptor, i2c: &mut I, channel: u8, out: &mut Vec<u8>) -> bool {
    // Config/calibration writes, reapplied each cycle: idempotent, and robust to a power glitch
    // resetting the chip (the INA219 driver re-writes its calibration for the same reason).
    for (reg, bytes) in &desc.init {
        let mut frame = Vec::with_capacity(bytes.len() + 1);
        frame.push(*reg);
        frame.extend_from_slice(bytes);
        if !matches!(with_timeout(TIMEOUT, i2c.write(desc.addr, &frame)).await, Ok(Ok(()))) {
            return false;
        }
    }
    let mut records = Vec::new();
    let mut raw = [0u8; 5]; // up to 4 data bytes + an optional SMBus PEC byte
    for field in &desc.fields {
        let buf = &mut raw[..field.read_len()];
        if !matches!(with_timeout(TIMEOUT, i2c.write_read(desc.addr, &[field.reg], buf)).await, Ok(Ok(()))) {
            return false;
        }
        // A failed PEC is corrupt data, not a missing device: drop the field, keep the rest.
        if let Some(v) = field.decode(desc.addr, buf) {
            lpp::push(&mut records, channel, field.lpp, v);
        }
    }
    out.extend_from_slice(&records);
    true
}
