//! Firmware updates over the app link (BLE, USB), as on the other boards (`tools/ota-app`). The
//! nRF52840 has room for one firmware only, so the image goes to a staging area in the QSPI flash,
//! is verified there, and at the next reset the installer (`installer/`, kept by this firmware at
//! [`INSTALLER`]) copies it over the app; a power loss during the copy only makes it start again.
//!
//! Images are the app's flash contents followed by a trailer (`meshcore::ota::ImageFormat::Nrf`,
//! written by `tools/uf2/elf2uf2.py`), which is flashed with the app: the running firmware finds
//! its own image by it, the base of delta updates.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_nrf::pac;
use embassy_nrf::qspi::Qspi;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use meshcore::ota::{FirmwareTarget, ImageFormat, NRF_CHIP_NRF52840};
use meshcore_fw_core::storage::SharedStore;

// shared with the installer (installer/src/main.rs)
pub const APP_START: u32 = 0x26000;
pub const INSTALLER: u32 = 0xED000;
/// The staging area in the QSPI flash, after the companion's region, then the install request.
const STAGING: u32 = 0x11_0000;
const RECORD: u32 = 0x1F_F000;
const RECORD_MAGIC: &[u8; 8] = b"MCRS-INS";

const PAGE: u32 = 4096;
/// The Adafruit bootloader's settings page: `bank_0` (0x0001 = valid app), then `bank_0_crc`.
const BOOTLOADER_SETTINGS: u32 = 0xFF000;

static INSTALLER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/installer.bin"));

/// An update is staged and verified: [`crate::platform::Nrf::reboot`] hands over to the installer.
pub static INSTALL_PENDING: AtomicBool = AtomicBool::new(false);

pub struct StagingTarget {
    store: SharedStore<Qspi<'static>>,
}

impl StagingTarget {
    /// `None` if the bootloader would not start an app it did not write itself: it checks a CRC
    /// of the app when one was written over BLE DFU (UF2 copies leave the CRC out).
    pub fn new(store: SharedStore<Qspi<'static>>) -> Option<Self> {
        let settings = unsafe { core::ptr::read_volatile(BOOTLOADER_SETTINGS as *const u32) };
        let crc = settings >> 16;
        (crc == 0 || crc == 0xFFFF).then_some(Self { store })
    }
}

/// The watchdog, while a long flash job (inflating, patching) holds up the main loop.
fn feed_watchdog() {
    pac::WDT.rr(0).write(|w| w.set_rr(pac::wdt::vals::Rr::Reload));
}

impl FirmwareTarget for StagingTarget {
    fn capacity(&self) -> u32 {
        RECORD - STAGING
    }

    // a staged update waits for the restart as verified: nothing may write over it
    fn erase(&mut self, offset: u32, len: u32) -> bool {
        feed_watchdog();
        if INSTALL_PENDING.load(Ordering::Relaxed) {
            return false;
        }
        let from = STAGING + offset;
        NorFlash::erase(self.store.borrow_mut().flash(), from, from + len).is_ok()
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> bool {
        if INSTALL_PENDING.load(Ordering::Relaxed) {
            return false;
        }
        // the QSPI's DMA wants word-aligned RAM and whole words: through a buffer, padded
        let mut buf = [0u32; 64];
        let mut store = self.store.borrow_mut();
        for (k, chunk) in data.chunks(256).enumerate() {
            let words = chunk.len().div_ceil(4);
            buf[..words].fill(0xFFFF_FFFF);
            as_bytes_mut(&mut buf[..words])[..chunk.len()].copy_from_slice(chunk);
            if NorFlash::write(store.flash(), STAGING + offset + 256 * k as u32, as_bytes_mut(&mut buf[..words])).is_err() {
                return false;
            }
        }
        true
    }

    fn read(&mut self, offset: u32, out: &mut [u8]) -> bool {
        let mut buf = [0u32; 256];
        let mut store = self.store.borrow_mut();
        // from the word holding `offset`, a word-aligned kilobyte at a time
        let mut at = (STAGING + offset) & !3;
        let mut skip = ((STAGING + offset) & 3) as usize;
        let mut done = 0;
        while done < out.len() {
            let want = (out.len() - done + skip).min(1024);
            let words = want.div_ceil(4);
            if ReadNorFlash::read(store.flash(), at, as_bytes_mut(&mut buf[..words])).is_err() {
                return false;
            }
            let n = want - skip;
            out[done..done + n].copy_from_slice(&as_bytes_mut(&mut buf[..words])[skip..want]);
            done += n;
            at += 4 * words as u32;
            skip = 0;
        }
        true
    }

    fn running_capacity(&self) -> u32 {
        INSTALLER - APP_START
    }

    fn read_running(&mut self, offset: u32, buf: &mut [u8]) -> bool {
        if offset as usize + buf.len() > self.running_capacity() as usize {
            return false;
        }
        // SAFETY: inside the app flash, which is memory-mapped
        unsafe { core::ptr::copy_nonoverlapping((APP_START + offset) as *const u8, buf.as_mut_ptr(), buf.len()) };
        true
    }

    fn format(&self) -> ImageFormat {
        ImageFormat::Nrf { chip: NRF_CHIP_NRF52840, load: APP_START }
    }

    fn activate(&mut self, size: u32) -> bool {
        // the request: `[magic 8][image length u32][its complement u32]`
        let mut rec = [0u32; 4];
        as_bytes_mut(&mut rec)[..8].copy_from_slice(RECORD_MAGIC);
        (rec[2], rec[3]) = (size, !size);
        let mut store = self.store.borrow_mut();
        let ok = NorFlash::erase(store.flash(), RECORD, RECORD + PAGE).is_ok() && NorFlash::write(store.flash(), RECORD, as_bytes_mut(&mut rec)).is_ok();
        INSTALL_PENDING.fetch_or(ok, Ordering::Relaxed);
        ok
    }
}

fn as_bytes_mut(words: &mut [u32]) -> &mut [u8] {
    // SAFETY: u32 has no padding and is at least as aligned as u8
    unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 4) }
}

/// At start: a request left in the staging area has been carried out (or was never handed over,
/// after a crash): forget it.
pub fn clear_request(store: &SharedStore<Qspi<'static>>) {
    let mut head = [0u32; 2];
    let mut store = store.borrow_mut();
    if ReadNorFlash::read(store.flash(), RECORD, as_bytes_mut(&mut head)).is_ok() && as_bytes_mut(&mut head)[..] == RECORD_MAGIC[..] {
        let _ = NorFlash::erase(store.flash(), RECORD, RECORD + PAGE);
        log::info!("OTA: install request cleared");
    }
}

/// Hand over to the installer: put it in place if this build carries another one, point the app's
/// first page at it, reset. Interrupts stay off from here: the old firmware is gone once its first
/// page is.
pub fn hand_over() -> ! {
    cortex_m::interrupt::disable();
    let flash = |addr: u32| unsafe { core::ptr::read_volatile(addr as *const u32) };
    let words = INSTALLER_BIN.len().div_ceil(4);
    let word = |i: usize| {
        let mut w = [0xFFu8; 4];
        let b = &INSTALLER_BIN[4 * i..(4 * i + 4).min(INSTALLER_BIN.len())];
        w[..b.len()].copy_from_slice(b);
        u32::from_le_bytes(w)
    };
    if (0..words).any(|i| flash(INSTALLER + 4 * i as u32) != word(i)) {
        for page in (INSTALLER..INSTALLER + 4 * words as u32).step_by(PAGE as usize) {
            nvmc_erase(page);
        }
        for i in 0..words {
            nvmc_write(INSTALLER + 4 * i as u32, word(i));
        }
        if (0..words).any(|i| flash(INSTALLER + 4 * i as u32) != word(i)) {
            // no installer to hand over to: this firmware starts again (and drops the request)
            cortex_m::peripheral::SCB::sys_reset();
        }
    }
    // the installer's stack pointer and reset vector; the stack pointer last, as it marks an app
    nvmc_erase(APP_START);
    nvmc_write(APP_START + 4, word(1));
    nvmc_write(APP_START, word(0));
    pac::POWER.gpregret().write(|w| w.set_gpregret(0));
    cortex_m::peripheral::SCB::sys_reset()
}

fn nvmc_erase(addr: u32) {
    feed_watchdog();
    pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Een));
    pac::NVMC.erasepage().write_value(addr);
    while !pac::NVMC.ready().read().ready() {}
    pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Ren));
}

fn nvmc_write(addr: u32, word: u32) {
    pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Wen));
    // SAFETY: a word-aligned address in the app or installer flash, erased just before
    unsafe { core::ptr::write_volatile(addr as *mut u32, word) };
    while !pac::NVMC.ready().read().ready() {}
    pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Ren));
}
