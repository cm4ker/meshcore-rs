//! The repeater's serial console: echo what is typed and hand complete lines to the loop. Where
//! the bytes come from is the board's ([`crate::Variant::start_console`]).

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use embassy_time::Instant;
use esp_println::print;
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
