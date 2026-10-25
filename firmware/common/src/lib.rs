//! ESP32 firmware parts shared by the board crates (Heltec V4, XIAO ESP32-C6): the chip side of
//! the radio drivers and the flash store (both in `meshcore_fw_core`), firmware slots, the watchdog,
//! the app transports (USB, TCP, BLE) and the companion role's main loop.

#![no_std]

extern crate alloc;

pub mod ble;
pub mod clock_cell;
pub mod espnow;
pub mod companion_app;
pub mod frames;
pub mod net;
pub mod ota;
pub mod power;
pub mod radio;
pub mod reset;
pub mod storage;
pub mod usb;

pub use meshcore_fw_core::{lr2021, noise, sx1262};
