//! A two-sector clock journal. The first record on a sector uses the small-blob format.
//! An older reader takes the first four payload bytes as the epoch. With the new format,
//! the rest of the payload protects the generation word against interrupted erases.
//! A legacy write to the other sector advances the generation and supersedes this journal.

use super::{HEADER, MAGIC, SECTOR, Store, crc32, slot_header};
use alloc::vec::Vec;
use embedded_storage::nor_flash::NorFlash;

const ENTRY: usize = 8;
const JOURNAL_START: u32 = (HEADER + ENTRY) as u32;
// Bound the gap between a sector's checkpoint and its latest saved epoch for cold downgrades.
const CHECKPOINT_AGE_SECS: u32 = 24 * 60 * 60;

#[derive(Clone, Copy)]
struct ClockPage {
    generation: u32,
    epoch: Option<u32>,
    journal: bool,
}

fn newest_clock_page(a: Option<ClockPage>, b: Option<ClockPage>) -> (Option<ClockPage>, usize) {
    match (a, b) {
        (Some(a), Some(b)) if (b.generation.wrapping_sub(a.generation) as i32) > 0 => (Some(b), 1),
        (Some(a), _) => (Some(a), 0),
        (None, b) => (b, 1),
    }
}

impl<F: NorFlash> Store<F> {
    fn clock_page(&mut self, addr: u32) -> Result<Option<ClockPage>, ()> {
        let bytes = self.read_words::<{ HEADER + ENTRY }>(addr).ok_or(())?;
        let word = |i: usize| u32::from_le_bytes(bytes[4 * i..4 * i + 4].try_into().unwrap());
        let (generation, len) = (word(1), word(2) as usize);
        if word(0) != MAGIC || !matches!(len, 0 | 4 | ENTRY) || crc32(&bytes[HEADER..HEADER + len]) != word(3) {
            return Ok(None);
        }
        let journal = len == ENTRY;
        if journal && word(5) != !generation {
            return Ok(None);
        }
        Ok(Some(ClockPage { generation, epoch: (len != 0).then(|| word(4)), journal }))
    }

    /// Latest complete entry and next untouched slot. An interrupted program leaves at
    /// least one bit pair as 11, which fails the complement check. Skip that slot.
    fn scan_clock(&mut self, addr: u32, mut latest: u32) -> (u32, Result<Option<u32>, ()>) {
        let mut next = addr + JOURNAL_START;
        let mut free = None;
        let read_all = self.stream_flash(next, SECTOR - JOURNAL_START, |chunk| {
            for slot in chunk.chunks_exact(ENTRY) {
                if slot == [0xFF; ENTRY] {
                    free = Some(next);
                    return false;
                }
                let epoch = u32::from_le_bytes(slot[..4].try_into().unwrap());
                if slot[4..] == (!epoch).to_le_bytes() {
                    latest = epoch;
                }
                next += ENTRY as u32;
            }
            true
        });
        let append_at = if read_all || free.is_some() { Ok(free) } else { Err(()) };
        (latest, append_at)
    }

    pub(super) fn load_clock(&mut self, addr: [u32; 2]) -> Option<Vec<u8>> {
        let (page, index) = newest_clock_page(self.clock_page(addr[0]).ok().flatten(), self.clock_page(addr[1]).ok().flatten());
        let page = page?;
        let epoch = page.epoch?;
        let latest = if page.journal { self.scan_clock(addr[index], epoch).0 } else { epoch };
        Some(latest.to_le_bytes().to_vec())
    }

    pub(super) fn save_clock(&mut self, addr: [u32; 2], data: &[u8]) -> bool {
        let epoch = if data.is_empty() {
            None
        } else {
            let Ok(bytes) = data.try_into() else { return false };
            Some(u32::from_le_bytes(bytes))
        };
        let (Ok(a), Ok(b)) = (self.clock_page(addr[0]), self.clock_page(addr[1])) else { return false };
        let (page, index) = newest_clock_page(a, b);
        if let (Some(epoch), Some(page)) = (epoch, page)
            && page.journal
            && let Some(checkpoint) = page.epoch
            && epoch.abs_diff(checkpoint) < CHECKPOINT_AGE_SECS
        {
            let (_, Ok(free)) = self.scan_clock(addr[index], checkpoint) else { return false };
            if let Some(at) = free {
                let mut entry = [0u8; ENTRY];
                entry[..4].copy_from_slice(&epoch.to_le_bytes());
                entry[4..].copy_from_slice(&(!epoch).to_le_bytes());
                return self.write_chunked(at, &entry);
            }
        }

        let generation = page.map_or(1, |p| p.generation.wrapping_add(1));
        let mut record = [0u8; HEADER + ENTRY];
        let len = if let Some(epoch) = epoch {
            record[HEADER..HEADER + 4].copy_from_slice(&epoch.to_le_bytes());
            record[HEADER + 4..].copy_from_slice(&(!generation).to_le_bytes());
            ENTRY
        } else {
            0 // Keep the small-blob deletion record readable by old firmware too.
        };
        let hdr = slot_header(generation, &record[HEADER..HEADER + len]);
        record[..HEADER].copy_from_slice(&hdr);

        // Keep the current sector intact until the other checkpoint is published.
        let target = addr[1 - index];
        self.erase_sector(target)
            && self.write_chunked(target + 4, &record[4..HEADER + len])
            && self.write_chunked(target, &record[..4])
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Blob, tests::RamFlash};
    use super::*;
    use alloc::vec;
    use embedded_storage::nor_flash::{ErrorType, NorFlashErrorKind, ReadNorFlash};

    #[derive(Clone, Copy)]
    enum Cut {
        Read(u32),
        Erase(u32),
        Write(u32, usize),
    }

    struct TestFlash {
        ram: RamFlash,
        erases: usize,
        cut: Option<Cut>,
    }

    impl ErrorType for TestFlash {
        type Error = NorFlashErrorKind;
    }

    impl ReadNorFlash for TestFlash {
        const READ_SIZE: usize = RamFlash::READ_SIZE;

        fn read(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
            if matches!(self.cut, Some(Cut::Read(at)) if at == offset) {
                return Err(NorFlashErrorKind::Other);
            }
            self.ram.read(offset, buf)
        }

        fn capacity(&self) -> usize {
            self.ram.capacity()
        }
    }

    impl NorFlash for TestFlash {
        const WRITE_SIZE: usize = RamFlash::WRITE_SIZE;
        const ERASE_SIZE: usize = RamFlash::ERASE_SIZE;

        fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
            self.erases += 1;
            if matches!(self.cut, Some(Cut::Erase(at)) if at == from) {
                // An interrupted erase can corrupt the generation while leaving the header readable.
                self.ram.0[from as usize + 4] |= 2; // generation 1 appears newer than 2
                return Err(NorFlashErrorKind::Other);
            }
            self.ram.erase(from, to)
        }

        fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), Self::Error> {
            assert_eq!(offset % 4, 0);
            assert_eq!(data.len() % 4, 0);
            let at = offset as usize;
            assert!(self.ram.0[at..at + data.len()].iter().all(|&b| b == 0xFF));
            if let Some(Cut::Write(at, bytes)) = self.cut
                && at == offset
            {
                self.ram.write(offset, &data[..bytes])?;
                return Err(NorFlashErrorKind::Other);
            }
            self.ram.write(offset, data)
        }
    }

    const PAGES: [u32; 2] = [0, SECTOR];

    fn mount(flash: TestFlash) -> Store<TestFlash> {
        Store::with_regions(flash, (0, 2 * SECTOR), None).with_data_from(Blob::Clock)
    }

    fn blank() -> Store<TestFlash> {
        mount(TestFlash { ram: RamFlash(vec![0xFF; (2 * SECTOR) as usize]), erases: 0, cut: None })
    }

    fn remount(store: Store<TestFlash>) -> Store<TestFlash> {
        mount(TestFlash { cut: None, ..store.flash })
    }

    fn save(store: &mut Store<TestFlash>, epoch: u32) -> bool {
        store.save(Blob::Clock, &epoch.to_le_bytes())
    }

    fn load(store: &mut Store<TestFlash>) -> Option<u32> {
        let data = store.load(Blob::Clock)?;
        Some(u32::from_le_bytes(data.try_into().unwrap()))
    }

    // The old firmware takes the first word of the latest small-blob record.
    fn legacy_load(store: &mut Store<TestFlash>) -> Option<u32> {
        let (_, record) = store.newest(PAGES).0?;
        Some(u32::from_le_bytes(record.get(..4)?.try_into().unwrap()))
    }

    #[test]
    fn reads_legacy_clock_and_respects_a_write_from_older_firmware() {
        let mut store = blank();
        assert!(store.save_pair(PAGES, &99u32.to_le_bytes()));
        assert!(store.save_pair(PAGES, &100u32.to_le_bytes()));
        assert_eq!(load(&mut store), Some(100));

        assert!(save(&mut store, 101));
        assert!(save(&mut store, 102));
        assert_eq!(load(&mut store), Some(102));
        assert_eq!(legacy_load(&mut store), Some(101));

        assert!(store.save_pair(PAGES, &90u32.to_le_bytes()));
        let mut store = remount(store);
        assert_eq!(load(&mut store), Some(90));
        assert!(save(&mut store, 91));
        assert_eq!(load(&mut store), Some(91));

        assert!(store.save(Blob::Clock, &[]));
        assert_eq!(load(&mut store), None);
        assert_eq!(legacy_load(&mut store), None);
    }

    #[test]
    fn appends_until_the_page_is_full() {
        let mut store = blank();
        let entries = (SECTOR - JOURNAL_START) / ENTRY as u32;
        for epoch in 0..=entries {
            assert!(save(&mut store, epoch));
        }
        assert_eq!(store.flash.erases, 1);
        assert_eq!(load(&mut store), Some(entries));
        assert!(save(&mut store, entries + 1));
        assert_eq!(store.flash.erases, 2);
        assert_eq!(load(&mut store), Some(entries + 1));
        assert_eq!(legacy_load(&mut store), Some(entries + 1));
    }

    #[test]
    fn rotates_when_the_legacy_checkpoint_is_a_day_old() {
        let mut store = blank();
        assert!(save(&mut store, 0));
        assert!(save(&mut store, CHECKPOINT_AGE_SECS - 1));
        assert_eq!(legacy_load(&mut store), Some(0));
        assert_eq!(store.flash.erases, 1);
        assert!(save(&mut store, CHECKPOINT_AGE_SECS));
        assert_eq!(legacy_load(&mut store), Some(CHECKPOINT_AGE_SECS));
        assert_eq!(store.flash.erases, 2);
    }

    #[test]
    fn skips_a_torn_append_and_uses_physical_order() {
        let mut store = blank();
        assert!(save(&mut store, u32::MAX - 8));
        assert!(save(&mut store, u32::MAX - 4));
        store.flash.cut = Some(Cut::Write(JOURNAL_START + ENTRY as u32, 4));
        assert!(!save(&mut store, u32::MAX - 2));
        let mut store = remount(store);
        assert_eq!(load(&mut store), Some(u32::MAX - 4));
        assert!(save(&mut store, u32::MAX - 6));
        assert_eq!(load(&mut store), Some(u32::MAX - 6));
        assert!(save(&mut store, u32::MAX));
        assert_eq!(load(&mut store), Some(u32::MAX));
        assert_eq!(store.flash.erases, 1);
    }

    #[test]
    fn keeps_the_current_clock_if_rotation_is_interrupted() {
        for cut in [Cut::Erase(0), Cut::Write(4, 12), Cut::Write(0, 2)] {
            let mut store = blank();
            assert!(save(&mut store, 0));
            assert!(save(&mut store, CHECKPOINT_AGE_SECS));
            assert!(save(&mut store, CHECKPOINT_AGE_SECS + 1));
            store.flash.cut = Some(cut);
            assert!(!save(&mut store, 2 * CHECKPOINT_AGE_SECS));
            let mut store = remount(store);
            assert_eq!(load(&mut store), Some(CHECKPOINT_AGE_SECS + 1));
            assert!(save(&mut store, 2 * CHECKPOINT_AGE_SECS));
            assert_eq!(load(&mut store), Some(2 * CHECKPOINT_AGE_SECS));
        }
    }

    #[test]
    fn an_unreadable_page_or_tail_does_not_authorize_an_erase() {
        let mut store = blank();
        assert!(save(&mut store, 100));
        for at in [SECTOR, JOURNAL_START] {
            store.flash.cut = Some(Cut::Read(at));
            assert_eq!(load(&mut store), Some(100));
            assert!(!save(&mut store, 101));
            assert_eq!(store.flash.erases, 1);
        }
        assert_eq!(load(&mut remount(store)), Some(100));
    }
}
