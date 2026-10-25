//! Wire-compatible implementation of the MeshCore LoRa mesh protocol.
//!
//! The crate is `no_std` + `alloc` and fully sans-IO: the firmware feeds it
//! received frames and the current time, and asks it which frame to transmit
//! next. Everything protocol-related (packet format, crypto, flood/direct
//! routing, the repeater role) lives here so it can be unit-tested on a host.

#![no_std]
// The application callbacks mirror the C++ `Mesh` virtuals argument for argument.
#![allow(clippy::too_many_arguments)]

extern crate alloc;

pub mod advert;
pub mod bridge;
pub mod companion;
pub mod crypto;
pub mod fwshare;
pub mod identity;
pub mod mesh;
pub mod ota;
pub mod packet;
pub mod prefs;
pub mod queue;
pub mod radio;
pub mod region;
pub mod release;
pub mod repeater;
pub mod rng;
pub mod rtc;
pub mod tables;
pub mod uplink;
pub mod util;

pub use identity::{Identity, LocalIdentity};
pub use packet::Packet;

pub const MAX_HASH_SIZE: usize = 8;
pub const PUB_KEY_SIZE: usize = 32;
pub const PRV_KEY_SIZE: usize = 64;
pub const SEED_SIZE: usize = 32;
pub const SIGNATURE_SIZE: usize = 64;
pub const MAX_ADVERT_DATA_SIZE: usize = 32;
pub const CIPHER_KEY_SIZE: usize = 16;
pub const CIPHER_BLOCK_SIZE: usize = 16;

// V1 payload format
pub const CIPHER_MAC_SIZE: usize = 2;
pub const PATH_HASH_SIZE: usize = 1;

pub const MAX_PACKET_PAYLOAD: usize = 184;
pub const MAX_GROUP_DATA_LENGTH: usize = MAX_PACKET_PAYLOAD - CIPHER_BLOCK_SIZE - 3;
pub const MAX_PATH_SIZE: usize = 64;
pub const MAX_TRANS_UNIT: usize = 255;

/// Firmware version reported to admin clients (mirrors the upstream release we track).
pub const FIRMWARE_VERSION: &str = "v1.18.0-rs";
