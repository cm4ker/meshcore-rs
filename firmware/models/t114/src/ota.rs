//! The T114's firmware updates, applied in place: the board has its internal flash only, and an
//! app slot beside a staging area as large (the Pro Micro's way, `promicro::ota`) would cap the
//! app at 340 KiB. A delta comes into the staging area as a plan (`meshcore_inplace`, which the
//! protocol core makes as it comes: `FirmwareTarget::in_place_shift`); once the image it makes
//! has checked out, the plan is checked against the running firmware once more, and at the next
//! reset the installer (`installer/`) moves that firmware up by [`SHIFT`] and writes the new one
//! over it, a page at a time. A whole image does not fit: it goes on as UF2 over USB.
//!
//! The update runs on trial as on the Pro Micro, but a firmware that fails its trial boots stays:
//! there is no firmware to go back to.
//!
//! Layout (memory.x): the app 0x26000..0x99000; the staging area 0x99000..0xD6000, where the
//! companion keeps its data and lends it to its updates (to 0xD1000, as on the Pro Micro); from
//! 0xD6000 on as on the Pro Micro.

use alloc::boxed::Box;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

use embedded_storage::nor_flash::NorFlash;
use meshcore::ota::{FirmwareTarget, ImageFormat, NRF_CHIP_NRF52840, NRF_TRAILER_LEN};
use meshcore_fw_core::storage::SharedStore;
use meshcore_inplace::{Header, PAGE, make_page, request_words};
use promicro::flash::Flash;
use promicro::ota::{APP_START, FwRecord, MAX_TRIAL_BOOTS, RECORD, Updates, feed_watchdog, image_in, no_app_crc};
use sha2::{Digest, Sha256};

// shared with the installer (installer/src/main.rs)
pub const APP_END: u32 = 0x99000;
const STAGING: u32 = 0x99000;
const STAGING_END: u32 = promicro::DATA_OVERFLOW.0;
/// How far the old firmware moves up first: what code moves by between releases, with room to
/// spare (a page of the new image refers only to old data at least this far above it, less a page).
pub const SHIFT: u32 = 32 * 1024;

/// The companion's data: the staging area up to where its data waits while an update is there
/// (`promicro::COMPANION_ASIDE`), the smallest region the store takes.
pub const COMPANION_REGION: (u32, u32) = (STAGING, promicro::COMPANION_ASIDE[0].0 - STAGING);

static INSTALLER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/installer.bin"));

/// A plan is staged and checked: the reboot hands over to the installer.
static INSTALL_PENDING: AtomicBool = AtomicBool::new(false);

/// `len` bytes of the internal flash from `addr`.
fn flash_bytes(addr: u32, len: u32) -> &'static [u8] {
    // SAFETY: inside the internal flash, which is memory-mapped
    unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) }
}

/// The running firmware: its length and the app's hash.
fn own() -> Option<(u32, [u8; 32])> {
    image_in(APP_START, APP_END - APP_START)
}

/// Where an update's plan comes in. `LENDS`: the companion's, whose data the staging area holds
/// and moves aside as the update begins.
pub struct InPlaceTarget<const LENDS: bool> {
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

    /// The staged plan, checked against the running firmware: the image it makes from it, page
    /// by page as the installer will, must hash as the plan says (the protocol core checked that
    /// hash against the release as the plan came). Returns the plan's length and the new app's
    /// hash (its first 8 bytes, as the trial record keeps it).
    fn check_plan(&self, size: u32) -> Option<(u32, [u8; 8])> {
        let first = flash_bytes(STAGING, PAGE);
        let h = Header::parse(first)?;
        let (old_len, _) = own()?;
        if h.new_len != size || h.old_len != old_len || h.len > self.capacity() || h.shift != SHIFT {
            return None;
        }
        let plan = flash_bytes(STAGING, h.len);
        let mut hash = Sha256::new();
        let mut page = [0u8; PAGE as usize];
        let mut app_hash = [0u8; 8];
        for p in 0..h.pages {
            feed_watchdog();
            let (from, to) = h.record(plan, p)?;
            let mut old = |at: u32, out: &mut [u8]| {
                out.copy_from_slice(flash_bytes(APP_START + at, out.len() as u32));
                true
            };
            let len = h.page_len(p) as usize;
            make_page(&h, p, &plan[from as usize..to as usize], &mut old, &mut page)?;
            hash.update(&page[..len]);
            // the trailer's app hash, wherever the page boundary cuts it
            let at = p * PAGE;
            for i in 0..8u32 {
                let k = size - NRF_TRAILER_LEN + 32 + i;
                if (at..at + len as u32).contains(&k) {
                    app_hash[i as usize] = page[(k - at) as usize];
                }
            }
        }
        (hash.finalize()[..] == h.new_sha256).then_some((h.len, app_hash))
    }
}

impl<const LENDS: bool> FirmwareTarget for InPlaceTarget<LENDS> {
    fn capacity(&self) -> u32 {
        if LENDS { COMPANION_REGION.1 } else { STAGING_END - STAGING }
    }

    // a staged plan waits for the restart as checked: nothing may write over it
    fn erase(&mut self, offset: u32, len: u32) -> bool {
        if INSTALL_PENDING.load(Ordering::Relaxed) || offset + len > self.capacity() || !self.claim() {
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
        let Some((plan_len, trial_hash)) = self.check_plan(size) else {
            log::warn!("OTA: the plan does not make the image it should");
            return false;
        };
        let own = own();
        let mut store = self.store.borrow_mut();
        // the update runs on trial; this firmware is gone once it is in
        let record = FwRecord { trial: Some(0), skip: FwRecord::load(&mut store).skip, trial_hash, ..FwRecord::left(own) };
        let words = request_words(plan_len, own.map_or(0, |(l, _)| l));
        let mut bytes = [0u8; 32];
        for (b, w) in bytes.chunks_mut(4).zip(words) {
            b.copy_from_slice(&w.to_le_bytes());
        }
        let ok = record.save(&mut store)
            && NorFlash::erase(store.flash(), RECORD, RECORD + PAGE).is_ok()
            && NorFlash::write(store.flash(), RECORD, &bytes).is_ok();
        INSTALL_PENDING.fetch_or(ok, Ordering::Relaxed);
        ok
    }
}

/// The T114's updates.
pub struct InPlace;

pub static IN_PLACE: InPlace = InPlace;

impl Updates for InPlace {
    fn clear_request(&self, store: &SharedStore<Flash>) {
        promicro::ota::clear_request(store)
    }

    /// As the Pro Micro's, but a firmware that fails its trial boots stays.
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
            log::error!("OTA: the update failed {boots} trial boots, and there is no firmware to go back to");
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
        (false, record.prev, record.skip, false)
    }

    fn rollback(&self, _store: &SharedStore<Flash>) -> bool {
        false
    }

    fn confirm(&self, store: &SharedStore<Flash>) {
        if INSTALL_PENDING.load(Ordering::Relaxed) {
            return; // the record is the next update's trial already
        }
        let mut s = store.borrow_mut();
        let mut record: FwRecord = FwRecord::load(&mut s);
        record.trial = None;
        record.save(&mut s);
    }

    fn previous_kept(&self) -> bool {
        false
    }

    fn install_pending(&self) -> bool {
        INSTALL_PENDING.load(Ordering::Relaxed)
    }

    fn hand_over(&self) -> ! {
        promicro::ota::hand_over_with(INSTALLER_BIN)
    }
}

const _: () = assert!(COMPANION_REGION.1 >= meshcore_fw_core::storage::MIN_COMPANION_SIZE);
const _: () = assert!(APP_END - APP_START >= SHIFT + PAGE);
