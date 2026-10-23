//! Firmware slots: the OTA target (inactive app slot) and trial boots with automatic rollback.
//!
//! The ESP-IDF 2nd-stage bootloader picks the slot from `otadata`, but the one espflash ships
//! is built without rollback support, so rollback is done here:
//! - activating an update records `[new slot, previous slot, boots = 0]` in the store;
//! - every boot of the new slot increments `boots`; after [`MAX_TRIAL_BOOTS`] boots that never
//!   reached [`confirm`] (hangs are turned into reboots by the watchdog), `otadata` is switched
//!   back to the previous slot;
//! - once the new firmware has run healthy for a while, [`confirm`] clears the record.

use alloc::rc::Rc;
use core::cell::RefCell;

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

fn trial_record(store: &mut EspStore<'_>) -> Option<[u8; 3]> {
    store.load(Blob::OtaTrial).and_then(|r| r.get(..3).and_then(|b| b.try_into().ok()))
}

/// Run at boot, before anything that could hang. Returns `true` while on a trial boot.
/// Never returns if the trial budget is exhausted (switches back and resets).
pub fn check_trial(store: &mut EspStore<'_>, booted: u8) -> bool {
    let Some([slot, prev, boots]) = trial_record(store) else {
        return false;
    };
    if slot != booted {
        // the bootloader refused the new image and fell back
        log::warn!("OTA: update in slot {slot} did not boot, staying on slot {booted}");
        store.save(Blob::OtaTrial, &[]);
        return false;
    }
    if boots >= MAX_TRIAL_BOOTS {
        log::error!("OTA: slot {slot} failed {boots} trial boots, rolling back to slot {prev}");
        store.save(Blob::OtaTrial, &[]);
        select_slot(store, prev, OtaImageState::Valid);
        crate::reset::hard_reset();
    }
    store.save(Blob::OtaTrial, &[slot, prev, boots + 1]);
    log::info!("OTA: trial boot {} of {MAX_TRIAL_BOOTS} for slot {slot}", boots + 1);
    true
}

/// The new firmware is healthy: keep it.
pub fn confirm(store: &mut EspStore<'_>) {
    store.save(Blob::OtaTrial, &[]);
    let mut buf = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    if let Ok(mut updater) = OtaUpdater::new(store.flash(), &mut buf) {
        let _ = updater.set_current_ota_state(OtaImageState::Valid);
    }
}

/// Switch back to the other slot (CLI `ota rollback`), keeping it on trial too.
pub fn rollback(store: &mut EspStore<'_>, slots: Slots) -> bool {
    store.save(Blob::OtaTrial, &[]);
    select_slot(store, slots.target.0, OtaImageState::Valid)
}

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

    fn erase(&mut self, offset: u32, len: u32) -> bool {
        // committing a compressed or delta update rewrites the whole slot in one go
        crate::reset::feed();
        let base = self.slots.target.1 + offset;
        self.store.borrow_mut().flash().erase(base, base + len).is_ok()
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> bool {
        self.store.borrow_mut().flash().write_nor(self.slots.target.1 + offset, data).is_ok()
    }

    fn read(&mut self, offset: u32, buf: &mut [u8]) -> bool {
        self.store.borrow_mut().flash().read(self.slots.target.1 + offset, buf).is_ok()
    }

    fn running_capacity(&self) -> u32 {
        self.slots.current.1
    }

    fn read_running(&mut self, offset: u32, buf: &mut [u8]) -> bool {
        self.store.borrow_mut().flash().read(self.slots.current.0 + offset, buf).is_ok()
    }

    fn chip_id(&self) -> u16 {
        if cfg!(feature = "esp32c6") { meshcore::ota::ESP_CHIP_ID_ESP32C6 } else { meshcore::ota::ESP_CHIP_ID_ESP32S3 }
    }

    fn activate(&mut self) -> bool {
        let mut store = self.store.borrow_mut();
        let (slot, booted) = (self.slots.target.0, self.slots.booted);
        store.save(Blob::OtaTrial, &[slot, booted, 0]) && select_slot(&mut store, slot, OtaImageState::New)
    }
}
