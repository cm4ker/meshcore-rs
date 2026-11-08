//! The repeater's serial console: echo what is typed and hand complete lines to the loop. Where
//! the bytes come from is the board's ([`crate::Variant::start_console`]).

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_time::{Instant, Timer};
use esp_hal::Blocking;
use esp_hal::peripherals::{GPIO44, UART0};
use esp_hal::rtc_cntl::{WakeupSource, wakeup_cause};
use esp_hal::uart::{Config, UartRx, WakeupConfig};
use esp_println::print;
use log::warn;
use meshcore_fw::repeater_app::{CLI_LINES, CONSOLE_WOKE_MS};

/// Input closer than this to the wake, or to the input before it, is the rest of the cut line.
const CUT_LINE_GAP_MS: u32 = 500;

/// Where the console's bytes come from.
#[allow(async_fn_in_trait)]
pub trait ConsoleRx {
    /// Wait for input and take what has come, at least one byte.
    async fn read(&mut self, buf: &mut [u8]) -> usize;
}

/// The chip's USB Serial/JTAG port, read in its interrupt (`meshcore_fw::usb`).
impl ConsoleRx for meshcore_fw::usb::Input {
    async fn read(&mut self, buf: &mut [u8]) -> usize {
        meshcore_fw::usb::Input::read(self, buf).await
    }
}

/// UART0 (RX 44) behind a CP2102 bridge, the console of the boards whose USB port is one (the V3,
/// the Wireless Paper): polled, its FIFO holds what came while the task slept.
struct Uart0(UartRx<'static, Blocking>);

impl ConsoleRx for Uart0 {
    async fn read(&mut self, buf: &mut [u8]) -> usize {
        loop {
            let n = self.0.read_buffered(buf).unwrap_or(0);
            if n > 0 {
                return n;
            }
            Timer::after_millis(20).await;
        }
    }
}

#[embassy_executor::task]
async fn uart0_console(rx: Uart0) {
    run(rx).await
}

/// The console on UART0; input on it wakes the chip from light sleep ([`uart0_woke`]).
pub fn start_uart0(spawner: Spawner, uart0: UART0<'static>, rx: GPIO44<'static>) {
    let mut rx = UartRx::new(uart0, Config::default()).unwrap().with_rx(rx);
    if let Err(e) = rx.enable_wakeup(&WakeupConfig::default()) {
        warn!("console: no wake from sleep: {e}");
    }
    spawner.spawn(uart0_console(Uart0(rx)).unwrap());
}

/// Input on UART0 woke the chip.
pub fn uart0_woke() -> bool {
    wakeup_cause().contains(WakeupSource::Uart0)
}

/// Read the console, echo, and hand complete lines to the main loop.
pub async fn run(mut rx: impl ConsoleRx) -> ! {
    let mut line: Vec<u8> = Vec::with_capacity(160);
    let mut buf = [0u8; 64];
    let mut shown = 0;
    let show = |part: &[u8]| part.iter().for_each(|&c| print!("{}", c as char));
    let mut cut_line: Option<u32> = None;
    loop {
        let n = rx.read(&mut buf).await;
        let typed = n <= 2;
        let woke = CONSOLE_WOKE_MS.swap(0, Ordering::Relaxed);
        if woke != 0 {
            (cut_line, shown) = (Some(woke), 0);
            line.clear();
        }
        for &c in &buf[..n] {
            if let Some(last) = cut_line {
                let t = Instant::now().as_millis() as u32;
                if t.wrapping_sub(last) < CUT_LINE_GAP_MS {
                    cut_line = (!matches!(c, b'\r' | b'\n')).then_some(t);
                    continue;
                }
                cut_line = None;
            }
            match c {
                b'\r' | b'\n' => {
                    if !line.is_empty() {
                        show(&line[shown..]);
                        // esp-println's `println!` ends a line with LF only; the console's lines end with CR LF
                        print!("\r\n");
                        shown = 0;
                        CLI_LINES.send(core::mem::take(&mut line)).await;
                    }
                }
                0x08 | 0x7F => {
                    if line.pop().is_some() && shown > line.len() {
                        shown = line.len();
                        print!("\x08 \x08");
                    }
                }
                _ if line.len() < 159 => {
                    line.push(c);
                    if typed {
                        show(&line[shown..]);
                        shown = line.len();
                    }
                }
                _ => {}
            }
        }
    }
}
