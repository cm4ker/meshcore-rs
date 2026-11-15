//! Plans of real-looking deltas, installed on a simulated flash with the power going at any erase
//! or write.

use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

const APP: u32 = 0x26000;
const APP_END: u32 = 0x99000;
const PLAN: u32 = 0x99000;
const SAVED_FIRST: u32 = 0xD8000;
const REQUEST: u32 = 0xD9000;
const LAYOUT: Layout = Layout { app: APP, app_end: APP_END, plan: PLAN, saved_first: SAVED_FIRST, request: REQUEST };
const INSTALLER_SP: u32 = 0x2004_0000;
const INSTALLER_RESET: u32 = 0xEA101;

/// The flash from the app to the request's page (the plan is not written while installing: the
/// tests keep it aside). The power goes at operation `cut`, leaving that one half done.
struct Sim {
    words: Vec<u32>,
    ops: u64,
    cut: Option<u64>,
    rng: u64,
}

struct PowerCut;

impl Sim {
    fn new(seed: u64) -> Self {
        Self { words: vec![0xFFFF_FFFF; ((REQUEST + PAGE - APP) / 4) as usize], ops: 0, cut: None, rng: seed | 1 }
    }

    fn random(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng as u32
    }

    fn at(&mut self, addr: u32) -> &mut u32 {
        assert!(addr.is_multiple_of(4) && (APP..REQUEST + PAGE).contains(&addr), "outside the flash: {addr:#x}");
        &mut self.words[((addr - APP) / 4) as usize]
    }

    fn power_cut(&mut self) -> bool {
        self.ops += 1;
        self.cut == Some(self.ops)
    }

    fn put(&mut self, addr: u32, bytes: &[u8]) {
        for (i, w) in bytes.chunks(4).enumerate() {
            let mut b = [0xFF; 4];
            b[..w.len()].copy_from_slice(w);
            *self.at(addr + 4 * i as u32) = u32::from_le_bytes(b);
        }
    }

    fn bytes(&self, addr: u32, len: u32) -> Vec<u8> {
        (0..len.div_ceil(4)).flat_map(|i| self.read(addr + 4 * i).to_le_bytes()).take(len as usize).collect()
    }
}

impl Flash for Sim {
    fn read(&self, addr: u32) -> u32 {
        assert!(addr.is_multiple_of(4) && (APP..REQUEST + PAGE).contains(&addr), "outside the flash: {addr:#x}");
        self.words[((addr - APP) / 4) as usize]
    }

    fn erase(&mut self, page: u32) {
        assert_eq!(page % PAGE, 0);
        let cut = self.power_cut();
        for i in 0..WORDS as u32 {
            if !cut || self.random().is_multiple_of(2) {
                *self.at(page + 4 * i) = 0xFFFF_FFFF;
            }
        }
        if cut {
            std::panic::panic_any(PowerCut);
        }
    }

    fn write(&mut self, addr: u32, word: u32) {
        if self.power_cut() {
            let partly = word | self.random();
            *self.at(addr) &= partly;
            std::panic::panic_any(PowerCut);
        }
        *self.at(addr) &= word;
    }
}

fn quiet_power_cuts() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !info.payload().is::<PowerCut>() {
                default(info)
            }
        }));
    });
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as u32
    }
}

/// An image of `len` bytes that looks like code: words, many of them addresses into itself.
fn image(len: usize, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        let w = if r.next() % 3 == 0 { 0x0002_6000 + r.next() % len as u32 } else { r.next() };
        v.extend_from_slice(&w.to_le_bytes());
    }
    v.truncate(len);
    v[..4].copy_from_slice(&0x2003_FFF8u32.to_le_bytes());
    v[4..8].copy_from_slice(&(APP + 0x401).to_le_bytes());
    v
}

/// The next release of `old`: code inserted and removed here and there (so the rest moves), the
/// addresses into it changed by as much, some words changed outright.
fn next_release(old: &[u8], seed: u64, grow: usize) -> Vec<u8> {
    let mut r = Rng(seed | 1);
    let mut new = old.to_vec();
    for _ in 0..6 {
        let at = (r.next() as usize % (new.len() - 64)) & !3;
        let n = 4 * (r.next() as usize % (grow / 24 + 1));
        let ins: Vec<u8> = (0..n).map(|_| r.next() as u8).collect();
        new.splice(at..at, ins);
    }
    for _ in 0..3 {
        let at = (r.next() as usize % (new.len() - 512)) & !3;
        new.drain(at..at + 4 * (r.next() as usize % 64));
    }
    for w in new.chunks_mut(4).skip(2) {
        let v = u32::from_le_bytes(w.try_into().unwrap_or([0; 4]));
        if w.len() == 4 && (0x26000..0x99000).contains(&v) {
            w.copy_from_slice(&(v + 0x100).to_le_bytes());
        }
    }
    for _ in 0..50 {
        let at = ((r.next() as usize % (new.len() - 8)) & !3) + 8;
        new[at] ^= 0x5A;
    }
    new
}

/// The plan of the bsdiff delta from `old` to `new`, as the device makes it from the patch.
fn plan_of(old: &[u8], new: &[u8], shift: u32) -> Vec<u8> {
    let mut patch = Vec::new();
    bsdiff::diff(old, new, &mut patch).unwrap();
    let mut planner = Planner::new(old.len() as u32, new.len() as u32, shift).unwrap();
    let mut records = Vec::new();
    let mut out = |r: &[u8]| {
        records.extend_from_slice(r);
        true
    };
    let (mut i, mut old_pos) = (0usize, 0i64);
    let field = |i: usize| u64::from_le_bytes(patch[i..i + 8].try_into().unwrap());
    while i < patch.len() {
        let (mix, copy, raw) = (field(i) as usize, field(i + 8) as usize, field(i + 16));
        let seek = if raw >> 63 == 0 { raw as i64 } else { -((raw & !(1 << 63)) as i64) };
        i += 24;
        let diff = &patch[i..i + mix];
        let made: Vec<u8> = diff.iter().enumerate().map(|(k, d)| old[old_pos as usize + k].wrapping_add(*d)).collect();
        assert!(planner.mix(old_pos as u32, diff, &made, &mut out));
        i += mix;
        old_pos += mix as i64;
        assert!(planner.literal(&patch[i..i + copy], &mut out));
        i += copy;
        old_pos += seek;
    }
    let first = planner.finish([7; 32]).expect("every page planned");
    [first, records].concat()
}

/// The flash as the firmware leaves it to the installer: the old image with its first page
/// saved and pointing at the installer, the request.
fn handed_over(old: &[u8], plan_len: u32, seed: u64) -> Sim {
    handed_over_at(old, 0, plan_len, seed)
}

/// [`handed_over`] with the plan at `at` in the staging area.
fn handed_over_at(old: &[u8], at: u32, plan_len: u32, seed: u64) -> Sim {
    let mut f = Sim::new(seed);
    f.put(APP, old);
    f.put(SAVED_FIRST, &old[..PAGE as usize]);
    let mut first = vec![0xFF; PAGE as usize];
    first[..4].copy_from_slice(&INSTALLER_SP.to_le_bytes());
    first[4..8].copy_from_slice(&INSTALLER_RESET.to_le_bytes());
    let erased: Vec<u8> = vec![0xFF; PAGE as usize];
    f.put(APP, &erased);
    f.put(APP, &first);
    for (i, w) in request_words(at, plan_len, old.len() as u32).into_iter().enumerate() {
        *f.at(REQUEST + 4 * i as u32) = w;
    }
    f
}

#[test]
fn a_plan_makes_the_new_image() {
    for seed in 1..6 {
        let old = image(200_000 + 1000 * seed as usize, seed);
        let new = next_release(&old, seed + 100, 20_000);
        let plan = plan_of(&old, &new, 32 * 1024);
        let h = Header::parse(&plan[..PAGE as usize]).unwrap();
        let mut made = vec![0u8; new.len()];
        for p in 0..h.pages {
            let (from, to) = h.record(&plan, p).unwrap();
            let len = h.page_len(p) as usize;
            let mut old_at = |at: u32, out: &mut [u8]| {
                out.copy_from_slice(&old[at as usize..at as usize + out.len()]);
                true
            };
            let page = &mut made[(p * PAGE) as usize..(p * PAGE) as usize + len];
            make_page(&h, p, &plan[from as usize..to as usize], &mut old_at, page).unwrap();
        }
        assert!(made == new, "seed {seed}");
        assert!(plan.len() < new.len() / 2, "plan {} for an image of {}", plan.len(), new.len());
    }
}

#[test]
fn installs_with_the_power_going_at_any_step() {
    quiet_power_cuts();
    let old = image(120_000, 7);
    let new = next_release(&old, 8, 30_000);
    let plan = plan_of(&old, &new, 32 * 1024);
    // the whole run once, to count its operations
    let mut f = handed_over(&old, plan.len() as u32, 1);
    assert_eq!(install(&mut f, &LAYOUT, &plan), Outcome::Installed);
    assert_eq!(f.bytes(APP, new.len() as u32), new);
    let total = f.ops;
    // the power goes at every 97th operation (every one takes minutes), and once more on the way
    for cut in (1..=total).step_by(97) {
        let mut f = handed_over(&old, plan.len() as u32, cut);
        f.cut = Some(cut);
        let first = catch_unwind(AssertUnwindSafe(|| install(&mut f, &LAYOUT, &plan)));
        assert!(first.is_err(), "the power went at {cut}");
        // until it is done, the bootloader starts the installer (or finds no app at all)
        let sp = f.read(APP);
        assert!(sp == INSTALLER_SP || sp == 0xFFFF_FFFF || f.bytes(APP, 4) == new[..4], "cut {cut}: {sp:#x}");
        f.cut = Some(f.ops + 1 + cut % 997);
        if catch_unwind(AssertUnwindSafe(|| install(&mut f, &LAYOUT, &plan))).is_err() {
            f.cut = None;
            assert_eq!(install(&mut f, &LAYOUT, &plan), Outcome::Installed, "cut {cut}, then again");
        }
        assert!(f.bytes(APP, new.len() as u32) == new, "cut {cut}");
    }
}

#[test]
fn the_first_page_is_made_of_literal_bytes() {
    let old = image(60_000, 3);
    let new = next_release(&old, 4, 8_000);
    let plan = plan_of(&old, &new, 16 * 1024);
    let h = Header::parse(&plan[..PAGE as usize]).unwrap();
    // it goes last, when nothing of the old image is where it was
    let (from, to) = h.record(&plan, 0).unwrap();
    let mut never = |_: u32, _: &mut [u8]| panic!("page 0 reads old data");
    let mut page = vec![0u8; PAGE as usize];
    make_page(&h, 0, &plan[from as usize..to as usize], &mut never, &mut page).unwrap();
    assert_eq!(page, new[..PAGE as usize]);
    let mut f = handed_over(&old, plan.len() as u32, 5);
    assert_eq!(install(&mut f, &LAYOUT, &plan), Outcome::Installed);
    assert_eq!(f.bytes(APP, new.len() as u32), new);
}

#[test]
fn a_record_may_not_reach_below_what_is_overwritten() {
    let old = image(40_000, 9);
    let new = next_release(&old, 10, 4_000);
    let plan = plan_of(&old, &new, 8 * 1024);
    let h = Header::parse(&plan[..PAGE as usize]).unwrap();
    // every copy the planner made is from at or above the page's lowest offset
    for p in 1..h.pages {
        let (from, to) = h.record(&plan, p).unwrap();
        let lowest = h.lowest(p);
        let mut check = |at: u32, out: &mut [u8]| {
            assert!(at >= lowest, "page {p} reads {at:#x} below {lowest:#x}");
            out.copy_from_slice(&old[at as usize..at as usize + out.len()]);
            true
        };
        let mut page = vec![0u8; PAGE as usize];
        make_page(&h, p, &plan[from as usize..to as usize], &mut check, &mut page).unwrap();
    }
    // and a record that does is refused
    let mut bad = vec![COPY];
    bad.extend_from_slice(&0u32.to_le_bytes());
    bad.extend_from_slice(&(PAGE as u16).to_le_bytes());
    bad.extend_from_slice(&[0x90, 0x20, 0]);
    let mut old_at = |_: u32, _: &mut [u8]| true;
    let mut page = vec![0u8; PAGE as usize];
    assert!(make_page(&h, 3, &bad, &mut old_at, &mut page).is_none());
}

#[test]
fn no_request_puts_the_first_page_back() {
    let old = image(30_000, 11);
    let mut f = handed_over(&old, 0, 1);
    for i in 0..8 {
        *f.at(REQUEST + 4 * i) = 0xFFFF_FFFF;
    }
    assert_eq!(install(&mut f, &LAYOUT, &[]), Outcome::Kept);
    assert_eq!(f.bytes(APP, old.len() as u32), old);
}

/// The plan back from the image `plan` makes to `old`.
fn reverse_of(plan: &[u8], old: &[u8]) -> Vec<u8> {
    let mut records = Vec::new();
    let first = reverse(plan, old, [9; 32], &mut |r: &[u8]| {
        records.extend_from_slice(r);
        true
    })
    .expect("a plan back");
    [first, records].concat()
}

#[test]
fn a_plan_back_makes_the_old_image_from_the_new_one() {
    for seed in 1..5 {
        let old = image(150_000 + 3000 * seed as usize, seed + 20);
        let new = next_release(&old, seed + 40, 25_000);
        let plan = plan_of(&old, &new, 32 * 1024);
        let back = reverse_of(&plan, &old);
        let h = Header::parse(&back[..PAGE as usize]).unwrap();
        assert_eq!((h.old_len, h.new_len), (new.len() as u32, old.len() as u32));
        // the new image as the plan makes it, read from wherever the plan back needs it
        let mut rebuilt = Rebuilt::new(&plan, |at: u32, out: &mut [u8]| {
            out.copy_from_slice(&old[at as usize..at as usize + out.len()]);
            true
        })
        .unwrap();
        let mut made = vec![0u8; old.len()];
        for p in 0..h.pages {
            let (from, to) = h.record(&back, p).unwrap();
            let len = h.page_len(p) as usize;
            let mut new_at = |at: u32, out: &mut [u8]| rebuilt.read(at, out);
            let page = &mut made[(p * PAGE) as usize..(p * PAGE) as usize + len];
            make_page(&h, p, &back[from as usize..to as usize], &mut new_at, page).unwrap();
        }
        assert!(made == old, "seed {seed}");
        assert!(back.len() < old.len() / 2, "plan back {} for an image of {}", back.len(), old.len());
    }
}

#[test]
fn goes_forward_and_back_with_the_power_going() {
    quiet_power_cuts();
    let old = image(100_000, 31);
    let new = next_release(&old, 32, 20_000);
    let plan = plan_of(&old, &new, 32 * 1024);
    let back = reverse_of(&plan, &old);
    // both plans in the staging area, the plan back from the page after the plan
    let at = (plan.len() as u32).next_multiple_of(PAGE);
    let mut staging = plan.clone();
    staging.resize(at as usize, 0xFF);
    staging.extend_from_slice(&back);
    for cut in [None, Some(500), Some(3001)] {
        let mut f = handed_over_at(&old, 0, plan.len() as u32, 7);
        f.cut = cut;
        if catch_unwind(AssertUnwindSafe(|| install(&mut f, &LAYOUT, &staging))).is_err() {
            f.cut = None;
            assert_eq!(install(&mut f, &LAYOUT, &staging), Outcome::Installed);
        }
        assert!(f.bytes(APP, new.len() as u32) == new, "forward, cut {cut:?}");
        // the firmware hands over again, with the plan back
        let mut g = handed_over_at(&new, at, back.len() as u32, 8);
        g.cut = cut.map(|c| c + 777);
        if catch_unwind(AssertUnwindSafe(|| install(&mut g, &LAYOUT, &staging))).is_err() {
            g.cut = None;
            assert_eq!(install(&mut g, &LAYOUT, &staging), Outcome::Installed);
        }
        assert!(g.bytes(APP, old.len() as u32) == old, "back, cut {cut:?}");
    }
}
