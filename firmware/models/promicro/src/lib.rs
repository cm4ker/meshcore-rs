//! MeshCore firmware for an nRF52840 Pro Micro with an SX1262 module (the "Faketec" DIY board,
//! upstream `variants/promicro`), in Rust: the board and the nRF52840 side of the repeater and of
//! the companion.
//!
//! Flashed as UF2 through the Adafruit bootloader the board ships with (its MBR, SoftDevice and
//! bootloader stay; see memory.x), so the stock firmware can always be put back the same way. The
//! SoftDevice runs for the repeater's BLE update window, and all the time in the companion
//! (`ble`, `sd`).

#![no_std]

extern crate alloc;

pub mod ble;
pub mod board;
pub mod companion;
pub mod console;
pub mod flash;
pub mod ina219;
pub mod ota;
pub mod sd;
pub mod sensors;
pub mod usb_frames;

/// The node's data in the internal flash (`meshcore_fw_core::storage`, the 64 KiB layout
/// `tools/provision` writes): right below 0xEA000, the end of what the Adafruit bootloader takes
/// over UF2 (so a UF2 can carry it), and clear of upstream's file system (0xED000..0xF4000).
pub const DATA_REGION: (u32, u32) = (0xDA000, 0x10000);

/// The blobs that came after [`DATA_REGION`] was laid out and have no room in it (the sensors),
/// `Store::with_overflow`: the end of the staging area, below the page the installer keeps the
/// app's first page in (`ota`).
pub const DATA_OVERFLOW: (u32, u32) = (0xD6000, 0x2000);

/// The companion's data (identity, contacts, channels, paired phones): the repeater's staging area
/// (`ota`), up to the sensors' blob. Contacts need more room than the flash has besides, so the
/// companion takes no updates over the air: UF2 over USB only. Going back to the repeater keeps
/// its own data ([`DATA_REGION`], [`DATA_OVERFLOW`]); its next update takes the area over.
pub const COMPANION_REGION: (u32, u32) = (0x7B000, 0xD6000 - 0x7B000);
