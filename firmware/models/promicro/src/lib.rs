//! MeshCore firmware for an nRF52840 Pro Micro with an SX1262 module (the "Faketec" DIY board,
//! upstream `variants/promicro`), in Rust: the board and the nRF52840 side of the repeater.
//!
//! Flashed as UF2 through the Adafruit bootloader the board ships with (its MBR, SoftDevice and
//! bootloader stay; see memory.x), so the stock firmware can always be put back the same way. The
//! SoftDevice runs only for the BLE update window (`ble`, `sd`).

#![no_std]

extern crate alloc;

pub mod ble;
pub mod board;
pub mod console;
pub mod flash;
pub mod ina219;
pub mod ota;
pub mod sd;

/// The node's data in the internal flash (`meshcore_fw_core::storage`, the 64 KiB layout
/// `tools/provision` writes): right below 0xEA000, the end of what the Adafruit bootloader takes
/// over UF2 (so a UF2 can carry it), and clear of upstream's file system (0xED000..0xF4000).
pub const DATA_REGION: (u32, u32) = (0xDA000, 0x10000);
