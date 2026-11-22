//! Symmetric crypto exactly as MeshCore does it:
//! AES-128-ECB with zero padding, then a truncated HMAC-SHA256 over the ciphertext.
//!
//! Compact pure-Rust FIPS-197 AES-128 + RFC 2104 HMAC-SHA256 (~1.1 KiB total vs ~7 KiB for
//! fixslice `aes` + `hmac` crates on Cortex-M).
//!
//! The 32-byte shared secret is used whole as the HMAC key; its first 16 bytes are the AES key.

use sha2::{Digest, Sha256};

use crate::{CIPHER_BLOCK_SIZE, CIPHER_KEY_SIZE, CIPHER_MAC_SIZE, PUB_KEY_SIZE};

pub type Secret = [u8; PUB_KEY_SIZE];

#[rustfmt::skip]
const SBOX: [u8; 256] = [
    0x63,0x7c,0x77,0x7b,0xf2,0x6b,0x6f,0xc5,0x30,0x01,0x67,0x2b,0xfe,0xd7,0xab,0x76,
    0xca,0x82,0xc9,0x7d,0xfa,0x59,0x47,0xf0,0xad,0xd4,0xa2,0xaf,0x9c,0xa4,0x72,0xc0,
    0xb7,0xfd,0x93,0x26,0x36,0x3f,0xf7,0xcc,0x34,0xa5,0xe5,0xf1,0x71,0xd8,0x31,0x15,
    0x04,0xc7,0x23,0xc3,0x18,0x96,0x05,0x9a,0x07,0x12,0x80,0xe2,0xeb,0x27,0xb2,0x75,
    0x09,0x83,0x2c,0x1a,0x1b,0x6e,0x5a,0xa0,0x52,0x3b,0xd6,0xb3,0x29,0xe3,0x2f,0x84,
    0x53,0xd1,0x00,0xed,0x20,0xfc,0xb1,0x5b,0x6a,0xcb,0xbe,0x39,0x4a,0x4c,0x58,0xcf,
    0xd0,0xef,0xaa,0xfb,0x43,0x4d,0x33,0x85,0x45,0xf9,0x02,0x7f,0x50,0x3c,0x9f,0xa8,
    0x51,0xa3,0x40,0x8f,0x92,0x9d,0x38,0xf5,0xbc,0xb6,0xda,0x21,0x10,0xff,0xf3,0xd2,
    0xcd,0x0c,0x13,0xec,0x5f,0x97,0x44,0x17,0xc4,0xa7,0x7e,0x3d,0x64,0x5d,0x19,0x73,
    0x60,0x81,0x4f,0xdc,0x22,0x2a,0x90,0x88,0x46,0xee,0xb8,0x14,0xde,0x5e,0x0b,0xdb,
    0xe0,0x32,0x3a,0x0a,0x49,0x06,0x24,0x5c,0xc2,0xd3,0xac,0x62,0x91,0x95,0xe4,0x79,
    0xe7,0xc8,0x37,0x6d,0x8d,0xd5,0x4e,0xa9,0x6c,0x56,0xf4,0xea,0x65,0x7a,0xae,0x08,
    0xba,0x78,0x25,0x2e,0x1c,0xa6,0xb4,0xc6,0xe8,0xdd,0x74,0x1f,0x4b,0xbd,0x8b,0x8a,
    0x70,0x3e,0xb5,0x66,0x48,0x03,0xf6,0x0e,0x61,0x35,0x57,0xb9,0x86,0xc1,0x1d,0x9e,
    0xe1,0xf8,0x98,0x11,0x69,0xd9,0x8e,0x94,0x9b,0x1e,0x87,0xe9,0xce,0x55,0x28,0xdf,
    0x8c,0xa1,0x89,0x0d,0xbf,0xe6,0x42,0x68,0x41,0x99,0x2d,0x0f,0xb0,0x54,0xbb,0x16,
];

fn inv_sbox(y: u8) -> u8 {
    // Constant-time scan over the 256-byte S-box avoids keeping a second 256-byte table.
    let mut out = 0u8;
    for (i, &sb) in SBOX.iter().enumerate() {
        let eq = ((sb == y) as u8).wrapping_neg();
        out |= (i as u8) & eq;
    }
    out
}

#[inline(always)]
fn xtime(a: u8) -> u8 {
    (a << 1) ^ ((a >> 7).wrapping_neg() & 0x1b)
}

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        p ^= (b & 1).wrapping_neg() & a;
        a = xtime(a);
        b >>= 1;
    }
    p
}

#[inline(never)]
fn expand_key(key: &[u8]) -> [[u8; 16]; 11] {
    let mut rk = [[0u8; 16]; 11];
    rk[0].copy_from_slice(&key[..CIPHER_KEY_SIZE]);
    let mut rcon = 1u8;
    for r in 1..11 {
        let prev = rk[r - 1];
        let mut t = [SBOX[prev[13] as usize] ^ rcon, SBOX[prev[14] as usize], SBOX[prev[15] as usize], SBOX[prev[12] as usize]];
        rcon = xtime(rcon);
        for c in 0..4 {
            for j in 0..4 {
                t[j] ^= prev[c * 4 + j];
                rk[r][c * 4 + j] = t[j];
            }
        }
    }
    rk
}

fn add_round_key(s: &mut [u8; 16], rk: &[u8; 16]) {
    for i in 0..16 {
        s[i] ^= rk[i];
    }
}

#[inline(never)]
fn encrypt_block(rk: &[[u8; 16]; 11], s: &mut [u8; 16]) {
    add_round_key(s, &rk[0]);
    for (r, round_key) in rk.iter().enumerate().skip(1) {
        for b in s.iter_mut() {
            *b = SBOX[*b as usize];
        }
        // ShiftRows
        let t = *s;
        for c in 0..4 {
            for row in 1..4 {
                s[c * 4 + row] = t[((c + row) & 3) * 4 + row];
            }
        }
        if r < 10 {
            // MixColumns
            for c in 0..4 {
                let i = c * 4;
                let (a0, a1, a2, a3) = (s[i], s[i + 1], s[i + 2], s[i + 3]);
                let x = a0 ^ a1 ^ a2 ^ a3;
                s[i] = a0 ^ x ^ xtime(a0 ^ a1);
                s[i + 1] = a1 ^ x ^ xtime(a1 ^ a2);
                s[i + 2] = a2 ^ x ^ xtime(a2 ^ a3);
                s[i + 3] = a3 ^ x ^ xtime(a3 ^ a0);
            }
        }
        add_round_key(s, round_key);
    }
}

#[inline(never)]
fn decrypt_block(rk: &[[u8; 16]; 11], s: &mut [u8; 16]) {
    add_round_key(s, &rk[10]);
    for r in (0..10).rev() {
        // InvShiftRows
        let t = *s;
        for c in 0..4 {
            for row in 1..4 {
                s[c * 4 + row] = t[((c + 4 - row) & 3) * 4 + row];
            }
        }
        for b in s.iter_mut() {
            *b = inv_sbox(*b);
        }
        add_round_key(s, &rk[r]);
        if r > 0 {
            // InvMixColumns
            for c in 0..4 {
                let i = c * 4;
                let a = [s[i], s[i + 1], s[i + 2], s[i + 3]];
                for row in 0..4 {
                    s[i + row] = gmul(a[row], 0x0e)
                        ^ gmul(a[(row + 1) & 3], 0x0b)
                        ^ gmul(a[(row + 2) & 3], 0x0d)
                        ^ gmul(a[(row + 3) & 3], 0x09);
                }
            }
        }
    }
}

/// SHA-256 over the concatenation of `parts`, truncated into `out`.
pub fn sha256(out: &mut [u8], parts: &[&[u8]]) {
    let mut sha = Sha256::new();
    for p in parts {
        sha.update(p);
    }
    let digest = sha.finalize();
    let n = out.len().min(32);
    out[..n].copy_from_slice(&digest[..n]);
}

/// HMAC-SHA256 (RFC 2104) over the concatenation of `parts`, truncated into `out`.
#[inline(never)]
pub fn hmac_sha256(key: &[u8], out: &mut [u8], parts: &[&[u8]]) {
    let mut k_block = [0u8; 64];
    if key.len() > 64 {
        sha256(&mut k_block[..32], &[key]);
    } else {
        k_block[..key.len()].copy_from_slice(key);
    }
    let mut pad = [0u8; 64];
    for i in 0..64 {
        pad[i] = k_block[i] ^ 0x36;
    }
    let mut inner = Sha256::new();
    inner.update(pad);
    for p in parts {
        inner.update(p);
    }
    let inner_hash = inner.finalize();
    for i in 0..64 {
        pad[i] = k_block[i] ^ 0x5c;
    }
    let mut outer = Sha256::new();
    outer.update(pad);
    outer.update(inner_hash);
    let tag = outer.finalize();
    let n = out.len().min(32);
    out[..n].copy_from_slice(&tag[..n]);
}

/// Length of ciphertext produced for `len` plaintext bytes.
pub const fn padded_len(len: usize) -> usize {
    len.div_ceil(CIPHER_BLOCK_SIZE) * CIPHER_BLOCK_SIZE
}

/// AES-128-ECB encrypt, final block zero-padded. Returns bytes written (multiple of 16).
#[inline(never)]
pub fn encrypt(secret: &[u8], dest: &mut [u8], src: &[u8]) -> usize {
    let rk = expand_key(secret);
    let mut written = 0;
    for chunk in src.chunks(CIPHER_BLOCK_SIZE) {
        let mut block = [0u8; CIPHER_BLOCK_SIZE];
        block[..chunk.len()].copy_from_slice(chunk);
        encrypt_block(&rk, &mut block);
        dest[written..written + CIPHER_BLOCK_SIZE].copy_from_slice(&block);
        written += CIPHER_BLOCK_SIZE;
    }
    written
}

/// AES-128-ECB decrypt of whole blocks. Returns bytes written (trailing padding is kept).
#[inline(never)]
pub fn decrypt(secret: &[u8], dest: &mut [u8], src: &[u8]) -> usize {
    let rk = expand_key(secret);
    let mut written = 0;
    for chunk in src.as_chunks::<CIPHER_BLOCK_SIZE>().0 {
        let mut block = *chunk;
        decrypt_block(&rk, &mut block);
        dest[written..written + CIPHER_BLOCK_SIZE].copy_from_slice(&block);
        written += CIPHER_BLOCK_SIZE;
    }
    written
}

/// Encrypt `src`, prefixing `dest` with a 2-byte MAC of the ciphertext. Returns total length.
pub fn encrypt_then_mac(secret: &Secret, dest: &mut [u8], src: &[u8]) -> usize {
    let enc_len = encrypt(secret, &mut dest[CIPHER_MAC_SIZE..], src);
    let (mac, ct) = dest.split_at_mut(CIPHER_MAC_SIZE);
    hmac_sha256(secret, mac, &[&ct[..enc_len]]);
    CIPHER_MAC_SIZE + enc_len
}

/// Check the leading MAC, then decrypt. Returns decrypted length (zero-padded), or `None`.
pub fn mac_then_decrypt(secret: &Secret, dest: &mut [u8], src: &[u8]) -> Option<usize> {
    if src.len() <= CIPHER_MAC_SIZE {
        return None;
    }
    let ct = &src[CIPHER_MAC_SIZE..];
    if !ct.len().is_multiple_of(CIPHER_BLOCK_SIZE) {
        return None;
    }
    let mut mac = [0u8; CIPHER_MAC_SIZE];
    hmac_sha256(secret, &mut mac, &[ct]);
    if mac != src[..CIPHER_MAC_SIZE] {
        return None;
    }
    Some(decrypt(secret, dest, ct))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_matches_fips197() {
        // FIPS-197 appendix C.1
        let key = hex::decode("000102030405060708090a0b0c0d0e0f").unwrap();
        let pt = hex::decode("00112233445566778899aabbccddeeff").unwrap();
        let mut ct = [0u8; 16];
        assert_eq!(encrypt(&key, &mut ct, &pt), 16);
        assert_eq!(hex::encode(ct), "69c4e0d86a7b0430d8cdb78070b4c55a");
        let mut back = [0u8; 16];
        decrypt(&key, &mut back, &ct);
        assert_eq!(&back[..], &pt[..]);
    }

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 test case 2 (short key)
        let mut out = [0u8; 32];
        hmac_sha256(b"Jefe", &mut out, &[b"what do ya want ", b"for nothing?"]);
        assert_eq!(
            hex::encode(out),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // RFC 4231 test case 6 (key longer than 64-byte SHA-256 block)
        let long_key = [0xaa; 131];
        hmac_sha256(&long_key, &mut out, &[b"Test Using Larger Than Block-Size Key - Hash Key First"]);
        assert_eq!(
            hex::encode(out),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn encrypt_then_mac_roundtrip() {
        let secret: Secret = core::array::from_fn(|i| i as u8 * 7);
        let msg = b"hello mesh, this is longer than one block";
        let mut buf = [0u8; 128];
        let n = encrypt_then_mac(&secret, &mut buf, msg);
        assert_eq!(n, 2 + padded_len(msg.len()));
        let mut out = [0u8; 128];
        let m = mac_then_decrypt(&secret, &mut out, &buf[..n]).unwrap();
        assert_eq!(&out[..msg.len()], msg);
        assert!(out[msg.len()..m].iter().all(|&b| b == 0));

        buf[5] ^= 1;
        assert!(mac_then_decrypt(&secret, &mut out, &buf[..n]).is_none());
    }
}
