//! Running noise-floor estimate (upstream `RadioLibWrapper::loop` and `triggerNoiseFloorCalibrate`).
//!
//! Upstream reads the RSSI on every pass of a loop that spins in microseconds, so its 64 samples
//! come in within milliseconds. Our loop passes every 50 ms, and with `powersaving on` only when
//! something wakes the chip, so [`NoiseFloor::measure`] takes a whole batch in one burst instead.
//!
//! Upstream also zeroes the floor on every AGC reset, so a floor stuck too low (every reading
//! over it + 14) can recover. That made ours tell 0, or one unfiltered snapshot of a busy channel,
//! with `agc.reset.interval` set; here a burst that finds nearly every reading over the threshold
//! takes the next batch whole instead.

pub struct NoiseFloor {
    pub floor: i16,
    samples: u16,
    sum: i32,
    /// No floor to filter by: the batch takes every reading.
    open: bool,
}

impl Default for NoiseFloor {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseFloor {
    const SAMPLES: u16 = 64;
    const THRESHOLD: i16 = 14;
    /// Readings one burst may spend on a batch, the rejected ones included.
    const BURST: u16 = 2 * Self::SAMPLES;
    /// Upstream `NOISE_FLOOR_CALIB_INTERVAL`.
    pub const INTERVAL_MS: u64 = 2000;

    pub const fn new() -> Self {
        Self { floor: 0, samples: 0, sum: 0, open: true }
    }

    /// Read the RSSI until a batch is complete; `read` gives `None` while a packet is arriving,
    /// which ends the burst (the batch goes on next time). True when a new floor came out.
    pub fn measure(&mut self, mut read: impl FnMut() -> Option<i16>) -> bool {
        for _ in 0..Self::BURST {
            let Some(rssi) = read() else { return false };
            if !self.open && rssi >= self.floor + Self::THRESHOLD {
                continue;
            }
            self.samples += 1;
            self.sum += rssi as i32;
            if self.samples < Self::SAMPLES {
                continue;
            }
            // all zeros: the radio was not listening, nothing was measured
            let measured = self.sum != 0;
            if measured {
                self.floor = ((self.sum / Self::SAMPLES as i32) as i16).max(-120);
                self.open = false;
            }
            self.samples = 0;
            self.sum = 0;
            return measured;
        }
        // most readings over the floor + 14: the noise rose, or the floor is stuck too low
        self.open = true;
        self.samples = 0;
        self.sum = 0;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_burst_gives_a_floor() {
        let mut n = NoiseFloor::new();
        assert!(n.measure(|| Some(-110)));
        assert_eq!(n.floor, -110);
    }

    #[test]
    fn a_busy_moment_does_not_lift_the_floor() {
        let mut n = NoiseFloor::new();
        n.measure(|| Some(-110));
        // a neighbour's packet at -60 every other reading
        let mut v = [-60, -111].into_iter().cycle();
        assert!(n.measure(|| v.next()));
        assert_eq!(n.floor, -111);
    }

    #[test]
    fn a_floor_stuck_low_recovers() {
        let mut n = NoiseFloor::new();
        n.measure(|| Some(-120));
        // -100 is over -120 + 14: the first burst finds nothing, the next takes it whole
        assert!(!n.measure(|| Some(-100)));
        assert_eq!(n.floor, -120);
        assert!(n.measure(|| Some(-100)));
        assert_eq!(n.floor, -100);
        // and the batch after that filters again
        let mut v = [-60, -101].into_iter().cycle();
        assert!(n.measure(|| v.next()));
        assert_eq!(n.floor, -101);
    }

    #[test]
    fn a_silent_radio_does_not_zero_the_floor() {
        let mut n = NoiseFloor::new();
        n.measure(|| Some(-112));
        assert!(!n.measure(|| Some(0)));
        assert!(!n.measure(|| Some(0)));
        assert_eq!(n.floor, -112);
    }

    #[test]
    fn an_arriving_packet_ends_the_burst_and_the_batch_goes_on() {
        let mut n = NoiseFloor::new();
        let mut left = 40;
        assert!(!n.measure(|| (left > 0).then(|| { left -= 1; -115 })));
        assert_eq!(n.floor, 0);
        assert!(n.measure(|| Some(-115)));
        assert_eq!(n.floor, -115);
    }
}
