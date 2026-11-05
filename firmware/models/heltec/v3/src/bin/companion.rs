//! MeshCore companion radio firmware for the Heltec WiFi LoRa 32 V3
//! ([`heltec_shared::companion`]).

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]

use embassy_executor::Spawner;
use esp_backtrace as _;
use heltec_v3::HeltecV3;

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    heltec_shared::companion::run::<HeltecV3>(spawner, env!("BUILD_EPOCH"), env!("BUILD_DATE")).await
}
