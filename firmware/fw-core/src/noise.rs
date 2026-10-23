//! Running noise-floor estimate (upstream `RadioLibWrapper::loop`).

pub struct NoiseFloor {
    pub floor: i16,
    samples: u16,
    sum: i32,
}

impl Default for NoiseFloor {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseFloor {
    const SAMPLES: u16 = 64;
    const THRESHOLD: i16 = 14;

    pub const fn new() -> Self {
        Self { floor: 0, samples: 0, sum: 0 }
    }

    pub fn sample(&mut self, rssi: i16) {
        if rssi < self.floor + Self::THRESHOLD {
            self.samples += 1;
            self.sum += rssi as i32;
        }
        if self.samples >= Self::SAMPLES {
            self.floor = ((self.sum / Self::SAMPLES as i32) as i16).max(-120);
            self.samples = 0;
            self.sum = 0;
        }
    }
}
