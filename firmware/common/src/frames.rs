//! Companion frames (`meshcore_fw_core::frames`), and the serial link to the host that carries
//! them: the ESP32's USB Serial/JTAG port, or a UART behind a USB bridge (the Heltec V3's CP2102).
//! Either way the link is `Link::Usb`, and the frames go out through esp-println, which writes to
//! the USB port while a host polls it and to UART0 otherwise.

use embassy_time::{Duration, with_timeout};
use esp_hal::Async;
use esp_hal::uart::UartRx;
pub use meshcore_fw_core::frames::*;

use crate::usb;

/// A frame comes in one write from the host: a pause this long in the middle of one means its
/// bytes were lost, and the rest is dropped.
const FRAME_GAP: Duration = Duration::from_millis(500);

/// Send a frame to the USB host, whole, or not at all if nobody reads the port. Written without a
/// reader, a frame filled the FIFO and was cut short there; the next host to open the port got
/// that piece first, and the app's parser took its own answer (to APP_START) for the rest of it.
pub fn usb_write(frame: &[u8]) {
    if usb::host_reading() {
        esp_println::Printer::write_bytes(&encode(frame));
    }
}

/// Send a frame over the UART: the bridge takes it whether a host has the port open or not.
pub fn uart_write(frame: &[u8]) {
    esp_println::Printer::write_bytes(&encode(frame));
}

/// Where a link's bytes come from.
trait SerialIn {
    /// Wait for input and take what has come; 0 after an error (the bytes around it are lost).
    async fn read(&mut self, buf: &mut [u8]) -> usize;
}

impl SerialIn for usb::Input {
    async fn read(&mut self, buf: &mut [u8]) -> usize {
        usb::Input::read(self, buf).await
    }
}

impl SerialIn for UartRx<'static, Async> {
    async fn read(&mut self, buf: &mut [u8]) -> usize {
        self.read_async(buf).await.unwrap_or(0)
    }
}

#[embassy_executor::task]
pub async fn usb_rx_task(input: usb::Input) {
    rx(input).await
}

#[embassy_executor::task]
pub async fn uart_rx_task(input: UartRx<'static, Async>) {
    rx(input).await
}

async fn rx(mut input: impl SerialIn) -> ! {
    let mut parser = FrameParser::default();
    let mut buf = [0u8; 64];
    loop {
        let n = if parser.partial() {
            match with_timeout(FRAME_GAP, input.read(&mut buf)).await {
                Ok(n) => n,
                Err(_) => {
                    parser.reset();
                    continue;
                }
            }
        } else {
            input.read(&mut buf).await
        };
        for &c in &buf[..n] {
            if let Some(f) = parser.push(c) {
                FRAMES_IN.send((Link::Usb, f)).await;
            }
        }
    }
}
