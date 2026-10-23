//! Light sleep between packets for a repeater with `powersaving on`, as upstream's
//! `ESP32Board::sleep`: the radio keeps receiving, and its IRQ line, the PRG button or the next
//! timer wakes the chip. esp-hal moves its clock on by the time slept, so timers stay right.
//!
//! The caller decides when sleeping is safe (no Wi-Fi or BLE, nothing queued to send); this
//! module only adds the one condition the hardware can tell: a USB host on the port means the
//! console is in use, and a sleeping chip drops off the bus.

use esp_hal::peripherals::{LPWR, USB_DEVICE};
use esp_hal::rtc_cntl::sleep::{LowPower, RtcSleepConfig};
use esp_hal::time::{Duration, Instant};

/// A host sends a USB start-of-frame every millisecond; this long without one means no host.
const USB_HOST_GONE: Duration = Duration::from_secs(3);

pub struct Sleeper<'d> {
    lp: LowPower<'d>,
    last_sof: u16,
    sof_seen: Instant,
    started: Instant,
    asleep: Duration,
    wakes: u32,
}

impl<'d> Sleeper<'d> {
    pub fn new(lpwr: LPWR<'d>) -> Self {
        let now = Instant::now();
        Self { lp: LowPower::new(lpwr), last_sof: 0, sof_seen: now, started: now, asleep: Duration::ZERO, wakes: 0 }
    }

    /// Whether a USB host has polled the port lately (call it every loop pass).
    pub fn usb_host_active(&mut self) -> bool {
        let sof = USB_DEVICE::regs().fram_num().read().sof_frame_index().bits();
        if sof != self.last_sof {
            self.last_sof = sof;
            self.sof_seen = Instant::now();
        }
        self.sof_seen.elapsed() < USB_HOST_GONE
    }

    /// Sleep for at most `max` (less if a wakeup source fires: arm the radio IRQ first). Feeds the
    /// watchdog around the sleep, so `max` must stay below its timeout.
    pub fn light_sleep(&mut self, max: Duration) {
        crate::reset::feed();
        let start = Instant::now();
        self.lp.set_wakeup_deadline(start + max);
        self.lp.sleep_light(RtcSleepConfig::default());
        self.lp.clear_wakeup_deadline();
        crate::reset::feed();
        self.asleep += start.elapsed();
        self.wakes += 1;
    }

    /// For `powersaving stats`.
    pub fn summary(&self) -> alloc::string::String {
        let up = self.started.elapsed().as_secs().max(1);
        let slept = self.asleep.as_secs();
        alloc::format!("asleep {}% of {} min ({} sleeps)", slept * 100 / up, up / 60, self.wakes)
    }
}
