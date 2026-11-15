//! MeshCore firmware for the LilyGO T-Echo (nRF52840, SX1262, e-paper), in Rust: the board and
//! the nRF52840 side of `meshcore_fw_core`, the companion (`bin/companion.rs`) and the repeater
//! (`repeater`, `bin/repeater.rs`).
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
pub mod console;
pub mod diag;
pub mod epaper;
pub mod gps;
pub mod ota;
pub mod platform;
pub mod repeater;
pub mod screen;
pub mod usb;

/// The 2 MB QSPI flash: the node's blobs in the first 64 KiB (the repeater's, and the companion's
/// update record), the companion's data in the next megabyte, then the staging area of updates
/// (`ota`).
pub const DATA_REGION: (u32, u32) = (0, 0x1_0000);
pub const COMPANION_REGION: (u32, u32) = (0x1_0000, 0x10_0000);

/// The blobs that have no room in [`DATA_REGION`] (the repeater's sensors,
/// `Store::with_overflow`): the end of the staging area, below the page the installer keeps the
/// app's first page in (`ota`).
pub const DATA_OVERFLOW: (u32, u32) = (0x1F_C000, 0x2000);

/// Without a pass of the main loop for this long, the watchdog restarts the board. Long, as the
/// watchdog keeps running into the UF2 bootloader (the 1200 baud touch is a soft reset), which
/// does not feed it: copying the next firmware has to fit in.
pub const WATCHDOG_SECS: u32 = 60;
