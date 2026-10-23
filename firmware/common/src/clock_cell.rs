//! Last known wall-clock time in RTC fast RAM, which keeps its contents across software,
//! watchdog and USB resets (zeroed only at power-on).

const CHECK: u32 = 0xC10C_4B1D;

#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut CELL: [u32; 2] = [0; 2];

pub fn load() -> Option<u32> {
    // SAFETY: single-threaded access from the main task; plain words, any bit pattern is valid
    let [t, check] = unsafe { core::ptr::read_volatile(&raw const CELL) };
    (check == t ^ CHECK && t != 0).then_some(t)
}

pub fn store(t: u32) {
    // SAFETY: as above
    unsafe { core::ptr::write_volatile(&raw mut CELL, [t, t ^ CHECK]) };
}
