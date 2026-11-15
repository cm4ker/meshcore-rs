//! Firmware updates (an admin's requests over LoRa, a neighbour's delta for `fw.auto`, the BLE
//! window, the companion's app link), as on the other boards, applied in place: the boards have
//! their internal flash only, and an app slot beside a staging area as large would cap the app at
//! 340 KiB. A delta comes into the staging area as a plan (`meshcore_inplace`, which the protocol
//! core makes as it comes: `FirmwareTarget::in_place_shift`). Once the image it makes has checked
//! out, the plan is checked against the running firmware once more, and turned around into the
//! plan back to it, which is checked too and stays beside it. At the next reset the installer
//! (`installer/`, kept by this firmware at [`INSTALLER`]) moves the running firmware up by
//! [`SHIFT`] and writes the new one over it, a page at a time; a power loss only makes it go on at
//! the next boot. A whole image does not fit: it goes on as UF2 over USB.
//!
//! The update then runs on trial: after [`MAX_TRIAL_BOOTS`] boots that never ran long enough to be
//! kept, the plan back goes on the same way, as with `fw rollback`. It stays until the next update
//! comes into the staging area (on the companion, until its data comes back there after the
//! trial). The firmware rolled back from is not installed by itself again, as on the ESP32 boards.
//!
//! The Pro Micro took its updates by swapping them with the app before ([`IN_PLACE_AFTER_SWAP`]):
//! its app slot ended at 0x7B000, where the staging area began. The update to the first firmware
//! with updates in place came that way, and the one it replaced waits whole in that staging area,
//! until an update or the companion's data comes over it: going back to it is a swap again, which
//! the Pro Micro's installer still does (`installer/src/lib.rs`).
//!
//! Images are the app's flash contents followed by a trailer (`meshcore::ota::ImageFormat::Nrf`,
//! written by `tools/uf2/elf2uf2.py`), which is flashed with the app: the running firmware finds
//! its own image by it, the base of delta updates.
//!
//! Layout (memory.x): the app 0x26000..0x8F000; the staging area 0x8F000..0xD6000, the plan from
//! its start, the plan back from the page after it; the companion keeps its data there and lends
//! it to its updates (to 0xD1000, `crate::COMPANION_REGION`); then the sensors' blob, the app's
//! first page while the installer works, the install request, the node's data, the installer.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_nrf::pac;
use embedded_storage::nor_flash::NorFlash;
use meshcore::ota::{FirmwareTarget, ImageFormat, NRF_CHIP_NRF52840, NRF_TRAILER_LEN, NRF_TRAILER_MAGIC};
use meshcore_fw_core::storage::{Blob, SharedStore, Store};
use meshcore_inplace::{Header, PAGE, Rebuilt, make_page, request_words};
use sha2::{Digest, Sha256};

use crate::flash::Flash;

// shared with the installer (installer/src/main.rs)
pub const APP_START: u32 = 0x26000;
pub const APP_END: u32 = 0x8F000;
const STAGING: u32 = 0x8F000;
/// Up to the sensors' blob.
const STAGING_END: u32 = crate::DATA_OVERFLOW.0;
pub const SAVED_FIRST: u32 = 0xD8000;
pub const RECORD: u32 = 0xD9000;
pub const INSTALLER: u32 = 0xEA000;
/// How far the old firmware moves up first: what code moves by between releases, with room to
/// spare (a page of the new image refers only to old data at least this far above it, less a page).
pub const SHIFT: u32 = 32 * 1024;

// the Pro Micro's layout before updates in place (installer/src/lib.rs)
const SWAP_APP_END: u32 = 0x7B000;
const SWAP_STAGING: u32 = 0x7B000;
const SWAP_MAGIC: &[u8; 8] = b"MCRS-IN2";

/// The Adafruit bootloader's settings page: `bank_0` (0x0001 = valid app), then `bank_0_crc`.
const BOOTLOADER_SETTINGS: u32 = 0xFF000;

pub const MAX_TRIAL_BOOTS: u8 = 3;

static INSTALLER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/installer.bin"));

type NrfStore = Store<Flash>;

/// An update is staged and checked: the reboot hands over to the installer.
static INSTALL_PENDING: AtomicBool = AtomicBool::new(false);
/// The way back to the firmware that ran before is still there: no update has come in over it.
static PREVIOUS_KEPT: AtomicBool = AtomicBool::new(false);

/// The bootloader starts whatever app is in place: it checks a CRC of the app only when its
/// settings hold one (UF2 copies and 0.9.2-OTAFIX's serial DFU leave it out, 0).
pub fn no_app_crc() -> bool {
    let settings = unsafe { core::ptr::read_volatile(BOOTLOADER_SETTINGS as *const u32) };
    let crc = settings >> 16;
    crc == 0 || crc == 0xFFFF
}

/// The watchdog, while a long flash job (inflating, patching) holds up the main loop.
pub fn feed_watchdog() {
    pac::WDT.rr(0).write(|w| w.set_rr(pac::wdt::vals::Rr::Reload));
}

/// `len` bytes of the internal flash from `addr`.
fn flash_bytes(addr: u32, len: u32) -> &'static [u8] {
    // SAFETY: inside the internal flash, which is memory-mapped
    unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) }
}

/// The SHA-256 of `len` bytes of the internal flash from `addr`.
fn sha256_at(addr: u32, len: u32) -> [u8; 32] {
    let mut h = Sha256::new();
    for part in flash_bytes(addr, len).chunks(PAGE as usize) {
        feed_watchdog();
        h.update(part);
    }
    h.finalize().into()
}

/// The image in the `len` bytes from `base`: its length and the app's hash its trailer names. The
/// trailer is the first one naming its own offset as the app's length, as `meshcore::ota` finds
/// the running image's.
fn image_in(base: u32, len: u32) -> Option<(u32, [u8; 32])> {
    let flash = flash_bytes(base, len);
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

/// The running firmware: its length and the app's hash.
fn own() -> Option<(u32, [u8; 32])> {
    image_in(APP_START, APP_END - APP_START)
}

/// `Blob::OtaTrial`: `[trial boots so far u8, 0xFF when not on trial, 0xFE when the firmware gone
/// back from failed its trial boots][the release rolled back from u32 LE, 0 = none][the app hash
/// of the firmware on trial, first 8 bytes][the app hash of the firmware to go back to 32][its
/// name, UTF-8, to the end]`.
#[derive(Default)]
struct FwRecord {
    trial: Option<u8>,
    /// The node went back by itself from the firmware it was given.
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

    /// This firmware is left for another: not installed by itself again.
    fn left(own: Option<(u32, [u8; 32])>) -> Self {
        Self {
            skip: meshcore::release::release_counter(),
            prev_hash: own.map_or([0; 32], |(_, h)| h),
            prev: own_label(),
            ..Self::default()
        }
    }
}

/// This firmware, as the record names it for after it is left: `0.8.6 #69`, or `0.8.6` on a local build.
fn own_label() -> String {
    meshcore::fwstate::Release::new(meshcore::release::PRODUCT_VERSION, meshcore::release::release_counter()).to_string()
}

/// The plan at `at` in the staging area.
fn plan_at(at: u32) -> Option<(Header, &'static [u8])> {
    let h = Header::parse(flash_bytes(STAGING + at, PAGE))?;
    (at + h.len <= STAGING_END - STAGING).then(|| (h, flash_bytes(STAGING + at, h.len)))
}

/// Where the plan back starts: the page after the plan.
fn back_at(plan: &Header) -> u32 {
    plan.len.next_multiple_of(PAGE)
}

/// Make every page of the image `plan` makes, reading the image it is made from with `source`;
/// `page` gets them in turn. `None`: the plan does not hold together.
fn make_all(h: &Header, plan: &[u8], source: &mut impl FnMut(u32, &mut [u8]) -> bool, mut page: impl FnMut(u32, &[u8])) -> Option<()> {
    let mut buf = [0u8; PAGE as usize];
    for p in 0..h.pages {
        feed_watchdog();
        let (from, to) = h.record(plan, p)?;
        make_page(h, p, &plan[from as usize..to as usize], source, &mut buf)?;
        page(p, &buf[..h.page_len(p) as usize]);
    }
    Some(())
}

/// The running firmware read where it is.
fn running(at: u32, out: &mut [u8]) -> bool {
    out.copy_from_slice(flash_bytes(APP_START + at, out.len() as u32));
    true
}

/// Ask the installer for the request `words` (the plan's, or the swap's).
fn request(store: &mut NrfStore, words: &[u32]) -> bool {
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let ok = NorFlash::erase(store.flash(), RECORD, RECORD + PAGE).is_ok() && NorFlash::write(store.flash(), RECORD, &bytes).is_ok();
    INSTALL_PENDING.fetch_or(ok, Ordering::Relaxed);
    ok
}

/// The request for the plan at `at` in the staging area, `len` bytes long, over the running
/// firmware of `old_len` bytes.
fn plan_request(store: &mut NrfStore, at: u32, len: u32, old_len: u32) -> bool {
    request(store, &request_words(at, len, old_len))
}

/// The request to swap the `size`-byte firmware in the staging area of before back in, over this
/// one of `running` bytes: `[magic 8][image length u32][its complement u32][running length
/// u32][its complement u32]`.
fn swap_request(store: &mut NrfStore, size: u32, running: u32) -> bool {
    let magic = |i: usize| u32::from_le_bytes(SWAP_MAGIC[4 * i..4 * i + 4].try_into().unwrap());
    request(store, &[magic(0), magic(1), size, !size, running, !running])
}

/// The way back to the firmware that ran before this one.
enum WayBack {
    /// The plan back at this offset of the staging area: the plan there made this very firmware,
    /// and the plan back after it goes from it.
    Plan(u32, Header),
    /// The firmware the swap of before left in its staging area, this long.
    Swap(u32),
}

fn way_back(after_swap: bool, record: &FwRecord) -> Option<WayBack> {
    let plan = || {
        let (plan, _) = plan_at(0)?;
        let (len, _) = own()?;
        let at = back_at(&plan);
        let (back, _) = plan_at(at)?;
        let fits = plan.new_len == len && back.old_len == len && back.new_len == plan.old_len;
        (fits && plan.new_sha256 == sha256_at(APP_START, len)).then_some(WayBack::Plan(at, back))
    };
    // whole, the one the record names
    let swapped = || {
        let hash = record.prev_hash;
        let (len, _) = image_in(SWAP_STAGING, SWAP_APP_END - APP_START).filter(|&(_, named)| named == hash && hash != [0; 32])?;
        (sha256_at(SWAP_STAGING, len - NRF_TRAILER_LEN) == hash).then_some(WayBack::Swap(len))
    };
    plan().or_else(|| after_swap.then(swapped).flatten())
}

/// Where an update's plan comes in. `LENDS`: the companion's, whose data the staging area holds
/// and moves aside as the update begins (a type parameter, so the repeater carries none of the
/// lending code).
struct InPlaceTarget<const LENDS: bool> {
    store: SharedStore<Flash>,
}

impl<const LENDS: bool> InPlaceTarget<LENDS> {
    /// The staging area is the update's from here on.
    fn claim(&mut self) -> bool {
        if !LENDS {
            return true;
        }
        let mut store = self.store.borrow_mut();
        if store.lent() {
            return true;
        }
        log::info!("OTA: the companion's data moves aside for the update");
        store.lend()
    }

    /// The staged plan, checked against the running firmware: the image it makes, page by page as
    /// the installer will, must hash as the plan says (the protocol core checked that hash against
    /// the release as the plan came). Returns the plan's header and the new app's hash (its first 8
    /// bytes, as the trial record keeps it).
    fn check_plan(&self, size: u32) -> Option<(Header, [u8; 8])> {
        let (h, plan) = plan_at(0)?;
        let (old_len, _) = own()?;
        if h.new_len != size || h.old_len != old_len || h.len > self.capacity() || h.shift != SHIFT {
            return None;
        }
        let mut hash = Sha256::new();
        let mut app_hash = [0u8; 8];
        make_all(&h, plan, &mut running, |p, page| {
            hash.update(page);
            // the trailer's app hash, wherever the page boundary cuts it
            let at = p * PAGE;
            for i in 0..8u32 {
                let k = size - NRF_TRAILER_LEN + 32 + i;
                if (at..at + page.len() as u32).contains(&k) {
                    app_hash[i as usize] = page[(k - at) as usize];
                }
            }
        })?;
        (hash.finalize()[..] == h.new_sha256).then_some((h, app_hash))
    }

    /// The plan back from the image the staged plan makes to the running firmware, written after
    /// it and checked: made from the new image (as the plan makes it), it must hash as the running
    /// firmware does. `false`: no room for it, or it does not hold together.
    fn plan_back(&mut self, plan: &Header) -> bool {
        let (forward, old_len) = (flash_bytes(STAGING, plan.len), plan.old_len);
        let old_sha = sha256_at(APP_START, old_len);
        let at = back_at(plan);
        let room = self.capacity();
        let mut store = self.store.borrow_mut();
        // the records from the plan back's second page on, a sector at a time
        let (mut sector, mut written, mut ok) = (Vec::with_capacity(PAGE as usize), 0u32, true);
        let mut put = |r: &[u8], sector: &mut Vec<u8>, written: &mut u32, flush: bool| -> bool {
            let mut r = r;
            while !r.is_empty() || (flush && !sector.is_empty()) {
                let n = r.len().min(PAGE as usize - sector.len());
                sector.extend_from_slice(&r[..n]);
                r = &r[n..];
                if sector.len() == PAGE as usize || (flush && r.is_empty()) {
                    let to = STAGING + at + PAGE + *written;
                    sector.resize(sector.len().next_multiple_of(4), 0xFF);
                    feed_watchdog();
                    if to + PAGE > STAGING + room
                        || NorFlash::erase(store.flash(), to, to + PAGE).is_err()
                        || NorFlash::write(store.flash(), to, sector).is_err()
                    {
                        return false;
                    }
                    *written += PAGE;
                    sector.clear();
                }
            }
            true
        };
        let first = meshcore_inplace::reverse(forward, flash_bytes(APP_START, old_len), old_sha, &mut |r| {
            ok = ok && put(r, &mut sector, &mut written, false);
            ok
        });
        let Some(first) = first.filter(|_| ok && put(&[], &mut sector, &mut written, true)) else {
            return false;
        };
        let start = STAGING + at;
        if start + PAGE > STAGING + room
            || NorFlash::erase(store.flash(), start, start + PAGE).is_err()
            || NorFlash::write(store.flash(), start, &first).is_err()
        {
            return false;
        }
        drop(store);
        // made from the new image as the plan makes it, the plan back must give this firmware
        let Some((back, back_plan)) = plan_at(at) else {
            return false;
        };
        let Some(mut new_image) = Rebuilt::new(forward, running) else {
            return false;
        };
        let mut hash = Sha256::new();
        let made = make_all(&back, back_plan, &mut |at, out| new_image.read(at, out), |_, page| hash.update(page));
        made.is_some() && hash.finalize()[..] == old_sha
    }
}

impl<const LENDS: bool> FirmwareTarget for InPlaceTarget<LENDS> {
    fn capacity(&self) -> u32 {
        if LENDS { crate::COMPANION_REGION.1 } else { STAGING_END - STAGING }
    }

    // a staged plan waits for the restart as checked: nothing may write over it
    fn erase(&mut self, offset: u32, len: u32) -> bool {
        if INSTALL_PENDING.load(Ordering::Relaxed) || offset + len > self.capacity() || !self.claim() {
            return false;
        }
        // an update comes in over the way back
        PREVIOUS_KEPT.store(false, Ordering::Relaxed);
        let from = STAGING + offset;
        // a page takes ~85 ms with the CPU halted: the watchdog between pages
        (from..from + len).step_by(PAGE as usize).all(|page| {
            feed_watchdog();
            NorFlash::erase(self.store.borrow_mut().flash(), page, page + PAGE).is_ok()
        })
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> bool {
        if INSTALL_PENDING.load(Ordering::Relaxed) || offset as usize + data.len() > self.capacity() as usize || !self.claim() {
            return false;
        }
        NorFlash::write(self.store.borrow_mut().flash(), STAGING + offset, data).is_ok()
    }

    fn read(&mut self, offset: u32, out: &mut [u8]) -> bool {
        if offset as usize + out.len() > self.capacity() as usize {
            return false;
        }
        out.copy_from_slice(flash_bytes(STAGING + offset, out.len() as u32));
        true
    }

    fn running_capacity(&self) -> u32 {
        APP_END - APP_START
    }

    fn read_running(&mut self, offset: u32, buf: &mut [u8]) -> bool {
        if offset as usize + buf.len() > self.running_capacity() as usize {
            return false;
        }
        buf.copy_from_slice(flash_bytes(APP_START + offset, buf.len() as u32));
        true
    }

    fn format(&self) -> ImageFormat {
        ImageFormat::Nrf { chip: NRF_CHIP_NRF52840, load: APP_START }
    }

    fn in_place_shift(&self) -> Option<u32> {
        Some(SHIFT)
    }

    fn activate(&mut self, size: u32) -> bool {
        let Some((plan, trial_hash)) = self.check_plan(size) else {
            log::warn!("OTA: the plan does not make the image it should");
            return false;
        };
        if !self.plan_back(&plan) {
            log::warn!("OTA: no room for the way back beside the plan (or it does not hold together): not installed");
            return false;
        }
        let own = own();
        let mut store = self.store.borrow_mut();
        // the update runs on trial; the plan back is the way to this firmware, should it fail
        let record = FwRecord { trial: Some(0), skip: FwRecord::load(&mut store).skip, trial_hash, ..FwRecord::left(own) };
        record.save(&mut store) && plan_request(&mut store, 0, plan.len, plan.old_len)
    }
}

/// At start: a request left behind has been carried out (or was never handed over, after a
/// crash): forget it.
fn clear_request(store: &SharedStore<Flash>) {
    if flash_bytes(RECORD, 4) != [0xFF; 4] {
        let _ = NorFlash::erase(store.borrow_mut().flash(), RECORD, RECORD + PAGE);
        log::info!("OTA: install request cleared");
    }
}

/// Hand over to the installer: put it in place if it is not there as this build carries it, save
/// the app's first page for it, point that page at it, reset. Interrupts stay off from here: this
/// firmware is gone once its first page is.
fn hand_over() -> ! {
    let installer = INSTALLER_BIN;
    // the flash controller is the SoftDevice's while it runs
    crate::sd::disable();
    cortex_m::interrupt::disable();
    let flash = |addr: u32| unsafe { core::ptr::read_volatile(addr as *const u32) };
    let words = installer.len().div_ceil(4);
    let word = |i: usize| {
        let mut w = [0xFFu8; 4];
        let b = &installer[4 * i..(4 * i + 4).min(installer.len())];
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
    // the first page waits for the installer where it looks for it
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

/// How the repeater and the companion take updates (see the module's top): [`IN_PLACE`] on the
/// T114, [`IN_PLACE_AFTER_SWAP`] on the Pro Micro.
pub trait Updates: Sync {
    /// At start: a request left behind has been carried out, or was never handed over.
    fn clear_request(&self, store: &SharedStore<Flash>);
    /// Run at boot, before anything that could hang: `true` while on a trial boot. Never returns
    /// if the trial boots are used up and there is a firmware to go back to.
    fn check_trial(&self, store: &SharedStore<Flash>) -> bool;
    /// Where an update comes in; `lends`: the companion's, its data lent to the update.
    fn target(&self, store: SharedStore<Flash>, lends: bool) -> Option<Box<dyn FirmwareTarget>>;
    /// What `fw` and `fw rollback` know of the firmware to go back to: whether there is a way back
    /// to it, its name, the release rolled back from, and whether it failed its trial boots.
    fn previous(&self, store: &SharedStore<Flash>) -> (bool, String, u32, bool);
    /// Go back to the firmware that ran before (`fw rollback`); the restart is the caller's, and
    /// hands over to the installer.
    fn rollback(&self, store: &SharedStore<Flash>) -> bool;
    /// The new firmware is healthy: keep it.
    fn confirm(&self, store: &SharedStore<Flash>);
    /// The way back is still there: no update has come in over it.
    fn previous_kept(&self) -> bool;
    /// An update is staged and checked: the reboot hands over to the installer.
    fn install_pending(&self) -> bool;
    fn hand_over(&self) -> !;
}

/// Updates applied in place; `after_swap`: the board took them by a swap before (the Pro Micro).
pub struct InPlace {
    after_swap: bool,
}

/// The T114's.
pub static IN_PLACE: InPlace = InPlace { after_swap: false };
/// The Pro Micro's: the way back from the first firmware with updates in place is the swap.
pub static IN_PLACE_AFTER_SWAP: InPlace = InPlace { after_swap: true };

impl InPlace {
    /// [`Updates::rollback`], also by itself after failed trial boots (`failed`).
    fn go_back(&self, store: &SharedStore<Flash>, failed: bool) -> bool {
        let mut s = store.borrow_mut();
        let Some(way) = way_back(self.after_swap, &FwRecord::load(&mut s)) else {
            return false;
        };
        let own = own();
        if !(FwRecord { failed, ..FwRecord::left(own) }).save(&mut s) {
            return false;
        }
        match way {
            WayBack::Plan(at, back) => plan_request(&mut s, at, back.len, back.old_len),
            WayBack::Swap(len) => swap_request(&mut s, len, own.map_or(0, |(l, _)| l)),
        }
    }
}

impl Updates for InPlace {
    fn clear_request(&self, store: &SharedStore<Flash>) {
        clear_request(store)
    }

    fn check_trial(&self, store: &SharedStore<Flash>) -> bool {
        let mut s = store.borrow_mut();
        let mut record = FwRecord::load(&mut s);
        let Some(boots) = record.trial else {
            return false;
        };
        if own().is_none_or(|(_, h)| h[..8] != record.trial_hash) {
            // the installer found nothing valid and kept this firmware
            log::warn!("OTA: the update was not installed");
            record.trial = None;
            record.save(&mut s);
            return false;
        }
        if boots >= MAX_TRIAL_BOOTS {
            log::error!("OTA: the update failed {boots} trial boots, going back");
            drop(s);
            if self.go_back(store, true) {
                hand_over();
            }
            log::error!("OTA: no way back, keeping this one");
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

    fn target(&self, store: SharedStore<Flash>, lends: bool) -> Option<Box<dyn FirmwareTarget>> {
        if !no_app_crc() {
            return None;
        }
        Some(match lends {
            true => Box::new(InPlaceTarget::<true> { store }),
            false => Box::new(InPlaceTarget::<false> { store }),
        })
    }

    fn previous(&self, store: &SharedStore<Flash>) -> (bool, String, u32, bool) {
        let record = FwRecord::load(&mut store.borrow_mut());
        let kept = way_back(self.after_swap, &record).is_some();
        PREVIOUS_KEPT.store(kept, Ordering::Relaxed);
        if record.failed {
            log::warn!("OTA: rolled back, {} did not start", record.prev);
        }
        (kept, record.prev, record.skip, record.failed)
    }

    fn rollback(&self, store: &SharedStore<Flash>) -> bool {
        self.go_back(store, false)
    }

    fn confirm(&self, store: &SharedStore<Flash>) {
        if INSTALL_PENDING.load(Ordering::Relaxed) {
            return; // the record is the next update's trial already
        }
        let mut s = store.borrow_mut();
        let mut record = FwRecord::load(&mut s);
        record.trial = None;
        record.save(&mut s);
    }

    fn previous_kept(&self) -> bool {
        PREVIOUS_KEPT.load(Ordering::Relaxed)
    }

    fn install_pending(&self) -> bool {
        INSTALL_PENDING.load(Ordering::Relaxed)
    }

    fn hand_over(&self) -> ! {
        hand_over()
    }
}

const _: () = assert!(crate::COMPANION_REGION.0 == STAGING && crate::COMPANION_REGION.1 >= meshcore_fw_core::storage::MIN_COMPANION_SIZE);
const _: () = assert!(APP_END - APP_START >= SHIFT + PAGE);
