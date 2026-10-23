//! The companion's on-device interface: pages, navigation and actions, driven by one button
//! (as upstream's: a click turns to the next page, a double click back, a long press acts on
//! the page). What a page looks like, and when the screen redraws, is the board's: an e-paper
//! and an OLED want different things, so the board gets a [`View`], a plain description of the
//! page, and renders it in its own layout and language.

use alloc::string::String;
use alloc::vec::Vec;

/// Two clicks closer together than this are a double click (upstream `MULTI_CLICK_WINDOW_MS`).
const MULTI_CLICK_MS: u64 = 280;
/// A press this long is a long press (upstream's T-Echo button).
const LONG_PRESS_MS: u64 = 1000;
/// Without a press for this long, the interface returns to the home page.
const HOME_AFTER_MS: u64 = 60_000;
/// How long a notice ("advert sent") stays over the page.
const ALERT_MS: u64 = 2500;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Press {
    Click,
    Double,
    /// Reported while the button is still held, once it has been down long enough.
    Long,
}

/// Turns the button's level, sampled every loop pass, into presses.
#[derive(Default)]
pub struct Button {
    down_at: Option<u64>,
    long_sent: bool,
    clicks: u8,
    last_up: u64,
}

impl Button {
    pub fn poll(&mut self, down: bool, now: u64) -> Option<Press> {
        match (down, self.down_at) {
            (true, None) => {
                self.down_at = Some(now);
                None
            }
            (true, Some(t)) if !self.long_sent && now - t >= LONG_PRESS_MS => {
                self.long_sent = true;
                self.clicks = 0;
                Some(Press::Long)
            }
            (false, Some(_)) => {
                self.down_at = None;
                if core::mem::take(&mut self.long_sent) {
                    return None;
                }
                self.clicks += 1;
                self.last_up = now;
                if self.clicks >= 2 {
                    self.clicks = 0;
                    return Some(Press::Double);
                }
                None
            }
            (false, None) if self.clicks == 1 && now - self.last_up > MULTI_CLICK_MS => {
                self.clicks = 0;
                Some(Press::Click)
            }
            _ => None,
        }
    }

    /// The button is down or a click may still become a double click: poll it often.
    pub fn busy(&self) -> bool {
        self.down_at.is_some() || self.clicks > 0
    }
}

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
}

/// What a long press on a page asks the companion to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Advert,
    ToggleBle,
    ToggleGps,
    PowerOff,
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

/// The interface's state: the page shown, notices, the battery level shown.
pub struct Ui {
    pages: &'static [Page],
    idx: usize,
    last_press: u64,
    alert: Option<(Alert, u64)>,
    battery_pct: Option<u8>,
}

impl Ui {
    /// `pages` in the order a click goes through them, the home page first; none = the board
    /// has no interface of its own.
    pub fn new(pages: &'static [Page]) -> Self {
        Self { pages, idx: 0, last_press: 0, alert: None, battery_pct: None }
    }

    pub fn enabled(&self) -> bool {
        !self.pages.is_empty()
    }

    pub fn page(&self) -> Page {
        self.pages.get(self.idx).copied().unwrap_or(Page::Home)
    }

    pub fn press(&mut self, press: Press, now: u64) -> Option<Action> {
        self.last_press = now;
        self.alert = None;
        let n = self.pages.len().max(1);
        match press {
            Press::Click => self.idx = (self.idx + 1) % n,
            Press::Double => self.idx = (self.idx + n - 1) % n,
            Press::Long => {
                return match self.page() {
                    Page::Advert => Some(Action::Advert),
                    Page::Bluetooth => Some(Action::ToggleBle),
                    Page::Gps => Some(Action::ToggleGps),
                    Page::PowerOff => Some(Action::PowerOff),
                    _ => None,
                };
            }
        }
        None
    }

    pub fn alert(&mut self, alert: Alert, now: u64) {
        self.alert = Some((alert, now + ALERT_MS));
    }

    /// Time passes: notices expire, an idle interface goes home.
    pub fn tick(&mut self, now: u64) {
        if self.alert.is_some_and(|(_, until)| now >= until) {
            self.alert = None;
        }
        if self.idx != 0 && now - self.last_press >= HOME_AFTER_MS {
            self.idx = 0;
        }
    }

    /// Battery level to show, in steps of 5 % that move only once the reading has clearly
    /// crossed into the next step (a reading near a step's edge would flip it back and forth).
    pub fn battery_pct(&mut self, mv: u16) -> u8 {
        let pct = battery_percent(mv);
        let shown = match self.battery_pct {
            Some(s) if pct.abs_diff(s) < 5 + 2 => s,
            _ => (pct + 2) / 5 * 5,
        };
        self.battery_pct = Some(shown.min(100));
        shown.min(100)
    }

    pub fn view_frame(&self) -> (u8, u8, Option<Alert>) {
        (self.idx as u8, self.pages.len() as u8, self.alert.map(|(a, _)| a))
    }
}

/// A single-cell LiPo's charge from its voltage under light load.
pub fn battery_percent(mv: u16) -> u8 {
    const CURVE: [(u16, u8); 9] =
        [(4200, 100), (4100, 90), (4000, 80), (3900, 65), (3800, 50), (3700, 30), (3600, 15), (3500, 5), (3300, 0)];
    if mv >= CURVE[0].0 {
        return 100;
    }
    for w in CURVE.windows(2) {
        let ((hi_mv, hi), (lo_mv, lo)) = (w[0], w[1]);
        if mv >= lo_mv {
            return lo + ((mv - lo_mv) as u32 * (hi - lo) as u32 / (hi_mv - lo_mv) as u32) as u8;
        }
    }
    0
}

/// Minutes since something happened, coarse enough that it changes rarely: whole minutes in the
/// first hour, then whole hours, then whole days (in minutes).
pub fn coarse_minutes(secs: u32) -> u32 {
    let m = secs / 60;
    match m {
        0..60 => m,
        60..1440 => m / 60 * 60,
        _ => m / 1440 * 1440,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BleState {
    Off,
    Waiting,
    Connected,
}

/// Always shown: the top bar.
#[derive(Clone, PartialEq, Debug)]
pub struct Bar {
    pub battery_pct: u8,
    pub ble: BleState,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Message {
    pub from: String,
    pub text: String,
    /// See [`coarse_minutes`].
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
    pub battery_pct: u8,
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

/// The page's content.
#[derive(Clone, PartialEq, Debug)]
pub enum Body {
    /// Calm on purpose (it is up most of the time): no message text, nothing that ticks.
    Home { name: String, unread: u32 },
    /// Most recent first.
    Messages(Vec<Message>),
    /// Most recent first.
    Nearby(Vec<Node>),
    Radio(RadioInfo),
    Bluetooth { state: BleState, name: String, pin: Option<u32> },
    Gps(GpsInfo),
    Advert,
    PowerOff,
    /// Left on a screen that keeps its image while the node is off.
    Off { name: String },
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
