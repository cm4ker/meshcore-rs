//! Firmware slots: the OTA target (inactive app slot) and trial boots with automatic rollback.
//!
//! The ESP-IDF 2nd-stage bootloader picks the slot from `otadata`, but the one espflash ships
//! is built without rollback support, so rollback is done here:
//! - activating an update records `[new slot, previous slot, boots = 0]` in the store;
//! - every boot of the new slot increments `boots`; after [`MAX_TRIAL_BOOTS`] boots that never
//!   reached [`confirm`] (hangs are turned into reboots by the watchdog), `otadata` is switched
//!   back to the previous slot;
//! - once the new firmware has run healthy for a while, [`confirm`] ends the trial.
//!
//! The same record keeps what the other slot holds (`fw rollback` goes back to it) and the release
//! this node was rolled back from, by hand or after failed trial boots: an update by itself does
//! not install that one again ([`FwRecord`]).

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{self, AppPartitionSubType, PartitionType};
use meshcore::ota::FirmwareTarget;

use crate::storage::{Blob, EspStore};

pub const MAX_TRIAL_BOOTS: u8 = 3;

pub type SharedStore<'d> = Rc<RefCell<EspStore<'d>>>;

#[derive(Clone, Copy, Debug)]
pub struct Slots {
    /// OTA number (0/1) of the running slot.
    pub booted: u8,
    /// The running slot: (flash offset, size); the base of delta updates.
    pub current: (u32, u32),
    /// The other slot: (OTA number, flash offset, size).
    pub target: (u8, u32, u32),
}

fn sub(n: u8) -> AppPartitionSubType {
    if n == 0 { AppPartitionSubType::Ota0 } else { AppPartitionSubType::Ota1 }
}

/// Find the running slot and the inactive one. `None` with a single-slot partition table.
pub fn slots(store: &mut EspStore<'_>) -> Option<Slots> {
    let mut buf = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(store.flash(), &mut buf).ok()?;
    let running = pt.booted_partition().ok()??;
    let booted = match running.partition_type() {
        PartitionType::App(AppPartitionSubType::Ota0) => 0,
        PartitionType::App(AppPartitionSubType::Ota1) => 1,
        _ => return None,
    };
    let other = 1 - booted;
    let entry = pt.find_partition(PartitionType::App(sub(other))).ok()??;
    Some(Slots { booted, current: (running.offset(), running.len()), target: (other, entry.offset(), entry.len()) })
}

fn select_slot(store: &mut EspStore<'_>, slot: u8, state: OtaImageState) -> bool {
    let mut buf = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let Ok(mut updater) = OtaUpdater::new(store.flash(), &mut buf) else {
        return false;
    };
    let Ok(mut data) = updater.ota_data() else {
        return false;
    };
    data.set_current_app_partition(sub(slot)).is_ok() && data.set_current_ota_state(state).is_ok()
}

/// `Blob::OtaTrial`: `[new slot, previous slot, boots]` while an update runs on trial (the first
/// byte [`NO_TRIAL`] otherwise), the release this node was rolled back from (u32 LE, 0 = none) and
/// the name of the firmware in the other slot (UTF-8, to the end). Builds from before kept only
/// the first three bytes, and only during a trial.
#[derive(Default)]
pub struct FwRecord {
    pub trial: Option<[u8; 3]>,
    pub skip: u32,
    pub prev: String,
}

const NO_TRIAL: u8 = 0xFF;

impl FwRecord {
    pub fn load(store: &mut EspStore<'_>) -> Self {
        let Some(r) = store.load(Blob::OtaTrial) else {
            return Self::default();
        };
        Self {
            trial: r.get(..3).filter(|b| b[0] != NO_TRIAL).map(|b| [b[0], b[1], b[2]]),
            skip: r.get(3..7).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap())),
            prev: r.get(7..).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
        }
    }

    fn save(&self, store: &mut EspStore<'_>) -> bool {
        let mut r = alloc::vec::Vec::from(self.trial.unwrap_or([NO_TRIAL, 0, 0]));
        r.extend_from_slice(&self.skip.to_le_bytes());
        r.extend_from_slice(self.prev.as_bytes());
        store.save(Blob::OtaTrial, &r)
    }
}

/// This firmware, as the record names it for after it is left: `0.8.6 #69`, or `0.8.6` on a local build.
fn own_label() -> String {
    meshcore::fwstate::Release::new(meshcore::release::PRODUCT_VERSION, meshcore::release::release_counter()).to_string()
}

/// Run at boot, before anything that could hang. Returns `true` while on a trial boot.
/// Never returns if the trial budget is exhausted (switches back and resets).
pub fn check_trial(store: &mut EspStore<'_>, booted: u8) -> bool {
    let mut record = FwRecord::load(store);
    let Some([slot, prev, boots]) = record.trial else {
        return false;
    };
    if slot != booted {
        // the bootloader refused the new image and fell back
        log::warn!("OTA: update in slot {slot} did not boot, staying on slot {booted}");
        record.trial = None;
        record.save(store);
        return false;
    }
    if boots >= MAX_TRIAL_BOOTS {
        log::error!("OTA: slot {slot} failed {boots} trial boots, rolling back to slot {prev}");
        // the firmware that runs is the one left: not installed by itself again
        let left = FwRecord { trial: None, skip: meshcore::release::release_counter(), prev: own_label() };
        left.save(store);
        select_slot(store, prev, OtaImageState::Valid);
        crate::reset::hard_reset();
    }
    record.trial = Some([slot, prev, boots + 1]);
    record.save(store);
    log::info!("OTA: trial boot {} of {MAX_TRIAL_BOOTS} for slot {slot}", boots + 1);
    true
}

/// What `fw` and `fw rollback` know of the other slot: whether it holds a firmware (an ESP image
/// starts with 0xE9), its name, and the release rolled back from.
pub fn other_slot(store: &mut EspStore<'_>, slots: Slots) -> (bool, String, u32) {
    let mut magic = [0u8; 1];
    let bootable = store.flash().read(slots.target.1, &mut magic).is_ok() && magic[0] == 0xE9;
    let record = FwRecord::load(store);
    (bootable, record.prev, record.skip)
}

/// The new firmware is healthy: keep it.
pub fn confirm(store: &mut EspStore<'_>) {
    let mut record = FwRecord::load(store);
    record.trial = None;
    record.save(store);
    let mut buf = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    if let Ok(mut updater) = OtaUpdater::new(store.flash(), &mut buf) {
        let _ = updater.set_current_ota_state(OtaImageState::Valid);
    }
}

/// Switch back to the other slot (`fw rollback`); the restart is the caller's. The firmware that
/// runs now is the one left: not installed by itself again.
pub fn rollback(store: &mut EspStore<'_>, slots: Slots) -> bool {
    let left = FwRecord { trial: None, skip: meshcore::release::release_counter(), prev: own_label() };
    left.save(store) && select_slot(store, slots.target.0, OtaImageState::Valid)
}

/// An update was selected for the next boot. Its slot then holds a verified image, which no
/// writer may touch until the restart: each link (LoRa, BLE, the Wi-Fi page) has a target of its own.
static ACTIVATED: AtomicBool = AtomicBool::new(false);

/// The inactive app slot, as seen by the update session.
pub struct FlashTarget<'d> {
    store: SharedStore<'d>,
    slots: Slots,
}

impl<'d> FlashTarget<'d> {
    pub fn new(store: SharedStore<'d>, slots: Slots) -> Self {
        Self { store, slots }
    }
}

impl FirmwareTarget for FlashTarget<'_> {
    fn capacity(&self) -> u32 {
        self.slots.target.2
    }

    // Committing a compressed or delta update rebuilds the image and reads it all back for the
    // hash without returning to the main loop, for tens of seconds: every flash access feeds the
    // watchdog.
    fn erase(&mut self, offset: u32, len: u32) -> bool {
        crate::reset::feed();
        if ACTIVATED.load(Ordering::Relaxed) {
            return false;
        }
        let base = self.slots.target.1 + offset;
        self.store.borrow_mut().flash().erase(base, base + len).is_ok()
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> bool {
        crate::reset::feed();
        if ACTIVATED.load(Ordering::Relaxed) {
            return false;
        }
        self.store.borrow_mut().flash().write_nor(self.slots.target.1 + offset, data).is_ok()
    }

    fn read(&mut self, offset: u32, buf: &mut [u8]) -> bool {
        crate::reset::feed();
        self.store.borrow_mut().flash().read(self.slots.target.1 + offset, buf).is_ok()
    }

    fn running_capacity(&self) -> u32 {
        self.slots.current.1
    }

    fn read_running(&mut self, offset: u32, buf: &mut [u8]) -> bool {
        crate::reset::feed();
        self.store.borrow_mut().flash().read(self.slots.current.0 + offset, buf).is_ok()
    }

    fn chip_id(&self) -> u16 {
        if cfg!(feature = "esp32c6") { meshcore::ota::ESP_CHIP_ID_ESP32C6 } else { meshcore::ota::ESP_CHIP_ID_ESP32S3 }
    }

    fn activate(&mut self, _size: u32) -> bool {
        let mut store = self.store.borrow_mut();
        let (slot, booted) = (self.slots.target.0, self.slots.booted);
        // this firmware is what the update goes back to, should it fail
        let skip = FwRecord::load(&mut store).skip;
        let record = FwRecord { trial: Some([slot, booted, 0]), skip, prev: own_label() };
        let ok = record.save(&mut store) && select_slot(&mut store, slot, OtaImageState::New);
        ACTIVATED.fetch_or(ok, Ordering::Relaxed);
        ok
    }
}
