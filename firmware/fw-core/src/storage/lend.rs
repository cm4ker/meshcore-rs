//! Lending the companion partition to firmware updates, for a platform whose flash has no room
//! for both (the nRF52840 Pro Micro): an update comes into the partition, and the data moves
//! aside for that time.
//!
//! Aside is a small area of its own (two pieces of flash, read as one), where the data goes as
//! records one after the other, `[magic u32][tag u8][key u8][length u16][crc32 u32][data]`,
//! behind a header written last. While the header is there the data lives in the area: the store
//! reads the newest record of each kind and appends what changes (a contact list or an advert
//! only while room is left for the small blobs). Giving the partition back ([`Store::take_back`])
//! writes it anew from the records, then erases the header: a power loss on the way repeats it.
//!
//! With lending the partition keeps its adverts in a short ring (only the newest are worth
//! moving) and marks its layout (`CompBlob::Layout`): one laid out before, with the advert table,
//! moves to the ring by way of the area at the first start ([`Store::mount`]).

use alloc::vec::Vec;

use embedded_storage::nor_flash::NorFlash;
use log::{info, warn};

use super::{CompBlob, SLOT_USED, Store, crc32};

const HEADER_MAGIC: &[u8; 8] = b"MCRS-LNT";
const HEADER_LEN: u32 = 16;
const REC_MAGIC: u32 = 0x5252_434D; // "MCRR"
const REC_HEAD: u32 = 12;

pub(super) const TAG_BLOB: u8 = 1;
pub(super) const TAG_CONTACTS: u8 = 2;
/// An advert: its key, then the advert.
pub(super) const TAG_ADVERT: u8 = 3;
/// An advert deleted: its key.
pub(super) const TAG_ADVERT_GONE: u8 = 4;

/// The adverts that move with the data, the newest (upstream keeps 20 in all).
const ADVERTS_MOVED: usize = 20;
/// Room kept for the small blobs: contact lists and adverts stop short of it.
const RESERVE: u32 = 4096;
/// `CompBlob::Layout`: the adverts in a ring.
const LAYOUT_RING: &[u8] = b"ring";
const ALL_BLOBS: [CompBlob; 6] =
    [CompBlob::Identity, CompBlob::Prefs, CompBlob::Channels, CompBlob::Clock, CompBlob::Bonds, CompBlob::Layout];

pub(super) struct Lending {
    /// The area: two pieces `(address, length)`, sector-aligned, read as one.
    area: [(u32, u32); 2],
    /// The partition's size in the layout before the ring, to move it from.
    legacy_size: u32,
    /// While the partition is lent: where the next record goes, from the area's start.
    head: Option<u32>,
}

impl Lending {
    fn capacity(&self) -> u32 {
        self.area[0].1 + self.area[1].1
    }

    /// The address of `at` in the area, and the bytes left in its piece from there.
    fn locate(&self, at: u32) -> Option<(u32, u32)> {
        let [(a, n), (b, m)] = self.area;
        match at {
            at if at < n => Some((a + at, n - at)),
            at if at < n + m => Some((b + at - n, n + m - at)),
            _ => None,
        }
    }
}

/// What [`Store::mount`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mount {
    /// The data is as it was left.
    Ready,
    /// The partition is lent to an update: the data is in the area.
    Lent,
    /// Laid out before the ring: moved to it.
    Moved,
    /// Nothing of the companion's was there: the partition starts empty.
    Fresh,
    /// The flash failed to take the move.
    Failed,
}

/// A record in the area: its data's place and length, and the CRC it must have.
struct Rec {
    tag: u8,
    key: u8,
    at: u32,
    len: u32,
    crc: u32,
}

fn rec_size(len: u32) -> u32 {
    REC_HEAD + len.next_multiple_of(4)
}

/// Runs of zero bytes as `[0, run]`: names and paths in contact records are mostly zeros.
pub(super) fn pack(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2);
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0 {
            let run = data[i..].iter().take(255).take_while(|&&b| b == 0).count();
            out.extend_from_slice(&[0, run as u8]);
            i += run;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

pub(super) fn unpack(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 2);
    let mut it = data.iter();
    while let Some(&b) = it.next() {
        match b {
            0 => out.resize(out.len() + it.next().copied().unwrap_or(0) as usize, 0),
            b => out.push(b),
        }
    }
    out
}

impl<F: NorFlash> Store<F> {
    /// The companion partition is lent to firmware updates: its data moves to `area` (two
    /// pieces, `(address, length)`) meanwhile. `legacy_size`: the partition's size in the layout
    /// before (the advert table), moved from at the first start. [`Self::mount`] comes next.
    pub fn with_lending(mut self, area: [(u32, u32); 2], legacy_size: u32) -> Self {
        self.ring = true;
        self.lending = Some(Lending { area, legacy_size, head: None });
        self
    }

    /// At start, with lending: find where the data is, move it to this layout if it was laid out
    /// before, clear the partition if it holds nothing of the companion's.
    pub fn mount(&mut self) -> Mount {
        let Some((base, size)) = self.comp.filter(|_| self.lending.is_some()) else {
            return Mount::Ready;
        };
        if let Some((_, head)) = self.rescue_records() {
            self.lending.as_mut().unwrap().head = Some(head);
            return Mount::Lent;
        }
        if self.comp_load(CompBlob::Layout).is_some() {
            return Mount::Ready;
        }
        if self.comp_load(CompBlob::Identity).is_some() {
            info!("companion region: moving to the advert ring");
            return if self.lend_legacy() && self.take_back() { Mount::Moved } else { Mount::Failed };
        }
        // the repeater's firmware to go back to, an update it received, upstream's files
        let blank = (base..base + size)
            .step_by(256)
            .all(|a| self.read_words::<256>(a).is_some_and(|w| w.iter().all(|&b| b == 0xFF)));
        if !blank {
            info!("companion region: clearing what was there before");
        }
        match (blank || self.comp_erase()) && self.comp_save(CompBlob::Layout, LAYOUT_RING) {
            true => Mount::Fresh,
            false => Mount::Failed,
        }
    }

    /// The partition is lent: the data is in the area.
    pub fn lent(&self) -> bool {
        self.lending.as_ref().is_some_and(|l| l.head.is_some())
    }

    /// Move the data aside: the partition is the update's until [`Self::take_back`]. `false` if
    /// the data does not fit the area (or the flash failed): the partition stays as it is.
    pub fn lend(&mut self) -> bool {
        if self.lent() {
            return true;
        }
        if self.lending.is_none() {
            return false;
        }
        let mut recs = self.blob_records();
        for (key, data) in self.ring_newest(ADVERTS_MOVED).into_iter().rev() {
            recs.push((TAG_ADVERT, 0, [&key[..], &data].concat()));
        }
        if let Some(contacts) = self.comp_log_latest() {
            recs.push((TAG_CONTACTS, 0, pack(&contacts)));
        }
        self.write_rescue(&recs)
    }

    /// Give the partition back: write the data in it anew from the area, then let the area go.
    pub fn take_back(&mut self) -> bool {
        if !self.lent() {
            return true;
        }
        let Some((recs, _)) = self.rescue_records() else {
            return false;
        };
        let mut blobs = Vec::new();
        for b in ALL_BLOBS {
            let mut newest = recs.iter().rev().filter(|r| r.tag == TAG_BLOB && r.key == b as u8);
            if let Some(d) = newest.find_map(|r| self.rec_data(r)) {
                blobs.push((b, d));
            }
        }
        // the adverts as they stand, oldest first
        let mut adverts: Vec<Vec<u8>> = Vec::new();
        for r in recs.iter().filter(|r| matches!(r.tag, TAG_ADVERT | TAG_ADVERT_GONE) && r.len >= 32) {
            if let Some(d) = self.rec_data(r) {
                adverts.retain(|a| a[..32] != d[..32]);
                if r.tag == TAG_ADVERT {
                    adverts.push(d);
                }
            }
        }
        let skip = adverts.len().saturating_sub(ADVERTS_MOVED);
        let contacts = recs.iter().rev().filter(|r| r.tag == TAG_CONTACTS).find_map(|r| self.rec_data(r));
        // from here the store writes the partition
        let head = self.lending.as_mut().and_then(|l| l.head.take());
        let ok = self.comp_erase()
            && blobs.iter().all(|(b, d)| self.comp_save(*b, d))
            && self.comp_save(CompBlob::Layout, LAYOUT_RING)
            && adverts[skip..].iter().all(|a| self.ring_put(a[..32].try_into().unwrap(), &a[32..]))
            && contacts.is_none_or(|c| self.comp_log_append(&unpack(&c)));
        if !ok {
            warn!("companion region: could not write it back, the data stays aside");
            self.lending.as_mut().unwrap().head = head;
            return false;
        }
        // home again: the area's header goes (the rest is erased by the next lending)
        let header = self.lending.as_ref().unwrap().area[0].0;
        self.erase_sector(header)
    }

    /// What is used of the area and its size, while the partition is lent.
    pub(super) fn lent_usage(&self) -> Option<(u32, u32)> {
        let l = self.lending.as_ref()?;
        Some((l.head?, l.capacity()))
    }

    /// The newest intact record of a kind, while lent.
    pub(super) fn lent_load(&mut self, tag: u8, key: u8) -> Option<Vec<u8>> {
        let (recs, _) = self.rescue_records()?;
        recs.iter().rev().filter(|r| r.tag == tag && r.key == key).find_map(|r| self.rec_data(r))
    }

    /// An advert, while lent.
    pub(super) fn lent_advert(&mut self, key: &[u8; 32]) -> Option<Vec<u8>> {
        let (recs, _) = self.rescue_records()?;
        for r in recs.iter().rev().filter(|r| matches!(r.tag, TAG_ADVERT | TAG_ADVERT_GONE) && r.len >= 32) {
            if let Some(d) = self.rec_data(r)
                && d[..32] == key[..]
            {
                return (r.tag == TAG_ADVERT).then(|| d[32..].to_vec());
            }
        }
        None
    }

    /// Append a record of `parts`, while lent; `reserve`: only while room for the small blobs
    /// stays after it.
    pub(super) fn lent_append(&mut self, tag: u8, key: u8, parts: &[&[u8]], reserve: bool) -> bool {
        let Some((at, cap)) = self.lent_usage() else {
            return false;
        };
        let len: usize = parts.iter().map(|p| p.len()).sum();
        let size = rec_size(len as u32);
        if len > u16::MAX as usize || at + size + if reserve { RESERVE } else { 0 } > cap {
            return false;
        }
        // the next record goes after this one even if its write is cut short: its length skips it
        self.lending.as_mut().unwrap().head = Some(at + size);
        self.rescue_record(at, tag, key, &parts.concat())
    }

    /// The area's data anew: `recs` behind a fresh header, the partition lent from then on.
    pub(super) fn write_rescue(&mut self, recs: &[(u8, u8, Vec<u8>)]) -> bool {
        let Some(l) = self.lending.as_mut() else {
            return false;
        };
        let need = HEADER_LEN + recs.iter().map(|r| rec_size(r.2.len() as u32)).sum::<u32>();
        if need > l.capacity() {
            warn!("companion data: {} KB, more than the {} KB it has while an update comes", need / 1024, l.capacity() / 1024);
            return false;
        }
        // the header goes first: until the new one, the data is wherever it was
        l.head = None;
        let area = l.area;
        if !area.iter().all(|&(a, n)| self.flash.erase(a, a + n).is_ok()) {
            return false;
        }
        let mut at = HEADER_LEN;
        for (tag, key, data) in recs {
            if !self.rescue_record(at, *tag, *key, data) {
                return false;
            }
            at += rec_size(data.len() as u32);
        }
        let mut header = [0xFFu8; HEADER_LEN as usize];
        header[..8].copy_from_slice(HEADER_MAGIC);
        header[8..12].copy_from_slice(&1u32.to_le_bytes());
        if !self.rescue_write(0, &header) {
            return false;
        }
        self.lending.as_mut().unwrap().head = Some(at);
        true
    }

    /// The small blobs, as records.
    fn blob_records(&mut self) -> Vec<(u8, u8, Vec<u8>)> {
        ALL_BLOBS.iter().filter_map(|&b| Some((TAG_BLOB, b as u8, self.comp_load(b)?))).collect()
    }

    /// Lend the partition as laid out before the ring: the advert table and a longer log. A
    /// contact list too long for the area loses its tail (the adverts go first) rather than all.
    fn lend_legacy(&mut self) -> bool {
        let (Some((base, size)), Some(legacy)) = (self.comp, self.lending.as_ref().map(|l| l.legacy_size)) else {
            return false;
        };
        (self.comp, self.ring, self.log_head) = (Some((base, legacy)), false, None);
        let mut contacts = self.comp_log_latest();
        let mut adverts = self.table_newest(ADVERTS_MOVED);
        (self.comp, self.ring, self.log_head) = (Some((base, size)), true, None);
        let cap = self.lending.as_ref().unwrap().capacity();
        loop {
            let mut recs = self.blob_records();
            for (key, data) in &adverts {
                recs.push((TAG_ADVERT, 0, [&key[..], data].concat()));
            }
            if let Some(c) = &contacts {
                recs.push((TAG_CONTACTS, 0, pack(c)));
            }
            let need = HEADER_LEN + recs.iter().map(|r| rec_size(r.2.len() as u32)).sum::<u32>();
            if need <= cap {
                return self.write_rescue(&recs);
            }
            if !adverts.is_empty() {
                adverts.clear();
            } else if let Some(c) = contacts.as_mut().filter(|c| !c.is_empty()) {
                warn!("companion region: the contact list is cut to fit");
                c.truncate(c.len().saturating_sub(4096));
            } else {
                return false;
            }
        }
    }

    /// Up to `max` adverts of the table (laid out before the ring), in no order.
    fn table_newest(&mut self, max: usize) -> Vec<([u8; 32], Vec<u8>)> {
        let mut out = Vec::new();
        for i in 0..self.table_slots() {
            if out.len() >= max {
                break;
            }
            if let Some(h) = self.slot_head(i).filter(|h| h[0] == SLOT_USED)
                && let Some(d) = self.slot_data(i)
            {
                out.push((h[2..34].try_into().unwrap(), d));
            }
        }
        out
    }

    /// The area's records and where the next one goes, if its header is there.
    fn rescue_records(&mut self) -> Option<(Vec<Rec>, u32)> {
        let cap = self.lending.as_ref()?.capacity();
        if self.rescue_read(0, 8)?[..] != HEADER_MAGIC[..] {
            return None;
        }
        let mut recs = Vec::new();
        let mut at = HEADER_LEN;
        while at + REC_HEAD <= cap {
            let h = self.rescue_read(at, REC_HEAD as usize)?;
            let word = |i: usize| u32::from_le_bytes(h[i..i + 4].try_into().unwrap());
            if word(0) != REC_MAGIC {
                // the end; or a header cut short, which nothing may follow
                if h.iter().any(|&b| b != 0xFF) {
                    at = cap;
                }
                break;
            }
            let len = u16::from_le_bytes([h[6], h[7]]) as u32;
            if at + rec_size(len) > cap {
                at = cap;
                break;
            }
            recs.push(Rec { tag: h[4], key: h[5], at: at + REC_HEAD, len, crc: word(8) });
            at += rec_size(len);
        }
        Some((recs, at))
    }

    fn rec_data(&mut self, r: &Rec) -> Option<Vec<u8>> {
        let data = self.rescue_read(r.at, r.len as usize)?;
        (crc32(&data) == r.crc).then_some(data)
    }

    /// The record's head, then its data.
    fn rescue_record(&mut self, at: u32, tag: u8, key: u8, data: &[u8]) -> bool {
        let mut head = [0u8; REC_HEAD as usize];
        head[..4].copy_from_slice(&REC_MAGIC.to_le_bytes());
        head[4..6].copy_from_slice(&[tag, key]);
        head[6..8].copy_from_slice(&(data.len() as u16).to_le_bytes());
        head[8..].copy_from_slice(&crc32(data).to_le_bytes());
        self.rescue_write(at, &head) && self.rescue_write(at + REC_HEAD, data)
    }

    /// Write at a word-aligned `at` of the area, across its pieces.
    fn rescue_write(&mut self, mut at: u32, mut data: &[u8]) -> bool {
        while !data.is_empty() {
            let Some((addr, room)) = self.lending.as_ref().and_then(|l| l.locate(at)) else {
                return false;
            };
            let n = data.len().min(room as usize);
            if !self.write_chunked(addr, &data[..n]) {
                return false;
            }
            (at, data) = (at + n as u32, &data[n..]);
        }
        true
    }

    fn rescue_read(&mut self, mut at: u32, len: usize) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let (addr, room) = self.lending.as_ref()?.locate(at)?;
            let n = (len - out.len()).min(room as usize);
            out.extend_from_slice(&self.read_chunked(addr, n)?);
            at += n as u32;
        }
        Some(out)
    }
}
