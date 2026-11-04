//! The Pro Micro firmware's installer. An update arrives in the staging area of the internal
//! flash, where the firmware has verified it; to install it, the firmware saves the first page of
//! the app flash, points that page at this program and resets. This swaps the image with the app
//! (`lib.rs`), the first page last, and resets into it: the firmware that ran stays in the staging
//! area, to go back to. (The T-Echo's installer copies from its QSPI flash instead.)
//!
//! A power loss along the way leaves the first page pointing here, so the swap goes on at the next
//! boot. With nothing valid to install, the firmware that ran gets its first page back and starts
//! again; with no firmware left to start, this restarts into the bootloader's USB drive mode: the
//! board then needs a firmware copied over USB.

#![no_std]
#![no_main]

use cortex_m_rt::{ExceptionFrame, entry, exception};
use nrf_pac as pac;
use promicro_installer::{Flash, Outcome};

/// `GPREGRET` value that keeps the Adafruit bootloader in its USB drive mode.
const DFU_MAGIC_UF2_RESET: u8 = 0x57;
/// The LED (P0.15, active high), lit while installing.
const LED_PIN: usize = 15;

#[entry]
fn main() -> ! {
    pac::P0.outset().write(|w| w.set_pin(LED_PIN, true));
    pac::P0.pin_cnf(LED_PIN).write(|w| w.set_dir(pac::gpio::vals::Dir::Output));
    let outcome = promicro_installer::install(&mut Nvmc);
    pac::P0.outclr().write(|w| w.set_pin(LED_PIN, true));
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
        // SAFETY: a word-aligned address in the app, staging or request flash
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
