//! MeshCore firmware for the LilyGO T-Echo (nRF52840, SX1262, e-paper), in Rust: the board and
//! the nRF52840 side of `meshcore_fw_core`.
//!
//! Flashed as UF2 through the Adafruit bootloader the board ships with (its MBR, SoftDevice and
//! bootloader stay; see memory.x), so the stock firmware can always be put back the same way.

#![no_std]

extern crate alloc;

#[cfg(not(feature = "ble-trouble"))]
pub mod ble;
// the BLE interface over trouble-host, as it was until meshcore-ble: to tell a fault of the new
// host from one of a phone, until the new one has seen enough phones
#[cfg(feature = "ble-trouble")]
#[path = "ble_trouble.rs"]
pub mod ble;
pub mod board;
pub mod diag;
pub mod epaper;
pub mod gps;
pub mod ota;
pub mod platform;
pub mod usb;
