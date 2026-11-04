//! Text-described I2C sensors: deliver a new sensor as a line of text, not a new firmware.
//!
//! Many I2C parts (the INA current monitors, simple barometers) need no per-chip compensation:
//! a reading is a register read, an optional endianness flip and a linear scale. Such a sensor is
//! described by one text line the user can add at runtime (`sensor add`), stored in prefs and
//! parsed back here. The firmware's generic driver walks the descriptor, reads each field's
//! register and turns the bytes into CayenneLPP with [`Field::value`] + [`crate::lpp::push`].
//!
//! Grammar (whitespace-separated, after `sensor add`):
//! ```text
//! <name> <addr-hex> [init=<reg>:<hex>[,<reg>:<hex>]...] <field>...
//!   field = <lpp>@<reg>:<len><b|l><s|u>[p][*<mul>][+<off>]
//! ```
//! `len` is 1..=4 bytes, `b`/`l` the endianness, `s`/`u` signedness; the raw integer becomes
//! `value * mul + off` (both optional; `mul` defaults to 1, `off` to 0; a negative `off` is
//! written `+-273.15`). An optional `p` after the signedness means an SMBus PEC byte follows the
//! data and is verified (MLX90614 and similar). `lpp` is one of: `volt cur pow temp hum pres alt`.
//!
//! Example (INA260): `ina260 41 volt@02:2bu*0.00125 cur@01:2bs*0.00125 pow@03:2bu*0.01`
//! Example (MLX90614 object temperature, K→°C with PEC): `mlx 5a temp@07:2lup*0.02+-273.15`

use core::fmt;

use alloc::string::String;
use alloc::vec::Vec;

use crate::lpp;

/// Longest descriptor line accepted, so a malformed prefs blob can't force a huge allocation.
pub const MAX_LEN: usize = 160;

/// Most text-described sensors kept at once (the flash blob and `sensor list` stay small).
pub const MAX_SENSORS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    TooLong,
    BadAddress,
    BadInit,
    BadField,
    BadLppType,
    BadRegister,
    BadLength,
    BadNumber,
    NoFields,
}

/// One value read from the sensor: a register run scaled linearly to an LPP type.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// CayenneLPP type byte (`lpp::VOLTAGE`, …).
    pub lpp: u8,
    pub reg: u8,
    pub len: u8,
    pub big_endian: bool,
    pub signed: bool,
    /// The read returns an SMBus PEC byte after the data (MLX90614 and other SMBus parts): read
    /// `len + 1` bytes and check the CRC before trusting the value.
    pub pec: bool,
    pub mult: f32,
    pub off: f32,
}

impl Field {
    /// Bytes the firmware must read for this field: the data, plus the PEC byte when present.
    pub fn read_len(&self) -> usize {
        self.len as usize + self.pec as usize
    }

    /// Turn a full read (`read_len` bytes) into a value, checking the PEC first when the field has
    /// one. `None` means a failed PEC — corrupt data to drop rather than report. With no PEC it
    /// never returns `None`.
    pub fn decode(&self, addr: u8, buf: &[u8]) -> Option<f32> {
        if self.pec {
            let (data, rest) = buf.split_at(self.len as usize);
            if rest.first().copied()? != smbus_pec(addr, self.reg, data) {
                return None;
            }
            Some(self.value(data))
        } else {
            Some(self.value(buf))
        }
    }

    /// Scale `raw` (exactly `len` bytes, in the register's byte order) to its physical value.
    pub fn value(&self, raw: &[u8]) -> f32 {
        let mut u: u64 = 0;
        if self.big_endian {
            for &b in raw {
                u = (u << 8) | b as u64;
            }
        } else {
            for &b in raw.iter().rev() {
                u = (u << 8) | b as u64;
            }
        }
        let bits = self.len as u32 * 8;
        let signed_val = if self.signed && bits < 64 && (u >> (bits - 1)) & 1 == 1 {
            (u as i64) - (1i64 << bits)
        } else {
            u as i64
        };
        signed_val as f32 * self.mult + self.off
    }
}

/// A full sensor description: address, one-time init writes, and the fields to read each cycle.
#[derive(Clone, Debug, PartialEq)]
pub struct Descriptor {
    pub name: String,
    pub addr: u8,
    /// Register writes applied once after the chip is found (e.g. an INA226 calibration).
    pub init: Vec<(u8, Vec<u8>)>,
    pub fields: Vec<Field>,
}

impl Descriptor {
    /// Parse one descriptor line (the text after `sensor add`).
    pub fn parse(line: &str) -> Result<Self, ParseError> {
        let line = line.trim();
        if line.is_empty() {
            return Err(ParseError::Empty);
        }
        if line.len() > MAX_LEN {
            return Err(ParseError::TooLong);
        }
        let mut tokens = line.split_whitespace();
        let name = tokens.next().ok_or(ParseError::Empty)?;
        let addr = u8::from_str_radix(tokens.next().ok_or(ParseError::BadAddress)?, 16).map_err(|_| ParseError::BadAddress)?;

        let mut init = Vec::new();
        let mut fields = Vec::new();
        for tok in tokens {
            if let Some(rest) = tok.strip_prefix("init=") {
                parse_init(rest, &mut init)?;
            } else {
                fields.push(parse_field(tok)?);
            }
        }
        if fields.is_empty() {
            return Err(ParseError::NoFields);
        }
        Ok(Self { name: String::from(name), addr, init, fields })
    }
}

fn parse_init(rest: &str, out: &mut Vec<(u8, Vec<u8>)>) -> Result<(), ParseError> {
    for pair in rest.split(',') {
        let (reg, hex) = pair.split_once(':').ok_or(ParseError::BadInit)?;
        let reg = u8::from_str_radix(reg, 16).map_err(|_| ParseError::BadInit)?;
        if hex.is_empty() || !hex.len().is_multiple_of(2) {
            return Err(ParseError::BadInit);
        }
        let mut bytes = Vec::new();
        let b = hex.as_bytes();
        for i in (0..b.len()).step_by(2) {
            let byte = u8::from_str_radix(core::str::from_utf8(&b[i..i + 2]).map_err(|_| ParseError::BadInit)?, 16)
                .map_err(|_| ParseError::BadInit)?;
            bytes.push(byte);
        }
        out.push((reg, bytes));
    }
    Ok(())
}

fn parse_field(tok: &str) -> Result<Field, ParseError> {
    let (lpp_name, rest) = tok.split_once('@').ok_or(ParseError::BadField)?;
    let lpp = lpp_type(lpp_name).ok_or(ParseError::BadLppType)?;
    let (reg_str, spec) = rest.split_once(':').ok_or(ParseError::BadField)?;
    let reg = u8::from_str_radix(reg_str, 16).map_err(|_| ParseError::BadRegister)?;

    // spec = <len><b|l><s|u>[*mul][+off]
    let bytes = spec.as_bytes();
    let len = bytes.first().and_then(|c| (*c as char).to_digit(10)).ok_or(ParseError::BadLength)? as u8;
    if !(1..=4).contains(&len) {
        return Err(ParseError::BadLength);
    }
    let big_endian = match bytes.get(1) {
        Some(b'b') => true,
        Some(b'l') => false,
        _ => return Err(ParseError::BadField),
    };
    let signed = match bytes.get(2) {
        Some(b's') => true,
        Some(b'u') => false,
        _ => return Err(ParseError::BadField),
    };
    // optional `p`: an SMBus PEC byte trails the data
    let (pec, scale_at) = match bytes.get(3) {
        Some(b'p') => (true, 4),
        _ => (false, 3),
    };

    // optional *mul and +off, in that order
    let tail = &spec[scale_at.min(spec.len())..];
    let (mult, off) = parse_scale(tail)?;
    Ok(Field { lpp, reg, len, big_endian, signed, pec, mult, off })
}

fn parse_scale(mut tail: &str) -> Result<(f32, f32), ParseError> {
    let mut mult = 1.0;
    let mut off = 0.0;
    if let Some(after) = tail.strip_prefix('*') {
        let end = after.find('+').unwrap_or(after.len());
        mult = parse_f32(&after[..end]).ok_or(ParseError::BadNumber)?;
        tail = &after[end..];
    }
    if let Some(after) = tail.strip_prefix('+') {
        off = parse_f32(after).ok_or(ParseError::BadNumber)?;
    } else if !tail.is_empty() {
        return Err(ParseError::BadField);
    }
    Ok((mult, off))
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}@{:02x}:{}{}{}",
            lpp_name(self.lpp),
            self.reg,
            self.len,
            if self.big_endian { 'b' } else { 'l' },
            if self.signed { 's' } else { 'u' },
        )?;
        if self.pec {
            f.write_str("p")?;
        }
        if self.mult != 1.0 {
            write!(f, "*{}", self.mult)?;
        }
        if self.off != 0.0 {
            write!(f, "+{}", self.off)?;
        }
        Ok(())
    }
}

impl fmt::Display for Descriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:02x}", self.name, self.addr)?;
        if !self.init.is_empty() {
            f.write_str(" init=")?;
            for (i, (reg, bytes)) in self.init.iter().enumerate() {
                write!(f, "{}{reg:02x}:", if i > 0 { "," } else { "" })?;
                for b in bytes {
                    write!(f, "{b:02x}")?;
                }
            }
        }
        for field in &self.fields {
            write!(f, " {field}")?;
        }
        Ok(())
    }
}

fn lpp_name(t: u8) -> &'static str {
    match t {
        lpp::VOLTAGE => "volt",
        lpp::CURRENT => "cur",
        lpp::POWER => "pow",
        lpp::TEMPERATURE => "temp",
        lpp::RELATIVE_HUMIDITY => "hum",
        lpp::BAROMETRIC_PRESSURE => "pres",
        lpp::ALTITUDE => "alt",
        _ => "?",
    }
}

fn lpp_type(name: &str) -> Option<u8> {
    Some(match name {
        "volt" => lpp::VOLTAGE,
        "cur" => lpp::CURRENT,
        "pow" => lpp::POWER,
        "temp" => lpp::TEMPERATURE,
        "hum" => lpp::RELATIVE_HUMIDITY,
        "pres" => lpp::BAROMETRIC_PRESSURE,
        "alt" => lpp::ALTITUDE,
        _ => return None,
    })
}

/// SMBus Packet Error Code: CRC-8 (poly `0x07`, init `0x00`) over the read-word frame the master
/// and slave both see — `[addr<<1, reg, addr<<1|1, data…]` — as MLX90614 and other SMBus parts
/// append after the data bytes.
fn smbus_pec(addr: u8, reg: u8, data: &[u8]) -> u8 {
    let header = [addr << 1, reg, (addr << 1) | 1];
    let mut crc: u8 = 0;
    for &b in header.iter().chain(data) {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    crc
}

/// A small decimal parser (no exponent): `[-+]?digits[.digits]`. Kept local so the crate needs no
/// float-from-str, and so descriptors round the same way on every target.
fn parse_f32(s: &str) -> Option<f32> {
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut parts = s.splitn(2, '.');
    let int_str = parts.next().unwrap_or("");
    let frac_str = parts.next();
    let mut seen = false;
    let mut int_part = 0.0f32;
    for c in int_str.chars() {
        int_part = int_part * 10.0 + c.to_digit(10)? as f32;
        seen = true;
    }
    let mut frac = 0.0f32;
    let mut scale = 1.0f32;
    if let Some(fs) = frac_str {
        for c in fs.chars() {
            frac = frac * 10.0 + c.to_digit(10)? as f32;
            scale *= 10.0;
            seen = true;
        }
    }
    if !seen {
        return None;
    }
    let v = int_part + frac / scale;
    Some(if neg { -v } else { v })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ina260() {
        let d = Descriptor::parse("ina260 41 volt@02:2bu*0.00125 cur@01:2bs*0.00125 pow@03:2bu*0.01").unwrap();
        assert_eq!(d.name, "ina260");
        assert_eq!(d.addr, 0x41);
        assert!(d.init.is_empty());
        assert_eq!(d.fields.len(), 3);
        let v = &d.fields[0];
        assert_eq!((v.lpp, v.reg, v.len, v.big_endian, v.signed), (lpp::VOLTAGE, 0x02, 2, true, false));
        assert!((v.mult - 0.00125).abs() < 1e-9 && v.off == 0.0);
        assert!(d.fields[1].signed && d.fields[1].lpp == lpp::CURRENT && d.fields[1].reg == 0x01);
        assert_eq!(d.fields[2].lpp, lpp::POWER);
    }

    #[test]
    fn ina260_scales_registers() {
        let d = Descriptor::parse("ina260 41 volt@02:2bu*0.00125 cur@01:2bs*0.00125 pow@03:2bu*0.01").unwrap();
        // 0x1000 = 4096 * 1.25 mV = 5.12 V
        assert!((d.fields[0].value(&[0x10, 0x00]) - 5.12).abs() < 1e-4);
        // current signed, 0xFF38 = -200 * 1.25 mA = -0.25 A
        assert!((d.fields[1].value(&[0xFF, 0x38]) + 0.25).abs() < 1e-4);
        // power 0x0100 = 256 * 10 mW = 2.56 W
        assert!((d.fields[2].value(&[0x01, 0x00]) - 2.56).abs() < 1e-4);
    }

    #[test]
    fn little_endian_and_offset() {
        // raw little-endian 0x0100 -> 1, *0.02 + -273.15 (MLX-style temperature offset)
        let f = parse_field("temp@07:2lu*0.02+-273.15").unwrap();
        assert!(!f.big_endian);
        assert!((f.value(&[0x01, 0x00]) - (-273.13)).abs() < 1e-3);
    }

    #[test]
    fn pec_field_checks_the_crc() {
        // MLX90614 object temp: 2 LE bytes + a PEC byte, raw·0.02 K − 273.15 -> °C.
        let f = parse_field("temp@07:2lup*0.02+-273.15").unwrap();
        assert!(f.pec && f.read_len() == 3);
        // raw 0x3A99 (15001) -> 300.02 K -> 26.87 °C; append the PEC the sensor would (addr 0x5a).
        let pec = smbus_pec(0x5a, 0x07, &[0x99, 0x3A]);
        let v = f.decode(0x5a, &[0x99, 0x3A, pec]).expect("valid pec");
        assert!((v - 26.87).abs() < 0.01, "v {v}");
        // a wrong PEC drops the reading
        assert!(f.decode(0x5a, &[0x99, 0x3A, pec ^ 0xFF]).is_none());
    }

    #[test]
    fn non_pec_field_never_drops() {
        let f = parse_field("volt@02:2bu*0.00125").unwrap();
        assert!(!f.pec && f.read_len() == 2);
        assert_eq!(f.decode(0x41, &[0x10, 0x00]), Some(f.value(&[0x10, 0x00])));
    }

    #[test]
    fn init_writes_parsed() {
        let d = Descriptor::parse("ina226 44 init=05:0800,00:4127 volt@02:2bu*0.00125").unwrap();
        assert_eq!(d.init, alloc::vec![(0x05u8, alloc::vec![0x08, 0x00]), (0x00u8, alloc::vec![0x41, 0x27])]);
        assert_eq!(d.fields.len(), 1);
    }

    #[test]
    fn emits_lpp_like_a_driver_would() {
        let d = Descriptor::parse("ina260 41 volt@02:2bu*0.00125 pow@03:2bu*0.01").unwrap();
        let reads: [&[u8]; 2] = [&[0x10, 0x00], &[0x01, 0x00]];
        let mut out = Vec::new();
        for (f, raw) in d.fields.iter().zip(reads) {
            lpp::push(&mut out, 2, f.lpp, f.value(raw));
        }
        // volt 5.12 -> 512 (0x0200), pow 2.56 -> 2 (cut toward zero, 0x0002)
        assert_eq!(out, [2, lpp::VOLTAGE, 0x02, 0x00, 2, lpp::POWER, 0x00, 0x02]);
    }

    #[test]
    fn display_round_trips() {
        for line in [
            "ina260 41 volt@02:2bu*0.00125 cur@01:2bs*0.00125 pow@03:2bu*0.01",
            "ina226 44 init=05:0800,00:4127 volt@02:2bu*0.00125",
            "mlx 5a temp@07:2lu*0.02+-273.15",
            "mlx 5a temp@07:2lup*0.02+-273.15",
        ] {
            let d = Descriptor::parse(line).unwrap();
            let s1 = alloc::format!("{d}");
            let s2 = alloc::format!("{}", Descriptor::parse(&s1).unwrap());
            assert_eq!(s1, s2, "unstable round-trip for {line}");
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(Descriptor::parse(""), Err(ParseError::Empty));
        assert_eq!(Descriptor::parse("name"), Err(ParseError::BadAddress));
        assert_eq!(Descriptor::parse("name zz volt@02:2bu"), Err(ParseError::BadAddress));
        assert_eq!(Descriptor::parse("name 41"), Err(ParseError::NoFields));
        assert_eq!(Descriptor::parse("name 41 bad@02:2bu"), Err(ParseError::BadLppType));
        assert_eq!(Descriptor::parse("name 41 volt@02:9bu"), Err(ParseError::BadLength));
        assert_eq!(Descriptor::parse("name 41 volt@02:2xu"), Err(ParseError::BadField));
    }
}
