//! Timing for the repeater's light sleep. Deadlines use monotonic milliseconds.

pub struct Deadlines {
    pub mesh: Option<u64>,
    pub room: Option<u64>,
    pub power_status: u64,
    pub battery: u64,
    pub sensors: u64,
    pub clock_save: u64,
    pub agc_reset: Option<u64>,
    pub gps_sync: Option<u64>,
    pub trial_confirm: Option<u64>,
}

impl Deadlines {
    /// Return the duration the board can sleep, or None if work is due too soon.
    pub fn sleep_span(&self, now: u64, max_sleep: u64) -> Option<u64> {
        let board = self.power_status.min(self.battery).min(self.sensors).min(self.clock_save);
        let next = [self.mesh, self.room, self.agc_reset, self.gps_sync, self.trial_confirm]
            .into_iter()
            .flatten()
            .fold(board, u64::min);
        let span = next.saturating_sub(now).min(max_sleep);
        (span >= 100).then_some(span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deadlines() -> Deadlines {
        Deadlines {
            mesh: None,
            room: None,
            power_status: 60_000,
            battery: 60_000,
            sensors: 60_000,
            clock_save: 60_000,
            agc_reset: None,
            gps_sync: None,
            trial_confirm: None,
        }
    }

    #[test]
    fn maintenance_can_end_or_prevent_light_sleep() {
        let now = 10_000;
        for due in [
            Deadlines { power_status: 9_999, ..deadlines() },
            Deadlines { battery: now, ..deadlines() },
            Deadlines { sensors: now, ..deadlines() },
            Deadlines { clock_save: now, ..deadlines() },
            Deadlines { agc_reset: Some(now), ..deadlines() },
            Deadlines { gps_sync: Some(now), ..deadlines() },
            Deadlines { trial_confirm: Some(now), ..deadlines() },
        ] {
            assert_eq!(due.sleep_span(now, 20_000), None);
        }

        let future = Deadlines {
            mesh: Some(13_000),
            room: Some(12_000),
            trial_confirm: Some(11_000),
            ..deadlines()
        };
        assert_eq!(future.sleep_span(now, 20_000), Some(1_000));
    }

    #[test]
    fn light_sleep_keeps_mesh_deadlines_and_bounds() {
        let now = 10_000;
        assert_eq!(deadlines().sleep_span(now, 20_000), Some(20_000));

        let mut next = Deadlines { mesh: Some(10_800), room: Some(10_100), ..deadlines() };
        assert_eq!(next.sleep_span(now, 20_000), Some(100));
        assert_eq!(next.sleep_span(now + 1, 20_000), None);
        next.room = None;
        assert_eq!(next.sleep_span(now, 20_000), Some(800));
    }
}
