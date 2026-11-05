//! The repeater's console on the nRF52840's own USB port (CDC ACM serial): CLI lines in, replies
//! and log lines out, with CR LF line ends as the Arduino firmware's (MeshCore's web consoles show
//! a line only once its CR LF has come).
//!
//! Like Adafruit's firmware, opening the port at 1200 baud and closing it restarts into the UF2
//! bootloader (its drive appears for the next firmware), so updates need no button presses.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either, select};
use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{ControlChanged, Receiver, Sender};

pub type UsbDriver = Driver<'static, HardwareVbusDetect>;

/// Text for the USB host. Held until a host opens the port (the boot log waits for it), dropped
/// when full.
static OUT: Channel<CriticalSectionRawMutex, Vec<u8>, 32> = Channel::new();
/// Complete CLI lines for the main loop.
pub static LINES: Channel<CriticalSectionRawMutex, Vec<u8>, 4> = Channel::new();
/// A host has the port open (DTR, or it sent something).
static HOST: AtomicBool = AtomicBool::new(false);
static HOST_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

const PACKET: usize = 64;
const LINE_MAX: usize = 159;
/// `GPREGRET` value that makes the Adafruit bootloader stay in UF2 drive mode.
const DFU_MAGIC_UF2_RESET: u8 = 0x57;

/// Queue text for the console.
pub fn write(text: &str) {
    if !text.is_empty() {
        let _ = OUT.try_send(Vec::from(text.as_bytes()));
    }
}

/// `println!` for the console: the line ends with CR LF.
#[macro_export]
macro_rules! println {
    () => {
        $crate::console::write("\r\n")
    };
    ($($arg:tt)*) => {
        $crate::console::write(&alloc::format!("{}\r\n", format_args!($($arg)*)))
    };
}

fn set_host(on: bool) {
    if HOST.swap(on, Ordering::Relaxed) != on {
        HOST_CHANGED.signal(());
    }
}

/// Restart into the UF2 bootloader.
pub fn enter_bootloader() -> ! {
    // the POWER registers are the SoftDevice's while it runs
    crate::sd::disable();
    embassy_nrf::pac::POWER.gpregret().write(|w| w.set_gpregret(DFU_MAGIC_UF2_RESET));
    cortex_m::peripheral::SCB::sys_reset()
}

#[embassy_executor::task]
pub async fn device_task(mut device: UsbDevice<'static, UsbDriver>) {
    device.run().await
}

/// Read the port, echo, and hand complete lines to the main loop.
#[embassy_executor::task]
pub async fn rx_task(mut rx: Receiver<'static, UsbDriver>, control: ControlChanged<'static>) {
    let mut line: Vec<u8> = Vec::with_capacity(LINE_MAX + 1);
    let mut buf = [0u8; PACKET];
    loop {
        match select(rx.read_packet(&mut buf), control.control_changed()).await {
            Either::First(Ok(n)) => {
                set_host(true);
                let mut echo = Vec::with_capacity(n + 2);
                for &c in &buf[..n] {
                    match c {
                        b'\r' | b'\n' => {
                            if !line.is_empty() {
                                echo.extend_from_slice(b"\r\n");
                                let _ = OUT.try_send(core::mem::take(&mut echo));
                                LINES.send(core::mem::take(&mut line)).await;
                            }
                        }
                        0x08 | 0x7F => {
                            if line.pop().is_some() {
                                echo.extend_from_slice(b"\x08 \x08");
                            }
                        }
                        _ if line.len() < LINE_MAX => {
                            line.push(c);
                            echo.push(c);
                        }
                        _ => {}
                    }
                }
                if !echo.is_empty() {
                    let _ = OUT.try_send(echo);
                }
            }
            Either::First(Err(_)) => {
                set_host(false);
                rx.wait_connection().await;
            }
            // the "1200 baud touch": port opened at 1200 baud, then closed
            Either::Second(()) => {
                set_host(control.dtr());
                if !control.dtr() && rx.line_coding().data_rate() == 1200 {
                    enter_bootloader();
                }
            }
        }
    }
}

#[embassy_executor::task]
pub async fn tx_task(mut tx: Sender<'static, UsbDriver>) {
    loop {
        let data = OUT.receive().await;
        if !HOST.load(Ordering::Relaxed) {
            while !HOST.load(Ordering::Relaxed) {
                HOST_CHANGED.wait().await;
            }
            // a host that has just opened the port may clear what came in first (pyserial on
            // Windows raises DTR, then purges): the boot log would be lost
            Timer::after_millis(200).await;
        }
        for chunk in data.chunks(PACKET) {
            if tx.write_packet(chunk).await.is_err() {
                break; // the host went away: the text is dropped
            }
        }
        if data.len() % PACKET == 0 {
            let _ = tx.write_packet(&[]).await; // a full last packet needs a zero-length one
        }
    }
}

struct ConsoleLogger;

impl log::Log for ConsoleLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        let mut line = String::new();
        let _ = write!(line, "{} - {}\r\n", record.level(), record.args());
        write(&line);
    }

    fn flush(&self) {}
}

static LOGGER: ConsoleLogger = ConsoleLogger;

pub fn init_logger() {
    // SAFETY: called once, before any other thread of execution exists
    unsafe { log::set_logger_racy(&LOGGER).ok() };
    log::set_max_level(log::LevelFilter::Info);
}

// ---------------------------------------------------------------- crash record

const CRASH_MAGIC: u32 = 0xDEAD_C0DE;

#[repr(C)]
struct Crash {
    magic: u32,
    len: u32,
    text: [u8; 120],
}

#[unsafe(link_section = ".uninit.crash")]
static mut CRASH: MaybeUninit<Crash> = MaybeUninit::uninit();

/// Remember why the firmware is about to reset (RAM a reset does not clear), for the next start.
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
