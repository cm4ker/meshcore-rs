//! The companion's USB port (CDC ACM serial): upstream's frames, and the boot log as text until
//! the companion loop takes the port over. The SoftDevice runs all along (BLE), so VBUS comes as
//! its events (`sd::usb_power_events`), not from the POWER registers.
//!
//! As on the repeater, opening the port at 1200 baud and closing it restarts into the UF2
//! bootloader (its drive appears for the next firmware), so updates need no button presses.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either, select};
use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_usb::UsbDevice;
use embassy_usb::class::cdc_acm::{ControlChanged, Receiver, Sender};
use meshcore_fw_core::frames::{FRAMES_IN, FrameParser, Link, encode};

use crate::console::enter_bootloader;

pub type UsbDriver = Driver<'static, &'static SoftwareVbusDetect>;

/// Frames for the USB host.
pub static USB_OUT: Channel<CriticalSectionRawMutex, Vec<u8>, 16> = Channel::new();
/// A host has the port open (DTR): frames for it wait for room instead of being dropped.
pub static USB_HOST: AtomicBool = AtomicBool::new(false);
/// Log lines for the USB host.
static USB_TEXT: Channel<CriticalSectionRawMutex, Vec<u8>, 32> = Channel::new();

const PACKET: usize = 64;

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
