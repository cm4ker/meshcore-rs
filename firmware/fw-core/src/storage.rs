//! Power-loss-safe storage in a data region of a NOR flash (and the companion role's own region).
//! The platform finds the regions (ESP32: the `meshcore` and `companion` partitions).
//!
//! Small blobs: each owns two 4 KiB sectors. A record is `magic | seq | len | crc32 | data`;
//! writes go to the sector not holding the newest valid record, so an interrupted write never
//! loses the previous value.
//!
//! The companion partition also holds a ring log (contact list snapshots, which can be tens of
//! KiB and change often: appending spreads the wear over the whole log) and a table of raw
//! contact adverts keyed by public key.
//!
//! A platform short of flash may lend the companion partition to firmware updates ([`lend`]):
//! the adverts then go in a short ring, and the data moves to a small area of its own while an
//! update is in the partition.
//!
//! Data always passes through a small stack buffer, so heap buffers may live anywhere.

mod lend;

pub use lend::Mount;

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};

const SECTOR: u32 = 4096;
const HEADER: usize = 16;
const MAGIC: u32 = 0x5352_434D; // "MCRS"
const LOG_MAGIC: u32 = 0x474C_434D; // "MCLG"

#[derive(Clone, Copy, Debug)]
pub enum Blob {
    Identity = 0,
    Prefs = 1,
    Acl = 2,
    /// Last known wall-clock time (u32 LE), so adverts keep increasing across power cycles.
    Clock = 3,
    /// Firmware slots: a pending trial `[slot, previous slot, boots]`, the release rolled back
    /// from, the name of the other slot's firmware (see the ESP crate's `ota.rs`; the Pro Micro
    /// keeps its own layout, see its `ota.rs`).
    OtaTrial = 4,
    /// Region map (text form, see `meshcore::region`).
    Regions = 5,
    /// Hosted room server: private key (64 bytes) followed by its prefs (text form).
    Room = 6,
    /// Hosted room server's ACL.
    RoomAcl = 7,
    /// Text-described I2C sensors (`sensor add`), one descriptor per line.
    Sensors = 8,
}

/// Small blobs of the companion partition.
#[derive(Clone, Copy, Debug)]
pub enum CompBlob {
    /// Private key (64 bytes) + public key (32 bytes).
    Identity = 0,
    /// `CompanionPrefs` text form.
    Prefs = 1,
    Channels = 2,
    Clock = 3,
    /// BLE bonds (paired phones).
    Bonds = 4,
    /// The partition's layout, with lending (`lend`): the advert ring and the log after it.
    Layout = 5,
}

/// Blobs in the `meshcore` partition (two sectors each).
const BLOBS: u32 = 9;

/// Companion partition layout, in sectors: small blobs, the advert table, then the ring log.
const COMP_BLOB_SECTORS: u32 = 16;
const COMP_TABLE_FIRST: u32 = COMP_BLOB_SECTORS;
const COMP_TABLE_SECTORS: u32 = 24;
const COMP_LOG_FIRST: u32 = COMP_TABLE_FIRST + COMP_TABLE_SECTORS;
/// With lending, the adverts go in a ring of this many sectors instead of the table: the newest
/// ones are kept (63 or more; upstream keeps 20).
const RING_SECTORS: u32 = 4;

/// Advert table slot: `[state][len][key 32][data 158]`; state 0xFF = free, 0x01 = used,
/// 0x00 = deleted (programmed in place, no erase).
const SLOT: u32 = 192;
const SLOTS_PER_SECTOR: u32 = SECTOR / SLOT;
pub const MAX_ADVERT_BLOB: usize = SLOT as usize - 34;
const SLOT_USED: u8 = 0x01;
const SLOT_DELETED: u8 = 0x00;
const SLOT_FREE: u8 = 0xFF;

pub struct Store<F> {
    flash: F,
    base: u32,
    size: u32,
    /// The companion region (offset, size), if the platform has one.
    comp: Option<(u32, u32)>,
    /// Where the blobs past the end of the data region go (offset, size), if the platform has
    /// such a place (`with_overflow`).
    overflow: Option<(u32, u32)>,
    /// Ring log write position (offset in the partition) and next sequence number.
    log_head: Option<(u32, u32)>,
    /// The adverts in a ring ([`RING_SECTORS`]) rather than the table.
    ring: bool,
    /// The slot the ring's next advert goes to, once found.
    ring_head: Option<u32>,
    /// Where the companion's data goes while its partition is lent to an update (`lend`).
    lending: Option<lend::Lending>,
}

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    crc
}

fn crc32(data: &[u8]) -> u32 {
    !crc32_update(0xFFFF_FFFF, data)
}

/// The store, shared between the main loop and what writes beside it (firmware slots, adverts).
pub type SharedStore<F> = Rc<RefCell<Store<F>>>;

/// Smallest companion region: its blobs, the advert table and a ring log of at least 16 sectors.
pub const MIN_COMPANION_SIZE: u32 = (COMP_LOG_FIRST + 16) * SECTOR;

impl<F: NorFlash> Store<F> {
    /// A store in the data region `(offset, size)` of `flash`, plus the companion role's region if
    /// the platform has one (ignored if smaller than [`MIN_COMPANION_SIZE`]).
    pub fn with_regions(flash: F, data: (u32, u32), companion: Option<(u32, u32)>) -> Self {
        let comp = companion.filter(|&(_, len)| len >= MIN_COMPANION_SIZE);
        Self { flash, base: data.0, size: data.1, comp, overflow: None, log_head: None, ring: false, ring_head: None, lending: None }
    }

    /// The blobs the data region has no room for go to `region` (offset, size), in order: for a
    /// data region laid out before they came (the Pro Micro's 64 KiB, eight blobs).
    pub fn with_overflow(self, region: (u32, u32)) -> Self {
        Self { overflow: Some(region), ..self }
    }

    pub fn has_companion_partition(&self) -> bool {
        self.comp.is_some()
    }

    // ------------------------------------------------------------ raw access

    /// Write `data` at `addr` (word aligned), padding the tail with 0xFF.
    fn write_chunked(&mut self, addr: u32, data: &[u8]) -> bool {
        let mut buf = [0u32; 64];
        for (i, chunk) in data.chunks(256).enumerate() {
            let bytes: &mut [u8; 256] = bytemuck_bytes(&mut buf);
            bytes[..chunk.len()].copy_from_slice(chunk);
            let n = chunk.len().next_multiple_of(4);
            bytes[chunk.len()..n].fill(0xFF);
            if self.flash.write(addr + (i * 256) as u32, &bytes[..n]).is_err() {
                return false;
            }
        }
        true
    }

    /// Read `len` bytes at `addr` (word aligned).
    fn read_chunked(&mut self, addr: u32, len: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        let mut buf = [0u32; 64];
        let mut off = 0;
        while off < len {
            let bytes: &mut [u8; 256] = bytemuck_bytes(&mut buf);
            let n = (len - off).min(256);
            ReadNorFlash::read(&mut self.flash, addr + off as u32, &mut bytes[..n.next_multiple_of(4)]).ok()?;
            out.extend_from_slice(&bytes[..n]);
            off += n;
        }
        Some(out)
    }

    /// A short read (`N` <= 256, a multiple of 4).
    fn read_words<const N: usize>(&mut self, addr: u32) -> Option<[u8; N]> {
        let mut buf = [0u32; 64];
        let bytes: &mut [u8; 256] = bytemuck_bytes(&mut buf);
        ReadNorFlash::read(&mut self.flash, addr, &mut bytes[..N]).ok()?;
        bytes[..N].try_into().ok()
    }

    fn erase_sector(&mut self, addr: u32) -> bool {
        self.flash.erase(addr, addr + SECTOR).is_ok()
    }

    // ------------------------------------------------------------ small blobs

    /// (seq, data) of a valid record in one sector.
    fn read_slot(&mut self, addr: u32) -> Option<(u32, Vec<u8>)> {
        let hdr: [u8; HEADER] = self.read_words(addr)?;
        let word = |i: usize| u32::from_le_bytes(hdr[i * 4..i * 4 + 4].try_into().unwrap());
        if word(0) != MAGIC {
            return None;
        }
        let (seq, len, crc) = (word(1), word(2) as usize, word(3));
        if len > SECTOR as usize - HEADER {
            return None;
        }
        let data = self.read_chunked(addr + HEADER as u32, len)?;
        (crc32(&data) == crc).then_some((seq, data))
    }

    /// Newest record among a blob's two sectors at `addr`, and the sector it is in.
    fn newest(&mut self, addr: [u32; 2]) -> (Option<(u32, Vec<u8>)>, u32) {
        let a = self.read_slot(addr[0]);
        let b = self.read_slot(addr[1]);
        match (a, b) {
            (Some(a), Some(b)) => {
                if b.0 > a.0 { (Some(b), 1) } else { (Some(a), 0) }
            }
            (Some(a), None) => (Some(a), 0),
            (None, Some(b)) => (Some(b), 1),
            (None, None) => (None, 1),
        }
    }

    fn save_pair(&mut self, addr: [u32; 2], data: &[u8]) -> bool {
        if data.len() > SECTOR as usize - HEADER {
            return false;
        }
        let (cur, cur_slot) = self.newest(addr);
        let seq = cur.map(|(s, _)| s.wrapping_add(1)).unwrap_or(1);
        let target = addr[1 - cur_slot as usize];
        let mut hdr = [0u8; HEADER];
        hdr[..4].copy_from_slice(&MAGIC.to_le_bytes());
        hdr[4..8].copy_from_slice(&seq.to_le_bytes());
        hdr[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        hdr[12..].copy_from_slice(&crc32(data).to_le_bytes());
        // data first, header last: a record is only valid once complete
        self.erase_sector(target) && self.write_chunked(target + HEADER as u32, data) && self.write_chunked(target, &hdr)
    }

    fn blob_addr(&self, blob: Blob) -> Option<[u32; 2]> {
        let in_data = self.size / (2 * SECTOR);
        let (base, size, i) = match self.overflow {
            Some((base, size)) if blob as u32 >= in_data => (base, size, blob as u32 - in_data),
            _ => (self.base, self.size, blob as u32),
        };
        let off = i * 2 * SECTOR;
        (off + 2 * SECTOR <= size).then_some([base + off, base + off + SECTOR])
    }

    /// Newest value of a blob; an empty record counts as absent.
    pub fn load(&mut self, blob: Blob) -> Option<Vec<u8>> {
        let addr = self.blob_addr(blob)?;
        self.newest(addr).0.map(|(_, d)| d).filter(|d| !d.is_empty())
    }

    /// Raw flash access (OTA slot writes, partition table).
    pub fn flash(&mut self) -> &mut F {
        &mut self.flash
    }

    pub fn save(&mut self, blob: Blob, data: &[u8]) -> bool {
        match self.blob_addr(blob) {
            Some(addr) => self.save_pair(addr, data),
            None => false,
        }
    }

    pub fn erase_all(&mut self) -> bool {
        let end = self.base + (BLOBS * 2 * SECTOR).min(self.size);
        let overflow = match self.overflow {
            Some((base, size)) => {
                let blobs = BLOBS.saturating_sub(self.size / (2 * SECTOR));
                blobs == 0 || self.flash.erase(base, base + (blobs * 2 * SECTOR).min(size)).is_ok()
            }
            None => true,
        };
        self.flash.erase(self.base, end).is_ok() && overflow
    }

    // ------------------------------------------------------- companion partition

    fn comp_blob_addr(&self, blob: CompBlob) -> Option<[u32; 2]> {
        let (base, _) = self.comp?;
        let off = blob as u32 * 2 * SECTOR;
        Some([base + off, base + off + SECTOR])
    }

    pub fn comp_load(&mut self, blob: CompBlob) -> Option<Vec<u8>> {
        if self.lent() {
            return self.lent_load(lend::TAG_BLOB, blob as u8).filter(|d| !d.is_empty());
        }
        let addr = self.comp_blob_addr(blob)?;
        self.newest(addr).0.map(|(_, d)| d).filter(|d| !d.is_empty())
    }

    pub fn comp_save(&mut self, blob: CompBlob, data: &[u8]) -> bool {
        if self.lent() {
            return self.lent_append(lend::TAG_BLOB, blob as u8, &[data], false);
        }
        match self.comp_blob_addr(blob) {
            Some(addr) => self.save_pair(addr, data),
            None => false,
        }
    }

    /// Erase the whole companion partition (factory reset).
    pub fn comp_erase(&mut self) -> bool {
        if self.lent() {
            // the partition holds the update: the data starts over where it is
            return self.write_rescue(&[]);
        }
        self.log_head = None;
        self.ring_head = None;
        match self.comp {
            Some((base, size)) => self.flash.erase(base, base + size).is_ok(),
            None => false,
        }
    }

    /// The first sector of the ring log.
    fn log_first(&self) -> u32 {
        COMP_TABLE_FIRST + if self.ring { RING_SECTORS } else { COMP_TABLE_SECTORS }
    }

    fn log_range(&self) -> Option<(u32, u32)> {
        let (base, size) = self.comp?;
        Some((base + self.log_first() * SECTOR, base + size))
    }

    /// Scan the ring log: candidate records `(seq, addr, len, crc)`.
    fn log_scan(&mut self) -> Vec<(u32, u32, u32, u32)> {
        let mut found = Vec::new();
        let Some((start, end)) = self.log_range() else {
            return found;
        };
        let mut addr = start;
        while addr + HEADER as u32 <= end {
            let Some(hdr) = self.read_words::<HEADER>(addr) else {
                break;
            };
            let word = |i: usize| u32::from_le_bytes(hdr[i * 4..i * 4 + 4].try_into().unwrap());
            let rec_end = addr as u64 + (HEADER as u64 + word(2) as u64).next_multiple_of(4);
            if word(0) == LOG_MAGIC && rec_end <= end as u64 {
                found.push((word(1), addr, word(2), word(3)));
                addr = rec_end as u32;
            } else {
                addr = (addr / SECTOR + 1) * SECTOR; // nothing here: try the next sector
            }
        }
        found
    }

    /// The newest complete snapshot in the ring log.
    pub fn comp_log_latest(&mut self) -> Option<Vec<u8>> {
        if self.lent() {
            return self.lent_load(lend::TAG_CONTACTS, 0).map(|d| lend::unpack(&d));
        }
        let mut found = self.log_scan();
        found.sort_by_key(|r| core::cmp::Reverse(r.0));
        if let (Some(&(seq, addr, len, _)), None) = (found.first(), self.log_head) {
            // appends continue after the newest record, even an interrupted one
            self.log_head = Some((addr + (HEADER as u32 + len).next_multiple_of(4), seq.wrapping_add(1)));
        }
        found.into_iter().find_map(|(_, addr, len, crc)| {
            let data = self.read_chunked(addr + HEADER as u32, len as usize)?;
            (crc32(&data) == crc).then_some(data)
        })
    }

    /// Append a snapshot to the ring log, wrapping around (and erasing ahead) as needed.
    pub fn comp_log_append(&mut self, data: &[u8]) -> bool {
        if self.lent() {
            return self.lent_append(lend::TAG_CONTACTS, 0, &[&lend::pack(data)], true);
        }
        let Some((start, end)) = self.log_range() else {
            return false;
        };
        if self.log_head.is_none() {
            self.comp_log_latest();
        }
        let (mut addr, seq) = self.log_head.unwrap_or((start, 1));
        let total = (HEADER + data.len()).next_multiple_of(4) as u32;
        if total > (end - start) / 2 {
            return false; // must leave room for the previous snapshot
        }
        if addr + total > end {
            addr = start;
        }
        // erase every sector the record enters from its start
        let mut s = addr.next_multiple_of(SECTOR);
        if addr % SECTOR == 0 {
            s = addr;
        }
        while s < addr + total {
            if !self.erase_sector(s) {
                return false;
            }
            s += SECTOR;
        }
        let mut hdr = [0u8; HEADER];
        hdr[..4].copy_from_slice(&LOG_MAGIC.to_le_bytes());
        hdr[4..8].copy_from_slice(&seq.to_le_bytes());
        hdr[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        hdr[12..].copy_from_slice(&crc32(data).to_le_bytes());
        self.log_head = Some((addr + total, seq.wrapping_add(1)));
        self.write_chunked(addr, &hdr) && self.write_chunked(addr + HEADER as u32, data)
    }

    // ------------------------------------------------------------ adverts

    pub fn advert_get(&mut self, key: &[u8; 32]) -> Option<Vec<u8>> {
        if self.lent() {
            self.lent_advert(key)
        } else if self.ring {
            let i = self.ring_find(key)?;
            self.slot_data(i)
        } else {
            self.table_get(key)
        }
    }

    pub fn advert_put(&mut self, key: &[u8; 32], data: &[u8]) -> bool {
        if data.len() > MAX_ADVERT_BLOB || self.comp.is_none() {
            return false;
        }
        if self.lent() {
            return self.lent_advert(key).as_deref() == Some(data)
                || self.lent_append(lend::TAG_ADVERT, 0, &[key, data], true);
        }
        if self.ring { self.ring_put(key, data) } else { self.table_put(key, data) }
    }

    pub fn advert_delete(&mut self, key: &[u8; 32]) {
        if self.lent() {
            if self.lent_advert(key).is_some() {
                self.lent_append(lend::TAG_ADVERT_GONE, 0, &[key], false);
            }
        } else if self.ring {
            while let Some(i) = self.ring_find(key) {
                if !self.mark_deleted(i) {
                    break;
                }
            }
        } else {
            self.table_delete(key)
        }
    }

    /// A slot's record: `[state][len][key 32][data]`.
    fn slot_head(&mut self, i: u32) -> Option<[u8; 36]> {
        let addr = self.slot_addr(i)?;
        self.read_words::<36>(addr)
    }

    fn slot_data(&mut self, i: u32) -> Option<Vec<u8>> {
        let rec = self.read_chunked(self.slot_addr(i)?, SLOT as usize)?;
        let len = (rec[1] as usize).min(MAX_ADVERT_BLOB);
        Some(rec[34..34 + len].to_vec())
    }

    fn mark_deleted(&mut self, i: u32) -> bool {
        let Some(addr) = self.slot_addr(i) else {
            return false;
        };
        let Some(mut w) = self.read_words::<4>(addr) else {
            return false;
        };
        w[0] = SLOT_DELETED; // only clears bits: programmable without an erase
        self.write_chunked(addr, &w)
    }

    // ------------------------------------------------------------ advert ring (with lending)

    fn ring_slots(&self) -> u32 {
        RING_SECTORS * SLOTS_PER_SECTOR
    }

    /// The slot the next advert goes to: the first blank one after a written one. The ring
    /// erases the sector it enters as soon as it enters it, so the blank slots run from there;
    /// a ring found without one (the power went before that erase) starts over at its start.
    fn ring_head(&mut self) -> u32 {
        if let Some(h) = self.ring_head {
            return h;
        }
        let n = self.ring_slots();
        let blank: Vec<bool> = (0..n)
            .map(|i| {
                let addr = self.slot_addr(i).unwrap_or(0);
                self.read_chunked(addr, SLOT as usize).is_some_and(|r| r.iter().all(|&b| b == 0xFF))
            })
            .collect();
        let head = match (0..n).find(|&i| blank[i as usize] && !blank[((i + n - 1) % n) as usize]) {
            Some(i) => i,
            None if blank.iter().all(|&b| b) => 0,
            None => {
                self.erase_ring_sector(0);
                0
            }
        };
        self.ring_head = Some(head);
        head
    }

    fn erase_ring_sector(&mut self, slot: u32) -> bool {
        match self.slot_addr(slot) {
            Some(addr) => self.erase_sector(addr - addr % SECTOR),
            None => false,
        }
    }

    /// The newest slot holding `key`, from the head backwards.
    fn ring_find(&mut self, key: &[u8; 32]) -> Option<u32> {
        let (n, head) = (self.ring_slots(), self.ring_head());
        (1..=n).map(|k| (head + n - k) % n).find(|&i| {
            self.slot_head(i).is_some_and(|h| h[0] == SLOT_USED && h[2..34] == key[..])
        })
    }

    fn ring_put(&mut self, key: &[u8; 32], data: &[u8]) -> bool {
        let old = self.ring_find(key);
        if let Some(i) = old
            && self.slot_data(i).as_deref() == Some(data)
        {
            return true; // unchanged: no flash wear
        }
        let (n, i) = (self.ring_slots(), self.ring_head());
        let Some(addr) = self.slot_addr(i) else {
            return false;
        };
        let mut rec = vec![0xFFu8; SLOT as usize];
        rec[1] = data.len() as u8;
        rec[2..34].copy_from_slice(key);
        rec[34..34 + data.len()].copy_from_slice(data);
        // the record, then its state: a slot cut short stays unused
        let mut ok = self.write_chunked(addr, &rec) && self.write_chunked(addr, &[SLOT_USED, rec[1], key[0], key[1]]);
        if let Some(old) = old {
            ok &= self.mark_deleted(old);
        }
        // the next sector is erased on entering it: the oldest adverts go
        let next = (i + 1) % n;
        if next.is_multiple_of(SLOTS_PER_SECTOR) {
            ok &= self.erase_ring_sector(next);
        }
        self.ring_head = Some(next);
        ok
    }

    /// Up to `max` adverts of the ring, newest first: `(key, data)`.
    fn ring_newest(&mut self, max: usize) -> Vec<([u8; 32], Vec<u8>)> {
        let (n, head) = (self.ring_slots(), self.ring_head());
        let mut out: Vec<([u8; 32], Vec<u8>)> = Vec::new();
        for i in (1..=n).map(|k| (head + n - k) % n) {
            if out.len() >= max {
                break;
            }
            if let Some(h) = self.slot_head(i).filter(|h| h[0] == SLOT_USED) {
                let key: [u8; 32] = h[2..34].try_into().unwrap();
                if !out.iter().any(|(k, _)| *k == key)
                    && let Some(d) = self.slot_data(i)
                {
                    out.push((key, d));
                }
            }
        }
        out
    }

    // ------------------------------------------------------------ advert table

    fn table_slots(&self) -> u32 {
        COMP_TABLE_SECTORS * SLOTS_PER_SECTOR
    }

    fn slot_addr(&self, i: u32) -> Option<u32> {
        let (base, _) = self.comp?;
        Some(base + (COMP_TABLE_FIRST + i / SLOTS_PER_SECTOR) * SECTOR + (i % SLOTS_PER_SECTOR) * SLOT)
    }

    /// Probe sequence for a key: `(slot, state, key matches)` until a free slot.
    fn find_slot(&mut self, key: &[u8; 32]) -> (Option<u32>, Option<u32>) {
        let n = self.table_slots();
        let start = u32::from_le_bytes(key[..4].try_into().unwrap()) % n;
        let mut first_reusable = None;
        for k in 0..n {
            let i = (start + k) % n;
            let Some(addr) = self.slot_addr(i) else {
                break;
            };
            let Some(head) = self.read_words::<36>(addr) else {
                break;
            };
            match head[0] {
                SLOT_FREE => return (None, first_reusable.or(Some(i))),
                SLOT_USED if head[2..34] == key[..] => return (Some(i), first_reusable),
                SLOT_DELETED if first_reusable.is_none() => first_reusable = Some(i),
                _ => {}
            }
        }
        (None, first_reusable)
    }

    fn table_get(&mut self, key: &[u8; 32]) -> Option<Vec<u8>> {
        let (found, _) = self.find_slot(key);
        self.slot_data(found?)
    }

    fn table_put(&mut self, key: &[u8; 32], data: &[u8]) -> bool {
        let mut rec = vec![0xFFu8; SLOT as usize];
        rec[0] = SLOT_USED;
        rec[1] = data.len() as u8;
        rec[2..34].copy_from_slice(key);
        rec[34..34 + data.len()].copy_from_slice(data);
        let (found, reusable) = self.find_slot(key);
        if let Some(i) = found {
            if self.table_get(key).as_deref() == Some(data) {
                return true; // unchanged: no flash wear
            }
            self.rewrite_slot(i, &rec)
        } else if let Some(i) = reusable {
            let addr = self.slot_addr(i).unwrap();
            match self.read_words::<4>(addr) {
                // a free slot can be programmed directly; a deleted one needs its sector rewritten
                Some(w) if w[0] == SLOT_FREE => self.write_chunked(addr, &rec),
                _ => self.rewrite_slot(i, &rec),
            }
        } else {
            false // table full
        }
    }

    fn table_delete(&mut self, key: &[u8; 32]) {
        if let (Some(i), _) = self.find_slot(key) {
            self.mark_deleted(i);
        }
    }

    /// Read-modify-write the sector holding slot `i`.
    fn rewrite_slot(&mut self, i: u32, rec: &[u8]) -> bool {
        let addr = self.slot_addr(i).unwrap();
        let sector = addr - addr % SECTOR;
        let Some(mut data) = self.read_chunked(sector, SECTOR as usize) else {
            return false;
        };
        let off = (addr - sector) as usize;
        data[off..off + SLOT as usize].copy_from_slice(rec);
        self.erase_sector(sector) && self.write_chunked(sector, &data)
    }

    /// Used and total size of the companion partition, KiB (of the area the data is in while
    /// the partition is lent).
    pub fn comp_usage_kb(&mut self) -> (u32, u32) {
        if let Some((used, size)) = self.lent_usage() {
            return (used / 1024, size / 1024);
        }
        let Some((_, size)) = self.comp else {
            return (0, 0);
        };
        let log = self.log_head.map_or(0, |(addr, _)| addr - self.log_range().map_or(0, |r| r.0));
        ((self.log_first() * SECTOR + log) / 1024, size / 1024)
    }
}

fn bytemuck_bytes(buf: &mut [u32; 64]) -> &mut [u8; 256] {
    // SAFETY: same size, u8 has no alignment requirement, any bit pattern is valid
    unsafe { &mut *(buf as *mut [u32; 64] as *mut [u8; 256]) }
}

#[cfg(test)]
mod tests {
    use embedded_storage::nor_flash::{ErrorType, NorFlashErrorKind};

    use super::*;

    /// A NOR flash in RAM: erase sets 0xFF, a write only clears bits.
    struct RamFlash(Vec<u8>);

    impl ErrorType for RamFlash {
        type Error = NorFlashErrorKind;
    }

    impl ReadNorFlash for RamFlash {
        const READ_SIZE: usize = 4;
        fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
            let at = offset as usize;
            bytes.copy_from_slice(self.0.get(at..at + bytes.len()).ok_or(NorFlashErrorKind::OutOfBounds)?);
            Ok(())
        }
        fn capacity(&self) -> usize {
            self.0.len()
        }
    }

    impl NorFlash for RamFlash {
        const WRITE_SIZE: usize = 4;
        const ERASE_SIZE: usize = SECTOR as usize;
        fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
            self.0.get_mut(from as usize..to as usize).ok_or(NorFlashErrorKind::OutOfBounds)?.fill(0xFF);
            Ok(())
        }
        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
            let at = offset as usize;
            let to = self.0.get_mut(at..at + bytes.len()).ok_or(NorFlashErrorKind::OutOfBounds)?;
            to.iter_mut().zip(bytes).for_each(|(t, b)| *t &= b);
            Ok(())
        }
    }

    // the Pro Micro's layout, from 0: overflow 2 sectors, a sector between, data 64 KiB
    const OVERFLOW: (u32, u32) = (0, 2 * SECTOR);
    const DATA: (u32, u32) = (3 * SECTOR, 16 * SECTOR);

    fn flash() -> RamFlash {
        RamFlash(vec![0x5A; (3 + 16) as usize * SECTOR as usize])
    }

    #[test]
    fn a_blob_past_the_data_region_goes_to_the_overflow() {
        let mut store = Store::with_regions(flash(), DATA, None).with_overflow(OVERFLOW);
        assert!(store.save(Blob::RoomAcl, b"acl"));
        assert!(store.save(Blob::Sensors, b"bme 76"));
        assert!(store.save(Blob::Sensors, b"bme 77"));
        assert_eq!(store.load(Blob::Sensors).as_deref(), Some(&b"bme 77"[..]));
        assert_eq!(store.load(Blob::RoomAcl).as_deref(), Some(&b"acl"[..]));
        // the sector between is left alone
        assert!(store.flash().0[2 * SECTOR as usize..3 * SECTOR as usize].iter().all(|&b| b == 0x5A));

        assert!(store.erase_all());
        assert_eq!(store.load(Blob::Sensors), None);
        assert_eq!(store.load(Blob::RoomAcl), None);
    }

    // lending, as on the Pro Micro (in sectors): data 16, the companion partition 86 (91 before
    // the ring), the area 5 right after the partition and 7 more after the old one's end
    const COMP_AT: u32 = 16 * SECTOR;
    const COMP: (u32, u32) = (COMP_AT, 86 * SECTOR);
    const LEGACY: u32 = 91 * SECTOR;
    const AREA: [(u32, u32); 2] = [(COMP_AT + 86 * SECTOR, 5 * SECTOR), (COMP_AT + 91 * SECTOR, 7 * SECTOR)];

    fn lending_flash() -> RamFlash {
        RamFlash(vec![0xFF; (16 + 98) as usize * SECTOR as usize])
    }

    fn lending(flash: RamFlash) -> Store<RamFlash> {
        Store::with_regions(flash, (0, 16 * SECTOR), Some(COMP)).with_lending(AREA, LEGACY)
    }

    fn key(i: u32) -> [u8; 32] {
        let mut k = [0u8; 32];
        k[..4].copy_from_slice(&i.to_le_bytes());
        k[31] = 0xA5;
        k
    }

    /// A contact list as the companion writes it: 151-byte records, mostly zeros past the name.
    fn contacts(n: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..n {
            let mut rec = [0u8; 151];
            rec[..32].copy_from_slice(&key(i));
            rec[32..40].copy_from_slice(b"node-abc");
            rec[131..135].copy_from_slice(&(1_700_000_000 + i).to_le_bytes());
            out.extend_from_slice(&rec);
        }
        out
    }

    /// An update fills the partition.
    fn update_over(store: &mut Store<RamFlash>) {
        store.flash().0[COMP.0 as usize..(COMP.0 + COMP.1) as usize].fill(0x3C);
    }

    #[test]
    fn the_ring_keeps_the_newest_adverts() {
        let mut store = lending(lending_flash());
        assert_eq!(store.mount(), Mount::Fresh);
        for i in 0..200 {
            assert!(store.advert_put(&key(i), &[i as u8; 100]));
        }
        // an advert again: one copy, the new one
        assert!(store.advert_put(&key(199), &[7; 90]));
        assert_eq!(store.advert_get(&key(199)), Some(vec![7; 90]));
        assert_eq!(store.advert_get(&key(198)), Some(vec![198; 100]));
        assert_eq!(store.advert_get(&key(0)), None, "the oldest went");
        store.advert_delete(&key(198));
        assert_eq!(store.advert_get(&key(198)), None);

        // after a restart the ring goes on where it was
        let mut store = lending(core::mem::replace(store.flash(), RamFlash(Vec::new())));
        assert_eq!(store.mount(), Mount::Ready);
        assert_eq!(store.advert_get(&key(199)), Some(vec![7; 90]));
        assert!(store.advert_put(&key(500), &[1; 10]));
        assert_eq!(store.advert_get(&key(197)), Some(vec![197; 100]));
        assert_eq!(store.advert_get(&key(500)), Some(vec![1; 10]));
    }

    #[test]
    fn data_lent_to_an_update_comes_back() {
        let mut store = lending(lending_flash());
        assert_eq!(store.mount(), Mount::Fresh);
        assert!(store.comp_save(CompBlob::Identity, &[1; 96]));
        assert!(store.comp_save(CompBlob::Prefs, b"name=a"));
        for i in 0..30 {
            assert!(store.advert_put(&key(i), &[i as u8; 120]));
        }
        assert!(store.comp_log_append(&contacts(350)));

        assert!(store.lend());
        update_over(&mut store);
        assert!(store.lent());
        assert_eq!(store.comp_load(CompBlob::Identity), Some(vec![1; 96]));
        assert_eq!(store.comp_log_latest(), Some(contacts(350)));
        assert_eq!(store.advert_get(&key(29)), Some(vec![29; 120]));
        assert_eq!(store.advert_get(&key(5)), None, "only the newest 20 move");

        // what changes meanwhile is kept
        assert!(store.comp_save(CompBlob::Prefs, b"name=b"));
        assert!(store.comp_log_append(&contacts(351)));
        assert!(store.advert_put(&key(77), &[77; 50]));
        store.advert_delete(&key(29));
        assert_eq!(store.comp_load(CompBlob::Prefs).as_deref(), Some(&b"name=b"[..]));
        assert_eq!(store.advert_get(&key(29)), None);

        // a restart meanwhile finds it there
        let mut store = lending(core::mem::replace(store.flash(), RamFlash(Vec::new())));
        assert_eq!(store.mount(), Mount::Lent);
        assert_eq!(store.comp_log_latest(), Some(contacts(351)));

        assert!(store.take_back());
        assert!(!store.lent());
        let mut store = lending(core::mem::replace(store.flash(), RamFlash(Vec::new())));
        assert_eq!(store.mount(), Mount::Ready);
        assert_eq!(store.comp_load(CompBlob::Identity), Some(vec![1; 96]));
        assert_eq!(store.comp_load(CompBlob::Prefs).as_deref(), Some(&b"name=b"[..]));
        assert_eq!(store.comp_log_latest(), Some(contacts(351)));
        assert_eq!(store.advert_get(&key(77)), Some(vec![77; 50]));
        assert_eq!(store.advert_get(&key(28)), Some(vec![28; 120]));
        assert_eq!(store.advert_get(&key(29)), None);
    }

    #[test]
    fn a_take_back_cut_short_is_done_again() {
        let mut store = lending(lending_flash());
        store.mount();
        assert!(store.comp_save(CompBlob::Identity, &[2; 96]));
        assert!(store.comp_log_append(&contacts(40)));
        assert!(store.lend());
        update_over(&mut store);
        // the partition half written back when the power went: the area still holds the data
        store.flash().0[COMP.0 as usize..(COMP.0 + 10 * SECTOR) as usize].fill(0xFF);
        let mut store = lending(core::mem::replace(store.flash(), RamFlash(Vec::new())));
        assert_eq!(store.mount(), Mount::Lent);
        assert!(store.take_back());
        assert_eq!(store.comp_load(CompBlob::Identity), Some(vec![2; 96]));
        assert_eq!(store.comp_log_latest(), Some(contacts(40)));
    }

    #[test]
    fn appends_stop_short_of_the_room_for_blobs() {
        let mut store = lending(lending_flash());
        store.mount();
        assert!(store.comp_save(CompBlob::Identity, &[3; 96]));
        assert!(store.lend());
        // contact lists that pack badly: the area fills, then the blobs still go in
        let noisy: Vec<u8> = (0..20_000u32).map(|i| (i % 251 + 1) as u8).collect();
        assert!(store.comp_log_append(&noisy));
        assert!(store.comp_log_append(&noisy));
        assert!(!store.comp_log_append(&noisy));
        assert!(store.comp_save(CompBlob::Clock, &[9; 4]));
        assert_eq!(store.comp_load(CompBlob::Clock), Some(vec![9; 4]));
        assert_eq!(store.comp_log_latest(), Some(noisy));
    }

    #[test]
    fn data_too_large_for_the_area_is_not_lent() {
        let mut store = lending(lending_flash());
        store.mount();
        let noisy: Vec<u8> = (0..60_000u32).map(|i| (i % 251 + 1) as u8).collect();
        assert!(store.comp_log_append(&noisy));
        assert!(!store.lend());
        assert!(!store.lent());
        assert_eq!(store.comp_log_latest(), Some(noisy));
    }

    #[test]
    fn the_layout_before_the_ring_moves_to_it() {
        // the partition as the companion laid it out before: advert table, log up to 91 sectors
        let mut old = Store::with_regions(lending_flash(), (0, 16 * SECTOR), Some((COMP_AT, LEGACY)));
        assert!(old.comp_save(CompBlob::Identity, &[4; 96]));
        assert!(old.comp_save(CompBlob::Channels, b"public"));
        for i in 0..40 {
            assert!(old.advert_put(&key(i), &[i as u8; 60]));
        }
        // enough snapshots for the newest to lie past the new end
        for n in 300..306 {
            assert!(old.comp_log_append(&contacts(n)));
        }
        let mut store = lending(core::mem::replace(old.flash(), RamFlash(Vec::new())));
        assert_eq!(store.mount(), Mount::Moved);
        assert_eq!(store.comp_load(CompBlob::Identity), Some(vec![4; 96]));
        assert_eq!(store.comp_load(CompBlob::Channels).as_deref(), Some(&b"public"[..]));
        assert_eq!(store.comp_log_latest(), Some(contacts(305)));
        assert_eq!((0..40).filter(|&i| store.advert_get(&key(i)).is_some()).count(), 20);
        let mut store = lending(core::mem::replace(store.flash(), RamFlash(Vec::new())));
        assert_eq!(store.mount(), Mount::Ready);
        assert_eq!(store.comp_log_latest(), Some(contacts(305)));
    }

    #[test]
    fn a_partition_with_something_else_in_it_starts_empty() {
        let mut flash = lending_flash();
        flash.0[COMP.0 as usize..(COMP.0 + COMP.1) as usize].fill(0x11);
        let mut store = lending(flash);
        assert_eq!(store.mount(), Mount::Fresh);
        assert_eq!(store.comp_load(CompBlob::Identity), None);
        assert_eq!(store.comp_log_latest(), None);
        let mut store = lending(core::mem::replace(store.flash(), RamFlash(Vec::new())));
        assert_eq!(store.mount(), Mount::Ready);
    }

    #[test]
    fn a_factory_reset_while_lent_leaves_the_update_alone() {
        let mut store = lending(lending_flash());
        store.mount();
        assert!(store.comp_save(CompBlob::Identity, &[5; 96]));
        assert!(store.lend());
        update_over(&mut store);
        assert!(store.comp_erase());
        assert!(store.lent());
        assert_eq!(store.comp_load(CompBlob::Identity), None);
        assert!(store.flash().0[COMP.0 as usize..(COMP.0 + COMP.1) as usize].iter().all(|&b| b == 0x3C));
    }

    #[test]
    fn without_an_overflow_a_blob_past_the_data_region_is_not_kept() {
        let mut store = Store::with_regions(flash(), DATA, None);
        assert!(!store.save(Blob::Sensors, b"bme 76"));
        assert_eq!(store.load(Blob::Sensors), None);
        assert!(store.erase_all());
        assert!(store.flash().0[..3 * SECTOR as usize].iter().all(|&b| b == 0x5A));
    }
}
