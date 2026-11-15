//! MeshCore firmware for the Heltec Mesh Node T114 (nRF52840, SX1262), in Rust: the board
//! (`board`), its screen (`tft`, the pages of `screen` and of the companion), its GPS (`gps`),
//! its updates applied in place (`ota`), which give the app 460 KiB of its flash. The rest is the
//! Pro Micro's (`promicro`): the same chip and bootloader, the BLE on the SoftDevice, the console,
//! the repeater and the companion around the board.

#![no_std]

extern crate alloc;

pub mod board;
pub mod companion;
pub mod gps;
pub mod ota;
pub mod screen;
pub mod tft;
