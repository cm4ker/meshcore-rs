//! Turning a delta into a plan as it comes, on the device that will install it.

use alloc::vec;
use alloc::vec::Vec;

use crate::{COPY, Header, LITERAL, MAX_PAGES, Op, PAGE, next_op};

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
    /// Adjacent `mix` calls on contiguous old bytes merged into one COPY operation.
    copy_from: u32,
    copy_diff: Vec<u8>,
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
        Some(Self {
            h,
            page: 0,
            filled: 0,
            record: Vec::new(),
            literal: Vec::new(),
            copy_from: 0,
            copy_diff: Vec::new(),
            starts: Vec::new(),
        })
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
            if a > 0 {
                self.flush_copy();
                self.literal.extend_from_slice(&new[done..done + a]);
            }
            if b > a {
                self.flush_literal();
                self.push_copy(from + a as u32, &diff[done + a..done + b]);
            }
            if b < n {
                self.flush_copy();
                self.literal.extend_from_slice(&new[done + b..done + n]);
            }
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
            self.flush_copy();
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
        self.flush_copy();
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

    /// Buffer a copy operation so adjacent `mix` chunks (e.g. a 256-byte patcher buffer across a
    /// long bsdiff mix) coalesce into one COPY per page instead of 16.
    fn push_copy(&mut self, from: u32, diff: &[u8]) {
        if !self.copy_diff.is_empty() && self.copy_from.checked_add(self.copy_diff.len() as u32) == Some(from) {
            self.copy_diff.extend_from_slice(diff);
            return;
        }
        self.flush_copy();
        self.copy_from = from;
        self.copy_diff.extend_from_slice(diff);
    }

    fn flush_copy(&mut self) {
        if self.copy_diff.is_empty() {
            return;
        }
        self.emit_copy(self.copy_from);
        self.copy_diff.clear();
    }

    /// Old bytes from `from`, plus `self.copy_diff`: runs of zeros, and runs of the rest (a lone
    /// zero stays in the run it is in: a new run would cost more).
    fn emit_copy(&mut self, from: u32) {
        let diff = &self.copy_diff;
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

/// One run of old bytes the new image took, compactly (16 bytes) for `reverse`.
#[derive(Clone, Copy)]
struct Run {
    old: u32,
    new: u32,
    diff_start: u32,
    len: u16,
    diff_len: u16,
}

/// The plan back: from the image `forward` makes to `old`, the one it is made from, for a
/// rollback. Every new byte `forward` takes from an old one (plus a difference) gives that old
/// byte back (minus the difference); old bytes the new image has nothing of go in as themselves.
/// `old_sha256`: the old image's hash, for the record. Its records go to `out` as a [`Planner`]'s
/// do; its first page comes back.
pub fn reverse(forward: &[u8], old: &[u8], old_sha256: [u8; 32], out: &mut impl FnMut(&[u8]) -> bool) -> Option<Vec<u8>> {
    let h = Header::parse(forward.get(..PAGE as usize)?)?;
    if old.len() != h.old_len as usize {
        return None;
    }
    // count runs first so one exact reservation holds them all: no doubling or peak copy on the heap
    let mut count = 0usize;
    for p in 0..h.pages {
        let (from, to) = h.record(forward, p)?;
        let record = &forward[from as usize..to as usize];
        let mut at = 0usize;
        while at < record.len() {
            let (op, next) = next_op(record, at)?;
            if matches!(op, Op::Copy { .. }) {
                count += 1;
            }
            at = next;
        }
    }
    let mut runs: Vec<Run> = Vec::new();
    runs.try_reserve_exact(count).ok()?;
    for p in 0..h.pages {
        let (from, to) = h.record(forward, p)?;
        let record = &forward[from as usize..to as usize];
        let (mut at, mut pos) = (0usize, p * PAGE);
        while at < record.len() {
            let (op, next) = next_op(record, at)?;
            match op {
                Op::Copy { from: o, len, diff } => {
                    let start = diff.as_ptr() as usize - forward.as_ptr() as usize;
                    runs.push(Run {
                        old: o,
                        new: pos,
                        diff_start: u32::try_from(start).ok()?,
                        len: u16::try_from(len).ok()?,
                        diff_len: u16::try_from(diff.len()).ok()?,
                    });
                    pos += len as u32;
                }
                Op::Literal(bytes) => pos += bytes.len() as u32,
            }
            at = next;
        }
    }
    // In-place heapsort by `r.old`: O(n log n) worst-case, ~150 B instead of core's ~2.5 KiB ipnsort.
    fn sift_down(s: &mut [Run], mut root: usize) {
        let n = s.len();
        loop {
            let mut child = root * 2 + 1;
            if child >= n {
                break;
            }
            if child + 1 < n && s[child + 1].old > s[child].old {
                child += 1;
            }
            if s[root].old >= s[child].old {
                break;
            }
            s.swap(root, child);
            root = child;
        }
    }
    for i in (0..runs.len() / 2).rev() {
        sift_down(&mut runs, i);
    }
    for end in (1..runs.len()).rev() {
        runs.swap(0, end);
        sift_down(&mut runs[..end], 0);
    }
    let mut back = Planner::new(h.new_len, h.old_len, h.shift)?;
    let mut diff = Vec::new();
    diff.try_reserve_exact(PAGE as usize).ok()?;
    diff.resize(PAGE as usize, 0);
    let (mut u, mut i) = (0u32, 0usize);
    while u < h.old_len {
        while runs.get(i).is_some_and(|r| r.old + r.len as u32 <= u) {
            i += 1;
        }
        match runs.get(i) {
            Some(&r) if r.old <= u => {
                let (k, n) = ((u - r.old) as usize, r.len as usize);
                let start = r.diff_start as usize;
                let end = start + r.diff_len as usize;
                crate::decode_diff(&forward[start..end], &mut diff[..n])?;
                for d in &mut diff[k..n] {
                    *d = d.wrapping_neg();
                }
                if !back.mix(r.new + k as u32, &diff[k..n], &old[u as usize..u as usize + n - k], out) {
                    return None;
                }
                u += (n - k) as u32;
            }
            next => {
                let end = next.map_or(h.old_len, |r| r.old);
                if !back.literal(&old[u as usize..end as usize], out) {
                    return None;
                }
                u = end;
            }
        }
    }
    drop(runs);
    drop(diff);
    back.finish(old_sha256)
}
