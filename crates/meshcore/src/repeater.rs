//! The repeater role: a port of upstream `examples/simple_repeater` (`MyMesh`) plus `CommonCLI`.
//!
//! Forwards flood/direct traffic, keeps a neighbour table, lets admins log in (ANON_REQ with a
//! password) and run CLI commands or status requests over the mesh. The same node can run as a
//! room server instead ([`Repeater::new_room_server`], see [`room`]).
//!
//! Anything that needs hardware or flash is reported through [`Effects`], which the firmware
//! drains after each call.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::bridge::Bridge;
use crate::advert::{ADV_TYPE_REPEATER, ADV_TYPE_ROOM, AdvertData, is_valid_name};
use crate::companion::{CMD_DEVICE_QUERY, RESP_CODE_DEVICE_INFO};
use crate::crypto::{self, Secret};
use crate::fwshare::{self, FwStore};
use crate::identity::{Identity, LocalIdentity};
use crate::mesh::{MeshApp, MeshCtx};
use crate::ota::{OtaEndpoint, OtaOutcome};
use crate::release;
use crate::packet::*;
use crate::prefs::*;
use crate::radio::RadioParams;
use crate::region::{REGION_DENY_FLOOD, RegionMap, TransportKey, is_name_char};
use crate::rtc::DEFAULT_EPOCH;
use crate::util::{atof, atoi, atoi_signed, epoch_to_datetime, from_hex, ftoa, to_hex, truncate_str};
use crate::{FIRMWARE_VERSION, MAX_ADVERT_DATA_SIZE, MAX_PACKET_PAYLOAD, MAX_PATH_SIZE, PRV_KEY_SIZE, PUB_KEY_SIZE};

mod autoupdate;
pub mod room;
pub use room::{FIRMWARE_ROLE_ROOM, MAX_POST_TEXT_LEN, RoomClient};

pub const FIRMWARE_ROLE: &str = "repeater";
const FIRMWARE_VER_LEVEL: u8 = 2;

pub const PERM_ACL_ROLE_MASK: u8 = 3;
pub const PERM_ACL_GUEST: u8 = 0;
pub const PERM_ACL_READ_ONLY: u8 = 1;
pub const PERM_ACL_READ_WRITE: u8 = 2;
pub const PERM_ACL_ADMIN: u8 = 3;
pub const OUT_PATH_UNKNOWN: u8 = 0xFF;

pub const MAX_CLIENTS: usize = 32;
/// Leads the clients' last timestamps after the ACL records (see [`Repeater::acl_serialize`]).
const ACL_STAMPS_MAGIC: &[u8; 2] = b"TS";
const _: () = assert!(2 + 4 * MAX_CLIENTS < 136, "the timestamps must stay shorter than a record");
/// Wrong passwords taken in [`BAD_LOGINS_SECS`]; past them, no password is compared until the
/// window ends.
const BAD_LOGINS_MAX: u16 = 10;
const BAD_LOGINS_SECS: u32 = 600;
pub const MAX_NEIGHBOURS: usize = 50;

const REQ_TYPE_GET_STATUS: u8 = 0x01;
const REQ_TYPE_GET_TELEMETRY_DATA: u8 = 0x03;
const REQ_TYPE_GET_ACCESS_LIST: u8 = 0x05;
const REQ_TYPE_GET_NEIGHBOURS: u8 = 0x06;
const REQ_TYPE_GET_OWNER_INFO: u8 = 0x07;

// Firmware update over LoRa (this firmware's extension; admins only), see `crate::ota`.
pub use crate::ota::{
    REQ_TYPE_OTA_ABORT, REQ_TYPE_OTA_BEGIN, REQ_TYPE_OTA_COMMIT, REQ_TYPE_OTA_DATA, REQ_TYPE_OTA_INFO, REQ_TYPE_OTA_STATUS,
};

const RESP_SERVER_LOGIN_OK: u8 = 0;

const ANON_REQ_TYPE_REGIONS: u8 = 0x01;
const ANON_REQ_TYPE_OWNER: u8 = 0x02;
const ANON_REQ_TYPE_BASIC: u8 = 0x03;

const TXT_TYPE_PLAIN: u8 = 0;
const TXT_TYPE_CLI_DATA: u8 = 1;
const TXT_TYPE_CLI_COMMAND: u8 = 3;

const CTL_TYPE_NODE_DISCOVER_REQ: u8 = 0x80;
const CTL_TYPE_NODE_DISCOVER_RESP: u8 = 0x90;

const SERVER_RESPONSE_DELAY: u32 = 300;
const TXT_ACK_DELAY: u32 = 200;
const CLI_REPLY_DELAY_MILLIS: u32 = 600;
const LAZY_CONTACTS_WRITE_DELAY: u64 = 5000;
const MIN_LOCAL_ADVERT_INTERVAL: u32 = 60;
const BOOT_ADVERT_DELAY: u32 = 16_000;

const LPP_VOLTAGE: u8 = 116;
/// Packets buffered for the MQTT observer before new ones are dropped.
const UPLINK_QUEUE_MAX: usize = 24;
const LPP_TEMPERATURE: u8 = 103;
const TELEM_CHANNEL_SELF: u8 = 1;

/// A client known to this repeater (logged-in admin or guest).
#[derive(Clone, Debug)]
pub struct ClientInfo {
    pub id: Identity,
    pub permissions: u8,
    pub out_path_len: u8,
    pub out_path: [u8; MAX_PATH_SIZE],
    pub shared_secret: Secret,
    /// By THEIR clock (replay protection, not persisted).
    pub last_timestamp: u32,
    /// By OUR clock.
    pub last_activity: u32,
    /// Room server sync state.
    pub room: RoomClient,
}

impl ClientInfo {
    fn new(id: Identity, permissions: u8) -> Self {
        Self {
            id,
            permissions,
            out_path_len: OUT_PATH_UNKNOWN,
            out_path: [0; MAX_PATH_SIZE],
            shared_secret: [0; PUB_KEY_SIZE],
            last_timestamp: 0,
            last_activity: 0,
            room: RoomClient::default(),
        }
    }

    pub fn is_admin(&self) -> bool {
        self.permissions & PERM_ACL_ROLE_MASK == PERM_ACL_ADMIN
    }

    fn out_path(&self) -> Option<(u8, &[u8])> {
        (self.out_path_len != OUT_PATH_UNKNOWN).then_some((self.out_path_len, &self.out_path[..]))
    }
}

#[derive(Clone, Debug)]
pub struct Neighbour {
    pub id: Identity,
    pub advert_timestamp: u32,
    pub heard_timestamp: u32,
    /// SNR x 4.
    pub snr: i8,
}

/// Fixed-window rate limiter (`RateLimiter.h`).
#[derive(Clone, Debug)]
struct RateLimiter {
    start: u32,
    secs: u32,
    maximum: u16,
    count: u16,
}

impl RateLimiter {
    const fn new(maximum: u16, secs: u32) -> Self {
        Self { start: 0, secs, maximum, count: 0 }
    }

    /// The window's allowance is used up (without counting anything).
    fn exhausted(&self, now: u32) -> bool {
        now < self.start.wrapping_add(self.secs) && self.count >= self.maximum
    }

    fn allow(&mut self, now: u32) -> bool {
        if now < self.start.wrapping_add(self.secs) {
            self.count = self.count.saturating_add(1);
            self.count <= self.maximum
        } else {
            self.start = now;
            self.count = 1;
            true
        }
    }
}

/// Hardware-facing requests produced by the repeater, drained by the firmware.
#[derive(Default, Debug)]
pub struct Effects {
    pub save_prefs: bool,
    pub save_acl: bool,
    pub save_identity: Option<[u8; PRV_KEY_SIZE]>,
    pub reboot: bool,
    pub power_off: bool,
    pub erase: bool,
    /// Re-apply modem parameters now (temporary radio params or their revert).
    pub apply_radio: Option<RadioParams>,
    pub tx_power: Option<i8>,
    pub rx_boosted_gain: Option<bool>,
    pub fem_lna: Option<bool>,
    /// Bring up the Wi-Fi update page (`start ota`): the one-time code uploads must carry, and
    /// the passphrase of the access point (one per boot, as the point comes up only once).
    pub start_ota: Option<(u32, String)>,
    /// Open the BLE update window (`start ota ble`) with this pairing PIN.
    pub start_ble_ota: Option<u32>,
    /// Persist the region map.
    pub save_regions: bool,
    /// Wi-Fi or MQTT settings changed: reconnect.
    pub network_changed: bool,
    /// The bridge was switched on or off, or moved to another channel: restart its medium.
    pub bridge_changed: bool,
    /// Publish a status message now (`send mqtt.status`).
    pub send_mqtt_status: bool,
    /// Lines for the serial console (packet log, ACL dumps).
    pub console: Vec<String>,
}

/// A packet for the MQTT observer: raw frame plus reception details (`tx` = we sent it).
#[derive(Clone, Debug)]
pub struct UplinkPacket {
    pub raw: Vec<u8>,
    pub tx: bool,
    pub snr: f32,
    pub rssi: f32,
    pub score_duration: Option<(i32, u32)>,
}

/// MQTT broker names and their bits (`MQTTUplink.h`); at most 3 enabled at once.
/// How long `start ota ble` keeps BLE up.
pub const BLE_OTA_WINDOW_MINS: u32 = 15;
/// Frames waiting for the bridge's medium; more are dropped (the firmware drains it every pass).
const BRIDGE_QUEUE_MAX: usize = 16;

pub const MQTT_BROKERS: [(&str, u8); 4] = [("meshcoretel", 0x01), ("letsmesh-eu", 0x02), ("letsmesh-us", 0x04), ("ommesh", 0x08)];

/// Live board values the firmware keeps up to date.
#[derive(Clone, Debug, Default)]
pub struct BoardInfo {
    pub manufacturer: String,
    pub batt_mv: u16,
    pub mcu_temp_c: Option<f32>,
    pub fem_lna_controllable: bool,
    pub build_date: String,
    /// SSID of the Wi-Fi update access point, if the board supports `start ota`.
    pub wifi_ota_ssid: Option<String>,
    /// Advertised name of the BLE update window, if the board supports `start ota ble`.
    pub ble_ota_name: Option<String>,
    /// LAN address while the Wi-Fi station is connected (`start ota` serves there instead of an AP).
    pub wifi_ip: Option<String>,
    /// The board can light-sleep between packets (`powersaving on`).
    pub powersaving_supported: bool,
    /// The bridge the board can run (`get bridge.type`): `espnow`; none = no bridge.
    pub bridge: Option<&'static str>,
    /// How much the board slept, for `powersaving stats`, kept by the firmware.
    pub power_status: String,
    /// Network status lines for `get wifi.status` / `get mqtt.status`, kept by the firmware.
    pub wifi_status: String,
    pub mqtt_status: String,
    /// What the update store holds, for `get fw.status`, kept by the firmware.
    pub fw_status: String,
}

pub struct Repeater {
    pub prefs: NodePrefs,
    pub board: BoardInfo,
    pub effects: Effects,
    acl: Vec<ClientInfo>,
    neighbours: Vec<Neighbour>,
    matching_peers: Vec<usize>,
    next_local_advert: u64,
    next_flood_advert: u64,
    dirty_contacts_expiry: u64,
    set_radio_at: u64,
    revert_radio_at: u64,
    pending_radio: RadioParams,
    discover_limiter: RateLimiter,
    anon_limiter: RateLimiter,
    /// FW requests: a companion fetching an update asks for a piece about every second.
    fw_limiter: RateLimiter,
    pending_discover_tag: u32,
    pending_discover_until: u64,
    /// Region of the packet being processed: `None` = unknown scope, `Some(0)` = wildcard.
    recv_region: Option<u16>,
    pub regions: RegionMap,
    /// `region load` in progress: (new map, parent stack by indent level).
    region_load: Option<(RegionMap, [Option<u16>; 8])>,
    logging: bool,
    /// Reply route picked by an anonymous request handler: (path_len, path).
    reply_path: Option<(u8, [u8; MAX_PATH_SIZE])>,
    /// Firmware updates over LoRa; set its `target` if the platform supports updates.
    pub ota: OtaEndpoint,
    /// The updates this node hands on to its neighbours (`set fw.serve on`), and installs itself
    /// (`set fw.auto on`), if the platform can fetch them.
    pub fw_store: Option<Box<dyn FwStore>>,
    autoupdate: autoupdate::AutoUpdate,
    /// Packets heard/sent, waiting for the MQTT observer (drained by the firmware).
    pub uplink: Vec<UplinkPacket>,
    /// Collect packets into `uplink` (an observer is connected).
    pub uplink_enabled: bool,
    /// Room server state: `Some` when this node is a room server.
    room: Option<Box<room::Room>>,
    bridge: Bridge,
    /// Frames for the bridge's medium, taken by the firmware.
    bridge_out: Vec<Vec<u8>>,
    /// Passphrase of the Wi-Fi update access point, made at the first `start ota`.
    wifi_ota_key: String,
    /// Wrong passwords, whoever sent them (see [`Self::check_password`]).
    bad_logins: RateLimiter,
    /// An admin's last timestamp moved since the ACL was last saved.
    stamps_dirty: bool,
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).unwrap_or(0)
}

/// Bytes up to the first NUL (decrypted payloads are zero-padded).
fn c_str(b: &[u8]) -> &[u8] {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    &b[..n]
}

/// Hex public-key prefix (at most a full key; odd trailing digit ignored).
fn parse_hex_prefix(hex: &str) -> Option<Vec<u8>> {
    let hex = hex.trim();
    let n = (hex.len().min(PUB_KEY_SIZE * 2)) / 2;
    let mut key = alloc::vec![0u8; n];
    from_hex(&mut key, hex.get(..n * 2)?).then_some(key)
}

fn fmt_time(t: u32) -> String {
    let (y, mo, d, h, mi, _) = epoch_to_datetime(t);
    format!("{h:02}:{mi:02} - {d}/{mo}/{y} UTC")
}

impl Repeater {
    pub fn new(prefs: NodePrefs, board: BoardInfo) -> Self {
        let pending_radio = prefs.radio;
        let bridge = Bridge::new(&prefs.bridge_secret);
        let ota = OtaEndpoint::new(prefs.ota_roots.clone(), prefs.ota_owners.clone(), release::role::REPEATER);
        Self {
            prefs,
            board,
            effects: Effects::default(),
            acl: Vec::new(),
            neighbours: Vec::new(),
            matching_peers: Vec::new(),
            next_local_advert: 0,
            next_flood_advert: 0,
            dirty_contacts_expiry: 0,
            set_radio_at: 0,
            revert_radio_at: 0,
            pending_radio,
            discover_limiter: RateLimiter::new(4, 120),
            anon_limiter: RateLimiter::new(4, 180),
            fw_limiter: RateLimiter::new(90, 60),
            pending_discover_tag: 0,
            pending_discover_until: 0,
            recv_region: None,
            regions: RegionMap::new(),
            region_load: None,
            logging: false,
            reply_path: None,
            ota,
            fw_store: None,
            autoupdate: autoupdate::AutoUpdate::new(),
            uplink: Vec::new(),
            uplink_enabled: false,
            room: None,
            bridge,
            bridge_out: Vec::new(),
            wifi_ota_key: String::new(),
            bad_logins: RateLimiter::new(BAD_LOGINS_MAX, BAD_LOGINS_SECS),
            stamps_dirty: false,
        }
    }

    /// The bridge is on and the board has one.
    pub fn bridge_active(&self) -> bool {
        self.prefs.bridge_enabled && self.board.bridge.is_some()
    }

    /// Frames to send over the bridge.
    pub fn take_bridge_out(&mut self) -> Vec<Vec<u8>> {
        core::mem::take(&mut self.bridge_out)
    }

    /// A frame from the bridge: the packet to process (see [`Mesh::queue_bridged`]), after
    /// `bridge.delay` milliseconds, if the frame is ours and the packet new.
    ///
    /// [`Mesh::queue_bridged`]: crate::mesh::Mesh::queue_bridged
    pub fn bridge_receive(&mut self, frame: &[u8]) -> Option<(Packet, u64)> {
        if !self.bridge_active() {
            return None;
        }
        self.bridge.decode(frame).map(|p| (p, self.prefs.bridge_delay as u64))
    }

    fn bridge_send(&mut self, pkt: &Packet) {
        if self.bridge_out.len() < BRIDGE_QUEUE_MAX
            && let Some(frame) = self.bridge.encode(pkt)
        {
            self.bridge_out.push(frame);
        }
    }

    /// Arm timers and queue the boot advert (call once after `Mesh::begin`).
    pub fn start(&mut self, ctx: &mut MeshCtx) {
        self.update_advert_timer(ctx);
        self.update_flood_advert_timer(ctx);
        self.send_self_advertisement(ctx, BOOT_ADVERT_DELAY, false);
    }

    pub fn take_effects(&mut self) -> Effects {
        // before a restart the admins' last timestamps go to flash, or the packet that brought it
        // about (`reboot`, an update) could be played again after it, and again
        let fx = &self.effects;
        if (fx.reboot || fx.network_changed || fx.bridge_changed || fx.power_off)
            && (self.stamps_dirty || self.dirty_contacts_expiry != 0)
        {
            self.dirty_contacts_expiry = 0;
            self.effects.save_acl = true;
        }
        if self.effects.save_acl {
            self.stamps_dirty = false;
        }
        core::mem::take(&mut self.effects)
    }

    pub fn is_hidden_node(&self) -> bool {
        self.prefs.advert_interval == 0 && self.prefs.flood_advert_interval == 0
    }

    pub fn neighbours(&self) -> &[Neighbour] {
        &self.neighbours
    }

    pub fn clients(&self) -> &[ClientInfo] {
        &self.acl
    }

    /// The app's 12-byte "build date" field: the release number, or a local build's date.
    fn app_build_field(build_date: &str) -> String {
        match release::RELEASE_COUNTER {
            0 => build_date.into(),
            n => format!("build #{n}"),
        }
    }

    fn console(&mut self, line: String) {
        self.effects.console.push(line);
    }

    fn ota_outcome(&mut self, outcome: OtaOutcome) {
        match outcome {
            OtaOutcome::Committed => {
                self.effects.reboot = true; // after the reply has gone out
                self.console(String::from("OTA: new firmware verified, rebooting into it"));
            }
            OtaOutcome::CommitFailed(line) => self.console(line),
            OtaOutcome::None => {}
        }
    }

    /// A frame from the local update link (BLE while `start ota ble` is open, already
    /// authenticated by pairing): firmware update requests, answered under the same command
    /// byte as on a companion, and the companion device query, so the same update tool serves
    /// both roles. Anything else is ignored.
    pub fn handle_link_frame(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        let code = *frame.first()?;
        if code == CMD_DEVICE_QUERY {
            let mut f = alloc::vec![RESP_CODE_DEVICE_INFO, crate::companion::FIRMWARE_VER_CODE, 0, 0, 0, 0, 0, 0];
            let build = Self::app_build_field(&self.board.build_date);
            for (s, width) in [(build.as_str(), 12), (self.board.manufacturer.as_str(), 40), (FIRMWARE_VERSION, 20)] {
                let s = &s.as_bytes()[..s.len().min(width - 1)];
                f.extend_from_slice(s);
                f.resize(f.len() + width - s.len(), 0);
            }
            return Some(f);
        }
        if !OtaEndpoint::is_request(code) {
            return None;
        }
        let (reply, outcome) = self.ota.handle(frame, FIRMWARE_VERSION);
        self.ota_outcome(outcome);
        reply.map(|body| [&[code][..], &body].concat())
    }

    // ------------------------------------------------------------ persistence

    fn save_prefs(&mut self) {
        if (self.prefs.advert_interval as u32) * 2 < MIN_LOCAL_ADVERT_INTERVAL {
            self.prefs.advert_interval = 0; // device has been manually configured now
        }
        self.effects.save_prefs = true;
    }

    /// ACL in upstream's `/s_contacts` record format (136 bytes per client). A room server keeps
    /// only its admins (upstream `saveFilter`). Then `TS` and each client's last timestamp (u32),
    /// so that their packets can't be played again after a restart (upstream keeps them in RAM
    /// only); shorter than a record, older firmware reads past it.
    pub fn acl_serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut stamps = Vec::from(&ACL_STAMPS_MAGIC[..]);
        let room = self.room.is_some();
        for c in self.acl.iter().filter(|c| c.permissions != 0 && (!room || c.is_admin())) {
            out.extend_from_slice(&c.id.pub_key);
            out.push(c.permissions);
            out.extend_from_slice(&c.room.sync_since.to_le_bytes());
            out.extend_from_slice(&[0; 2]); // unused
            out.push(c.out_path_len);
            out.extend_from_slice(&c.out_path);
            out.extend_from_slice(&c.shared_secret);
            stamps.extend_from_slice(&c.last_timestamp.to_le_bytes());
        }
        if !out.is_empty() {
            out.extend_from_slice(&stamps);
        }
        out
    }

    pub fn acl_load(&mut self, data: &[u8], self_id: &LocalIdentity) {
        self.acl.clear();
        let (records, rest) = data.as_chunks::<136>();
        let stamps = rest
            .strip_prefix(&ACL_STAMPS_MAGIC[..])
            .filter(|s| s.len() == 4 * records.len())
            .map(|s| s.as_chunks::<4>().0);
        for (k, rec) in records.iter().take(MAX_CLIENTS).enumerate() {
            let id = Identity::from_slice(&rec[..32]).unwrap();
            let mut c = ClientInfo::new(id, rec[32]);
            c.room.sync_since = read_u32(rec, 33);
            c.out_path_len = rec[39];
            c.out_path.copy_from_slice(&rec[40..104]);
            // recalculated in case our private key changed
            c.shared_secret = self_id.calc_shared_secret(&id.pub_key).unwrap_or([0; PUB_KEY_SIZE]);
            c.last_timestamp = stamps.map_or(0, |s| u32::from_le_bytes(s[k]));
            self.acl.push(c);
        }
    }

    /// An admin's packet moved its last timestamp: it reaches flash with the ACL before any
    /// restart, and a command's also a few seconds later (a request's may wait: an app asks for
    /// the status every few seconds, more writes than the flash should take).
    fn note_admin_timestamp(&mut self, ctx: &MeshCtx, i: usize, is_command: bool) {
        if self.acl[i].is_admin() {
            self.stamps_dirty = true;
            if is_command {
                self.dirty_contacts_expiry = ctx.now + LAZY_CONTACTS_WRITE_DELAY;
            }
        }
    }

    /// Which password this is: `Some(true)` the admin's, `Some(false)` the guest's (a room's
    /// password), `None` neither. Wrong ones are counted, whoever sends them (keys cost nothing):
    /// past [`BAD_LOGINS_MAX`] in [`BAD_LOGINS_SECS`] none is compared until the window ends, so
    /// a search for the password over LoRa takes ages. An empty one is no guess and isn't counted.
    fn check_password(&mut self, ctx: &MeshCtx, password: &[u8]) -> Option<bool> {
        let now = (ctx.now / 1000) as u32;
        if self.bad_logins.exhausted(now) {
            return None;
        }
        if password == self.prefs.password.as_bytes() {
            Some(true)
        } else if password == self.prefs.guest_password.as_bytes() {
            Some(false)
        } else {
            if !password.is_empty() {
                self.bad_logins.allow(now);
            }
            None
        }
    }

    fn get_client(&self, pubkey_prefix: &[u8]) -> Option<usize> {
        self.acl.iter().position(|c| c.id.pub_key.starts_with(pubkey_prefix))
    }

    fn put_client(&mut self, id: Identity, init_perms: u8) -> usize {
        if let Some(i) = self.acl.iter().position(|c| c.id == id) {
            return i;
        }
        let c = ClientInfo::new(id, init_perms);
        if self.acl.len() < MAX_CLIENTS {
            self.acl.push(c);
            return self.acl.len() - 1;
        }
        // evict the least active non-admin
        let idx = self
            .acl
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_admin())
            .min_by_key(|(_, c)| c.last_activity)
            .map(|(i, _)| i)
            .unwrap_or(MAX_CLIENTS - 1);
        self.acl[idx] = c;
        idx
    }

    fn apply_permissions(&mut self, self_id: &LocalIdentity, pubkey: &[u8], perms: u8) -> bool {
        if perms & PERM_ACL_ROLE_MASK == PERM_ACL_GUEST {
            // guests are not persisted: delete
            let Some(i) = self.get_client(pubkey) else {
                return false;
            };
            self.acl.remove(i);
        } else {
            let Some(id) = Identity::from_slice(pubkey) else {
                return false; // need the complete key to add/modify
            };
            // a mistyped key may be no curve point at all: nobody could log in with it, and
            // with no secret the entry would be good for nothing (upstream adds it anyway)
            let Some(secret) = self_id.calc_shared_secret(&id.pub_key) else {
                return false;
            };
            let i = self.put_client(id, 0);
            self.acl[i].permissions = perms;
            self.acl[i].shared_secret = secret;
        }
        true
    }

    // ------------------------------------------------------------- adverts

    fn build_advert_data(&self, out: &mut [u8; MAX_ADVERT_DATA_SIZE]) -> usize {
        let adv_type = if self.room.is_some() { ADV_TYPE_ROOM } else { ADV_TYPE_REPEATER };
        let adv = AdvertData::new(adv_type, &self.prefs.node_name);
        let adv = match self.prefs.advert_loc_policy {
            ADVERT_LOC_NONE => adv,
            _ => adv.with_location(self.prefs.node_lat, self.prefs.node_lon),
        };
        adv.encode(out)
    }

    fn create_self_advert(&self, ctx: &mut MeshCtx) -> Option<Box<Packet>> {
        let mut app_data = [0u8; MAX_ADVERT_DATA_SIZE];
        let n = self.build_advert_data(&mut app_data);
        ctx.create_advert(&app_data[..n])
    }

    /// Key of the default region (`region default`), used for this node's own floods.
    fn default_scope(&self) -> Option<TransportKey> {
        self.regions.default_region().and_then(|r| self.regions.first_key(r))
    }

    fn send_flood_scoped(&self, ctx: &mut MeshCtx, pkt: Box<Packet>, scope: Option<TransportKey>, delay_ms: u32, phs: usize) {
        match scope {
            Some(key) => {
                let code = key.transport_code(&pkt);
                ctx.send_flood_scoped(pkt, [code, 0], delay_ms, phs);
            }
            None => ctx.send_flood(pkt, delay_ms, phs),
        }
    }

    pub fn send_self_advertisement(&mut self, ctx: &mut MeshCtx, delay_ms: u32, flood: bool) {
        if let Some(pkt) = self.create_self_advert(ctx) {
            if flood {
                let scope = self.default_scope();
                self.send_flood_scoped(ctx, pkt, scope, delay_ms, self.prefs.path_hash_mode as usize + 1);
            } else {
                ctx.send_zero_hop(pkt, delay_ms);
            }
        }
    }

    fn update_advert_timer(&mut self, ctx: &MeshCtx) {
        self.next_local_advert = if self.prefs.advert_interval > 0 {
            ctx.now + self.prefs.advert_interval as u64 * 2 * 60 * 1000
        } else {
            0
        };
    }

    fn update_flood_advert_timer(&mut self, ctx: &MeshCtx) {
        self.next_flood_advert = if self.prefs.flood_advert_interval > 0 {
            ctx.now + self.prefs.flood_advert_interval as u64 * 60 * 60 * 1000
        } else {
            0
        };
    }

    /// Flood a reply with the scope the request came in on (`chooseReplyScope`).
    fn send_flood_reply(&mut self, ctx: &mut MeshCtx, pkt: Box<Packet>, delay_ms: u32, path_hash_size: usize) {
        let request_region = self.recv_region.and_then(|id| self.regions.find_by_id(id));
        let is_wildcard = request_region.is_some_and(|r| r.is_wildcard());
        let request_key = request_region.filter(|r| !r.is_wildcard()).and_then(|r| self.regions.first_key(r));
        let scope = match (request_key, is_wildcard) {
            (Some(k), _) => Some(k),         // same scope as the request
            (None, true) => None,            // requester chose un-scoped: mirror it
            (None, false) => self.default_scope(), // DIRECT request, or unresolved region
        };
        self.send_flood_scoped(ctx, pkt, scope, delay_ms, path_hash_size);
    }

    // ----------------------------------------------------------- neighbours

    fn put_neighbour(&mut self, ctx: &MeshCtx, id: &Identity, timestamp: u32, snr: f32) {
        let heard = ctx.current_time();
        let n = Neighbour { id: *id, advert_timestamp: timestamp, heard_timestamp: heard, snr: (snr * 4.0) as i8 };
        if let Some(e) = self.neighbours.iter_mut().find(|e| e.id == *id) {
            *e = n;
        } else if self.neighbours.len() < MAX_NEIGHBOURS {
            self.neighbours.push(n);
        } else if let Some(oldest) = self.neighbours.iter_mut().min_by_key(|e| e.heard_timestamp) {
            *oldest = n;
        }
    }

    fn format_neighbours(&self, ctx: &MeshCtx) -> String {
        let mut sorted: Vec<&Neighbour> = self.neighbours.iter().filter(|n| n.heard_timestamp > 0).collect();
        sorted.sort_by_key(|a| core::cmp::Reverse(a.heard_timestamp));
        let now = ctx.current_time();
        let mut out = String::new();
        for (i, n) in sorted.iter().enumerate() {
            if out.len() >= 134 {
                break;
            }
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&format!("{}:{}:{}", to_hex(&n.id.pub_key[..4]), now.wrapping_sub(n.heard_timestamp), n.snr));
        }
        if out.is_empty() {
            out.push_str("-none-");
        }
        out
    }

    fn send_node_discover_req(&mut self, ctx: &mut MeshCtx) {
        let mut data = [0u8; 10];
        data[0] = CTL_TYPE_NODE_DISCOVER_REQ; // prefix_only = 0
        data[1] = 1 << ADV_TYPE_REPEATER;
        ctx.rng.fill(&mut data[2..6]);
        self.pending_discover_tag = read_u32(&data, 2);
        self.pending_discover_until = ctx.now + 60_000;
        if let Some(pkt) = ctx.create_control_data(&data) {
            ctx.send_zero_hop(pkt, 0);
        }
    }

    // --------------------------------------------------------------- login

    fn handle_login_req(
        &mut self, ctx: &mut MeshCtx, sender: &Identity, secret: &Secret, sender_timestamp: u32, password: &[u8],
        is_flood: bool, reply: &mut Vec<u8>,
    ) -> bool {
        let mut client = if password.is_empty() { self.get_client(&sender.pub_key) } else { None };
        if client.is_none() {
            let perms = match self.check_password(ctx, password) {
                Some(true) => PERM_ACL_ADMIN,
                Some(false) => PERM_ACL_GUEST,
                None => return false,
            };
            let i = self.put_client(*sender, 0);
            let now = ctx.current_time();
            let c = &mut self.acl[i];
            if sender_timestamp <= c.last_timestamp {
                return false; // replay attack (or table full)
            }
            c.last_timestamp = sender_timestamp;
            c.last_activity = now;
            c.permissions = (c.permissions & !PERM_ACL_ROLE_MASK) | perms;
            c.shared_secret = *secret;
            if perms != PERM_ACL_GUEST {
                self.dirty_contacts_expiry = ctx.now + LAZY_CONTACTS_WRITE_DELAY;
            }
            client = Some(i);
        }
        let i = client.unwrap();
        if is_flood {
            self.acl[i].out_path_len = OUT_PATH_UNKNOWN; // rediscover out_path
        }
        let now = ctx.current_time_unique();
        reply.extend_from_slice(&now.to_le_bytes());
        reply.push(RESP_SERVER_LOGIN_OK);
        reply.push(0); // legacy keep-alive interval
        reply.push(if self.acl[i].is_admin() { 1 } else { 0 });
        reply.push(self.acl[i].permissions);
        let mut blob = [0u8; 4];
        ctx.rng.fill(&mut blob); // packet-hash uniqueness
        reply.extend_from_slice(&blob);
        reply.push(FIRMWARE_VER_LEVEL);
        true
    }

    /// Common prologue of the anonymous info requests: `{reply-path-len}{reply-path}`.
    fn anon_prologue(&mut self, ctx: &MeshCtx, sender_timestamp: u32, data: &[u8], reply: &mut Vec<u8>) -> bool {
        if !self.anon_limiter.allow(ctx.current_time()) {
            return false;
        }
        let Some(&path_len) = data.first() else {
            return false;
        };
        if !Packet::is_valid_path_len(path_len) {
            return false;
        }
        let mut path = [0u8; MAX_PATH_SIZE];
        let n = Packet::path_bytes_for(path_len).min(data.len() - 1);
        path[..n].copy_from_slice(&data[1..1 + n]);
        self.reply_path = Some((path_len, path));
        reply.extend_from_slice(&sender_timestamp.to_le_bytes());
        reply.extend_from_slice(&ctx.current_time().to_le_bytes());
        true
    }

    /// A neighbour's FW request (see [`crate::fwshare`]): `{reply-path-len}{reply-path}` and the
    /// request. Only while this node hands updates on, and at about one a second.
    fn fw_request(&mut self, ctx: &MeshCtx, sender_timestamp: u32, kind: u8, data: &[u8], reply: &mut Vec<u8>) -> bool {
        if !self.prefs.fw_serve || !self.fw_limiter.allow(ctx.current_time()) {
            return false;
        }
        let Some(store) = self.fw_store.as_deref_mut() else {
            return false;
        };
        let Some(&path_len) = data.first() else {
            return false;
        };
        if !Packet::is_valid_path_len(path_len) {
            return false;
        }
        let mut path = [0u8; MAX_PATH_SIZE];
        let n = Packet::path_bytes_for(path_len).min(data.len() - 1);
        path[..n].copy_from_slice(&data[1..1 + n]);
        reply.extend_from_slice(&sender_timestamp.to_le_bytes());
        let answered = fwshare::answer(store, kind, &data[1 + n..], reply);
        if answered {
            self.reply_path = Some((path_len, path));
        }
        answered
    }

    // ------------------------------------------------------------- requests

    fn handle_request(&mut self, ctx: &mut MeshCtx, client_idx: usize, sender_timestamp: u32, payload: &[u8]) -> Vec<u8> {
        let mut r = Vec::with_capacity(MAX_PACKET_PAYLOAD);
        r.extend_from_slice(&sender_timestamp.to_le_bytes()); // reflected back like a tag
        let at = |i: usize| payload.get(i).copied().unwrap_or(0);
        if OtaEndpoint::is_request(at(0)) {
            if !self.acl[client_idx].is_admin() {
                return Vec::new();
            }
            let (reply, outcome) = self.ota.handle_remote(payload, FIRMWARE_VERSION);
            self.ota_outcome(outcome);
            return match reply {
                Some(body) => {
                    r.extend_from_slice(&body);
                    r
                }
                None => Vec::new(),
            };
        }
        match at(0) {
            REQ_TYPE_GET_STATUS => {
                let s = &ctx.stats;
                r.extend_from_slice(&self.board.batt_mv.to_le_bytes());
                r.extend_from_slice(&(ctx.outbound_len() as u16).to_le_bytes());
                r.extend_from_slice(&s.noise_floor.to_le_bytes());
                r.extend_from_slice(&(s.last_rssi as i16).to_le_bytes());
                r.extend_from_slice(&s.n_packets_recv.to_le_bytes());
                r.extend_from_slice(&s.n_packets_sent.to_le_bytes());
                r.extend_from_slice(&((s.total_air_time_ms / 1000) as u32).to_le_bytes());
                r.extend_from_slice(&((ctx.now / 1000) as u32).to_le_bytes());
                r.extend_from_slice(&s.n_sent_flood.to_le_bytes());
                r.extend_from_slice(&s.n_sent_direct.to_le_bytes());
                r.extend_from_slice(&s.n_recv_flood.to_le_bytes());
                r.extend_from_slice(&s.n_recv_direct.to_le_bytes());
                r.extend_from_slice(&s.err_flags.to_le_bytes());
                r.extend_from_slice(&((s.last_snr * 4.0) as i16).to_le_bytes());
                r.extend_from_slice(&(ctx.tables.direct_dups as u16).to_le_bytes());
                r.extend_from_slice(&(ctx.tables.flood_dups as u16).to_le_bytes());
                if let Some((posted, pushes)) = self.room_stats() {
                    r.extend_from_slice(&posted.to_le_bytes());
                    r.extend_from_slice(&pushes.to_le_bytes());
                } else {
                    r.extend_from_slice(&((s.rx_air_time_ms / 1000) as u32).to_le_bytes());
                    r.extend_from_slice(&s.n_recv_errors.to_le_bytes());
                }
            }
            REQ_TYPE_GET_TELEMETRY_DATA => {
                // CayenneLPP: channel, type, big-endian value
                let volts = (self.board.batt_mv as u32 / 10) as u16; // 0.01 V
                r.extend_from_slice(&[TELEM_CHANNEL_SELF, LPP_VOLTAGE]);
                r.extend_from_slice(&volts.to_be_bytes());
                if let Some(t) = self.board.mcu_temp_c {
                    r.extend_from_slice(&[TELEM_CHANNEL_SELF, LPP_TEMPERATURE]);
                    r.extend_from_slice(&((t * 10.0) as i16).to_be_bytes());
                }
            }
            REQ_TYPE_GET_ACCESS_LIST if self.acl[client_idx].is_admin() => {
                if at(1) == 0 && at(2) == 0 {
                    // a room server lists its admins only
                    let room = self.room.is_some();
                    for c in self.acl.iter().filter(|c| c.permissions != 0 && (!room || c.is_admin())) {
                        if r.len() + 7 > MAX_PACKET_PAYLOAD - 4 {
                            break;
                        }
                        r.extend_from_slice(&c.id.pub_key[..6]);
                        r.push(c.permissions);
                    }
                } else {
                    r.clear();
                }
            }
            REQ_TYPE_GET_NEIGHBOURS | REQ_TYPE_GET_OWNER_INFO if self.room.is_some() => r.clear(),
            REQ_TYPE_GET_NEIGHBOURS if at(1) == 0 => {
                let count = at(2) as usize;
                let offset = u16::from_le_bytes([at(3), at(4)]) as usize;
                let order_by = at(5);
                let prefix_len = (at(6) as usize).min(PUB_KEY_SIZE);
                let mut sorted: Vec<&Neighbour> = self.neighbours.iter().filter(|n| n.heard_timestamp > 0).collect();
                match order_by {
                    0 => sorted.sort_by_key(|a| core::cmp::Reverse(a.heard_timestamp)),
                    1 => sorted.sort_by_key(|a| a.heard_timestamp),
                    2 => sorted.sort_by_key(|a| core::cmp::Reverse(a.snr)),
                    3 => sorted.sort_by_key(|a| a.snr),
                    _ => {}
                }
                let now = ctx.current_time();
                let mut results = Vec::with_capacity(130);
                let mut results_count: u16 = 0;
                for n in sorted.iter().skip(offset).take(count) {
                    if results.len() + prefix_len + 5 > 130 {
                        break;
                    }
                    results.extend_from_slice(&n.id.pub_key[..prefix_len]);
                    results.extend_from_slice(&now.wrapping_sub(n.heard_timestamp).to_le_bytes());
                    results.push(n.snr as u8);
                    results_count += 1;
                }
                r.extend_from_slice(&(sorted.len() as i16).to_le_bytes());
                r.extend_from_slice(&results_count.to_le_bytes());
                r.extend_from_slice(&results);
            }
            REQ_TYPE_GET_OWNER_INFO => {
                r.extend_from_slice(
                    format!("{}\n{}\n{}", FIRMWARE_VERSION, self.prefs.node_name, self.prefs.owner_info).as_bytes(),
                );
            }
            _ => r.clear(), // unknown command
        }
        r
    }

    fn timers(&self) -> impl Iterator<Item = u64> {
        [
            self.next_local_advert,
            self.next_flood_advert,
            self.dirty_contacts_expiry,
            self.set_radio_at,
            self.revert_radio_at,
        ]
        .into_iter()
        .filter(|&t| t != 0)
    }

    fn is_looped(&self, ctx: &MeshCtx, pkt: &Packet, max_counters: &[u8; 4]) -> bool {
        let size = pkt.path_hash_size();
        let n = pkt.path()
            .chunks_exact(size)
            .filter(|h| ctx.self_id.id.is_hash_match(h))
            .count();
        n >= max_counters[size] as usize
    }
}

fn is_flood_hop_limit_exceeded(pkt: &Packet, flood_max: u8, flood_max_unscoped: u8, flood_max_advert: u8) -> bool {
    let hops = pkt.path_hash_count();
    hops >= flood_max as usize
        || (pkt.route_type() == ROUTE_TYPE_FLOOD && hops >= flood_max_unscoped as usize)
        || (pkt.payload_type() == PAYLOAD_TYPE_ADVERT && hops >= flood_max_advert as usize)
}

const MAX_LOOP_MINIMAL: [u8; 4] = [0, 4, 2, 1];
const MAX_LOOP_MODERATE: [u8; 4] = [0, 2, 1, 1];
const MAX_LOOP_STRICT: [u8; 4] = [0, 1, 1, 1];

impl MeshApp for Repeater {
    fn on_recv_packet_pre(&mut self, _ctx: &mut MeshCtx, pkt: &Packet) {
        self.recv_region = match pkt.route_type() {
            ROUTE_TYPE_TRANSPORT_FLOOD => self.regions.find_match(pkt, REGION_DENY_FLOOD).map(|r| r.id),
            ROUTE_TYPE_FLOOD if self.regions.wildcard().allows_flood() => Some(0),
            _ => None,
        };
    }

    fn allow_packet_forward(&mut self, ctx: &MeshCtx, pkt: &Packet) -> bool {
        if self.prefs.disable_fwd {
            return false;
        }
        if pkt.is_route_flood() {
            if is_flood_hop_limit_exceeded(pkt, self.prefs.flood_max, self.prefs.flood_max_unscoped, self.prefs.flood_max_advert) {
                return false;
            }
            if self.recv_region.is_none() {
                return false; // unknown transport code, or un-scoped flood denied
            }
            let maximums = match self.prefs.loop_detect {
                LOOP_DETECT_OFF => None,
                LOOP_DETECT_MINIMAL => Some(&MAX_LOOP_MINIMAL),
                LOOP_DETECT_MODERATE => Some(&MAX_LOOP_MODERATE),
                _ => Some(&MAX_LOOP_STRICT),
            };
            if let Some(m) = maximums
                && self.is_looped(ctx, pkt, m)
            {
                return false;
            }
        }
        true
    }

    fn retransmit_delay(&mut self, ctx: &mut MeshCtx, pkt: &Packet) -> u32 {
        let air = ctx.radio.est_airtime_ms(pkt.path_byte_len() + pkt.payload_len + 2);
        let t = (air as f32 * self.prefs.tx_delay_factor) as u32;
        ctx.rng.next_int(0, 5 * t + 1)
    }

    fn direct_retransmit_delay(&mut self, ctx: &mut MeshCtx, pkt: &Packet) -> u32 {
        let air = ctx.radio.est_airtime_ms(pkt.path_byte_len() + pkt.payload_len + 2);
        let t = (air as f32 * self.prefs.direct_tx_delay_factor) as u32;
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

    fn search_peers_by_hash(&mut self, _ctx: &mut MeshCtx, hash: u8) -> usize {
        self.matching_peers.clear();
        for (i, c) in self.acl.iter().enumerate() {
            if c.id.is_hash_match_v1(hash) {
                self.matching_peers.push(i);
            }
        }
        // the neighbour this node pulls an update from (`fw.auto`), while its answer is awaited
        if self.autoupdate.peer().is_some_and(|(id, _)| id.is_hash_match_v1(hash)) {
            self.matching_peers.push(autoupdate::FW_PEER);
        }
        self.matching_peers.len()
    }

    fn peer_shared_secret(&mut self, _ctx: &mut MeshCtx, peer_idx: usize) -> Option<Secret> {
        let i = *self.matching_peers.get(peer_idx)?;
        if i == autoupdate::FW_PEER {
            return self.autoupdate.peer().map(|&(_, secret)| secret);
        }
        self.acl.get(i).map(|c| c.shared_secret)
    }

    fn on_advert_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet, id: &Identity, timestamp: u32, app_data: &[u8]) {
        // zero-hop advert, not a 'share' (transport codes {0,0})
        let is_share = pkt.has_transport_codes() && pkt.transport_codes == [0, 0];
        if self.room.is_none()
            && pkt.path_hash_count() == 0
            && !is_share
            && let Some(adv) = AdvertData::parse(app_data)
            && adv.adv_type == ADV_TYPE_REPEATER
        {
            self.put_neighbour(ctx, id, timestamp, pkt.snr_db());
        }
        if self.logging {
            let name = AdvertData::parse(app_data).and_then(|a| a.name).unwrap_or_default();
            self.console(format!("ADVERT from {} '{}' hops={}", to_hex(&id.pub_key[..4]), name, pkt.path_hash_count()));
        }
    }

    fn on_anon_data_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet, secret: &Secret, sender: &Identity, data: &[u8]) {
        if self.room.is_some() {
            return self.room_login(ctx, pkt, secret, sender, data);
        }
        if data.len() < 5 {
            return;
        }
        let timestamp = read_u32(data, 0);
        let kind = data[4];
        let mut reply = Vec::with_capacity(MAX_PACKET_PAYLOAD);
        self.reply_path = None;
        let ok = if kind == 0 || kind >= b' ' {
            // a login: data[4..] is the password
            self.handle_login_req(ctx, sender, secret, timestamp, c_str(&data[4..]), pkt.is_route_flood(), &mut reply)
        } else if pkt.is_route_direct() && kind == ANON_REQ_TYPE_REGIONS {
            self.anon_prologue(ctx, timestamp, &data[5..], &mut reply) && {
                let names = self.regions.export_names(REGION_DENY_FLOOD, false, MAX_PACKET_PAYLOAD - 12 - 8);
                reply.extend_from_slice(names.as_bytes());
                true
            }
        } else if pkt.is_route_direct() && kind == ANON_REQ_TYPE_OWNER {
            self.anon_prologue(ctx, timestamp, &data[5..], &mut reply) && {
                reply.extend_from_slice(format!("{}\n{}", self.prefs.node_name, self.prefs.owner_info).as_bytes());
                true
            }
        } else if pkt.is_route_direct()
            && matches!(kind, fwshare::ANON_REQ_TYPE_FW_QUERY | fwshare::ANON_REQ_TYPE_FW_GET)
        {
            self.fw_request(ctx, timestamp, kind, &data[5..], &mut reply)
        } else if pkt.is_route_direct() && kind == ANON_REQ_TYPE_BASIC {
            self.anon_prologue(ctx, timestamp, &data[5..], &mut reply) && {
                let bridge = if self.bridge_active() { 0x03 } else { 0 }; // bridge, over ESP-NOW
                reply.push(if self.prefs.disable_fwd { 0x80 } else { 0 } | bridge); // feature bits
                true
            }
        } else {
            false
        };
        if !ok || reply.is_empty() {
            return;
        }

        let client_out = self.get_client(&sender.pub_key).and_then(|i| self.acl[i].out_path().map(|(l, p)| (l, p.to_vec())));
        let phs = pkt.path_hash_size();
        if pkt.is_route_flood() {
            // tell the sender the path to here, with the response attached
            if let Some(path) = ctx.create_path_return(sender.hash(), secret, pkt.path_len, &pkt.path, PAYLOAD_TYPE_RESPONSE, &reply) {
                self.send_flood_reply(ctx, path, SERVER_RESPONSE_DELAY, phs);
            }
            return;
        }
        let Some(resp) = ctx.create_datagram(PAYLOAD_TYPE_RESPONSE, sender, secret, &reply) else {
            return;
        };
        if let Some((len, path)) = self.reply_path.take() {
            ctx.send_direct(resp, len, &path, SERVER_RESPONSE_DELAY);
        } else if let Some((len, path)) = client_out {
            ctx.send_direct(resp, len, &path, SERVER_RESPONSE_DELAY);
        } else {
            self.send_flood_reply(ctx, resp, SERVER_RESPONSE_DELAY, phs);
        }
    }

    fn on_peer_data_recv(
        &mut self, ctx: &mut MeshCtx, pkt: &Packet, payload_type: u8, sender_idx: usize, secret: &Secret, data: &[u8],
    ) {
        let Some(&i) = self.matching_peers.get(sender_idx) else {
            return;
        };
        if i == autoupdate::FW_PEER {
            if payload_type == PAYLOAD_TYPE_RESPONSE {
                self.auto_update_answer(ctx, data);
            }
            return;
        }
        if i >= self.acl.len() || data.len() < 5 {
            return;
        }
        if self.room.is_some() {
            return self.room_peer_data(ctx, pkt, payload_type, i, secret, data);
        }
        let phs = pkt.path_hash_size();
        let timestamp = read_u32(data, 0);

        if payload_type == PAYLOAD_TYPE_REQ {
            if timestamp <= self.acl[i].last_timestamp {
                return; // replay
            }
            let reply = self.handle_request(ctx, i, timestamp, &data[4..]);
            if reply.is_empty() {
                return;
            }
            let now = ctx.current_time();
            let client = &mut self.acl[i];
            client.last_timestamp = timestamp;
            client.last_activity = now;
            let id = client.id;
            let out = client.out_path().map(|(l, p)| (l, p.to_vec()));
            self.note_admin_timestamp(ctx, i, false);
            if pkt.is_route_flood() {
                if let Some(path) = ctx.create_path_return(id.hash(), secret, pkt.path_len, &pkt.path, PAYLOAD_TYPE_RESPONSE, &reply) {
                    self.send_flood_reply(ctx, path, SERVER_RESPONSE_DELAY, phs);
                }
            } else if let Some(resp) = ctx.create_datagram(PAYLOAD_TYPE_RESPONSE, &id, secret, &reply) {
                match out {
                    Some((len, path)) => ctx.send_direct(resp, len, &path, SERVER_RESPONSE_DELAY),
                    None => self.send_flood_reply(ctx, resp, SERVER_RESPONSE_DELAY, phs),
                }
            }
        } else if payload_type == PAYLOAD_TYPE_TXT_MSG && data.len() > 5 && self.acl[i].is_admin() {
            let flags = data[4] >> 2;
            if !matches!(flags, TXT_TYPE_PLAIN | TXT_TYPE_CLI_DATA | TXT_TYPE_CLI_COMMAND) {
                return;
            }
            if timestamp < self.acl[i].last_timestamp {
                return; // replay
            }
            let is_retry = timestamp == self.acl[i].last_timestamp;
            let now = ctx.current_time();
            let client = &mut self.acl[i];
            client.last_timestamp = timestamp;
            client.last_activity = now;
            let id = client.id;
            let out = client.out_path().map(|(l, p)| (l, p.to_vec()));
            if !is_retry {
                self.note_admin_timestamp(ctx, i, true);
            }
            let text = c_str(&data[5..]);

            if flags == TXT_TYPE_PLAIN {
                // legacy CLI: ack = sha256(timestamp + flags + text + sender pubkey)[..4]
                let mut ack = [0u8; 4];
                crypto::sha256(&mut ack, &[&data[..5 + text.len()], &id.pub_key]);
                let pkt_ack = ctx.create_ack(&ack);
                match &out {
                    Some((len, path)) => ctx.send_direct(pkt_ack, *len, path, TXT_ACK_DELAY),
                    None => self.send_flood_reply(ctx, pkt_ack, TXT_ACK_DELAY, phs),
                }
            }

            let reply = if is_retry {
                String::new()
            } else {
                let cmd = String::from_utf8_lossy(text).into_owned();
                self.handle_command(ctx, timestamp, &cmd)
            };
            if !reply.is_empty() {
                let mut ts = ctx.current_time_unique();
                if ts == timestamp {
                    ts += 1; // the two timestamps must differ in the CLI view
                }
                let mut temp = Vec::with_capacity(5 + reply.len());
                temp.extend_from_slice(&ts.to_le_bytes());
                temp.push(TXT_TYPE_CLI_DATA << 2);
                let max_text = MAX_PACKET_PAYLOAD - 2 - 2 - 16 - 5;
                temp.extend_from_slice(&reply.as_bytes()[..reply.len().min(max_text)]);
                if let Some(msg) = ctx.create_datagram(PAYLOAD_TYPE_TXT_MSG, &id, secret, &temp) {
                    match out {
                        Some((len, path)) => ctx.send_direct(msg, len, &path, CLI_REPLY_DELAY_MILLIS),
                        None => self.send_flood_reply(ctx, msg, CLI_REPLY_DELAY_MILLIS, phs),
                    }
                }
            }
        }
    }

    fn on_peer_path_recv(
        &mut self, ctx: &mut MeshCtx, _pkt: &Packet, sender_idx: usize, _secret: &Secret, path_len: u8, path: &[u8],
        extra_type: u8, extra: &[u8],
    ) -> bool {
        if let Some(&i) = self.matching_peers.get(sender_idx)
            && i < self.acl.len()
        {
            let now = ctx.current_time();
            let c = &mut self.acl[i];
            let n = Packet::path_bytes_for(path_len).min(path.len());
            c.out_path[..n].copy_from_slice(&path[..n]);
            c.out_path_len = path_len;
            c.last_activity = now;
        }
        if self.room.is_some() && extra_type == PAYLOAD_TYPE_ACK {
            self.room_process_ack(extra); // a pushed post's ACK came with the path
        }
        false // no reciprocal path
    }

    fn on_ack_recv(&mut self, _ctx: &mut MeshCtx, _pkt: &Packet, ack_crc: u32) -> bool {
        self.room.is_some() && self.room_process_ack(&ack_crc.to_le_bytes())
    }

    fn on_control_data_recv(&mut self, ctx: &mut MeshCtx, pkt: &Packet) {
        if self.room.is_some() {
            return; // neighbour discovery is for repeaters
        }
        let p = pkt.payload();
        let kind = p[0] & 0xF0;
        if kind == CTL_TYPE_NODE_DISCOVER_REQ
            && p.len() >= 6
            && !self.prefs.disable_fwd
            && !self.is_hidden_node()
            && self.discover_limiter.allow(ctx.current_time())
        {
            let filter = p[1];
            let tag = read_u32(p, 2);
            let since = if p.len() >= 10 { read_u32(p, 6) } else { 0 };
            if filter & (1 << ADV_TYPE_REPEATER) != 0 && self.prefs.discovery_mod_timestamp >= since {
                let prefix_only = p[0] & 1 != 0;
                let mut data = [0u8; 6 + PUB_KEY_SIZE];
                data[0] = CTL_TYPE_NODE_DISCOVER_RESP | ADV_TYPE_REPEATER;
                data[1] = pkt.snr as u8; // inbound SNR x4
                data[2..6].copy_from_slice(&tag.to_le_bytes());
                data[6..].copy_from_slice(ctx.self_id.pub_key());
                let len = if prefix_only { 6 + 8 } else { 6 + PUB_KEY_SIZE };
                if let Some(resp) = ctx.create_control_data(&data[..len]) {
                    // widened random delay: several nodes may answer
                    let d = self.retransmit_delay(ctx, &resp) * 4;
                    ctx.send_zero_hop(resp, d);
                }
            }
        } else if kind == CTL_TYPE_NODE_DISCOVER_RESP && p.len() >= 6 + PUB_KEY_SIZE {
            if p[0] & 0x0F != ADV_TYPE_REPEATER {
                return;
            }
            if self.pending_discover_tag == 0 || ctx.now > self.pending_discover_until {
                self.pending_discover_tag = 0;
                return;
            }
            if read_u32(p, 2) != self.pending_discover_tag {
                return;
            }
            let id = Identity::from_slice(&p[6..]).unwrap();
            if id == ctx.self_id.id {
                return;
            }
            let now = ctx.current_time();
            self.put_neighbour(ctx, &id, now, pkt.snr_db());
        }
    }

    fn on_loop(&mut self, ctx: &mut MeshCtx) {
        let now = ctx.now;
        if self.next_flood_advert != 0 && now >= self.next_flood_advert {
            if let Some(pkt) = self.create_self_advert(ctx) {
                let scope = self.default_scope();
                self.send_flood_scoped(ctx, pkt, scope, 0, self.prefs.path_hash_mode as usize + 1);
            }
            self.update_flood_advert_timer(ctx);
            self.update_advert_timer(ctx); // so they don't overlap
        } else if self.next_local_advert != 0 && now >= self.next_local_advert {
            if let Some(pkt) = self.create_self_advert(ctx) {
                ctx.send_zero_hop(pkt, 0);
            }
            self.update_advert_timer(ctx);
        }
        if self.set_radio_at != 0 && now >= self.set_radio_at {
            self.set_radio_at = 0;
            self.effects.apply_radio = Some(self.pending_radio);
        }
        if self.revert_radio_at != 0 && now >= self.revert_radio_at {
            self.revert_radio_at = 0;
            self.effects.apply_radio = Some(self.prefs.radio);
        }
        if self.dirty_contacts_expiry != 0 && now >= self.dirty_contacts_expiry {
            self.dirty_contacts_expiry = 0;
            self.effects.save_acl = true;
        }
        self.room_loop(ctx);
        self.auto_update_loop(ctx);
    }

    fn next_timer(&self) -> Option<u64> {
        self.timers().chain(self.room.as_ref().and_then(|r| r.next_timer())).chain(self.auto_update_timer()).min()
    }

    fn log_rx(&mut self, ctx: &MeshCtx, pkt: &Packet, len: usize, score: f32) {
        if self.bridge_active() && self.prefs.bridge_source_rx {
            self.bridge_send(pkt);
        }
        if self.uplink_enabled && self.prefs.mqtt_packets && self.uplink.len() < UPLINK_QUEUE_MAX {
            let mut raw = [0u8; crate::MAX_TRANS_UNIT];
            if let Some(n) = pkt.write_to(&mut raw) {
                self.uplink.push(UplinkPacket {
                    raw: raw[..n].to_vec(),
                    tx: false,
                    snr: ctx.stats.last_snr,
                    rssi: ctx.stats.last_rssi,
                    score_duration: Some(((score * 1000.0) as i32, ctx.radio.est_airtime_ms(len))),
                });
            }
        }
        if !self.logging {
            return;
        }
        let mut line = format!(
            "{}: RX, len={} (type={}, route={}, payload_len={}) SNR={} RSSI={} score={}",
            fmt_time(ctx.current_time()),
            len,
            pkt.payload_type(),
            if pkt.is_route_direct() { "D" } else { "F" },
            pkt.payload_len,
            ctx.stats.last_snr as i32,
            ctx.stats.last_rssi as i32,
            (score * 1000.0) as i32
        );
        if matches!(pkt.payload_type(), PAYLOAD_TYPE_PATH | PAYLOAD_TYPE_REQ | PAYLOAD_TYPE_RESPONSE | PAYLOAD_TYPE_TXT_MSG)
            && pkt.payload_len >= 2
        {
            line.push_str(&format!(" [{:02X} -> {:02X}]", pkt.payload[1], pkt.payload[0]));
        }
        self.console(line);
    }

    fn log_tx(&mut self, ctx: &MeshCtx, pkt: &Packet, len: usize, _airtime_ms: usize) {
        if self.bridge_active() && !self.prefs.bridge_source_rx {
            self.bridge_send(pkt);
        }
        if self.uplink_enabled && self.prefs.mqtt_packets && self.prefs.mqtt_tx && self.uplink.len() < UPLINK_QUEUE_MAX {
            let mut raw = [0u8; crate::MAX_TRANS_UNIT];
            if let Some(n) = pkt.write_to(&mut raw) {
                // as in the C++ firmware: SNR/RSSI are the last RX values, no score
                self.uplink.push(UplinkPacket {
                    raw: raw[..n].to_vec(),
                    tx: true,
                    snr: ctx.stats.last_snr,
                    rssi: ctx.stats.last_rssi,
                    score_duration: None,
                });
            }
        }
        if !self.logging {
            return;
        }
        let mut line = format!(
            "{}: TX, len={} (type={}, route={}, payload_len={})",
            fmt_time(ctx.current_time()),
            len,
            pkt.payload_type(),
            if pkt.is_route_direct() { "D" } else { "F" },
            pkt.payload_len
        );
        if matches!(pkt.payload_type(), PAYLOAD_TYPE_PATH | PAYLOAD_TYPE_REQ | PAYLOAD_TYPE_RESPONSE | PAYLOAD_TYPE_TXT_MSG)
            && pkt.payload_len >= 2
        {
            line.push_str(&format!(" [{:02X} -> {:02X}]", pkt.payload[1], pkt.payload[0]));
        }
        self.console(line);
    }

    fn tx_coding_rate(&self, ctx: &MeshCtx, pkt: &Packet) -> Option<u8> {
        if !self.prefs.dynamic_cr || !pkt.is_route_direct() || pkt.path_hash_count() == 0 {
            return None;
        }
        // next hop = first path entry; use the SNR we last heard from that neighbour
        let next = &pkt.path[..pkt.path_hash_size()];
        let n = self.neighbours.iter().find(|n| n.id.is_hash_match(next))?;
        Some(ctx.radio.dynamic_cr(n.snr as f32 / 4.0))
    }

    fn log_tx_fail(&mut self, ctx: &MeshCtx) {
        if self.logging {
            self.console(format!("{}: TX FAIL!", fmt_time(ctx.current_time())));
        }
    }
}

// ------------------------------------------------------------------- CLI

impl Repeater {
    /// Run one CLI command (`MyMesh::handleCommand` + `CommonCLI`). `sender_timestamp` is 0
    /// for the local serial console, which unlocks a few sensitive commands.
    pub fn handle_command(&mut self, ctx: &mut MeshCtx, sender_timestamp: u32, command: &str) -> String {
        if self.region_load.is_some() {
            return self.region_load_line(command);
        }
        let mut command = command.trim_start_matches(' ');
        let mut prefix = "";
        // optional "XX|" prefix (companion radio CLI), reflected back
        if command.len() > 4 && command.as_bytes()[2] == b'|' && command.is_char_boundary(3) {
            prefix = &command[..3];
            command = &command[3..];
        }
        let reply = self.dispatch_command(ctx, sender_timestamp, command);
        if prefix.is_empty() { reply } else { format!("{prefix}{reply}") }
    }

    fn dispatch_command(&mut self, ctx: &mut MeshCtx, ts: u32, cmd: &str) -> String {
        let serial = ts == 0;
        if let Some(r) = self.room_command(ctx, cmd) {
            return r;
        }
        if let Some(r) = self.handle_radio_command(cmd) {
            return r;
        }
        if let Some(r) = self.handle_board_command(cmd) {
            return r;
        }

        if let Some(rest) = cmd.strip_prefix("setperm ") {
            // setperm {pubkey-hex} {permissions}
            let Some((hex, perms)) = rest.split_once(' ') else {
                return "Err - bad params".into();
            };
            let Some(key) = parse_hex_prefix(hex) else {
                return "Err - bad pubkey".into();
            };
            let self_id = ctx.self_id.clone();
            return if self.apply_permissions(&self_id, &key, atoi(perms) as u8) {
                self.dirty_contacts_expiry = ctx.now + LAZY_CONTACTS_WRITE_DELAY;
                "OK".into()
            } else {
                "Err - invalid params".into()
            };
        }
        if serial && cmd == "get acl" {
            let lines: Vec<String> = self
                .acl
                .iter()
                .filter(|c| c.permissions != 0)
                .map(|c| format!("{:02X} {}", c.permissions, to_hex(&c.id.pub_key)))
                .collect();
            self.console("ACL:".into());
            for l in lines {
                self.console(l);
            }
            return String::new();
        }
        if let Some(sub) = cmd.strip_prefix("discover.neighbors") {
            if !sub.trim().is_empty() {
                return "Err - discover.neighbors has no options".into();
            }
            self.send_node_discover_req(ctx);
            return "OK - Discover sent".into();
        }

        if cmd.starts_with("poweroff") || cmd.starts_with("shutdown") {
            self.effects.power_off = true;
            return "OK - powering off".into();
        }
        if cmd.starts_with("reboot") {
            self.effects.reboot = true;
            return "OK - rebooting".into();
        }
        if cmd.starts_with("clkreboot") {
            ctx.set_current_time(DEFAULT_EPOCH);
            self.effects.reboot = true;
            return "OK - rebooting".into();
        }
        if cmd == "advert.zerohop" || cmd.starts_with("advert.zerohop ") {
            self.send_self_advertisement(ctx, 1500, false);
            return "OK - zerohop advert sent".into();
        }
        if cmd.starts_with("advert") {
            self.send_self_advertisement(ctx, 1500, true);
            return "OK - Advert sent".into();
        }
        if cmd.starts_with("clock sync") {
            let curr = ctx.current_time();
            return if ts > curr {
                ctx.set_current_time(ts + 1);
                format!("OK - clock set: {}", fmt_time(ctx.current_time()))
            } else {
                "ERR: clock cannot go backwards".into()
            };
        }
        if cmd == "fw update" {
            return self.auto_update_now(ctx);
        }
        if cmd.starts_with("start ota ble") {
            return match self.board.ble_ota_name.clone() {
                Some(name) => {
                    let pin = 100_000 + ctx.rng.next_u32() % 900_000;
                    self.effects.start_ble_ota = Some(pin);
                    format!("Started: BLE '{name}' PIN {pin} for {BLE_OTA_WINDOW_MINS} min")
                }
                None => "Error".into(),
            };
        }
        if cmd.starts_with("start ota") {
            let Some(ssid) = self.board.wifi_ota_ssid.clone() else {
                return "Error".into();
            };
            // a fresh code each time: on the LAN anyone could reach the page otherwise
            let code = 100_000 + ctx.rng.next_u32() % 900_000;
            // not the admin password: a recorded handshake would give it away to an offline search
            if self.wifi_ota_key.is_empty() {
                const CHARS: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
                self.wifi_ota_key = (0..10).map(|_| CHARS[ctx.rng.next_u32() as usize % CHARS.len()] as char).collect();
            }
            self.effects.start_ota = Some((code, self.wifi_ota_key.clone()));
            return match self.board.wifi_ip.clone() {
                Some(ip) => format!("Started: http://{ip}/update code {code}"),
                None => format!(
                    "Started: http://192.168.4.1/update (Wi-Fi '{ssid}' password {}) code {code}",
                    self.wifi_ota_key
                ),
            };
        }
        if cmd.starts_with("clock") {
            return fmt_time(ctx.current_time());
        }
        if let Some(v) = cmd.strip_prefix("time ") {
            let secs = atoi(v);
            // the local console may also correct a clock that ran ahead
            return if secs > ctx.current_time() || (serial && secs > DEFAULT_EPOCH) {
                ctx.set_current_time(secs);
                format!("OK - clock set: {}", fmt_time(ctx.current_time()))
            } else {
                "(ERR: clock cannot go backwards)".into()
            };
        }
        if let Some(hex) = cmd.strip_prefix("neighbor.remove ") {
            let Some(key) = parse_hex_prefix(hex) else {
                return "ERR: bad pubkey".into();
            };
            self.neighbours.retain(|n| !n.id.pub_key.starts_with(&key));
            return "OK".into();
        }
        if cmd.starts_with("neighbors") {
            return self.format_neighbours(ctx);
        }
        if let Some(rest) = cmd.strip_prefix("tempradio ") {
            let parts: Vec<&str> = rest.splitn(5, ',').collect();
            let get = |i: usize| parts.get(i).copied().unwrap_or("");
            let params = RadioParams {
                freq_mhz: atof(get(0)) as f32,
                bw_khz: atof(get(1)) as f32,
                sf: atoi(get(2)) as u8,
                cr: atoi(get(3)) as u8,
            };
            let mins = atoi_signed(get(4));
            return if params.is_valid() && mins > 0 {
                self.pending_radio = params;
                self.set_radio_at = ctx.now + 2000; // let the reply go out first
                self.revert_radio_at = ctx.now + 2000 + mins as u64 * 60_000;
                format!("OK - temp params for {mins} mins")
            } else {
                "Error, invalid params".into()
            };
        }
        if let Some(pw) = cmd.strip_prefix("password ") {
            self.prefs.password = truncate_str(pw, PASSWORD_MAX);
            self.save_prefs();
            return format!("password now: {}", self.prefs.password);
        }
        if cmd.starts_with("clear stats") {
            ctx.reset_stats();
            return "(OK - stats reset)".into();
        }
        if cmd.trim() == "send mqtt.status" {
            self.effects.send_mqtt_status = true;
            return "OK".into();
        }
        if let Some(cfg) = cmd.strip_prefix("get ") {
            return self.handle_get(ctx, serial, cfg);
        }
        if let Some(cfg) = cmd.strip_prefix("set ") {
            return self.handle_set(ctx, serial, cfg);
        }
        if serial && cmd == "erase" {
            self.effects.erase = true;
            return "File system erase: OK".into();
        }
        if cmd.starts_with("ver") {
            return format!("{} ({})", FIRMWARE_VERSION, release::build_label(&self.board.build_date));
        }
        if cmd.starts_with("board") {
            return self.board.manufacturer.clone();
        }
        if cmd == "power" {
            return format!("batt:{} mV usb:n/a solar_chg:n/a ext:no charger:no", self.board.batt_mv);
        }
        if cmd.starts_with("sensor get ") {
            return "null".into();
        }
        if cmd.starts_with("sensor set ") {
            return "can't find custom var".into();
        }
        if cmd.starts_with("sensor list") {
            return "no custom var".into();
        }
        if cmd.starts_with("region") {
            return self.handle_region_command(ctx, cmd);
        }
        if cmd.starts_with("powersaving on") {
            if !self.board.powersaving_supported {
                return "Board not supported".into();
            }
            if self.bridge_active() {
                return "Bridge not supported".into();
            }
            self.prefs.powersaving_enabled = true;
            self.save_prefs();
            // upstream's ESP32 wording: the first sleep comes two minutes after boot
            return "on - After 2 minutes".into();
        }
        if cmd.starts_with("powersaving off") {
            self.prefs.powersaving_enabled = false;
            self.save_prefs();
            return "off".into();
        }
        if cmd.starts_with("powersaving stats") {
            return match self.board.power_status.as_str() {
                "" => "> no sleep yet".into(),
                s => format!("> {s}"),
            };
        }
        if cmd.starts_with("powersaving") {
            return if self.prefs.powersaving_enabled { "on" } else { "off" }.into();
        }
        if cmd.starts_with("log start") {
            self.logging = true;
            return "   logging on".into();
        }
        if cmd.starts_with("log stop") {
            self.logging = false;
            return "   logging off".into();
        }
        if cmd.starts_with("log erase") {
            return "   log erased".into();
        }
        if serial && cmd.starts_with("log") {
            return "   EOF".into();
        }
        if serial && (cmd == "stats-packets" || cmd.starts_with("stats-packets ")) {
            let s = &ctx.stats;
            return format!(
                "{{\"recv\":{},\"sent\":{},\"flood_tx\":{},\"direct_tx\":{},\"flood_rx\":{},\"direct_rx\":{},\"recv_errors\":{}}}",
                s.n_packets_recv, s.n_packets_sent, s.n_sent_flood, s.n_sent_direct, s.n_recv_flood, s.n_recv_direct, s.n_recv_errors
            );
        }
        if serial && (cmd == "stats-radio" || cmd.starts_with("stats-radio ")) {
            let s = &ctx.stats;
            return format!(
                "{{\"noise_floor\":{},\"last_rssi\":{},\"last_snr\":{:.2},\"tx_air_secs\":{},\"rx_air_secs\":{}}}",
                s.noise_floor,
                s.last_rssi as i16,
                s.last_snr,
                s.total_air_time_ms / 1000,
                s.rx_air_time_ms / 1000
            );
        }
        if serial && (cmd == "stats-core" || cmd.starts_with("stats-core ")) {
            return format!(
                "{{\"battery_mv\":{},\"uptime_secs\":{},\"errors\":{},\"queue_len\":{}}}",
                self.board.batt_mv,
                ctx.now / 1000,
                ctx.stats.err_flags,
                ctx.outbound_len()
            );
        }
        "Unknown command".into()
    }

    /// `CommonRadioPrefs::handleCommand`.
    fn handle_radio_command(&mut self, cmd: &str) -> Option<String> {
        let p = &mut self.prefs;
        let r: String = if cmd == "get radio" {
            format!("> {},{},{},{}", ftoa(p.radio.freq_mhz as f64, 3), ftoa(p.radio.bw_khz as f64, 3), p.radio.sf, p.radio.cr)
        } else if let Some(rest) = cmd.strip_prefix("set radio ") {
            let parts: Vec<&str> = rest.splitn(4, ',').collect();
            let get = |i: usize| parts.get(i).copied().unwrap_or("");
            let params = RadioParams {
                freq_mhz: atof(get(0)) as f32,
                bw_khz: atof(get(1)) as f32,
                sf: atoi(get(2)) as u8,
                cr: atoi(get(3)) as u8,
            };
            if params.is_valid() {
                p.radio = params;
                self.save_prefs();
                "OK - reboot to apply".into()
            } else {
                "Error, invalid radio params".into()
            }
        } else if cmd == "get freq" {
            format!("> {}", ftoa(p.radio.freq_mhz as f64, 3))
        } else if cmd == "get af" {
            format!("> {}", ftoa(p.airtime_factor as f64, 3))
        } else if let Some(v) = cmd.strip_prefix("set af ") {
            let af = atof(v) as f32;
            if v.trim().is_empty() || !(0.0..=9.0).contains(&af) {
                "ERROR: af must be 0-9".into()
            } else {
                p.airtime_factor = af;
                self.save_prefs();
                "OK".into()
            }
        } else if cmd == "get dutycycle" {
            format!("> {:.1}%", 100.0 / (p.airtime_factor + 1.0))
        } else if let Some(v) = cmd.strip_prefix("set dutycycle ") {
            let dc = atof(v) as f32;
            if !(1.0..=100.0).contains(&dc) {
                "ERROR: dutycycle must be 1-100".into()
            } else {
                p.airtime_factor = 100.0 / dc - 1.0;
                let actual = 100.0 / (p.airtime_factor + 1.0);
                self.save_prefs();
                format!("OK - {actual:.1}%")
            }
        } else if cmd == "get int.thresh" {
            format!("> {}", p.interference_threshold)
        } else if let Some(v) = cmd.strip_prefix("set int.thresh ") {
            p.interference_threshold = atoi(v) as u8;
            self.save_prefs();
            "OK".into()
        } else if cmd == "get cad" {
            format!("> {}", if p.cad_enabled { "on" } else { "off" })
        } else if let Some(v) = cmd.strip_prefix("set cad ") {
            p.cad_enabled = v.starts_with("on");
            self.save_prefs();
            "OK".into()
        } else if cmd == "get radio.rxgain" {
            format!("> {}", if p.rx_boosted_gain { "on" } else { "off" })
        } else if let Some(v) = cmd.strip_prefix("set radio.rxgain ") {
            p.rx_boosted_gain = v.starts_with("on");
            self.effects.rx_boosted_gain = Some(p.rx_boosted_gain);
            self.save_prefs();
            "OK".into()
        } else if cmd == "get tx" || cmd.starts_with("get tx ") {
            format!("> {}", p.tx_power_dbm)
        } else if let Some(v) = cmd.strip_prefix("set tx ") {
            p.tx_power_dbm = atoi_signed(v).clamp(-9, 22) as i8;
            self.effects.tx_power = Some(p.tx_power_dbm);
            self.save_prefs();
            "OK".into()
        } else if cmd == "get rxdelay" {
            format!("> {}", ftoa(p.rx_delay_base as f64, 3))
        } else if let Some(v) = cmd.strip_prefix("set rxdelay ") {
            let d = atof(v) as f32;
            if (0.0..=20.0).contains(&d) {
                p.rx_delay_base = d;
                self.save_prefs();
                "OK".into()
            } else {
                "Error, must be 0-20".into()
            }
        } else if cmd == "get agc.reset.interval" {
            format!("> {}", p.agc_reset_interval as u32 * 4)
        } else if let Some(v) = cmd.strip_prefix("set agc.reset.interval ") {
            let secs = atoi_signed(v).clamp(0, 255 * 4);
            p.agc_reset_interval = (secs / 4) as u8;
            let rounded = p.agc_reset_interval as u32 * 4;
            self.save_prefs();
            format!("OK - interval rounded to {rounded}")
        } else if cmd == "get path.hash.mode" {
            format!("> {}", p.path_hash_mode)
        } else if let Some(v) = cmd.strip_prefix("set path.hash.mode ") {
            let m = atoi(v);
            if m < 3 {
                p.path_hash_mode = m as u8;
                self.save_prefs();
                "OK".into()
            } else {
                "Error, must be 0,1, or 2".into()
            }
        } else if cmd == "get multi.acks" {
            format!("> {}", p.multi_acks)
        } else if let Some(v) = cmd.strip_prefix("set multi.acks ") {
            p.multi_acks = (atoi(v) as u8).min(1);
            self.save_prefs();
            "OK".into()
        } else if cmd == "get txdelay" {
            format!("> {}", ftoa(p.tx_delay_factor as f64, 3))
        } else if let Some(v) = cmd.strip_prefix("set txdelay ") {
            let f = atof(v) as f32;
            if (0.0..=2.0).contains(&f) {
                p.tx_delay_factor = f;
                self.save_prefs();
                "OK".into()
            } else {
                "Error, must be 0-2".into()
            }
        } else if cmd == "get direct.txdelay" {
            format!("> {}", ftoa(p.direct_tx_delay_factor as f64, 3))
        } else {
            let v = cmd.strip_prefix("set direct.txdelay ")?;
            let f = atof(v) as f32;
            if (0.0..=2.0).contains(&f) {
                p.direct_tx_delay_factor = f;
                self.save_prefs();
                "OK".into()
            } else {
                "Error, must be 0-2".into()
            }
        };
        Some(r)
    }

    /// Heltec V4 FEM LNA control (`HeltecV4Board::handleCommand`).
    fn handle_board_command(&mut self, cmd: &str) -> Option<String> {
        if cmd == "get radio.fem.rxgain" {
            return Some(if !self.board.fem_lna_controllable {
                "Error: unsupported".into()
            } else {
                format!("> {}", if self.prefs.radio_fem_rxgain { "on" } else { "off" })
            });
        }
        if let Some(v) = cmd.strip_prefix("set radio.fem.rxgain ") {
            return Some(if !self.board.fem_lna_controllable {
                "Error: unsupported".into()
            } else if v.starts_with("on") || v.starts_with("off") {
                let on = v.starts_with("on");
                self.prefs.radio_fem_rxgain = on;
                self.effects.fem_lna = Some(on);
                self.save_prefs();
                format!("OK - LoRa FEM RX gain {}", if on { "on" } else { "off" })
            } else {
                "Error: state must be on or off".into()
            });
        }
        None
    }

    fn handle_get(&mut self, ctx: &MeshCtx, serial: bool, cfg: &str) -> String {
        let p = &self.prefs;
        let onoff = |b: bool| if b { "on" } else { "off" };
        if let Some(r) = self.handle_get_network(cfg) {
            return r;
        }
        if cfg.starts_with("ota.root") {
            return self.ota_root_status();
        }
        if cfg.starts_with("ota.owner") {
            return match (self.prefs.ota_owners.is_empty(), self.prefs.ota_roots.is_empty()) {
                (true, _) => "> none, the root's signature is enough".into(),
                (false, true) => format!("> {} (no root pinned: not checked)", release::join_fingerprints(&self.prefs.ota_owners, ", ")),
                (false, false) => format!("> {}", release::join_fingerprints(&self.prefs.ota_owners, ", ")),
            };
        }
        if cfg.starts_with("fw.auto") {
            self.auto_update_status()
        } else if cfg.starts_with("fw.channel") {
            format!("> {}", release::channel_name(p.fw_channel))
        } else if cfg.starts_with("fw.source") {
            if p.fw_source.is_empty() { "> auto".into() } else { format!("> {}", to_hex(&p.fw_source)) }
        } else if cfg.starts_with("fw.server") {
            format!("> {}", p.fw_server)
        } else if cfg.starts_with("fw.serve") {
            format!("> {}", onoff(p.fw_serve))
        } else if cfg.starts_with("fw.status") {
            // the store fetches for `fw.auto` and `fw update` too, not only for `fw.serve`: what it
            // last did (or why it failed) is worth seeing whichever asked
            if self.fw_store.is_none() {
                "> no update store on this board".into()
            } else if self.board.fw_status.is_empty() {
                "> nothing fetched yet".into()
            } else {
                format!("> {}", self.board.fw_status)
            }
        } else if cfg.starts_with("dyncr") {
            format!("> {}", onoff(p.dynamic_cr))
        } else if cfg.starts_with("room") && self.room.is_none() {
            format!("> {}", onoff(p.room))
        } else if cfg.starts_with("allow.read.only") {
            format!("> {}", onoff(p.allow_read_only))
        } else if cfg.starts_with("flood.advert.interval") {
            format!("> {}", p.flood_advert_interval)
        } else if cfg.starts_with("advert.interval") {
            format!("> {}", p.advert_interval as u32 * 2)
        } else if cfg.starts_with("guest.password") {
            format!("> {}", p.guest_password)
        } else if serial && cfg.starts_with("prv.key") {
            format!("> {}", to_hex(ctx.self_id.private_key()))
        } else if cfg.starts_with("name") {
            format!("> {}", p.node_name)
        } else if cfg.starts_with("repeat") {
            format!("> {}", onoff(!p.disable_fwd))
        } else if cfg.starts_with("lat") {
            format!("> {}", ftoa(p.node_lat, 6))
        } else if cfg.starts_with("lon") {
            format!("> {}", ftoa(p.node_lon, 6))
        } else if cfg.starts_with("flood.max.advert") {
            format!("> {}", p.flood_max_advert)
        } else if cfg.starts_with("flood.max.unscoped") {
            format!("> {}", p.flood_max_unscoped)
        } else if cfg.starts_with("flood.max") {
            format!("> {}", p.flood_max)
        } else if cfg.starts_with("owner.info") {
            format!("> {}", p.owner_info.replace('\n', "|"))
        } else if cfg.starts_with("loop.detect") {
            match p.loop_detect {
                LOOP_DETECT_OFF => "> off",
                LOOP_DETECT_MINIMAL => "> minimal",
                LOOP_DETECT_MODERATE => "> moderate",
                _ => "> strict",
            }
            .into()
        } else if cfg.starts_with("public.key") {
            format!("> {}", to_hex(ctx.self_id.pub_key()))
        } else if cfg.starts_with("role") {
            format!("> {FIRMWARE_ROLE}")
        } else if cfg.starts_with("bridge.type") {
            format!("> {}", self.board.bridge.unwrap_or("none"))
        } else if self.board.bridge.is_some() && cfg.starts_with("bridge.enabled") {
            format!("> {}", onoff(p.bridge_enabled))
        } else if self.board.bridge.is_some() && cfg.starts_with("bridge.delay") {
            format!("> {}", p.bridge_delay)
        } else if self.board.bridge.is_some() && cfg.starts_with("bridge.source") {
            format!("> {}", if p.bridge_source_rx { "logRx" } else { "logTx" })
        } else if self.board.bridge.is_some() && cfg.starts_with("bridge.channel") {
            format!("> {}", p.bridge_channel)
        } else if self.board.bridge.is_some() && cfg.starts_with("bridge.secret") {
            format!("> {}", p.bridge_secret)
        } else if cfg.starts_with("bootloader.ver") || cfg.starts_with("adc.multiplier") {
            "Error: unsupported".into()
        } else if cfg.starts_with("pwrmgt.support") {
            "> unsupported".into()
        } else if cfg.starts_with("pwrmgt.source") || cfg.starts_with("pwrmgt.bootmv") {
            "ERROR: Power management not supported".into()
        } else if cfg.starts_with("pwrmgt.bootreason") {
            "> Reset: Not available; Shutdown: Not available".into()
        } else if cfg.starts_with("extra.sf") {
            "No extra SF configured".into()
        } else {
            format!("??: {cfg}")
        }
    }

    /// `get ota.root`: the pinned root keys and what a release must name to be installed here.
    fn ota_root_status(&self) -> String {
        let b = &self.ota.build;
        let build = format!("{}/{} #{}", release::board_name(b.board), release::role_name(b.role), b.counter);
        if self.prefs.ota_roots.is_empty() {
            format!("> none, unsigned updates accepted over BLE, Wi-Fi and USB, none over LoRa ({build})")
        } else {
            format!("> {} ({build})", release::join_fingerprints(&self.prefs.ota_roots, ", "))
        }
    }

    /// `set ota.root <fingerprint>[,<fingerprint>...]` pins the root keys whose signed releases
    /// are installed; `set ota.root off` unpins them. Over LoRa only such releases are taken:
    /// with no root pinned, updates come over BLE, Wi-Fi or USB alone. The first pin may come
    /// from an admin, as an admin can open the BLE update window until then; once a root is
    /// pinned only the serial console changes it, so a leaked admin password can't swap the root
    /// along with the firmware.
    fn set_ota_root(&mut self, serial: bool, value: &str) -> String {
        let value = value.trim();
        let roots = if value == "off" { Some(Vec::new()) } else { release::parse_fingerprints(value) };
        let Some(roots) = roots else {
            return format!("Error: up to {} fingerprints of 64 hex digits, or off", release::MAX_ROOTS);
        };
        if roots == self.prefs.ota_roots {
            return "OK".into();
        }
        if !self.prefs.ota_roots.is_empty() && !serial {
            return "Error: a pinned root can only be changed on the serial console".into();
        }
        self.ota.roots = roots.clone();
        self.prefs.ota_roots = roots;
        self.save_prefs();
        if self.prefs.ota_roots.is_empty() {
            "OK - unsigned updates accepted over BLE, Wi-Fi and USB, none over LoRa".into()
        } else {
            format!("OK - only releases signed by {} are installed", release::join_fingerprints(&self.prefs.ota_roots, ", "))
        }
    }

    /// `set ota.owner <fingerprint>[,<fingerprint>...]` pins owner keys: a release is installed
    /// only once one of them signed it too, so the owner decides which of the root's releases go
    /// onto this node; `set ota.owner off` unpins them. The same rule as for the root: the first
    /// pin may come from an admin, a change only from the serial console.
    fn set_ota_owner(&mut self, serial: bool, value: &str) -> String {
        let value = value.trim();
        let owners = if value == "off" { Some(Vec::new()) } else { release::parse_fingerprints(value) };
        let Some(owners) = owners else {
            return format!("Error: up to {} fingerprints of 64 hex digits, or off", release::MAX_ROOTS);
        };
        if owners == self.prefs.ota_owners {
            return "OK".into();
        }
        if !owners.is_empty() && self.prefs.ota_roots.is_empty() {
            return "Error: pin the root key first (set ota.root)".into();
        }
        if !self.prefs.ota_owners.is_empty() && !serial {
            return "Error: a pinned owner key can only be changed on the serial console".into();
        }
        self.ota.owners = owners.clone();
        self.prefs.ota_owners = owners;
        self.save_prefs();
        if self.prefs.ota_owners.is_empty() {
            "OK - the root's signature is enough".into()
        } else {
            format!("OK - a release must be signed by {} too", release::join_fingerprints(&self.prefs.ota_owners, ", "))
        }
    }

    fn handle_set(&mut self, ctx: &mut MeshCtx, serial: bool, cfg: &str) -> String {
        if let Some(r) = self.handle_set_network(cfg) {
            return r;
        }
        if let Some(v) = cfg.strip_prefix("ota.root ") {
            return self.set_ota_root(serial, v);
        }
        if let Some(v) = cfg.strip_prefix("ota.owner ") {
            return self.set_ota_owner(serial, v);
        }
        if let Some(v) = cfg.strip_prefix("fw.serve ") {
            self.prefs.fw_serve = v.starts_with("on");
            self.save_prefs();
            return match (self.prefs.fw_serve, self.fw_store.is_some()) {
                (true, false) => "OK - but this board has no update store".into(),
                (true, true) => format!("OK - updates from {} handed on to neighbours", self.prefs.fw_server),
                (false, _) => "OK - updates not handed on".into(),
            };
        }
        if let Some(v) = cfg.strip_prefix("fw.auto ") {
            self.prefs.fw_auto = v.starts_with("on");
            self.save_prefs();
            return match (self.prefs.fw_auto, self.prefs.ota_roots.is_empty()) {
                (true, true) => "OK - but nothing is installed until a root is pinned (set ota.root)".into(),
                (true, false) => format!(
                    "OK - this node updates itself from {} (without it, from a neighbour over LoRa), {} releases",
                    self.prefs.fw_server,
                    release::channel_name(self.prefs.fw_channel)
                ),
                (false, _) => "OK - no updates by itself".into(),
            };
        }
        if let Some(v) = cfg.strip_prefix("fw.channel ") {
            let Some(ch) = release::channel_by_name(v.trim()) else {
                return "Error: stable or dev".into();
            };
            self.prefs.fw_channel = ch;
            self.save_prefs();
            return format!("OK - following {} releases", release::channel_name(ch));
        }
        if let Some(v) = cfg.strip_prefix("fw.source ") {
            let v = v.trim();
            let mut key = alloc::vec![0u8; v.len() / 2];
            if v == "auto" {
                key.clear();
            } else if !(2..=64).contains(&v.len()) || v.len() % 2 != 0 || !from_hex(&mut key, v) {
                return "Error: the start of a neighbour's key in hex, or auto".into();
            }
            self.prefs.fw_source = key;
            self.save_prefs();
            return if self.prefs.fw_source.is_empty() {
                "OK - updates from the release server, or from the best neighbour over LoRa without it".into()
            } else {
                format!("OK - updates from {} over LoRa", to_hex(&self.prefs.fw_source))
            };
        }
        if let Some(v) = cfg.strip_prefix("fw.server ") {
            let v = v.trim();
            if v.is_empty() || v.len() > 63 || v.contains(char::is_whitespace) {
                return "Error: a host name, or host:port".into();
            }
            self.prefs.fw_server = v.into();
            self.save_prefs();
            return format!("OK - release server {v}");
        }
        if let Some(v) = cfg.strip_prefix("room ")
            && self.room.is_none()
        {
            self.prefs.room = v.starts_with("on");
            self.save_prefs();
            format!("OK - room server {}, reboot to apply", if self.prefs.room { "on" } else { "off" })
        } else if let Some(r) = self.handle_set_bridge(cfg) {
            r
        } else if let Some(v) = cfg.strip_prefix("dyncr ") {
            self.prefs.dynamic_cr = v.starts_with("on");
            self.save_prefs();
            format!("OK - dynamic coding rate {}", if self.prefs.dynamic_cr { "on" } else { "off" })
        } else if let Some(v) = cfg.strip_prefix("allow.read.only ") {
            self.prefs.allow_read_only = v.starts_with("on");
            self.save_prefs();
            "OK".into()
        } else if let Some(v) = cfg.strip_prefix("flood.advert.interval ") {
            let hours = atoi(v);
            if (hours > 0 && hours < 3) || hours > 168 {
                "Error: interval range is 3-168 hours".into()
            } else {
                self.prefs.flood_advert_interval = hours as u8;
                self.update_flood_advert_timer(ctx);
                self.save_prefs();
                "OK".into()
            }
        } else if let Some(v) = cfg.strip_prefix("advert.interval ") {
            let mins = atoi(v);
            if (mins > 0 && mins < MIN_LOCAL_ADVERT_INTERVAL) || mins > 240 {
                format!("Error: interval range is {MIN_LOCAL_ADVERT_INTERVAL}-240 minutes")
            } else {
                self.prefs.advert_interval = (mins / 2) as u8;
                self.update_advert_timer(ctx);
                self.save_prefs();
                "OK".into()
            }
        } else if let Some(v) = cfg.strip_prefix("guest.password ") {
            self.prefs.guest_password = truncate_str(v, PASSWORD_MAX);
            self.save_prefs();
            "OK".into()
        } else if let Some(v) = cfg.strip_prefix("prv.key ") {
            let mut prv = [0u8; PRV_KEY_SIZE];
            if from_hex(&mut prv, v.trim()) && LocalIdentity::validate_private_key(&prv) {
                let new_id = LocalIdentity::from_private_key(&prv);
                self.effects.save_identity = Some(prv);
                format!("OK, reboot to apply! New pubkey: {}", to_hex(new_id.pub_key()))
            } else {
                "Error, bad key".into()
            }
        } else if let Some(v) = cfg.strip_prefix("name ") {
            if is_valid_name(v) {
                self.prefs.node_name = truncate_str(v, NAME_MAX);
                self.save_prefs();
                "OK".into()
            } else {
                "Error, bad chars".into()
            }
        } else if let Some(v) = cfg.strip_prefix("repeat ") {
            self.prefs.disable_fwd = v.starts_with("off");
            self.save_prefs();
            if self.prefs.disable_fwd { "OK - repeat is now OFF" } else { "OK - repeat is now ON" }.into()
        } else if let Some(v) = cfg.strip_prefix("lat ") {
            self.prefs.node_lat = atof(v);
            self.save_prefs();
            "OK".into()
        } else if let Some(v) = cfg.strip_prefix("lon ") {
            self.prefs.node_lon = atof(v);
            self.save_prefs();
            "OK".into()
        } else if let Some(v) = cfg.strip_prefix("flood.max.unscoped ") {
            self.set_flood_limit(v, |p, m| p.flood_max_unscoped = m)
        } else if let Some(v) = cfg.strip_prefix("flood.max.advert ") {
            self.set_flood_limit(v, |p, m| p.flood_max_advert = m)
        } else if let Some(v) = cfg.strip_prefix("flood.max ") {
            self.set_flood_limit(v, |p, m| p.flood_max = m)
        } else if let Some(v) = cfg.strip_prefix("owner.info ") {
            self.prefs.owner_info = truncate_str(&v.replace('|', "\n"), OWNER_INFO_MAX);
            self.save_prefs();
            "OK".into()
        } else if let Some(v) = cfg.strip_prefix("loop.detect ") {
            let mode = if v.starts_with("off") {
                LOOP_DETECT_OFF
            } else if v.starts_with("minimal") {
                LOOP_DETECT_MINIMAL
            } else if v.starts_with("moderate") {
                LOOP_DETECT_MODERATE
            } else if v.starts_with("strict") {
                LOOP_DETECT_STRICT
            } else {
                return "Error, must be: off, minimal, moderate, or strict".into();
            };
            self.prefs.loop_detect = mode;
            self.save_prefs();
            "OK".into()
        } else if serial && let Some(v) = cfg.strip_prefix("freq ") {
            self.prefs.radio.freq_mhz = atof(v) as f32;
            self.save_prefs();
            "OK - reboot to apply".into()
        } else if cfg.starts_with("adc.multiplier ") {
            "Error: unsupported".into()
        } else {
            format!("unknown config: {cfg}")
        }
    }

    /// One line of `region load`: indented names build the tree; a blank line finishes.
    fn region_load_line(&mut self, line: &str) -> String {
        if line.trim().is_empty() {
            let (map, _) = self.region_load.take().unwrap();
            self.regions = map;
            return format!("OK - loaded {} regions", self.regions.len());
        }
        let name_start = line.len() - line.trim_start_matches(' ').len();
        let rest = &line[name_start..];
        let name_end = rest.bytes().position(|c| !is_name_char(c)).unwrap_or(rest.len());
        let (name, tail) = rest.split_at(name_end);
        let flood = tail.contains('F');
        let old = self.regions.find_by_name(name).cloned();
        let (map, stack) = self.region_load.as_mut().unwrap();
        if (1..8).contains(&name_start)
            && !name.is_empty()
            && let Some(parent) = stack[name_start - 1]
            && let Some(id) = map.put_region(name, parent, old.as_ref().map_or(0, |o| o.id))
        {
            let flags = old.map_or(if flood { 0 } else { REGION_DENY_FLOOD }, |o| o.flags);
            map.entry_mut(id).unwrap().flags = flags;
            stack[name_start] = Some(id);
        }
        String::new()
    }

    /// `CommonCLI::handleRegionCmd`.
    fn handle_region_command(&mut self, ctx: &mut MeshCtx, cmd: &str) -> String {
        let trimmed = cmd.trim();
        // `region def a b|jump c ...`: chain of regions, each the parent of the next
        if let Some(def) = trimmed.strip_prefix("region def").filter(|d| d.is_empty() || d.starts_with(' ')) {
            let def = def.trim();
            if def.is_empty() {
                return "Err - empty def".into();
            }
            let mut cursor = 0u16;
            for tok in def.split_whitespace() {
                let (name, jump) = match tok.split_once(['|', ',']) {
                    Some((n, j)) => (n.trim(), Some(j.trim())),
                    None => (tok, None),
                };
                if name.is_empty() {
                    return "Err - empty name".into();
                }
                if jump == Some("") {
                    return "Err - empty jump".into();
                }
                let Some(id) = self.regions.put_region(name, cursor, 0) else {
                    return format!("Err - put failed: {name}");
                };
                self.regions.entry_mut(id).unwrap().flags = 0;
                cursor = match jump {
                    Some(j) => match self.regions.find_by_name_prefix(j) {
                        Some(r) => r.id,
                        None => return format!("Err - unknown jump: {j}"),
                    },
                    None => id,
                };
            }
            return self.regions.export_tree();
        }

        let parts: Vec<&str> = trimmed.split(' ').filter(|p| !p.is_empty()).collect();
        let arg = |i: usize| parts.get(i).copied().unwrap_or("");
        match (parts.len(), arg(1)) {
            (1, _) => self.regions.export_tree(),
            (_, "load") => {
                let mut stack = [None; 8];
                stack[0] = Some(0);
                self.region_load = Some((RegionMap::deserialize(&self.regions.serialize()).cleared(), stack));
                String::new()
            }
            (_, "save") => {
                self.prefs.discovery_mod_timestamp = ctx.current_time(); // modified, for discovery info
                self.save_prefs();
                self.effects.save_regions = true;
                "OK".into()
            }
            (n, "allowf" | "denyf") if n >= 3 => match self.regions.find_by_name_prefix(arg(2)).map(|r| r.id) {
                Some(id) => {
                    let e = self.regions.entry_mut(id).unwrap();
                    if arg(1) == "allowf" {
                        e.flags &= !REGION_DENY_FLOOD;
                    } else {
                        e.flags |= REGION_DENY_FLOOD;
                    }
                    "OK".into()
                }
                None => "Err - unknown region".into(),
            },
            (n, "get") if n >= 3 => match self.regions.find_by_name_prefix(arg(2)) {
                Some(r) => {
                    let flood = if r.allows_flood() { "F" } else { "" };
                    match self.regions.find_by_id(r.parent).filter(|p| p.id != 0) {
                        Some(p) => format!(" {} ({}) {}", r.name, p.name, flood),
                        None => format!(" {} {}", r.name, flood),
                    }
                }
                None => "Err - unknown region".into(),
            },
            (n, "home") if n >= 3 => match self.regions.find_by_name_prefix(arg(2)).map(|r| (r.id, r.name.clone())) {
                Some((id, name)) => {
                    self.regions.set_home(id);
                    format!(" home is now {name}")
                }
                None => "Err - unknown region".into(),
            },
            (2, "home") => format!(" home is {}", self.regions.home().map_or("*", |r| r.name.as_str())),
            (n, "default") if n >= 3 => {
                if arg(2) == "<null>" {
                    self.regions.set_default(0);
                    self.effects.save_regions = true;
                    return " default scope is now <null>".into();
                }
                let id = match self.regions.find_by_name_prefix(arg(2)) {
                    Some(r) => Some(r.id),
                    None => self.regions.put_region(arg(2), 0, 0), // auto-create
                };
                match id {
                    Some(id) => {
                        self.regions.entry_mut(id).unwrap().flags = 0; // allow flood
                        self.regions.set_default(id);
                        self.effects.save_regions = true;
                        format!(" default scope is now {}", self.regions.find_by_id(id).unwrap().name)
                    }
                    None => "Err - region table full".into(),
                }
            }
            (2, "default") => format!(" default scope is {}", self.regions.default_region().map_or("<null>", |r| r.name.as_str())),
            (n, "put") if n >= 3 => {
                let parent = if n >= 4 { self.regions.find_by_name_prefix(arg(3)).map(|r| r.id) } else { Some(0) };
                match parent {
                    None => "Err - unknown parent".into(),
                    Some(parent) => match self.regions.put_region(arg(2), parent, 0) {
                        Some(id) => {
                            self.regions.entry_mut(id).unwrap().flags = 0; // new default: flood allowed
                            "OK - (flood allowed)".into()
                        }
                        None => "Err - unable to put".into(),
                    },
                }
            }
            (n, "remove") if n >= 3 => match self.regions.find_by_name(arg(2)).map(|r| r.id) {
                Some(id) if self.regions.remove(id) => "OK".into(),
                Some(_) => "Err - not empty".into(),
                None => "Err - not found".into(),
            },
            (n, "list") if n >= 3 => {
                let invert = match arg(2) {
                    "allowed" => false,
                    "denied" => true,
                    _ => return "Err - use 'allowed' or 'denied'".into(),
                };
                let names = self.regions.export_names(REGION_DENY_FLOOD, invert, 160);
                if names.is_empty() { "-none-".into() } else { names }
            }
            _ => "Err - ??".into(),
        }
    }

    /// `set bridge.*` (upstream's), on boards with a bridge.
    fn handle_set_bridge(&mut self, cfg: &str) -> Option<String> {
        self.board.bridge?;
        let (key, value) = cfg.split_once(' ')?;
        let value = value.trim();
        let reply = match key {
            "bridge.enabled" => {
                self.prefs.bridge_enabled = value.starts_with("on");
                if self.prefs.bridge_enabled {
                    self.prefs.powersaving_enabled = false; // the bridge keeps the Wi-Fi radio on
                }
                self.effects.bridge_changed = true;
                "OK - rebooting to apply".into()
            }
            "bridge.delay" => match value.parse::<u16>() {
                Ok(ms) if ms <= 10_000 => {
                    self.prefs.bridge_delay = ms;
                    "OK".into()
                }
                _ => return Some("Error: delay must be between 0-10000 ms".into()),
            },
            // upstream takes "rx" for received packets and anything else for transmitted ones
            "bridge.source" => {
                self.prefs.bridge_source_rx = value.eq_ignore_ascii_case("rx") || value.eq_ignore_ascii_case("logrx");
                "OK".into()
            }
            "bridge.channel" => match value.parse::<u8>() {
                Ok(ch) if (1..15).contains(&ch) => {
                    self.prefs.bridge_channel = ch;
                    self.effects.bridge_changed = true;
                    "OK - rebooting to apply".into()
                }
                _ => return Some("Error: channel must be between 1-14".into()),
            },
            "bridge.secret" => {
                self.prefs.bridge_secret = truncate_str(value, crate::prefs::BRIDGE_SECRET_MAX);
                self.bridge.set_secret(&self.prefs.bridge_secret);
                "OK".into()
            }
            _ => return None,
        };
        self.save_prefs();
        Some(reply)
    }

    /// `get wifi.*` / `get mqtt.*` (MeshCoreTel command set).
    fn handle_get_network(&self, cfg: &str) -> Option<String> {
        let p = &self.prefs;
        let onoff = |b: bool| if b { "on" } else { "off" };
        let r = match cfg.trim() {
            "wifi.ssid" => format!("> {}", p.wifi_ssid),
            "wifi.status" => format!("> {}", self.board.wifi_status),
            "mqtt.status" => format!("> {}", self.board.mqtt_status),
            "mqtt.iata" => format!("> {}", p.mqtt_iata),
            "mqtt.statuscfg" => format!("> {}", onoff(p.mqtt_status)),
            "mqtt.packets" => format!("> {}", onoff(p.mqtt_packets)),
            "mqtt.tx" => format!("> {}", onoff(p.mqtt_tx)),
            "mqtt.raw" => format!("> {}", onoff(p.mqtt_raw)),
            "mqtt.owner" => format!("> {}", p.mqtt_owner),
            "mqtt.email" => format!("> {}", p.mqtt_email),
            "mqtt.client_version" => format!("> {}", crate::FIRMWARE_VERSION),
            other => {
                let name = other.strip_prefix("mqtt.")?.replace('.', "-");
                let bit = MQTT_BROKERS.iter().find(|(n, _)| *n == name)?.1;
                format!("> {}", onoff(p.mqtt_brokers & bit != 0))
            }
        };
        Some(r)
    }

    fn handle_set_network(&mut self, cfg: &str) -> Option<String> {
        const RESTART: &str = "OK - rebooting to apply";
        let (key, value) = cfg.split_once(' ').unwrap_or((cfg, ""));
        let on = value.starts_with("on");
        let p = &mut self.prefs;
        let reply = match key {
            "wifi.ssid" => {
                p.wifi_ssid = truncate_str(value, 33);
                RESTART.into()
            }
            "wifi.pwd" => {
                p.wifi_pwd = truncate_str(value, 65);
                RESTART.into()
            }
            "mqtt.iata" => {
                p.mqtt_iata = crate::uplink::sanitize_iata(value);
                format!("OK - iata {}, rebooting to apply", p.mqtt_iata)
            }
            "mqtt.status" => {
                p.mqtt_status = on;
                self.save_prefs();
                return Some("OK".into());
            }
            "mqtt.packets" => {
                p.mqtt_packets = on;
                self.save_prefs();
                return Some("OK".into());
            }
            "mqtt.tx" => {
                p.mqtt_tx = on;
                self.save_prefs();
                return Some("OK".into());
            }
            "mqtt.raw" => {
                p.mqtt_raw = on;
                self.save_prefs();
                return Some("OK".into());
            }
            "mqtt.owner" => {
                let v = value.trim();
                let mut key = [0u8; 32];
                if !v.is_empty() && !from_hex(&mut key, v) {
                    return Some("Err - owner must be 64 hex chars".into());
                }
                p.mqtt_owner = v.to_ascii_uppercase();
                "OK".into()
            }
            "mqtt.email" => {
                p.mqtt_email = truncate_str(value.trim(), 96);
                "OK".into()
            }
            k => {
                let name = k.strip_prefix("mqtt.")?.replace('.', "-");
                let bit = MQTT_BROKERS.iter().find(|(n, _)| *n == name)?.1;
                if on && p.mqtt_brokers & bit == 0 && p.mqtt_brokers.count_ones() >= 3 {
                    return Some("Err - max 3 mqtt brokers".into());
                }
                if on { p.mqtt_brokers |= bit } else { p.mqtt_brokers &= !bit }
                RESTART.into()
            }
        };
        self.save_prefs();
        self.effects.network_changed = true;
        Some(reply)
    }

    fn set_flood_limit(&mut self, v: &str, apply: impl FnOnce(&mut NodePrefs, u8)) -> String {
        let m = atoi(v);
        if m <= 64 {
            apply(&mut self.prefs, m as u8);
            self.save_prefs();
            "OK".into()
        } else {
            "Error, max 64".into()
        }
    }
}
