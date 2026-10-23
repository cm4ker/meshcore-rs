//! Board-independent firmware parts shared by the board crates (Heltec V4, XIAO ESP32-C6):
//! LoRa radio drivers behind [`radio::LoraRadio`], flash storage, firmware slots, the watchdog,
//! the app transports (USB, TCP, BLE) and the companion role's main loop.

#![no_std]

extern crate alloc;

pub mod ble;
pub mod clock_cell;
pub mod companion_app;
pub mod frames;
pub mod lr2021;
pub mod net;
pub mod noise;
pub mod ota;
pub mod power;
pub mod radio;
pub mod reset;
pub mod storage;
pub mod sx1262;
