//! Diagnostics over the USB port until the companion takes it over: a `log` logger, and the
//! reason of the last crash (a panic message or a hard fault's address), kept in RAM that a reset
//! does not clear and reported at the next start.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::mem::MaybeUninit;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;

/// Log lines for the USB host.
pub static USB_TEXT: Channel<CriticalSectionRawMutex, Vec<u8>, 32> = Channel::new();

struct UsbLogger;

impl log::Log for UsbLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let mut line = String::new();
        let _ = write!(line, "{} - {}\r\n", record.level(), record.args());
        let _ = USB_TEXT.try_send(line.into_bytes());
    }

    fn flush(&self) {}
}

static LOGGER: UsbLogger = UsbLogger;

pub fn init_logger() {
    // SAFETY: called once, before any other thread of execution exists
    unsafe { log::set_logger_racy(&LOGGER).ok() };
    log::set_max_level(log::LevelFilter::Info);
}

const CRASH_MAGIC: u32 = 0xDEAD_C0DE;

#[repr(C)]
struct Crash {
    magic: u32,
    len: u32,
    text: [u8; 120],
}

#[unsafe(link_section = ".uninit.crash")]
static mut CRASH: MaybeUninit<Crash> = MaybeUninit::uninit();

/// Remember why the firmware is about to reset.
pub fn record_crash(text: &str) {
    let n = text.len().min(120);
    // SAFETY: only touched here and in `take_crash`, never concurrently (crash path or boot)
    unsafe {
        let c = &mut *(&raw mut CRASH).cast::<Crash>();
        c.text[..n].copy_from_slice(&text.as_bytes()[..n]);
        c.len = n as u32;
        c.magic = CRASH_MAGIC;
    }
}

/// The reason of the last crash, once.
pub fn take_crash() -> Option<String> {
    // SAFETY: see `record_crash`; any bit pattern is valid for these plain integers
    unsafe {
        let c = &mut *(&raw mut CRASH).cast::<Crash>();
        if c.magic != CRASH_MAGIC {
            return None;
        }
        c.magic = 0;
        let n = (c.len as usize).min(120);
        Some(String::from_utf8_lossy(&c.text[..n]).into_owned())
    }
}

/// A short form of a panic, for [`record_crash`] (no allocation: the heap may be the problem).
pub fn panic_text(info: &core::panic::PanicInfo) -> heapless::String<120> {
    let mut s = heapless::String::new();
    let _ = write!(s, "panic: {}", info.message());
    if let Some(l) = info.location() {
        let _ = write!(s, " at {}:{}", l.file().rsplit(['/', '\\']).next().unwrap_or(""), l.line());
    }
    s
}
