//! The swap on a simulated flash, with the power going at any erase or write.

use super::*;
use std::collections::{BTreeSet, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};

const INSTALLER_SP: u32 = 0x2004_0000;
const INSTALLER_RESET: u32 = 0xEA101;
const FLASH_START: u32 = APP_START;
const FLASH_END: u32 = RECORD + PAGE;

/// The flash from the app to the request's page. Each erase or write is one operation; the power
/// goes at operation `cut`, leaving that one half done.
struct Sim {
    words: Vec<u32>,
    ops: u64,
    cut: Option<u64>,
    /// Operations that erase a page or log a step.
    marks: Vec<u64>,
    rng: u64,
}

struct PowerCut;

impl Sim {
    fn new(seed: u64) -> Self {
        let words = vec![0xFFFF_FFFF; ((FLASH_END - FLASH_START) / 4) as usize];
        Self { words, ops: 0, cut: None, marks: Vec::new(), rng: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1 }
    }

    fn random(&mut self) -> u32 {
        // xorshift64
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng as u32
    }

    fn at(&mut self, addr: u32) -> &mut u32 {
        assert!(addr.is_multiple_of(4) && (FLASH_START..FLASH_END).contains(&addr), "outside the flash: {addr:#x}");
        &mut self.words[((addr - FLASH_START) / 4) as usize]
    }

    /// The power goes now, if this is the operation it goes at.
    fn power_cut(&mut self, mark: bool) -> bool {
        self.ops += 1;
        if mark {
            self.marks.push(self.ops);
        }
        self.cut == Some(self.ops)
    }

    fn put(&mut self, addr: u32, data: &[u32]) {
        for (i, &w) in data.iter().enumerate() {
            *self.at(addr + 4 * i as u32) = w;
        }
    }

    fn get(&self, addr: u32, len: usize) -> Vec<u32> {
        (0..len).map(|i| self.read(addr + 4 * i as u32)).collect()
    }

    /// What the bootloader finds in the app's first page.
    fn boots(&self) -> Boot {
        match self.read(APP_START) {
            INSTALLER_SP => Boot::Installer,
            0xFFFF_FFFF => Boot::Drive,
            _ => Boot::App,
        }
    }
}

impl Flash for Sim {
    fn read(&self, addr: u32) -> u32 {
        assert!(addr.is_multiple_of(4) && (FLASH_START..FLASH_END).contains(&addr), "outside the flash: {addr:#x}");
        self.words[((addr - FLASH_START) / 4) as usize]
    }

    fn erase(&mut self, page: u32) {
        assert_eq!(page % PAGE, 0);
        let cut = self.power_cut(true);
        for i in 0..WORDS as u32 {
            // cut short: some words erased, some not
            if !cut || self.random().is_multiple_of(2) {
                *self.at(page + 4 * i) = 0xFFFF_FFFF;
            }
        }
        if cut {
            std::panic::panic_any(PowerCut);
        }
    }

    fn write(&mut self, addr: u32, word: u32) {
        if self.power_cut((LOG..RECORD + PAGE).contains(&addr)) {
            // cut short: some bits cleared, some not
            let partly = word | self.random();
            *self.at(addr) &= partly;
            std::panic::panic_any(PowerCut);
        }
        *self.at(addr) &= word;
    }
}

#[derive(Debug, PartialEq)]
enum Boot {
    Installer,
    App,
    Drive,
}

/// The panics that stand for power cuts say nothing.
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

/// An app of `len` bytes, its first words a vector table, then its trailer.
fn image(len: u32, seed: u64) -> Vec<u32> {
    let mut sim = Sim::new(seed);
    let mut app: Vec<u32> = (0..len / 4).map(|_| sim.random()).collect();
    app[0] = 0x2003_FFF8;
    app[1] = APP_START + 0x401;
    let bytes: Vec<u8> = app.iter().flat_map(|w| w.to_le_bytes()).collect();
    let hash = Sha256::digest(&bytes);
    let word = |b: &[u8]| u32::from_le_bytes(b.try_into().unwrap());
    let mut t = vec![0u32; (TRAILER_LEN / 4) as usize];
    (t[0], t[1], t[2], t[3], t[4]) = (word(&TRAILER_MAGIC[..4]), word(&TRAILER_MAGIC[4..]), len, APP_START, CHIP);
    for i in 0..8 {
        t[8 + i] = word(&hash[4 * i..4 * i + 4]);
    }
    app.extend(t);
    app
}

/// What the firmware does: the request, then `hand_over`.
fn hand_over(sim: &mut Sim, total: u32, running: u32) {
    let magic = |i: usize| u32::from_le_bytes(RECORD_MAGIC[4 * i..4 * i + 4].try_into().unwrap());
    sim.erase(RECORD);
    sim.put(RECORD, &[magic(0), magic(1), total, !total, running, !running]);
    let first = sim.get(APP_START, WORDS);
    sim.erase(SAVED_FIRST);
    sim.put(SAVED_FIRST, &first);
    sim.erase(APP_START);
    sim.put(APP_START, &[INSTALLER_SP, INSTALLER_RESET]);
}

/// A node running `old` with `new` staged and verified, handed over to the installer.
fn node(old: &[u32], new: &[u32], seed: u64) -> Sim {
    let mut sim = Sim::new(seed);
    // whatever was there before
    for addr in (APP_START..SAVED_FIRST).step_by(4) {
        let r = sim.random();
        *sim.at(addr) = r;
    }
    sim.put(APP_START, old);
    sim.put(STAGING, new);
    hand_over(&mut sim, 4 * new.len() as u32, 4 * old.len() as u32);
    sim.ops = 0;
    sim.marks.clear();
    sim
}

/// Boots until the installer has finished, the power going at operation `cut` once.
fn run(sim: &mut Sim, cut: Option<u64>) -> Outcome {
    quiet_power_cuts();
    sim.cut = cut;
    loop {
        assert_eq!(sim.boots(), Boot::Installer);
        match catch_unwind(AssertUnwindSafe(|| install(sim))) {
            Ok(outcome) => return outcome,
            Err(e) if e.is::<PowerCut>() => {
                sim.cut = None;
                if sim.boots() == Boot::Installer {
                    continue;
                }
                // the exposed moment: the last step, writing the app's first page
                let (total, running) = request(sim).unwrap();
                let m = total.div_ceil(PAGE).max(running.div_ceil(PAGE));
                assert_eq!(sim.read(LOG + 4 * (3 * m - 2)), 0, "no installer to start before the last step");
                let complete = sim.get(APP_START, WORDS) == sim.get(STAGING + m * PAGE, WORDS);
                return if complete { Outcome::Installed } else { Outcome::Broken };
            }
            Err(e) => std::panic::resume_unwind(e),
        }
    }
}

fn assert_swapped(sim: &Sim, old: &[u32], new: &[u32]) {
    assert_eq!(sim.get(APP_START, new.len()), new, "the new firmware in the app flash");
    assert_eq!(sim.get(STAGING, old.len()), old, "the old firmware in the staging area");
    assert_eq!(sim.boots(), Boot::App);
}

#[test]
fn plan_swaps_every_page() {
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Is {
        Old(u32),
        New(u32),
        Installer,
    }
    for m in 2..=(APP_END - APP_START) / PAGE {
        let mut flash: HashMap<u32, Is> = HashMap::new();
        for k in 0..m {
            flash.insert(APP_START + k * PAGE, Is::Old(k));
            flash.insert(STAGING + k * PAGE, Is::New(k));
        }
        flash.insert(STAGING + m * PAGE, Is::New(m)); // past the image
        flash.insert(APP_START, Is::Installer);
        flash.insert(SAVED_FIRST, Is::Old(0));
        for s in 0..3 * m {
            let (to, from) = step(s, m);
            assert!(to < SAVED_FIRST, "the saved page and the request stay");
            assert!(to != APP_START || s == 3 * m - 1, "the app's first page last");
            flash.insert(to, flash[&from]);
        }
        for k in 0..m {
            assert_eq!(flash[&(APP_START + k * PAGE)], Is::New(k), "m {m}");
            assert_eq!(flash[&(STAGING + k * PAGE)], Is::Old(k), "m {m}");
        }
    }
}

#[test]
fn swaps_full_size_images() {
    let old = image(80 * PAGE - 300, 1);
    let new = image(84 * PAGE + 100, 2);
    let mut sim = node(&old, &new, 3);
    assert_eq!(run(&mut sim, None), Outcome::Installed);
    assert_swapped(&sim, &old, &new);
}

#[test]
fn swaps_a_shorter_or_longer_image() {
    for (o, n) in [(5u32, 3u32), (3, 5), (4, 4)] {
        let old = image(o * PAGE - 64 - 4 * o, 10 + o as u64);
        let new = image(n * PAGE + 8, 20 + n as u64);
        let mut sim = node(&old, &new, 30);
        assert_eq!(run(&mut sim, None), Outcome::Installed);
        assert_swapped(&sim, &old, &new);
    }
}

#[test]
fn rollback_is_the_same_swap() {
    let old = image(6 * PAGE - 64, 1);
    let new = image(7 * PAGE + 64, 2);
    let mut sim = node(&old, &new, 3);
    assert_eq!(run(&mut sim, None), Outcome::Installed);
    hand_over(&mut sim, 4 * old.len() as u32, 4 * new.len() as u32);
    assert_eq!(run(&mut sim, None), Outcome::Installed);
    assert_swapped(&sim, &new, &old);
}

#[test]
fn survives_a_power_cut_anywhere() {
    let old = image(4 * PAGE - 64 - 8, 1);
    let new = image(5 * PAGE - 64 - 12, 2);
    let (total, marks) = {
        let mut sim = node(&old, &new, 3);
        run(&mut sim, None);
        (sim.ops, sim.marks)
    };
    // every erase and every logged step, the operations next to them, and a stride through the
    // writes in between
    let mut cuts: BTreeSet<u64> = (1..=total).step_by(11).collect();
    cuts.extend(marks.iter().flat_map(|&m| [m - 1, m, m + 1]));
    let mut exposed = 0;
    for &cut in cuts.iter().filter(|&&c| (1..=total).contains(&c)) {
        let mut sim = node(&old, &new, 3 + cut);
        match run(&mut sim, Some(cut)) {
            Outcome::Installed => assert_swapped(&sim, &old, &new),
            Outcome::Broken => exposed += 1,
            Outcome::Kept => panic!("kept the old firmware after a cut at {cut}"),
        }
    }
    // only cuts while the app's first page is written
    assert!(exposed <= 4 + WORDS / 11, "{exposed} cuts left no app");
}

#[test]
fn a_second_cut_while_resuming() {
    quiet_power_cuts();
    let old = image(3 * PAGE + 4, 1);
    let new = image(4 * PAGE - 64, 2);
    for first in (1..9000).step_by(997) {
        let mut sim = node(&old, &new, first);
        sim.cut = Some(first);
        let r = catch_unwind(AssertUnwindSafe(|| install(&mut sim)));
        assert!(r.is_err(), "the cut at {first} happened");
        sim.ops = 0;
        assert_eq!(run(&mut sim, Some(first / 2 + 1)), Outcome::Installed);
        assert_swapped(&sim, &old, &new);
    }
}

#[test]
fn a_bad_image_brings_the_old_firmware_back() {
    let old = image(5 * PAGE - 64, 1);
    let mut new = image(5 * PAGE - 64, 2);
    new[100] ^= 1;
    let mut sim = node(&old, &new, 3);
    assert_eq!(run(&mut sim, None), Outcome::Kept);
    assert_eq!(sim.get(APP_START, old.len()), old);
    assert_eq!(sim.get(STAGING, new.len()), new, "the staging area untouched");
}

#[test]
fn no_request_brings_the_old_firmware_back() {
    let old = image(5 * PAGE - 64, 1);
    let new = image(5 * PAGE - 64, 2);
    let mut sim = node(&old, &new, 3);
    sim.erase(RECORD);
    assert_eq!(run(&mut sim, None), Outcome::Kept);
    assert_eq!(sim.get(APP_START, old.len()), old);
}
