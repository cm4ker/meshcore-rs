//! Heltec Wireless Paper: a Heltec V3 (ESP32-S3 with 8 MB of flash, SX1262, a CP2102 on UART0)
//! with a 2.13" e-paper in place of the OLED, and its own pins around them (upstream
//! `variants/heltec_wireless_paper`, heltec-eink-modules' `Platforms/WirelessPaper`):
//! - SX1262: NSS 8, SCK 9, MOSI 10, MISO 11, RESET 12, BUSY 13, DIO1 14 (DIO2 = RF switch, DIO3 = TCXO 1.8 V)
//! - e-paper: MOSI 2, SCK 3, CS 4, DC 5, RST 6, BUSY 7
//! - Vext 45 (LOW = on), feeding the radio and the screen
//! - TX LED 18, battery divider enable 19, battery sense 20 (ADC2), PRG button 0
//!
//! No GPS header, no front-end module. The board-independent parts come from [`heltec_shared`].

#![no_std]

extern crate alloc;

pub mod battery;
pub mod companion;
pub mod epaper;
pub mod repeater;
