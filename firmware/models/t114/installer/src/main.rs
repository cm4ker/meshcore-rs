//! The T114 firmware's installer. An update arrives as a plan (`meshcore_inplace`) in the staging
//! area of the internal flash, where the firmware has checked it; to install it, the firmware
//! saves the first page of the app flash, points that page at this program and resets. This
//! moves the old firmware up and writes the new one over it page by page, the first page last
//! (`meshcore_inplace::install`), and resets into it. There is no firmware to go back to.
//!
//! A power loss along the way leaves the first page pointing here, so the work goes on at the
//! next boot. With nothing valid to install, the firmware that ran gets its first page back and
//! starts again; with no firmware left to start, this restarts into the bootloader's USB drive
//! mode: the board then needs a firmware copied over USB.

#![no_std]
#![no_main]

use cortex_m_rt::{ExceptionFrame, entry, exception};
use meshcore_inplace::{Flash, Layout, Outcome};
use nrf_pac as pac;

/// `GPREGRET` value that keeps the Adafruit bootloader in its USB drive mode.
const DFU_MAGIC_UF2_RESET: u8 = 0x57;
/// The LED (P1.03, active low), lit while installing.
const LED_PIN: usize = 3;

// shared with the firmware (../src/ota.rs)
const LAYOUT: Layout = Layout { app: 0x26000, app_end: 0x99000, plan: 0x99000, saved_first: 0xD8000, request: 0xD9000 };
/// The staging area's end: the plan is in it.
const PLAN_END: u32 = 0xD6000;

#[entry]
fn main() -> ! {
    pac::P1.outclr().write(|w| w.set_pin(LED_PIN, true));
    pac::P1.pin_cnf(LED_PIN).write(|w| w.set_dir(pac::gpio::vals::Dir::Output));
    // SAFETY: the staging area, in the internal flash, which is memory-mapped
    let plan = unsafe { core::slice::from_raw_parts(LAYOUT.plan as *const u8, (PLAN_END - LAYOUT.plan) as usize) };
    let outcome = meshcore_inplace::install(&mut Nvmc, &LAYOUT, plan);
    pac::P1.outset().write(|w| w.set_pin(LED_PIN, true));
    reset(if outcome == Outcome::Broken { DFU_MAGIC_UF2_RESET } else { 0 })
}

struct Nvmc;

impl Flash for Nvmc {
    fn read(&self, addr: u32) -> u32 {
        // SAFETY: a word-aligned address in the internal flash, which is memory-mapped
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }

    fn erase(&mut self, page: u32) {
        pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Een));
        pac::NVMC.erasepage().write_value(page);
        while !pac::NVMC.ready().read().ready() {}
        pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Ren));
    }

    fn write(&mut self, addr: u32, word: u32) {
        pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Wen));
        // SAFETY: a word-aligned address in the app or request flash
        unsafe { core::ptr::write_volatile(addr as *mut u32, word) };
        while !pac::NVMC.ready().read().ready() {}
        pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Ren));
    }

    /// The firmware's watchdog keeps running through the reset that started us.
    fn feed(&mut self) {
        let wdt = pac::WDT;
        if wdt.runstatus().read().runstatus() {
            let enabled = wdt.rren().read().0;
            for i in 0..8 {
                if enabled & (1 << i) != 0 {
                    wdt.rr(i).write(|w| w.set_rr(pac::wdt::vals::Rr::Reload));
                }
            }
        }
    }
}

fn reset(gpregret: u8) -> ! {
    pac::POWER.gpregret().write(|w| w.set_gpregret(gpregret));
    cortex_m::peripheral::SCB::sys_reset()
}

#[exception]
unsafe fn HardFault(_frame: &ExceptionFrame) -> ! {
    reset(DFU_MAGIC_UF2_RESET)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    reset(DFU_MAGIC_UF2_RESET)
}
