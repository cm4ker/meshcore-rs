//! Arithmetic mod 2^252 + 27742317777372353535851937790883648493
//! with 9 29-bit unsigned limbs
//!
//! To see that this is safe for intermediate results, note that
//! the largest limb in a 9 by 9 product of 29-bit limbs will be
//! (0x1fffffff^2) * 9 = 0x23fffffdc0000009 (62 bits).
//!
//! For a one level Karatsuba decomposition, the specific ranges
//! depend on how the limbs are combined, but will stay within
//! -0x1ffffffe00000008 (62 bits with sign bit) to
//! 0x43fffffbc0000011 (63 bits), which is still safe.

use core::fmt::Debug;
use core::ops::{Index, IndexMut};
use subtle::{Choice, ConditionallySelectable};

#[cfg(feature = "zeroize")]
use zeroize::Zeroize;

use crate::constants;

/// The `Scalar29` struct represents an element in \\(\mathbb{Z} / \ell\mathbb{Z}\\) as 9 29-bit
/// limbs
#[derive(Copy, Clone)]
pub struct Scalar29(pub [u32; 9]);

impl Debug for Scalar29 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Scalar29: {:?}", &self.0[..])
    }
}

#[cfg(feature = "zeroize")]
impl Zeroize for Scalar29 {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Index<usize> for Scalar29 {
    type Output = u32;
    fn index(&self, _index: usize) -> &u32 {
        &(self.0[_index])
    }
}

impl IndexMut<usize> for Scalar29 {
    fn index_mut(&mut self, _index: usize) -> &mut u32 {
        &mut (self.0[_index])
    }
}

/// u32 * u32 = u64 multiply helper
#[inline(always)]
fn m(x: u32, y: u32) -> u64 {
    (x as u64) * (y as u64)
}

impl Scalar29 {
    /// The scalar \\( 0 \\).
    pub const ZERO: Scalar29 = Scalar29([0, 0, 0, 0, 0, 0, 0, 0, 0]);

    /// Unpack a 32 byte / 256 bit scalar into 9 29-bit limbs.
    #[rustfmt::skip] // keep alignment of s[*] calculations
    pub fn from_bytes(bytes: &[u8; 32]) -> Scalar29 {
        let mut words = [0u32; 8];
        for i in 0..8 {
            for j in 0..4 {
                words[i] |= (bytes[(i * 4) + j] as u32) << (j * 8);
            }
        }

        let mask = (1u32 << 29) - 1;
        let top_mask = (1u32 << 24) - 1;
        let mut s = Scalar29::ZERO;

        s[0] =   words[0]                            & mask;
        s[1] = ((words[0] >> 29) | (words[1] <<  3)) & mask;
        s[2] = ((words[1] >> 26) | (words[2] <<  6)) & mask;
        s[3] = ((words[2] >> 23) | (words[3] <<  9)) & mask;
        s[4] = ((words[3] >> 20) | (words[4] << 12)) & mask;
        s[5] = ((words[4] >> 17) | (words[5] << 15)) & mask;
        s[6] = ((words[5] >> 14) | (words[6] << 18)) & mask;
        s[7] = ((words[6] >> 11) | (words[7] << 21)) & mask;
        s[8] =  (words[7] >>  8)                     & top_mask;

        s
    }

    /// Reduce a 64 byte / 512 bit scalar mod l.
    #[rustfmt::skip] // keep alignment of lo[*] calculations
    pub fn from_bytes_wide(bytes: &[u8; 64]) -> Scalar29 {
        let mut words = [0u32; 16];
        for i in 0..16 {
            for j in 0..4 {
                words[i] |= (bytes[(i * 4) + j] as u32) << (j * 8);
            }
        }

        let mask = (1u32 << 29) - 1;
        let mut lo = Scalar29::ZERO;
        let mut hi = Scalar29::ZERO;

        lo[0] =   words[ 0]                             & mask;
        lo[1] = ((words[ 0] >> 29) | (words[ 1] <<  3)) & mask;
        lo[2] = ((words[ 1] >> 26) | (words[ 2] <<  6)) & mask;
        lo[3] = ((words[ 2] >> 23) | (words[ 3] <<  9)) & mask;
        lo[4] = ((words[ 3] >> 20) | (words[ 4] << 12)) & mask;
        lo[5] = ((words[ 4] >> 17) | (words[ 5] << 15)) & mask;
        lo[6] = ((words[ 5] >> 14) | (words[ 6] << 18)) & mask;
        lo[7] = ((words[ 6] >> 11) | (words[ 7] << 21)) & mask;
        lo[8] = ((words[ 7] >>  8) | (words[ 8] << 24)) & mask;
        hi[0] = ((words[ 8] >>  5) | (words[ 9] << 27)) & mask;
        hi[1] =  (words[ 9] >>  2)                      & mask;
        hi[2] = ((words[ 9] >> 31) | (words[10] <<  1)) & mask;
        hi[3] = ((words[10] >> 28) | (words[11] <<  4)) & mask;
        hi[4] = ((words[11] >> 25) | (words[12] <<  7)) & mask;
        hi[5] = ((words[12] >> 22) | (words[13] << 10)) & mask;
        hi[6] = ((words[13] >> 19) | (words[14] << 13)) & mask;
        hi[7] = ((words[14] >> 16) | (words[15] << 16)) & mask;
        hi[8] =   words[15] >> 13                             ;

        lo = Scalar29::montgomery_mul(&lo, &constants::R);  // (lo * R) / R = lo
        hi = Scalar29::montgomery_mul(&hi, &constants::RR); // (hi * R^2) / R = hi * R

        Scalar29::add(&hi, &lo) // (hi * R) + lo
    }

    /// Pack the limbs of this `Scalar29` into 32 bytes.
    #[rustfmt::skip] // keep alignment of s[*] calculations
    #[allow(clippy::identity_op)]
    pub fn to_bytes(self) -> [u8; 32] {
        let mut s = [0u8; 32];

        s[ 0] =  (self.0[0] >>  0)                      as u8;
        s[ 1] =  (self.0[0] >>  8)                      as u8;
        s[ 2] =  (self.0[0] >> 16)                      as u8;
        s[ 3] = ((self.0[0] >> 24) | (self.0[1] << 5))  as u8;
        s[ 4] =  (self.0[1] >>  3)                      as u8;
        s[ 5] =  (self.0[1] >> 11)                      as u8;
        s[ 6] =  (self.0[1] >> 19)                      as u8;
        s[ 7] = ((self.0[1] >> 27) | (self.0[2] << 2))  as u8;
        s[ 8] =  (self.0[2] >>  6)                      as u8;
        s[ 9] =  (self.0[2] >> 14)                      as u8;
        s[10] = ((self.0[2] >> 22) | (self.0[3] << 7))  as u8;
        s[11] =  (self.0[3] >>  1)                      as u8;
        s[12] =  (self.0[3] >>  9)                      as u8;
        s[13] =  (self.0[3] >> 17)                      as u8;
        s[14] = ((self.0[3] >> 25) | (self.0[4] << 4))  as u8;
        s[15] =  (self.0[4] >>  4)                      as u8;
        s[16] =  (self.0[4] >> 12)                      as u8;
        s[17] =  (self.0[4] >> 20)                      as u8;
        s[18] = ((self.0[4] >> 28) | (self.0[5] << 1))  as u8;
        s[19] =  (self.0[5] >>  7)                      as u8;
        s[20] =  (self.0[5] >> 15)                      as u8;
        s[21] = ((self.0[5] >> 23) | (self.0[6] << 6))  as u8;
        s[22] =  (self.0[6] >>  2)                      as u8;
        s[23] =  (self.0[6] >> 10)                      as u8;
        s[24] =  (self.0[6] >> 18)                      as u8;
        s[25] = ((self.0[6] >> 26) | (self.0[7] << 3))  as u8;
        s[26] =  (self.0[7] >>  5)                      as u8;
        s[27] =  (self.0[7] >> 13)                      as u8;
        s[28] =  (self.0[7] >> 21)                      as u8;
        s[29] =  (self.0[8] >>  0)                      as u8;
        s[30] =  (self.0[8] >>  8)                      as u8;
        s[31] =  (self.0[8] >> 16)                      as u8;

        s
    }

    /// Compute `a + b` (mod l).
    pub fn add(a: &Scalar29, b: &Scalar29) -> Scalar29 {
        let mut sum = Scalar29::ZERO;
        let mask = (1u32 << 29) - 1;

        // a + b
        let mut carry: u32 = 0;
        for i in 0..9 {
            carry = a[i] + b[i] + (carry >> 29);
            sum[i] = carry & mask;
        }

        // subtract l if the sum is >= l
        Scalar29::sub(&sum, &constants::L)
    }

    /// Compute `a - b` (mod l).
    pub fn sub(a: &Scalar29, b: &Scalar29) -> Scalar29 {
        let mut difference = Scalar29::ZERO;
        let mask = (1u32 << 29) - 1;

        // a - b
        let mut borrow: u32 = 0;
        for i in 0..9 {
            borrow = a[i].wrapping_sub(b[i] + (borrow >> 31));
            difference[i] = borrow & mask;
        }

        // conditionally add l if the difference is negative
        difference.conditional_add_l(Choice::from((borrow >> 31) as u8));
        difference
    }

    pub(crate) fn conditional_add_l(&mut self, condition: Choice) -> u32 {
        let mut carry: u32 = 0;
        let mask = (1u32 << 29) - 1;

        for i in 0..9 {
            let addend = u32::conditional_select(&0, &constants::L[i], condition);
            carry = (carry >> 29) + self[i] + addend;
            self[i] = carry & mask;
        }
        carry
    }

    /// Compute a raw in-place carrying right shift over the limbs.
    #[inline(always)]
    pub(crate) fn shr1_assign(&mut self) -> u32 {
        let mut carry: u32 = 0;
        for i in (0..9).rev() {
            let limb = self[i];
            let next_carry = limb & 1;
            self[i] = (limb >> 1) | (carry << 28);
            carry = next_carry;
        }
        carry
    }

    /// Compute `a * b`.
    // meshcore-rust patch: compact constant-time schoolbook loop (replaces unrolled Karatsuba)
    #[inline(never)]
    pub(crate) fn mul_internal(a: &Scalar29, b: &Scalar29) -> [u64; 17] {
        let mut z = [0u64; 17];
        for i in 0..9 {
            let ai = a[i] as u64;
            for j in 0..9 {
                z[i + j] += ai * (b[j] as u64);
            }
        }
        z
    }

    /// Compute `a^2`.
    // meshcore-rust patch: reuse mul_internal instead of a separate unrolled squaring routine
    #[inline(always)]
    fn square_internal(a: &Scalar29) -> [u64; 17] {
        Scalar29::mul_internal(a, a)
    }

    /// Compute `limbs/R` (mod l), where R is the Montgomery modulus 2^261
    // meshcore-rust patch: compact constant-time loop (replaces unrolled reduction)
    #[inline(never)]
    pub(crate) fn montgomery_reduce(limbs: &[u64; 17]) -> Scalar29 {
        const MASK: u32 = (1u32 << 29) - 1;
        let l = &constants::L;
        let mut n = [0u32; 9];
        let mut r = [0u32; 9];
        let mut carry = 0u64;

        for k in 0..9 {
            let mut sum = carry + limbs[k];
            for j in 0..k {
                sum += m(n[j], l[k - j]);
            }
            let p = (sum as u32).wrapping_mul(constants::LFACTOR) & MASK;
            n[k] = p;
            carry = (sum + m(p, l[0])) >> 29;
        }

        for k in 9..17 {
            let mut sum = carry + limbs[k];
            for j in (k - 8)..9 {
                sum += m(n[j], l[k - j]);
            }
            r[k - 9] = (sum as u32) & MASK;
            carry = sum >> 29;
        }
        r[8] = carry as u32;

        // result may be >= l, so attempt to subtract l
        Scalar29::sub(&Scalar29(r), l)
    }

    /// Compute `a * b` (mod l).
    #[inline(never)]
    pub fn mul(a: &Scalar29, b: &Scalar29) -> Scalar29 {
        let ab = Scalar29::montgomery_reduce(&Scalar29::mul_internal(a, b));
        Scalar29::montgomery_reduce(&Scalar29::mul_internal(&ab, &constants::RR))
    }

    /// Compute `a^2` (mod l).
    #[inline(never)]
    #[allow(dead_code)] // XXX we don't expose square() via the Scalar API
    pub fn square(&self) -> Scalar29 {
        let aa = Scalar29::montgomery_reduce(&Scalar29::square_internal(self));
        Scalar29::montgomery_reduce(&Scalar29::mul_internal(&aa, &constants::RR))
    }

    /// Compute `(a * b) / R` (mod l), where R is the Montgomery modulus 2^261
    #[inline(never)]
    pub fn montgomery_mul(a: &Scalar29, b: &Scalar29) -> Scalar29 {
        Scalar29::montgomery_reduce(&Scalar29::mul_internal(a, b))
    }

    /// Compute `(a^2) / R` (mod l) in Montgomery form, where R is the Montgomery modulus 2^261
    #[inline(never)]
    pub fn montgomery_square(&self) -> Scalar29 {
        Scalar29::montgomery_reduce(&Scalar29::square_internal(self))
    }

    /// Puts a Scalar29 in to Montgomery form, i.e. computes `a*R (mod l)`
    #[inline(never)]
    pub fn as_montgomery(&self) -> Scalar29 {
        Scalar29::montgomery_mul(self, &constants::RR)
    }

    /// Takes a Scalar29 out of Montgomery form, i.e. computes `a/R (mod l)`
    #[allow(clippy::wrong_self_convention)]
    pub fn from_montgomery(&self) -> Scalar29 {
        let mut limbs = [0u64; 17];
        for i in 0..9 {
            limbs[i] = self[i] as u64;
        }
        Scalar29::montgomery_reduce(&limbs)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// Note: x is 2^253-1 which is slightly larger than the largest scalar produced by
    /// this implementation (l-1), and should verify there are no overflows for valid scalars
    ///
    /// x = 2^253-1 = 14474011154664524427946373126085988481658748083205070504932198000989141204991
    /// x = 7237005577332262213973186563042994240801631723825162898930247062703686954002 mod l
    /// x = 5147078182513738803124273553712992179887200054963030844803268920753008712037*R mod l in Montgomery form
    pub static X: Scalar29 = Scalar29([
        0x1fffffff, 0x1fffffff, 0x1fffffff, 0x1fffffff, 0x1fffffff, 0x1fffffff, 0x1fffffff,
        0x1fffffff, 0x001fffff,
    ]);

    /// x^2 = 3078544782642840487852506753550082162405942681916160040940637093560259278169 mod l
    pub static XX: Scalar29 = Scalar29([
        0x00217559, 0x000b3401, 0x103ff43b, 0x1462a62c, 0x1d6f9f38, 0x18e7a42f, 0x09a3dcee,
        0x008dbe18, 0x0006ce65,
    ]);

    /// x^2 = 2912514428060642753613814151688322857484807845836623976981729207238463947987*R mod l in Montgomery form
    pub static XX_MONT: Scalar29 = Scalar29([
        0x152b4d2e, 0x0571d53b, 0x1da6d964, 0x188663b6, 0x1d1b5f92, 0x19d50e3f, 0x12306c29,
        0x0c6f26fe, 0x00030edb,
    ]);

    /// y = 6145104759870991071742105800796537629880401874866217824609283457819451087098
    pub static Y: Scalar29 = Scalar29([
        0x1e1458fa, 0x165ba838, 0x1d787b36, 0x0e577f3a, 0x1d2baf06, 0x1d689a19, 0x1fff3047,
        0x117704ab, 0x000d9601,
    ]);

    /// x*y = 36752150652102274958925982391442301741
    pub static XY: Scalar29 = Scalar29([
        0x0ba7632d, 0x017736bb, 0x15c76138, 0x0c69daa1, 0x000001ba, 0x00000000, 0x00000000,
        0x00000000, 0x00000000,
    ]);

    /// x*y = 3783114862749659543382438697751927473898937741870308063443170013240655651591*R mod l in Montgomery form
    pub static XY_MONT: Scalar29 = Scalar29([
        0x077b51e1, 0x1c64e119, 0x02a19ef5, 0x18d2129e, 0x00de0430, 0x045a7bc8, 0x04cfc7c9,
        0x1c002681, 0x000bdc1c,
    ]);

    /// a = 2351415481556538453565687241199399922945659411799870114962672658845158063753
    pub static A: Scalar29 = Scalar29([
        0x07b3be89, 0x02291b60, 0x14a99f03, 0x07dc3787, 0x0a782aae, 0x16262525, 0x0cfdb93f,
        0x13f5718d, 0x000532da,
    ]);

    /// b = 4885590095775723760407499321843594317911456947580037491039278279440296187236
    pub static B: Scalar29 = Scalar29([
        0x15421564, 0x1e69fd72, 0x093d9692, 0x161785be, 0x1587d69f, 0x09d9dada, 0x130246c0,
        0x0c0a8e72, 0x000acd25,
    ]);

    /// a+b = 0
    /// a-b = 4702830963113076907131374482398799845891318823599740229925345317690316127506
    pub static AB: Scalar29 = Scalar29([
        0x0f677d12, 0x045236c0, 0x09533e06, 0x0fb86f0f, 0x14f0555c, 0x0c4c4a4a, 0x19fb727f,
        0x07eae31a, 0x000a65b5,
    ]);

    // c = (2^512 - 1) % l = 1627715501170711445284395025044413883736156588369414752970002579683115011840
    pub static C: Scalar29 = Scalar29([
        0x049c0f00, 0x00308f1a, 0x0164d1e9, 0x1c374ed1, 0x1be65d00, 0x19e90bfa, 0x08f73bb1,
        0x036f8613, 0x00039941,
    ]);

    #[test]
    fn mul_max() {
        let res = Scalar29::mul(&X, &X);
        for i in 0..9 {
            assert!(res[i] == XX[i]);
        }
    }

    #[test]
    fn square_max() {
        let res = X.square();
        for i in 0..9 {
            assert!(res[i] == XX[i]);
        }
    }

    #[test]
    fn montgomery_mul_max() {
        let res = Scalar29::montgomery_mul(&X, &X);
        for i in 0..9 {
            assert!(res[i] == XX_MONT[i]);
        }
    }

    #[test]
    fn montgomery_square_max() {
        let res = X.montgomery_square();
        for i in 0..9 {
            assert!(res[i] == XX_MONT[i]);
        }
    }

    #[test]
    fn mul() {
        let res = Scalar29::mul(&X, &Y);
        for i in 0..9 {
            assert!(res[i] == XY[i]);
        }
    }

    #[test]
    fn montgomery_mul() {
        let res = Scalar29::montgomery_mul(&X, &Y);
        for i in 0..9 {
            assert!(res[i] == XY_MONT[i]);
        }
    }

    #[test]
    fn add() {
        let res = Scalar29::add(&A, &B);
        let zero = Scalar29::ZERO;
        for i in 0..9 {
            assert!(res[i] == zero[i]);
        }
    }

    #[test]
    fn sub() {
        let res = Scalar29::sub(&A, &B);
        for i in 0..9 {
            assert!(res[i] == AB[i]);
        }
    }

    #[test]
    fn from_bytes_wide() {
        let bignum = [255u8; 64]; // 2^512 - 1
        let reduced = Scalar29::from_bytes_wide(&bignum);
        for i in 0..9 {
            assert!(reduced[i] == C[i]);
        }
    }
}
