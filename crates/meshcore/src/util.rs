//! Small helpers shared by the CLI and protocol code.

use alloc::string::String;
use core::fmt::Write;

/// Length of the longest valid UTF-8 prefix of `text` that fits in `max_bytes`
/// (`mesh::validUtf8PrefixLength`).
pub fn valid_utf8_prefix_len(text: &[u8], max_bytes: usize) -> usize {
    let mut offset = 0;
    while offset < text.len() && text[offset] != 0 {
        let first = text[offset];
        let seq = match first {
            0x00..=0x7F => 1,
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => break,
        };
        if offset + seq > max_bytes || offset + seq > text.len() {
            break;
        }
        if !text[offset + 1..offset + seq].iter().all(|&b| b & 0xC0 == 0x80) {
            break;
        }
        if seq == 3 {
            let second = text[offset + 1];
            if (first == 0xE0 && second < 0xA0) || (first == 0xED && second > 0x9F) {
                break;
            }
        } else if seq == 4 {
            let second = text[offset + 1];
            if (first == 0xF0 && second < 0x90) || (first == 0xF4 && second > 0x8F) {
                break;
            }
        }
        offset += seq;
    }
    offset
}

/// Upper-case hex, as upstream `Utils::toHex`.
/// The start of a public key, as people type it: 8 to 64 hex digits, an even count.
pub fn parse_key_prefix(s: &str) -> Option<alloc::vec::Vec<u8>> {
    if !(8..=64).contains(&s.len()) || s.len() % 2 != 0 {
        return None;
    }
    let mut key = alloc::vec![0u8; s.len() / 2];
    from_hex(&mut key, s).then_some(key)
}

pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02X}");
    }
    s
}

/// Decode hex into `dest`; length must match exactly.
pub fn from_hex(dest: &mut [u8], hex: &str) -> bool {
    let hex = hex.as_bytes();
    if hex.len() != dest.len() * 2 {
        return false;
    }
    fn val(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    for (i, d) in dest.iter_mut().enumerate() {
        match (val(hex[i * 2]), val(hex[i * 2 + 1])) {
            (Some(h), Some(l)) => *d = (h << 4) | l,
            _ => return false,
        }
    }
    true
}

/// Leading-digits integer parse (upstream `_atoi`: stops at first non-digit, never fails).
pub fn atoi(s: &str) -> u32 {
    let mut n: u32 = 0;
    for c in s.bytes() {
        if !c.is_ascii_digit() {
            break;
        }
        n = n.wrapping_mul(10).wrapping_add((c - b'0') as u32);
    }
    n
}

/// Signed leading-integer parse (C `atoi`).
pub fn atoi_signed(s: &str) -> i32 {
    let s = s.trim_start();
    let (neg, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let v = atoi(rest) as i32;
    if neg { -v } else { v }
}

/// Leading float parse (C `atof`): parses the longest numeric prefix, `0.0` if none.
pub fn atof(s: &str) -> f64 {
    let s = s.trim_start();
    let bytes = s.as_bytes();
    let mut end = 0;
    let mut seen_dot = false;
    let mut seen_digit = false;
    if matches!(bytes.first(), Some(b'-' | b'+')) {
        end = 1;
    }
    while end < bytes.len() {
        match bytes[end] {
            b'0'..=b'9' => seen_digit = true,
            b'.' if !seen_dot => seen_dot = true,
            _ => break,
        }
        end += 1;
    }
    if !seen_digit {
        return 0.0;
    }
    // The digits as one integer over a power of ten, divided exactly: as `str::parse` rounds, up
    // to 19 digits. core's float parser is 14 KB of tables and code on the firmware.
    let (mut mantissa, mut exponent) = (0u64, 0i32);
    let mut in_fraction = false;
    for &b in &bytes[..end] {
        match b {
            b'.' => in_fraction = true,
            b'0'..=b'9' if mantissa <= (u64::MAX - 9) / 10 => {
                mantissa = mantissa * 10 + (b - b'0') as u64;
                exponent -= in_fraction as i32;
            }
            // digits past what a u64 takes: beyond what an f64 holds
            b'0'..=b'9' => exponent += !in_fraction as i32,
            _ => {}
        }
    }
    let v = match exponent {
        -18..=0 => ratio(mantissa, 10u64.pow(exponent.unsigned_abs())),
        // longer than anyone types: pure integer scaling so Cortex-M never pulls soft-f64
        _ => scale_pow10(mantissa, exponent),
    };
    if bytes[0] == b'-' { f64::from_bits(v.to_bits() | (1u64 << 63)) } else { v }
}

/// Pack positive `(q + eps) * 2^exp2_lsb` (`eps > 0` iff `sticky`) into an IEEE-754 `f64`
/// (ties to even), using pure integer ops so soft-float `__muldf3`/`__divdf3` are never linked.
fn pack_f64(q: u128, sticky: bool, exp2_lsb: i32) -> f64 {
    if q == 0 {
        return 0.0;
    }
    let bits = 128 - q.leading_zeros();
    let (mut sig, e) = if bits > 53 {
        let shift = bits - 53;
        let mut s = (q >> shift) as u64;
        let half = 1u128 << (shift - 1);
        let rem = q & ((1u128 << shift) - 1);
        if rem > half || (rem == half && (sticky || (s & 1) == 1)) {
            s += 1;
        }
        let mut e = exp2_lsb + shift as i32 + 52;
        if s == (1u64 << 53) {
            s >>= 1;
            e += 1;
        }
        (s, e)
    } else {
        let shift = 53 - bits;
        ((q as u64) << shift, exp2_lsb - shift as i32 + 52)
    };
    let biased = e + 1023;
    if biased <= 0 {
        let sub = (1 - biased) as u32;
        if sub >= 54 {
            return 0.0;
        }
        sig >>= sub;
        return f64::from_bits(sig);
    }
    if biased >= 2047 {
        return f64::INFINITY;
    }
    f64::from_bits(((biased as u64) << 52) | (sig & ((1u64 << 52) - 1)))
}

fn scale_pow10(mantissa: u64, mut exp10: i32) -> f64 {
    if mantissa == 0 {
        return 0.0;
    }
    let mut val = mantissa as u128;
    let mut exp2 = 0i32;
    let mut sticky = false;
    while exp10 > 0 {
        if val >= (1u128 << 124) {
            sticky |= (val & 0xF) != 0;
            val >>= 4;
            exp2 += 4;
        }
        val *= 10;
        exp10 -= 1;
        if exp2 > 1100 {
            return f64::INFINITY;
        }
    }
    while exp10 < 0 {
        let free = val.leading_zeros();
        if free > 0 {
            val <<= free;
            exp2 -= free as i32;
        }
        let rem = val % 10;
        sticky |= rem != 0;
        val /= 10;
        exp10 += 1;
        if exp2 < -1200 {
            return 0.0;
        }
    }
    pack_f64(val, sticky, exp2)
}

/// `n / unit` as the nearest f64 (ties to even), for a unit up to 10^18: a long division in
/// integers, rounded once.
fn ratio(n: u64, unit: u64) -> f64 {
    if n == 0 {
        return 0.0;
    }
    // n at the top of 128 bits, so the quotient has more bits than an f64 keeps
    let lift = n.leading_zeros();
    let dividend = ((n << lift) as u128) << 64;
    let (mut quotient, mut rest) = (0u128, 0u128);
    for bit in (0..128).rev() {
        rest = rest << 1 | (dividend >> bit) & 1;
        quotient <<= 1;
        if rest >= unit as u128 {
            rest -= unit as u128;
            quotient |= 1;
        }
    }
    pack_f64(quotient, rest != 0, -64 - lift as i32)
}

/// `v / 10^exp10` as an exact `f64` without soft-float `__divdf3` (for GPS microdegrees / mm).
pub fn i32_div_10n(v: i32, exp10: u32) -> f64 {
    let mag = ratio(v.unsigned_abs() as u64, 10u64.pow(exp10));
    if v < 0 { f64::from_bits(mag.to_bits() | (1u64 << 63)) } else { mag }
}

/// `(|v| * 10^exp10)` rounded half-to-even (matches IEEE-754 `f64` multiplication on decimal
/// coordinates like `37.6175 * 1e6` without pulling soft-float `__muldf3`).
pub fn f64_mul_10n_u64(v: f64, exp10: u32) -> u64 {
    if !v.is_finite() {
        return 0;
    }
    in_units(v, exp10 as usize).0
}

/// `(v * 10^exp10)` rounded half-to-even and clamped to `i32`, without soft-float `__muldf3`.
pub fn f64_mul_10n_i32(v: f64, exp10: u32) -> i32 {
    let neg = (v.to_bits() >> 63) != 0;
    let mag = f64_mul_10n_u64(v, exp10);
    if neg {
        -(mag.min(i32::MAX as u64 + 1) as i64) as i32
    } else {
        mag.min(i32::MAX as u64) as i32
    }
}

/// Convert `f32` to `f64` using pure integer bit manipulation (avoids `__aeabi_f2d`).
#[inline]
pub fn f32_to_f64(v: f32) -> f64 {
    let b = v.to_bits();
    let sign = ((b >> 31) as u64) << 63;
    let exp = (b >> 23) & 0xFF;
    let frac = b & 0x7F_FFFF;
    if exp == 0 {
        if frac == 0 {
            return f64::from_bits(sign);
        }
        let shift = frac.leading_zeros() - 8;
        let norm_frac = ((frac << shift) & 0x7F_FFFF) as u64;
        let new_exp = (1023 - 126 - shift) as u64;
        return f64::from_bits(sign | (new_exp << 52) | (norm_frac << 29));
    }
    if exp == 0xFF {
        return f64::from_bits(sign | (0x7FFu64 << 52) | ((frac as u64) << 29));
    }
    let new_exp = (exp as u64) + (1023 - 127);
    f64::from_bits(sign | (new_exp << 52) | ((frac as u64) << 29))
}

/// Convert `f64` to `f32` (nearest, ties to even) using pure integer bit ops (avoids `__aeabi_d2f`).
pub fn f64_to_f32(v: f64) -> f32 {
    let b = v.to_bits();
    let sign = ((b >> 63) as u32) << 31;
    let exp = ((b >> 52) & 0x7FF) as i32;
    let frac = b & ((1u64 << 52) - 1);
    if exp == 0 {
        return f32::from_bits(sign);
    }
    if exp == 0x7FF {
        let f = if frac != 0 { 0x40_0000 } else { 0 };
        return f32::from_bits(sign | (0xFF << 23) | f);
    }
    let mut new_exp = exp - 1023 + 127;
    if new_exp >= 255 {
        return f32::from_bits(sign | (0xFF << 23));
    }
    if new_exp <= 0 {
        let shift = (1 - new_exp) as u32 + 29;
        if shift >= 55 {
            return f32::from_bits(sign);
        }
        let m = frac | (1u64 << 52);
        let q = (m >> shift) as u32;
        let half = 1u64 << (shift - 1);
        let rem = m & ((1u64 << shift) - 1);
        let r = q + (rem > half || (rem == half && (q & 1) == 1)) as u32;
        return f32::from_bits(sign | r);
    }
    let mut q = (frac >> 29) as u32;
    let rem = frac & ((1u64 << 29) - 1);
    let half = 1u64 << 28;
    if rem > half || (rem == half && (q & 1) == 1) {
        q += 1;
        if q == (1 << 23) {
            q = 0;
            new_exp += 1;
            if new_exp >= 255 {
                return f32::from_bits(sign | (0xFF << 23));
            }
        }
    }
    f32::from_bits(sign | ((new_exp as u32) << 23) | q)
}

/// A float with a fixed number of decimals, as `{:.N}` prints it (half to even), without core's
/// float formatting: 9 KB of code and tables on the firmware.
pub struct Fixed(pub f64, pub usize);

impl core::fmt::Display for Fixed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Fixed(v, decimals) = *self;
        if v.is_nan() {
            return f.write_str("NaN");
        }
        if v.is_sign_negative() {
            f.write_str("-")?;
        }
        if v.is_infinite() {
            return f.write_str("inf");
        }
        let decimals = decimals.min(MAX_DECIMALS);
        let (n, unit) = in_units(v, decimals);
        write!(f, "{}", n / unit)?;
        if decimals > 0 {
            write!(f, ".{:0decimals$}", n % unit)?;
        }
        Ok(())
    }
}

/// The most decimals [`Fixed`] prints: their unit still fits a `u64`.
const MAX_DECIMALS: usize = 18;

/// `|v|` as a whole number of units of its last decimal (rounded half to even, at most `u64::MAX`),
/// and how many of those units make one.
fn in_units(v: f64, decimals: usize) -> (u64, u64) {
    let unit = 10u64.pow(decimals as u32);
    // v = mantissa * 2^exponent, so in units it is `scaled` shifted: exact integer arithmetic,
    // rounded once
    let bits = v.to_bits();
    let (fraction, biased) = (bits & ((1 << 52) - 1), (bits >> 52) as i32 & 0x7FF);
    let (mantissa, exponent) = if biased == 0 { (fraction, -1074) } else { (fraction | 1 << 52, biased - 1075) };
    let scaled = mantissa as u128 * unit as u128; // below 2^113
    let n = match exponent {
        0..15 => scaled << exponent,
        15.. => u128::MAX,
        -127..0 => {
            let (whole, rest) = (scaled >> -exponent, scaled & ((1 << -exponent) - 1));
            let half = 1u128 << (-exponent - 1);
            whole + (rest > half || (rest == half && whole % 2 == 1)) as u128
        }
        _ => 0,
    };
    (n.min(u64::MAX as u128) as u64, unit)
}

/// A float in the fewest decimals that [`atof`] reads back as the same value, as `{}` prints it:
/// what goes into the stored settings.
pub struct Shortest<T>(pub T);

impl core::fmt::Display for Shortest<f64> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let abs_bits = self.0.to_bits() & !(1u64 << 63);
        let reads_back = |d: &usize| {
            let (n, unit) = in_units(self.0, *d);
            ratio(n, unit).to_bits() == abs_bits
        };
        Fixed(self.0, (0..MAX_DECIMALS).find(reads_back).unwrap_or(MAX_DECIMALS)).fmt(f)
    }
}

impl core::fmt::Display for Shortest<f32> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let v = f32_to_f64(self.0);
        let abs_bits = self.0.to_bits() & 0x7FFF_FFFF;
        let reads_back = |d: &usize| {
            let (n, unit) = in_units(v, *d);
            f64_to_f32(ratio(n, unit)).to_bits() == abs_bits
        };
        Fixed(v, (0..MAX_DECIMALS).find(reads_back).unwrap_or(MAX_DECIMALS)).fmt(f)
    }
}

/// Format a float like upstream `StrHelper::ftoa` (trailing zeros trimmed).
pub fn ftoa(v: f64, decimals: usize) -> String {
    let mut s = String::new();
    let _ = write!(s, "{}", Fixed(v, decimals));
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// Copy at most `max_len - 1` bytes of `s` (C buffer semantics), on a char boundary.
pub fn truncate_str(s: &str, max_len: usize) -> String {
    let n = valid_utf8_prefix_len(s.as_bytes(), max_len.saturating_sub(1));
    String::from(&s[..n])
}

/// The link the MeshCore app adds a contact from (`docs/qr_codes.md`): its name, public key and
/// type (`ADV_TYPE_*`).
pub fn contact_link(name: &str, pub_key: &[u8], adv_type: u8) -> String {
    let mut s = String::from("meshcore://contact/add?name=");
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => s.push(b as char),
            b' ' => s.push('+'),
            _ => {
                let _ = core::fmt::Write::write_fmt(&mut s, format_args!("%{b:02X}"));
            }
        }
    }
    s.push_str("&public_key=");
    s.push_str(&to_hex(pub_key).to_ascii_lowercase());
    let _ = core::fmt::Write::write_fmt(&mut s, format_args!("&type={adv_type}"));
    s
}

/// [`contact_link`] at most `max_len` bytes long (what a small screen's QR code holds), the name
/// shortened by whole characters to fit.
pub fn contact_link_within(name: &str, pub_key: &[u8], adv_type: u8, max_len: usize) -> String {
    let mut name = name;
    loop {
        let link = contact_link(name, pub_key, adv_type);
        if link.len() <= max_len || name.is_empty() {
            return link;
        }
        name = &name[..name.char_indices().last().map_or(0, |(i, _)| i)];
    }
}

/// Absolute value of an `f32` without `libm` (clears the sign bit: one `vabs.f32` on Cortex-M4F).
#[inline]
pub fn fabsf(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & 0x7FFF_FFFF)
}

/// `base.powf(exp)` in ~150 bytes of hardware-FPU `f32` arithmetic (instead of `libm::powf`'s ~2 KiB),
/// accurate to ~1e-6 relative error for `base > 0` (barometric altitude and RX score delay).
pub fn powf(base: f32, exp: f32) -> f32 {
    if exp == 0.0 || base == 1.0 {
        return 1.0;
    }
    if !base.is_finite() || base <= 0.0 || !exp.is_finite() {
        return 0.0;
    }
    // base = m * 2^e with m in [1, 2); log2(m) via atanh series in t = (m - 1) / (m + 1) in [0, 1/3)
    let bits = base.to_bits();
    let e = ((bits >> 23) as i32) - 127;
    let m = f32::from_bits((bits & 0x007F_FFFF) | 0x3F80_0000);
    let t = (m - 1.0) / (m + 1.0);
    let t2 = t * t;
    let ln_m = 2.0 * t * (1.0 + t2 * (1.0 / 3.0 + t2 * (1.0 / 5.0 + t2 * (1.0 / 7.0 + t2 * (1.0 / 9.0)))));
    let log2_base = e as f32 + ln_m * core::f32::consts::LOG2_E;

    // 2^z = 2^k * e^(f * ln2) with f in [-0.5, 0.5]
    let z = exp * log2_base;
    if z < -126.0 {
        return 0.0;
    }
    if z > 127.0 {
        return f32::INFINITY;
    }
    let k = if z >= 0.0 { (z + 0.5) as i32 } else { (z - 0.5) as i32 };
    let u = (z - k as f32) * core::f32::consts::LN_2;
    let exp_u = 1.0 + u * (1.0 + u * (0.5 + u * (1.0 / 6.0 + u * (1.0 / 24.0 + u * (1.0 / 120.0 + u / 720.0)))));
    f32::from_bits(((k + 127) as u32) << 23) * exp_u
}

/// Days-from-civil inverse: epoch seconds to (year, month, day, hour, minute, second), UTC.
pub fn epoch_to_datetime(t: u32) -> (i32, u32, u32, u32, u32, u32) {
    let days = (t / 86400) as i64;
    let secs = t % 86400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d, secs / 3600, (secs / 60) % 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contact_links_as_the_app_reads_them() {
        let key = [0x9c; 32];
        assert_eq!(
            contact_link("Example Contact", &key, 1),
            alloc::format!("meshcore://contact/add?name=Example+Contact&public_key={}&type=1", "9c".repeat(32))
        );
        assert!(contact_link("Ёлка 🎃", &key, 2).starts_with("meshcore://contact/add?name=%D0%81%D0%BB%D0%BA%D0%B0+%F0%9F%8E%83&"));
        let short = contact_link_within("Ёлка 🎃🎃🎃🎃🎃", &key, 2, 154);
        assert!(short.len() <= 154);
        assert!(short.starts_with("meshcore://contact/add?name=%D0%81%D0%BB%D0%BA%D0%B0+%F0%9F%8E%83&"), "one pumpkin left");
    }

    #[test]
    fn parsing_like_c() {
        assert_eq!(atoi("123abc"), 123);
        assert_eq!(atoi("x"), 0);
        assert_eq!(atoi_signed("-9"), -9);
        assert_eq!(atof("869.618"), 869.618);
        assert_eq!(atof("62.5,8"), 62.5);
        assert_eq!(atof("abc"), 0.0);
        assert_eq!(ftoa(869.618, 3), "869.618");
        assert_eq!(ftoa(62.5, 3), "62.5");
        assert_eq!(ftoa(10.0, 3), "10");
    }

    /// `atof` and [`Fixed`] against core's float parsing and formatting, which they stand in for.
    #[test]
    fn floats_as_core_does_them() {
        for s in ["0", "-0.5", "+3", "869.525", "433.92", "55.751244", "-37.618423", "0.000001", "62.5", ".5", "7."] {
            assert_eq!(atof(s), s.parse::<f64>().unwrap(), "{s}");
        }
        // more digits than an f64 holds: as close as two roundings get
        for s in ["123456789012345678901234567890", "0.1234567890123456789012345"] {
            let (v, exact) = (atof(s), s.parse::<f64>().unwrap());
            assert!((v - exact).abs() <= exact * 1e-15, "{s}");
        }
        assert_eq!(atof("-"), 0.0);

        // a walk over values of every size, as an f32 (most of the firmware's) and as an f64
        let (mut x, mut ties) = (0x2545_F491_4F6C_DD1Du64, 0);
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let v = (x >> 40) as f64 / 1000.0 - 8000.0;
            for d in 0..=6 {
                assert_eq!(alloc::format!("{}", Fixed(v, d)), alloc::format!("{v:.d$}"), "{v} to {d}");
                let w = v as f32;
                assert_eq!(alloc::format!("{}", Fixed(w as f64, d)), alloc::format!("{w:.d$}"), "{w} to {d}");
            }
            // The stored form: as short as core's and read back as the same value. The digits are
            // core's too, but for a value halfway between two of them (core takes the upper).
            let w = v as f32;
            for u in [v, v / 7.0, (x >> 40) as f64 / 1000.0] {
                let (ours, cores) = (alloc::format!("{}", Shortest(u)), alloc::format!("{u}"));
                assert_eq!(ours.len(), cores.len(), "{ours} for {cores}");
                assert_eq!(atof(&ours), u, "{ours}");
                ties += (ours != cores) as u32;
            }
            for u in [w, w / 7.0] {
                let (ours, cores) = (alloc::format!("{}", Shortest(u)), alloc::format!("{u}"));
                assert_eq!(ours.len(), cores.len(), "{ours} for {cores}");
                assert_eq!(atof(&ours) as f32, u, "{ours}");
                ties += (ours != cores) as u32;
            }
        }
        assert!(ties < 20, "{ties} of 100000 differ from core");
        assert_eq!(alloc::format!("{}", Shortest(0.0f32)), "0");
        assert_eq!(alloc::format!("{}", Shortest(f32::NAN)), "NaN");
        for (v, d, s) in [(0.125, 2, "0.12"), (0.375, 2, "0.38"), (2.5, 0, "2"), (-0.04, 1, "-0.0"), (99.96, 1, "100.0")] {
            assert_eq!(alloc::format!("{}", Fixed(v, d)), s);
            assert_eq!(alloc::format!("{v:.d$}"), s);
        }
        assert_eq!(alloc::format!("{}", Fixed(f64::NAN, 1)), "NaN");
        assert_eq!(alloc::format!("{}", Fixed(f64::NEG_INFINITY, 1)), "-inf");
    }

    #[test]
    fn hex_roundtrip() {
        let mut b = [0u8; 3];
        assert!(from_hex(&mut b, "0aFf10"));
        assert_eq!(b, [0x0A, 0xFF, 0x10]);
        assert_eq!(to_hex(&b), "0AFF10");
        assert!(!from_hex(&mut b, "0aFf1"));
    }

    #[test]
    fn datetime() {
        assert_eq!(epoch_to_datetime(1715770351), (2024, 5, 15, 10, 52, 31));
        assert_eq!(epoch_to_datetime(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn powf_matches_std() {
        for base in [0.25f32, 0.5, 0.9, 1.0, 1.01325, 2.0, 5.0, 10.0, 20.0] {
            for exp in [-0.5f32, -0.15, 0.0, 0.1903, 0.5, 0.85, 1.5] {
                let got = powf(base, exp);
                let want = base.powf(exp);
                assert!((got - want).abs() <= want.abs() * 1e-5 + 1e-6, "powf({base}, {exp}): got={got} want={want}");
            }
        }
        assert_eq!(powf(0.0, 0.5), 0.0);
        assert_eq!(powf(0.0, 0.0), 1.0);
        assert_eq!(powf(f32::NAN, 0.5), 0.0);
    }

    #[test]
    fn float_helpers_match_hardware_casts() {
        for v in [0.0f32, -0.0, 1.0, -1.0, 869.525, 62.5, f32::MIN_POSITIVE, f32::from_bits(1), f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(f32_to_f64(v).to_bits(), (v as f64).to_bits(), "f32_to_f64({v})");
        }
        assert!(f32_to_f64(f32::NAN).is_nan());
        for v in [
            0.0f64,
            -0.0,
            1.0,
            -1.0,
            55.751244,
            -37.618423,
            1e-40,
            1e-45,
            1e40,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            assert_eq!(f64_to_f32(v).to_bits(), (v as f32).to_bits(), "f64_to_f32({v})");
        }
        assert!(f64_to_f32(f64::NAN).is_nan());
        assert_eq!(i32_div_10n(55_751_244, 6), 55.751244);
        assert_eq!(i32_div_10n(-37_618_423, 6), -37.618423);
        assert_eq!(f64_mul_10n_i32(37.6175, 6), 37_617_500);
        assert_eq!(f64_mul_10n_i32(-37.6175, 6), -37_617_500);
    }
}
