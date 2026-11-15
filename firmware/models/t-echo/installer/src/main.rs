//! The T-Echo firmware's installer. An update arrives in the QSPI flash, where the firmware has
//! verified it; to install it, the firmware saves the first page of the app flash there too,
//! points that page at this program and resets. This swaps the image with the app (`lib.rs`), the
//! first page last, and resets into it: the firmware that ran stays in the QSPI flash, to go back
//! to.
//!
//! A power loss along the way leaves the first page pointing here, so the swap goes on at the next
//! boot. With nothing valid to install, the firmware that ran gets its first page back and starts
//! again; with no firmware left to start, this restarts into the bootloader's USB drive mode: the
//! board then needs a firmware copied over USB.

#![no_std]
#![no_main]

use cortex_m_rt::{ExceptionFrame, entry, exception};
use nrf_pac as pac;
use t_echo_installer::{Flash, Outcome, QSPI};

/// `GPREGRET` value that keeps the Adafruit bootloader in its USB drive mode.
const DFU_MAGIC_UF2_RESET: u8 = 0x57;
/// The blue LED, lit while installing (active low).
const LED_PIN: usize = 14;

#[entry]
fn main() -> ! {
    pac::P0.outclr().write(|w| w.set_pin(LED_PIN, true));
    pac::P0.pin_cnf(LED_PIN).write(|w| w.set_dir(pac::gpio::vals::Dir::Output));
    qspi_init();
    let outcome = t_echo_installer::install(&mut Flashes);
    pac::P0.outset().write(|w| w.set_pin(LED_PIN, true));
    reset(if outcome == Outcome::Broken { DFU_MAGIC_UF2_RESET } else { 0 })
}

/// The app flash through the NVMC, the QSPI flash from [`QSPI`] up.
struct Flashes;

impl Flash for Flashes {
    fn read(&mut self, addr: u32, out: &mut [u32]) {
        if addr >= QSPI {
            // the QSPI's DMA takes a word-aligned buffer in RAM, as `out` is
            qspi_task(addr - QSPI, out.as_ptr() as u32, out.len() as u32 * 4, Task::Read);
        } else {
            for (i, w) in out.iter_mut().enumerate() {
                // SAFETY: a word-aligned address in the app flash, which is memory-mapped
                *w = unsafe { core::ptr::read_volatile((addr as *const u32).add(i)) };
            }
        }
    }

    fn erase(&mut self, page: u32) {
        if page >= QSPI {
            qspi_task(page - QSPI, 0, 0, Task::Erase);
        } else {
            pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Een));
            pac::NVMC.erasepage().write_value(page);
            while !pac::NVMC.ready().read().ready() {}
            pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Ren));
        }
    }

    fn write(&mut self, addr: u32, words: &[u32]) {
        if addr >= QSPI {
            // through RAM (the DMA cannot read the app flash), the first word last
            let mut buf = [0u32; 64];
            let mut put = |at: usize, part: &[u32]| {
                for (k, chunk) in part.chunks(buf.len()).enumerate() {
                    buf[..chunk.len()].copy_from_slice(chunk);
                    let to = addr - QSPI + 4 * (at + k * buf.len()) as u32;
                    qspi_task(to, buf.as_ptr() as u32, chunk.len() as u32 * 4, Task::Write);
                }
            };
            put(1, &words[1.min(words.len())..]);
            put(0, &words[..1.min(words.len())]);
            return;
        }
        pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Wen));
        for (i, &word) in words.iter().enumerate().rev().filter(|&(_, &w)| w != 0xFFFF_FFFF) {
            // SAFETY: a word-aligned address in the app flash, erased just before
            unsafe { core::ptr::write_volatile((addr as *mut u32).add(i), word) };
            while !pac::NVMC.ready().read().ready() {}
        }
        pac::NVMC.config().write(|w| w.set_wen(pac::nvmc::vals::Wen::Ren));
    }

    /// The firmware's watchdog keeps running through the reset that started us.
    fn feed(&mut self) {
        let wdt = pac::WDT;
        if wdt.runstatus().read().runstatus() {
            let enabled = wdt.rren().read().0;
            for i in 0..8 {
                if enabled & (1 << i) != 0 {
                    wdt.rr(i).write(|w| w.set_rr(pac::wdt::vals::Rr::Reload));
                }
            }
        }
    }
}

/// The QSPI flash as the firmware sets it up (P0.12 powers it; SCK P1.14, CSN P1.15, IO0-3 P1.12
/// P1.13 P0.07 P0.05; FAST_READ at 8 MHz).
fn qspi_init() {
    pac::P0.outset().write(|w| w.set_pin(12, true));
    pac::P0.pin_cnf(12).write(|w| w.set_dir(pac::gpio::vals::Dir::Output));
    cortex_m::asm::delay(64_000 * 10); // 10 ms for the flash to power up
    let q = pac::QSPI;
    for (port, pin) in [(1, 14), (1, 15), (1, 12), (1, 13), (0, 7), (0, 5)] {
        let gpio = if port == 0 { pac::P0 } else { pac::P1 };
        gpio.outset().write(|w| w.set_pin(pin, true));
        gpio.pin_cnf(pin).write(|w| {
            w.set_dir(pac::gpio::vals::Dir::Output);
            w.set_drive(pac::gpio::vals::Drive::H0h1);
        });
    }
    let psel = |port: u8, pin: u8| {
        let mut p = pac::shared::regs::Psel(0);
        p.set_pin(pin);
        p.set_port(port == 1);
        p.set_connect(pac::shared::vals::Connect::Connected);
        p
    };
    q.psel().sck().write_value(psel(1, 14));
    q.psel().csn().write_value(psel(1, 15));
    q.psel().io0().write_value(psel(1, 12));
    q.psel().io1().write_value(psel(1, 13));
    q.psel().io2().write_value(psel(0, 7));
    q.psel().io3().write_value(psel(0, 5));
    q.ifconfig0().write(|w| {
        w.set_addrmode(pac::qspi::vals::Addrmode::_24bit);
        w.set_dpmenable(false);
        w.set_ppsize(pac::qspi::vals::Ppsize::_256bytes);
        w.set_readoc(pac::qspi::vals::Readoc::Fastread);
        w.set_writeoc(pac::qspi::vals::Writeoc::Pp);
    });
    q.ifconfig1().write(|w| {
        w.set_sckdelay(80);
        w.set_dpmen(false);
        w.set_spimode(pac::qspi::vals::Spimode::Mode0);
        w.set_sckfreq(3); // 32 MHz / (3 + 1)
    });
    q.iftiming().write(|w| w.set_rxdelay(2));
    q.xipoffset().write_value(0);
    q.enable().write(|w| w.set_enable(true));
    q.events_ready().write_value(0);
    q.tasks_activate().write_value(1);
    while q.events_ready().read() == 0 {}
}

enum Task {
    Read,
    Write,
    Erase,
}

/// One QSPI job, waited out: the peripheral raises READY once the flash has finished an erase or
/// a write too (it polls the flash's status). `ram`, `len`: the buffer for a read or a write.
fn qspi_task(at: u32, ram: u32, len: u32, task: Task) {
    let q = pac::QSPI;
    match task {
        Task::Read => {
            q.read().src().write_value(at);
            q.read().dst().write_value(ram);
            q.read().cnt().write(|w| w.set_cnt(len));
        }
        Task::Write => {
            q.write().src().write_value(ram);
            q.write().dst().write_value(at);
            q.write().cnt().write(|w| w.set_cnt(len));
        }
        Task::Erase => {
            q.erase().ptr().write_value(at);
            q.erase().len().write(|w| w.set_len(pac::qspi::vals::Len::_4kb));
        }
    }
    q.events_ready().write_value(0);
    match task {
        Task::Read => q.tasks_readstart().write_value(1),
        Task::Write => q.tasks_writestart().write_value(1),
        Task::Erase => q.tasks_erasestart().write_value(1),
    }
    while q.events_ready().read() == 0 {}
}

fn reset(gpregret: u8) -> ! {
    pac::POWER.gpregret().write(|w| w.set_gpregret(gpregret));
    cortex_m::peripheral::SCB::sys_reset()
}

#[exception]
unsafe fn HardFault(_frame: &ExceptionFrame) -> ! {
    reset(DFU_MAGIC_UF2_RESET)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    reset(DFU_MAGIC_UF2_RESET)
}
