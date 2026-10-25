#![no_std]

extern crate alloc;

pub mod board;
pub mod broker;
pub mod common;
pub mod display;
pub mod wifi_ota;

/// The board a signed release for this build must name (`meshcore::release::board`).
pub const RELEASE_BOARD: u8 =
    if cfg!(feature = "heltec-v3") { meshcore::release::board::HELTEC_V3 } else { meshcore::release::board::HELTEC_V4 };

// board-independent parts, shared with other boards
pub use meshcore_fw::{ble, clock_cell, espnow, frames, net, noise, ota, power, radio, reset, storage, sx1262};
