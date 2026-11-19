//! MeshCore Companion for the Heltec Mesh Node T096. The nRF52840 application, BLE and USB
//! are shared with the Pro Micro. The T096 supplies its own radio, TFT and GNSS wiring.

#![no_std]

extern crate alloc;

pub mod board;
pub mod companion;
pub mod gps;
pub mod tft;
