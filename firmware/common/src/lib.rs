//! ESP32 firmware parts shared by the board crates (Heltec V4, XIAO ESP32-C6): the chip side of
//! the radio drivers and the flash store (both in `meshcore_fw_core`), firmware slots, the watchdog,
//! the app transports (USB, TCP, BLE) and the companion role's main loop.

#![no_std]

extern crate alloc;

#[cfg(not(feature = "ble-trouble"))]
pub mod ble;
// the BLE interface over trouble-host, as it was until meshcore-ble: to tell a fault of the new
// host from one of a phone, until the new one has seen enough phones
#[cfg(feature = "ble-trouble")]
#[path = "ble_trouble.rs"]
pub mod ble;
pub mod broker;
pub mod clock_cell;
pub mod espnow;
pub mod fwstore;
pub mod fwstream;
pub mod companion_app;
pub mod frames;
pub mod net;
pub mod ota;
pub mod power;
pub mod radio;
pub mod repeater_app;
pub mod reset;
pub mod storage;
pub mod usb;
pub mod wifi_ota;

pub use meshcore_fw_core::{fwtext, lr2021, noise, sx1262};
