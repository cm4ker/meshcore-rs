//! The update installer of the nRF52840 boards with their internal flash only (the Pro Micro, the
//! T114, T096). An update arrives as a plan (`meshcore_inplace`) in the staging area of the internal
//! flash, where the firmware has checked it; to install it, the firmware saves the first page of
//! the app flash, points that page at this program and resets. This moves the old firmware up and
//! writes the new one over it page by page, the first page last (`meshcore_inplace::install`), and
//! resets into it. Going back is the same with the plan back the firmware made, which the request
//! then names.
//!
//! The Pro Micro's (feature `swap`) also swaps a firmware back in from the staging area as the Pro
//! Micro took its updates before (`lib.rs`): the way back from the first firmware that applies
//! them in place to the one that swapped it in. (The T-Echo's installer works with its QSPI flash
//! instead.)
//!
//! A power loss along the way leaves the first page pointing here, so the work goes on at the next
//! boot. With nothing valid to install, the firmware that ran gets its first page back and starts
//! again; with no firmware left to start, this restarts into the bootloader's USB drive mode: the
//! board then needs a firmware copied over USB.

#![no_std]
#![no_main]

use cortex_m_rt::{ExceptionFrame, entry, exception};
use meshcore_inplace::{Flash, Layout, Outcome};
use nrf_pac as pac;

#[allow(dead_code)]
#[path = "../../ota_layout.rs"]
mod ota_layout;

/// `GPREGRET` value that keeps the Adafruit bootloader in its USB drive mode.
const DFU_MAGIC_UF2_RESET: u8 = 0x57;

/// The LED, lit while installing: the Pro Micro's P0.15 (active high), the T114's P1.03 (active
/// low), the T096's P0.28 (active high), or MeshPocket's P0.13 (active low).
#[cfg(not(feature = "t114"))]
mod led {
    use super::pac;
    const ACTIVE_LOW: bool = cfg!(feature = "mesh-pocket");
    const PIN: usize = if ACTIVE_LOW { 13 } else if cfg!(feature = "t096") { 28 } else { 15 };
    pub fn on() {
        pac::P0.out().modify(|w| w.set_pin(PIN, !ACTIVE_LOW));
        pac::P0.pin_cnf(PIN).write(|w| w.set_dir(pac::gpio::vals::Dir::Output));
    }
    pub fn off() {
        pac::P0.out().modify(|w| w.set_pin(PIN, ACTIVE_LOW));
    }
}

#[cfg(feature = "t114")]
mod led {
    use super::pac;
    const PIN: usize = 3;
    pub fn on() {
        pac::P1.outclr().write(|w| w.set_pin(PIN, true));
        pac::P1.pin_cnf(PIN).write(|w| w.set_dir(pac::gpio::vals::Dir::Output));
    }
    pub fn off() {
        pac::P1.outset().write(|w| w.set_pin(PIN, true));
    }
}

const LAYOUT: Layout = Layout {
    app: ota_layout::APP_START,
    app_end: ota_layout::APP_END,
    plan: ota_layout::STAGING,
    saved_first: ota_layout::SAVED_FIRST,
    request: ota_layout::RECORD,
};
const PLAN_END: u32 = ota_layout::STAGING_END;

#[entry]
fn main() -> ! {
    led::on();
    #[cfg(feature = "swap")]
    let outcome = match promicro_installer::requested(&Nvmc) {
        true => promicro_installer::install(&mut Nvmc),
        false => in_place(),
    };
    #[cfg(not(feature = "swap"))]
    let outcome = in_place();
    led::off();
    reset(if outcome == Outcome::Broken { DFU_MAGIC_UF2_RESET } else { 0 })
}

fn in_place() -> Outcome {
    // SAFETY: the staging area, in the internal flash, which is memory-mapped
    let staging = unsafe { core::slice::from_raw_parts(LAYOUT.plan as *const u8, (PLAN_END - LAYOUT.plan) as usize) };
    meshcore_inplace::install(&mut Nvmc, &LAYOUT, staging)
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
