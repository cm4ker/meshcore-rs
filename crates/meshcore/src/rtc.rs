//! Wall clock (UNIX epoch seconds) derived from uptime plus an adjustable base.

/// Upstream default when no clock is known: 15 May 2024, 8:50pm (`clkreboot`).
pub const DEFAULT_EPOCH: u32 = 1_715_770_351;

#[derive(Clone, Debug)]
pub struct RtcClock {
    base_epoch: u32,
    base_ms: u64,
    last_unique: u32,
}

impl RtcClock {
    pub fn new(epoch: u32) -> Self {
        Self { base_epoch: epoch.max(DEFAULT_EPOCH), base_ms: 0, last_unique: 0 }
    }

    pub fn current_time(&self, now_ms: u64) -> u32 {
        self.base_epoch.wrapping_add(((now_ms.saturating_sub(self.base_ms)) / 1000) as u32)
    }

    pub fn set_current_time(&mut self, now_ms: u64, epoch: u32) {
        self.base_epoch = epoch;
        self.base_ms = now_ms;
    }

    /// Strictly increasing timestamp (`getCurrentTimeUnique`).
    pub fn current_time_unique(&mut self, now_ms: u64) -> u32 {
        let t = self.current_time(now_ms);
        if t <= self.last_unique {
            self.last_unique += 1;
        } else {
            self.last_unique = t;
        }
        self.last_unique
    }
}
