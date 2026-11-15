//! How the T-Echo's installer puts a staged update in place, apart from the hardware (`main.rs`),
//! so it runs on the host too: `cargo test --lib --target <host triple>`.
//!
//! The update waits in the staging area of the QSPI flash, verified by the firmware, which also
//! saved the first page of its own app flash at [`SAVED_FIRST`], then pointed that page at the
//! installer and reset. The installer swaps the image with the app flash page by page ([`step`]),
//! as the Pro Micro's does within its internal flash, so the firmware that ran ends up in the
//! staging area, whole: `fw rollback` (or a new firmware that fails its trial boots) puts it back
//! the same way. The staging area is larger than the app flash, and the swap needs one spare page
//! of it: no page is ever only in RAM.
//!
//! Each step is logged in the request's page once done. A power loss makes the next boot (the
//! app's first page still points at the installer) go on from the first step not logged; a step
//! cut short is done again, its source untouched. Only the last step, the app's first page
//! itself, is exposed for a few milliseconds: written last word first, it reads as no app until
//! complete, and the bootloader then stays in its USB drive mode.

#![cfg_attr(not(test), no_std)]

use sha2::{Digest, Sha256};

// shared with the firmware (../src/ota.rs)
pub const APP_START: u32 = 0x26000;
pub const APP_END: u32 = 0xED000;
/// The QSPI flash as [`Flash`] addresses it: its offsets from here up.
pub const QSPI: u32 = 0x1200_0000;
pub const STAGING: u32 = QSPI + 0x11_0000;
/// The first page of the app the firmware leaves: it points that page at the installer.
pub const SAVED_FIRST: u32 = QSPI + 0x1F_E000;
/// The request: `[magic 8][image length u32][its complement u32][length of the running image
/// u32][its complement u32][reserved 8]`, then the log of steps done (a zeroed word each).
pub const RECORD: u32 = QSPI + 0x1F_F000;
pub const RECORD_MAGIC: &[u8; 8] = b"MCRS-IN2";
const LOG: u32 = RECORD + 32;
const TRAILER_MAGIC: &[u8; 8] = b"MCRS-NRF";
const TRAILER_LEN: u32 = 64;
const CHIP: u32 = 0x52840;

pub const PAGE: u32 = 4096;
pub const WORDS: usize = (PAGE / 4) as usize;

/// The app flash (below [`QSPI`]) and the QSPI flash: page erases, and writes that only clear bits.
pub trait Flash {
    fn read(&mut self, addr: u32, out: &mut [u32]);
    fn erase(&mut self, page: u32);
    /// Within a page, the first word last: the app's first page counts as written once its first
    /// word (the stack pointer) is.
    fn write(&mut self, addr: u32, words: &[u32]);
    /// Long work in between: the watchdog.
    fn feed(&mut self) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The new firmware is in place.
    Installed,
    /// Nothing valid to install: the firmware that ran is back as it was.
    Kept,
    /// The flash did not take a page, or there is no firmware to start: a UF2 over USB is needed.
    Broken,
}

pub fn install(f: &mut impl Flash) -> Outcome {
    let Some((total, running)) = request(f) else {
        return restore(f);
    };
    let m = total.div_ceil(PAGE).max(running.div_ceil(PAGE));
    let steps = 3 * m;
    let mut log = [0u32; 3 * ((APP_END - APP_START) / PAGE) as usize];
    f.read(LOG, &mut log[..steps as usize]);
    let done = log[..steps as usize].iter().take_while(|&&w| w == 0).count() as u32;
    // the first step writes past the image: until a step is logged, the image is as verified
    if done == 0 && !image_ok(f, total) {
        return restore(f);
    }
    let mut buf = [0u32; WORDS];
    for s in done..steps {
        let (to, from) = step(s, m);
        if !copy_page(f, to, from, &mut buf) {
            return Outcome::Broken;
        }
        f.write(LOG + 4 * s, &[0]);
    }
    Outcome::Installed
}

/// Where step `s` of a swap over `m` pages copies a page: `(to, from)`, flash addresses.
/// - The image moves up one page in the staging area, pages `m - 1` down to 1 (`m - 1` steps).
/// - For each app page `k` from 1 on: the app's page into the staging page freed by the step
///   before, then the new page into the app (`2 (m - 1)` steps).
/// - The new first page goes to the spare page after the old firmware, the old firmware's first
///   page (saved by the firmware) to the head of the staging area, and the new first page into
///   the app: from then on the bootloader starts the new firmware.
pub fn step(s: u32, m: u32) -> (u32, u32) {
    let app = |k: u32| APP_START + k * PAGE;
    let staged = |k: u32| STAGING + k * PAGE;
    let moves = m - 1;
    if s < moves {
        let k = m - 1 - s;
        (staged(k + 1), staged(k))
    } else if s < 3 * moves {
        let k = 1 + (s - moves) / 2;
        if (s - moves).is_multiple_of(2) { (staged(k), app(k)) } else { (app(k), staged(k + 1)) }
    } else {
        match s - 3 * moves {
            0 => (staged(m), staged(0)),
            1 => (staged(0), SAVED_FIRST),
            _ => (app(0), staged(m)),
        }
    }
}

/// The image's length and the running one's, if the request is whole and fits.
fn request(f: &mut impl Flash) -> Option<(u32, u32)> {
    let mut rec = [0u32; 6];
    f.read(RECORD, &mut rec);
    let magic = |i: usize| u32::from_le_bytes(RECORD_MAGIC[4 * i..4 * i + 4].try_into().unwrap());
    let (total, running) = (rec[2], rec[4]);
    let max = APP_END - APP_START;
    (rec[0] == magic(0)
        && rec[1] == magic(1)
        && rec[3] == !total
        && rec[5] == !running
        && total % 4 == 0
        && (PAGE + TRAILER_LEN..=max).contains(&total)
        && running <= max)
        .then_some((total, running))
}

/// The staged image's trailer, and the app's hash in it.
fn image_ok(f: &mut impl Flash, total: u32) -> bool {
    let app_len = total - TRAILER_LEN;
    let mut t = [0u32; 16];
    f.read(STAGING + app_len, &mut t);
    let magic = |i: usize| u32::from_le_bytes(TRAILER_MAGIC[4 * i..4 * i + 4].try_into().unwrap());
    if t[0] != magic(0) || t[1] != magic(1) || t[2] != app_len || t[3] != APP_START || t[4] != CHIP {
        return false;
    }
    let mut hash = Sha256::new();
    let mut words = [0u32; 64];
    let mut block = [0u8; 256];
    for offset in (0..app_len).step_by(block.len()) {
        let n = (app_len - offset).min(block.len() as u32) as usize;
        f.read(STAGING + offset, &mut words[..n / 4]);
        for (b, w) in block.chunks_exact_mut(4).zip(&words[..n / 4]) {
            b.copy_from_slice(&w.to_le_bytes());
        }
        hash.update(&block[..n]);
        if offset % PAGE == 0 {
            f.feed();
        }
    }
    let hash = hash.finalize();
    (0..8).all(|i| hash[4 * i..4 * i + 4] == t[8 + i].to_le_bytes())
}

/// Nothing was swapped: put back the app's first page, if the firmware saved one.
fn restore(f: &mut impl Flash) -> Outcome {
    let mut head = [0u32; 2];
    f.read(SAVED_FIRST, &mut head);
    let [sp, reset] = head;
    let is_app = (0x2000_0000..=0x2004_0000).contains(&sp) && (APP_START..APP_END).contains(&(reset & !1));
    if is_app && copy_page(f, APP_START, SAVED_FIRST, &mut [0; WORDS]) { Outcome::Kept } else { Outcome::Broken }
}

/// Bring the page at `to` in line with the one at `from`; false if it does not read back.
fn copy_page(f: &mut impl Flash, to: u32, from: u32, buf: &mut [u32; WORDS]) -> bool {
    f.feed();
    f.read(from, buf);
    if holds(f, to, buf) {
        return true; // done before the power went, or the same in both
    }
    f.erase(to);
    f.write(to, buf);
    holds(f, to, buf)
}

fn holds(f: &mut impl Flash, at: u32, words: &[u32; WORDS]) -> bool {
    // a quarter of the page at a time: the stack stays small
    let mut part = [0u32; WORDS / 4];
    words.chunks_exact(WORDS / 4).enumerate().all(|(i, want)| {
        f.read(at + 4 * (i * WORDS / 4) as u32, &mut part);
        part[..] == want[..]
    })
}

#[cfg(test)]
mod tests;
