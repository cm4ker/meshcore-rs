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
//! Data always passes through a small stack buffer, so heap buffers may live anywhere.

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
    /// Pending firmware trial `[slot, previous slot, boots]` (see `ota.rs`).
    OtaTrial = 4,
    /// Region map (text form, see `meshcore::region`).
    Regions = 5,
    /// Hosted room server: private key (64 bytes) followed by its prefs (text form).
    Room = 6,
    /// Hosted room server's ACL.
    RoomAcl = 7,
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
}

/// Blobs in the `meshcore` partition (two sectors each).
const BLOBS: u32 = 8;

/// Companion partition layout, in sectors: small blobs, the advert table, then the ring log.
const COMP_BLOB_SECTORS: u32 = 16;
const COMP_TABLE_FIRST: u32 = COMP_BLOB_SECTORS;
const COMP_TABLE_SECTORS: u32 = 24;
const COMP_LOG_FIRST: u32 = COMP_TABLE_FIRST + COMP_TABLE_SECTORS;

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
    /// Ring log write position (offset in the partition) and next sequence number.
    log_head: Option<(u32, u32)>,
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
        Self { flash, base: data.0, size: data.1, comp, log_head: None }
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
        let off = blob as u32 * 2 * SECTOR;
        (off + 2 * SECTOR <= self.size).then_some([self.base + off, self.base + off + SECTOR])
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
        self.flash.erase(self.base, end).is_ok()
    }

    // ------------------------------------------------------- companion partition

    fn comp_blob_addr(&self, blob: CompBlob) -> Option<[u32; 2]> {
        let (base, _) = self.comp?;
        let off = blob as u32 * 2 * SECTOR;
        Some([base + off, base + off + SECTOR])
    }

    pub fn comp_load(&mut self, blob: CompBlob) -> Option<Vec<u8>> {
        let addr = self.comp_blob_addr(blob)?;
        self.newest(addr).0.map(|(_, d)| d).filter(|d| !d.is_empty())
    }

    pub fn comp_save(&mut self, blob: CompBlob, data: &[u8]) -> bool {
        match self.comp_blob_addr(blob) {
            Some(addr) => self.save_pair(addr, data),
            None => false,
        }
    }

    /// Erase the whole companion partition (factory reset).
    pub fn comp_erase(&mut self) -> bool {
        self.log_head = None;
        match self.comp {
            Some((base, size)) => self.flash.erase(base, base + size).is_ok(),
            None => false,
        }
    }

    fn log_range(&self) -> Option<(u32, u32)> {
        let (base, size) = self.comp?;
        Some((base + COMP_LOG_FIRST * SECTOR, base + size))
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

    pub fn advert_get(&mut self, key: &[u8; 32]) -> Option<Vec<u8>> {
        let (found, _) = self.find_slot(key);
        let addr = self.slot_addr(found?)?;
        let rec = self.read_chunked(addr, SLOT as usize)?;
        let len = (rec[1] as usize).min(MAX_ADVERT_BLOB);
        Some(rec[34..34 + len].to_vec())
    }

    pub fn advert_put(&mut self, key: &[u8; 32], data: &[u8]) -> bool {
        if data.len() > MAX_ADVERT_BLOB || self.comp.is_none() {
            return false;
        }
        let mut rec = vec![0xFFu8; SLOT as usize];
        rec[0] = SLOT_USED;
        rec[1] = data.len() as u8;
        rec[2..34].copy_from_slice(key);
        rec[34..34 + data.len()].copy_from_slice(data);
        let (found, reusable) = self.find_slot(key);
        if let Some(i) = found {
            if self.advert_get(key).as_deref() == Some(data) {
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

    pub fn advert_delete(&mut self, key: &[u8; 32]) {
        if let (Some(i), _) = self.find_slot(key)
            && let Some(addr) = self.slot_addr(i)
            && let Some(mut w) = self.read_words::<4>(addr)
        {
            w[0] = SLOT_DELETED; // only clears bits: programmable without an erase
            self.write_chunked(addr, &w);
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

    /// Used and total size of the companion partition, KiB.
    pub fn comp_usage_kb(&mut self) -> (u32, u32) {
        let Some((_, size)) = self.comp else {
            return (0, 0);
        };
        let log = self.log_head.map_or(0, |(addr, _)| addr - self.log_range().map_or(0, |r| r.0));
        ((COMP_LOG_FIRST * SECTOR + log) / 1024, size / 1024)
    }
}

fn bytemuck_bytes(buf: &mut [u32; 64]) -> &mut [u8; 256] {
    // SAFETY: same size, u8 has no alignment requirement, any bit pattern is valid
    unsafe { &mut *(buf as *mut [u32; 64] as *mut [u8; 256]) }
}
