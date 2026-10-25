//! Companion frames over the nRF52840's own USB port (CDC ACM serial), upstream's framing.
//!
//! Like Adafruit's firmware, opening the port at 1200 baud and closing it restarts into the UF2
//! bootloader (its drive appears for the next firmware), so updates need no button presses.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either, select};
use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{ControlChanged, Receiver, Sender};
use meshcore_fw_core::frames::{FRAMES_IN, FrameParser, Link, encode};

use crate::diag::USB_TEXT;

pub type UsbDriver = Driver<'static, HardwareVbusDetect>;

/// Frames for the USB host.
pub static USB_OUT: Channel<CriticalSectionRawMutex, Vec<u8>, 16> = Channel::new();
/// A host has the port open (DTR): frames for it wait for room instead of being dropped.
pub static USB_HOST: AtomicBool = AtomicBool::new(false);

const PACKET: usize = 64;
/// `GPREGRET` value that makes the Adafruit bootloader stay in UF2 drive mode.
const DFU_MAGIC_UF2_RESET: u8 = 0x57;

/// Restart into the UF2 bootloader.
pub fn enter_bootloader() -> ! {
    embassy_nrf::pac::POWER.gpregret().write(|w| w.set_gpregret(DFU_MAGIC_UF2_RESET));
    cortex_m::peripheral::SCB::sys_reset()
}

#[embassy_executor::task]
pub async fn device_task(mut device: UsbDevice<'static, UsbDriver>) {
    device.run().await
}

#[embassy_executor::task]
pub async fn rx_task(mut rx: Receiver<'static, UsbDriver>, control: ControlChanged<'static>) {
    let mut parser = FrameParser::default();
    let mut buf = [0u8; PACKET];
    loop {
        match select(rx.read_packet(&mut buf), control.control_changed()).await {
            Either::First(Ok(n)) => {
                for &c in &buf[..n] {
                    if let Some(f) = parser.push(c) {
                        FRAMES_IN.send((Link::Usb, f)).await;
                    }
                }
            }
            Either::First(Err(_)) => rx.wait_connection().await,
            // the "1200 baud touch": port opened at 1200 baud, then closed
            Either::Second(()) => {
                USB_HOST.store(control.dtr(), Ordering::Relaxed);
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
        // companion frames, and log lines until the companion turns logging off
        let data = match select(USB_OUT.receive(), USB_TEXT.receive()).await {
            Either::First(frame) => encode(&frame),
            Either::Second(text) => text,
        };
        if !USB_HOST.load(Ordering::Relaxed) {
            continue; // nobody listens
        }
        for chunk in data.chunks(PACKET) {
            if tx.write_packet(chunk).await.is_err() {
                break; // no host: the frame is dropped
            }
        }
        if data.len() % PACKET == 0 {
            let _ = tx.write_packet(&[]).await; // a full last packet needs a zero-length one
        }
    }
}
