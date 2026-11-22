// -*- mode: rust; -*-
//
// This file is part of curve25519-dalek.
// Copyright (c) 2016-2021 isis lovecruft
// Copyright (c) 2016-2019 Henry de Valence
// See LICENSE for licensing information.
//
// Authors:
// - isis agora lovecruft <isis@patternsinthevoid.net>
// - Henry de Valence <hdevalence@hdevalence.ca>

//! Field arithmetic modulo \\(p = 2\^{255} - 19\\), using \\(32\\)-bit
//! limbs with \\(64\\)-bit products.
//!
//! This code was originally derived from Adam Langley's Golang ed25519
//! implementation, and was then rewritten to use unsigned limbs instead
//! of signed limbs.

use core::fmt::Debug;
use core::ops::Neg;
use core::ops::{Add, AddAssign};
use core::ops::{Mul, MulAssign};
use core::ops::{Sub, SubAssign};

use subtle::Choice;
use subtle::ConditionallySelectable;

#[cfg(feature = "zeroize")]
use zeroize::Zeroize;

/// A `FieldElement2625` represents an element of the field
/// \\( \mathbb Z / (2\^{255} - 19)\\).
///
/// In the 32-bit implementation, a `FieldElement` is represented in
/// radix \\(2\^{25.5}\\) as ten `u32`s.  This means that a field
/// element \\(x\\) is represented as
/// $$
/// x = \sum\_{i=0}\^9 x\_i 2\^{\lceil i \frac {51} 2 \rceil}
///   = x\_0 + x\_1 2\^{26} + x\_2 2\^{51} + x\_3 2\^{77} + \cdots + x\_9 2\^{230};
/// $$
/// the coefficients are alternately bounded by \\(2\^{25}\\) and
/// \\(2\^{26}\\).  The limbs are allowed to grow between reductions up
/// to \\(2\^{25+b}\\) or \\(2\^{26+b}\\), where \\(b = 1.75\\).
///
/// # Note
///
/// The `curve25519_dalek::field` module provides a type alias
/// `curve25519_dalek::field::FieldElement` to either `FieldElement51`
/// or `FieldElement2625`.
///
/// The backend-specific type `FieldElement2625` should not be used
/// outside of the `curve25519_dalek::field` module.
#[derive(Copy, Clone)]
pub struct FieldElement2625(pub(crate) [u32; 10]);

impl Debug for FieldElement2625 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FieldElement2625({:?})", &self.0[..])
    }
}

#[cfg(feature = "zeroize")]
impl Zeroize for FieldElement2625 {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl<'b> AddAssign<&'b FieldElement2625> for FieldElement2625 {
    fn add_assign(&mut self, _rhs: &'b FieldElement2625) {
        for i in 0..10 {
            self.0[i] += _rhs.0[i];
        }
    }
}

impl<'a, 'b> Add<&'b FieldElement2625> for &'a FieldElement2625 {
    type Output = FieldElement2625;
    fn add(self, _rhs: &'b FieldElement2625) -> FieldElement2625 {
        let mut output = *self;
        output += _rhs;
        output
    }
}

impl<'b> SubAssign<&'b FieldElement2625> for FieldElement2625 {
    fn sub_assign(&mut self, _rhs: &'b FieldElement2625) {
        // See comment in FieldElement51::Sub
        //
        // Compute a - b as ((a + 2^4 * p) - b) to avoid underflow.
        let b = &_rhs.0;
        self.0 = FieldElement2625::reduce([
            ((self.0[0] + (0x3ffffed << 4)) - b[0]) as u64,
            ((self.0[1] + (0x1ffffff << 4)) - b[1]) as u64,
            ((self.0[2] + (0x3ffffff << 4)) - b[2]) as u64,
            ((self.0[3] + (0x1ffffff << 4)) - b[3]) as u64,
            ((self.0[4] + (0x3ffffff << 4)) - b[4]) as u64,
            ((self.0[5] + (0x1ffffff << 4)) - b[5]) as u64,
            ((self.0[6] + (0x3ffffff << 4)) - b[6]) as u64,
            ((self.0[7] + (0x1ffffff << 4)) - b[7]) as u64,
            ((self.0[8] + (0x3ffffff << 4)) - b[8]) as u64,
            ((self.0[9] + (0x1ffffff << 4)) - b[9]) as u64,
        ])
        .0;
    }
}

impl<'a, 'b> Sub<&'b FieldElement2625> for &'a FieldElement2625 {
    type Output = FieldElement2625;
    fn sub(self, _rhs: &'b FieldElement2625) -> FieldElement2625 {
        let mut output = *self;
        output -= _rhs;
        output
    }
}

impl<'b> MulAssign<&'b FieldElement2625> for FieldElement2625 {
    fn mul_assign(&mut self, _rhs: &'b FieldElement2625) {
        let result = (self as &FieldElement2625) * _rhs;
        self.0 = result.0;
    }
}

impl<'a, 'b> Mul<&'b FieldElement2625> for &'a FieldElement2625 {
    type Output = FieldElement2625;

    // meshcore-rust patch: compact constant-time loop shared by mul and square
    #[inline(never)]
    fn mul(self, _rhs: &'b FieldElement2625) -> FieldElement2625 {
        FieldElement2625::reduce(FieldElement2625::mul_inner(&self.0, &_rhs.0))
    }
}

impl<'a> Neg for &'a FieldElement2625 {
    type Output = FieldElement2625;
    fn neg(self) -> FieldElement2625 {
        let mut output = *self;
        output.negate();
        output
    }
}

impl ConditionallySelectable for FieldElement2625 {
    fn conditional_select(
        a: &FieldElement2625,
        b: &FieldElement2625,
        choice: Choice,
    ) -> FieldElement2625 {
        FieldElement2625([
            u32::conditional_select(&a.0[0], &b.0[0], choice),
            u32::conditional_select(&a.0[1], &b.0[1], choice),
            u32::conditional_select(&a.0[2], &b.0[2], choice),
            u32::conditional_select(&a.0[3], &b.0[3], choice),
            u32::conditional_select(&a.0[4], &b.0[4], choice),
            u32::conditional_select(&a.0[5], &b.0[5], choice),
            u32::conditional_select(&a.0[6], &b.0[6], choice),
            u32::conditional_select(&a.0[7], &b.0[7], choice),
            u32::conditional_select(&a.0[8], &b.0[8], choice),
            u32::conditional_select(&a.0[9], &b.0[9], choice),
        ])
    }

    fn conditional_assign(&mut self, other: &FieldElement2625, choice: Choice) {
        self.0[0].conditional_assign(&other.0[0], choice);
        self.0[1].conditional_assign(&other.0[1], choice);
        self.0[2].conditional_assign(&other.0[2], choice);
        self.0[3].conditional_assign(&other.0[3], choice);
        self.0[4].conditional_assign(&other.0[4], choice);
        self.0[5].conditional_assign(&other.0[5], choice);
        self.0[6].conditional_assign(&other.0[6], choice);
        self.0[7].conditional_assign(&other.0[7], choice);
        self.0[8].conditional_assign(&other.0[8], choice);
        self.0[9].conditional_assign(&other.0[9], choice);
    }

    fn conditional_swap(a: &mut FieldElement2625, b: &mut FieldElement2625, choice: Choice) {
        u32::conditional_swap(&mut a.0[0], &mut b.0[0], choice);
        u32::conditional_swap(&mut a.0[1], &mut b.0[1], choice);
        u32::conditional_swap(&mut a.0[2], &mut b.0[2], choice);
        u32::conditional_swap(&mut a.0[3], &mut b.0[3], choice);
        u32::conditional_swap(&mut a.0[4], &mut b.0[4], choice);
        u32::conditional_swap(&mut a.0[5], &mut b.0[5], choice);
        u32::conditional_swap(&mut a.0[6], &mut b.0[6], choice);
        u32::conditional_swap(&mut a.0[7], &mut b.0[7], choice);
        u32::conditional_swap(&mut a.0[8], &mut b.0[8], choice);
        u32::conditional_swap(&mut a.0[9], &mut b.0[9], choice);
    }
}

impl FieldElement2625 {
    pub(crate) const fn from_limbs(limbs: [u32; 10]) -> FieldElement2625 {
        FieldElement2625(limbs)
    }

    /// The scalar \\( 0 \\).
    pub const ZERO: FieldElement2625 = FieldElement2625::from_limbs([0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    /// The scalar \\( 1 \\).
    pub const ONE: FieldElement2625 = FieldElement2625::from_limbs([1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    /// The scalar \\( -1 \\).
    pub const MINUS_ONE: FieldElement2625 = FieldElement2625::from_limbs([
        0x3ffffec, 0x1ffffff, 0x3ffffff, 0x1ffffff, 0x3ffffff, 0x1ffffff, 0x3ffffff, 0x1ffffff,
        0x3ffffff, 0x1ffffff,
    ]);

    /// Invert the sign of this field element
    pub fn negate(&mut self) {
        // Compute -b as ((2^4 * p) - b) to avoid underflow.
        let neg = FieldElement2625::reduce([
            ((0x3ffffed << 4) - self.0[0]) as u64,
            ((0x1ffffff << 4) - self.0[1]) as u64,
            ((0x3ffffff << 4) - self.0[2]) as u64,
            ((0x1ffffff << 4) - self.0[3]) as u64,
            ((0x3ffffff << 4) - self.0[4]) as u64,
            ((0x1ffffff << 4) - self.0[5]) as u64,
            ((0x3ffffff << 4) - self.0[6]) as u64,
            ((0x1ffffff << 4) - self.0[7]) as u64,
            ((0x3ffffff << 4) - self.0[8]) as u64,
            ((0x1ffffff << 4) - self.0[9]) as u64,
        ]);
        self.0 = neg.0;
    }

    /// Given `k > 0`, return `self^(2^k)`.
    pub fn pow2k(&self, k: u32) -> FieldElement2625 {
        debug_assert!(k > 0);
        let mut z = self.square();
        for _ in 1..k {
            z = z.square();
        }
        z
    }

    /// Given unreduced coefficients `z[0], ..., z[9]` of any size,
    /// carry and reduce them mod p to obtain a `FieldElement2625`
    /// whose coefficients have excess `b < 0.007`.
    ///
    /// In other words, each coefficient of the result is bounded by
    /// either `2^(25 + 0.007)` or `2^(26 + 0.007)`, as appropriate.
    #[rustfmt::skip] // keep alignment of carry chain
    const fn reduce(mut z: [u64; 10]) -> FieldElement2625 {

        const LOW_25_BITS: u64 = (1 << 25) - 1;
        const LOW_26_BITS: u64 = (1 << 26) - 1;

        /// Carry the value from limb i = 0..8 to limb i+1
        #[inline(always)]
        const fn carry(z: &mut [u64; 10], i: usize) {
            debug_assert!(i < 9);
            if i % 2 == 0 {
                // Even limbs have 26 bits
                z[i + 1] += z[i] >> 26;
                z[i] &= LOW_26_BITS;
            } else {
                // Odd limbs have 25 bits
                z[i + 1] += z[i] >> 25;
                z[i] &= LOW_25_BITS;
            }
        }

        // Perform two halves of the carry chain in parallel.
        carry(&mut z, 0); carry(&mut z, 4);
        carry(&mut z, 1); carry(&mut z, 5);
        carry(&mut z, 2); carry(&mut z, 6);
        carry(&mut z, 3); carry(&mut z, 7);
        // Since z[3] < 2^64, c < 2^(64-25) = 2^39,
        // so    z[4] < 2^26 + 2^39 < 2^39.0002
        carry(&mut z, 4); carry(&mut z, 8);
        // Now z[4] < 2^26
        // and z[5] < 2^25 + 2^13.0002 < 2^25.0004 (good enough)

        // Last carry has a multiplication by 19:
        z[0] += 19 * (z[9] >> 25);
        z[9] &= LOW_25_BITS;

        // Since z[9] < 2^64, c < 2^(64-25) = 2^39,
        //    so z[0] + 19*c < 2^26 + 2^43.248 < 2^43.249.
        carry(&mut z, 0);
        // Now z[1] < 2^25 - 2^(43.249 - 26)
        //          < 2^25.007 (good enough)
        // and we're done.

        FieldElement2625([
            z[0] as u32,
            z[1] as u32,
            z[2] as u32,
            z[3] as u32,
            z[4] as u32,
            z[5] as u32,
            z[6] as u32,
            z[7] as u32,
            z[8] as u32,
            z[9] as u32,
        ])
    }

    /// Load a `FieldElement51` from the low 255 bits of a 256-bit
    /// input.
    ///
    /// # Warning
    ///
    /// This function does not check that the input used the canonical
    /// representative.  It masks the high bit, but it will happily
    /// decode 2^255 - 18 to 1.  Applications that require a canonical
    /// encoding of every field element should decode, re-encode to
    /// the canonical encoding, and check that the input was
    /// canonical.
    #[rustfmt::skip] // keep alignment of h[*] values
    pub const fn from_bytes(data: &[u8; 32]) -> FieldElement2625 {
        #[inline]
        const fn load3_at(b: &[u8], i: usize) -> u64 {
           (b[i] as u64) | ((b[i + 1] as u64) << 8) | ((b[i + 2] as u64) << 16)
        }

        #[inline]
        const fn load4_at(b: &[u8], i: usize) -> u64 {
           (b[i] as u64)
           | ((b[i + 1] as u64) << 8)
           | ((b[i + 2] as u64) << 16)
           | ((b[i + 3] as u64) << 24)
        }

        let mut h = [0u64;10];
        const LOW_23_BITS: u64 = (1 << 23) - 1;
        h[0] =  load4_at(data,  0);
        h[1] =  load3_at(data,  4) << 6;
        h[2] =  load3_at(data,  7) << 5;
        h[3] =  load3_at(data, 10) << 3;
        h[4] =  load3_at(data, 13) << 2;
        h[5] =  load4_at(data, 16);
        h[6] =  load3_at(data, 20) << 7;
        h[7] =  load3_at(data, 23) << 5;
        h[8] =  load3_at(data, 26) << 4;
        h[9] = (load3_at(data, 29) & LOW_23_BITS) << 2;

        FieldElement2625::reduce(h)
    }

    /// Serialize this `FieldElement51` to a 32-byte array.  The
    /// encoding is canonical.
    #[allow(clippy::identity_op)]
    pub fn to_bytes(self) -> [u8; 32] {
        let inp = &self.0;
        // Reduce the value represented by `in` to the range [0,2*p)
        let mut h: [u32; 10] = FieldElement2625::reduce([
            // XXX this cast is annoying
            inp[0] as u64,
            inp[1] as u64,
            inp[2] as u64,
            inp[3] as u64,
            inp[4] as u64,
            inp[5] as u64,
            inp[6] as u64,
            inp[7] as u64,
            inp[8] as u64,
            inp[9] as u64,
        ])
        .0;

        // Let h be the value to encode.
        //
        // Write h = pq + r with 0 <= r < p.  We want to compute r = h mod p.
        //
        // Since h < 2*p, q = 0 or 1, with q = 0 when h < p and q = 1 when h >= p.
        //
        // Notice that h >= p <==> h + 19 >= p + 19 <==> h + 19 >= 2^255.
        // Therefore q can be computed as the carry bit of h + 19.

        let mut q: u32 = (h[0] + 19) >> 26;
        q = (h[1] + q) >> 25;
        q = (h[2] + q) >> 26;
        q = (h[3] + q) >> 25;
        q = (h[4] + q) >> 26;
        q = (h[5] + q) >> 25;
        q = (h[6] + q) >> 26;
        q = (h[7] + q) >> 25;
        q = (h[8] + q) >> 26;
        q = (h[9] + q) >> 25;

        debug_assert!(q == 0 || q == 1);

        // Now we can compute r as r = h - pq = r - (2^255-19)q = r + 19q - 2^255q

        const LOW_25_BITS: u32 = (1 << 25) - 1;
        const LOW_26_BITS: u32 = (1 << 26) - 1;

        h[0] += 19 * q;

        // Now carry the result to compute r + 19q...
        h[1] += h[0] >> 26;
        h[0] &= LOW_26_BITS;
        h[2] += h[1] >> 25;
        h[1] &= LOW_25_BITS;
        h[3] += h[2] >> 26;
        h[2] &= LOW_26_BITS;
        h[4] += h[3] >> 25;
        h[3] &= LOW_25_BITS;
        h[5] += h[4] >> 26;
        h[4] &= LOW_26_BITS;
        h[6] += h[5] >> 25;
        h[5] &= LOW_25_BITS;
        h[7] += h[6] >> 26;
        h[6] &= LOW_26_BITS;
        h[8] += h[7] >> 25;
        h[7] &= LOW_25_BITS;
        h[9] += h[8] >> 26;
        h[8] &= LOW_26_BITS;

        // ... but instead of carrying the value
        // (h[9] >> 25) = q*2^255 into another limb,
        // discard it, subtracting the value from h.
        debug_assert!((h[9] >> 25) == 0 || (h[9] >> 25) == 1);
        h[9] &= LOW_25_BITS;

        let mut s = [0u8; 32];
        s[0] = (h[0] >> 0) as u8;
        s[1] = (h[0] >> 8) as u8;
        s[2] = (h[0] >> 16) as u8;
        s[3] = ((h[0] >> 24) | (h[1] << 2)) as u8;
        s[4] = (h[1] >> 6) as u8;
        s[5] = (h[1] >> 14) as u8;
        s[6] = ((h[1] >> 22) | (h[2] << 3)) as u8;
        s[7] = (h[2] >> 5) as u8;
        s[8] = (h[2] >> 13) as u8;
        s[9] = ((h[2] >> 21) | (h[3] << 5)) as u8;
        s[10] = (h[3] >> 3) as u8;
        s[11] = (h[3] >> 11) as u8;
        s[12] = ((h[3] >> 19) | (h[4] << 6)) as u8;
        s[13] = (h[4] >> 2) as u8;
        s[14] = (h[4] >> 10) as u8;
        s[15] = (h[4] >> 18) as u8;
        s[16] = (h[5] >> 0) as u8;
        s[17] = (h[5] >> 8) as u8;
        s[18] = (h[5] >> 16) as u8;
        s[19] = ((h[5] >> 24) | (h[6] << 1)) as u8;
        s[20] = (h[6] >> 7) as u8;
        s[21] = (h[6] >> 15) as u8;
        s[22] = ((h[6] >> 23) | (h[7] << 3)) as u8;
        s[23] = (h[7] >> 5) as u8;
        s[24] = (h[7] >> 13) as u8;
        s[25] = ((h[7] >> 21) | (h[8] << 4)) as u8;
        s[26] = (h[8] >> 4) as u8;
        s[27] = (h[8] >> 12) as u8;
        s[28] = ((h[8] >> 20) | (h[9] << 6)) as u8;
        s[29] = (h[9] >> 2) as u8;
        s[30] = (h[9] >> 10) as u8;
        s[31] = (h[9] >> 18) as u8;

        // Check that high bit is cleared
        debug_assert!((s[31] & 0b1000_0000u8) == 0u8);

        s
    }

    // meshcore-rust patch: compact constant-time loop shared by Mul and square/square2
    #[inline(never)]
    fn mul_inner(x: &[u32; 10], y: &[u32; 10]) -> [u64; 10] {
        let mut t = [0u64; 19];
        for i in 0..10 {
            let xi = x[i];
            let xi_2 = xi << 1;
            let odd = (i & 1) != 0;
            for j in 0..10 {
                let a = if odd && (j & 1) != 0 { xi_2 } else { xi };
                t[i + j] += (a as u64) * (y[j] as u64);
            }
        }
        let mut z = [0u64; 10];
        for k in 0..9 {
            z[k] = t[k] + t[k + 10] * 19;
        }
        z[9] = t[9];
        z
    }

    fn square_inner(&self) -> [u64; 10] {
        Self::mul_inner(&self.0, &self.0)
    }

    /// Compute `self^2`.
    #[inline(never)]
    pub fn square(&self) -> FieldElement2625 {
        FieldElement2625::reduce(self.square_inner())
    }

    /// Compute `2*self^2`.
    #[inline(never)]
    pub fn square2(&self) -> FieldElement2625 {
        let mut coeffs = self.square_inner();
        for coeff in &mut coeffs {
            *coeff += *coeff;
        }
        FieldElement2625::reduce(coeffs)
    }
}
