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

use embassy_time::Timer;
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{self, AppPartitionSubType, PartitionType};
use meshcore::ota::FirmwareTarget;
use meshcore_fw_core::esp_ota_trial::{FwRecord, MAX_TRIAL_BOOTS, Rollback, TrialBoot};

use crate::storage::EspStore;

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

/// This firmware, as the record names it for after it is left: `0.8.6 #69`, or `0.8.6` on a local build.
fn own_label() -> String {
    meshcore::fwstate::Release::new(meshcore::release::PRODUCT_VERSION, meshcore::release::release_counter()).to_string()
}

/// Run at boot before starting the application. Returns true only on a trial boot.
pub async fn check_trial(store: &SharedStore<'_>, slots: Slots) -> bool {
    let booted = slots.booted;
    let status = {
        let mut store = store.borrow_mut();
        FwRecord::load(&mut store).on_boot(&mut store, booted)
    };
    match status {
        TrialBoot::Normal => {}
        TrialBoot::Trial(boots) => {
            log::info!("OTA: trial boot {boots} of {MAX_TRIAL_BOOTS} for slot {booted}");
            return true;
        }
        TrialBoot::Fallback(slot) => {
            log::warn!("OTA: update in slot {slot} did not boot, staying on slot {booted}");
        }
        TrialBoot::RolledBack => log::info!("OTA: booted rollback slot {booted}"),
        TrialBoot::RollbackRejected => {
            log::error!("OTA: bootloader returned to slot {booted} after selecting the rollback slot");
        }
        TrialBoot::Rollback => {
            OTA_WRITE_LOCKED.store(true, Ordering::Relaxed);
            let prev = slots.target.0;
            log::warn!("OTA: slot {booted} needs rollback to slot {prev}");
            for attempt in 1..=3 {
                crate::reset::feed();
                if rollback(&mut store.borrow_mut(), slots) {
                    crate::reset::hard_reset();
                }
                if attempt < 3 {
                    Timer::after_secs(1).await;
                }
            }
            log::error!("OTA: rollback failed after three attempts; will retry at next boot");
        }
    }
    false
}

/// What `fw` and `fw rollback` know of the other slot: whether it holds a firmware (an ESP image
/// starts with 0xE9), its name, and the release rolled back from.
pub fn other_slot(store: &mut EspStore<'_>, slots: Slots) -> (bool, String, u32) {
    let mut magic = [0u8; 1];
    let bootable = store.flash().read(slots.target.1, &mut magic).is_ok() && magic[0] == 0xE9;
    let record = FwRecord::load(store);
    let label = if record.rollback_pending() && record.trial.is_some_and(|[slot, _, _]| slot == slots.booted) {
        String::new()
    } else {
        record.prev
    };
    (bootable, label, record.skip)
}

/// The new firmware is healthy: keep it.
pub fn confirm(store: &mut EspStore<'_>) {
    let mut record = FwRecord::load(store);
    if record.rollback_pending() {
        return;
    }
    record.trial = None;
    record.save(store);
    let mut buf = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    if let Ok(mut updater) = OtaUpdater::new(store.flash(), &mut buf) {
        if updater.set_current_ota_state(OtaImageState::Valid).is_ok() {
            log::info!("OTA: new firmware confirmed");
        }
    }
}

/// Switch back to the other slot (`fw rollback`); the restart is the caller's. The firmware that
/// runs now is the one left: not installed by itself again.
pub fn rollback(store: &mut EspStore<'_>, slots: Slots) -> bool {
    let result = FwRecord::load(store).request_rollback(
        store,
        slots.booted,
        slots.target.0,
        || (meshcore::release::release_counter(), own_label()),
        |store, slot| select_slot(store, slot, OtaImageState::Valid),
    );
    match result {
        Rollback::RecordError => log::error!("OTA: could not save rollback intent"),
        Rollback::SelectionRecordError => log::error!("OTA: could not record the selected rollback slot"),
        Rollback::SlotError => log::error!("OTA: could not select rollback slot {}", slots.target.0),
        Rollback::Selected => {}
    }
    if result != Rollback::RecordError {
        OTA_WRITE_LOCKED.store(true, Ordering::Relaxed);
    }
    result == Rollback::Selected
}

/// Protect the inactive app from OTA writes when selected for the next boot or still needed
/// for a pending rollback. Each OTA link has its own target.
static OTA_WRITE_LOCKED: AtomicBool = AtomicBool::new(false);

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
        if OTA_WRITE_LOCKED.load(Ordering::Relaxed) {
            return false;
        }
        let base = self.slots.target.1 + offset;
        self.store.borrow_mut().flash().erase(base, base + len).is_ok()
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> bool {
        crate::reset::feed();
        if OTA_WRITE_LOCKED.load(Ordering::Relaxed) {
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
        if OTA_WRITE_LOCKED.load(Ordering::Relaxed) {
            return false;
        }
        let mut store = self.store.borrow_mut();
        let (slot, booted) = (self.slots.target.0, self.slots.booted);
        // this firmware is what the update goes back to, should it fail
        let skip = FwRecord::load(&mut store).skip;
        let record = FwRecord { trial: Some([slot, booted, 0]), skip, prev: own_label() };
        let ok = record.save(&mut store) && select_slot(&mut store, slot, OtaImageState::New);
        OTA_WRITE_LOCKED.fetch_or(ok, Ordering::Relaxed);
        ok
    }
}
