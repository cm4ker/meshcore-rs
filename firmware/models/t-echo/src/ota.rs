//! Firmware updates over the app link (BLE, USB) and, on the repeater, over LoRa, as on the other
//! boards. The nRF52840 has room for one firmware only, so the image goes to a staging area in the
//! QSPI flash, is verified there, and at the next reset the installer (`installer/`, kept by this
//! firmware at [`INSTALLER`]) swaps it with the app, so the firmware that ran stays in the staging
//! area; a power loss during the swap only makes it go on at the next boot.
//!
//! The update then runs on trial ([`check_trial`]): after [`MAX_TRIAL_BOOTS`] boots that never
//! ran long enough to be kept ([`confirm`]), the firmware in the staging area goes back on, as
//! with `fw rollback` ([`rollback`]). It stays there until the next update takes the staging area
//! over. The firmware rolled back from is not installed by itself again, as on the other boards.
//!
//! Images are the app's flash contents followed by a trailer (`meshcore::ota::ImageFormat::Nrf`,
//! written by `tools/uf2/elf2uf2.py`), which is flashed with the app: the running firmware finds
//! its own image by it, the base of delta updates.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_nrf::pac;
use embassy_nrf::qspi::Qspi;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use meshcore::ota::{FirmwareTarget, ImageFormat, NRF_CHIP_NRF52840, NRF_TRAILER_LEN, NRF_TRAILER_MAGIC};
use meshcore_fw_core::storage::{Blob, SharedStore, Store};
use sha2::{Digest, Sha256};

// shared with the installer (installer/src/lib.rs)
pub const APP_START: u32 = 0x26000;
pub const INSTALLER: u32 = 0xED000;
/// The staging area in the QSPI flash, after the companion's region, up to the repeater's sensors
/// (`DATA_OVERFLOW`); then the page the installer takes the app's first page from, and the install
/// request.
const STAGING: u32 = 0x11_0000;
const STAGING_END: u32 = crate::DATA_OVERFLOW.0;
const SAVED_FIRST: u32 = 0x1F_E000;
const RECORD: u32 = 0x1F_F000;
const RECORD_MAGIC: &[u8; 8] = b"MCRS-IN2";

const PAGE: u32 = 4096;
/// The Adafruit bootloader's settings page: `bank_0` (0x0001 = valid app), then `bank_0_crc`.
const BOOTLOADER_SETTINGS: u32 = 0xFF000;

pub const MAX_TRIAL_BOOTS: u8 = 3;

static INSTALLER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/installer.bin"));

type QspiStore = Store<Qspi<'static>>;

/// An update is staged and verified: the reboot hands over to the installer ([`hand_over`]).
pub static INSTALL_PENDING: AtomicBool = AtomicBool::new(false);
/// The staging area still holds the firmware to go back to: no update has come into it since.
pub static PREVIOUS_KEPT: AtomicBool = AtomicBool::new(false);

pub struct StagingTarget {
    store: SharedStore<Qspi<'static>>,
}

impl StagingTarget {
    /// `None` if the bootloader would not start an app it did not write itself.
    pub fn new(store: SharedStore<Qspi<'static>>) -> Option<Self> {
        no_app_crc().then_some(Self { store })
    }
}

/// The bootloader starts whatever app is in place: it checks a CRC of the app only when its
/// settings hold one (a UF2 copy leaves it out, 0; BLE DFU writes one).
fn no_app_crc() -> bool {
    let settings = unsafe { core::ptr::read_volatile(BOOTLOADER_SETTINGS as *const u32) };
    let crc = settings >> 16;
    crc == 0 || crc == 0xFFFF
}

/// The watchdog, while a long flash job (inflating, patching) holds up the main loop.
fn feed_watchdog() {
    pac::WDT.rr(0).write(|w| w.set_rr(pac::wdt::vals::Rr::Reload));
}

fn as_bytes_mut(words: &mut [u32]) -> &mut [u8] {
    // SAFETY: u32 has no padding and is at least as aligned as u8
    unsafe { core::slice::from_raw_parts_mut(words.as_mut_ptr() as *mut u8, words.len() * 4) }
}

/// `out.len()` bytes of the QSPI flash from `addr`, any alignment.
fn qspi_read(store: &mut QspiStore, addr: u32, out: &mut [u8]) -> bool {
    let mut buf = [0u32; 256];
    // from the word holding `addr`, a word-aligned kilobyte at a time
    let mut at = addr & !3;
    let mut skip = (addr & 3) as usize;
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

/// `data` into the erased QSPI flash at the word-aligned `addr`.
fn qspi_write(store: &mut QspiStore, addr: u32, data: &[u8]) -> bool {
    // the QSPI's DMA wants word-aligned RAM and whole words: through a buffer, padded
    let mut buf = [0u32; 64];
    data.chunks(256).enumerate().all(|(k, chunk)| {
        let words = chunk.len().div_ceil(4);
        buf[..words].fill(0xFFFF_FFFF);
        as_bytes_mut(&mut buf[..words])[..chunk.len()].copy_from_slice(chunk);
        NorFlash::write(store.flash(), addr + 256 * k as u32, as_bytes_mut(&mut buf[..words])).is_ok()
    })
}

/// `len` bytes of the app flash from `addr`.
fn app_bytes(addr: u32, len: u32) -> &'static [u8] {
    // SAFETY: inside the app flash, which is memory-mapped
    unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) }
}

/// The trailer of an image whose app is `app_len` bytes long, built for this board's flash:
/// the app's hash it names.
fn trailer_hash(t: &[u8], app_len: u32) -> Option<[u8; 32]> {
    let u32_at = |i: usize| u32::from_le_bytes(t[i..i + 4].try_into().unwrap());
    (&t[..8] == NRF_TRAILER_MAGIC && u32_at(8) == app_len && u32_at(12) == APP_START && u32_at(16) == NRF_CHIP_NRF52840)
        .then(|| t[32..64].try_into().unwrap())
}

/// The running image: its length and the app's hash its trailer names. The trailer is the first
/// one naming its own offset as the app's length, as `meshcore::ota` finds it.
fn running_image() -> Option<(u32, [u8; 32])> {
    let flash = app_bytes(APP_START, INSTALLER - APP_START);
    (0..flash.len() - NRF_TRAILER_LEN as usize)
        .step_by(4)
        .find_map(|at| Some((at as u32 + NRF_TRAILER_LEN, trailer_hash(&flash[at..at + NRF_TRAILER_LEN as usize], at as u32)?)))
}

/// The firmware in the staging area, if it is the `len`-byte one whose app `hash` names, whole.
fn staged(store: &mut QspiStore, len: u32, hash: &[u8; 32]) -> bool {
    let Some(app_len) = len.checked_sub(NRF_TRAILER_LEN).filter(|&l| l >= PAGE && l + NRF_TRAILER_LEN <= INSTALLER - APP_START) else {
        return false;
    };
    let mut t = [0u8; NRF_TRAILER_LEN as usize];
    if *hash == [0; 32] || !no_app_crc() || !qspi_read(store, STAGING + app_len, &mut t) || trailer_hash(&t, app_len) != Some(*hash) {
        return false;
    }
    let mut h = Sha256::new();
    let mut buf = [0u8; 1024];
    for at in (0..app_len).step_by(buf.len()) {
        let n = (app_len - at).min(buf.len() as u32) as usize;
        if !qspi_read(store, STAGING + at, &mut buf[..n]) {
            return false;
        }
        h.update(&buf[..n]);
    }
    h.finalize()[..] == hash[..]
}

/// `Blob::OtaTrial`: `[trial boots so far u8, 0xFF when not on trial, 0xFE when the firmware in
/// the staging area failed its trial boots][the release rolled back from u32 LE, 0 = none][the
/// app hash of the firmware on trial, first 8 bytes][the app hash of the firmware in the staging
/// area 32][its image's length u32 LE][its name, UTF-8, to the end]`.
#[derive(Default)]
pub struct FwRecord {
    pub trial: Option<u8>,
    /// The node went back by itself from the firmware in the staging area.
    pub failed: bool,
    pub skip: u32,
    pub trial_hash: [u8; 8],
    pub prev_hash: [u8; 32],
    pub prev_len: u32,
    pub prev: String,
}

const NO_TRIAL: u8 = 0xFF;
const FAILED: u8 = 0xFE;

impl FwRecord {
    pub fn load(store: &mut QspiStore) -> Self {
        let Some(r) = store.load(Blob::OtaTrial).filter(|r| r.len() >= 49) else {
            return Self::default();
        };
        Self {
            trial: Some(r[0]).filter(|&b| b < FAILED),
            failed: r[0] == FAILED,
            skip: u32::from_le_bytes(r[1..5].try_into().unwrap()),
            trial_hash: r[5..13].try_into().unwrap(),
            prev_hash: r[13..45].try_into().unwrap(),
            prev_len: u32::from_le_bytes(r[45..49].try_into().unwrap()),
            prev: String::from_utf8_lossy(&r[49..]).into_owned(),
        }
    }

    pub fn save(&self, store: &mut QspiStore) -> bool {
        let mut r = Vec::from([self.trial.unwrap_or(if self.failed { FAILED } else { NO_TRIAL })]);
        r.extend_from_slice(&self.skip.to_le_bytes());
        r.extend_from_slice(&self.trial_hash);
        r.extend_from_slice(&self.prev_hash);
        r.extend_from_slice(&self.prev_len.to_le_bytes());
        r.extend_from_slice(self.prev.as_bytes());
        store.save(Blob::OtaTrial, &r)
    }

    /// This firmware is left for the one in the staging area: not installed by itself again.
    fn left(own: Option<(u32, [u8; 32])>) -> Self {
        let (prev_len, prev_hash) = own.unwrap_or_default();
        Self { skip: meshcore::release::release_counter(), prev_hash, prev_len, prev: own_label(), ..Self::default() }
    }
}

/// This firmware, as the record names it for after it is left: `0.8.6 #69`, or `0.8.6` on a local build.
fn own_label() -> String {
    meshcore::fwstate::Release::new(meshcore::release::PRODUCT_VERSION, meshcore::release::release_counter()).to_string()
}

/// Ask the installer to swap in the `size`-byte image in the staging area; `running` is this
/// firmware's length, all of which goes to the staging area. The app's first page waits for the
/// installer at [`SAVED_FIRST`]: [`hand_over`] points that page at the installer.
fn request(store: &mut QspiStore, size: u32, running: u32) -> bool {
    // `[magic 8][image length u32][its complement u32][running length u32][its complement u32]`
    let mut rec = [0u8; 24];
    rec[..8].copy_from_slice(RECORD_MAGIC);
    for (i, v) in [size, !size, running, !running].into_iter().enumerate() {
        rec[8 + 4 * i..12 + 4 * i].copy_from_slice(&v.to_le_bytes());
    }
    let ok = NorFlash::erase(store.flash(), SAVED_FIRST, SAVED_FIRST + PAGE).is_ok()
        && qspi_write(store, SAVED_FIRST, app_bytes(APP_START, PAGE))
        && NorFlash::erase(store.flash(), RECORD, RECORD + PAGE).is_ok()
        && qspi_write(store, RECORD, &rec);
    INSTALL_PENDING.fetch_or(ok, Ordering::Relaxed);
    ok
}

/// Run at boot, before anything that could hang. Returns `true` while on a trial boot.
/// Never returns if the trial boots are used up and there is a firmware to go back to.
pub fn check_trial(store: &SharedStore<Qspi<'static>>) -> bool {
    let mut s = store.borrow_mut();
    let mut record = FwRecord::load(&mut s);
    let Some(boots) = record.trial else {
        return false;
    };
    if running_image().is_none_or(|(_, h)| h[..8] != record.trial_hash) {
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
pub fn previous(store: &SharedStore<Qspi<'static>>) -> (bool, String, u32, bool) {
    let mut s = store.borrow_mut();
    let record = FwRecord::load(&mut s);
    let kept = staged(&mut s, record.prev_len, &record.prev_hash);
    PREVIOUS_KEPT.store(kept, Ordering::Relaxed);
    if record.failed {
        log::warn!("OTA: rolled back, {} did not start", record.prev);
    }
    (kept, record.prev, record.skip, record.failed)
}

/// The new firmware is healthy: keep it.
pub fn confirm(store: &SharedStore<Qspi<'static>>) {
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
pub fn rollback(store: &SharedStore<Qspi<'static>>) -> bool {
    go_back(store, false)
}

/// [`rollback`], also by itself after failed trial boots (`failed`).
fn go_back(store: &SharedStore<Qspi<'static>>, failed: bool) -> bool {
    let mut s = store.borrow_mut();
    let record = FwRecord::load(&mut s);
    if !staged(&mut s, record.prev_len, &record.prev_hash) {
        return false;
    }
    let own = running_image();
    FwRecord { failed, ..FwRecord::left(own) }.save(&mut s) && request(&mut s, record.prev_len, own.map_or(0, |(l, _)| l))
}

impl FirmwareTarget for StagingTarget {
    fn capacity(&self) -> u32 {
        STAGING_END - STAGING
    }

    // a staged update waits for the restart as verified: nothing may write over it
    fn erase(&mut self, offset: u32, len: u32) -> bool {
        feed_watchdog();
        if INSTALL_PENDING.load(Ordering::Relaxed) || offset + len > self.capacity() {
            return false;
        }
        // an update comes in over the firmware to go back to
        PREVIOUS_KEPT.store(false, Ordering::Relaxed);
        let from = STAGING + offset;
        NorFlash::erase(self.store.borrow_mut().flash(), from, from + len).is_ok()
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> bool {
        if INSTALL_PENDING.load(Ordering::Relaxed) || offset as usize + data.len() > self.capacity() as usize {
            return false;
        }
        qspi_write(&mut self.store.borrow_mut(), STAGING + offset, data)
    }

    fn read(&mut self, offset: u32, out: &mut [u8]) -> bool {
        offset as usize + out.len() <= self.capacity() as usize && qspi_read(&mut self.store.borrow_mut(), STAGING + offset, out)
    }

    fn running_capacity(&self) -> u32 {
        INSTALLER - APP_START
    }

    fn read_running(&mut self, offset: u32, buf: &mut [u8]) -> bool {
        if offset as usize + buf.len() > self.running_capacity() as usize {
            return false;
        }
        buf.copy_from_slice(app_bytes(APP_START + offset, buf.len() as u32));
        true
    }

    fn format(&self) -> ImageFormat {
        ImageFormat::Nrf { chip: NRF_CHIP_NRF52840, load: APP_START }
    }

    fn activate(&mut self, size: u32) -> bool {
        let mut store = self.store.borrow_mut();
        // the update runs on trial; this firmware is what it goes back to, should it fail
        let own = running_image();
        let mut trial_hash = [0u8; 8];
        if size < NRF_TRAILER_LEN || !qspi_read(&mut store, STAGING + size - NRF_TRAILER_LEN + 32, &mut trial_hash) {
            return false;
        }
        let record = FwRecord { trial: Some(0), skip: FwRecord::load(&mut store).skip, trial_hash, ..FwRecord::left(own) };
        record.save(&mut store) && request(&mut store, size, own.map_or(0, |(l, _)| l))
    }
}

/// At start: a request left behind has been carried out (or was never handed over, after a
/// crash): forget it.
pub fn clear_request(store: &SharedStore<Qspi<'static>>) {
    let mut head = [0u8; 4];
    let mut store = store.borrow_mut();
    if qspi_read(&mut store, RECORD, &mut head) && head != [0xFF; 4] {
        let _ = NorFlash::erase(store.flash(), RECORD, RECORD + PAGE);
        log::info!("OTA: install request cleared");
    }
}

/// Hand over to the installer: put it in place if this build carries another one, point the app's
/// first page at it, reset. Interrupts stay off from here: this firmware is gone once its first
/// page is (the installer has it at [`SAVED_FIRST`]).
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

// the swap takes the larger image's pages and a spare one; the sensors and the saved page stay
const _: () = assert!(STAGING_END - STAGING >= INSTALLER - APP_START + PAGE && STAGING_END + crate::DATA_OVERFLOW.1 <= SAVED_FIRST);
