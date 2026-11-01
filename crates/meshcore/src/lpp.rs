//! CayenneLPP records as upstream's telemetry writes them (the ElectronicCats CayenneLPP library):
//! channel, type, then the value big-endian, scaled and cut toward zero.

use alloc::vec::Vec;

/// 2 bytes, 0.01 V, unsigned.
pub const VOLTAGE: u8 = 116;
/// 2 bytes, 0.001 A, unsigned.
pub const CURRENT: u8 = 117;
/// 2 bytes, 1 W, unsigned.
pub const POWER: u8 = 128;
/// 2 bytes, 0.1 °C, signed.
pub const TEMPERATURE: u8 = 103;

/// The node's own channel; upstream gives sensors the next ones, in the order it found them.
pub const CHANNEL_SELF: u8 = 1;

/// Append a record of `kind` (one of the types above; others are left out). As in upstream's
/// library, an unsigned type keeps the magnitude of a negative value.
pub fn push(out: &mut Vec<u8>, channel: u8, kind: u8, value: f32) {
    let (mult, signed) = match kind {
        VOLTAGE => (100.0, false),
        CURRENT => (1000.0, false),
        POWER => (1.0, false),
        TEMPERATURE => (10.0, true),
        _ => return,
    };
    let magnitude = (if value < 0.0 { -value } else { value } * mult) as u32 as u16;
    let raw = if signed && value < 0.0 { magnitude.wrapping_neg() } else { magnitude };
    out.extend_from_slice(&[channel, kind]);
    out.extend_from_slice(&raw.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_as_upstream_writes_them() {
        let mut out = Vec::new();
        push(&mut out, 2, VOLTAGE, 4.137);
        push(&mut out, 2, CURRENT, -0.0123);
        push(&mut out, 2, POWER, 0.6);
        push(&mut out, 1, TEMPERATURE, -2.5);
        assert_eq!(out, [2, 116, 0x01, 0x9D, 2, 117, 0, 12, 2, 128, 0, 0, 1, 103, 0xFF, 0xE7]);
    }
}
