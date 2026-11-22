//! Compact, constant-time NIST P-256 (secp256r1) ECDH for BLE LE Secure Connections.
//!
//! Field arithmetic runs in 8×32-bit CIOS Montgomery form (where `-p^-1 mod 2^32 == 1`, so each
//! reduction step needs no extra multiply), point addition uses Renes–Costello–Batina (2015)
//! complete projective formulas (Algorithm 4, `a = -3`), and scalar multiplication runs a
//! constant-time 256-bit Montgomery ladder. ~1.9 KiB of Thumb-2 code vs ~11 KiB for `p256` +
//! `primeorder` + `elliptic-curve` + `der` + `sec1`.

type Fe = [u32; 8];
type Point = (Fe, Fe, Fe);

/// Prime modulus `p = 2^256 - 2^224 + 2^192 + 2^96 - 1` (little-endian words).
const P: Fe = [0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, 0x0000_0000, 0x0000_0000, 0x0000_0000, 0x0000_0001, 0xFFFF_FFFF];
/// Group order `n` (little-endian words).
const N: Fe = [0xFC63_2551, 0xF3B9_CAC2, 0xA717_9E84, 0xBCE6_FAAD, 0xFFFF_FFFF, 0xFFFF_FFFF, 0x0000_0000, 0xFFFF_FFFF];
/// Montgomery `1 = 2^256 mod p`.
const ONE_M: Fe = [0x0000_0001, 0x0000_0000, 0x0000_0000, 0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFE, 0x0000_0000];
/// Montgomery `R^2 = 2^512 mod p`.
const R2: Fe = [0x0000_0003, 0x0000_0000, 0xFFFF_FFFF, 0xFFFF_FFFB, 0xFFFF_FFFE, 0xFFFF_FFFF, 0xFFFF_FFFD, 0x0000_0004];
/// Curve parameter `b` in Montgomery form (`b * R mod p`).
const B_M: Fe = [0x29C4_BDDF, 0xD89C_DF62, 0x7884_3090, 0xACF0_05CD, 0xF721_2ED6, 0xE5A2_20AB, 0x0487_4834, 0xDC30_061D];
/// Base point `G` coordinates (standard form, little-endian words).
const GX: Fe = [0xD898_C296, 0xF4A1_3945, 0x2DEB_33A0, 0x7703_7D81, 0x63A4_40F2, 0xF8BC_E6E5, 0xE12C_4247, 0x6B17_D1F2];
const GY: Fe = [0x37BF_51F5, 0xCBB6_4068, 0x6B31_5ECE, 0x2BCE_3357, 0x7C0F_9E16, 0x8EE7_EB4A, 0xFE1A_7F9B, 0x4FE3_42E2];

/// A validated P-256 secret scalar (`1 <= secret < n`, big-endian bytes).
#[derive(Clone)]
pub struct Secret([u8; 32]);

impl Drop for Secret {
    fn drop(&mut self) {
        for b in &mut self.0 {
            // SAFETY: volatile write prevents dead-store elimination of key zeroization.
            unsafe { core::ptr::write_volatile(b, 0) };
        }
    }
}

impl Secret {
    /// Validate 32 big-endian bytes as a secret key (`1 <= k < n`).
    pub fn from_be_bytes(bytes: [u8; 32]) -> Option<Self> {
        let w = from_be(&bytes);
        if is_zero(&w) || !lt(&w, &N) {
            return None;
        }
        Some(Self(bytes))
    }

    /// The corresponding uncompressed public key `X || Y` (big-endian, 64 bytes).
    pub fn public_key(&self) -> [u8; 64] {
        let g = (to_mont(&GX), to_mont(&GY), ONE_M);
        let (x, y) = scalar_mul(&self.0, g);
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&to_be(&x));
        out[32..].copy_from_slice(&to_be(&y));
        out
    }

    /// Compute the ECDH shared secret (`X` coordinate, big-endian, 32 bytes) with `peer` (`X || Y`
    /// big-endian). Returns `None` if `peer` is not a valid affine point on P-256.
    pub fn dh_key(&self, peer: &[u8; 64]) -> Option<[u8; 32]> {
        let x = from_be(peer[..32].try_into().ok()?);
        let y = from_be(peer[32..].try_into().ok()?);
        if !lt(&x, &P) || !lt(&y, &P) {
            return None;
        }
        let xm = to_mont(&x);
        let ym = to_mont(&y);
        // Check y^2 == x^3 - 3x + b (mod p)
        let lhs = mont_mul(&ym, &ym);
        let x2 = mont_mul(&xm, &xm);
        let x3 = mont_mul(&x2, &xm);
        let x3_m3x = fe_sub(&x3, &fe_add(&fe_add(&xm, &xm), &xm));
        let rhs = fe_add(&x3_m3x, &B_M);
        if lhs != rhs {
            return None;
        }
        let (sx, _) = scalar_mul(&self.0, (xm, ym, ONE_M));
        Some(to_be(&sx))
    }
}

fn from_be(b: &[u8; 32]) -> Fe {
    core::array::from_fn(|i| {
        let k = (7 - i) * 4;
        u32::from_be_bytes([b[k], b[k + 1], b[k + 2], b[k + 3]])
    })
}

fn to_be(w: &Fe) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..8 {
        let b = w[7 - i].to_be_bytes();
        out[i * 4..i * 4 + 4].copy_from_slice(&b);
    }
    out
}

fn is_zero(a: &Fe) -> bool {
    a.iter().fold(0u32, |acc, &w| acc | w) == 0
}

/// Constant-time `a < b` on 256-bit unsigned little-endian words.
fn lt(a: &Fe, b: &Fe) -> bool {
    let mut borrow = 0u64;
    for i in 0..8 {
        let d = (a[i] as u64).wrapping_sub(b[i] as u64).wrapping_sub(borrow);
        borrow = d >> 63;
    }
    borrow != 0
}

#[inline(never)]
fn reduce_p(a: &Fe, carry: u32) -> Fe {
    let mut sub = [0u32; 8];
    let mut borrow = 0u64;
    for i in 0..8 {
        let d = (a[i] as u64).wrapping_sub(P[i] as u64).wrapping_sub(borrow);
        sub[i] = d as u32;
        borrow = d >> 63;
    }
    // Keep `sub` iff `carry != 0 || borrow == 0`
    let use_sub = ((carry as u64 | (borrow ^ 1)) != 0) as u32;
    let mask = use_sub.wrapping_neg();
    core::array::from_fn(|i| (sub[i] & mask) | (a[i] & !mask))
}

#[inline(never)]
fn fe_add(a: &Fe, b: &Fe) -> Fe {
    let mut out = [0u32; 8];
    let mut c = 0u64;
    for i in 0..8 {
        let v = a[i] as u64 + b[i] as u64 + c;
        out[i] = v as u32;
        c = v >> 32;
    }
    reduce_p(&out, c as u32)
}

#[inline(never)]
fn fe_sub(a: &Fe, b: &Fe) -> Fe {
    let mut out = [0u32; 8];
    let mut borrow = 0u64;
    for i in 0..8 {
        let d = (a[i] as u64).wrapping_sub(b[i] as u64).wrapping_sub(borrow);
        out[i] = d as u32;
        borrow = d >> 63;
    }
    let mask = (borrow as u32).wrapping_neg();
    let mut c = 0u64;
    for i in 0..8 {
        let v = out[i] as u64 + (P[i] & mask) as u64 + c;
        out[i] = v as u32;
        c = v >> 32;
    }
    out
}

/// CIOS Montgomery multiplication: `a * b * R^-1 mod p`.
/// Because `P[0] == 0xFFFF_FFFF`, `-P^-1 mod 2^32 == 1`, so `m == t[0]`.
#[inline(never)]
fn mont_mul(a: &Fe, b: &Fe) -> Fe {
    let mut t = [0u32; 9];
    for &ai in a {
        let mut c = 0u64;
        for j in 0..8 {
            let v = t[j] as u64 + ai as u64 * b[j] as u64 + c;
            t[j] = v as u32;
            c = v >> 32;
        }
        let v = t[8] as u64 + c;
        t[8] = v as u32;
        let t9 = (v >> 32) as u32;

        let m = t[0] as u64;
        let mut c = (m + m * P[0] as u64) >> 32;
        for j in 1..8 {
            let v = t[j] as u64 + m * P[j] as u64 + c;
            t[j - 1] = v as u32;
            c = v >> 32;
        }
        let v = t[8] as u64 + c;
        t[7] = v as u32;
        t[8] = t9 + (v >> 32) as u32;
    }
    reduce_p(t[..8].try_into().unwrap(), t[8])
}

fn to_mont(a: &Fe) -> Fe {
    mont_mul(a, &R2)
}

fn from_mont(a: &Fe) -> Fe {
    let one = [1, 0, 0, 0, 0, 0, 0, 0];
    mont_mul(a, &one)
}

/// Modular inverse in Montgomery form via Fermat's little theorem: `a^(p - 2) mod p`.
fn fe_inv(a: &Fe) -> Fe {
    // Exponent `p - 2` is public, so branching on its bits leaks nothing about secret inputs.
    const P_MINUS_2: Fe = [0xFFFF_FFFD, 0xFFFF_FFFF, 0xFFFF_FFFF, 0x0000_0000, 0x0000_0000, 0x0000_0000, 0x0000_0001, 0xFFFF_FFFF];
    let mut res = ONE_M;
    let mut base = *a;
    for word in P_MINUS_2 {
        for bit in 0..32 {
            if (word >> bit) & 1 != 0 {
                res = mont_mul(&res, &base);
            }
            base = mont_mul(&base, &base);
        }
    }
    res
}

/// Renes–Costello–Batina (2015) Algorithm 4 complete projective addition for `y^2 = x^3 - 3x + b`.
#[inline(never)]
fn point_add(p1: &Point, p2: &Point) -> Point {
    let (x1, y1, z1) = p1;
    let (x2, y2, z2) = p2;
    let mut t0 = mont_mul(x1, x2);
    let mut t1 = mont_mul(y1, y2);
    let mut t2 = mont_mul(z1, z2);
    let mut t3 = fe_add(x1, y1);
    let mut t4 = fe_add(x2, y2);
    t3 = mont_mul(&t3, &t4);
    t4 = fe_add(&t0, &t1);
    t3 = fe_sub(&t3, &t4);
    t4 = fe_add(y1, z1);
    let mut x3 = fe_add(y2, z2);
    t4 = mont_mul(&t4, &x3);
    x3 = fe_add(&t1, &t2);
    t4 = fe_sub(&t4, &x3);
    x3 = fe_add(x1, z1);
    let mut y3 = fe_add(x2, z2);
    x3 = mont_mul(&x3, &y3);
    y3 = fe_add(&t0, &t2);
    y3 = fe_sub(&x3, &y3);
    let mut z3 = mont_mul(&B_M, &t2);
    x3 = fe_sub(&y3, &z3);
    z3 = fe_add(&x3, &x3);
    x3 = fe_add(&x3, &z3);
    z3 = fe_sub(&t1, &x3);
    x3 = fe_add(&t1, &x3);
    y3 = mont_mul(&B_M, &y3);
    t1 = fe_add(&t2, &t2);
    t2 = fe_add(&t1, &t2);
    y3 = fe_sub(&y3, &t2);
    y3 = fe_sub(&y3, &t0);
    t1 = fe_add(&y3, &y3);
    y3 = fe_add(&t1, &y3);
    t1 = fe_add(&t0, &t0);
    t0 = fe_add(&t1, &t0);
    t0 = fe_sub(&t0, &t2);
    t1 = mont_mul(&t4, &y3);
    t2 = mont_mul(&t0, &y3);
    y3 = mont_mul(&x3, &z3);
    y3 = fe_add(&y3, &t2);
    x3 = mont_mul(&t3, &x3);
    x3 = fe_sub(&x3, &t1);
    z3 = mont_mul(&t4, &z3);
    t1 = mont_mul(&t3, &t0);
    z3 = fe_add(&z3, &t1);
    (x3, y3, z3)
}

fn cswap(a: &mut Point, b: &mut Point, swap: u32) {
    let mask = swap.wrapping_neg();
    let swap_fe = |u: &mut Fe, v: &mut Fe| {
        for i in 0..8 {
            let d = (u[i] ^ v[i]) & mask;
            u[i] ^= d;
            v[i] ^= d;
        }
    };
    swap_fe(&mut a.0, &mut b.0);
    swap_fe(&mut a.1, &mut b.1);
    swap_fe(&mut a.2, &mut b.2);
}

/// Constant-time 256-bit Montgomery ladder: returns affine `(x, y)` in standard form.
fn scalar_mul(k_be: &[u8; 32], p: Point) -> (Fe, Fe) {
    let mut r0: Point = ([0; 8], ONE_M, [0; 8]); // point at infinity
    let mut r1 = p;
    for &byte in k_be {
        for bit in (0..8).rev() {
            let b = ((byte >> bit) & 1) as u32;
            cswap(&mut r0, &mut r1, b);
            r1 = point_add(&r0, &r1);
            r0 = point_add(&r0, &r0);
            cswap(&mut r0, &mut r1, b);
        }
    }
    let zi = fe_inv(&r0.2);
    let x = from_mont(&mont_mul(&r0.0, &zi));
    let y = from_mont(&mont_mul(&r0.1, &zi));
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_bounds_and_order() {
        assert!(Secret::from_be_bytes([0u8; 32]).is_none());
        assert!(Secret::from_be_bytes(to_be(&N)).is_none());
        assert!(Secret::from_be_bytes([0xFF; 32]).is_none());

        // (n - 1) * G == -G = (Gx, p - Gy)
        let mut n_minus_1 = N;
        n_minus_1[0] -= 1;
        let s = Secret::from_be_bytes(to_be(&n_minus_1)).unwrap();
        let pk = s.public_key();
        assert_eq!(&pk[..32], &to_be(&GX));
        assert_eq!(&pk[32..], &to_be(&fe_sub(&[0; 8], &GY)));
    }

    #[test]
    fn dh_rejects_invalid_and_is_symmetric() {
        let mut ka = [0u8; 32];
        ka[31] = 7;
        let mut kb = [0u8; 32];
        kb[31] = 19;
        let a = Secret::from_be_bytes(ka).unwrap();
        let b = Secret::from_be_bytes(kb).unwrap();
        let pa = a.public_key();
        let pb = b.public_key();
        assert_eq!(a.dh_key(&pb), b.dh_key(&pa));

        // Reject point at infinity, off-curve, and out-of-field coordinates (>= p)
        assert!(a.dh_key(&[0u8; 64]).is_none());
        let mut bad = pb;
        bad[63] ^= 1;
        assert!(a.dh_key(&bad).is_none());
        let mut ge_p = pb;
        ge_p[..32].copy_from_slice(&to_be(&P));
        assert!(a.dh_key(&ge_p).is_none());
    }
}
