//! Environment-sensor support, split sans-IO from the firmware's I2C.
//!
//! Each driver here is the pure part of a sensor: it turns raw register bytes into physical
//! values (often through the chip's own compensation maths) and appends them as CayenneLPP
//! records (`crate::lpp`), exactly as upstream's `EnvironmentSensorManager` queries do. The
//! firmware owns the I2C bus: it probes the address, reads the registers and hands the bytes
//! to the driver. This keeps the fiddly, bug-prone maths on the host test bench.

pub mod ahtx0;
pub mod bme280;
pub mod bmp280;
pub mod descriptor;
pub mod sht4x;
pub mod shtc3;

/// Sensirion CRC-8 (poly `0x31`, init `0xFF`), over the two-byte word that precedes each check
/// byte in an SHTC3 / SHT4X reply. Returns the CRC the sensor should have appended.
pub(crate) fn sensirion_crc8(data: &[u8]) -> u8 {
    let mut crc: u8 = 0xFF;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x31 } else { crc << 1 };
        }
    }
    crc
}
