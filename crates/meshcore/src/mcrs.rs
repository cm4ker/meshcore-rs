//! The `meshcore-rs` extension protocol (`MCRS`), spoken alongside the upstream MeshCore protocol.
//!
//! A single vendor opcode (`0x70`, strictly below the `0x80` push boundary and clear of Ommesh's
//! `0xF0`/`0xF1` relay frames) multiplexes all `meshcore-rs` subcommands by their second byte:
//!
//! - Companion app link (USB / BLE / TCP): `[0x70, subcmd, ...args]` -> `[0x70, subcmd, ...body]`
//!   (or standard `RESP_CODE_ERR` on an unknown subcommand or bad arguments).
//! - Remote repeater/room over LoRa (`CMD_SEND_BINARY_REQ`): `[0x70, subcmd, ...args]` ->
//!   `[tag: u32 LE][subcmd, ...body]` (empty / silent on an unknown subcommand).
//!
//! Because both transports share the `[subcmd, ...body]` payload, one encoder and one decoder
//! handle connected companions and remote nodes alike. Clients that do not know `0x70` never send
//! it, and upstream MeshCore radios never see it unless asked, keeping full two-way compatibility.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::release;

/// Current `meshcore-rs` extension protocol version (`MCRS_VERSION`).
///
/// Bump whenever a new subcommand or trailing field is added so clients can gate features on it.
///
/// - v1: `SUB_INFO` (`0x00`) on companion link and remote repeater/room LoRa requests.
/// - v2: Reliable ACKed binary P2P datagrams (`SUB_SEND_PEER_DATA`, `SUB_SYNC_PEER_DATA`,
///   `PUSH_OPCODE`, `cap::PEER_DATA`) and `REQ_TYPE_MCRS` `SUB_INFO` replies on companions.
/// - v3: Paginated repeater/room ACL requests ([`SUB_ACL_PAGE`]).
pub const MCRS_VERSION: u8 = 3;

/// Vendor opcode shared by companion frames (`CMD_MCRS` / `RESP_MCRS`) and LoRa binary requests
/// (`REQ_TYPE_MCRS`).
pub const OPCODE: u8 = 0x70;

/// Unprompted push opcode (`0xE0`, above upstream's `0x80..=0x90` and below Ommesh's `0xF0`/`0xF1`
/// phone-relay frames). Second byte identifies the notification (e.g. [`SUB_PEER_DATA_WAITING`]).
pub const PUSH_OPCODE: u8 = 0xE0;

/// Subcommand `0x00`: query protocol version, build identity and board capabilities.
pub const SUB_INFO: u8 = 0x00;

/// Subcommand `0x01`: send an ACKed encrypted binary P2P datagram to a contact.
pub const SUB_SEND_PEER_DATA: u8 = 0x01;

/// Subcommand `0x02`: pop the next incoming binary P2P datagram from the companion's queue.
pub const SUB_SYNC_PEER_DATA: u8 = 0x02;

/// Push subcode `0x03` (under [`PUSH_OPCODE`]): at least one incoming binary P2P datagram is
/// queued and ready for [`SUB_SYNC_PEER_DATA`].
pub const SUB_PEER_DATA_WAITING: u8 = 0x03;

/// Remote repeater/room subcommand `0x04`: list an ACL page (admin only).
///
/// With `CMD_SEND_BINARY_REQ`, send `[OPCODE, SUB_ACL_PAGE, offset:u8]`. The LoRa
/// plaintext reply is `[tag:u32 LE, SUB_ACL_PAGE, total:u8, returned:u8, rows...]`.
/// Each row is a six-byte public-key prefix and one permission byte. Read exactly
/// `returned` rows, ignoring cipher padding. Start at offset 0 and add `returned`
/// for the next request until the offset reaches `total` (or `returned` is zero).
/// Offsets count visible entries, and each page is read from the current ACL.
/// Discover support with [`cap::ACL_PAGES`].
pub const SUB_ACL_PAGE: u8 = 0x04;

/// Maximum binary payload length of a peer-to-peer datagram (`TXT_TYPE_PEER_DATA`).
///
/// Chosen so `8 + MAX_PEER_DATA_LEN + 1 (ext_attempt) = 159 <= 160` (10 AES-128 blocks on air)
/// and both companion TX (`15 + 150 = 165`) and RX (`17 + 150 = 167`) frames fit inside
/// `MAX_FRAME_SIZE` (176).
pub const MAX_PEER_DATA_LEN: usize = 150;

/// Maximum length of the product version string in [`McrsInfo`].
const MAX_PRODUCT_VERSION: usize = 31;

/// Fixed byte count of a `SUB_INFO` payload up to and including `product_version_len`.
const INFO_FIXED_LEN: usize = 13;

/// Capability bits reported in [`McrsInfo::caps`].
pub mod cap {
    /// Board has a display (`screen` setting).
    pub const SCREEN: u32 = 1 << 0;
    /// Board has a controllable status/TX LED (`led` setting).
    pub const LED: u32 = 1 << 1;
    /// Board has a buzzer (`buzzer` setting).
    pub const BUZZER: u32 = 1 << 2;
    /// Board has a GPS receiver (`gps` commands).
    pub const GPS: u32 = 1 << 3;
    /// Firmware was built with Wi-Fi / networking (`feature = "net"`).
    pub const WIFI: u32 = 1 << 4;
    /// Board has a controllable front-end LNA (`radio.fem.rxgain`).
    pub const FEM_LNA: u32 = 1 << 5;
    /// Board supports battery ADC multiplier calibration (`adc.multiplier`).
    pub const ADC_MULT: u32 = 1 << 6;
    /// Firmware has an OTA update slot / installer target.
    pub const OTA: u32 = 1 << 7;
    /// Inactive slot holds a previous firmware that `fw rollback` can restore.
    pub const ROLLBACK: u32 = 1 << 8;
    /// Signed network time sync (`time.sync`, `time sync`).
    pub const TIMESYNC: u32 = 1 << 9;
    /// Board has an I2C environment sensor bus (`sensor scan`).
    pub const SENSORS: u32 = 1 << 10;
    /// Repeater is configured as a room server.
    pub const ROOM: u32 = 1 << 11;
    /// Repeater can open a local BLE OTA window (`fw ble`).
    pub const BLE_OTA: u32 = 1 << 12;
    /// Repeater can serve a Wi-Fi OTA page (`fw wifi`).
    pub const WIFI_OTA: u32 = 1 << 13;
    /// Board supports light-sleep power saving (`powersaving`).
    pub const POWERSAVING: u32 = 1 << 14;
    /// Repeater supports MQTT observer uplink (`mqtt.*`).
    pub const MQTT: u32 = 1 << 15;
    /// Companion supports ACKed binary P2P datagrams (`SUB_SEND_PEER_DATA` / `SUB_SYNC_PEER_DATA`).
    pub const PEER_DATA: u32 = 1 << 16;
    /// Repeater or room supports paginated ACL queries.
    pub const ACL_PAGES: u32 = 1 << 17;
}

/// Build identity and capability snapshot returned by [`SUB_INFO`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McrsInfo {
    /// Extension protocol version ([`MCRS_VERSION`]).
    pub version: u8,
    /// Target board id (`release::board::*`, 0 if unknown).
    pub board: u8,
    /// Target role id (`release::role::*`, 0 if unknown).
    pub role: u8,
    /// Release counter (`MCFW_COUNTER`, 0 for local builds).
    pub counter: u32,
    /// Bitmask of [`cap`] flags supported by this build and board.
    pub caps: u32,
    /// Crate/product version string (e.g. `"0.9.0"`).
    pub product_version: String,
}

impl McrsInfo {
    /// Encode as the `[SUB_INFO, ...]` payload shared by companion replies and LoRa responses.
    pub fn encode(&self) -> Vec<u8> {
        let ver = &self.product_version.as_bytes()[..self.product_version.len().min(MAX_PRODUCT_VERSION)];
        let mut out = Vec::with_capacity(INFO_FIXED_LEN + ver.len());
        out.push(SUB_INFO);
        out.push(self.version);
        out.push(self.board);
        out.push(self.role);
        out.extend_from_slice(&self.counter.to_le_bytes());
        out.extend_from_slice(&self.caps.to_le_bytes());
        out.push(ver.len() as u8);
        out.extend_from_slice(ver);
        out
    }

    /// Decode a `[SUB_INFO, ...]` payload; trailing bytes from newer `MCRS_VERSION`s are ignored.
    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < INFO_FIXED_LEN || data[0] != SUB_INFO {
            return None;
        }
        let version = data[1];
        let board = data[2];
        let role = data[3];
        let counter = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let caps = u32::from_le_bytes(data[8..12].try_into().ok()?);
        let vlen = data[12] as usize;
        if vlen > MAX_PRODUCT_VERSION {
            return None;
        }
        let product_version = core::str::from_utf8(data.get(INFO_FIXED_LEN..INFO_FIXED_LEN + vlen)?).ok()?.into();
        Some(Self { version, board, role, counter, caps, product_version })
    }

    /// One-line summary for the `mcrs` CLI command.
    pub fn cli_summary(&self) -> String {
        let board = release::board_name(self.board);
        let role = release::role_name(self.role);
        match self.counter {
            0 => format!("> MCRS v{} ({} local, {board}/{role}, caps 0x{:x})", self.version, self.product_version, self.caps),
            n => format!("> MCRS v{} ({} #{n}, {board}/{role}, caps 0x{:x})", self.version, self.product_version, self.caps),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_info() {
        let info = McrsInfo {
            version: MCRS_VERSION,
            board: release::board::HELTEC_V4,
            role: release::role::COMPANION,
            counter: 168,
            caps: cap::SCREEN | cap::LED | cap::WIFI | cap::OTA | cap::TIMESYNC,
            product_version: String::from("0.9.0"),
        };
        let encoded = info.encode();
        assert_eq!(encoded[0], SUB_INFO);
        assert_eq!(encoded.len(), INFO_FIXED_LEN + 5);
        assert_eq!(McrsInfo::decode(&encoded), Some(info.clone()));

        // Future MCRS versions may append fields; v1 decoders ignore trailing bytes.
        let mut extended = encoded;
        extended.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(McrsInfo::decode(&extended), Some(info));
    }

    #[test]
    fn reject_truncated_or_wrong_sub() {
        assert!(McrsInfo::decode(&[]).is_none());
        assert!(McrsInfo::decode(&[SUB_INFO; INFO_FIXED_LEN - 1]).is_none());
        let mut bad = McrsInfo {
            version: 1,
            board: 0,
            role: 0,
            counter: 0,
            caps: 0,
            product_version: String::from("0.9.0"),
        }
        .encode();
        bad[0] = 0x01;
        assert!(McrsInfo::decode(&bad).is_none());
        bad[0] = SUB_INFO;
        bad.pop();
        assert!(McrsInfo::decode(&bad).is_none());
    }
}
