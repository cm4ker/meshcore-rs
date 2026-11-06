//! CayenneLPP records as upstream's telemetry writes them (the ElectronicCats CayenneLPP library,
//! 1.6.1 as upstream pins it): channel, type, then the value big-endian, scaled and cut toward zero.

use alloc::vec::Vec;

/// 2 bytes, 0.1 °C, signed.
pub const TEMPERATURE: u8 = 103;
/// 1 byte, 0.5 %, unsigned.
pub const RELATIVE_HUMIDITY: u8 = 104;
/// 2 bytes, 0.1 hPa, unsigned.
pub const BAROMETRIC_PRESSURE: u8 = 115;
/// 2 bytes, 0.01 V, signed.
pub const VOLTAGE: u8 = 116;
/// 2 bytes, 0.001 A, signed: an INA219 on the battery reads a discharge below zero.
pub const CURRENT: u8 = 117;
/// 2 bytes, 1 m, signed.
pub const ALTITUDE: u8 = 121;
/// 2 bytes, 1 W, unsigned.
pub const POWER: u8 = 128;

/// The node's own channel; upstream gives sensors the next ones, in the order it found them.
pub const CHANNEL_SELF: u8 = 1;

/// Append a record of `kind` (one of the types above; others are left out). As in upstream's
/// CayenneLPP library: big-endian, scaled and cut toward zero, an unsigned type keeping the
/// magnitude of a negative value and a signed one storing it two's-complement over its width.
pub fn push(out: &mut Vec<u8>, channel: u8, kind: u8, value: f32) {
    let (size, mult, signed): (u8, f32, bool) = match kind {
        TEMPERATURE => (2, 10.0, true),
        RELATIVE_HUMIDITY => (1, 2.0, false),
        BAROMETRIC_PRESSURE => (2, 10.0, false),
        VOLTAGE => (2, 100.0, true),
        CURRENT => (2, 1000.0, true),
        ALTITUDE => (2, 1.0, true),
        POWER => (2, 1.0, false),
        _ => return,
    };
    let mask: u32 = (1u32 << (8 * size)) - 1;
    let magnitude = ((if value < 0.0 { -value } else { value }) * mult) as u32 & mask;
    let raw = if signed && value < 0.0 { (mask - magnitude + 1) & mask } else { magnitude };
    out.extend_from_slice(&[channel, kind]);
    for i in (0..size).rev() {
        out.push((raw >> (8 * i)) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_as_upstream_writes_them() {
        let mut out = Vec::new();
        push(&mut out, 2, VOLTAGE, 4.137);
        push(&mut out, 2, CURRENT, -0.0123); // 1 mA signed: -12 -> 0xFFF4
        push(&mut out, 2, POWER, -0.6); // unsigned: the magnitude, whole watts
        push(&mut out, 1, TEMPERATURE, -2.5);
        assert_eq!(out, [2, 116, 0x01, 0x9D, 2, 117, 0xFF, 0xF4, 2, 128, 0, 0, 1, 103, 0xFF, 0xE7]);
    }

    #[test]
    fn environment_records() {
        let mut out = Vec::new();
        push(&mut out, 2, RELATIVE_HUMIDITY, 46.3); // 0.5 %: 92 -> 0x5C, one byte
        push(&mut out, 2, BAROMETRIC_PRESSURE, 1013.2); // 0.1 hPa: 10132 -> 0x2794
        push(&mut out, 2, ALTITUDE, -5.0); // 1 m signed: -5 -> 0xFFFB
        assert_eq!(out, [2, 104, 0x5C, 2, 115, 0x27, 0x94, 2, 121, 0xFF, 0xFB]);
    }
}
