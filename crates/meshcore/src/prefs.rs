//! Node preferences (`NodePrefs`) and their persistent text form (`key=value` lines).
//!
//! The text form is forward/backward tolerant: unknown keys are ignored and missing keys keep
//! their defaults, so fields can be added without a migration.

use alloc::string::String;
use core::fmt::Write;

use crate::radio::RadioParams;
use crate::util::{atof, atoi, atoi_signed, truncate_str};

pub const ADVERT_LOC_NONE: u8 = 0;
pub const ADVERT_LOC_SHARE: u8 = 1;
pub const ADVERT_LOC_PREFS: u8 = 2;

pub const LOOP_DETECT_OFF: u8 = 0;
pub const LOOP_DETECT_MINIMAL: u8 = 1;
pub const LOOP_DETECT_MODERATE: u8 = 2;
pub const LOOP_DETECT_STRICT: u8 = 3;

pub const NAME_MAX: usize = 32;
pub const PASSWORD_MAX: usize = 16;
pub const OWNER_INFO_MAX: usize = 120;

#[derive(Clone, Debug, PartialEq)]
pub struct NodePrefs {
    pub airtime_factor: f32,
    pub node_name: String,
    pub node_lat: f64,
    pub node_lon: f64,
    pub password: String,
    pub guest_password: String,
    pub radio: RadioParams,
    pub tx_power_dbm: i8,
    pub disable_fwd: bool,
    /// Local (zero-hop) advert interval in minutes / 2; 0 = off.
    pub advert_interval: u8,
    /// Flood advert interval in hours; 0 = off.
    pub flood_advert_interval: u8,
    pub rx_delay_base: f32,
    pub tx_delay_factor: f32,
    pub direct_tx_delay_factor: f32,
    pub allow_read_only: bool,
    pub multi_acks: u8,
    pub flood_max: u8,
    pub flood_max_unscoped: u8,
    pub flood_max_advert: u8,
    pub interference_threshold: u8,
    /// AGC reset interval in seconds / 4; 0 = off.
    pub agc_reset_interval: u8,
    pub owner_info: String,
    pub rx_boosted_gain: bool,
    pub radio_fem_rxgain: bool,
    pub path_hash_mode: u8,
    pub loop_detect: u8,
    pub cad_enabled: bool,
    pub advert_loc_policy: u8,
    pub discovery_mod_timestamp: u32,
    pub powersaving_enabled: bool,
    /// Pick the coding rate per hop from the link SNR (direct packets to known neighbours).
    pub dynamic_cr: bool,
    /// Wi-Fi station (MQTT observer, LAN OTA).
    pub wifi_ssid: String,
    pub wifi_pwd: String,
    /// MQTT observer: region code in topics; `UNSET` disables publishing.
    pub mqtt_iata: String,
    /// Enabled brokers, bits as in the C++ firmware: 1 meshcoretel, 2/4 letsmesh eu/us, 8 ommesh.
    pub mqtt_brokers: u8,
    pub mqtt_packets: bool,
    pub mqtt_tx: bool,
    /// Also publish every packet as a RAW message (`.../raw`).
    pub mqtt_raw: bool,
    pub mqtt_status: bool,
    pub mqtt_owner: String,
    pub mqtt_email: String,
    /// Also host a room server on this device (a second node with its own identity).
    pub room: bool,
}

impl Default for NodePrefs {
    /// Defaults of upstream `simple_repeater` built for Heltec V4.
    fn default() -> Self {
        Self {
            airtime_factor: 1.0,
            node_name: "Rust Repeater".into(),
            node_lat: 0.0,
            node_lon: 0.0,
            password: "password".into(),
            guest_password: String::new(),
            radio: RadioParams::default(),
            tx_power_dbm: 10, // Heltec V4: SX1262 at 10 dBm -> ~22 dBm after the FEM PA
            disable_fwd: false,
            advert_interval: 1, // 2 minutes for new installs (turned off on first manual config)
            flood_advert_interval: 47,
            rx_delay_base: 0.0,
            tx_delay_factor: 0.5,
            direct_tx_delay_factor: 0.3,
            allow_read_only: false,
            multi_acks: 0,
            flood_max: 64,
            flood_max_unscoped: 64,
            flood_max_advert: 8,
            interference_threshold: 0,
            agc_reset_interval: 0,
            owner_info: String::new(),
            rx_boosted_gain: true,
            radio_fem_rxgain: true,
            path_hash_mode: 0,
            loop_detect: LOOP_DETECT_MINIMAL,
            cad_enabled: false,
            advert_loc_policy: ADVERT_LOC_PREFS,
            discovery_mod_timestamp: 0,
            powersaving_enabled: false,
            dynamic_cr: false,
            wifi_ssid: String::new(),
            wifi_pwd: String::new(),
            mqtt_iata: "UNSET".into(),
            mqtt_brokers: 0x01,
            mqtt_packets: true,
            mqtt_tx: false,
            mqtt_raw: false,
            mqtt_status: true,
            mqtt_owner: String::new(),
            mqtt_email: String::new(),
            room: false,
        }
    }
}

pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

pub(crate) fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl NodePrefs {
    /// Clamp values into the ranges upstream accepts.
    pub fn sanitize(&mut self) {
        self.rx_delay_base = self.rx_delay_base.clamp(0.0, 20.0);
        self.tx_delay_factor = self.tx_delay_factor.clamp(0.0, 2.0);
        self.direct_tx_delay_factor = self.direct_tx_delay_factor.clamp(0.0, 2.0);
        self.airtime_factor = self.airtime_factor.clamp(0.0, 9.0);
        self.radio.freq_mhz = self.radio.freq_mhz.clamp(150.0, 2500.0);
        self.radio.bw_khz = self.radio.bw_khz.clamp(7.8, 500.0);
        self.radio.sf = self.radio.sf.clamp(5, 12);
        self.radio.cr = self.radio.cr.clamp(5, 8);
        self.tx_power_dbm = self.tx_power_dbm.clamp(-9, 22);
        self.multi_acks = self.multi_acks.min(1);
        self.path_hash_mode = self.path_hash_mode.min(2);
        self.advert_loc_policy = self.advert_loc_policy.min(2);
        self.loop_detect = self.loop_detect.min(3);
        self.flood_max = self.flood_max.min(64);
        self.flood_max_unscoped = self.flood_max_unscoped.min(64);
        self.flood_max_advert = self.flood_max_advert.min(64);
        self.node_name = truncate_str(&self.node_name, NAME_MAX);
        self.password = truncate_str(&self.password, PASSWORD_MAX);
        self.guest_password = truncate_str(&self.guest_password, PASSWORD_MAX);
        self.owner_info = truncate_str(&self.owner_info, OWNER_INFO_MAX);
    }

    pub fn serialize(&self) -> String {
        let mut s = String::new();
        let b = |v: bool| if v { 1 } else { 0 };
        let _ = writeln!(s, "af={}", self.airtime_factor);
        let _ = writeln!(s, "name={}", escape(&self.node_name));
        let _ = writeln!(s, "lat={}", self.node_lat);
        let _ = writeln!(s, "lon={}", self.node_lon);
        let _ = writeln!(s, "password={}", escape(&self.password));
        let _ = writeln!(s, "guest_password={}", escape(&self.guest_password));
        let _ = writeln!(s, "freq={}", self.radio.freq_mhz);
        let _ = writeln!(s, "bw={}", self.radio.bw_khz);
        let _ = writeln!(s, "sf={}", self.radio.sf);
        let _ = writeln!(s, "cr={}", self.radio.cr);
        let _ = writeln!(s, "tx={}", self.tx_power_dbm);
        let _ = writeln!(s, "disable_fwd={}", b(self.disable_fwd));
        let _ = writeln!(s, "advert_interval={}", self.advert_interval);
        let _ = writeln!(s, "flood_advert_interval={}", self.flood_advert_interval);
        let _ = writeln!(s, "rxdelay={}", self.rx_delay_base);
        let _ = writeln!(s, "f_txdelay={}", self.tx_delay_factor);
        let _ = writeln!(s, "d_txdelay={}", self.direct_tx_delay_factor);
        let _ = writeln!(s, "allow_read_only={}", b(self.allow_read_only));
        let _ = writeln!(s, "multi_ack={}", self.multi_acks);
        let _ = writeln!(s, "flood_max={}", self.flood_max);
        let _ = writeln!(s, "flood_max_unscoped={}", self.flood_max_unscoped);
        let _ = writeln!(s, "flood_max_advert={}", self.flood_max_advert);
        let _ = writeln!(s, "int_thr={}", self.interference_threshold);
        let _ = writeln!(s, "agc_int={}", self.agc_reset_interval);
        let _ = writeln!(s, "owner_info={}", escape(&self.owner_info));
        let _ = writeln!(s, "rxgain={}", b(self.rx_boosted_gain));
        let _ = writeln!(s, "fem_rxgain={}", b(self.radio_fem_rxgain));
        let _ = writeln!(s, "hash_mode={}", self.path_hash_mode);
        let _ = writeln!(s, "loop_detect={}", self.loop_detect);
        let _ = writeln!(s, "cad={}", b(self.cad_enabled));
        let _ = writeln!(s, "advert_loc_policy={}", self.advert_loc_policy);
        let _ = writeln!(s, "discovery_mod_ts={}", self.discovery_mod_timestamp);
        let _ = writeln!(s, "powersaving={}", b(self.powersaving_enabled));
        let _ = writeln!(s, "dyn_cr={}", b(self.dynamic_cr));
        let _ = writeln!(s, "wifi_ssid={}", escape(&self.wifi_ssid));
        let _ = writeln!(s, "wifi_pwd={}", escape(&self.wifi_pwd));
        let _ = writeln!(s, "mqtt_iata={}", self.mqtt_iata);
        let _ = writeln!(s, "mqtt_brokers={}", self.mqtt_brokers);
        let _ = writeln!(s, "mqtt_packets={}", b(self.mqtt_packets));
        let _ = writeln!(s, "mqtt_tx={}", b(self.mqtt_tx));
        let _ = writeln!(s, "mqtt_raw={}", b(self.mqtt_raw));
        let _ = writeln!(s, "room={}", b(self.room));
        let _ = writeln!(s, "mqtt_status={}", b(self.mqtt_status));
        let _ = writeln!(s, "mqtt_owner={}", self.mqtt_owner);
        let _ = writeln!(s, "mqtt_email={}", escape(&self.mqtt_email));
        s
    }

    pub fn deserialize(text: &str) -> Self {
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
                "password" => p.password = unescape(v),
                "guest_password" => p.guest_password = unescape(v),
                "freq" => p.radio.freq_mhz = atof(v) as f32,
                "bw" => p.radio.bw_khz = atof(v) as f32,
                "sf" => p.radio.sf = u8v(),
                "cr" => p.radio.cr = u8v(),
                "tx" => p.tx_power_dbm = atoi_signed(v).clamp(-128, 127) as i8,
                "disable_fwd" => p.disable_fwd = flag(),
                "advert_interval" => p.advert_interval = u8v(),
                "flood_advert_interval" => p.flood_advert_interval = u8v(),
                "rxdelay" => p.rx_delay_base = atof(v) as f32,
                "f_txdelay" => p.tx_delay_factor = atof(v) as f32,
                "d_txdelay" => p.direct_tx_delay_factor = atof(v) as f32,
                "allow_read_only" => p.allow_read_only = flag(),
                "multi_ack" => p.multi_acks = u8v(),
                "flood_max" => p.flood_max = u8v(),
                "flood_max_unscoped" => p.flood_max_unscoped = u8v(),
                "flood_max_advert" => p.flood_max_advert = u8v(),
                "int_thr" => p.interference_threshold = u8v(),
                "agc_int" => p.agc_reset_interval = u8v(),
                "owner_info" => p.owner_info = unescape(v),
                "rxgain" => p.rx_boosted_gain = flag(),
                "fem_rxgain" => p.radio_fem_rxgain = flag(),
                "hash_mode" => p.path_hash_mode = u8v(),
                "loop_detect" => p.loop_detect = u8v(),
                "cad" => p.cad_enabled = flag(),
                "advert_loc_policy" => p.advert_loc_policy = u8v(),
                "discovery_mod_ts" => p.discovery_mod_timestamp = atoi(v),
                "powersaving" => p.powersaving_enabled = flag(),
                "dyn_cr" => p.dynamic_cr = flag(),
                "wifi_ssid" => p.wifi_ssid = unescape(v),
                "wifi_pwd" => p.wifi_pwd = unescape(v),
                "mqtt_iata" => p.mqtt_iata = v.into(),
                "mqtt_brokers" => p.mqtt_brokers = u8v(),
                "mqtt_packets" => p.mqtt_packets = flag(),
                "mqtt_tx" => p.mqtt_tx = flag(),
                "mqtt_raw" => p.mqtt_raw = flag(),
                "room" => p.room = flag(),
                "mqtt_status" => p.mqtt_status = flag(),
                "mqtt_owner" => p.mqtt_owner = v.into(),
                "mqtt_email" => p.mqtt_email = unescape(v),
                _ => {}
            }
        }
        p.sanitize();
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut p =
            NodePrefs { node_name: "Крыша \\ 1".into(), owner_info: "line1\nline2".into(), ..Default::default() };
        p.radio.freq_mhz = 868.731;
        p.tx_power_dbm = -3;
        let q = NodePrefs::deserialize(&p.serialize());
        assert_eq!(p, q);
    }

    #[test]
    fn missing_keys_use_defaults() {
        let q = NodePrefs::deserialize("name=X\nbogus=1\n");
        assert_eq!(q.node_name, "X");
        assert_eq!(q.flood_max, 64);
    }
}
