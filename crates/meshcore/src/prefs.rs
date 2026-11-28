//! Node preferences (`NodePrefs`) and their persistent text form (`key=value` lines).
//!
//! The text form is forward/backward tolerant: unknown keys are ignored and missing keys keep
//! their defaults, so fields can be added without a migration.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::radio::RadioParams;
use crate::release::{Fingerprint, join_fingerprints, parse_fingerprints};
use crate::util::{Shortest, atof, atoi, atoi_signed, truncate_str};

pub const ADVERT_LOC_NONE: u8 = 0;
pub const ADVERT_LOC_SHARE: u8 = 1;
pub const ADVERT_LOC_PREFS: u8 = 2;

pub const LOOP_DETECT_OFF: u8 = 0;
pub const LOOP_DETECT_MINIMAL: u8 = 1;
pub const LOOP_DETECT_MODERATE: u8 = 2;
pub const LOOP_DETECT_STRICT: u8 = 3;

/// How a board's screen wakes (`set screen auto|manual|off`, the `screen` custom variable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ScreenMode {
    /// Wake on boot, button press, new messages (companion) and update window.
    #[default]
    Auto = 0,
    /// Wake only on button press (and BLE pairing PIN / power off); incoming messages do not light it.
    Manual = 1,
    /// Keep the screen off (except BLE pairing PIN while pairing).
    Off = 2,
}

impl ScreenMode {
    pub fn name(self) -> &'static str {
        match self {
            ScreenMode::Auto => "auto",
            ScreenMode::Manual => "manual",
            ScreenMode::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" | "on" | "1" => Some(ScreenMode::Auto),
            "manual" => Some(ScreenMode::Manual),
            "off" | "0" => Some(ScreenMode::Off),
            _ => None,
        }
    }

    pub fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(v: u8) -> Self {
        match v {
            1 => ScreenMode::Manual,
            2 => ScreenMode::Off,
            _ => ScreenMode::Auto,
        }
    }
}

pub const NAME_MAX: usize = 32;
pub const PASSWORD_MAX: usize = 16;
pub const OWNER_INFO_MAX: usize = 120;
/// Upstream keeps the bridge secret in 16 bytes with the terminating zero.
pub const BRIDGE_SECRET_MAX: usize = 15;

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
    /// A GPS receiver (`gps on`/`off`) is powered; `advert_loc_policy` decides if adverts carry the
    /// position. Only boards with a receiver (`BoardInfo::has_gps`) act on it.
    pub gps_enabled: bool,
    /// Battery reading calibration (`set adc.multiplier`, upstream `adc_mult`) on boards that take
    /// one; 0 = the board's own.
    pub adc_multiplier: f32,
    /// Pick the coding rate per hop from the link SNR (direct packets to known neighbours).
    pub dynamic_cr: bool,
    /// Wi-Fi station (MQTT observer, LAN OTA).
    #[cfg(feature = "net")]
    pub wifi_ssid: String,
    #[cfg(feature = "net")]
    pub wifi_pwd: String,
    /// `set wifi.enabled off` keeps the station down without forgetting the network.
    #[cfg(feature = "net")]
    pub wifi_enabled: bool,
    /// MQTT observer: region code in topics; `UNSET` disables publishing.
    #[cfg(feature = "net")]
    pub mqtt_iata: String,
    /// Enabled brokers, bits as in the C++ firmware: 1 meshcoretel, 8 ommesh (2/4 letsmesh eu/us
    /// were wss-only and dropped with TLS; the bits stay reserved).
    #[cfg(feature = "net")]
    pub mqtt_brokers: u8,
    #[cfg(feature = "net")]
    pub mqtt_packets: bool,
    #[cfg(feature = "net")]
    pub mqtt_tx: bool,
    /// Also publish every packet as a RAW message (`.../raw`).
    #[cfg(feature = "net")]
    pub mqtt_raw: bool,
    #[cfg(feature = "net")]
    pub mqtt_status: bool,
    #[cfg(feature = "net")]
    pub mqtt_owner: String,
    #[cfg(feature = "net")]
    pub mqtt_email: String,
    /// Also host a room server on this device (a second node with its own identity).
    pub room: bool,
    /// Bridge to other repeaters over ESP-NOW (upstream `bridge.*`, see [`crate::bridge`]).
    #[cfg(feature = "net")]
    pub bridge_enabled: bool,
    /// Packets from the bridge are processed this many milliseconds after they come in.
    #[cfg(feature = "net")]
    pub bridge_delay: u16,
    /// Bridge the packets this node receives (`bridge.source rx`), not those it transmits.
    #[cfg(feature = "net")]
    pub bridge_source_rx: bool,
    /// Wi-Fi channel of the ESP-NOW bridge (the router's while the Wi-Fi station runs).
    #[cfg(feature = "net")]
    pub bridge_channel: u8,
    /// Shared by the bridged repeaters.
    #[cfg(feature = "net")]
    pub bridge_secret: String,
    /// Fingerprints of the root keys whose signed releases this node installs (see
    /// [`crate::release`]); none = unsigned updates are accepted.
    pub ota_roots: Vec<Fingerprint>,
    /// Fingerprints of the owner keys one of which must sign a release too (`set fw.owner`);
    /// none = the root's signature is enough.
    pub ota_owners: Vec<Fingerprint>,
    /// Hand firmware updates on to the nodes next to this one (`set fw.serve on`, see
    /// [`crate::fwshare`]), fetched from `fw_server` over the Wi-Fi link. Off by default.
    #[cfg(feature = "net")]
    pub fw_serve: bool,
    /// Host of the release server (plain HTTP: what it serves is signed).
    #[cfg(feature = "net")]
    pub fw_server: String,
    /// What this node does about updates by itself (`set fw.auto`, `crate::fwstate::mode`): only
    /// with a pinned root. Off by default.
    pub fw_mode: u8,
    /// The release channel followed (`release::channel`): stable by default.
    pub fw_channel: u8,
    /// Where updates come from without a working link to `fw_server`: the start of a
    /// neighbouring repeater's key (`set fw.source`), asked over LoRa; empty = the best neighbour
    /// heard directly that answers.
    pub fw_source: Vec<u8>,
    /// What this node does about network time (`set time.sync`, `crate::timesync::Mode`): it
    /// takes beacons by default, and sends nothing.
    pub time_sync: u8,
    /// Pass the time server's signed time on (`set time.serve on`): beacons, and answers to
    /// requests, fetched from `time_server` over the Wi-Fi link. Off by default.
    #[cfg(feature = "net")]
    pub time_serve: bool,
    /// Host of the time server (plain HTTP: what it serves is signed).
    #[cfg(feature = "net")]
    pub time_server: String,
    /// Beacons a day a gateway sends (`set time.beacon`); 0 = answers to requests only.
    #[cfg(feature = "net")]
    pub time_beacons: u8,
    /// The time server's keys (`set time.key`, on the serial console only); none = the built-in
    /// ones (`crate::timesync::TIME_KEYS`).
    pub time_keys: Vec<[u8; 32]>,
    /// The gateway that last answered, asked first after a boot
    /// (`crate::timesync::TimeSync::take_remembered`).
    pub time_gateway: String,
    /// The speaker is silent (`set buzzer off`): the board still has it, and marks nothing with
    /// it. On by default: a board may have no speaker soldered, and nobody is woken by a chirp
    /// they did not ask for.
    pub buzzer_quiet: bool,
    /// How the screen wakes (`set screen auto|manual|off`, [`ScreenMode`]).
    pub screen_mode: u8,
    /// Software-controlled status/TX LEDs enabled (`set led on|off`); on by default.
    pub led_enabled: bool,
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
            radio_fem_rxgain: false, // the V4.3 KCT8103L LNA hears better bypassed (#6); upstream: on
            path_hash_mode: 0,
            loop_detect: LOOP_DETECT_MINIMAL,
            cad_enabled: false,
            advert_loc_policy: ADVERT_LOC_PREFS,
            discovery_mod_timestamp: 0,
            powersaving_enabled: false,
            gps_enabled: false,
            adc_multiplier: 0.0,
            dynamic_cr: false,
            #[cfg(feature = "net")]
            wifi_ssid: String::new(),
            #[cfg(feature = "net")]
            wifi_pwd: String::new(),
            #[cfg(feature = "net")]
            wifi_enabled: true,
            #[cfg(feature = "net")]
            mqtt_iata: "UNSET".into(),
            #[cfg(feature = "net")]
            mqtt_brokers: 0x01,
            #[cfg(feature = "net")]
            mqtt_packets: true,
            #[cfg(feature = "net")]
            mqtt_tx: false,
            #[cfg(feature = "net")]
            mqtt_raw: false,
            #[cfg(feature = "net")]
            mqtt_status: true,
            #[cfg(feature = "net")]
            mqtt_owner: String::new(),
            #[cfg(feature = "net")]
            mqtt_email: String::new(),
            room: false,
            // upstream's bridge defaults, but off: bridging is a build of its own there
            #[cfg(feature = "net")]
            bridge_enabled: false,
            #[cfg(feature = "net")]
            bridge_delay: 500,
            #[cfg(feature = "net")]
            bridge_source_rx: false,
            #[cfg(feature = "net")]
            bridge_channel: 1,
            #[cfg(feature = "net")]
            bridge_secret: "LVSITANOS".into(),
            ota_roots: crate::release::default_roots(),
            ota_owners: Vec::new(),
            #[cfg(feature = "net")]
            fw_serve: false,
            #[cfg(feature = "net")]
            fw_server: "nw.ommesh.ru".into(),
            fw_mode: crate::fwstate::mode::OFF,
            fw_channel: 0,
            fw_source: Vec::new(),
            time_sync: crate::timesync::Mode::Listen.code(),
            #[cfg(feature = "net")]
            time_serve: false,
            #[cfg(feature = "net")]
            time_server: "nw.ommesh.ru".into(),
            #[cfg(feature = "net")]
            time_beacons: 4,
            time_keys: Vec::new(),
            time_gateway: String::new(),
            buzzer_quiet: true, // a board may have no speaker soldered: silent until asked
            screen_mode: ScreenMode::Auto.code(),
            led_enabled: true,
        }
    }
}

/// Keys as comma-separated hex.
pub(crate) fn join_keys(keys: &[[u8; 32]]) -> String {
    let mut s = String::new();
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&crate::util::to_hex(k).to_ascii_lowercase());
    }
    s
}

/// Comma-separated hex keys; `None` if one is not a key.
pub(crate) fn parse_keys(v: &str) -> Option<Vec<[u8; 32]>> {
    v.split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| {
            let mut key = [0u8; 32];
            (k.len() == 64 && crate::util::from_hex(&mut key, k)).then_some(key)
        })
        .collect()
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
        // `set dutycycle 1` stores an airtime factor of 99.
        self.airtime_factor = self.airtime_factor.clamp(0.0, 99.0);
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
        #[cfg(feature = "net")]
        {
            self.bridge_delay = self.bridge_delay.min(10_000);
            self.bridge_channel = self.bridge_channel.clamp(1, 14);
            self.bridge_secret = truncate_str(&self.bridge_secret, BRIDGE_SECRET_MAX);
        }
    }

    pub fn serialize(&self) -> String {
        let mut s = String::new();
        let b = |v: bool| if v { 1 } else { 0 };
        let _ = writeln!(s, "af={}", Shortest(self.airtime_factor));
        let _ = writeln!(s, "name={}", escape(&self.node_name));
        let _ = writeln!(s, "lat={}", Shortest(self.node_lat));
        let _ = writeln!(s, "lon={}", Shortest(self.node_lon));
        let _ = writeln!(s, "password={}", escape(&self.password));
        let _ = writeln!(s, "guest_password={}", escape(&self.guest_password));
        let _ = writeln!(s, "freq={}", Shortest(self.radio.freq_mhz));
        let _ = writeln!(s, "bw={}", Shortest(self.radio.bw_khz));
        let _ = writeln!(s, "sf={}", self.radio.sf);
        let _ = writeln!(s, "cr={}", self.radio.cr);
        let _ = writeln!(s, "tx={}", self.tx_power_dbm);
        let _ = writeln!(s, "disable_fwd={}", b(self.disable_fwd));
        let _ = writeln!(s, "advert_interval={}", self.advert_interval);
        let _ = writeln!(s, "flood_advert_interval={}", self.flood_advert_interval);
        let _ = writeln!(s, "rxdelay={}", Shortest(self.rx_delay_base));
        let _ = writeln!(s, "f_txdelay={}", Shortest(self.tx_delay_factor));
        let _ = writeln!(s, "d_txdelay={}", Shortest(self.direct_tx_delay_factor));
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
        let _ = writeln!(s, "gps={}", b(self.gps_enabled));
        let _ = writeln!(s, "adc_mult={}", Shortest(self.adc_multiplier));
        let _ = writeln!(s, "dyn_cr={}", b(self.dynamic_cr));
        #[cfg(feature = "net")]
        {
            let _ = writeln!(s, "wifi_ssid={}", escape(&self.wifi_ssid));
            let _ = writeln!(s, "wifi_pwd={}", escape(&self.wifi_pwd));
            let _ = writeln!(s, "wifi_enabled={}", b(self.wifi_enabled));
            let _ = writeln!(s, "mqtt_iata={}", self.mqtt_iata);
            let _ = writeln!(s, "mqtt_brokers={}", self.mqtt_brokers);
            let _ = writeln!(s, "mqtt_packets={}", b(self.mqtt_packets));
            let _ = writeln!(s, "mqtt_tx={}", b(self.mqtt_tx));
            let _ = writeln!(s, "mqtt_raw={}", b(self.mqtt_raw));
            let _ = writeln!(s, "mqtt_status={}", b(self.mqtt_status));
            let _ = writeln!(s, "mqtt_owner={}", self.mqtt_owner);
            let _ = writeln!(s, "mqtt_email={}", escape(&self.mqtt_email));
            let _ = writeln!(s, "bridge={}", b(self.bridge_enabled));
            let _ = writeln!(s, "bridge_delay={}", self.bridge_delay);
            let _ = writeln!(s, "bridge_src={}", b(self.bridge_source_rx));
            let _ = writeln!(s, "bridge_ch={}", self.bridge_channel);
            let _ = writeln!(s, "bridge_secret={}", escape(&self.bridge_secret));
            let _ = writeln!(s, "fw_serve={}", b(self.fw_serve));
            let _ = writeln!(s, "fw_server={}", escape(&self.fw_server));
            let _ = writeln!(s, "time_serve={}", b(self.time_serve));
            let _ = writeln!(s, "time_server={}", escape(&self.time_server));
            let _ = writeln!(s, "time_beacons={}", self.time_beacons);
        }
        let _ = writeln!(s, "room={}", b(self.room));
        let _ = writeln!(s, "ota_root={}", join_fingerprints(&self.ota_roots, ","));
        let _ = writeln!(s, "ota_owner={}", join_fingerprints(&self.ota_owners, ","));
        let _ = writeln!(s, "fw_mode={}", self.fw_mode);
        let _ = writeln!(s, "fw_channel={}", self.fw_channel);
        let _ = writeln!(s, "fw_source={}", crate::util::to_hex(&self.fw_source));
        let _ = writeln!(s, "time_sync={}", self.time_sync);
        let _ = writeln!(s, "time_keys={}", join_keys(&self.time_keys));
        let _ = writeln!(s, "time_gateway={}", self.time_gateway);
        let _ = writeln!(s, "buzzer_quiet={}", b(self.buzzer_quiet));
        let _ = writeln!(s, "screen={}", self.screen_mode);
        let _ = writeln!(s, "led={}", b(self.led_enabled));
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
                "gps" => p.gps_enabled = flag(),
                "adc_mult" => p.adc_multiplier = (atof(v) as f32).clamp(0.0, 10.0),
                "dyn_cr" => p.dynamic_cr = flag(),
                #[cfg(feature = "net")]
                "wifi_ssid" => p.wifi_ssid = unescape(v),
                #[cfg(feature = "net")]
                "wifi_pwd" => p.wifi_pwd = unescape(v),
                #[cfg(feature = "net")]
                "wifi_enabled" => p.wifi_enabled = flag(),
                #[cfg(feature = "net")]
                "mqtt_iata" => p.mqtt_iata = v.into(),
                #[cfg(feature = "net")]
                "mqtt_brokers" => p.mqtt_brokers = u8v(),
                #[cfg(feature = "net")]
                "mqtt_packets" => p.mqtt_packets = flag(),
                #[cfg(feature = "net")]
                "mqtt_tx" => p.mqtt_tx = flag(),
                #[cfg(feature = "net")]
                "mqtt_raw" => p.mqtt_raw = flag(),
                "room" => p.room = flag(),
                #[cfg(feature = "net")]
                "mqtt_status" => p.mqtt_status = flag(),
                #[cfg(feature = "net")]
                "mqtt_owner" => p.mqtt_owner = v.into(),
                #[cfg(feature = "net")]
                "mqtt_email" => p.mqtt_email = unescape(v),
                #[cfg(feature = "net")]
                "bridge" => p.bridge_enabled = flag(),
                #[cfg(feature = "net")]
                "bridge_delay" => p.bridge_delay = atoi(v).min(u16::MAX as u32) as u16,
                #[cfg(feature = "net")]
                "bridge_src" => p.bridge_source_rx = flag(),
                #[cfg(feature = "net")]
                "bridge_ch" => p.bridge_channel = u8v(),
                #[cfg(feature = "net")]
                "bridge_secret" => p.bridge_secret = unescape(v),
                "ota_root" => p.ota_roots = parse_fingerprints(v).unwrap_or_default(),
                "ota_owner" => p.ota_owners = parse_fingerprints(v).unwrap_or_default(),
                #[cfg(feature = "net")]
                "fw_serve" => p.fw_serve = flag(),
                #[cfg(feature = "net")]
                "fw_server" => p.fw_server = unescape(v),
                "fw_mode" => p.fw_mode = u8v().min(crate::fwstate::mode::INSTALL),
                // before the three modes: on was what `install` is now
                "fw_auto" => p.fw_mode = if flag() { crate::fwstate::mode::INSTALL } else { crate::fwstate::mode::OFF },
                "fw_channel" => p.fw_channel = u8v().min(1),
                "fw_source" => {
                    let n = (v.len() / 2).min(32);
                    let mut key = alloc::vec![0u8; n];
                    if crate::util::from_hex(&mut key, &v[..n * 2]) {
                        p.fw_source = key;
                    }
                }
                "time_sync" => p.time_sync = crate::timesync::Mode::from_code(u8v()).code(),
                #[cfg(feature = "net")]
                "time_serve" => p.time_serve = flag(),
                #[cfg(feature = "net")]
                "time_server" => p.time_server = unescape(v),
                #[cfg(feature = "net")]
                "time_beacons" => p.time_beacons = u8v().min(24),
                "time_keys" => p.time_keys = parse_keys(v).unwrap_or_default(),
                "time_gateway" => p.time_gateway = v.into(),
                "buzzer_quiet" => p.buzzer_quiet = flag(),
                "screen" => p.screen_mode = ScreenMode::from_code(u8v()).code(),
                "led" => p.led_enabled = flag(),
                _ => {}
            }
        }
        p.sanitize();
        p
    }

    /// The radio settings alone from saved prefs, as [`Self::deserialize`] reads them: what a
    /// companion takes over from the repeater firmware on its first start, without the code for
    /// the rest (several KB a Pro Micro companion has no room for).
    pub fn radio_settings(text: &str) -> RadioSettings {
        // the defaults are `NodePrefs::default()`'s
        let mut r = RadioSettings { radio: RadioParams::default(), tx_power_dbm: 10, rx_boosted_gain: true, radio_fem_rxgain: false };
        for (k, v) in text.lines().filter_map(|l| l.split_once('=')) {
            match k {
                "freq" => r.radio.freq_mhz = atof(v) as f32,
                "bw" => r.radio.bw_khz = atof(v) as f32,
                "sf" => r.radio.sf = atoi(v).min(255) as u8,
                "cr" => r.radio.cr = atoi(v).min(255) as u8,
                "tx" => r.tx_power_dbm = atoi_signed(v).clamp(-128, 127) as i8,
                "rxgain" => r.rx_boosted_gain = atoi(v) != 0,
                "fem_rxgain" => r.radio_fem_rxgain = atoi(v) != 0,
                _ => {}
            }
        }
        r.radio.freq_mhz = r.radio.freq_mhz.clamp(150.0, 2500.0);
        r.radio.bw_khz = r.radio.bw_khz.clamp(7.8, 500.0);
        r.radio.sf = r.radio.sf.clamp(5, 12);
        r.radio.cr = r.radio.cr.clamp(5, 8);
        r.tx_power_dbm = r.tx_power_dbm.clamp(-9, 22);
        r
    }
}

/// The radio part of [`NodePrefs`] ([`NodePrefs::radio_settings`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadioSettings {
    pub radio: RadioParams,
    pub tx_power_dbm: i8,
    pub rx_boosted_gain: bool,
    pub radio_fem_rxgain: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radio_settings_read_as_the_whole_prefs_do() {
        let mut p = NodePrefs { tx_power_dbm: 17, rx_boosted_gain: false, radio_fem_rxgain: true, ..Default::default() };
        p.radio.freq_mhz = 868.731;
        p.radio.bw_khz = 62.5;
        p.radio.sf = 7;
        p.radio.cr = 7;
        for text in [p.serialize(), String::new(), "freq=9999\nsf=40\ntx=30\n".into()] {
            let whole = NodePrefs::deserialize(&text);
            let r = NodePrefs::radio_settings(&text);
            let want = RadioSettings {
                radio: whole.radio,
                tx_power_dbm: whole.tx_power_dbm,
                rx_boosted_gain: whole.rx_boosted_gain,
                radio_fem_rxgain: whole.radio_fem_rxgain,
            };
            assert_eq!(r, want, "{text:?}");
        }
    }

    #[test]
    fn roundtrip() {
        let mut p =
            NodePrefs { node_name: "Крыша \\ 1".into(), owner_info: "line1\nline2".into(), ..Default::default() };
        p.radio.freq_mhz = 868.731;
        p.tx_power_dbm = -3;
        p.ota_roots = alloc::vec![[0xA5; 32], [7; 32]];
        p.ota_owners = alloc::vec![[0x3C; 32]];
        p.fw_serve = true;
        p.fw_server = "fw.example:8080".into();
        p.fw_mode = crate::fwstate::mode::DOWNLOAD;
        p.fw_channel = 1;
        p.fw_source = alloc::vec![0xCE, 0x5B, 0x93];
        p.gps_enabled = true;
        p.screen_mode = ScreenMode::Manual.code();
        p.led_enabled = false;
        let q = NodePrefs::deserialize(&p.serialize());
        assert_eq!(p, q);
    }

    #[test]
    fn screen_mode_parse_and_codes() {
        assert_eq!(ScreenMode::parse("auto"), Some(ScreenMode::Auto));
        assert_eq!(ScreenMode::parse("on"), Some(ScreenMode::Auto));
        assert_eq!(ScreenMode::parse("1"), Some(ScreenMode::Auto));
        assert_eq!(ScreenMode::parse("manual"), Some(ScreenMode::Manual));
        assert_eq!(ScreenMode::parse("off"), Some(ScreenMode::Off));
        assert_eq!(ScreenMode::parse("0"), Some(ScreenMode::Off));
        assert_eq!(ScreenMode::parse("bogus"), None);
        for m in [ScreenMode::Auto, ScreenMode::Manual, ScreenMode::Off] {
            assert_eq!(ScreenMode::from_code(m.code()), m);
            assert_eq!(ScreenMode::parse(m.name()), Some(m));
        }
        assert_eq!(ScreenMode::from_code(99), ScreenMode::Auto);
    }

    #[test]
    fn an_old_fw_auto_on_installs_by_itself() {
        assert_eq!(NodePrefs::deserialize("fw_auto=1\n").fw_mode, crate::fwstate::mode::INSTALL);
        assert_eq!(NodePrefs::deserialize("fw_auto=0\n").fw_mode, crate::fwstate::mode::OFF);
    }

    #[test]
    fn missing_keys_use_defaults() {
        let q = NodePrefs::deserialize("name=X\nbogus=1\n");
        assert_eq!(q.node_name, "X");
        assert_eq!(q.flood_max, 64);
    }
}
