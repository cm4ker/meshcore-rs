//! A firmware delta applied in place, over the firmware it replaces, for a board whose flash has
//! no room for a second image beside the app (the Heltec T114: no QSPI flash, and its internal
//! flash halved between the app and a staging area would cap the app at 340 KiB).
//!
//! The device receives the usual delta (a bsdiff patch against the running image) and, as it
//! comes, turns it into a plan ([`Planner`], feature `plan`): one record per page of the new image,
//! saying how to make that page from the old image and literal bytes. The installer ([`install`])
//! moves the old image up by the plan's shift, then writes the new image page by page from the
//! bottom, reading the old data where it moved to. A page only refers to old data that the pages
//! written before it have not overwritten, nor the page itself, so a page cut short by a power loss
//! is made again from the same data; where the delta refers to old data that would be gone by
//! then, the planner puts in the bytes it makes instead. The app's first page, which points the
//! bootloader at the installer until the end, is written last, from literal bytes only. Every step
//! is logged once done, and a power loss makes the installer go on from the first step not logged.
//!
//! The old firmware is gone afterwards; to go back to it, the device turns the plan around before
//! it installs it ([`reverse`], feature `plan`): a plan from the new image to the old one, made of
//! the new bytes the plan took from the old ones, the difference taken off again, and of the old
//! bytes themselves where the new image has nothing of them. It is installed the same way.

#![cfg_attr(not(test), no_std)]

#[cfg(feature = "plan")]
extern crate alloc;

#[cfg(feature = "plan")]
mod plan;
#[cfg(feature = "plan")]
pub use plan::{Planner, reverse};

pub const PAGE: u32 = 4096;
const WORDS: usize = (PAGE / 4) as usize;

/// The plan's first page: `[magic 4][version u8][0 u8 x3][old_len u32][new_len u32][shift u32]
/// [pages u32][plan length u32][SHA-256 of the new image 32][where page i's record starts u32 x
/// pages]`, offsets from the plan's start; the records follow from the plan's second page on.
pub const MAGIC: &[u8; 4] = b"MCIP";
const VERSION: u8 = 1;
const FIXED: usize = 60;
/// Pages a plan's first page has room to index: images up to 4 MiB.
pub const MAX_PAGES: u32 = (PAGE - FIXED as u32) / 4;

/// A record's operations, until the page is full:
/// - `[COPY][old offset u32][len u16]`, then the difference to add to those old bytes, byte by
///   byte: `[zeros varint][n varint][n bytes]` until `len` bytes are made;
/// - `[LITERAL][len u16][len bytes]`.
const COPY: u8 = 1;
const LITERAL: u8 = 2;

/// The install request, in its own page: `[magic 8][where the plan starts in the staging area
/// u32][its complement u32][plan length u32][its complement u32][length of the image it replaces
/// u32][its complement u32]`, then the log of steps done (a zeroed word each).
pub const REQUEST_MAGIC: &[u8; 8] = b"MCRS-IP1";
const LOG: u32 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub old_len: u32,
    pub new_len: u32,
    /// How far the old image moves up first: a multiple of [`PAGE`].
    pub shift: u32,
    pub pages: u32,
    /// The plan's length, its first page included.
    pub len: u32,
    pub new_sha256: [u8; 32],
}

impl Header {
    pub fn parse(page: &[u8]) -> Option<Self> {
        let u32_at = |i: usize| u32::from_le_bytes(page[i..i + 4].try_into().unwrap());
        if page.len() < FIXED || &page[..4] != MAGIC || page[4] != VERSION {
            return None;
        }
        let h = Self {
            old_len: u32_at(8),
            new_len: u32_at(12),
            shift: u32_at(16),
            pages: u32_at(20),
            len: u32_at(24),
            new_sha256: page[28..60].try_into().unwrap(),
        };
        let ok = h.shift.is_multiple_of(PAGE)
            && h.shift >= PAGE
            && h.len >= PAGE
            && h.pages == h.new_len.div_ceil(PAGE)
            && (1..=MAX_PAGES).contains(&h.pages)
            && page.len() >= FIXED + 4 * h.pages as usize;
        ok.then_some(h)
    }

    pub fn write(&self, starts: &[u32], page: &mut [u8]) {
        page.fill(0);
        page[..4].copy_from_slice(MAGIC);
        page[4] = VERSION;
        for (i, v) in [self.old_len, self.new_len, self.shift, self.pages, self.len].into_iter().enumerate() {
            page[8 + 4 * i..12 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        page[28..60].copy_from_slice(&self.new_sha256);
        for (i, s) in starts.iter().enumerate() {
            page[FIXED + 4 * i..FIXED + 4 * i + 4].copy_from_slice(&s.to_le_bytes());
        }
    }

    /// Where page `p`'s record is in the plan whose first page is `first`.
    pub fn record(&self, first: &[u8], p: u32) -> Option<(u32, u32)> {
        let start = |i: u32| u32::from_le_bytes(first[FIXED + 4 * i as usize..FIXED + 4 * i as usize + 4].try_into().unwrap());
        let from = start(p);
        let to = if p + 1 < self.pages { start(p + 1) } else { self.len };
        (PAGE <= from && from <= to && to <= self.len).then_some((from, to))
    }

    /// The bytes of page `p` of the new image.
    pub fn page_len(&self, p: u32) -> u32 {
        (self.new_len - p * PAGE).min(PAGE)
    }

    /// The lowest old offset page `p` may refer to: the old data below it is overwritten by the
    /// time page `p` is written, or by the page itself. Page 0 goes last: none.
    pub fn lowest(&self, p: u32) -> u32 {
        if p == 0 { u32::MAX } else { ((p + 1) * PAGE).saturating_sub(self.shift) }
    }
}

fn varint(data: &[u8], at: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    for shift in [0, 7, 14] {
        let b = *data.get(*at)?;
        *at += 1;
        v |= ((b & 0x7F) as u32) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Make page `p` of the new image into `out` (its [`Header::page_len`] bytes) from its record,
/// reading the old image with `old(offset, buf)`. `None`: the record is malformed, or refers to
/// old data the page may not ([`Header::lowest`]) or that the old image does not have.
pub fn make_page(h: &Header, p: u32, record: &[u8], old: &mut impl FnMut(u32, &mut [u8]) -> bool, out: &mut [u8]) -> Option<()> {
    let len = h.page_len(p) as usize;
    let out = out.get_mut(..len)?;
    let (mut at, mut pos) = (0usize, 0usize);
    let u16_at = |at: usize| Some(u16::from_le_bytes(record.get(at..at + 2)?.try_into().ok()?) as usize);
    while pos < len {
        let op = *record.get(at)?;
        match op {
            COPY => {
                let from = u32::from_le_bytes(record.get(at + 1..at + 5)?.try_into().ok()?);
                let n = u16_at(at + 5)?;
                at += 7;
                if n == 0 || pos + n > len || from < h.lowest(p) || from as u64 + n as u64 > h.old_len as u64 {
                    return None;
                }
                let dst = &mut out[pos..pos + n];
                if !old(from, dst) {
                    return None;
                }
                let mut k = 0;
                while k < n {
                    k += varint(record, &mut at)? as usize;
                    let m = varint(record, &mut at)? as usize;
                    let diff = record.get(at..at + m)?;
                    for (d, x) in dst.get_mut(k..k + m)?.iter_mut().zip(diff) {
                        *d = d.wrapping_add(*x);
                    }
                    at += m;
                    k += m;
                }
                if k != n {
                    return None;
                }
                pos += n;
            }
            LITERAL => {
                let n = u16_at(at + 1)?;
                at += 3;
                if n == 0 || pos + n > len {
                    return None;
                }
                out[pos..pos + n].copy_from_slice(record.get(at..at + n)?);
                at += n;
                pos += n;
            }
            _ => return None,
        }
    }
    (at == record.len()).then_some(())
}

/// One operation of a record.
#[derive(Clone, Copy)]
pub(crate) enum Op<'a> {
    /// `len` old bytes from `from`, plus the difference coded in `diff` (runs, see [`COPY`]).
    Copy { from: u32, len: usize, diff: &'a [u8] },
    Literal(&'a [u8]),
}

/// The operation of `record` at `at`, and where the next one is.
pub(crate) fn next_op(record: &[u8], at: usize) -> Option<(Op<'_>, usize)> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(record.get(at..at + 2)?.try_into().ok()?) as usize);
    match *record.get(at)? {
        COPY => {
            let from = u32::from_le_bytes(record.get(at + 1..at + 5)?.try_into().ok()?);
            let len = u16_at(at + 5)?;
            let start = at + 7;
            let (mut i, mut k) = (start, 0usize);
            while k < len {
                k += varint(record, &mut i)? as usize;
                let m = varint(record, &mut i)? as usize;
                i += m;
                k += m;
            }
            (k == len && i <= record.len()).then_some((Op::Copy { from, len, diff: &record[start..i] }, i))
        }
        LITERAL => {
            let len = u16_at(at + 1)?;
            Some((Op::Literal(record.get(at + 3..at + 3 + len)?), at + 3 + len))
        }
        _ => None,
    }
}

/// The difference `diff` codes, into `out` (as long as the copy it belongs to).
pub(crate) fn decode_diff(diff: &[u8], out: &mut [u8]) -> Option<()> {
    out.fill(0);
    let (mut i, mut k) = (0usize, 0usize);
    while k < out.len() {
        k += varint(diff, &mut i)? as usize;
        let m = varint(diff, &mut i)? as usize;
        out.get_mut(k..k + m)?.copy_from_slice(diff.get(i..i + m)?);
        i += m;
        k += m;
    }
    Some(())
}

/// The image a plan makes, read from anywhere in it without making it all: the pages it takes
/// are made from the plan as they are read, the last one kept.
pub struct Rebuilt<'a, R: FnMut(u32, &mut [u8]) -> bool> {
    h: Header,
    plan: &'a [u8],
    /// Reads the image the plan is made from.
    old: R,
    page: [u8; PAGE as usize],
    made: Option<u32>,
}

impl<'a, R: FnMut(u32, &mut [u8]) -> bool> Rebuilt<'a, R> {
    pub fn new(plan: &'a [u8], old: R) -> Option<Self> {
        let h = Header::parse(plan.get(..PAGE as usize)?)?;
        Some(Self { h, plan, old, page: [0; PAGE as usize], made: None })
    }

    /// `out.len()` bytes of the image from `at`.
    pub fn read(&mut self, mut at: u32, out: &mut [u8]) -> bool {
        let mut done = 0;
        while done < out.len() {
            let p = at / PAGE;
            if p >= self.h.pages {
                return false;
            }
            if self.made != Some(p) {
                let Some((from, to)) = self.h.record(self.plan, p) else {
                    return false;
                };
                if make_page(&self.h, p, &self.plan[from as usize..to as usize], &mut self.old, &mut self.page).is_none() {
                    return false;
                }
                self.made = Some(p);
            }
            let k = (at % PAGE) as usize;
            let n = (out.len() - done).min(self.h.page_len(p) as usize - k);
            out[done..done + n].copy_from_slice(&self.page[k..k + n]);
            done += n;
            at += n as u32;
        }
        true
    }
}

/// The internal flash: memory-mapped reads, page erases, word writes (bits go from 1 to 0 only).
pub trait Flash {
    fn read(&self, addr: u32) -> u32;
    fn erase(&mut self, page: u32);
    fn write(&mut self, addr: u32, word: u32);
    /// Long work in between: the watchdog.
    fn feed(&mut self) {}
}

/// Where things are.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    /// The app's flash: its first page is the bootloader's way in.
    pub app: u32,
    pub app_end: u32,
    /// The plan.
    pub plan: u32,
    /// The app's first page, saved by the firmware before it pointed that page at the installer.
    pub saved_first: u32,
    /// The install request and its log.
    pub request: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The new firmware is in place.
    Installed,
    /// Nothing to install: the firmware that ran is back as it was.
    Kept,
    /// The flash did not take a page, or there is no firmware to start: a UF2 over USB is needed.
    Broken,
}

/// The request's plan (where it starts in the staging area, its length) and the old image's
/// length, if it is whole.
fn request(f: &impl Flash, l: &Layout) -> Option<(u32, u32, u32)> {
    let rec = |i: u32| f.read(l.request + 4 * i);
    let magic = |i: usize| u32::from_le_bytes(REQUEST_MAGIC[4 * i..4 * i + 4].try_into().unwrap());
    let (at, plan_len, old_len) = (rec(2), rec(4), rec(6));
    let whole = rec(0) == magic(0) && rec(1) == magic(1) && rec(3) == !at && rec(5) == !plan_len && rec(7) == !old_len;
    whole.then_some((at, plan_len, old_len))
}

/// The request for the plan of `plan_len` bytes at `at` in the staging area that replaces an
/// image of `old_len` bytes, as the firmware writes it (word by word into an erased page).
pub fn request_words(at: u32, plan_len: u32, old_len: u32) -> [u32; 8] {
    let magic = |i: usize| u32::from_le_bytes(REQUEST_MAGIC[4 * i..4 * i + 4].try_into().unwrap());
    [magic(0), magic(1), at, !at, plan_len, !plan_len, old_len, !old_len]
}

fn read_bytes(f: &impl Flash, addr: u32, buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        let a = addr + i as u32;
        *b = (f.read(a & !3) >> (8 * (a & 3))) as u8;
    }
}

/// Install the plan the request names, going on from the first step not logged. `staging`: the
/// staging area's bytes, from [`Layout::plan`] on (memory-mapped on the device).
pub fn install(f: &mut impl Flash, l: &Layout, staging: &[u8]) -> Outcome {
    let Some((at, plan_len, old_len)) = request(f, l) else {
        return restore(f, l);
    };
    let Some(plan) = staging.get(at as usize..) else {
        return restore(f, l);
    };
    let Some(h) = plan.get(..PAGE as usize).and_then(Header::parse) else {
        return restore(f, l);
    };
    let room = l.app_end - l.app;
    if h.len != plan_len || plan.len() < plan_len as usize || h.old_len != old_len || old_len + h.shift > room || h.new_len > room {
        return restore(f, l);
    }
    let old_pages = old_len.div_ceil(PAGE);
    let steps = old_pages + h.pages;
    let done = (0..steps).take_while(|&s| f.read(l.request + LOG + 4 * s) == 0).count() as u32;
    let mut buf = [0u32; WORDS];
    for s in done..steps {
        f.feed();
        let ok = if s < old_pages {
            // the old image moves up, its top page first; its first page is the saved one
            let k = old_pages - 1 - s;
            let from = if k == 0 { l.saved_first } else { l.app + k * PAGE };
            for (i, w) in buf.iter_mut().enumerate() {
                *w = f.read(from + 4 * i as u32);
            }
            put_page(f, l.app + h.shift + k * PAGE, &buf)
        } else {
            // the new image from its second page up, then its first
            let i = s - old_pages;
            let p = if i + 1 < h.pages { i + 1 } else { 0 };
            let Some((from, to)) = h.record(plan, p) else {
                return Outcome::Broken;
            };
            buf.fill(0xFFFF_FFFF);
            // SAFETY: the buffer's words as bytes
            let bytes = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), PAGE as usize) };
            let moved = l.app + h.shift;
            let mut old = |at: u32, out: &mut [u8]| {
                read_bytes(f, moved + at, out);
                true
            };
            if make_page(&h, p, &plan[from as usize..to as usize], &mut old, bytes).is_none() {
                return Outcome::Broken;
            }
            put_page(f, l.app + p * PAGE, &buf)
        };
        if !ok {
            return Outcome::Broken;
        }
        f.write(l.request + LOG + 4 * s, 0);
    }
    Outcome::Installed
}

/// Nothing was done: put back the app's first page, if the firmware saved one.
fn restore(f: &mut impl Flash, l: &Layout) -> Outcome {
    let (sp, reset) = (f.read(l.saved_first), f.read(l.saved_first + 4));
    let is_app = (0x2000_0000..=0x2004_0000).contains(&sp) && (l.app..l.app_end).contains(&(reset & !1));
    let mut buf = [0u32; WORDS];
    for (i, w) in buf.iter_mut().enumerate() {
        *w = f.read(l.saved_first + 4 * i as u32);
    }
    if is_app && put_page(f, l.app, &buf) { Outcome::Kept } else { Outcome::Broken }
}

/// Bring the page at `to` in line with `words`; false if it does not read back.
fn put_page(f: &mut impl Flash, to: u32, words: &[u32; WORDS]) -> bool {
    if holds(f, to, words) {
        return true; // done before the power went
    }
    f.erase(to);
    // last word first: the app's first page counts as written once its first word (the stack
    // pointer) is
    for i in (0..WORDS).rev() {
        if words[i] != 0xFFFF_FFFF {
            f.write(to + 4 * i as u32, words[i]);
        }
    }
    holds(f, to, words)
}

fn holds(f: &impl Flash, at: u32, words: &[u32; WORDS]) -> bool {
    (0..WORDS).all(|i| f.read(at + 4 * i as u32) == words[i])
}

#[cfg(test)]
mod tests;
