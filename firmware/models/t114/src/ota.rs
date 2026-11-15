//! The T114's firmware updates, applied in place: the board has its internal flash only, and an
//! app slot beside a staging area as large (the Pro Micro's way, `promicro::ota`) would cap the
//! app at 340 KiB. A delta comes into the staging area as a plan (`meshcore_inplace`, which the
//! protocol core makes as it comes: `FirmwareTarget::in_place_shift`). Once the image it makes
//! has checked out, the plan is checked against the running firmware once more, and turned around
//! into the plan back to it, which is checked too and stays beside it. At the next reset the
//! installer (`installer/`) moves the running firmware up by [`SHIFT`] and writes the new one over
//! it, a page at a time. A whole image does not fit: it goes on as UF2 over USB.
//!
//! The update runs on trial as on the Pro Micro: after [`MAX_TRIAL_BOOTS`] boots that never ran
//! long enough to be kept, the plan back goes on the same way, as with `fw rollback`. It stays
//! until the next update comes into the staging area (on the companion, until its data comes
//! back there after the trial).
//!
//! Layout (memory.x): the app 0x26000..0x8F000; the staging area 0x8F000..0xD6000, the plan from
//! its start, the plan back from the page after it; the companion keeps its data there and lends
//! it to its updates (to 0xD1000, as on the Pro Micro); from 0xD6000 on as on the Pro Micro.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use embedded_storage::nor_flash::NorFlash;
use meshcore::ota::{FirmwareTarget, ImageFormat, NRF_CHIP_NRF52840, NRF_TRAILER_LEN};
use meshcore_fw_core::storage::{SharedStore, Store};
use meshcore_inplace::{Header, PAGE, Rebuilt, make_page, request_words};
use promicro::flash::Flash;
use promicro::ota::{APP_START, FwRecord, MAX_TRIAL_BOOTS, RECORD, Updates, feed_watchdog, image_in, no_app_crc};
use sha2::{Digest, Sha256};

// shared with the installer (installer/src/main.rs)
pub const APP_END: u32 = 0x8F000;
const STAGING: u32 = 0x8F000;
const STAGING_END: u32 = promicro::DATA_OVERFLOW.0;
/// How far the old firmware moves up first: what code moves by between releases, with room to
/// spare (a page of the new image refers only to old data at least this far above it, less a page).
pub const SHIFT: u32 = 32 * 1024;

/// The companion's data: the staging area up to where its data waits while an update is there
/// (`promicro::COMPANION_ASIDE`).
pub const COMPANION_REGION: (u32, u32) = (STAGING, promicro::COMPANION_ASIDE[0].0 - STAGING);

static INSTALLER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/installer.bin"));

/// A plan is staged and checked: the reboot hands over to the installer.
static INSTALL_PENDING: AtomicBool = AtomicBool::new(false);
/// The plan back to the firmware that ran before is still in the staging area.
static PREVIOUS_KEPT: AtomicBool = AtomicBool::new(false);

type NrfStore = Store<Flash>;

/// `len` bytes of the internal flash from `addr`.
fn flash_bytes(addr: u32, len: u32) -> &'static [u8] {
    // SAFETY: inside the internal flash, which is memory-mapped
    unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) }
}

/// The running firmware: its length and the app's hash.
fn own() -> Option<(u32, [u8; 32])> {
    image_in(APP_START, APP_END - APP_START)
}

/// The SHA-256 of the running firmware's `len` bytes, as a plan names the image it makes.
fn own_sha256(len: u32) -> [u8; 32] {
    let mut h = Sha256::new();
    for part in flash_bytes(APP_START, len).chunks(PAGE as usize) {
        feed_watchdog();
        h.update(part);
    }
    h.finalize().into()
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

/// Ask the installer for the plan at `at` in the staging area, `len` bytes long, over the running
/// firmware of `old_len` bytes.
fn request(store: &mut NrfStore, at: u32, len: u32, old_len: u32) -> bool {
    let mut bytes = [0u8; 32];
    for (b, w) in bytes.chunks_mut(4).zip(request_words(at, len, old_len)) {
        b.copy_from_slice(&w.to_le_bytes());
    }
    let ok = NorFlash::erase(store.flash(), RECORD, RECORD + PAGE).is_ok() && NorFlash::write(store.flash(), RECORD, &bytes).is_ok();
    INSTALL_PENDING.fetch_or(ok, Ordering::Relaxed);
    ok
}

/// The plan back to the firmware that ran before this one, if it is still in the staging area:
/// the plan there made this very firmware, and the plan back after it goes from it.
fn way_back() -> Option<(u32, Header)> {
    let (plan, _) = plan_at(0)?;
    let (len, _) = own()?;
    let at = back_at(&plan);
    let (back, _) = plan_at(at)?;
    let fits = plan.new_len == len && back.old_len == len && back.new_len == plan.old_len;
    (fits && plan.new_sha256 == own_sha256(len)).then_some((at, back))
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
        let old_sha = own_sha256(old_len);
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
        if LENDS { COMPANION_REGION.1 } else { STAGING_END - STAGING }
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
        record.save(&mut store) && request(&mut store, 0, plan.len, plan.old_len)
    }
}

/// The T114's updates.
pub struct InPlace;

pub static IN_PLACE: InPlace = InPlace;

/// Go back to the firmware that ran before (`fw rollback`, or by itself after failed trial boots,
/// `failed`); the restart is the caller's, and hands over to the installer.
fn go_back(store: &SharedStore<Flash>, failed: bool) -> bool {
    let Some((at, back)) = way_back() else {
        return false;
    };
    let mut s = store.borrow_mut();
    FwRecord { failed, ..FwRecord::left(own()) }.save(&mut s) && request(&mut s, at, back.len, back.old_len)
}

impl Updates for InPlace {
    fn clear_request(&self, store: &SharedStore<Flash>) {
        promicro::ota::clear_request(store)
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
            if go_back(store, true) {
                self.hand_over();
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
        let kept = way_back().is_some();
        PREVIOUS_KEPT.store(kept, Ordering::Relaxed);
        if record.failed {
            log::warn!("OTA: rolled back, {} did not start", record.prev);
        }
        (kept, record.prev, record.skip, record.failed)
    }

    fn rollback(&self, store: &SharedStore<Flash>) -> bool {
        go_back(store, false)
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
        promicro::ota::hand_over_with(INSTALLER_BIN)
    }
}

const _: () = assert!(COMPANION_REGION.1 >= meshcore_fw_core::storage::MIN_COMPANION_SIZE);
const _: () = assert!(APP_END - APP_START >= SHIFT + PAGE);
