//! The chip's USB Serial/JTAG port: what the host writes (console lines, companion frames), and
//! whether a host reads what we write.
//!
//! The port's OUT FIFO holds one 64-byte USB packet, and on the S3 a packet the host sends before
//! the FIFO has been read is lost. Polled every 20 ms, a console line that came in one write lost
//! everything after its first 64 bytes (a pasted `set ota.root <fingerprint>`), and the rest of it
//! was glued to the next line. The packet interrupt now drains the FIFO into a ring at once, and
//! tasks read the ring. (Interrupts masked for long lose packets all the same: the console shows
//! a pasted line only once it has come, see the repeater's `console_task`.)

use core::cell::RefCell;

use critical_section::{CriticalSection, Mutex};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration as EmbassyDuration, with_timeout};
use esp_hal::Blocking;
use esp_hal::handler;
use esp_hal::peripherals::USB_DEVICE;
use esp_hal::time::{Duration, Instant};
use esp_hal::usb::usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx};
use heapless::Deque;

/// A host with the port open asks the IN endpoint for data at least once per 1 ms USB frame.
const HOST_POLL: Duration = Duration::from_millis(2);

/// Whether a host reads the port now: it keeps sending IN tokens while it has the port open, and
/// none once it closed it (or when there is no host). What is written without a reader waits in
/// the 64-byte IN FIFO for whoever opens the port next, cut off where the FIFO filled up.
pub fn host_reading() -> bool {
    let regs = USB_DEVICE::regs();
    regs.int_clr().write(|w| w.in_token_rec_in_ep1().clear_bit_by_one());
    let since = Instant::now();
    while since.elapsed() < HOST_POLL {
        if regs.int_raw().read().in_token_rec_in_ep1().bit_is_set() {
            return true;
        }
    }
    false
}

/// Input not read yet: a few frames of the largest size.
const RING: usize = 1024;

static PORT: Mutex<RefCell<Option<UsbSerialJtagRx<'static, Blocking>>>> = Mutex::new(RefCell::new(None));
static INPUT: Mutex<RefCell<Deque<u8, RING>>> = Mutex::new(RefCell::new(Deque::new()));
static ARRIVED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The port's input, once [`start`] has taken it over (its output stays with esp-println).
pub struct Input(());

pub fn start(usb: USB_DEVICE<'static>) -> Input {
    let mut usb = UsbSerialJtag::new(usb);
    usb.set_interrupt_handler(on_packet);
    let (mut rx, _tx) = usb.split();
    // the port goes in first: an interrupt taken without it would leave its flag set, and with
    // the flag set no further packet raises one
    rx.reset_rx_packet_recv_interrupt();
    critical_section::with(|cs| {
        PORT.borrow_ref_mut(cs).replace(rx);
        if let Some(rx) = PORT.borrow_ref_mut(cs).as_mut() {
            rx.listen_rx_packet_recv_interrupt();
        }
        pump(cs);
    });
    Input(())
}

/// Move what the FIFO holds into the ring; what does not fit stays in the FIFO (the host's next
/// packets then wait) until a read makes room.
fn pump(cs: CriticalSection<'_>) -> bool {
    let mut port = PORT.borrow_ref_mut(cs);
    let Some(rx) = port.as_mut() else { return false };
    // cleared first: a packet that lands while the FIFO is drained raises it again
    rx.reset_rx_packet_recv_interrupt();
    let mut input = INPUT.borrow_ref_mut(cs);
    let mut moved = false;
    while !input.is_full() {
        let Ok(c) = rx.read_byte() else { break };
        let _ = input.push_back(c);
        moved = true;
    }
    moved
}

#[handler]
fn on_packet() {
    if critical_section::with(pump) {
        ARRIVED.signal(());
    }
}

impl Input {
    /// Wait for input, then copy what has come into `buf`. The FIFO is also looked at every
    /// 20 ms, as before the interrupt, so input still comes should the interrupt ever stop.
    pub async fn read(&mut self, buf: &mut [u8]) -> usize {
        loop {
            let n = critical_section::with(|cs| {
                pump(cs);
                let mut input = INPUT.borrow_ref_mut(cs);
                let mut n = 0;
                while n < buf.len() {
                    let Some(c) = input.pop_front() else { break };
                    buf[n] = c;
                    n += 1;
                }
                n
            });
            if n > 0 {
                // room again for what a full ring left in the FIFO
                critical_section::with(pump);
                return n;
            }
            let _ = with_timeout(EmbassyDuration::from_millis(20), ARRIVED.wait()).await;
        }
    }
}
