//! A small QR code encoder for the contact link: byte mode, error correction level L, versions 1
//! to 10, mask 0. qrcodegen also switched modes, raised the error correction where it fitted and
//! scored all eight masks; any of those choices scans, and this takes a fraction of its code
//! (the T114's app slot has no room to spare). The tests check it module for module against
//! qrcodegen given the same choices.

use alloc::boxed::Box;

/// The largest code drawn: version 10, 57 modules (a contact link with a long name).
pub const MAX_VERSION: usize = 10;
const MAX_SIZE: usize = 17 + 4 * MAX_VERSION;
/// Codewords of the largest version.
const MAX_CODEWORDS: usize = 346;

/// Level L, versions 1 to 10: error correction codewords per block, and blocks.
const EC_PER_BLOCK: [u8; MAX_VERSION] = [7, 10, 15, 20, 26, 18, 20, 24, 30, 18];
const BLOCKS: [u8; MAX_VERSION] = [1, 1, 1, 1, 1, 2, 2, 2, 2, 4];

/// A code's modules, a row in each word (bit `x` dark), and which of them are function patterns.
pub struct Qr {
    size: usize,
    dark: [u64; MAX_SIZE],
    function: [u64; MAX_SIZE],
}

impl Qr {
    /// The code for `text`, if it fits in version 10.
    pub fn encode(text: &[u8]) -> Option<Box<Qr>> {
        let ver = (1..=MAX_VERSION).find(|&v| 4 + count_bits(v) + 8 * text.len() <= 8 * data_codewords(v))?;
        let mut qr = Box::new(Qr { size: 17 + 4 * ver, dark: [0; MAX_SIZE], function: [0; MAX_SIZE] });
        qr.function_patterns(ver);
        let mut codewords = [0u8; MAX_CODEWORDS];
        let n = codewords_of(text, ver, &mut codewords);
        qr.place(&codewords[..n]);
        qr.mask();
        qr.format_bits();
        Some(qr)
    }

    pub fn size(&self) -> i32 {
        self.size as i32
    }

    pub fn get(&self, x: i32, y: i32) -> bool {
        self.dark[y as usize] >> x & 1 != 0
    }

    fn set(&mut self, x: usize, y: usize, dark: bool) {
        if dark {
            self.dark[y] |= 1 << x;
        } else {
            self.dark[y] &= !(1 << x);
        }
        self.function[y] |= 1 << x;
    }

    fn function_patterns(&mut self, ver: usize) {
        let size = self.size;
        for i in 0..size {
            self.set(6, i, i % 2 == 0);
            self.set(i, 6, i % 2 == 0);
        }
        for (x, y) in [(3, 3), (size - 4, 3), (3, size - 4)] {
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (xx, yy) = (x as i32 + dx, y as i32 + dy);
                    if (0..size as i32).contains(&xx) && (0..size as i32).contains(&yy) {
                        let dist = dx.abs().max(dy.abs());
                        self.set(xx as usize, yy as usize, dist != 2 && dist != 4);
                    }
                }
            }
        }
        // alignment patterns, but where the finders are
        if ver > 1 {
            let n = ver / 7 + 2;
            let step = (ver * 4 + n * 2 + 1) / (n * 2 - 2) * 2;
            let pos = |i: usize| if i == 0 { 6 } else { size - 7 - (n - 1 - i) * step };
            for i in 0..n {
                for j in 0..n {
                    if (i == 0 && j == 0) || (i == 0 && j == n - 1) || (i == n - 1 && j == 0) {
                        continue;
                    }
                    for dy in -2i32..=2 {
                        for dx in -2i32..=2 {
                            let (x, y) = ((pos(i) as i32 + dx) as usize, (pos(j) as i32 + dy) as usize);
                            self.set(x, y, dx.abs().max(dy.abs()) != 1);
                        }
                    }
                }
            }
        }
        // the format bits' place, filled in last
        self.format_bits();
        if ver >= 7 {
            let mut rem = ver as u32;
            for _ in 0..12 {
                rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
            }
            let bits = (ver as u32) << 12 | rem;
            for i in 0..18 {
                let (a, b) = (size - 11 + i % 3, i / 3);
                self.set(a, b, bits >> i & 1 != 0);
                self.set(b, a, bits >> i & 1 != 0);
            }
        }
    }

    /// Level L (format bits 01), mask 0.
    fn format_bits(&mut self) {
        let size = self.size;
        let data = 1u32 << 3;
        let mut rem = data;
        for _ in 0..10 {
            rem = (rem << 1) ^ ((rem >> 9) * 0x537);
        }
        let bits = (data << 10 | rem) ^ 0x5412;
        let bit = |i: usize| bits >> i & 1 != 0;
        for i in 0..6 {
            self.set(8, i, bit(i));
        }
        self.set(8, 7, bit(6));
        self.set(8, 8, bit(7));
        self.set(7, 8, bit(8));
        for i in 9..15 {
            self.set(14 - i, 8, bit(i));
        }
        for i in 0..8 {
            self.set(size - 1 - i, 8, bit(i));
        }
        for i in 8..15 {
            self.set(8, size - 15 + i, bit(i));
        }
        self.set(8, size - 8, true);
    }

    /// The codewords in the zigzag of column pairs, right to left.
    fn place(&mut self, codewords: &[u8]) {
        let size = self.size;
        let mut i = 0;
        let mut right = size - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vert in 0..size {
                for j in 0..2 {
                    let x = right - j;
                    let y = if (right + 1) & 2 == 0 { size - 1 - vert } else { vert };
                    if self.function[y] >> x & 1 == 0 && i < codewords.len() * 8 {
                        if codewords[i >> 3] >> (7 - (i & 7)) & 1 != 0 {
                            self.dark[y] |= 1 << x;
                        }
                        i += 1;
                    }
                }
            }
            if right < 2 {
                break;
            }
            right -= 2;
        }
    }

    /// Mask 0: the data modules where `x + y` is even are flipped.
    fn mask(&mut self) {
        for y in 0..self.size {
            let even: u64 = if y % 2 == 0 { 0x5555_5555_5555_5555 } else { 0xAAAA_AAAA_AAAA_AAAA };
            self.dark[y] ^= even & !self.function[y] & ((1 << self.size) - 1);
        }
    }
}

/// Bits of the character count in byte mode.
fn count_bits(ver: usize) -> usize {
    if ver < 10 { 8 } else { 16 }
}

/// The version's codewords: its modules less the function patterns and the format and version
/// bits, in bytes.
fn raw_codewords(ver: usize) -> usize {
    let mut modules = (16 * ver + 128) * ver + 64;
    if ver >= 2 {
        let n = ver / 7 + 2;
        modules -= (25 * n - 10) * n - 55;
        if ver >= 7 {
            modules -= 36;
        }
    }
    modules / 8
}

fn data_codewords(ver: usize) -> usize {
    raw_codewords(ver) - EC_PER_BLOCK[ver - 1] as usize * BLOCKS[ver - 1] as usize
}

/// `text` as the version's codewords: the data padded out, split into blocks, each with its
/// error correction, interleaved. Returns how many.
fn codewords_of(text: &[u8], ver: usize, out: &mut [u8; MAX_CODEWORDS]) -> usize {
    let data_len = data_codewords(ver);
    let mut data = [0u8; MAX_CODEWORDS];
    let mut bit = 0;
    let mut put = |value: u32, len: usize| {
        for i in (0..len).rev() {
            if value >> i & 1 != 0 {
                data[bit >> 3] |= 0x80 >> (bit & 7);
            }
            bit += 1;
        }
    };
    put(0b0100, 4);
    put(text.len() as u32, count_bits(ver));
    for &b in text {
        put(b as u32, 8);
    }
    // the terminator (data is zeroed), to a whole byte, then the pad bytes
    let mut at = (bit + 4).min(8 * data_len).div_ceil(8);
    for pad in [0xEC, 0x11].into_iter().cycle() {
        if at >= data_len {
            break;
        }
        data[at] = pad;
        at += 1;
    }
    let (blocks, ec_len) = (BLOCKS[ver - 1] as usize, EC_PER_BLOCK[ver - 1] as usize);
    let raw = raw_codewords(ver);
    let short = blocks - raw % blocks;
    let short_len = raw / blocks;
    let divisor = rs_divisor(ec_len);
    let mut n = 0;
    // the data of block i, then its error correction
    let block = |i: usize| {
        let start = i * (short_len - ec_len) + i.saturating_sub(short);
        let len = short_len - ec_len + (i >= short) as usize;
        (start, len)
    };
    for i in 0..=short_len - ec_len {
        for b in 0..blocks {
            let (start, len) = block(b);
            if i < len {
                out[n] = data[start + i];
                n += 1;
            }
        }
    }
    for i in 0..ec_len {
        for b in 0..blocks {
            let (start, len) = block(b);
            out[n] = rs_remainder(&data[start..start + len], &divisor[..ec_len])[i];
            n += 1;
        }
    }
    n
}

/// The Reed-Solomon generator of degree `degree`, highest power first, the leading 1 left out.
fn rs_divisor(degree: usize) -> [u8; 30] {
    let mut result = [0u8; 30];
    result[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            result[j] = gf_mul(result[j], root);
            if j + 1 < degree {
                result[j] ^= result[j + 1];
            }
        }
        root = gf_mul(root, 2);
    }
    result
}

fn rs_remainder(data: &[u8], divisor: &[u8]) -> [u8; 30] {
    let mut result = [0u8; 30];
    let n = divisor.len();
    for &b in data {
        let factor = b ^ result[0];
        result.copy_within(1..n, 0);
        result[n - 1] = 0;
        for (r, &d) in result[..n].iter_mut().zip(divisor) {
            *r ^= gf_mul(d, factor);
        }
    }
    result
}

/// Product in GF(2^8) modulo x^8 + x^4 + x^3 + x^2 + 1.
fn gf_mul(x: u8, y: u8) -> u8 {
    let mut z = 0u16;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= ((y >> i) & 1) as u16 * x as u16;
    }
    z as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use qrcodegen_no_heap::{Mask, QrCode, QrCodeEcc, Version};

    fn reference(text: &[u8]) -> Option<(i32, std::vec::Vec<bool>)> {
        let len = Version::new(MAX_VERSION as u8).buffer_len();
        let (mut buf, mut out) = (std::vec![0u8; len], std::vec![0u8; len]);
        buf[..text.len()].copy_from_slice(text);
        let v = Version::new(MAX_VERSION as u8);
        let qr = QrCode::encode_binary(&mut buf, text.len(), &mut out, QrCodeEcc::Low, Version::MIN, v, Some(Mask::new(0)), false).ok()?;
        let n = qr.size();
        Some((n, (0..n * n).map(|i| qr.get_module(i % n, i / n)).collect()))
    }

    #[test]
    fn module_for_module_as_qrcodegen() {
        // every length up to the largest that fits, so every version and block layout comes up
        for len in 0..=280 {
            let text: std::vec::Vec<u8> = (0..len).map(|i| (i * 37 + len) as u8).collect();
            let ours = Qr::encode(&text);
            match reference(&text) {
                None => assert!(ours.is_none(), "length {len} fits ours only"),
                Some((n, modules)) => {
                    let ours = ours.unwrap_or_else(|| panic!("length {len} fits qrcodegen only"));
                    assert_eq!(ours.size(), n, "length {len}");
                    for (i, &m) in modules.iter().enumerate() {
                        let (x, y) = (i as i32 % n, i as i32 / n);
                        assert_eq!(ours.get(x, y), m, "length {len}, module ({x}, {y})");
                    }
                }
            }
        }
    }
}
