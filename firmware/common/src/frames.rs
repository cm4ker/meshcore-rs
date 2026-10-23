//! Companion frames (`meshcore_fw_core::frames`), and the ESP32's USB Serial/JTAG port for them.

use embassy_time::Timer;
use esp_hal::Blocking;
use esp_hal::usb::usb_serial_jtag::UsbSerialJtagRx;
pub use meshcore_fw_core::frames::*;

/// Send a frame to the USB host (dropped if nobody reads the port).
pub fn usb_write(frame: &[u8]) {
    esp_println::Printer::write_bytes(&encode(frame));
}

#[embassy_executor::task]
pub async fn usb_rx_task(mut rx: UsbSerialJtagRx<'static, Blocking>) {
    let mut parser = FrameParser::default();
    let mut buf = [0u8; 64];
    loop {
        let n = rx.drain_rx_fifo(&mut buf);
        if n == 0 {
            Timer::after_millis(5).await;
            continue;
        }
        for &c in &buf[..n] {
            if let Some(f) = parser.push(c) {
                FRAMES_IN.send((Link::Usb, f)).await;
            }
        }
    }
}
