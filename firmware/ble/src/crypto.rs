//! The Security Manager's toolbox ([Vol 3] Part H, Section 2.2) and the P-256 key agreement.
//!
//! Everything here is in the specification's order, most significant byte first, as its sample
//! data is; on the air the same values go least significant byte first ([`flip`]).

use crate::p256::Secret;

/// A value in the other byte order: the air's for the specification's and back.
pub fn flip<const N: usize>(mut v: [u8; N]) -> [u8; N] {
    v.reverse();
    v
}

/// A device address as the functions take it: its type (0 public, 1 random), then the address.
pub type Addr = [u8; 7];

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

#[inline(always)]
fn xtime(a: u8) -> u8 {
    (a << 1) ^ ((a >> 7).wrapping_neg() & 0x1b)
}

#[inline(never)]
fn encrypt(key: &[u8; 16], s: &mut [u8; 16]) {
    let mut rk = *key;
    for i in 0..16 {
        s[i] ^= rk[i];
    }
    let mut rcon = 1u8;
    for r in 1..=10 {
        for b in s.iter_mut() {
            *b = SBOX[*b as usize];
        }
        let t = *s;
        for c in 0..4 {
            for row in 1..4 {
                s[c * 4 + row] = t[((c + row) & 3) * 4 + row];
            }
        }
        if r < 10 {
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
        // On-the-fly AES-128 round key schedule (16 bytes RAM, no expanded table)
        let prev = rk;
        let mut w = [SBOX[prev[13] as usize] ^ rcon, SBOX[prev[14] as usize], SBOX[prev[15] as usize], SBOX[prev[12] as usize]];
        rcon = xtime(rcon);
        for c in 0..4 {
            for j in 0..4 {
                w[j] ^= prev[c * 4 + j];
                rk[c * 4 + j] = w[j];
                s[c * 4 + j] ^= w[j];
            }
        }
    }
}

/// AES-CMAC (RFC 4493) of the parts taken as one message.
pub fn cmac(key: &[u8; 16], parts: &[&[u8]]) -> [u8; 16] {
    // a 128-bit value times x in GF(2^128)
    fn double(v: &mut [u8; 16]) {
        let carry = v[0] >> 7;
        for i in 0..15 {
            v[i] = (v[i] << 1) | (v[i + 1] >> 7);
        }
        v[15] = (v[15] << 1) ^ (carry * 0x87);
    }
    let mut last_key = [0u8; 16];
    encrypt(key, &mut last_key);
    double(&mut last_key); // K1: for a whole last block

    // the block that is filling; one is encrypted only once more bytes follow it
    let (mut state, mut block, mut filled) = ([0u8; 16], [0u8; 16], 0);
    for &byte in parts.iter().copied().flatten() {
        if filled == 16 {
            for (s, b) in state.iter_mut().zip(block) {
                *s ^= b;
            }
            encrypt(key, &mut state);
            filled = 0;
        }
        block[filled] = byte;
        filled += 1;
    }
    if filled < 16 {
        block[filled] = 0x80;
        block[filled + 1..].fill(0);
        double(&mut last_key); // K2: for a padded one
    }
    for ((s, b), k) in state.iter_mut().zip(block).zip(last_key) {
        *s ^= b ^ k;
    }
    encrypt(key, &mut state);
    state
}

/// The confirm value of LE Secure Connections (2.2.6): `x` commits to the public key X
/// coordinates `u`, `v` and to `z`.
pub fn f4(u: &[u8; 32], v: &[u8; 32], x: &[u8; 16], z: u8) -> [u8; 16] {
    cmac(x, &[u, v, &[z]])
}

/// The key of the check values and the long term key (2.2.7), from the Diffie-Hellman key, both
/// nonces and both addresses, the central's first.
pub fn f5(dh_key: &[u8; 32], n1: &[u8; 16], n2: &[u8; 16], a1: &Addr, a2: &Addr) -> ([u8; 16], [u8; 16]) {
    const SALT: [u8; 16] = 0x6C88_8391_AAF5_A538_6037_0BDB_5A60_83BE_u128.to_be_bytes();
    let t = cmac(&SALT, &[dh_key]);
    let half = |counter: u8| cmac(&t, &[&[counter], b"btle", n1, n2, a1, a2, &[1, 0]]);
    (half(0), half(1))
}

/// A check value (2.2.8): what each side proves the pairing with. `io_cap` is the sender's
/// AuthReq, OOB flag and IO capability, in this order.
pub fn f6(mac_key: &[u8; 16], n1: &[u8; 16], n2: &[u8; 16], r: &[u8; 16], io_cap: &[u8; 3], a1: &Addr, a2: &Addr) -> [u8; 16] {
    cmac(mac_key, &[n1, n2, r, io_cap, a1, a2])
}

/// The hash of a resolvable private address (2.2.2), from the key and the address's random part.
pub fn ah(irk: &[u8; 16], prand: [u8; 3]) -> [u8; 3] {
    let mut block = [0u8; 16];
    block[13..].copy_from_slice(&prand);
    encrypt(irk, &mut block);
    [block[13], block[14], block[15]]
}

/// Whether `address` (as on the air) is a resolvable private address made with the identity
/// resolving key `irk` (as on the air).
pub fn resolves(irk: &[u8; 16], address: &[u8; 6]) -> bool {
    let prand = [address[5], address[4], address[3]];
    prand[0] & 0xC0 == 0x40 && ah(&flip(*irk), prand) == [address[2], address[1], address[0]]
}

/// The debug key pair's public key ([Vol 3] Part H, Section 2.3.5.6.1): anyone can listen in on a
/// pairing done with it.
const DEBUG_PUBLIC_KEY: [u8; 64] = [
    0x20, 0xb0, 0x03, 0xd2, 0xf2, 0x97, 0xbe, 0x2c, 0x5e, 0x2c, 0x83, 0xa7, 0xe9, 0xf9, 0xa5, 0xb9, 0xef, 0xf4, 0x91, 0x11, 0xac,
    0xf4, 0xfd, 0xdb, 0xcc, 0x03, 0x01, 0x48, 0x0e, 0x35, 0x9d, 0xe6, 0xdc, 0x80, 0x9c, 0x49, 0x65, 0x2a, 0xeb, 0x6d, 0x63, 0x32,
    0x9a, 0xbf, 0x5a, 0x52, 0x15, 0x5c, 0x76, 0x63, 0x45, 0xc2, 0x8f, 0xed, 0x30, 0x24, 0x74, 0x1c, 0x8e, 0xd0, 0x15, 0x89, 0xd2,
    0x8b,
];

/// A P-256 key pair of one pairing: the public key is X then Y.
pub struct KeyPair {
    secret: Secret,
    pub public: [u8; 64],
}

impl KeyPair {
    /// A key pair from 32 random bytes; `None` for the few that are no secret key (zero, or not
    /// below the group's order): try other ones.
    pub fn new(random: [u8; 32]) -> Option<Self> {
        let secret = Secret::from_be_bytes(random)?;
        let public = secret.public_key();
        Some(Self { secret, public })
    }

    /// The Diffie-Hellman key with the peer's public key; `None` for one that is not on the curve
    /// (it would give our secret key away), is the debug key or is our own, sent back.
    pub fn dh_key(&self, peer: &[u8; 64]) -> Option<[u8; 32]> {
        if *peer == self.public || *peer == DEBUG_PUBLIC_KEY {
            return None;
        }
        self.secret.dh_key(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes<const N: usize>(hex: &str) -> [u8; N] {
        let digits: alloc::vec::Vec<u8> = hex.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
        assert_eq!(digits.len(), N * 2, "{hex}");
        core::array::from_fn(|i| {
            let digit = |b: u8| (b as char).to_digit(16).unwrap() as u8;
            digit(digits[i * 2]) << 4 | digit(digits[i * 2 + 1])
        })
    }

    /// RFC 4493, section 4: messages of no, one, two and a half, and four blocks.
    #[test]
    fn cmac_of_rfc_4493() {
        let key = bytes("2b7e1516 28aed2a6 abf71588 09cf4f3c");
        let message: [u8; 64] = bytes(
            "6bc1bee2 2e409f96 e93d7e11 7393172a ae2d8a57 1e03ac9c 9eb76fac 45af8e51
             30c81c46 a35ce411 e5fbc119 1a0a52ef f69f2445 df4f9b17 ad2b417b e66c3710",
        );
        assert_eq!(cmac(&key, &[]), bytes("bb1d6929 e9593728 7fa37d12 9b756746"));
        assert_eq!(cmac(&key, &[&message[..16]]), bytes("070a16b4 6b4d4144 f79bdd9d d04a287c"));
        assert_eq!(cmac(&key, &[&message[..40]]), bytes("dfa66747 de9ae630 30ca3261 1497c827"));
        assert_eq!(cmac(&key, &[&message]), bytes("51f0bebf 7e3b9d92 fc497417 79363cfe"));
        // in parts, as the functions give it
        assert_eq!(cmac(&key, &[&message[..3], &[], &message[3..40]]), bytes("dfa66747 de9ae630 30ca3261 1497c827"));
        assert_eq!(cmac(&key, &[&message[..16], &message[16..]]), bytes("51f0bebf 7e3b9d92 fc497417 79363cfe"));
    }

    const U: &str = "20b003d2 f297be2c 5e2c83a7 e9f9a5b9 eff49111 acf4fddb cc030148 0e359de6";
    const V: &str = "55188b3d 32f6bb9a 900afcfb eed4e72a 59cb9ac2 f19d7cfb 6b4fdd49 f47fc5fd";
    const N1: &str = "d5cb8454 d177733e ffffb2ec 712baeab";
    const N2: &str = "a6e8e7cc 25a75f6e 216583f7 ff3dc4cf";
    const A1: &str = "00 561237 37bfce";
    const A2: &str = "00 a71370 2dcfc1";

    /// [Vol 3] Part H, Appendix D.2 to D.4.
    #[test]
    fn functions_of_the_specification() {
        assert_eq!(f4(&bytes(U), &bytes(V), &bytes(N1), 0), bytes("f2c916f1 07a9bd1c f1eda1be a974872d"));

        let dh_key = bytes("ec0234a3 57c8ad05 341010a6 0a397d9b 99796b13 b4f866f1 868d34f3 73bfa698");
        let (mac_key, ltk) = f5(&dh_key, &bytes(N1), &bytes(N2), &bytes(A1), &bytes(A2));
        assert_eq!(ltk, bytes("69867911 69d7cd23 980522b5 94750a38"));
        assert_eq!(mac_key, bytes("2965f176 a1084a02 fd3f6a20 ce636e20"));

        let r = bytes("12a3343b b453bb54 08da42d2 0c2d0fc8");
        let check = f6(&mac_key, &bytes(N1), &bytes(N2), &r, &[1, 1, 2], &bytes(A1), &bytes(A2));
        assert_eq!(check, bytes("e3c47398 9cd0e8c5 d26c0b09 da958f61"));
    }

    /// [Vol 3] Part H, Appendix D.7, and an address a peer made.
    #[test]
    fn private_addresses() {
        let irk = bytes("ec0234a3 57c8ad05 341010a6 0a397d9b");
        assert_eq!(ah(&irk, [0x70, 0x81, 0x94]), [0x0d, 0xfb, 0xaa]);
        // prand 708194 and its hash, as the address 70:81:94:0D:FB:AA goes on the air
        assert!(resolves(&flip(irk), &[0xaa, 0xfb, 0x0d, 0x94, 0x81, 0x70]));
        assert!(!resolves(&flip(irk), &[0xab, 0xfb, 0x0d, 0x94, 0x81, 0x70]));

        let irk = flip(bytes("8b3958c1 58ed6446 7bd27bc9 0d3cf54d"));
        assert!(resolves(&irk, &[0x92, 0xF2, 0x8F, 0x84, 0x72, 0x4F]));
        // a static address is none of them
        assert!(!resolves(&irk, &[0x92, 0xF2, 0x8F, 0x84, 0x72, 0xCF]));
    }

    /// [Vol 2] Part G, Section 7.1.2: both sets of P-256 sample data.
    #[test]
    fn p256_of_the_specification() {
        for (secret_a, secret_b, public_a, public_b, dh_key) in [
            (
                "3f49f6d4 a3c55f38 74c9b3e3 d2103f50 4aff607b eb40b799 5899b8a6 cd3c1abd",
                "55188b3d 32f6bb9a 900afcfb eed4e72a 59cb9ac2 f19d7cfb 6b4fdd49 f47fc5fd",
                "20b003d2 f297be2c 5e2c83a7 e9f9a5b9 eff49111 acf4fddb cc030148 0e359de6
                 dc809c49 652aeb6d 63329abf 5a52155c 766345c2 8fed3024 741c8ed0 1589d28b",
                "1ea1f0f0 1faf1d96 09592284 f19e4c00 47b58afd 8615a69f 559077b2 2faaa190
                 4c55f33e 429dad37 7356703a 9ab85160 472d1130 e28e3676 5f89aff9 15b1214a",
                "ec0234a3 57c8ad05 341010a6 0a397d9b 99796b13 b4f866f1 868d34f3 73bfa698",
            ),
            (
                "06a51669 3c9aa31a 6084545d 0c5db641 b48572b9 7203ddff b7ac73f7 d0457663",
                "529aa067 0d72cd64 97502ed4 73502b03 7e8803b5 c60829a5 a3caa219 505530ba",
                "2c31a47b 5779809e f44cb5ea af5c3e43 d5f8faad 4a8794cb 987e9b03 745c78dd
                 91951218 3898dfbe cd52e240 8e43871f d0211091 17bd3ed4 eaf84377 43715d4f",
                "f465e43f f23d3f1b 9dc7dfc0 4da87581 84dbc966 204796ec cf0d6cf5 e16500cc
                 0201d048 bcbbd899 eeefc424 164e33c2 01c2b010 ca6b4d43 a8a155ca d8ecb279",
                "ab85843a 2f6d883f 62e5684b 38e30733 5fe6e194 5ecd1960 4105c6f2 3221eb69",
            ),
        ] {
            let (a, b) = (KeyPair::new(bytes(secret_a)).unwrap(), KeyPair::new(bytes(secret_b)).unwrap());
            assert_eq!(a.public, bytes(public_a));
            assert_eq!(b.public, bytes(public_b));
            // the first set's A is the debug key pair: nobody takes its public key
            let debug = a.public == DEBUG_PUBLIC_KEY;
            assert_eq!(b.dh_key(&a.public), (!debug).then(|| bytes(dh_key)));
            assert_eq!(a.dh_key(&b.public), Some(bytes(dh_key)));
        }
    }

    #[test]
    fn public_keys_nobody_should_take() {
        let pair = KeyPair::new([7; 32]).unwrap();
        assert_eq!(pair.dh_key(&pair.public), None, "our own");
        let mut off_curve = KeyPair::new([9; 32]).unwrap().public;
        off_curve[63] ^= 1;
        assert_eq!(pair.dh_key(&off_curve), None);
        assert!(KeyPair::new([0; 32]).is_none());
        assert!(KeyPair::new([0xff; 32]).is_none(), "above the group's order");
    }
}
