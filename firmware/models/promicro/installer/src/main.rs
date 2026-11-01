//! The Pro Micro firmware's installer. An update arrives in the staging area of the internal
//! flash, where the firmware has verified it; to install it, the firmware points the first page of
//! the app flash at this program and resets. This copies the image into the app flash, the first
//! page last, and resets into it. (The T-Echo's installer does the same from its QSPI flash.)
//!
//! A power loss along the way leaves the first page pointing here, so the copy starts over at the
//! next boot (pages already in place are skipped; the staging area is never written here). Only
//! rewriting the first page itself, a few milliseconds, is exposed: the page is written last word
//! first, so until it is complete the bootloader sees no app and stays in its USB drive mode. With
//! nothing valid to install, this restarts into that mode too: the board then needs a firmware
//! copied over USB.

#![no_std]
#![no_main]

use cortex_m_rt::{ExceptionFrame, entry, exception};
use nrf_pac as pac;
use sha2::{Digest, Sha256};

// shared with the firmware (src/ota.rs)
const APP_START: u32 = 0x26000;
const APP_END: u32 = 0x7B000;
const STAGING: u32 = 0x7B000;
const RECORD: u32 = 0xD9000;
const RECORD_MAGIC: &[u8; 8] = b"MCRS-INS";
const TRAILER_MAGIC: &[u8; 8] = b"MCRS-NRF";
const TRAILER_LEN: u32 = 64;
const CHIP: u32 = 0x52840;

const PAGE: u32 = 4096;
const WORDS: usize = (PAGE / 4) as usize;
/// `GPREGRET` value that keeps the Adafruit bootloader in its USB drive mode.
const DFU_MAGIC_UF2_RESET: u8 = 0x57;
/// The LED (P0.15, active high), lit while installing.
const LED_PIN: usize = 15;

#[entry]
fn main() -> ! {
    pac::P0.outset().write(|w| w.set_pin(LED_PIN, true));
    pac::P0.pin_cnf(LED_PIN).write(|w| w.set_dir(pac::gpio::vals::Dir::Output));
    let ok = install();
    pac::P0.outclr().write(|w| w.set_pin(LED_PIN, true));
    reset(if ok { 0 } else { DFU_MAGIC_UF2_RESET })
}

fn install() -> bool {
    // the request: `[magic 8][image length u32][its complement u32]`
    let rec = |i: usize| flash_word(RECORD + 4 * i as u32);
    let total = rec(2);
    let magic = |i: usize| u32::from_le_bytes(RECORD_MAGIC[4 * i..4 * i + 4].try_into().unwrap());
    if rec(0) != magic(0)
        || rec(1) != magic(1)
        || rec(3) != !total
        || total % 4 != 0
        || total < PAGE + TRAILER_LEN
        || total > (APP_END - APP_START).min(RECORD - STAGING)
    {
        return false;
    }
    // the image's trailer, and the app's hash in it
    let app_len = total - TRAILER_LEN;
    let trailer = staged(app_len, TRAILER_LEN);
    let word = |i: usize| u32::from_le_bytes(trailer[4 * i..4 * i + 4].try_into().unwrap());
    if trailer[..8] != TRAILER_MAGIC[..] || word(2) != app_len || word(3) != APP_START || word(4) != CHIP {
        return false;
    }
    let mut hash = Sha256::new();
    let mut offset = 0;
    while offset < app_len {
        let n = (app_len - offset).min(PAGE);
        hash.update(staged(offset, n));
        offset += n;
        feed_watchdog();
    }
    if hash.finalize()[..] != trailer[32..64] {
        return false;
    }
    // every page but the first, then the first: from then on the bootloader starts the new app
    let pages = total.div_ceil(PAGE);
    (1..pages).all(|p| copy_page(p, total)) && copy_page(0, total)
}

/// `len` bytes of the staged image from `offset` (memory-mapped flash).
fn staged(offset: u32, len: u32) -> &'static [u8] {
    // SAFETY: inside the staging area, which nothing writes while this runs
    unsafe { core::slice::from_raw_parts((STAGING + offset) as *const u8, len as usize) }
}

fn flash_word(addr: u32) -> u32 {
    // SAFETY: a word-aligned address in the internal flash
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

/// Bring page `p` of the app flash in line with the image; false if it does not read back.
fn copy_page(p: u32, total: u32) -> bool {
    feed_watchdog();
    let n = (total - p * PAGE).min(PAGE) as usize;
    let mut buf = [0xFFFF_FFFFu32; WORDS];
    for (i, w) in buf.iter_mut().enumerate().take(n / 4) {
        *w = flash_word(STAGING + p * PAGE + 4 * i as u32);
    }
    let addr = APP_START + p * PAGE;
    let flash = |i: usize| flash_word(addr + 4 * i as u32);
    if (0..WORDS).all(|i| flash(i) == buf[i]) {
        return true; // in place already (an earlier attempt, or unchanged)
    }
    nvmc_erase(addr);
    // the first page last word first: its first word (the stack pointer) marks it complete
    for i in (0..WORDS).rev() {
        if buf[i] != 0xFFFF_FFFF {
            nvmc_write(addr + 4 * i as u32, buf[i]);
        }
    }
    (0..WORDS).all(|i| flash(i) == buf[i])
}

fn nvmc_erase(addr: u32) {
    pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Een));
    pac::NVMC.erasepage().write_value(addr);
    while !pac::NVMC.ready().read().ready() {}
    pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Ren));
}

fn nvmc_write(addr: u32, word: u32) {
    pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Wen));
    // SAFETY: a word-aligned address in the app flash, erased just before
    unsafe { core::ptr::write_volatile(addr as *mut u32, word) };
    while !pac::NVMC.ready().read().ready() {}
    pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Ren));
}

/// The firmware's watchdog keeps running through the reset that started us.
fn feed_watchdog() {
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
