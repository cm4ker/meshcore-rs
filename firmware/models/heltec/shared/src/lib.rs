//! What the Heltec WiFi LoRa 32 V3 and V4 share: the same ESP32-S3 module, SX1262, OLED, battery
//! divider, button and GPS header, and so the same firmware around them. What sets one board apart
//! (the radio's front end, the console, the divider's switch) comes from its crate, `v3` or `v4`,
//! through [`Variant`].
//!
//! Pin map (from upstream `variants/heltec_v4`; the V3 shares all but the V4's FEM pins):
//! - SX1262: NSS 8, SCK 9, MOSI 10, MISO 11, RESET 12, BUSY 13, DIO1 14 (DIO2 = RF switch, DIO3 = TCXO 1.8 V)
//! - V4 FEM: LDO enable 7, chip enable 2, GC1109 PA mode 46, KCT8103L CTX 5
//! - TX LED 35, battery divider enable 37, battery sense 1
//! - OLED: SDA 17, SCL 18, RST 21, Vext 36 (LOW = on); PRG button 0
//! - GPS header: UART1 RX 38, TX 39, EN 34, RESET 42

#![no_std]

extern crate alloc;

pub mod battery;
pub mod companion;
pub mod console;
pub mod display;
pub mod gps;
pub mod repeater;
mod variant;

pub use variant::{ConsolePins, FemPins, FrontEnd, Variant};

// board-independent parts, shared with other boards
pub use meshcore_fw::{ble, broker, clock_cell, espnow, frames, net, noise, ota, power, radio, reset, storage, sx1262, wifi_ota};
