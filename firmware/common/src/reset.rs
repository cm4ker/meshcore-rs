//! Watchdog and reboots through the RTC watchdog's full system reset.
//!
//! A plain software reset (or a timer-group watchdog reset) leaves state behind that makes the
//! Wi-Fi driver hang the whole executor on the next connect. A full system reset (including
//! the RTC domain) behaves like a cold start, so both hang recovery and deliberate reboots use it.

use core::cell::RefCell;

use critical_section::Mutex;
use esp_hal::rtc_cntl::{Rtc, RwdtStage, RwdtStageAction};
use esp_hal::time::Duration;

static RTC: Mutex<RefCell<Option<Rtc<'static>>>> = Mutex::new(RefCell::new(None));

/// Arm the watchdog: a full reset if [`feed`] is not called within `timeout_secs`.
pub fn install(mut rtc: Rtc<'static>, timeout_secs: u64) {
    rtc.rwdt.set_timeout(RwdtStage::Stage0, Duration::from_secs(timeout_secs));
    rtc.rwdt.set_stage_action(RwdtStage::Stage0, RwdtStageAction::ResetSystem);
    rtc.rwdt.feed();
    rtc.rwdt.enable();
    critical_section::with(|cs| RTC.borrow_ref_mut(cs).replace(rtc));
}

pub fn feed() {
    critical_section::with(|cs| {
        if let Some(rtc) = RTC.borrow_ref_mut(cs).as_mut() {
            rtc.rwdt.feed();
        }
    });
}

/// Reboot like a cold start.
pub fn hard_reset() -> ! {
    critical_section::with(|cs| {
        if let Some(rtc) = RTC.borrow_ref_mut(cs).as_mut() {
            rtc.rwdt.set_timeout(RwdtStage::Stage0, Duration::from_millis(20));
            rtc.rwdt.set_stage_action(RwdtStage::Stage0, RwdtStageAction::ResetSystem);
            rtc.rwdt.feed();
            rtc.rwdt.enable();
        }
    });
    let start = esp_hal::time::Instant::now();
    while start.elapsed() < Duration::from_millis(500) {}
    // watchdog not installed (or did not fire): fall back to a software reset
    esp_hal::system::software_reset()
}
