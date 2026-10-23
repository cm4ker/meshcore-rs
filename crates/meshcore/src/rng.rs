//! Fast non-cryptographic PRNG for jitter delays and packet-uniqueness blobs
//! (upstream uses `StdRNG` seeded from the radio). Key generation uses the hardware TRNG instead.

/// xoshiro128++
#[derive(Clone, Debug)]
pub struct FastRng {
    s: [u32; 4],
}

impl FastRng {
    pub fn new(seed: [u8; 16]) -> Self {
        let mut s = [0u32; 4];
        for (i, w) in s.iter_mut().enumerate() {
            *w = u32::from_le_bytes(seed[i * 4..i * 4 + 4].try_into().unwrap());
        }
        if s == [0; 4] {
            s = [0x9E37_79B9, 0x7F4A_7C15, 0xF39C_C060, 0x5CED_C834];
        }
        Self { s }
    }

    pub fn next_u32(&mut self) -> u32 {
        let result = self.s[0].wrapping_add(self.s[3]).rotate_left(7).wrapping_add(self.s[0]);
        let t = self.s[1] << 9;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(11);
        result
    }

    pub fn fill(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(4) {
            let v = self.next_u32().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    /// Random number in `[min, max)` (`RNG::nextInt`); returns `min` for an empty range.
    pub fn next_int(&mut self, min: u32, max: u32) -> u32 {
        if max <= min {
            return min;
        }
        (self.next_u32() % (max - min)) + min
    }

    /// Mix extra entropy in (e.g. radio RSSI noise).
    pub fn stir(&mut self, entropy: u32) {
        self.s[0] ^= entropy;
        self.next_u32();
    }
}
