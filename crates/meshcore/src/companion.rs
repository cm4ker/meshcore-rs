//! The companion radio role: a port of upstream `examples/companion_radio` (`MyMesh`) on top of
//! `BaseChatMesh`.
//!
//! A phone or PC app drives the node through a byte-frame protocol carried over USB serial, BLE
//! or TCP: [`Companion::handle_frame`] takes one command frame and [`Companion::take_frame`]
//! yields the responses and push notifications. Contacts, group channels and the offline message
//! queue live here; persistence goes through [`Effects`] and a [`BlobStore`] (raw adverts).
//!
//! Beyond upstream, the app link also carries firmware updates: frames `0xF0..=0xF5` are the
//! `crate::ota` requests, answered with the same command byte.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use crate::advert::*;
use crate::crypto::{self, Secret};
use crate::identity::Identity;
use crate::mesh::{GroupChannel, MeshApp, MeshCtx};
use crate::ota::OtaEndpoint;
use crate::packet::*;
use crate::radio::RadioParams;
use crate::region::TransportKey;
use crate::{MAX_PACKET_PAYLOAD, MAX_PATH_SIZE, MAX_TRANS_UNIT, PRV_KEY_SIZE, PUB_KEY_SIZE};

mod cmd;
pub mod prefs;

pub use prefs::*;

/// Companion protocol version (`FIRMWARE_VER_CODE`).
pub const FIRMWARE_VER_CODE: u8 = 14;
pub const MAX_FRAME_SIZE: usize = 176;
pub const MAX_CONTACTS: usize = 350;
pub const MAX_GROUP_CHANNELS: usize = 40;
pub const MAX_ANON_CONTACTS: usize = 8;
pub const OFFLINE_QUEUE_SIZE: usize = 256;
pub const MAX_TEXT_LEN: usize = 160;
pub const OUT_PATH_UNKNOWN: u8 = 0xFF;

const MAX_SEARCH_RESULTS: usize = 24;
const MAX_CONNECTIONS: usize = 16;
const EXPECTED_ACK_TABLE_SIZE: usize = 8;
const ADVERT_PATH_TABLE_SIZE: usize = 16;
const MAX_CHANNEL_DATA_LENGTH: usize = MAX_FRAME_SIZE - 9;
/// Frames waiting for the transport before the oldest are dropped.
const OUT_QUEUE_MAX: usize = 128;

const SERVER_RESPONSE_DELAY: u32 = 300;
const TXT_ACK_DELAY: u32 = 200;
const CLI_REPLY_DELAY_MILLIS: u32 = 600;
const LAZY_CONTACTS_WRITE_DELAY: u64 = 5000;

const SEND_TIMEOUT_BASE_MILLIS: u32 = 500;
const FLOOD_SEND_TIMEOUT_FACTOR: f32 = 16.0;
const DIRECT_SEND_PERHOP_FACTOR: f32 = 6.0;
const DIRECT_SEND_PERHOP_EXTRA_MILLIS: u32 = 250;

/// `izOH6cXN6mrJ5e26oRXNcg==`, the well-known key of the "Public" channel.
pub const PUBLIC_GROUP_PSK: [u8; 16] = [139, 51, 135, 233, 197, 205, 234, 106, 201, 229, 237, 186, 161, 21, 205, 114];

pub const TXT_TYPE_PLAIN: u8 = 0;
pub const TXT_TYPE_CLI_DATA: u8 = 1;
pub const TXT_TYPE_SIGNED_PLAIN: u8 = 2;
pub const TXT_TYPE_CLI_COMMAND: u8 = 3;

pub const REQ_TYPE_GET_STATUS: u8 = 0x01;
pub const REQ_TYPE_KEEP_ALIVE: u8 = 0x02;
pub const REQ_TYPE_GET_TELEMETRY_DATA: u8 = 0x03;
const RESP_SERVER_LOGIN_OK: u8 = 0;

pub const TELEM_PERM_BASE: u8 = 0x01;
pub const TELEM_PERM_LOCATION: u8 = 0x02;
pub const TELEM_PERM_ENVIRONMENT: u8 = 0x04;
const TELEM_CHANNEL_SELF: u8 = 1;
const LPP_VOLTAGE: u8 = 116;
const LPP_TEMPERATURE: u8 = 103;
const LPP_GPS: u8 = 136;

// Command frames (app -> radio).
pub const CMD_APP_START: u8 = 1;
pub const CMD_SEND_TXT_MSG: u8 = 2;
pub const CMD_SEND_CHANNEL_TXT_MSG: u8 = 3;
pub const CMD_GET_CONTACTS: u8 = 4;
pub const CMD_GET_DEVICE_TIME: u8 = 5;
pub const CMD_SET_DEVICE_TIME: u8 = 6;
pub const CMD_SEND_SELF_ADVERT: u8 = 7;
pub const CMD_SET_ADVERT_NAME: u8 = 8;
pub const CMD_ADD_UPDATE_CONTACT: u8 = 9;
pub const CMD_SYNC_NEXT_MESSAGE: u8 = 10;
pub const CMD_SET_RADIO_PARAMS: u8 = 11;
pub const CMD_SET_RADIO_TX_POWER: u8 = 12;
pub const CMD_RESET_PATH: u8 = 13;
pub const CMD_SET_ADVERT_LATLON: u8 = 14;
pub const CMD_REMOVE_CONTACT: u8 = 15;
pub const CMD_SHARE_CONTACT: u8 = 16;
pub const CMD_EXPORT_CONTACT: u8 = 17;
pub const CMD_IMPORT_CONTACT: u8 = 18;
pub const CMD_REBOOT: u8 = 19;
pub const CMD_GET_BATT_AND_STORAGE: u8 = 20;
pub const CMD_SET_TUNING_PARAMS: u8 = 21;
pub const CMD_DEVICE_QUERY: u8 = 22;
pub const CMD_EXPORT_PRIVATE_KEY: u8 = 23;
pub const CMD_IMPORT_PRIVATE_KEY: u8 = 24;
pub const CMD_SEND_RAW_DATA: u8 = 25;
pub const CMD_SEND_LOGIN: u8 = 26;
pub const CMD_SEND_STATUS_REQ: u8 = 27;
pub const CMD_HAS_CONNECTION: u8 = 28;
pub const CMD_LOGOUT: u8 = 29;
pub const CMD_GET_CONTACT_BY_KEY: u8 = 30;
pub const CMD_GET_CHANNEL: u8 = 31;
pub const CMD_SET_CHANNEL: u8 = 32;
pub const CMD_SIGN_START: u8 = 33;
pub const CMD_SIGN_DATA: u8 = 34;
pub const CMD_SIGN_FINISH: u8 = 35;
pub const CMD_SEND_TRACE_PATH: u8 = 36;
pub const CMD_SET_DEVICE_PIN: u8 = 37;
pub const CMD_SET_OTHER_PARAMS: u8 = 38;
pub const CMD_SEND_TELEMETRY_REQ: u8 = 39;
pub const CMD_GET_CUSTOM_VARS: u8 = 40;
pub const CMD_SET_CUSTOM_VAR: u8 = 41;
pub const CMD_GET_ADVERT_PATH: u8 = 42;
pub const CMD_GET_TUNING_PARAMS: u8 = 43;
pub const CMD_SEND_BINARY_REQ: u8 = 50;
pub const CMD_FACTORY_RESET: u8 = 51;
pub const CMD_SEND_PATH_DISCOVERY_REQ: u8 = 52;
pub const CMD_SET_FLOOD_SCOPE_KEY: u8 = 54;
pub const CMD_SEND_CONTROL_DATA: u8 = 55;
pub const CMD_GET_STATS: u8 = 56;
pub const CMD_SEND_ANON_REQ: u8 = 57;
pub const CMD_SET_AUTOADD_CONFIG: u8 = 58;
pub const CMD_GET_AUTOADD_CONFIG: u8 = 59;
pub const CMD_GET_ALLOWED_REPEAT_FREQ: u8 = 60;
pub const CMD_SET_PATH_HASH_MODE: u8 = 61;
pub const CMD_SEND_CHANNEL_DATA: u8 = 62;
pub const CMD_SET_DEFAULT_FLOOD_SCOPE: u8 = 63;
pub const CMD_GET_DEFAULT_FLOOD_SCOPE: u8 = 64;
pub const CMD_SEND_RAW_PACKET: u8 = 65;
pub const CMD_RUN_CLI_COMMAND: u8 = 66;

pub const STATS_TYPE_CORE: u8 = 0;
pub const STATS_TYPE_RADIO: u8 = 1;
pub const STATS_TYPE_PACKETS: u8 = 2;

// Response frames (radio -> app, replies to commands).
pub const RESP_CODE_OK: u8 = 0;
pub const RESP_CODE_ERR: u8 = 1;
pub const RESP_CODE_CONTACTS_START: u8 = 2;
pub const RESP_CODE_CONTACT: u8 = 3;
pub const RESP_CODE_END_OF_CONTACTS: u8 = 4;
pub const RESP_CODE_SELF_INFO: u8 = 5;
pub const RESP_CODE_SENT: u8 = 6;
pub const RESP_CODE_CONTACT_MSG_RECV: u8 = 7;
pub const RESP_CODE_CHANNEL_MSG_RECV: u8 = 8;
pub const RESP_CODE_CURR_TIME: u8 = 9;
pub const RESP_CODE_NO_MORE_MESSAGES: u8 = 10;
pub const RESP_CODE_EXPORT_CONTACT: u8 = 11;
pub const RESP_CODE_BATT_AND_STORAGE: u8 = 12;
pub const RESP_CODE_DEVICE_INFO: u8 = 13;
pub const RESP_CODE_PRIVATE_KEY: u8 = 14;
pub const RESP_CODE_DISABLED: u8 = 15;
pub const RESP_CODE_CONTACT_MSG_RECV_V3: u8 = 16;
pub const RESP_CODE_CHANNEL_MSG_RECV_V3: u8 = 17;
pub const RESP_CODE_CHANNEL_INFO: u8 = 18;
pub const RESP_CODE_SIGN_START: u8 = 19;
pub const RESP_CODE_SIGNATURE: u8 = 20;
pub const RESP_CODE_CUSTOM_VARS: u8 = 21;
pub const RESP_CODE_ADVERT_PATH: u8 = 22;
pub const RESP_CODE_TUNING_PARAMS: u8 = 23;
pub const RESP_CODE_STATS: u8 = 24;
pub const RESP_CODE_AUTOADD_CONFIG: u8 = 25;
pub const RESP_ALLOWED_REPEAT_FREQ: u8 = 26;
pub const RESP_CODE_CHANNEL_DATA_RECV: u8 = 27;
pub const RESP_CODE_DEFAULT_FLOOD_SCOPE: u8 = 28;
pub const RESP_CODE_CLI_REPLY: u8 = 29;

// Push frames (radio -> app, at any time).
pub const PUSH_CODE_ADVERT: u8 = 0x80;
pub const PUSH_CODE_PATH_UPDATED: u8 = 0x81;
pub const PUSH_CODE_SEND_CONFIRMED: u8 = 0x82;
pub const PUSH_CODE_MSG_WAITING: u8 = 0x83;
pub const PUSH_CODE_RAW_DATA: u8 = 0x84;
pub const PUSH_CODE_LOGIN_SUCCESS: u8 = 0x85;
pub const PUSH_CODE_LOGIN_FAIL: u8 = 0x86;
pub const PUSH_CODE_STATUS_RESPONSE: u8 = 0x87;
pub const PUSH_CODE_LOG_RX_DATA: u8 = 0x88;
pub const PUSH_CODE_TRACE_DATA: u8 = 0x89;
pub const PUSH_CODE_NEW_ADVERT: u8 = 0x8A;
pub const PUSH_CODE_TELEMETRY_RESPONSE: u8 = 0x8B;
pub const PUSH_CODE_BINARY_RESPONSE: u8 = 0x8C;
pub const PUSH_CODE_PATH_DISCOVERY_RESPONSE: u8 = 0x8D;
pub const PUSH_CODE_CONTROL_DATA: u8 = 0x8E;
pub const PUSH_CODE_CONTACT_DELETED: u8 = 0x8F;
pub const PUSH_CODE_CONTACTS_FULL: u8 = 0x90;

pub const ERR_CODE_UNSUPPORTED_CMD: u8 = 1;
pub const ERR_CODE_NOT_FOUND: u8 = 2;
pub const ERR_CODE_TABLE_FULL: u8 = 3;
pub const ERR_CODE_BAD_STATE: u8 = 4;
pub const ERR_CODE_FILE_IO_ERROR: u8 = 5;
pub const ERR_CODE_ILLEGAL_ARG: u8 = 6;

/// Frequencies (kHz) on which an app may switch on client repeat mode.
pub const REPEAT_FREQ_RANGES: [(u32, u32); 3] = [(433_000, 433_000), (869_495, 869_495), (918_000, 918_000)];

/// A contact (`ContactInfo`).
#[derive(Clone, Debug)]
pub struct ContactInfo {
    pub id: Identity,
    /// NUL-padded, at most 31 bytes used.
    pub name: [u8; 32],
    /// One of `ADV_TYPE_*`; `ADV_TYPE_NONE` marks a transient (anonymous-request) entry.
    pub adv_type: u8,
    /// Bit 0 favourite; bits 1..3 telemetry permissions; bit 4 remote CLI.
    pub flags: u8,
    pub out_path_len: u8,
    pub out_path: [u8; MAX_PATH_SIZE],
    /// By THEIR clock.
    pub last_advert_timestamp: u32,
    /// By OUR clock.
    pub lastmod: u32,
    pub gps_lat: i32,
    pub gps_lon: i32,
    pub sync_since: u32,
    secret: Option<Secret>,
}

impl ContactInfo {
    pub fn new(id: Identity) -> Self {
        Self {
            id,
            name: [0; 32],
            adv_type: ADV_TYPE_NONE,
            flags: 0,
            out_path_len: OUT_PATH_UNKNOWN,
            out_path: [0; MAX_PATH_SIZE],
            last_advert_timestamp: 0,
            lastmod: 0,
            gps_lat: 0,
            gps_lon: 0,
            sync_since: 0,
            secret: None,
        }
    }

    pub fn is_favourite(&self) -> bool {
        self.flags & 0x01 != 0
    }

    pub fn is_remote_cli_allowed(&self) -> bool {
        self.flags & 0x10 != 0
    }

    /// `StrHelper::strncpy` into the name buffer: up to the first NUL, at most 31 bytes.
    pub fn set_name(&mut self, name: &[u8]) {
        let n = name.iter().position(|&c| c == 0).unwrap_or(name.len()).min(31);
        self.name = [0; 32];
        self.name[..n].copy_from_slice(&name[..n]);
    }

    pub fn name_bytes(&self) -> &[u8] {
        let n = self.name.iter().position(|&c| c == 0).unwrap_or(31).min(31);
        &self.name[..n]
    }

    pub fn name_lossy(&self) -> String {
        String::from_utf8_lossy(self.name_bytes()).into_owned()
    }

    fn has_out_path(&self) -> bool {
        self.out_path_len != OUT_PATH_UNKNOWN
    }

    fn is_empty_slot(&self) -> bool {
        self.id.pub_key == [0; PUB_KEY_SIZE]
    }

    /// Record layout used for persistence (151 bytes).
    const RECORD_LEN: usize = 32 + 32 + 3 + MAX_PATH_SIZE + 20;

    fn write_record(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id.pub_key);
        out.extend_from_slice(&self.name);
        out.extend_from_slice(&[self.adv_type, self.flags, self.out_path_len]);
        out.extend_from_slice(&self.out_path);
        for v in [self.last_advert_timestamp, self.lastmod] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in [self.gps_lat, self.gps_lon] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.sync_since.to_le_bytes());
    }

    fn read_record(rec: &[u8]) -> Self {
        let mut c = ContactInfo::new(Identity::from_slice(&rec[..32]).unwrap());
        c.name.copy_from_slice(&rec[32..64]);
        c.name[31] = 0;
        c.adv_type = rec[64];
        c.flags = rec[65];
        c.out_path_len = rec[66];
        c.out_path.copy_from_slice(&rec[67..131]);
        c.last_advert_timestamp = read_u32(rec, 131);
        c.lastmod = read_u32(rec, 135);
        c.gps_lat = read_u32(rec, 139) as i32;
        c.gps_lon = read_u32(rec, 143) as i32;
        c.sync_since = read_u32(rec, 147);
        c
    }
}

/// A group channel slot (`ChannelDetails`); an empty name marks an unused slot.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Channel {
    pub name: String,
    /// A 128-bit key is zero-padded to 32 bytes.
    pub secret: Secret,
    pub hash: u8,
}

impl Channel {
    pub fn new(name: &str, key: &[u8]) -> Self {
        let mut secret = [0u8; PUB_KEY_SIZE];
        let n = key.len().min(PUB_KEY_SIZE);
        secret[..n].copy_from_slice(&key[..n]);
        let mut ch = Channel { name: crate::util::truncate_str(name, 32), secret, hash: 0 };
        ch.update_hash();
        ch
    }

    /// The channel hash is the first byte of SHA-256 over the 128- or 256-bit key.
    fn update_hash(&mut self) {
        let key_len = if self.secret[16..] == [0; 16] { 16 } else { 32 };
        let mut h = [0u8; 1];
        crypto::sha256(&mut h, &[&self.secret[..key_len]]);
        self.hash = h[0];
    }

    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
    }

    fn group(&self) -> GroupChannel {
        GroupChannel { hash: self.hash, secret: self.secret }
    }
}

/// Persistent storage for the last raw advert of each contact (for "share" / "export").
pub trait BlobStore {
    fn get(&self, key: &[u8; PUB_KEY_SIZE]) -> Option<Vec<u8>>;
    fn put(&mut self, key: &[u8; PUB_KEY_SIZE], data: &[u8]) -> bool;
    fn delete(&mut self, key: &[u8; PUB_KEY_SIZE]);
    /// Storage used and total, in KiB (`CMD_GET_BATT_AND_STORAGE`).
    fn usage_kb(&self) -> (u32, u32) {
        (0, 0)
    }
}

/// A [`BlobStore`] in RAM, for hosts without flash (tests, simulators).
#[derive(Default)]
pub struct MemBlobStore {
    map: BTreeMap<[u8; PUB_KEY_SIZE], Vec<u8>>,
}

impl BlobStore for MemBlobStore {
    fn get(&self, key: &[u8; PUB_KEY_SIZE]) -> Option<Vec<u8>> {
        self.map.get(key).cloned()
    }

    fn put(&mut self, key: &[u8; PUB_KEY_SIZE], data: &[u8]) -> bool {
        self.map.insert(*key, data.to_vec());
        true
    }

    fn delete(&mut self, key: &[u8; PUB_KEY_SIZE]) {
        self.map.remove(key);
    }
}

/// Hardware-facing requests, drained by the firmware.
#[derive(Default, Debug)]
pub struct Effects {
    pub save_prefs: bool,
    pub save_contacts: bool,
    pub save_channels: bool,
    pub save_identity: Option<[u8; PRV_KEY_SIZE]>,
    pub reboot: bool,
    pub power_off: bool,
    /// Erase all stored data, then reboot.
    pub factory_reset: bool,
    pub apply_radio: Option<RadioParams>,
    pub tx_power: Option<i8>,
    pub rx_boosted_gain: Option<bool>,
    pub fem_lna: Option<bool>,
    /// Wi-Fi settings changed (applied after a reboot).
    pub network_changed: bool,
    /// The GPS was switched on or off (`prefs.gps_enabled`).
    pub gps: bool,
}

/// Live board values the firmware keeps up to date.
#[derive(Clone, Debug)]
pub struct Board {
    pub manufacturer: String,
    pub build_date: String,
    pub batt_mv: u16,
    pub mcu_temp_c: Option<f32>,
    pub max_tx_power_dbm: i8,
    /// Status line for `get wifi.status`.
    pub wifi_status: String,
    pub wifi_ip: Option<String>,
    /// A GPS receiver: its `gps` setting is offered to the app.
    pub has_gps: bool,
    /// Altitude of the latest GPS position, in metres.
    pub altitude_m: f64,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            manufacturer: "Heltec V4".into(),
            build_date: String::new(),
            batt_mv: 0,
            mcu_temp_c: None,
            max_tx_power_dbm: 22,
            wifi_status: String::new(),
            wifi_ip: None,
            has_gps: false,
            altitude_m: 0.0,
        }
    }
}

/// An inbound path heard in a contact's advert (`AdvertPath`).
#[derive(Clone, Debug, Default)]
struct AdvertPath {
    pubkey_prefix: [u8; 7],
    path_len: u8,
    recv_timestamp: u32,
    path: Vec<u8>,
}

#[derive(Clone, Debug)]
struct AckEntry {
    msg_sent: u64,
    ack: u32,
    contact: [u8; PUB_KEY_SIZE],
}

/// A server we are logged in to (`ConnectionInfo`). Upstream no longer sends the keep-alive
/// pings (`checkConnections` is disabled), so only the bookkeeping remains.
#[derive(Clone, Debug)]
struct Connection {
    server: [u8; PUB_KEY_SIZE],
    last_activity: u32,
}

/// What a sent message or request went out as (`MSG_SEND_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sent {
    Flood,
    Direct,
}

impl Sent {
    fn flag(self) -> u8 {
        if self == Sent::Flood { 1 } else { 0 }
    }
}

/// Received text messages kept for an on-device screen (the app gets its own copy).
const RECENT_MESSAGES: usize = 8;
/// Nodes heard advertising, kept for an on-device screen.
const HEARD_NODES: usize = 8;

/// A received text message, for an on-device screen.
#[derive(Clone, Debug, PartialEq)]
pub struct RecentMessage {
    /// The contact's name, or the channel's.
    pub from: String,
    pub text: String,
    /// Our clock when it arrived.
    pub received: u32,
}

/// A node heard advertising, for an on-device screen.
#[derive(Clone, Debug, PartialEq)]
pub struct HeardNode {
    pub name: String,
    pub key_prefix: [u8; 4],
    pub adv_type: u8,
    /// Hops the advert travelled; 0 = heard directly.
    pub hops: u8,
    /// SNR of the last hop, multiplied by 4.
    pub snr: i8,
    /// Our clock when it was heard.
    pub heard: u32,
}

pub struct Companion {
    pub prefs: CompanionPrefs,
    pub board: Board,
    pub effects: Effects,
    pub blobs: Box<dyn BlobStore>,
    /// Firmware updates over the app link; set its `target` if the platform supports updates.
    pub ota: OtaEndpoint,
    /// The app is attached (upstream `isConnected()`): push frames are dropped otherwise.
    pub connected: bool,
    /// The latest text messages, oldest first.
    pub recent: VecDeque<RecentMessage>,
    /// The latest nodes heard advertising, most recent first.
    pub heard: VecDeque<HeardNode>,
    /// Transient slots first (`MAX_ANON_CONTACTS`), then the real contacts, like upstream.
    contacts: Vec<ContactInfo>,
    matching_peers: Vec<usize>,
    channels: Vec<Channel>,
    connections: Vec<Connection>,
    out: VecDeque<Vec<u8>>,
    offline_queue: VecDeque<Vec<u8>>,
    expected_acks: [Option<AckEntry>; EXPECTED_ACK_TABLE_SIZE],
    next_ack_idx: usize,
    advert_paths: Vec<AdvertPath>,
    app_target_ver: u8,
    pending_login: u32,
    pending_status: u32,
    pending_telemetry: u32,
    pending_discovery: u32,
    pending_req: u32,
    /// Contacts iterator of `CMD_GET_CONTACTS`: (next index, since filter, most recent lastmod).
    iter: Option<(usize, u32, u32)>,
    sign_data: Option<Vec<u8>>,
    send_scope: TransportKey,
    send_unscoped: bool,
    dirty_contacts_expiry: u64,
    /// An imported advert, processed as if received (`_pendingLoopback`).
    loopback: Option<Box<Packet>>,
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).unwrap_or(0)
}

/// Bytes up to the first NUL (decrypted payloads are zero-padded).
fn c_str(b: &[u8]) -> &[u8] {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    &b[..n]
}

impl Companion {
    pub fn new(prefs: CompanionPrefs, board: Board) -> Self {
        let mut c = Self {
            prefs,
            board,
            effects: Effects::default(),
            blobs: Box::new(MemBlobStore::default()),
            ota: OtaEndpoint::default(),
            connected: true,
            recent: VecDeque::new(),
            heard: VecDeque::new(),
            contacts: Vec::new(),
            matching_peers: Vec::new(),
            channels: alloc::vec![Channel::default(); MAX_GROUP_CHANNELS],
            connections: Vec::new(),
            out: VecDeque::new(),
            offline_queue: VecDeque::new(),
            expected_acks: Default::default(),
            next_ack_idx: 0,
            advert_paths: Vec::new(),
            app_target_ver: 0,
            pending_login: 0,
            pending_status: 0,
            pending_telemetry: 0,
            pending_discovery: 0,
            pending_req: 0,
            iter: None,
            sign_data: None,
            send_scope: TransportKey::default(),
            send_unscoped: false,
            dirty_contacts_expiry: 0,
            loopback: None,
        };
        c.reset_contacts();
        c.channels[0] = Channel::new("Public", &PUBLIC_GROUP_PSK); // pre-configured
        c
    }

    /// Finish start-up (call once after `Mesh::begin` and loading the stored data): default
    /// name, clock bootstrap from the contacts.
    pub fn start(&mut self, ctx: &mut MeshCtx) {
        if self.prefs.node_name.is_empty() {
            self.prefs.node_name = crate::util::to_hex(&ctx.self_id.pub_key()[..4]);
        }
        let latest = self.contacts.iter().map(|c| c.lastmod).max().unwrap_or(0);
        if latest != 0 && latest + 1 > ctx.current_time() {
            ctx.set_current_time(latest + 1);
        }
    }

    pub fn take_effects(&mut self) -> Effects {
        core::mem::take(&mut self.effects)
    }

    /// Next frame for the app, if any.
    pub fn take_frame(&mut self) -> Option<Vec<u8>> {
        if self.out.is_empty() {
            self.next_contact_frame();
        }
        self.out.pop_front()
    }

    pub fn has_frames(&self) -> bool {
        !self.out.is_empty() || self.iter.is_some()
    }

    /// Messages waiting in the offline queue.
    pub fn queued_messages(&self) -> usize {
        self.offline_queue.len()
    }

    /// Outbound packets or a contacts write still pending (`hasPendingWork`).
    pub fn has_pending_work(&self, ctx: &MeshCtx) -> bool {
        ctx.outbound_len() > 0 || self.dirty_contacts_expiry != 0
    }

    fn write_frame(&mut self, frame: &[u8]) {
        if frame.len() > MAX_FRAME_SIZE || !self.connected {
            return;
        }
        if self.out.len() >= OUT_QUEUE_MAX {
            self.out.pop_front();
        }
        self.out.push_back(frame.to_vec());
    }

    fn write_ok(&mut self) {
        self.write_frame(&[RESP_CODE_OK]);
    }

    fn write_err(&mut self, code: u8) {
        self.write_frame(&[RESP_CODE_ERR, code]);
    }

    fn contact_frame(code: u8, c: &ContactInfo) -> Vec<u8> {
        let mut f = Vec::with_capacity(148);
        f.push(code);
        f.extend_from_slice(&c.id.pub_key);
        f.extend_from_slice(&[c.adv_type, c.flags, c.out_path_len]);
        f.extend_from_slice(&c.out_path);
        let mut name = [0u8; 32];
        name[..c.name_bytes().len()].copy_from_slice(c.name_bytes());
        f.extend_from_slice(&name);
        f.extend_from_slice(&c.last_advert_timestamp.to_le_bytes());
        f.extend_from_slice(&c.gps_lat.to_le_bytes());
        f.extend_from_slice(&c.gps_lon.to_le_bytes());
        f.extend_from_slice(&c.lastmod.to_le_bytes());
        f
    }

    fn next_contact_frame(&mut self) {
        let Some((idx, since, most_recent)) = self.iter else {
            return;
        };
        if let Some(c) = self.contacts.get(idx) {
            let mut most_recent = most_recent;
            if c.lastmod > since {
                let f = Self::contact_frame(RESP_CODE_CONTACT, c);
                most_recent = most_recent.max(c.lastmod);
                self.write_frame(&f);
            }
            self.iter = Some((idx + 1, since, most_recent));
        } else {
            let mut f = alloc::vec![RESP_CODE_END_OF_CONTACTS];
            f.extend_from_slice(&most_recent.to_le_bytes());
            self.write_frame(&f);
            self.iter = None;
        }
    }

    fn mark_contacts_dirty(&mut self, ctx: &MeshCtx) {
        self.dirty_contacts_expiry = ctx.now + LAZY_CONTACTS_WRITE_DELAY;
    }

    // ---------------------------------------------------------------- contacts

    fn reset_contacts(&mut self) {
        self.contacts.clear();
        self.contacts.resize(MAX_ANON_CONTACTS, ContactInfo::new(Identity::new([0; PUB_KEY_SIZE])));
    }

    /// Real contacts (the transient slots excluded).
    pub fn contacts(&self) -> &[ContactInfo] {
        &self.contacts[MAX_ANON_CONTACTS..]
    }

    pub fn num_contacts(&self) -> usize {
        self.contacts.len() - MAX_ANON_CONTACTS
    }

    pub fn channels(&self) -> &[Channel] {
        &self.channels
    }

    pub fn contacts_serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for c in self.contacts().iter().filter(|c| c.adv_type != ADV_TYPE_NONE) {
            c.write_record(&mut out);
        }
        out
    }

    pub fn contacts_load(&mut self, data: &[u8]) {
        self.reset_contacts();
        for rec in data.as_chunks::<{ ContactInfo::RECORD_LEN }>().0.iter().take(MAX_CONTACTS) {
            self.contacts.push(ContactInfo::read_record(rec));
        }
    }

    /// Non-empty slots: `[idx][name 32][secret 32]` each.
    pub fn channels_serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, ch) in self.channels.iter().enumerate().filter(|(_, c)| !c.is_empty()) {
            out.push(i as u8);
            let mut name = [0u8; 32];
            let n = ch.name.len().min(31);
            name[..n].copy_from_slice(&ch.name.as_bytes()[..n]);
            out.extend_from_slice(&name);
            out.extend_from_slice(&ch.secret);
        }
        out
    }

    pub fn channels_load(&mut self, data: &[u8]) {
        for rec in data.as_chunks::<65>().0 {
            let idx = rec[0] as usize;
            if idx < MAX_GROUP_CHANNELS {
                let name = String::from_utf8_lossy(c_str(&rec[1..33])).into_owned();
                self.channels[idx] = Channel::new(&name, &rec[33..65]);
            }
        }
    }

    pub fn lookup_contact(&self, pub_key_prefix: &[u8]) -> Option<usize> {
        self.contacts.iter().position(|c| !c.is_empty_slot() && c.id.pub_key.starts_with(pub_key_prefix))
    }

    fn secret_of(&mut self, ctx: &MeshCtx, idx: usize) -> Secret {
        let c = &mut self.contacts[idx];
        *c.secret.get_or_insert_with(|| ctx.self_id.calc_shared_secret(&c.id.pub_key).unwrap_or([0; PUB_KEY_SIZE]))
    }

    /// `allocateContactSlot`: a transient slot, a new slot, or the oldest non-favourite.
    fn allocate_contact_slot(&mut self, transient_only: bool) -> Option<usize> {
        if transient_only {
            return (0..MAX_ANON_CONTACTS)
                .filter(|&i| self.contacts[i].adv_type == ADV_TYPE_NONE)
                .min_by_key(|&i| self.contacts[i].lastmod);
        }
        if self.contacts.len() < MAX_ANON_CONTACTS + MAX_CONTACTS {
            self.contacts.push(ContactInfo::new(Identity::new([0; PUB_KEY_SIZE])));
            return Some(self.contacts.len() - 1);
        }
        if self.prefs.autoadd_config & AUTO_ADD_OVERWRITE_OLDEST != 0 {
            let idx = (MAX_ANON_CONTACTS..self.contacts.len())
                .filter(|&i| !self.contacts[i].is_favourite())
                .min_by_key(|&i| self.contacts[i].lastmod)?;
            let key = self.contacts[idx].id.pub_key;
            self.blobs.delete(&key);
            let mut f = alloc::vec![PUSH_CODE_CONTACT_DELETED];
            f.extend_from_slice(&key);
            self.write_frame(&f);
            return Some(idx);
        }
        None
    }

    pub fn add_contact(&mut self, contact: ContactInfo) -> bool {
        match self.allocate_contact_slot(contact.adv_type == ADV_TYPE_NONE) {
            Some(i) => {
                self.contacts[i] = ContactInfo { secret: None, ..contact };
                true
            }
            None => false,
        }
    }

    fn remove_contact(&mut self, idx: usize) {
        if idx < MAX_ANON_CONTACTS {
            self.contacts[idx] = ContactInfo::new(Identity::new([0; PUB_KEY_SIZE]));
        } else {
            self.contacts.remove(idx);
        }
    }

    fn populate_from_advert(id: &Identity, adv: &AdvertData, name: &[u8], timestamp: u32, now: u32) -> ContactInfo {
        let mut ci = ContactInfo::new(*id);
        ci.set_name(name);
        ci.adv_type = adv.adv_type;
        if let Some((lat, lon)) = adv.location {
            ci.gps_lat = lat;
            ci.gps_lon = lon;
        }
        ci.last_advert_timestamp = timestamp;
        ci.lastmod = now;
        ci
    }

    fn should_auto_add_type(&self, adv_type: u8) -> bool {
        if self.prefs.manual_add_contacts & 1 == 0 {
            return true;
        }
        let bit = match adv_type {
            ADV_TYPE_CHAT => AUTO_ADD_CHAT,
            ADV_TYPE_REPEATER => AUTO_ADD_REPEATER,
            ADV_TYPE_ROOM => AUTO_ADD_ROOM_SERVER,
            ADV_TYPE_SENSOR => AUTO_ADD_SENSOR,
            _ => return false,
        };
        self.prefs.autoadd_config & bit != 0
    }

    /// `onDiscoveredContact`: tell the app and remember the inbound path.
    fn on_discovered_contact(&mut self, ctx: &MeshCtx, contact: &ContactInfo, is_new: bool, path_len: u8, path: &[u8]) {
        if is_new {
            let f = Self::contact_frame(PUSH_CODE_NEW_ADVERT, contact);
            self.write_frame(&f);
        } else {
            let mut f = alloc::vec![PUSH_CODE_ADVERT];
            f.extend_from_slice(&contact.id.pub_key);
            self.write_frame(&f);
        }

        if Packet::is_valid_path_len(path_len) {
            let prefix: [u8; 7] = contact.id.pub_key[..7].try_into().unwrap();
            let slot = match self.advert_paths.iter().position(|p| p.pubkey_prefix == prefix) {
                Some(i) => i,
                None if self.advert_paths.len() < ADVERT_PATH_TABLE_SIZE => {
                    self.advert_paths.push(AdvertPath::default());
                    self.advert_paths.len() - 1
                }
                None => (0..self.advert_paths.len()).min_by_key(|&i| self.advert_paths[i].recv_timestamp).unwrap(),
            };
            let n = Packet::path_bytes_for(path_len).min(path.len());
            self.advert_paths[slot] = AdvertPath {
                pubkey_prefix: prefix,
                path_len,
                recv_timestamp: ctx.current_time(),
                path: path[..n].to_vec(),
            };
        }

        if !is_new {
            self.mark_contacts_dirty(ctx);
        }
    }

    // ---------------------------------------------------------------- sending

    fn send_flood_with_key(&mut self, ctx: &mut MeshCtx, key: TransportKey, pkt: Box<Packet>, delay_ms: u32) {
        let hash_size = self.prefs.path_hash_mode as usize + 1;
        if key.is_null() {
            ctx.send_flood(pkt, delay_ms, hash_size);
        } else {
            let codes = [key.transport_code(&pkt), 0];
            ctx.send_flood_scoped(pkt, codes, delay_ms, hash_size);
        }
    }

    /// `sendFloodScoped`: the app's scope override, else the default scope.
    fn send_flood_scoped(&mut self, ctx: &mut MeshCtx, pkt: Box<Packet>, delay_ms: u32) {
        if self.send_unscoped {
            ctx.send_flood(pkt, delay_ms, self.prefs.path_hash_mode as usize + 1);
        } else {
            let key = if self.send_scope.is_null() { TransportKey(self.prefs.default_scope_key) } else { self.send_scope };
            self.send_flood_with_key(ctx, key, pkt, delay_ms);
        }
    }

    /// Direct along the known path, else flood.
    fn send_to_contact(&mut self, ctx: &mut MeshCtx, idx: usize, pkt: Box<Packet>, delay_ms: u32) -> Sent {
        let c = &self.contacts[idx];
        if c.has_out_path() {
            let (len, path) = (c.out_path_len, c.out_path);
            ctx.send_direct(pkt, len, &path, delay_ms);
            Sent::Direct
        } else {
            self.send_flood_scoped(ctx, pkt, delay_ms);
            Sent::Flood
        }
    }

    fn calc_flood_timeout(airtime: u32) -> u32 {
        SEND_TIMEOUT_BASE_MILLIS + (FLOOD_SEND_TIMEOUT_FACTOR * airtime as f32) as u32
    }

    fn calc_direct_timeout(airtime: u32, path_len: u8) -> u32 {
        let hops = (path_len & 63) as u32;
        SEND_TIMEOUT_BASE_MILLIS
            + ((airtime as f32 * DIRECT_SEND_PERHOP_FACTOR + DIRECT_SEND_PERHOP_EXTRA_MILLIS as f32) * (hops + 1) as f32)
                as u32
    }

    /// Send to a contact and estimate the reply timeout (`sendMessage` and friends).
    fn send_with_timeout(&mut self, ctx: &mut MeshCtx, idx: usize, pkt: Box<Packet>) -> (Sent, u32) {
        let t = ctx.radio.est_airtime_ms(pkt.raw_length());
        let path_len = self.contacts[idx].out_path_len;
        let sent = self.send_to_contact(ctx, idx, pkt, 0);
        let est = match sent {
            Sent::Flood => Self::calc_flood_timeout(t),
            Sent::Direct => Self::calc_direct_timeout(t, path_len),
        };
        (sent, est)
    }

    fn send_ack_to(&mut self, ctx: &mut MeshCtx, idx: usize, ack: &[u8]) {
        let c = &self.contacts[idx];
        if !c.has_out_path() {
            let pkt = ctx.create_ack(ack);
            self.send_flood_scoped(ctx, pkt, TXT_ACK_DELAY);
        } else {
            let (len, path) = (c.out_path_len, c.out_path);
            let mut d = TXT_ACK_DELAY;
            if self.prefs.multi_acks > 0 {
                let a1 = ctx.create_multi_ack(ack, 1);
                ctx.send_direct(a1, len, &path, d);
                d += 300;
            }
            let a2 = ctx.create_ack(ack);
            ctx.send_direct(a2, len, &path, d);
        }
    }

    /// Reply to a flood-routed packet: a PATH return carrying `extra`, so the sender learns the
    /// route here.
    fn send_path_return(
        &mut self, ctx: &mut MeshCtx, idx: usize, secret: &Secret, pkt: &Packet, extra_type: u8, extra: &[u8],
        delay_ms: u32,
    ) {
        let dest = self.contacts[idx].id.hash();
        if let Some(path) = ctx.create_path_return(dest, secret, pkt.path_len, &pkt.path, extra_type, extra) {
            self.send_flood_scoped(ctx, path, delay_ms);
        }
    }

    /// `handleReturnPathRetry`: the peer still floods although we have a direct path, so resend
    /// the reciprocal path directly.
    fn return_path_retry(&mut self, ctx: &mut MeshCtx, idx: usize, path_len: u8, path: &[u8]) {
        let secret = self.secret_of(ctx, idx);
        let c = &self.contacts[idx];
        let (dest, out_len, out_path) = (c.id.hash(), c.out_path_len, c.out_path);
        if let Some(rpath) = ctx.create_path_return(dest, &secret, path_len, path, 0, &[]) {
            ctx.send_direct(rpath, out_len, &out_path, 3000);
        }
    }

    fn self_advert(&mut self, ctx: &mut MeshCtx) -> Option<Box<Packet>> {
        let mut adv = AdvertData::new(ADV_TYPE_CHAT, &self.prefs.node_name);
        if self.prefs.advert_loc_policy != crate::prefs::ADVERT_LOC_NONE {
            adv = adv.with_location(self.prefs.node_lat, self.prefs.node_lon);
        }
        let mut data = [0u8; crate::MAX_ADVERT_DATA_SIZE];
        let n = adv.encode(&mut data);
        ctx.create_advert(&data[..n])
    }

    /// Zero-hop self advert (the UI's "advert" button).
    pub fn advert(&mut self, ctx: &mut MeshCtx) -> bool {
        match self.self_advert(ctx) {
            Some(pkt) => {
                ctx.send_zero_hop(pkt, 0);
                true
            }
            None => false,
        }
    }

    /// The GPS is on: its positions are the node's.
    pub fn gps_active(&self) -> bool {
        self.board.has_gps && self.prefs.gps_enabled
    }

    /// A position from the GPS (`EnvironmentSensorManager::loop`), in millionths of a degree and
    /// millimetres: the node's location from now on.
    pub fn set_gps_position(&mut self, lat: i32, lon: i32, alt_mm: i32) {
        self.prefs.node_lat = lat as f64 / 1e6;
        self.prefs.node_lon = lon as f64 / 1e6;
        self.board.altitude_m = alt_mm as f64 / 1000.0;
    }

    fn lpp_voltage(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[TELEM_CHANNEL_SELF, LPP_VOLTAGE]);
        out.extend_from_slice(&((self.board.batt_mv as u32 / 10) as u16).to_be_bytes());
    }

    fn lpp_temperature(&self, out: &mut Vec<u8>) {
        if let Some(t) = self.board.mcu_temp_c {
            out.extend_from_slice(&[TELEM_CHANNEL_SELF, LPP_TEMPERATURE]);
            out.extend_from_slice(&((t * 10.0) as i16).to_be_bytes());
        }
    }

    /// `querySensors`: the position while the GPS is on, if `perms` allow it.
    fn lpp_sensors(&self, out: &mut Vec<u8>, perms: u8) {
        if perms & TELEM_PERM_LOCATION != 0 && self.gps_active() {
            out.extend_from_slice(&[TELEM_CHANNEL_SELF, LPP_GPS]);
            // 1/10000 degree and centimetres, 24 bits each
            let p = &self.prefs;
            for v in [(p.node_lat * 10_000.0) as i32, (p.node_lon * 10_000.0) as i32, (self.board.altitude_m * 100.0) as i32] {
                out.extend_from_slice(&v.to_be_bytes()[1..]);
            }
        }
    }

    // ----------------------------------------------------------- app delivery

    fn add_to_offline_queue(&mut self, frame: &[u8]) {
        if self.offline_queue.len() >= OFFLINE_QUEUE_SIZE {
            // make room by dropping the oldest channel message; keep direct messages
            let is_channel = |f: &Vec<u8>| {
                matches!(f[0], RESP_CODE_CHANNEL_MSG_RECV | RESP_CODE_CHANNEL_MSG_RECV_V3 | RESP_CODE_CHANNEL_DATA_RECV)
            };
            let Some(pos) = self.offline_queue.iter().position(is_channel) else {
                return;
            };
            self.offline_queue.remove(pos);
        }
        self.offline_queue.push_back(frame.to_vec());
        self.write_frame(&[PUSH_CODE_MSG_WAITING]);
    }

    fn note_message(&mut self, from: String, text: &[u8], now: u32) {
        if self.recent.len() >= RECENT_MESSAGES {
            self.recent.pop_front();
        }
        self.recent.push_back(RecentMessage { from, text: String::from_utf8_lossy(text).into_owned(), received: now });
    }

    fn note_heard(&mut self, node: HeardNode) {
        self.heard.retain(|h| h.key_prefix != node.key_prefix);
        self.heard.push_front(node);
        self.heard.truncate(HEARD_NODES);
    }

    fn queue_message(&mut self, idx: usize, txt_type: u8, pkt: &Packet, sender_timestamp: u32, extra: &[u8], text: &[u8]) {
        let mut f = Vec::with_capacity(MAX_FRAME_SIZE);
        if self.app_target_ver >= 3 {
            f.extend_from_slice(&[RESP_CODE_CONTACT_MSG_RECV_V3, pkt.snr as u8, 0, 0]);
        } else {
            f.push(RESP_CODE_CONTACT_MSG_RECV);
        }
        f.extend_from_slice(&self.contacts[idx].id.pub_key[..6]);
        f.push(if pkt.is_route_flood() { pkt.path_len } else { 0xFF });
        f.push(txt_type);
        f.extend_from_slice(&sender_timestamp.to_le_bytes());
        f.extend_from_slice(extra);
        let n = text.len().min(MAX_FRAME_SIZE - f.len());
        f.extend_from_slice(&text[..n]);
        self.add_to_offline_queue(&f);
    }

    fn mark_connection_active(&mut self, ctx: &MeshCtx, idx: usize) {
        let key = self.contacts[idx].id.pub_key;
        if let Some(c) = self.connections.iter_mut().find(|c| c.server == key) {
            c.last_activity = ctx.current_time();
        }
    }

    fn start_connection(&mut self, ctx: &MeshCtx, idx: usize) -> bool {
        let key = self.contacts[idx].id.pub_key;
        let conn = Connection { server: key, last_activity: ctx.current_time() };
        if let Some(c) = self.connections.iter_mut().find(|c| c.server == key) {
            *c = conn;
        } else if self.connections.len() < MAX_CONNECTIONS {
            self.connections.push(conn);
        } else {
            return false;
        }
        true
    }

    fn has_connection_to(&self, pub_key: &[u8]) -> bool {
        self.connections.iter().any(|c| c.server[..] == *pub_key)
    }

    fn stop_connection(&mut self, pub_key: &[u8]) {
        self.connections.retain(|c| c.server[..] != *pub_key);
    }

    /// `processAck`: a delivery confirmation for one of our messages. Returns the contact it came
    /// from.
    fn process_ack(&mut self, ctx: &MeshCtx, ack: u32) -> Option<usize> {
        if let Some(slot) = self.expected_acks.iter_mut().find(|e| e.as_ref().is_some_and(|e| e.ack == ack)) {
            let e = slot.take().unwrap(); // the same ACK can arrive more than once
            let mut f = alloc::vec![PUSH_CODE_SEND_CONFIRMED];
            f.extend_from_slice(&ack.to_le_bytes());
            f.extend_from_slice(&((ctx.now - e.msg_sent) as u32).to_le_bytes());
            self.write_frame(&f);
            return self.lookup_contact(&e.contact);
        }
        None // `checkConnectionsAck`: keep-alives are no longer sent
    }

    fn clear_pending_reqs(&mut self) {
        self.pending_login = 0;
        self.pending_status = 0;
        self.pending_telemetry = 0;
        self.pending_discovery = 0;
        self.pending_req = 0;
    }

    /// `onContactRequest`: telemetry requests from other clients.
    fn on_contact_request(&mut self, idx: usize, sender_timestamp: u32, data: &[u8]) -> Vec<u8> {
        let mut reply = Vec::new();
        if data.first() == Some(&REQ_TYPE_GET_TELEMETRY_DATA) {
            let cp = self.contacts[idx].flags >> 1; // bit 0 is 'favourite'
            let mode = |m: u8, bit: u8| match m {
                TELEM_MODE_ALLOW_ALL => bit,
                TELEM_MODE_ALLOW_FLAGS => cp & bit,
                _ => 0,
            };
            let mut perms = mode(self.prefs.telemetry_mode_base, TELEM_PERM_BASE)
                | mode(self.prefs.telemetry_mode_loc, TELEM_PERM_LOCATION)
                | mode(self.prefs.telemetry_mode_env, TELEM_PERM_ENVIRONMENT);
            perms &= !data.get(1).copied().unwrap_or(0); // inverse mask from the requester
            if perms & TELEM_PERM_BASE != 0 {
                reply.extend_from_slice(&sender_timestamp.to_le_bytes());
                self.lpp_voltage(&mut reply);
                self.lpp_sensors(&mut reply, perms);
                self.lpp_temperature(&mut reply);
            }
        }
        reply
    }

    /// `onContactResponse`: match a response to the pending login / status / request.
    fn on_contact_response(&mut self, ctx: &MeshCtx, idx: usize, data: &[u8]) {
        let tag = read_u32(data, 0);
        let key = self.contacts[idx].id.pub_key;
        let prefix4 = u32::from_le_bytes(key[..4].try_into().unwrap());
        let at = |i: usize| data.get(i).copied().unwrap_or(0);
        let mut f = Vec::new();
        if self.pending_login != 0 && self.pending_login == prefix4 {
            self.pending_login = 0;
            if data.get(4..6) == Some(b"OK") {
                // legacy repeater login
                f.extend_from_slice(&[PUSH_CODE_LOGIN_SUCCESS, 0]);
                f.extend_from_slice(&key[..6]);
            } else if at(4) == RESP_SERVER_LOGIN_OK {
                let keep_alive_secs = at(5) as u16 * 16;
                if keep_alive_secs > 0 {
                    self.start_connection(ctx, idx);
                }
                f.extend_from_slice(&[PUSH_CODE_LOGIN_SUCCESS, at(6)]);
                f.extend_from_slice(&key[..6]);
                f.extend_from_slice(&tag.to_le_bytes());
                f.extend_from_slice(&[at(7), at(12)]); // ACL permissions, FIRMWARE_VER_LEVEL
            } else {
                f.extend_from_slice(&[PUSH_CODE_LOGIN_FAIL, 0]);
                f.extend_from_slice(&key[..6]);
            }
        } else if data.len() > 4 && self.pending_status != 0 && self.pending_status == prefix4 {
            self.pending_status = 0;
            f.extend_from_slice(&[PUSH_CODE_STATUS_RESPONSE, 0]);
            f.extend_from_slice(&key[..6]);
            f.extend_from_slice(&data[4..]);
        } else if data.len() > 4 && tag == self.pending_telemetry {
            self.pending_telemetry = 0;
            f.extend_from_slice(&[PUSH_CODE_TELEMETRY_RESPONSE, 0]);
            f.extend_from_slice(&key[..6]);
            f.extend_from_slice(&data[4..]);
        } else if data.len() > 4 && tag == self.pending_req {
            self.pending_req = 0;
            f.extend_from_slice(&[PUSH_CODE_BINARY_RESPONSE, 0]);
            f.extend_from_slice(&tag.to_le_bytes());
            f.extend_from_slice(&data[4..]);
        } else {
            // unknown/unhandled responses (eg. pushed telemetry) go to the app as they are
            f.extend_from_slice(&[PUSH_CODE_BINARY_RESPONSE, 0]);
            f.extend_from_slice(data);
        }
        self.write_frame(&f);
    }

    /// `onContactPathRecv` (base behaviour): store the new out-path and handle the extra.
    fn on_contact_path_recv(&mut self, ctx: &MeshCtx, idx: usize, path_len: u8, path: &[u8], extra_type: u8, extra: &[u8]) {
        let c = &mut self.contacts[idx];
        let n = Packet::path_bytes_for(path_len).min(path.len());
        c.out_path[..n].copy_from_slice(&path[..n]);
        c.out_path_len = path_len;
        c.lastmod = ctx.current_time();
        let mut f = alloc::vec![PUSH_CODE_PATH_UPDATED];
        f.extend_from_slice(&c.id.pub_key);
        self.write_frame(&f);
        self.mark_contacts_dirty(ctx);

        if extra_type == PAYLOAD_TYPE_ACK && extra.len() >= 4 {
            self.process_ack(ctx, read_u32(extra, 0));
        } else if extra_type == PAYLOAD_TYPE_RESPONSE && !extra.is_empty() {
            self.on_contact_response(ctx, idx, extra);
        }
    }

    fn find_channel_idx(&self, ch: &GroupChannel) -> u8 {
        self.channels.iter().position(|c| c.secret == ch.secret).map_or(0xFF, |i| i as u8)
    }
}

impl MeshApp for Companion {
    fn allow_packet_forward(&mut self, _ctx: &MeshCtx, _pkt: &Packet) -> bool {
        self.prefs.is_repeat_enabled()
    }

    fn retransmit_delay(&mut self, ctx: &mut MeshCtx, pkt: &Packet) -> u32 {
        let t = (ctx.radio.est_airtime_ms(pkt.path_byte_len() + pkt.payload_len + 2) as f32 * self.prefs.tx_delay_factor)
            as u32;
        ctx.rng.next_int(0, 5 * t + 1)
    }

    fn direct_retransmit_delay(&mut self, ctx: &mut MeshCtx, pkt: &Packet) -> u32 {
        let t = (ctx.radio.est_airtime_ms(pkt.path_byte_len() + pkt.payload_len + 2) as f32
            * self.prefs.direct_tx_delay_factor) as u32;
        ctx.rng.next_int(0, 5 * t + 1)
    }

    fn extra_ack_transmit_count(&self) -> u8 {
        self.prefs.multi_acks
    }

    fn airtime_budget_factor(&self) -> f32 {
        self.prefs.airtime_factor
    }

    fn calc_rx_delay(&self, score: f32, air_time: u32) -> i32 {
        if self.prefs.rx_delay_base <= 0.0 {
            return 0;
        }
        ((libm::powf(self.prefs.rx_delay_base, 0.85 - score) - 1.0) * air_time as f32) as i32
    }

    fn log_rx_raw(&mut self, _ctx: &MeshCtx, raw: &[u8], snr: f32, rssi: f32) {
        if raw.len() + 3 <= MAX_FRAME_SIZE {
            let mut f = alloc::vec![PUSH_CODE_LOG_RX_DATA, (snr * 4.0) as i8 as u8, rssi as i8 as u8];
            f.extend_from_slice(raw);
            self.write_frame(&f);
        }
    }

    fn search_peers_by_hash(&mut self, _ctx: &mut MeshCtx, hash: u8) -> usize {
        self.matching_peers.clear();
        for (i, c) in self.contacts.iter().enumerate() {
            if !c.is_empty_slot() && c.id.is_hash_match_v1(hash) && self.matching_peers.len() < MAX_SEARCH_RESULTS {
                self.matching_peers.push(i);
            }
        }
        self.matching_peers.len()
    }

    fn peer_shared_secret(&mut self, ctx: &mut MeshCtx, peer_idx: usize) -> Option<Secret> {
        let i = *self.matching_peers.get(peer_idx)?;
        Some(self.secret_of(ctx, i))
    }

    fn on_peer_data_recv(
        &mut self, ctx: &mut MeshCtx, pkt: &Packet, payload_type: u8, sender_idx: usize, secret: &Secret, data: &[u8],
    ) {
        let Some(&idx) = self.matching_peers.get(sender_idx) else {
            return;
        };
        let len = data.len();
        if payload_type == PAYLOAD_TYPE_TXT_MSG && len > 5 {
            let sender_timestamp = read_u32(data, 0);
            let flags = data[4] >> 2;
            let text = c_str(&data[5..]);
            match flags {
                TXT_TYPE_PLAIN => {
                    self.contacts[idx].lastmod = ctx.current_time();
                    self.mark_connection_active(ctx, idx);
                    self.queue_message(idx, TXT_TYPE_PLAIN, pkt, sender_timestamp, &[], text);
                    self.note_message(self.contacts[idx].name_lossy(), text, ctx.current_time());

                    // truncated hash of timestamp + text + sender key proves receipt
                    let mut ack = [0u8; 6];
                    crypto::sha256(&mut ack[..4], &[&data[..5 + text.len()], &self.contacts[idx].id.pub_key]);
                    ack[4] = data.get(5 + text.len() + 1).copied().unwrap_or(0); // extended attempt byte
                    ctx.rng.fill(&mut ack[5..]);
                    if pkt.is_route_flood() {
                        self.send_path_return(ctx, idx, secret, pkt, PAYLOAD_TYPE_ACK, &ack, TXT_ACK_DELAY);
                    } else {
                        self.send_ack_to(ctx, idx, &ack);
                    }
                }
                TXT_TYPE_CLI_DATA => {
                    self.mark_connection_active(ctx, idx);
                    self.queue_message(idx, TXT_TYPE_CLI_DATA, pkt, sender_timestamp, &[], text);
                }
                TXT_TYPE_CLI_COMMAND => {
                    self.mark_connection_active(ctx, idx);
                    let mut reply = String::new();
                    if self.contacts[idx].is_remote_cli_allowed() {
                        let command = String::from_utf8_lossy(text).into_owned();
                        reply = self.handle_command(ctx, &command);
                    } else {
                        self.queue_message(idx, TXT_TYPE_CLI_COMMAND, pkt, sender_timestamp, &[], text);
                    }
                    if !reply.is_empty() {
                        let mut ts = ctx.current_time_unique();
                        if ts == sender_timestamp {
                            ts += 1; // the CLI view needs distinct timestamps
                        }
                        let mut temp = ts.to_le_bytes().to_vec();
                        temp.push(TXT_TYPE_CLI_DATA << 2);
                        let n = crate::util::valid_utf8_prefix_len(reply.as_bytes(), 160);
                        temp.extend_from_slice(&reply.as_bytes()[..n]);
                        let id = self.contacts[idx].id;
                        if let Some(p) = ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &id, secret, &temp) {
                            self.send_to_contact(ctx, idx, p, CLI_REPLY_DELAY_MILLIS);
                        }
                    }
                }
                TXT_TYPE_SIGNED_PLAIN if len >= 9 => {
                    let c = &mut self.contacts[idx];
                    c.sync_since = c.sync_since.max(sender_timestamp);
                    c.lastmod = ctx.current_time();
                    self.mark_connection_active(ctx, idx);
                    self.mark_contacts_dirty(ctx); // sync_since changed
                    let text = c_str(&data[9..]);
                    self.queue_message(idx, TXT_TYPE_SIGNED_PLAIN, pkt, sender_timestamp, &data[5..9], text);
                    self.note_message(self.contacts[idx].name_lossy(), text, ctx.current_time());

                    // hash of timestamp + flags + prefix + text + OUR key
                    let mut ack = [0u8; 4];
                    crypto::sha256(&mut ack, &[&data[..9 + text.len()], ctx.self_id.pub_key()]);
                    if pkt.is_route_flood() {
                        self.send_path_return(ctx, idx, secret, pkt, PAYLOAD_TYPE_ACK, &ack, TXT_ACK_DELAY);
                    } else {
                        self.send_ack_to(ctx, idx, &ack);
                    }
                }
                _ => {}
            }
        } else if payload_type == PAYLOAD_TYPE_REQ && len > 4 {
            let sender_timestamp = read_u32(data, 0);
            let reply = self.on_contact_request(idx, sender_timestamp, &data[4..]);
            if !reply.is_empty() {
                if pkt.is_route_flood() {
                    self.send_path_return(ctx, idx, secret, pkt, PAYLOAD_TYPE_RESPONSE, &reply, SERVER_RESPONSE_DELAY);
                } else {
                    let id = self.contacts[idx].id;
                    if let Some(p) = ctx.create_datagram(PAYLOAD_TYPE_RESPONSE, &id, secret, &reply) {
                        self.send_to_contact(ctx, idx, p, SERVER_RESPONSE_DELAY);
                    }
                }
            }
        } else if payload_type == PAYLOAD_TYPE_RESPONSE && len > 0 {
            self.on_contact_response(ctx, idx, data);
            if pkt.is_route_flood() && self.contacts[idx].has_out_path() {
                // we have a direct path but they still flood: resend the reciprocal path
                self.return_path_retry(ctx, idx, pkt.path_len, &pkt.path);
            }
        }
    }

    fn on_peer_path_recv(
        &mut self, ctx: &mut MeshCtx, pkt: &Packet, sender_idx: usize, _secret: &Secret, path_len: u8, path: &[u8],
        extra_type: u8, extra: &[u8],
    ) -> bool {
        let Some(&idx) = self.matching_peers.get(sender_idx) else {
            return false;
        };
        if extra_type == PAYLOAD_TYPE_RESPONSE && extra.len() > 4 && read_u32(extra, 0) == self.pending_discovery {
            self.pending_discovery = 0;
            if Packet::is_valid_path_len(pkt.path_len) && Packet::is_valid_path_len(path_len) {
                let mut f = alloc::vec![PUSH_CODE_PATH_DISCOVERY_RESPONSE, 0];
                f.extend_from_slice(&self.contacts[idx].id.pub_key[..6]);
                f.push(path_len);
                f.extend_from_slice(&path[..Packet::path_bytes_for(path_len).min(path.len())]);
                f.push(pkt.path_len);
                f.extend_from_slice(pkt.path());
                self.write_frame(&f);
            }
            return false; // no reciprocal path
        }
        self.on_contact_path_recv(ctx, idx, path_len, path, extra_type, extra);
        true
    }

    fn on_ack_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet, ack_crc: u32) -> bool {
        let Some(idx) = self.process_ack(ctx, ack_crc) else {
            return false;
        };
        if pkt.is_route_flood() && self.contacts[idx].has_out_path() {
            self.return_path_retry(ctx, idx, pkt.path_len, &pkt.path);
        }
        true
    }

    fn on_advert_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet, id: &Identity, timestamp: u32, app_data: &[u8]) {
        let Some(adv) = AdvertData::parse(app_data) else {
            return;
        };
        let name = advert_name_bytes(app_data);
        if name.first().is_none_or(|&c| c == 0) {
            return; // a name is required
        }
        let now = ctx.current_time();
        let mut from = self.contacts.iter().position(|c| c.id == *id);
        if let Some(i) = from
            && timestamp <= self.contacts[i].last_advert_timestamp
        {
            return; // possible replay
        }
        self.note_heard(HeardNode {
            name: String::from_utf8_lossy(c_str(name)).into_owned(),
            key_prefix: id.pub_key[..4].try_into().unwrap(),
            adv_type: adv.adv_type,
            hops: if pkt.is_route_flood() { pkt.path_hash_count() as u8 } else { 0 },
            snr: pkt.snr,
            heard: now,
        });

        // raw advert, as a flood packet without transport codes ("share" / "export")
        let mut raw = [0u8; MAX_TRANS_UNIT];
        let mut copy = pkt.clone();
        copy.set_route_type(ROUTE_TYPE_FLOOD);
        let raw_len = copy.write_to(&mut raw).unwrap_or(0);

        if let Some(i) = from
            && self.contacts[i].adv_type == ADV_TYPE_NONE
        {
            // it was a transient entry: free the slot, add normally
            self.contacts[i] = ContactInfo::new(Identity::new([0; PUB_KEY_SIZE]));
            from = None;
        }

        let idx = match from {
            Some(i) => i,
            None => {
                let ci = Self::populate_from_advert(id, &adv, name, timestamp, now);
                let max_hops = self.prefs.autoadd_max_hops as usize;
                if !self.should_auto_add_type(adv.adv_type) || (max_hops > 0 && pkt.path_hash_count() >= max_hops) {
                    self.on_discovered_contact(ctx, &ci, true, pkt.path_len, pkt.path());
                    return;
                }
                let Some(slot) = self.allocate_contact_slot(false) else {
                    self.on_discovered_contact(ctx, &ci, true, pkt.path_len, pkt.path());
                    self.write_frame(&[PUSH_CODE_CONTACTS_FULL]);
                    return;
                };
                self.contacts[slot] = ci;
                slot
            }
        };

        if raw_len > 0 {
            self.blobs.put(&id.pub_key, &raw[..raw_len]);
        }
        let c = &mut self.contacts[idx];
        c.set_name(name);
        c.adv_type = adv.adv_type;
        if let Some((lat, lon)) = adv.location {
            c.gps_lat = lat;
            c.gps_lon = lon;
        }
        c.last_advert_timestamp = timestamp;
        c.lastmod = now;
        let contact = c.clone();
        // upstream reports even a newly added contact as "not new"
        self.on_discovered_contact(ctx, &contact, false, pkt.path_len, pkt.path());
    }

    fn search_channels_by_hash(&mut self, _ctx: &mut MeshCtx, hash: u8) -> Vec<GroupChannel> {
        // empty slots have an all-zero key, which would accept null-key traffic
        self.channels.iter().filter(|c| !c.is_empty() && c.hash == hash).map(Channel::group).collect()
    }

    fn on_group_data_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet, payload_type: u8, channel: &GroupChannel, data: &[u8]) {
        let channel_idx = self.find_channel_idx(channel);
        let path_len = if pkt.is_route_flood() { pkt.path_len } else { 0xFF };
        if payload_type == PAYLOAD_TYPE_GRP_TXT {
            if data.len() < 5 || data[4] >> 2 != 0 {
                return; // short, or an unsupported text type
            }
            let mut f = Vec::with_capacity(MAX_FRAME_SIZE);
            if self.app_target_ver >= 3 {
                f.extend_from_slice(&[RESP_CODE_CHANNEL_MSG_RECV_V3, pkt.snr as u8, 0, 0]);
            } else {
                f.push(RESP_CODE_CHANNEL_MSG_RECV);
            }
            f.extend_from_slice(&[channel_idx, path_len, TXT_TYPE_PLAIN]);
            f.extend_from_slice(&data[..4]);
            let text = c_str(&data[5..]);
            let n = text.len().min(MAX_FRAME_SIZE - f.len());
            f.extend_from_slice(&text[..n]);
            self.add_to_offline_queue(&f);
            let from = self.channels.get(channel_idx as usize).map(|c| c.name.clone()).unwrap_or_default();
            self.note_message(from, text, ctx.current_time());
        } else if payload_type == PAYLOAD_TYPE_GRP_DATA {
            if data.len() < 3 {
                return;
            }
            let data_len = data[2] as usize;
            if data_len > data.len() - 3 || data_len > MAX_CHANNEL_DATA_LENGTH {
                return;
            }
            let mut f = alloc::vec![RESP_CODE_CHANNEL_DATA_RECV, pkt.snr as u8, 0, 0, channel_idx, path_len];
            f.extend_from_slice(&data[..3]); // data type, length
            f.extend_from_slice(&data[3..3 + data_len]);
            self.add_to_offline_queue(&f);
        }
    }

    fn on_control_data_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet) {
        if pkt.payload_len + 4 <= MAX_FRAME_SIZE + 1 {
            let s = &ctx.stats;
            let mut f =
                alloc::vec![PUSH_CODE_CONTROL_DATA, (s.last_snr * 4.0) as i8 as u8, s.last_rssi as i8 as u8, pkt.path_len];
            f.extend_from_slice(pkt.payload());
            self.write_frame(&f);
        }
    }

    fn on_raw_data_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet) {
        if pkt.payload_len + 4 <= MAX_FRAME_SIZE + 1 {
            let s = &ctx.stats;
            let mut f = alloc::vec![PUSH_CODE_RAW_DATA, (s.last_snr * 4.0) as i8 as u8, s.last_rssi as i8 as u8, 0xFF];
            f.extend_from_slice(pkt.payload());
            self.write_frame(&f);
        }
    }

    fn on_trace_recv(
        &mut self, _ctx: &mut MeshCtx, pkt: &Packet, tag: u32, auth_code: u32, flags: u8, path_snrs: &[u8],
        path_hashes: &[u8],
    ) {
        let path_sz = flags & 0x03;
        let path_len = path_hashes.len();
        let n_snrs = (path_len >> path_sz).min(path_snrs.len());
        if 12 + path_len + n_snrs + 1 > MAX_FRAME_SIZE + 1 {
            return;
        }
        let mut f = alloc::vec![PUSH_CODE_TRACE_DATA, 0, path_len as u8, flags];
        f.extend_from_slice(&tag.to_le_bytes());
        f.extend_from_slice(&auth_code.to_le_bytes());
        f.extend_from_slice(path_hashes);
        f.extend_from_slice(&path_snrs[..n_snrs]);
        f.push(pkt.snr as u8); // final SNR, to this node
        self.write_frame(&f);
    }

    fn on_loop(&mut self, ctx: &mut MeshCtx) {
        if self.dirty_contacts_expiry != 0 && ctx.now >= self.dirty_contacts_expiry {
            self.effects.save_contacts = true;
            self.dirty_contacts_expiry = 0;
        }
    }

    fn next_timer(&self) -> Option<u64> {
        (self.dirty_contacts_expiry != 0).then_some(self.dirty_contacts_expiry)
    }

    fn take_loopback(&mut self) -> Option<Box<Packet>> {
        self.loopback.take()
    }
}

/// The raw name bytes of an advert's `app_data` (up to the first NUL), empty if none.
fn advert_name_bytes(app_data: &[u8]) -> &[u8] {
    let Some(&flags) = app_data.first() else {
        return &[];
    };
    let mut i = 1;
    if flags & ADV_LATLON_MASK != 0 {
        i += 8;
    }
    if flags & ADV_FEAT1_MASK != 0 {
        i += 2;
    }
    if flags & ADV_FEAT2_MASK != 0 {
        i += 2;
    }
    if flags & ADV_NAME_MASK == 0 || i >= app_data.len() {
        return &[];
    }
    c_str(&app_data[i..])
}

const _: () = assert!(MAX_TEXT_LEN < MAX_PACKET_PAYLOAD - 4 - crate::CIPHER_MAC_SIZE - 1);
