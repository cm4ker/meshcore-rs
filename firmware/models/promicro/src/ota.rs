//! Firmware updates over LoRa (an admin's requests, a neighbour's delta for `fw.auto`), as on the
//! other boards. The board has its internal flash only: the image comes into a staging area of it
//! (memory.x), is verified there, and at the next reset the installer (`installer/`, kept by this
//! firmware at [`INSTALLER`]) swaps it with the app, so the firmware that ran stays in the staging
//! area; a power loss during the swap only makes it go on at the next boot.
//!
//! The update then runs on trial ([`check_trial`]): after [`MAX_TRIAL_BOOTS`] boots that never
//! ran long enough to be kept ([`confirm`]), the firmware in the staging area goes back on, as
//! with `fw rollback` ([`rollback`]). It stays there until the next update takes the staging area
//! over. The firmware rolled back from is not installed by itself again, as on the ESP32 boards.
//!
//! Images are the app's flash contents followed by a trailer (`meshcore::ota::ImageFormat::Nrf`,
//! written by `tools/uf2/elf2uf2.py`), which is flashed with the app: the running firmware finds
//! its own image by it, the base of delta updates, and the one in the staging area by its own.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::pac;
use embedded_storage::nor_flash::NorFlash;
use meshcore::ota::{FirmwareTarget, ImageFormat, NRF_CHIP_NRF52840, NRF_TRAILER_LEN, NRF_TRAILER_MAGIC};
use meshcore_fw_core::storage::{Blob, SharedStore, Store};

// shared with the installer (installer/src/lib.rs)
pub const APP_START: u32 = 0x26000;
const APP_END: u32 = 0x7B000;
const STAGING: u32 = 0x7B000;
const SAVED_FIRST: u32 = 0xD8000;
const RECORD: u32 = 0xD9000;
pub const INSTALLER: u32 = 0xEA000;
const RECORD_MAGIC: &[u8; 8] = b"MCRS-IN2";

const PAGE: u32 = 4096;
/// The Adafruit bootloader's settings page: `bank_0` (0x0001 = valid app), then `bank_0_crc`.
const BOOTLOADER_SETTINGS: u32 = 0xFF000;

pub const MAX_TRIAL_BOOTS: u8 = 3;

static INSTALLER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/installer.bin"));

type NrfStore = Store<Nvmc<'static>>;

/// An update is staged and verified: the reboot hands over to the installer ([`hand_over`]).
pub static INSTALL_PENDING: AtomicBool = AtomicBool::new(false);
/// The staging area still holds the firmware to go back to: no update has come into it since.
pub static PREVIOUS_KEPT: AtomicBool = AtomicBool::new(false);

pub struct StagingTarget {
    store: SharedStore<Nvmc<'static>>,
}

impl StagingTarget {
    /// `None` if the bootloader would not start an app it did not write itself.
    pub fn new(store: SharedStore<Nvmc<'static>>) -> Option<Self> {
        no_app_crc().then_some(Self { store })
    }
}

/// The bootloader starts whatever app is in place: it checks a CRC of the app only when one was
/// written over BLE DFU (UF2 copies leave the CRC out).
fn no_app_crc() -> bool {
    let settings = unsafe { core::ptr::read_volatile(BOOTLOADER_SETTINGS as *const u32) };
    let crc = settings >> 16;
    crc == 0 || crc == 0xFFFF
}

/// The watchdog, while a long flash job (inflating, patching) holds up the main loop.
fn feed_watchdog() {
    pac::WDT.rr(0).write(|w| w.set_rr(pac::wdt::vals::Rr::Reload));
}

fn read_flash(addr: u32, out: &mut [u8]) {
    // SAFETY: inside the internal flash, which is memory-mapped
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, out.as_mut_ptr(), out.len()) };
}

/// `len` bytes of the internal flash from `addr`.
fn flash_bytes(addr: u32, len: u32) -> &'static [u8] {
    // SAFETY: inside the internal flash, which is memory-mapped
    unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) }
}

/// The image at `base` (the app flash or the staging area): its length and the app's hash its
/// trailer names. The trailer is the first one naming its own offset as the app's length, as
/// `meshcore::ota` finds the running image's.
fn image_at(base: u32) -> Option<(u32, [u8; 32])> {
    let flash = flash_bytes(base, APP_END - APP_START);
    let u32_at = |t: &[u8], i: usize| u32::from_le_bytes(t[i..i + 4].try_into().unwrap());
    (0..flash.len() - NRF_TRAILER_LEN as usize).step_by(4).find_map(|at| {
        let t = &flash[at..at + NRF_TRAILER_LEN as usize];
        (&t[..8] == NRF_TRAILER_MAGIC
            && u32_at(t, 8) == at as u32
            && u32_at(t, 12) == APP_START
            && u32_at(t, 16) == NRF_CHIP_NRF52840)
            .then(|| (at as u32 + NRF_TRAILER_LEN, t[32..].try_into().unwrap()))
    })
}

/// The firmware in the staging area, if it is the one `hash` names, whole: its length.
fn staged(hash: &[u8; 32]) -> Option<u32> {
    if *hash == [0; 32] || !no_app_crc() {
        return None;
    }
    let (len, named) = image_at(STAGING).filter(|(_, named)| named == hash)?;
    let mut actual = [0u8; 32];
    meshcore::crypto::sha256(&mut actual, &[flash_bytes(STAGING, len - NRF_TRAILER_LEN)]);
    (actual == named).then_some(len)
}

/// `Blob::OtaTrial`: `[trial boots so far u8, 0xFF when not on trial, 0xFE when the firmware in
/// the staging area failed its trial boots][the release rolled back from u32 LE, 0 = none][the
/// app hash of the firmware on trial, first 8 bytes][the app hash of the firmware in the staging
/// area 32][its name, UTF-8, to the end]`.
#[derive(Default)]
struct FwRecord {
    trial: Option<u8>,
    /// The node went back by itself from the firmware in the staging area.
    failed: bool,
    skip: u32,
    trial_hash: [u8; 8],
    prev_hash: [u8; 32],
    prev: String,
}

const NO_TRIAL: u8 = 0xFF;
const FAILED: u8 = 0xFE;

impl FwRecord {
    fn load(store: &mut NrfStore) -> Self {
        let Some(r) = store.load(Blob::OtaTrial).filter(|r| r.len() >= 45) else {
            return Self::default();
        };
        Self {
            trial: Some(r[0]).filter(|&b| b < FAILED),
            failed: r[0] == FAILED,
            skip: u32::from_le_bytes(r[1..5].try_into().unwrap()),
            trial_hash: r[5..13].try_into().unwrap(),
            prev_hash: r[13..45].try_into().unwrap(),
            prev: String::from_utf8_lossy(&r[45..]).into_owned(),
        }
    }

    fn save(&self, store: &mut NrfStore) -> bool {
        let mut r = Vec::from([self.trial.unwrap_or(if self.failed { FAILED } else { NO_TRIAL })]);
        r.extend_from_slice(&self.skip.to_le_bytes());
        r.extend_from_slice(&self.trial_hash);
        r.extend_from_slice(&self.prev_hash);
        r.extend_from_slice(self.prev.as_bytes());
        store.save(Blob::OtaTrial, &r)
    }

    /// This firmware is left for the one in the staging area: not installed by itself again.
    fn left(own: Option<(u32, [u8; 32])>) -> Self {
        Self {
            skip: meshcore::release::RELEASE_COUNTER,
            prev_hash: own.map_or([0; 32], |(_, h)| h),
            prev: own_label(),
            ..Self::default()
        }
    }
}

/// This firmware, as the record names it for after it is left: `0.8.3-dev.1 #45`.
fn own_label() -> String {
    match meshcore::release::RELEASE_COUNTER {
        0 => "a local build".into(),
        n => meshcore::fwstate::Release::new(meshcore::release::RELEASE_VERSION, n).to_string(),
    }
}

/// Ask the installer to swap in the `size`-byte image in the staging area; `running` is this
/// firmware's length, all of which goes to the staging area.
fn request(store: &mut NrfStore, size: u32, running: u32) -> bool {
    // `[magic 8][image length u32][its complement u32][running length u32][its complement u32]`
    let mut rec = [0u8; 24];
    rec[..8].copy_from_slice(RECORD_MAGIC);
    for (i, v) in [size, !size, running, !running].into_iter().enumerate() {
        rec[8 + 4 * i..12 + 4 * i].copy_from_slice(&v.to_le_bytes());
    }
    let ok = NorFlash::erase(store.flash(), RECORD, RECORD + PAGE).is_ok() && NorFlash::write(store.flash(), RECORD, &rec).is_ok();
    INSTALL_PENDING.fetch_or(ok, Ordering::Relaxed);
    ok
}

/// Run at boot, before anything that could hang. Returns `true` while on a trial boot.
/// Never returns if the trial boots are used up and there is a firmware to go back to.
pub fn check_trial(store: &SharedStore<Nvmc<'static>>) -> bool {
    let mut s = store.borrow_mut();
    let mut record = FwRecord::load(&mut s);
    let Some(boots) = record.trial else {
        return false;
    };
    let own = image_at(APP_START);
    if own.is_none_or(|(_, h)| h[..8] != record.trial_hash) {
        // the installer found nothing valid and kept this firmware
        log::warn!("OTA: the update was not installed");
        record.trial = None;
        record.save(&mut s);
        return false;
    }
    if boots >= MAX_TRIAL_BOOTS {
        log::error!("OTA: the update failed {boots} trial boots, going back");
        drop(s);
        if go_back(store, true) {
            hand_over();
        }
        log::error!("OTA: no firmware to go back to, keeping this one");
        let mut s = store.borrow_mut();
        record.trial = None;
        record.save(&mut s);
        return false;
    }
    record.trial = Some(boots + 1);
    record.save(&mut s);
    log::info!("OTA: trial boot {} of {MAX_TRIAL_BOOTS}", boots + 1);
    true
}

/// What `fw` and `fw rollback` know of the firmware to go back to: whether the staging area still
/// holds it, its name, the release rolled back from, and whether it failed its trial boots.
pub fn previous(store: &SharedStore<Nvmc<'static>>) -> (bool, String, u32, bool) {
    let record = FwRecord::load(&mut store.borrow_mut());
    let kept = staged(&record.prev_hash).is_some();
    PREVIOUS_KEPT.store(kept, Ordering::Relaxed);
    if record.failed {
        log::warn!("OTA: rolled back, {} did not start", record.prev);
    }
    (kept, record.prev, record.skip, record.failed)
}

/// The new firmware is healthy: keep it.
pub fn confirm(store: &SharedStore<Nvmc<'static>>) {
    if INSTALL_PENDING.load(Ordering::Relaxed) {
        return; // the record is the next update's trial already
    }
    let mut s = store.borrow_mut();
    let mut record = FwRecord::load(&mut s);
    record.trial = None;
    record.save(&mut s);
}

/// Go back to the firmware in the staging area (`fw rollback`); the restart is the caller's, and
/// hands over to the installer.
pub fn rollback(store: &SharedStore<Nvmc<'static>>) -> bool {
    go_back(store, false)
}

/// [`rollback`], also by itself after failed trial boots (`failed`).
fn go_back(store: &SharedStore<Nvmc<'static>>, failed: bool) -> bool {
    let mut s = store.borrow_mut();
    let Some(len) = staged(&FwRecord::load(&mut s).prev_hash) else {
        return false;
    };
    let own = image_at(APP_START);
    FwRecord { failed, ..FwRecord::left(own) }.save(&mut s) && request(&mut s, len, own.map_or(0, |(l, _)| l))
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
        // an update comes in over the firmware to go back to
        PREVIOUS_KEPT.store(false, Ordering::Relaxed);
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
        let mut store = self.store.borrow_mut();
        // the update runs on trial; this firmware is what it goes back to, should it fail
        let own = image_at(APP_START);
        let mut trial_hash = [0u8; 8];
        read_flash(STAGING + size - NRF_TRAILER_LEN + 32, &mut trial_hash);
        let record = FwRecord {
            trial: Some(0),
            skip: FwRecord::load(&mut store).skip,
            trial_hash,
            ..FwRecord::left(own)
        };
        record.save(&mut store) && request(&mut store, size, own.map_or(0, |(l, _)| l))
    }
}

/// At start: a request left behind has been carried out (or was never handed over, after a
/// crash): forget it.
pub fn clear_request(store: &SharedStore<Nvmc<'static>>) {
    let mut head = [0u8; 4];
    read_flash(RECORD, &mut head);
    if head != [0xFF; 4] {
        let _ = NorFlash::erase(store.borrow_mut().flash(), RECORD, RECORD + PAGE);
        log::info!("OTA: install request cleared");
    }
}

/// Hand over to the installer: put it in place if this build carries another one, save the app's
/// first page for it, point that page at it, reset. Interrupts stay off from here: this firmware
/// is gone once its first page is.
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
    // the first page goes to the staging area with the rest of this firmware
    let first = |i: u32| flash(APP_START + 4 * i);
    if (0..PAGE / 4).any(|i| flash(SAVED_FIRST + 4 * i) != first(i)) {
        nvmc_erase(SAVED_FIRST);
        for i in (0..PAGE / 4).filter(|&i| first(i) != 0xFFFF_FFFF) {
            nvmc_write(SAVED_FIRST + 4 * i, first(i));
        }
        if (0..PAGE / 4).any(|i| flash(SAVED_FIRST + 4 * i) != first(i)) {
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
    // SAFETY: a word-aligned address in the app, staging or installer flash, erased just before
    unsafe { core::ptr::write_volatile(addr as *mut u32, word) };
    while !pac::NVMC.ready().read().ready() {}
    pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Ren));
}
