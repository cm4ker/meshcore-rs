//! Firmware updates over LoRa (an admin's requests, a neighbour's delta for `fw.auto`), as on the
//! other boards. The board has its internal flash only: the image comes into a staging area of it
//! (memory.x), is verified there, and at the next reset the installer (`installer/`, kept by this
//! firmware at [`INSTALLER`]) copies it over the app; a power loss during the copy only makes it
//! start again. One firmware slot, no rollback, as on the T-Echo.
//!
//! Images are the app's flash contents followed by a trailer (`meshcore::ota::ImageFormat::Nrf`,
//! written by `tools/uf2/elf2uf2.py`), which is flashed with the app: the running firmware finds
//! its own image by it, the base of delta updates.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::pac;
use embedded_storage::nor_flash::NorFlash;
use meshcore::ota::{FirmwareTarget, ImageFormat, NRF_CHIP_NRF52840};
use meshcore_fw_core::storage::SharedStore;

// shared with the installer (installer/src/main.rs)
pub const APP_START: u32 = 0x26000;
const APP_END: u32 = 0x7B000;
const STAGING: u32 = 0x7B000;
const RECORD: u32 = 0xD9000;
pub const INSTALLER: u32 = 0xEA000;
const RECORD_MAGIC: &[u8; 8] = b"MCRS-INS";

const PAGE: u32 = 4096;
/// The Adafruit bootloader's settings page: `bank_0` (0x0001 = valid app), then `bank_0_crc`.
const BOOTLOADER_SETTINGS: u32 = 0xFF000;

static INSTALLER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/installer.bin"));

/// An update is staged and verified: the reboot hands over to the installer ([`hand_over`]).
pub static INSTALL_PENDING: AtomicBool = AtomicBool::new(false);

pub struct StagingTarget {
    store: SharedStore<Nvmc<'static>>,
}

impl StagingTarget {
    /// `None` if the bootloader would not start an app it did not write itself: it checks a CRC
    /// of the app when one was written over BLE DFU (UF2 copies leave the CRC out).
    pub fn new(store: SharedStore<Nvmc<'static>>) -> Option<Self> {
        let settings = unsafe { core::ptr::read_volatile(BOOTLOADER_SETTINGS as *const u32) };
        let crc = settings >> 16;
        (crc == 0 || crc == 0xFFFF).then_some(Self { store })
    }
}

/// The watchdog, while a long flash job (inflating, patching) holds up the main loop.
fn feed_watchdog() {
    pac::WDT.rr(0).write(|w| w.set_rr(pac::wdt::vals::Rr::Reload));
}

fn read_flash(addr: u32, out: &mut [u8]) {
    // SAFETY: inside the internal flash, which is memory-mapped
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len()) };
}

impl FirmwareTarget for StagingTarget {
    fn capacity(&self) -> u32 {
        RECORD - STAGING
    }

    // a staged update waits for the restart as verified: nothing may write over it
    fn erase(&mut self, offset: u32, len: u32) -> bool {
        if INSTALL_PENDING.load(Ordering::Relaxed) || offset + len > self.capacity() {
            return false;
        }
        let from = STAGING + offset;
        // a page takes ~85 ms with the CPU halted: the watchdog between pages
        (from..from + len).step_by(PAGE as usize).all(|page| {
            feed_watchdog();
            NorFlash::erase(self.store.borrow_mut().flash(), page, page + PAGE).is_ok()
        })
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> bool {
        if INSTALL_PENDING.load(Ordering::Relaxed) || offset as usize + data.len() > self.capacity() as usize {
            return false;
        }
        NorFlash::write(self.store.borrow_mut().flash(), STAGING + offset, data).is_ok()
    }

    fn read(&mut self, offset: u32, out: &mut [u8]) -> bool {
        if offset as usize + out.len() > self.capacity() as usize {
            return false;
        }
        read_flash(STAGING + offset, out);
        true
    }

    fn running_capacity(&self) -> u32 {
        APP_END - APP_START
    }

    fn read_running(&mut self, offset: u32, buf: &mut [u8]) -> bool {
        if offset as usize + buf.len() > self.running_capacity() as usize {
            return false;
        }
        read_flash(APP_START + offset, buf);
        true
    }

    fn format(&self) -> ImageFormat {
        ImageFormat::Nrf { chip: NRF_CHIP_NRF52840, load: APP_START }
    }

    fn activate(&mut self, size: u32) -> bool {
        // the request: `[magic 8][image length u32][its complement u32]`
        let mut rec = [0u8; 16];
        rec[..8].copy_from_slice(RECORD_MAGIC);
        rec[8..12].copy_from_slice(&size.to_le_bytes());
        rec[12..].copy_from_slice(&(!size).to_le_bytes());
        let mut store = self.store.borrow_mut();
        let ok = NorFlash::erase(store.flash(), RECORD, RECORD + PAGE).is_ok() && NorFlash::write(store.flash(), RECORD, &rec).is_ok();
        INSTALL_PENDING.fetch_or(ok, Ordering::Relaxed);
        ok
    }
}

/// At start: a request left behind has been carried out (or was never handed over, after a
/// crash): forget it.
pub fn clear_request(store: &SharedStore<Nvmc<'static>>) {
    let mut head = [0u8; 8];
    read_flash(RECORD, &mut head);
    if head == *RECORD_MAGIC {
        let _ = NorFlash::erase(store.borrow_mut().flash(), RECORD, RECORD + PAGE);
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
