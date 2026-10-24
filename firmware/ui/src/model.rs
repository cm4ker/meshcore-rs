//! What a screen shows, as plain data: a [`View`] is the frame (top bar, page dots, a notice,
//! the pairing PIN) around one page's content, its [`Body`]. Each role has its own pages: the
//! companion's answer "any new messages?", the repeater's "which node is this, how is it set up,
//! does it hear its neighbours?". Layouts ([`crate::layout`]) turn a view into pixels.

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Home,
    Messages,
    Nearby,
    Radio,
    Bluetooth,
    Gps,
    Advert,
    PowerOff,
    Neighbours,
    Traffic,
    Power,
    Network,
    Update,
    /// The node's contact as a QR code.
    Contact,
}

/// What a long press on a page asks the firmware to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Advert,
    ToggleBle,
    ToggleGps,
    PowerOff,
    /// The repeater's update window over BLE: open it (the PIN goes on the screen), or close it
    /// if no phone is connected.
    ToggleUpdate,
}

impl Page {
    /// The action of a long press on this page.
    pub fn action(self) -> Option<Action> {
        match self {
            Page::Advert => Some(Action::Advert),
            Page::Bluetooth => Some(Action::ToggleBle),
            Page::Gps => Some(Action::ToggleGps),
            Page::PowerOff => Some(Action::PowerOff),
            Page::Update => Some(Action::ToggleUpdate),
            _ => None,
        }
    }
}

/// A short notice over the page.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Alert {
    AdvertSent,
    AdvertFailed,
    BleOn,
    BleOff,
    GpsOn,
    GpsOff,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BleState {
    Off,
    Waiting,
    Connected,
}

/// Always shown: the top bar. No battery: its charge cannot be told from the voltage without
/// knowing the cell (Li-ion, LiFePO4...), and the voltage is on the pages.
#[derive(Clone, PartialEq, Debug)]
pub struct Bar {
    /// The Bluetooth icon; none on a repeater while its update window is closed.
    pub ble: Option<BleState>,
    /// The node's ID next to the battery (repeaters), in hex: 1 to 3 bytes by `path.hash.mode`,
    /// as paths and traces show it.
    pub id: Option<String>,
}

// ---- the companion's pages

#[derive(Clone, PartialEq, Debug)]
pub struct Message {
    pub from: String,
    pub text: String,
    /// See [`crate::coarse_minutes`].
    pub age_min: u32,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Node {
    pub name: String,
    pub hops: u8,
    pub age_min: u32,
}

#[derive(Clone, PartialEq, Debug)]
pub struct RadioInfo {
    pub freq_mhz: f32,
    pub bw_khz: f32,
    pub sf: u8,
    pub cr: u8,
    pub tx_dbm: i8,
    pub noise_dbm: i16,
    pub last_rssi: i16,
    /// Of the last packet, multiplied by 4.
    pub last_snr: i16,
    pub rx: u32,
    pub tx: u32,
    pub tx_air_min: u32,
    pub rx_air_min: u32,
    pub uptime_min: u32,
    pub batt_mv: u16,
}

#[derive(Clone, PartialEq, Debug)]
pub struct GpsInfo {
    pub on: bool,
    /// The receiver is talking (else it is starting, or broken).
    pub receiving: bool,
    /// Latitude and longitude in millionths of a degree.
    pub fix: Option<(i32, i32)>,
    /// Used for the position.
    pub satellites: u8,
    /// Seen.
    pub in_view: u8,
}

// ---- the repeater's pages

/// Which node this is and how it is set up: changes only with the settings. Its ID is in the
/// bar ([`Bar::id`]).
#[derive(Clone, PartialEq, Debug)]
pub struct Passport {
    pub name: String,
    pub freq_mhz: f32,
    pub bw_khz: f32,
    pub sf: u8,
    pub cr: u8,
    pub tx_dbm: i8,
    pub version: String,
}

/// Is the node alive and does it hear others: numbers that move slowly.
#[derive(Clone, PartialEq, Debug)]
pub struct Health {
    pub neighbours: u16,
    /// See [`crate::Ui::battery_mv`].
    pub batt_mv: u16,
    /// See [`crate::coarse_minutes`].
    pub uptime_min: u32,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Neighbour {
    /// As [`Passport::id`] shows it.
    pub id: String,
    /// Multiplied by 4.
    pub snr: i16,
    pub age_min: u32,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Traffic {
    pub rx: u32,
    pub tx: u32,
    /// Packets with a bad CRC.
    pub rx_errors: u32,
    /// Time on air transmitting, in tenths of a percent of the uptime.
    pub tx_air_permille: u16,
    pub rx_air_permille: u16,
    /// Waiting to be sent.
    pub queue: u16,
    pub noise_dbm: i16,
}

#[derive(Clone, PartialEq, Debug)]
pub struct PowerInfo {
    pub batt_mv: u16,
    /// Light sleep between packets (`powersaving`): none = the board cannot.
    pub powersaving: Option<bool>,
    /// The share of the uptime spent asleep.
    pub asleep_pct: u8,
    pub uptime_min: u32,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Wifi {
    /// No network set.
    Off,
    Connecting { ssid: String },
    Up { ssid: String, ip: String },
}

#[derive(Clone, PartialEq, Debug)]
pub struct NetInfo {
    pub wifi: Wifi,
    /// MQTT brokers switched on, and how many of them are connected.
    pub mqtt_on: u8,
    pub mqtt_up: u8,
    /// The clock is set from the network.
    pub time_synced: bool,
}

/// The update window over BLE (`start ota ble`).
#[derive(Clone, PartialEq, Debug)]
pub struct OtaWindow {
    /// The name the phone or PC sees.
    pub name: String,
    pub pin: u32,
    pub connected: bool,
    pub mins_left: u32,
}

#[derive(Clone, PartialEq, Debug)]
pub struct UpdateInfo {
    pub window: Option<OtaWindow>,
    /// Running a new image that has not confirmed itself yet.
    pub trial: bool,
    pub version: String,
}

/// The page's content.
#[derive(Clone, PartialEq, Debug)]
pub enum Body {
    /// The companion's home: calm on purpose (it is up most of the time), no message text,
    /// nothing that ticks.
    Home { name: String, unread: u32 },
    /// Most recent first.
    Messages(Vec<Message>),
    /// Most recent first.
    Nearby(Vec<Node>),
    Radio(RadioInfo),
    Bluetooth { state: BleState, name: String, pin: Option<u32> },
    Gps(GpsInfo),
    /// A zero-hop advert (companion), or one that floods the mesh (repeater).
    Advert { flood: bool },
    PowerOff,
    /// Left on a screen that keeps its image while the node is off.
    Off { name: String },
    /// The repeater's home: its passport and health.
    Repeater(Passport, Health),
    /// Most recently heard first.
    Neighbours(Vec<Neighbour>),
    Traffic(Traffic),
    /// The repeater's radio page: the preset, large, for checking a node where it is mounted.
    Preset(Passport),
    Power(PowerInfo),
    Network(NetInfo),
    Update(UpdateInfo),
    /// A QR code of the node's contact link (`meshcore://contact/add?...`), for the app's camera.
    Contact(String),
}

/// Everything a screen shows, for the board to render.
#[derive(Clone, PartialEq, Debug)]
pub struct View {
    pub page: Page,
    pub page_idx: u8,
    pub pages: u8,
    pub bar: Bar,
    pub body: Body,
    pub alert: Option<Alert>,
    /// A phone is pairing: the passkey it asks for.
    pub pin: Option<u32>,
}
