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
    s[..end].parse::<f64>().unwrap_or(0.0)
}

/// Format a float like upstream `StrHelper::ftoa` (trailing zeros trimmed).
pub fn ftoa(v: f64, decimals: usize) -> String {
    let mut s = String::new();
    let _ = write!(s, "{v:.decimals$}");
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
}
