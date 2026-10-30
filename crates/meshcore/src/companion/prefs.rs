//! Companion radio preferences (upstream `examples/companion_radio/NodePrefs.h`) and their
//! persistent text form (`key=value` lines, see [`crate::prefs`]).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::prefs::{ADVERT_LOC_NONE, NAME_MAX, escape, unescape};
use crate::radio::RadioParams;
use crate::release::{Fingerprint, join_fingerprints, parse_fingerprints};
use crate::util::{atof, atoi, atoi_signed, from_hex, to_hex, truncate_str};

pub const TELEM_MODE_DENY: u8 = 0;
/// Use the contact's flags.
pub const TELEM_MODE_ALLOW_FLAGS: u8 = 1;
pub const TELEM_MODE_ALLOW_ALL: u8 = 2;

/// Auto-add bits: overwrite the oldest non-favourite contact when full, then the contact types to
/// add when `manual_add_contacts` is on.
pub const AUTO_ADD_OVERWRITE_OLDEST: u8 = 1 << 0;
pub const AUTO_ADD_CHAT: u8 = 1 << 1;
pub const AUTO_ADD_REPEATER: u8 = 1 << 2;
pub const AUTO_ADD_ROOM_SERVER: u8 = 1 << 3;
pub const AUTO_ADD_SENSOR: u8 = 1 << 4;

pub const SCOPE_NAME_MAX: usize = 31;

#[derive(Clone, Debug, PartialEq)]
pub struct CompanionPrefs {
    pub airtime_factor: f32,
    pub node_name: String,
    pub node_lat: f64,
    pub node_lon: f64,
    pub radio: RadioParams,
    pub tx_power_dbm: i8,
    pub multi_acks: u8,
    pub manual_add_contacts: u8,
    pub telemetry_mode_base: u8,
    pub telemetry_mode_loc: u8,
    pub telemetry_mode_env: u8,
    pub rx_delay_base: f32,
    pub tx_delay_factor: f32,
    pub direct_tx_delay_factor: f32,
    /// BLE pairing PIN; 0 = the firmware picks one.
    pub ble_pin: u32,
    pub advert_loc_policy: u8,
    pub autoadd_config: u8,
    /// 0 = no limit, 1 = direct (0 hops), N = up to N-1 hops.
    pub autoadd_max_hops: u8,
    pub rx_boosted_gain: bool,
    pub radio_fem_rxgain: bool,
    pub path_hash_mode: u8,
    pub cad_enabled: bool,
    pub interference_threshold: u8,
    /// AGC reset interval in seconds / 4; 0 = off.
    pub agc_reset_interval: u8,
    /// Client repeat mode off (the default).
    pub disable_fwd: bool,
    pub default_scope_name: String,
    pub default_scope_key: [u8; 16],
    pub tz_offset: i8,
    /// Wi-Fi station for the TCP companion interface.
    pub wifi_ssid: String,
    pub wifi_pwd: String,
    pub wifi_enabled: bool,
    /// The GPS receiver on (boards that have one); off by default, as upstream.
    pub gps_enabled: bool,
    /// Seconds between GPS position updates; 0 = every second.
    pub gps_interval: u32,
    /// Round-trip manual paths (`set rtpath on`): a path the app sets for a contact also goes to
    /// the contact, reversed, so its replies come back direct. Off by default.
    pub round_trip_path: bool,
    /// Fingerprints of the root keys whose signed releases this device installs (see
    /// [`crate::release`]); none = unsigned updates are accepted.
    pub ota_roots: Vec<Fingerprint>,
    /// Fingerprints of the owner keys one of which must sign a release too (custom variable
    /// `ota_owner`); none = the root's signature is enough.
    pub ota_owners: Vec<Fingerprint>,
    /// Pull updates from a neighbouring repeater (`fw_pull`, see [`crate::fwshare`]): off, on
    /// (installed when the user says so) or auto. Only with a pinned root. Off by default.
    pub fw_pull: u8,
    /// Public key prefix of the repeater to ask; empty = the best one heard directly.
    pub fw_source: Vec<u8>,
    /// The release channel followed (`release::channel`): stable by default.
    pub fw_channel: u8,
}

impl Default for CompanionPrefs {
    /// Defaults of upstream `companion_radio` built for Heltec V4.
    fn default() -> Self {
        Self {
            airtime_factor: 1.0,
            node_name: String::new(), // the key prefix in hex, set at start-up
            node_lat: 0.0,
            node_lon: 0.0,
            radio: RadioParams::default(),
            tx_power_dbm: 10, // Heltec V4: SX1262 at 10 dBm -> ~22 dBm after the FEM PA
            multi_acks: 0,
            manual_add_contacts: 0,
            telemetry_mode_base: TELEM_MODE_DENY,
            telemetry_mode_loc: TELEM_MODE_DENY,
            telemetry_mode_env: TELEM_MODE_DENY,
            rx_delay_base: 0.0,
            tx_delay_factor: 0.5,
            direct_tx_delay_factor: 0.2,
            ble_pin: 0,
            advert_loc_policy: ADVERT_LOC_NONE,
            autoadd_config: 0,
            autoadd_max_hops: 0,
            rx_boosted_gain: true,
            radio_fem_rxgain: true,
            path_hash_mode: 0,
            cad_enabled: false,
            interference_threshold: 0,
            agc_reset_interval: 0,
            disable_fwd: true,
            default_scope_name: String::new(),
            default_scope_key: [0; 16],
            tz_offset: 0,
            wifi_ssid: String::new(),
            wifi_pwd: String::new(),
            wifi_enabled: true,
            gps_enabled: false,
            gps_interval: 0,
            round_trip_path: false,
            ota_roots: crate::release::default_roots(),
            ota_owners: Vec::new(),
            fw_pull: 0,
            fw_source: Vec::new(),
            fw_channel: 0,
        }
    }
}

impl CompanionPrefs {
    /// Clamp values into the ranges upstream accepts (`MyMesh::begin`).
    pub fn sanitize(&mut self, max_tx_power: i8) {
        self.rx_delay_base = self.rx_delay_base.clamp(0.0, 20.0);
        self.tx_delay_factor = self.tx_delay_factor.clamp(0.0, 2.0);
        self.direct_tx_delay_factor = self.direct_tx_delay_factor.clamp(0.0, 2.0);
        self.airtime_factor = self.airtime_factor.clamp(0.0, 9.0);
        self.radio.freq_mhz = self.radio.freq_mhz.clamp(150.0, 2500.0);
        self.radio.bw_khz = self.radio.bw_khz.clamp(7.8, 500.0);
        self.radio.sf = self.radio.sf.clamp(5, 12);
        self.radio.cr = self.radio.cr.clamp(5, 8);
        self.tx_power_dbm = self.tx_power_dbm.clamp(-9, max_tx_power);
        self.path_hash_mode = self.path_hash_mode.min(2);
        self.autoadd_max_hops = self.autoadd_max_hops.min(64);
        self.node_name = truncate_str(&self.node_name, NAME_MAX);
        self.default_scope_name = truncate_str(&self.default_scope_name, SCOPE_NAME_MAX);
        self.gps_interval = self.gps_interval.min(86_400);
    }

    pub fn is_repeat_enabled(&self) -> bool {
        !self.disable_fwd
    }

    pub fn serialize(&self) -> String {
        let mut s = String::new();
        let b = |v: bool| if v { 1 } else { 0 };
        let _ = writeln!(s, "af={}", self.airtime_factor);
        let _ = writeln!(s, "name={}", escape(&self.node_name));
        let _ = writeln!(s, "lat={}", self.node_lat);
        let _ = writeln!(s, "lon={}", self.node_lon);
        let _ = writeln!(s, "freq={}", self.radio.freq_mhz);
        let _ = writeln!(s, "bw={}", self.radio.bw_khz);
        let _ = writeln!(s, "sf={}", self.radio.sf);
        let _ = writeln!(s, "cr={}", self.radio.cr);
        let _ = writeln!(s, "tx={}", self.tx_power_dbm);
        let _ = writeln!(s, "multi_ack={}", self.multi_acks);
        let _ = writeln!(s, "man_add={}", self.manual_add_contacts);
        let _ = writeln!(s, "tel_base={}", self.telemetry_mode_base);
        let _ = writeln!(s, "tel_loc={}", self.telemetry_mode_loc);
        let _ = writeln!(s, "tel_env={}", self.telemetry_mode_env);
        let _ = writeln!(s, "rxdelay={}", self.rx_delay_base);
        let _ = writeln!(s, "f_txdelay={}", self.tx_delay_factor);
        let _ = writeln!(s, "d_txdelay={}", self.direct_tx_delay_factor);
        let _ = writeln!(s, "pin={}", self.ble_pin);
        let _ = writeln!(s, "adv_loc={}", self.advert_loc_policy);
        let _ = writeln!(s, "auto_add={}", self.autoadd_config);
        let _ = writeln!(s, "auto_max={}", self.autoadd_max_hops);
        let _ = writeln!(s, "rxgain={}", b(self.rx_boosted_gain));
        let _ = writeln!(s, "fem_rxgain={}", b(self.radio_fem_rxgain));
        let _ = writeln!(s, "hash_mode={}", self.path_hash_mode);
        let _ = writeln!(s, "cad={}", b(self.cad_enabled));
        let _ = writeln!(s, "int_thr={}", self.interference_threshold);
        let _ = writeln!(s, "agc_int={}", self.agc_reset_interval);
        let _ = writeln!(s, "disable_fwd={}", b(self.disable_fwd));
        let _ = writeln!(s, "defs_nm={}", escape(&self.default_scope_name));
        let _ = writeln!(s, "defs_key={}", to_hex(&self.default_scope_key));
        let _ = writeln!(s, "tz_offset={}", self.tz_offset);
        let _ = writeln!(s, "wifi_ssid={}", escape(&self.wifi_ssid));
        let _ = writeln!(s, "wifi_pwd={}", escape(&self.wifi_pwd));
        let _ = writeln!(s, "wifi_enabled={}", b(self.wifi_enabled));
        let _ = writeln!(s, "gps={}", b(self.gps_enabled));
        let _ = writeln!(s, "gps_int={}", self.gps_interval);
        let _ = writeln!(s, "rt_path={}", b(self.round_trip_path));
        let _ = writeln!(s, "ota_root={}", join_fingerprints(&self.ota_roots, ","));
        let _ = writeln!(s, "ota_owner={}", join_fingerprints(&self.ota_owners, ","));
        let _ = writeln!(s, "fw_pull={}", self.fw_pull);
        let _ = writeln!(s, "fw_source={}", crate::util::to_hex(&self.fw_source));
        let _ = writeln!(s, "fw_channel={}", self.fw_channel);
        s
    }

    pub fn deserialize(text: &str, max_tx_power: i8) -> Self {
        let mut p = Self::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let u8v = || atoi(v).min(255) as u8;
            let flag = || atoi(v) != 0;
            match k {
                "af" => p.airtime_factor = atof(v) as f32,
                "name" => p.node_name = unescape(v),
                "lat" => p.node_lat = atof(v),
                "lon" => p.node_lon = atof(v),
                "freq" => p.radio.freq_mhz = atof(v) as f32,
                "bw" => p.radio.bw_khz = atof(v) as f32,
                "sf" => p.radio.sf = u8v(),
                "cr" => p.radio.cr = u8v(),
                "tx" => p.tx_power_dbm = atoi_signed(v).clamp(-128, 127) as i8,
                "multi_ack" => p.multi_acks = u8v(),
                "man_add" => p.manual_add_contacts = u8v(),
                "tel_base" => p.telemetry_mode_base = u8v(),
                "tel_loc" => p.telemetry_mode_loc = u8v(),
                "tel_env" => p.telemetry_mode_env = u8v(),
                "rxdelay" => p.rx_delay_base = atof(v) as f32,
                "f_txdelay" => p.tx_delay_factor = atof(v) as f32,
                "d_txdelay" => p.direct_tx_delay_factor = atof(v) as f32,
                "pin" => p.ble_pin = atoi(v),
                "adv_loc" => p.advert_loc_policy = u8v(),
                "auto_add" => p.autoadd_config = u8v(),
                "auto_max" => p.autoadd_max_hops = u8v(),
                "rxgain" => p.rx_boosted_gain = flag(),
                "fem_rxgain" => p.radio_fem_rxgain = flag(),
                "hash_mode" => p.path_hash_mode = u8v(),
                "cad" => p.cad_enabled = flag(),
                "int_thr" => p.interference_threshold = u8v(),
                "agc_int" => p.agc_reset_interval = u8v(),
                "disable_fwd" => p.disable_fwd = flag(),
                "defs_nm" => p.default_scope_name = unescape(v),
                "defs_key" => {
                    let mut key = [0u8; 16];
                    if from_hex(&mut key, v) {
                        p.default_scope_key = key;
                    }
                }
                "tz_offset" => p.tz_offset = atoi_signed(v).clamp(-12, 14) as i8,
                "wifi_ssid" => p.wifi_ssid = unescape(v),
                "wifi_pwd" => p.wifi_pwd = unescape(v),
                "wifi_enabled" => p.wifi_enabled = flag(),
                "gps" => p.gps_enabled = flag(),
                "gps_int" => p.gps_interval = atoi(v),
                "rt_path" => p.round_trip_path = flag(),
                "ota_root" => p.ota_roots = parse_fingerprints(v).unwrap_or_default(),
                "ota_owner" => p.ota_owners = parse_fingerprints(v).unwrap_or_default(),
                "fw_pull" => p.fw_pull = u8v().min(2),
                "fw_channel" => p.fw_channel = u8v().min(1),
                "fw_source" => {
                    let mut key = alloc::vec![0u8; (v.len() / 2).min(32)];
                    let hex = &v[..key.len() * 2];
                    if from_hex(&mut key, hex) {
                        p.fw_source = key;
                    }
                }
                _ => {}
            }
        }
        p.sanitize(max_tx_power);
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut p = CompanionPrefs {
            node_name: "Карман \\ 2".into(),
            default_scope_name: "omsk".into(),
            default_scope_key: [7; 16],
            tz_offset: -5,
            ..Default::default()
        };
        p.radio.freq_mhz = 869.525;
        p.tx_power_dbm = -3;
        assert_eq!(p, CompanionPrefs::deserialize(&p.serialize(), 22));
    }
}
