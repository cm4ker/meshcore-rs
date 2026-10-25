//! Companion frames (`meshcore_fw_core::frames`), and the ESP32's USB Serial/JTAG port for them.

use embassy_time::{Duration, with_timeout};
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

#[embassy_executor::task]
pub async fn usb_rx_task(mut input: usb::Input) {
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
