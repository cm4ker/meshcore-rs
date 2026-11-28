//! Persistent trial state for dual-slot ESP firmware. The ESP layer performs the bootloader writes.

use alloc::string::String;
use alloc::vec::Vec;
use embedded_storage::nor_flash::NorFlash;

use crate::storage::{Blob, Store};

pub const MAX_TRIAL_BOOTS: u8 = 3;
const NO_TRIAL: u8 = 0xFF;
const ROLLBACK_PENDING: u8 = 0xFF;
const ROLLBACK_SELECTED: u8 = 0xFE;

pub enum TrialBoot {
    Normal,
    Trial(u8),
    Fallback(u8),
    Rollback,
    RolledBack,
    RollbackRejected,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Rollback {
    RecordError,
    SlotError,
    SelectionRecordError,
    Selected,
}

/// OtaTrial stores [new slot, previous slot, boots], followed by the skipped release (u32 LE)
/// and the label of the other firmware. Early builds stored only the first three bytes.
/// During a rollback the boots byte marks intent or a completed selection. The label and release
/// refer to the firmware being left. Older builds clear the trial on booting the other slot.
#[derive(Default)]
pub struct FwRecord {
    pub trial: Option<[u8; 3]>,
    pub skip: u32,
    pub prev: String,
}

impl FwRecord {
    pub fn load<F: NorFlash>(store: &mut Store<F>) -> Self {
        let Some(r) = store.load(Blob::OtaTrial) else {
            return Self::default();
        };
        Self {
            trial: r.get(..3).filter(|b| b[0] != NO_TRIAL).map(|b| [b[0], b[1], b[2]]),
            skip: r.get(3..7).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap())),
            prev: r.get(7..).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default(),
        }
    }

    pub fn save<F: NorFlash>(&self, store: &mut Store<F>) -> bool {
        let mut r = Vec::from(self.trial.unwrap_or([NO_TRIAL, 0, 0]));
        r.extend_from_slice(&self.skip.to_le_bytes());
        r.extend_from_slice(self.prev.as_bytes());
        store.save(Blob::OtaTrial, &r)
    }

    pub fn rollback_pending(&self) -> bool {
        matches!(self.trial, Some([_, _, ROLLBACK_PENDING | ROLLBACK_SELECTED]))
    }

    pub fn on_boot<F: NorFlash>(&mut self, store: &mut Store<F>, booted: u8) -> TrialBoot {
        let Some([slot, prev, boots]) = self.trial else {
            return TrialBoot::Normal;
        };
        if slot != booted {
            let outcome = if booted == prev && self.rollback_pending() {
                TrialBoot::RolledBack
            } else {
                TrialBoot::Fallback(slot)
            };
            self.trial = None;
            self.save(store);
            return outcome;
        }
        if boots == ROLLBACK_SELECTED {
            return TrialBoot::RollbackRejected;
        }
        if boots >= MAX_TRIAL_BOOTS {
            return TrialBoot::Rollback;
        }
        self.trial = Some([slot, prev, boots + 1]);
        self.save(store);
        TrialBoot::Trial(boots + 1)
    }

    pub fn request_rollback<F: NorFlash>(
        self,
        store: &mut Store<F>,
        from: u8,
        to: u8,
        release: impl FnOnce() -> (u32, String),
        select: impl FnOnce(&mut Store<F>, u8) -> bool,
    ) -> Rollback {
        let pending = Some([from, to, ROLLBACK_PENDING]);
        let mut record = self;
        if record.trial != pending {
            let (skip, prev) = release();
            record = Self { trial: pending, skip, prev };
            if !record.save(store) {
                return Rollback::RecordError;
            }
        }
        // A failed selector can leave part of the boot metadata written. Keep the pending
        // record for the next attempt or until the destination boots.
        if !select(store, to) {
            return Rollback::SlotError;
        }
        // Remember that the switch completed so a bootloader fallback cannot create a reset loop.
        record.trial = Some([from, to, ROLLBACK_SELECTED]);
        if record.save(store) { Rollback::Selected } else { Rollback::SelectionRecordError }
    }
}
