//! The Security Manager's toolbox ([Vol 3] Part H, Section 2.2) and the P-256 key agreement.
//!
//! Everything here is in the specification's order, most significant byte first, as its sample
//! data is; on the air the same values go least significant byte first ([`flip`]).

use aes::Aes128;
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use p256::elliptic_curve::point::AffineCoordinates;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{AffinePoint, EncodedPoint, NonZeroScalar, ProjectivePoint};

/// A value in the other byte order: the air's for the specification's and back.
pub fn flip<const N: usize>(mut v: [u8; N]) -> [u8; N] {
    v.reverse();
    v
}

/// A device address as the functions take it: its type (0 public, 1 random), then the address.
pub type Addr = [u8; 7];

fn encrypt(key: &[u8; 16], block: &mut [u8; 16]) {
    let mut b = aes::Block::from(*block);
    Aes128::new(&(*key).into()).encrypt_block(&mut b);
    *block = b.into();
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
    secret: NonZeroScalar,
    pub public: [u8; 64],
}

impl KeyPair {
    /// A key pair from 32 random bytes; `None` for the few that are no secret key (zero, or not
    /// below the group's order): try other ones.
    pub fn new(random: [u8; 32]) -> Option<Self> {
        let secret = Option::<NonZeroScalar>::from(NonZeroScalar::from_repr(random.into()))?;
        let point = (ProjectivePoint::GENERATOR * *secret).to_affine().to_encoded_point(false);
        let mut public = [0u8; 64];
        public[..32].copy_from_slice(point.x()?);
        public[32..].copy_from_slice(point.y()?);
        Some(Self { secret, public })
    }

    /// The Diffie-Hellman key with the peer's public key; `None` for one that is not on the curve
    /// (it would give our secret key away), is the debug key or is our own, sent back.
    pub fn dh_key(&self, peer: &[u8; 64]) -> Option<[u8; 32]> {
        if *peer == self.public || *peer == DEBUG_PUBLIC_KEY {
            return None;
        }
        let (x, y): ([u8; 32], [u8; 32]) = (peer[..32].try_into().ok()?, peer[32..].try_into().ok()?);
        let point = EncodedPoint::from_affine_coordinates(&x.into(), &y.into(), false);
        let point = Option::<AffinePoint>::from(AffinePoint::from_encoded_point(&point))?;
        Some((ProjectivePoint::from(point) * *self.secret).to_affine().x().into())
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
