//! MeshCore firmware parts that do not depend on the chip: the LoRa radio interface and its
//! drivers (over embedded-hal), the flash store (over embedded-storage's NOR flash traits), the
//! companion frame plumbing, the companion's main loop (its on-device interface is the
//! meshcore-ui crate), the noise floor estimate, GPS sentences. The platform crates (ESP32: meshcore-fw,
//! nRF52: the board crates) add what is chip specific.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod companion;
pub mod frames;
pub mod gps;
pub mod links;
pub mod lr2021;
pub mod noise;
pub mod radio;
pub mod storage;
pub mod sx1262;


// the executor's wake-up hook, which only firmware builds provide
#[cfg(test)]
#[unsafe(no_mangle)]
fn __pender(_: *mut ()) {}
