//! Turning a delta into a plan as it comes, on the device that will install it.

use alloc::vec;
use alloc::vec::Vec;

use crate::{COPY, Header, LITERAL, MAX_PAGES, PAGE};

/// Builds a plan from a delta's operations, in the order of the new image: bytes made by adding
/// a difference to old ones ([`Planner::mix`]) and literal bytes ([`Planner::literal`]). Each
/// page's record goes to `out` once the page is full, to be written one after another from the
/// plan's second page on; the first page, the header, comes last ([`Planner::finish`]).
pub struct Planner {
    h: Header,
    /// The page being planned, and how much of it is.
    page: u32,
    filled: u32,
    record: Vec<u8>,
    /// Literal bytes on their way into the record as one operation.
    literal: Vec<u8>,
    starts: Vec<u32>,
}

impl Planner {
    /// A plan that turns an `old_len`-byte image into a `new_len`-byte one, moving the old one up
    /// by `shift` bytes first (a multiple of [`PAGE`], at least one page).
    pub fn new(old_len: u32, new_len: u32, shift: u32) -> Option<Self> {
        let pages = new_len.div_ceil(PAGE);
        if !shift.is_multiple_of(PAGE) || shift < PAGE || !(1..=MAX_PAGES).contains(&pages) {
            return None;
        }
        let h = Header { old_len, new_len, shift, pages, len: PAGE, new_sha256: [0; 32] };
        Some(Self { h, page: 0, filled: 0, record: Vec::new(), literal: Vec::new(), starts: Vec::new() })
    }

    /// The plan's length so far, its first page included.
    pub fn len(&self) -> u32 {
        self.h.len
    }

    /// `new.len()` bytes of the new image, made by adding `diff` to the old image's bytes from
    /// `old_off` on. `false`: more bytes than the new image has, or `out` failed.
    pub fn mix(&mut self, old_off: u32, diff: &[u8], new: &[u8], out: &mut impl FnMut(&[u8]) -> bool) -> bool {
        let mut done = 0usize;
        while done < new.len() {
            let Some(n) = self.room(new.len() - done) else {
                return false;
            };
            let from = old_off.saturating_add(done as u32);
            // before what the page may refer to, and past the old image: the bytes themselves
            let a = (self.h.lowest(self.page).saturating_sub(from) as usize).min(n);
            let b = (self.h.old_len.saturating_sub(from) as usize).clamp(a, n);
            self.literal.extend_from_slice(&new[done..done + a]);
            if b > a {
                self.flush_literal();
                self.copy(from + a as u32, &diff[done + a..done + b]);
            }
            self.literal.extend_from_slice(&new[done + b..done + n]);
            if !self.advance(n as u32, out) {
                return false;
            }
            done += n;
        }
        true
    }

    /// Literal bytes of the new image.
    pub fn literal(&mut self, new: &[u8], out: &mut impl FnMut(&[u8]) -> bool) -> bool {
        let mut done = 0usize;
        while done < new.len() {
            let Some(n) = self.room(new.len() - done) else {
                return false;
            };
            self.literal.extend_from_slice(&new[done..done + n]);
            if !self.advance(n as u32, out) {
                return false;
            }
            done += n;
        }
        true
    }

    /// The plan's first page, once every page of the new image is planned; `new_sha256`: the
    /// new image's hash, for the record.
    pub fn finish(mut self, new_sha256: [u8; 32]) -> Option<Vec<u8>> {
        if self.page != self.h.pages {
            return None;
        }
        self.h.new_sha256 = new_sha256;
        let mut first = vec![0u8; PAGE as usize];
        self.h.write(&self.starts, &mut first);
        Some(first)
    }

    /// How many of `want` bytes the current page still takes.
    fn room(&self, want: usize) -> Option<usize> {
        (self.page < self.h.pages).then(|| want.min((self.h.page_len(self.page) - self.filled) as usize))
    }

    /// `n` more bytes of the page are planned; a full page's record goes out.
    fn advance(&mut self, n: u32, out: &mut impl FnMut(&[u8]) -> bool) -> bool {
        self.filled += n;
        if self.filled < self.h.page_len(self.page) {
            return true;
        }
        self.flush_literal();
        self.starts.push(self.h.len);
        let ok = out(&self.record);
        self.h.len += self.record.len() as u32;
        self.record.clear();
        self.page += 1;
        self.filled = 0;
        ok
    }

    fn flush_literal(&mut self) {
        if !self.literal.is_empty() {
            self.record.push(LITERAL);
            self.record.extend_from_slice(&(self.literal.len() as u16).to_le_bytes());
            self.record.append(&mut self.literal);
        }
    }

    /// Old bytes from `from`, plus `diff`: runs of zeros, and runs of the rest (a lone zero stays
    /// in the run it is in: a new run would cost more).
    fn copy(&mut self, from: u32, diff: &[u8]) {
        self.record.push(COPY);
        self.record.extend_from_slice(&from.to_le_bytes());
        self.record.extend_from_slice(&(diff.len() as u16).to_le_bytes());
        let mut i = 0;
        while i < diff.len() {
            let zeros = diff[i..].iter().take_while(|&&d| d == 0).count();
            let start = i + zeros;
            let mut end = start;
            while end < diff.len() && (diff[end] != 0 || (end + 1 < diff.len() && diff[end + 1] != 0)) {
                end += 1;
            }
            put_varint(&mut self.record, zeros as u32);
            put_varint(&mut self.record, (end - start) as u32);
            self.record.extend_from_slice(&diff[start..end]);
            i = end;
        }
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}
