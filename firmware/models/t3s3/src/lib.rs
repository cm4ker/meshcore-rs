//! What both images of the LilyGo LoRa32 T3-S3 share: the battery reading and the OLED driver. The
//! pin map is in the two binaries, where the peripherals are claimed, and is the one of upstream
//! `variants/lilygo_t3s3`.

#![no_std]

extern crate alloc;

pub mod battery;
pub mod display;
