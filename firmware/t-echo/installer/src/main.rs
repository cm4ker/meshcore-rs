//! The T-Echo firmware's installer. An update arrives in the QSPI flash, where the firmware has
//! verified it; to install it, the firmware points the first page of the app flash at this
//! program and resets. This copies the image into the app flash, the first page last, and resets
//! into it.
//!
//! A power loss along the way leaves the first page pointing here, so the copy starts over at the
//! next boot (pages already in place are skipped). Only rewriting the first page itself, a few
//! milliseconds, is exposed: the page is written last word first, so until it is complete the
//! bootloader sees no app and stays in its USB drive mode. With nothing valid to install, this
//! restarts into that mode too: the board then needs a firmware copied over USB.

#![no_std]
#![no_main]

use cortex_m_rt::{ExceptionFrame, entry, exception};
use nrf_pac as pac;
use sha2::{Digest, Sha256};

// shared with the firmware (src/ota.rs)
const APP_START: u32 = 0x26000;
const APP_END: u32 = 0xED000;
const STAGING: u32 = 0x11_0000;
const RECORD: u32 = 0x1F_F000;
const RECORD_MAGIC: &[u8; 8] = b"MCRS-INS";
const TRAILER_MAGIC: &[u8; 8] = b"MCRS-NRF";
const TRAILER_LEN: u32 = 64;
const CHIP: u32 = 0x52840;

const PAGE: u32 = 4096;
const WORDS: usize = (PAGE / 4) as usize;
/// `GPREGRET` value that keeps the Adafruit bootloader in its USB drive mode.
const DFU_MAGIC_UF2_RESET: u8 = 0x57;
/// The blue LED, lit while installing (active low).
const LED_PIN: usize = 14;

#[entry]
fn main() -> ! {
    pac::P0.outclr().write(|w| w.set_pin(LED_PIN, true));
    pac::P0.pin_cnf(LED_PIN).write(|w| w.set_dir(pac::gpio::vals::Dir::Output));
    qspi_init();
    let ok = install();
    pac::P0.outset().write(|w| w.set_pin(LED_PIN, true));
    reset(if ok { 0 } else { DFU_MAGIC_UF2_RESET })
}

fn install() -> bool {
    // the request: `[magic 8][image length u32][its complement u32]`
    let mut rec = [0u32; 4];
    qspi_read(RECORD, &mut rec);
    let total = rec[2];
    if bytes(&rec)[..8] != RECORD_MAGIC[..]
        || rec[3] != !total
        || total % 4 != 0
        || total < PAGE + TRAILER_LEN
        || total > APP_END - APP_START
    {
        return false;
    }
    // the image's trailer, and the app's hash in it
    let app_len = total - TRAILER_LEN;
    let mut trailer = [0u32; (TRAILER_LEN / 4) as usize];
    qspi_read(STAGING + app_len, &mut trailer);
    let t = bytes(&trailer);
    if t[..8] != TRAILER_MAGIC[..] || trailer[2] != app_len || trailer[3] != APP_START || trailer[4] != CHIP {
        return false;
    }
    let mut buf = [0u32; WORDS];
    let mut hash = Sha256::new();
    let mut offset = 0;
    while offset < app_len {
        let n = (app_len - offset).min(PAGE);
        qspi_read(STAGING + offset, &mut buf[..n.div_ceil(4) as usize]);
        hash.update(&bytes(&buf)[..n as usize]);
        offset += n;
        feed_watchdog();
    }
    if hash.finalize()[..] != t[32..64] {
        return false;
    }
    // every page but the first, then the first: from then on the bootloader starts the new app
    let pages = total.div_ceil(PAGE);
    (1..pages).all(|p| copy_page(p, total, &mut buf)) && copy_page(0, total, &mut buf)
}

/// Bring page `p` of the app flash in line with the image; false if it does not read back.
fn copy_page(p: u32, total: u32, buf: &mut [u32; WORDS]) -> bool {
    feed_watchdog();
    let n = (total - p * PAGE).min(PAGE);
    buf.fill(0xFFFF_FFFF);
    qspi_read(STAGING + p * PAGE, &mut buf[..(n / 4) as usize]);
    let addr = APP_START + p * PAGE;
    let flash = |i: usize| unsafe { core::ptr::read_volatile((addr as *const u32).add(i)) };
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

fn bytes(words: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding and is at least as aligned as u8
    unsafe { core::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 4) }
}

/// The QSPI flash as the firmware sets it up (P0.12 powers it; SCK P1.14, CSN P1.15, IO0-3 P1.12
/// P1.13 P0.07 P0.05; FAST_READ at 8 MHz).
fn qspi_init() {
    pac::P0.outset().write(|w| w.set_pin(12, true));
    pac::P0.pin_cnf(12).write(|w| w.set_dir(pac::gpio::vals::Dir::Output));
    cortex_m::asm::delay(64_000 * 10); // 10 ms for the flash to power up
    let q = pac::QSPI;
    for (port, pin) in [(1, 14), (1, 15), (1, 12), (1, 13), (0, 7), (0, 5)] {
        let gpio = if port == 0 { pac::P0 } else { pac::P1 };
        gpio.outset().write(|w| w.set_pin(pin, true));
        gpio.pin_cnf(pin).write(|w| {
            w.set_dir(pac::gpio::vals::Dir::Output);
            w.set_drive(pac::gpio::vals::Drive::H0h1);
        });
    }
    let psel = |port: u8, pin: u8| {
        let mut p = pac::shared::regs::Psel(0);
        p.set_pin(pin);
        p.set_port(port == 1);
        p.set_connect(pac::shared::vals::Connect::Connected);
        p
    };
    q.psel().sck().write_value(psel(1, 14));
    q.psel().csn().write_value(psel(1, 15));
    q.psel().io0().write_value(psel(1, 12));
    q.psel().io1().write_value(psel(1, 13));
    q.psel().io2().write_value(psel(0, 7));
    q.psel().io3().write_value(psel(0, 5));
    q.ifconfig0().write(|w| {
        w.set_addrmode(pac::qspi::vals::Addrmode::_24bit);
        w.set_dpmenable(false);
        w.set_ppsize(pac::qspi::vals::Ppsize::_256bytes);
        w.set_readoc(pac::qspi::vals::Readoc::Fastread);
        w.set_writeoc(pac::qspi::vals::Writeoc::Pp);
    });
    q.ifconfig1().write(|w| {
        w.set_sckdelay(80);
        w.set_dpmen(false);
        w.set_spimode(pac::qspi::vals::Spimode::Mode0);
        w.set_sckfreq(3); // 32 MHz / (3 + 1)
    });
    q.iftiming().write(|w| w.set_rxdelay(2));
    q.xipoffset().write_value(0);
    q.enable().write(|w| w.set_enable(true));
    q.events_ready().write_value(0);
    q.tasks_activate().write_value(1);
    while q.events_ready().read() == 0 {}
}

fn qspi_read(addr: u32, dst: &mut [u32]) {
    let q = pac::QSPI;
    q.read().src().write_value(addr);
    q.read().dst().write_value(dst.as_mut_ptr() as u32);
    q.read().cnt().write(|w| w.set_cnt(dst.len() as u32 * 4));
    q.events_ready().write_value(0);
    q.tasks_readstart().write_value(1);
    while q.events_ready().read() == 0 {}
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
