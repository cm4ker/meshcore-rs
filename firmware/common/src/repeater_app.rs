//! The MeshCore repeater role, shared by the board crates (Heltec V4, XIAO ESP32-C6).
//!
//! The board crate builds the LoRa radio and a [`RepeaterBoard`] (front end, LED, battery, screen,
//! GPS, light-sleep/power, button), then hands over to [`run`], which owns the whole repeater main
//! loop: TX arbitration, receive, the mesh poll, the ESP-NOW bridge, the on-device interface, the
//! Wi-Fi station and MQTT observer, firmware updates (Wi-Fi/BLE/neighbour) and power management.
//!
//! Everything the loop calls into — the mesh/repeater logic, the radio trait, net/broker/wifi_ota/
//! ota/ble/fwstore/storage/espnow — is board-independent; only the peripheral wiring behind
//! [`RepeaterBoard`] differs between boards.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either4, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Instant, Timer};
use esp_hal::peripherals::{BT, WIFI};
use esp_hal::rng::Rng;
use esp_println::print;
use log::{info, warn};

use meshcore::crypto;
use meshcore::gps::GpsReading;
use meshcore::identity::LocalIdentity;
use meshcore::mesh::{Mesh, MeshCtx};
use meshcore::prefs::{NAME_MAX, NodePrefs};
use meshcore::repeater::{BLE_OTA_WINDOW_MINS, BoardInfo, Repeater};
use meshcore::util::{Shortest, to_hex};
use meshcore::{PRV_KEY_SIZE, PUB_KEY_SIZE, uplink};
use meshcore_ui::refresh::oled::Oled as ScreenPolicy;
use meshcore_ui::{
    Action, Alert, Bar, BleState, Body, Button, Health, NetInfo, OtaWindow, Page, Passport, PowerInfo, Traffic, Ui, UpdateInfo,
    View, Wifi,
};

use crate::ble::{self, BLE_CONNECTED, BLE_OUT, BONDS_CHANGED};
use crate::frames::FRAMES_IN;
use crate::ota::{self, FlashTarget, SharedStore};
use crate::storage::{Blob, CompBlob};
use crate::{broker, clock_cell, espnow, fwstore, net, reset, wifi_ota};
use crate::net::BrokerState;
use crate::noise::NoiseFloor;

/// esp-println's `println!` ends a line with LF only. The console's lines end with CR LF, as the
/// Arduino firmware's do: MeshCore's web consoles show a line only once its CR LF has come.
macro_rules! println {
    () => {
        print!("\r\n")
    };
    ($($arg:tt)*) => {
        print!("{}\r\n", format_args!($($arg)*))
    };
}

/// Longest legitimate stall is one max-length SF12 transmission (~10 s).
const WATCHDOG_SECS: u64 = 30;
/// The screen stays lit this long after boot or a button press.
const SCREEN_ON_MS: u64 = 60_000;
/// `poweroff` or a long press on the home page: the board goes off this long after.
const POWER_OFF_DELAY_MS: u64 = 3000;
/// `powersaving on`: the first sleep comes this long after boot/activity, and one sleep lasts at
/// most this long (below the watchdog).
const AWAKE_AFTER_ACTIVITY_MS: u64 = 120_000;
const MAX_SLEEP_MS: u64 = 20_000;
/// An update counts as abandoned (and no longer keeps the board awake) after this long idle.
const OTA_IDLE_MS: u64 = 300_000;
/// A trial firmware that keeps running this long is kept.
const TRIAL_HEALTHY_MS: u64 = 120_000;
/// Wear-friendly: two sectors alternate, ~24 erases per sector per day.
const CLOCK_SAVE_INTERVAL_MS: u64 = 30 * 60 * 1000;
/// The clock follows the GPS this often while it has a fix (upstream `TIME_SYNC_INTERVAL`).
const GPS_TIME_SYNC_MS: u64 = 30 * 60 * 1000;
/// A fix that has held this long is trusted for the time (upstream waits out three checks).
const GPS_FIX_SETTLE_MS: u64 = 3000;

/// The screen's pages; the update page only with two firmware slots.
const PAGES: &[Page] = &[
    Page::Home,
    Page::Contact,
    Page::Neighbours,
    Page::Traffic,
    Page::Radio,
    Page::Power,
    Page::Network,
    Page::Update,
    Page::Advert,
];
const PAGES_SINGLE_SLOT: &[Page] =
    &[Page::Home, Page::Contact, Page::Neighbours, Page::Traffic, Page::Radio, Page::Power, Page::Network, Page::Advert];

/// CLI lines from the board's console task to the main loop.
pub static CLI_LINES: Channel<CriticalSectionRawMutex, Vec<u8>, 4> = Channel::new();
/// When (low 32 bits of the uptime in ms, 0 = never) console input woke the board from light sleep;
/// the console task drops the rest of the cut line. The board's [`RepeaterBoard::light_sleep`] sets
/// it when it supports a console that wakes the chip (the Heltec V3's UART); the console task reads it.
pub static CONSOLE_WOKE_MS: AtomicU32 = AtomicU32::new(0);

/// Read the USB Serial/JTAG console, echo input, and hand complete lines to the loop through
/// [`CLI_LINES`]. Boards with a USB console spawn this; the Heltec V3's UART console is its own.
#[embassy_executor::task]
pub async fn usb_console_task(mut rx: crate::usb::Input) {
    /// Input closer than this to a sleep-wake, or to the byte before it, is the rest of a cut line.
    const CUT_LINE_GAP_MS: u32 = 500;
    let mut line: Vec<u8> = Vec::with_capacity(160);
    let mut buf = [0u8; 64];
    let mut shown = 0usize;
    let show = |part: &[u8]| part.iter().for_each(|&c| print!("{}", c as char));
    let mut cut_line: Option<u32> = None;
    loop {
        let n = rx.read(&mut buf).await;
        let typed = n <= 2;
        let woke = CONSOLE_WOKE_MS.swap(0, Ordering::Relaxed);
        if woke != 0 {
            (cut_line, shown) = (Some(woke), 0);
            line.clear();
        }
        for &c in &buf[..n] {
            if let Some(last) = cut_line {
                let t = now_ms() as u32;
                if t.wrapping_sub(last) < CUT_LINE_GAP_MS {
                    cut_line = (!matches!(c, b'\r' | b'\n')).then_some(t);
                    continue;
                }
                cut_line = None;
            }
            match c {
                b'\r' | b'\n' => {
                    if !line.is_empty() {
                        show(&line[shown..]);
                        print!("\r\n");
                        shown = 0;
                        CLI_LINES.send(core::mem::take(&mut line)).await;
                    }
                }
                0x08 | 0x7F => {
                    if line.pop().is_some() && shown > line.len() {
                        shown = line.len();
                        print!("\x08 \x08");
                    }
                }
                _ if line.len() < 159 => {
                    line.push(c);
                    if typed {
                        show(&line[shown..]);
                        shown = line.len();
                    }
                }
                _ => {}
            }
        }
    }
}

/// Settings a board passes in; the running config comes from the stored prefs.
pub struct Config {
    pub build_epoch: u32,
    pub build_date: &'static str,
}

/// The peripherals the repeater loop owns besides the board itself.
pub struct Hardware<R> {
    /// Not initialised yet: [`run`] configures it from the stored radio settings.
    pub radio: R,
    pub store: Option<SharedStore<'static>>,
    pub wifi: WIFI<'static>,
    pub bt: BT<'static>,
    /// From the hardware TRNG; [`run`] mixes in the radio's own random numbers.
    pub entropy: [u8; 32],
}

/// Why the board came back from a light sleep.
#[derive(Clone, Copy, Default)]
pub struct SleepWake {
    /// The button was pressed.
    pub button: bool,
    /// Console input woke the chip (the first bytes were lost).
    pub console: bool,
}

/// The board-specific hardware the repeater loop drives. The radio is passed separately (generic),
/// and the Wi-Fi/BLE peripherals and the store travel in [`Hardware`].
#[allow(async_fn_in_trait)]
pub trait RepeaterBoard {
    // ---- static facts
    /// Model name, e.g. the FEM variant; becomes `BoardInfo::manufacturer` and the uplink model.
    fn name(&self) -> &str;
    /// The board a signed release must name (`meshcore::release::board`).
    fn release_board(&self) -> u8;
    /// SX1262 output for a fresh node (the FEM/driver turns it into the antenna power).
    fn default_tx_dbm(&self) -> i8;
    /// The board's own battery multiplier for `set adc.multiplier`; none = unsupported.
    fn adc_multiplier(&self) -> Option<f32>;
    /// A GPS receiver is wired (`gps` CLI).
    fn has_gps(&self) -> bool;
    /// A screen is present (the on-device interface runs).
    fn has_screen(&self) -> bool;
    /// The front end has a switchable LNA (`set radio.fem.rxgain`).
    fn lna_controllable(&self) -> bool;
    /// The board can light-sleep between packets (`powersaving on`).
    fn powersaving_supported(&self) -> bool;
    /// The bridge the board can run (`get bridge.type`); none = no bridge.
    fn bridge(&self) -> Option<&'static str>;

    // ---- front end + TX LED
    fn fem_tx_mode(&mut self);
    fn fem_rx_mode(&mut self);
    fn fem_set_lna(&mut self, on: bool);
    /// The TX LED (lit while transmitting).
    fn tx_led(&mut self, on: bool);
    /// Blink used only on the fatal radio-init failure.
    fn led_toggle(&mut self);

    // ---- battery: millivolts, already scaled by the board for the running multiplier (0 = none)
    async fn battery_mv(&mut self, multiplier: Option<f32>) -> u16;

    // ---- GPS
    fn set_gps(&mut self, on: bool);
    fn gps_latest(&self) -> Option<GpsReading>;

    // ---- screen (a no-op board reports `has_screen() == false` and need not implement these)
    /// A screen that keeps its image unpowered (e-paper): it never goes dark, and the board decides
    /// when to redraw what [`screen_show`](Self::screen_show) hands it.
    fn always_on(&self) -> bool {
        false
    }
    fn screen_is_on(&self) -> bool {
        false
    }
    fn screen_set_on(&mut self, _on: bool) {}
    /// Draw `view`; `false` if the frame could not be sent.
    fn screen_show(&mut self, _view: &View) -> bool {
        true
    }
    /// Failed I2C transfers since boot (diagnostics).
    fn screen_bus_errors(&self) -> u32 {
        0
    }
    /// Give up on the screen after repeated bus errors.
    fn screen_disable(&mut self) {}

    // ---- button + power
    fn button_down(&self) -> bool;
    /// A USB host is attached (its traffic keeps the chip awake).
    fn usb_host_active(&mut self) -> bool {
        false
    }
    /// `powersaving stats` summary line, refreshed about once a minute.
    fn power_summary(&mut self) -> String {
        String::new()
    }
    /// Percentage of time asleep, for the Power page.
    fn asleep_pct(&self) -> u8 {
        0
    }
    /// Arm the button (and any console) as wake sources, light-sleep for `span_ms`, and report what
    /// woke the chip. A board that cannot light-sleep returns `None` (the loop then does not sleep).
    fn light_sleep(&mut self, _span_ms: u64) -> Option<SleepWake> {
        None
    }
    /// Turn the board off (front end, radio hold, screen, deep sleep until the button). The radio
    /// is already asleep. Physically does not return (deep sleep), but need not be typed so.
    async fn power_off(&mut self);
}

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

fn load(store: &Option<SharedStore<'_>>, blob: Blob) -> Option<Vec<u8>> {
    store.as_ref()?.borrow_mut().load(blob)
}

fn save(store: &Option<SharedStore<'_>>, blob: Blob, data: &[u8]) -> bool {
    store.as_ref().is_some_and(|s| s.borrow_mut().save(blob, data))
}

fn load_identity(store: &Option<SharedStore<'_>>) -> Option<LocalIdentity> {
    let data = load(store, Blob::Identity)?;
    if data.len() != PRV_KEY_SIZE + PUB_KEY_SIZE {
        return None;
    }
    let prv: [u8; PRV_KEY_SIZE] = data[..PRV_KEY_SIZE].try_into().ok()?;
    let id = LocalIdentity::from_private_key(&prv);
    (id.pub_key()[..] == data[PRV_KEY_SIZE..]).then_some(id)
}

fn save_identity(store: &Option<SharedStore<'_>>, prv: &[u8; PRV_KEY_SIZE]) -> bool {
    let id = LocalIdentity::from_private_key(prv);
    let mut blob = Vec::with_capacity(PRV_KEY_SIZE + PUB_KEY_SIZE);
    blob.extend_from_slice(prv);
    blob.extend_from_slice(id.pub_key());
    save(store, Blob::Identity, &blob)
}

/// Hosted room server: `[private key][prefs text]` in [`Blob::Room`].
fn save_room(store: &Option<SharedStore<'_>>, prv: &[u8; PRV_KEY_SIZE], prefs: &NodePrefs) -> bool {
    let mut blob = prv.to_vec();
    blob.extend_from_slice(prefs.serialize().as_bytes());
    save(store, Blob::Room, &blob)
}

/// SSID of the update access point (at most 32 bytes).
fn ota_ssid(node_name: &str) -> String {
    let mut s = alloc::format!("MeshCore-OTA-{node_name}");
    while s.len() > 32 {
        s.pop();
    }
    s
}

/// The room server hosted next to the repeater (`set room on`): a second node with its own
/// identity, prefs and ACL on the same radio. Radio settings, clock and regions are the repeater's.
fn start_room(store: &Option<SharedStore<'_>>, rep: &Mesh<Repeater>, entropy: &[u8], rng_seed: [u8; 16]) -> Mesh<Repeater> {
    let saved = load(store, Blob::Room).and_then(|d| {
        let prv: [u8; PRV_KEY_SIZE] = d.get(..PRV_KEY_SIZE)?.try_into().ok()?;
        LocalIdentity::validate_private_key(&prv).then(|| {
            (LocalIdentity::from_private_key(&prv), NodePrefs::deserialize(&String::from_utf8_lossy(&d[PRV_KEY_SIZE..])))
        })
    });
    let rp = &rep.app.prefs;
    let (id, mut prefs) = saved.unwrap_or_else(|| {
        let taken = rep.ctx.self_id.pub_key()[0];
        let mut counter = 0u8;
        let id = loop {
            let mut seed = [0u8; 32];
            crypto::sha256(&mut seed, &[entropy, b"room", &[counter]]);
            let id = LocalIdentity::from_seed(&seed);
            counter = counter.wrapping_add(1);
            if ![0x00, 0xFF, taken].contains(&id.pub_key()[0]) {
                break id;
            }
        };
        let prefs = NodePrefs {
            node_name: meshcore::util::truncate_str(&alloc::format!("{} Room", rp.node_name), NAME_MAX),
            password: rp.password.clone(),
            node_lat: rp.node_lat,
            node_lon: rp.node_lon,
            advert_loc_policy: rp.advert_loc_policy,
            advert_interval: rp.advert_interval,
            flood_advert_interval: rp.flood_advert_interval,
            ..NodePrefs::room_server_defaults()
        };
        if !save_room(store, id.private_key(), &prefs) {
            warn!("room: identity save failed");
        }
        (id, prefs)
    });
    prefs.radio = rp.radio;
    prefs.tx_power_dbm = rp.tx_power_dbm;
    let board = BoardInfo { wifi_ota_ssid: None, ble_ota_name: None, adc_multiplier: None, ..rep.app.board.clone() };
    let ctx = MeshCtx::new(id.clone(), rng_seed, rep.ctx.radio, rep.ctx.current_time());
    let mut room = Mesh::new(ctx, Repeater::new_room_server(prefs, board));
    if let Some(acl) = load(store, Blob::RoomAcl) {
        room.app.acl_load(&acl, &id);
    }
    room.app.regions = rep.app.regions.clone();
    println!("Room server ID: {} ({})", to_hex(id.pub_key()), room.app.prefs.node_name);
    room
}

/// Time on air as tenths of a percent of the uptime.
fn permille(air_ms: u64, uptime_ms: u64) -> u16 {
    (air_ms * 1000 / uptime_ms.max(1)).min(1000) as u16
}

/// What the screen shows that lives outside the repeater.
struct ScreenInfo {
    ble_window: Option<(u32, u64)>,
    wifi: bool,
    asleep_pct: u8,
}

/// `get mqtt.status` line (MeshCoreTel format).
fn mqtt_status_line(p: &NodePrefs, station: bool) -> String {
    let mut s = alloc::format!(
        "wifi:{} ntp:{} iata:{}",
        if net::WIFI_UP.load(Ordering::Relaxed) { "up" } else { "down" },
        if net::TIME_SYNCED.load(Ordering::Relaxed) { "up" } else { "wait" },
        p.mqtt_iata
    );
    for (i, (name, bit)) in meshcore::repeater::MQTT_BROKERS.iter().enumerate() {
        let state = if p.mqtt_brokers & bit == 0 {
            "off"
        } else if !station || p.mqtt_iata == "UNSET" {
            "invalid iata"
        } else {
            BrokerState::load(i).name()
        };
        s.push_str(&alloc::format!(" {name}:{state}"));
    }
    s.push_str(&alloc::format!(" status:{} tx:{}", if p.mqtt_status { "on" } else { "off" }, if p.mqtt_tx { "on" } else { "off" }));
    s
}

/// The interface's current page with what it shows.
fn repeater_view(ui: &mut Ui, mesh: &Mesh<Repeater>, info: &ScreenInfo, now: u64, build_date: &str) -> View {
    let (app, ctx) = (&mesh.app, &mesh.ctx);
    let p = &app.prefs;
    let key = ctx.self_id.pub_key();
    let id_len = p.path_hash_mode as usize + 1;
    let id = to_hex(&key[..id_len]);
    let version = alloc::format!("{} {}", meshcore::FIRMWARE_VERSION, meshcore::release::build_label(build_date));
    let passport = || Passport {
        name: p.node_name.clone(),
        id: id.clone(),
        freq_mhz: p.radio.freq_mhz,
        bw_khz: p.radio.bw_khz,
        sf: p.radio.sf,
        cr: p.radio.cr,
        tx_dbm: p.tx_power_dbm,
        version: version.clone(),
    };
    let uptime_min = (now / 60_000) as u32;
    let s = &ctx.stats;
    let ble_connected = BLE_CONNECTED.load(Ordering::Relaxed);
    let body = match ui.page() {
        Page::Neighbours => {
            let clock = ctx.current_time();
            let mut list: Vec<_> = app.neighbours().iter().collect();
            list.sort_by_key(|n| core::cmp::Reverse(n.heard_timestamp));
            Body::Neighbours(
                list.iter()
                    .map(|n| meshcore_ui::Neighbour {
                        id: to_hex(&n.id.pub_key[..id_len]),
                        snr: n.snr as i16,
                        age_min: meshcore_ui::coarse_minutes(clock.saturating_sub(n.heard_timestamp)),
                    })
                    .collect(),
            )
        }
        Page::Traffic => Body::Traffic(Traffic {
            rx: s.n_packets_recv,
            tx: s.n_packets_sent,
            rx_errors: s.n_recv_errors,
            tx_air_permille: permille(s.total_air_time_ms, now),
            rx_air_permille: permille(s.rx_air_time_ms, now),
            queue: ctx.outbound_len() as u16,
            noise_dbm: s.noise_floor,
        }),
        Page::Radio => Body::Preset(passport()),
        Page::Power => Body::Power(PowerInfo {
            batt_mv: app.board.batt_mv,
            powersaving: Some(p.powersaving_enabled),
            asleep_pct: info.asleep_pct,
            uptime_min,
        }),
        Page::Network => {
            let up = |a: &core::sync::atomic::AtomicBool| a.load(Ordering::Relaxed);
            let wifi = match (info.wifi, up(&net::WIFI_UP)) {
                (false, _) if !p.wifi_enabled && !p.wifi_ssid.is_empty() => Wifi::Disabled { ssid: p.wifi_ssid.clone() },
                (false, _) => Wifi::Off,
                (true, false) => Wifi::Connecting { ssid: p.wifi_ssid.clone() },
                (true, true) => Wifi::Up { ssid: p.wifi_ssid.clone(), ip: app.board.wifi_ip.clone().unwrap_or_default() },
            };
            let on: Vec<usize> = (0..meshcore::repeater::MQTT_BROKERS.len())
                .filter(|&i| info.wifi && p.mqtt_brokers & meshcore::repeater::MQTT_BROKERS[i].1 != 0)
                .collect();
            Body::Network(NetInfo {
                wifi,
                mqtt_on: on.len() as u8,
                mqtt_up: on.iter().filter(|&&i| BrokerState::load(i) == BrokerState::Up).count() as u8,
                time_synced: up(&net::TIME_SYNCED),
            })
        }
        Page::Update => {
            let trial = app.board.fw_trial_until.filter(|&t| t > now).map(|t| ((t - now) / 1000, app.board.fw_prev.as_str()));
            let text = crate::fwtext::fw_text(&app.fw_state(), trial, false);
            Body::Update(UpdateInfo {
                window: info.ble_window.map(|(pin, until)| OtaWindow {
                    name: ota_ssid(&p.node_name),
                    pin,
                    connected: ble_connected,
                    mins_left: until.saturating_sub(now).div_ceil(60_000) as u32,
                }),
                state: text.state,
                detail: text.detail,
                progress: text.progress,
                version: meshcore::release::running_label(build_date),
            })
        }
        Page::Advert => Body::Advert { flood: true },
        Page::Contact => Body::Contact(meshcore::util::contact_link_within(
            &p.node_name,
            key,
            meshcore::advert::ADV_TYPE_REPEATER,
            meshcore_ui::layout::mono_128x64::CONTACT_LINK_MAX,
        )),
        _ => Body::Repeater(passport(), Health { neighbours: app.neighbours().len() as u16, uptime_min }),
    };
    let (page_idx, pages, alert) = ui.view_frame();
    let ble = info.ble_window.map(|_| if ble_connected { BleState::Connected } else { BleState::Waiting });
    let batt_mv = ui.battery_mv(app.board.batt_mv);
    View { page: ui.page(), page_idx, pages, bar: Bar { ble, batt_mv: (batt_mv > 0).then_some(batt_mv) }, body, alert, pin: None }
}

/// The repeater main loop. The board builds the radio and the hardware; this owns everything else.
pub async fn run<R, B>(spawner: Spawner, hw: Hardware<R>, mut board: B, cfg: Config) -> !
where
    R: crate::radio::LoraRadio,
    B: RepeaterBoard,
{
    use crate::radio::RxEvent;

    let Hardware { mut radio, store, wifi, bt, entropy: trng_entropy } = hw;
    let build_date = cfg.build_date;
    // the map's second line under "meshcore-rs v1.18.0": the product version and release number
    let client_version: String = meshcore::release::running_label(build_date);
    // the board installs the watchdog (it holds the RTC peripheral) before calling run

    println!();
    println!("MeshCore repeater (Rust) {} {}", meshcore::FIRMWARE_VERSION, meshcore::release::build_label(build_date));

    // ---- persistent state
    if store.is_none() {
        warn!("no settings partition: settings will not persist");
    }
    let slots = store.as_ref().and_then(|s| ota::slots(&mut s.borrow_mut()));
    let mut in_trial = match (&store, slots) {
        (Some(s), Some(sl)) => ota::check_trial(&mut s.borrow_mut(), sl.booted),
        _ => false,
    };
    match slots {
        Some(sl) => info!("firmware slot {}{}", sl.booted, if in_trial { " (trial)" } else { "" }),
        None => warn!("single-slot partition table: OTA updates unavailable"),
    }

    let prefs = load(&store, Blob::Prefs)
        .map(|d| NodePrefs::deserialize(&String::from_utf8_lossy(&d)))
        .unwrap_or_else(|| NodePrefs { tx_power_dbm: board.default_tx_dbm(), ..Default::default() });

    // ---- radio
    board.fem_rx_mode();
    if let Err(e) = radio.init(prefs.radio, prefs.tx_power_dbm, prefs.rx_boosted_gain).await {
        println!("ERROR: radio init failed: {e:?}");
        loop {
            board.led_toggle();
            Timer::after_millis(200).await;
        }
    }
    info!(
        "radio: {} MHz BW{} SF{} CR{} tx {} dBm",
        Shortest(prefs.radio.freq_mhz), Shortest(prefs.radio.bw_khz), prefs.radio.sf, prefs.radio.cr, prefs.tx_power_dbm
    );

    // ---- entropy: hardware TRNG (from the board) mixed with the radio's RSSI noise
    let mut entropy = [0u8; 64];
    entropy[..32].copy_from_slice(&trng_entropy);
    let _ = radio.random_bytes(&mut entropy[32..]).await;

    // ---- identity
    let self_id = match load_identity(&store) {
        Some(id) => id,
        None => {
            info!("generating new keypair");
            let mut id;
            let mut counter = 0u8;
            loop {
                let mut seed = [0u8; 32];
                crypto::sha256(&mut seed, &[&entropy, &[counter]]);
                id = LocalIdentity::from_seed(&seed);
                counter += 1;
                if (id.pub_key()[0] != 0x00 && id.pub_key()[0] != 0xFF) || counter > 10 {
                    break;
                }
            }
            save_identity(&store, id.private_key());
            id
        }
    };
    println!("Repeater ID: {}", to_hex(self_id.pub_key()));

    // ---- battery (the board applies its own scale)
    let set_multiplier = (prefs.adc_multiplier > 0.0).then_some(prefs.adc_multiplier);
    let batt_mv = board.battery_mv(set_multiplier).await;
    reset::feed();

    // ---- mesh
    let mut rng_seed = [0u8; 16];
    Rng::new().read(&mut rng_seed);
    for (s, e) in rng_seed.iter_mut().zip(entropy[32..].iter()) {
        *s ^= *e;
    }
    let ram_epoch = clock_cell::load().map(|t| t.saturating_add(2)).unwrap_or(0);
    let saved_epoch = load(&store, Blob::Clock)
        .and_then(|d| d.get(..4).map(|b| u32::from_le_bytes(b.try_into().unwrap())))
        .unwrap_or(0);
    let epoch: u32 = cfg.build_epoch.max(saved_epoch.saturating_add(1)).max(ram_epoch);
    clock_cell::store(epoch);
    let ctx = MeshCtx::new(self_id.clone(), rng_seed, prefs.radio, epoch);
    let model = String::from(board.name());
    let board_info = BoardInfo {
        manufacturer: model.clone(),
        batt_mv,
        mcu_temp_c: None,
        fem_lna_controllable: board.lna_controllable(),
        powersaving_supported: board.powersaving_supported(),
        bridge: board.bridge(),
        build_date: build_date.into(),
        wifi_ota_ssid: slots.map(|_| ota_ssid(&prefs.node_name)),
        ble_ota_name: slots.map(|_| ota_ssid(&prefs.node_name)),
        adc_multiplier: board.adc_multiplier(),
        has_gps: board.has_gps(),
        ..Default::default()
    };
    let mut mesh = Mesh::new(ctx, Repeater::new(prefs, board_info));
    if let (Some(s), Some(sl)) = (&store, slots) {
        let b = &mut mesh.app.board;
        (b.fw_rollback, b.fw_prev, b.fw_skip) = ota::other_slot(&mut s.borrow_mut(), sl);
    }
    if let Some(acl) = load(&store, Blob::Acl) {
        mesh.app.acl_load(&acl, &self_id);
    }
    if let Some(regions) = load(&store, Blob::Regions) {
        mesh.app.regions = meshcore::region::RegionMap::deserialize(&String::from_utf8_lossy(&regions));
    }
    if let (Some(s), Some(sl)) = (&store, slots) {
        mesh.app.ota.target = Some(Box::new(FlashTarget::new(s.clone(), sl)));
    }
    mesh.app.ota.build.board = board.release_board();
    let fw_region = match (&store, slots, mesh.app.ota.running()) {
        (Some(s), Some(sl), Some((image_len, _))) => fwstore::region(&sl, image_len).inspect(|&region| {
            mesh.app.fw_store = Some(Box::new(fwstore::FlashFwStore::new(s.clone(), region)));
        }),
        _ => None,
    };
    info!("boot: mesh");
    mesh.begin(now_ms());
    mesh.app.start(&mut mesh.ctx);
    let mut room = mesh.app.prefs.room.then(|| {
        let mut seed = rng_seed;
        seed[0] ^= 0x5A;
        let mut room = start_room(&store, &mesh, &entropy, seed);
        room.begin(now_ms());
        room.app.start(&mut room.ctx);
        room
    });

    let mut wifi = Some(wifi);
    // BLE only runs during `fw ble` (`bt` is kept for the window below)

    // ---- Wi-Fi station + MQTT observer (when configured)
    let device_id = to_hex(self_id.pub_key());
    let mut station: Option<embassy_net::Stack<'static>> = None;
    let bridge_on = mesh.app.prefs.bridge_enabled;
    let mut bridge_radio = false;
    if mesh.app.prefs.wifi_enabled && !mesh.app.prefs.wifi_ssid.is_empty() {
        let seed = (Rng::new().random() as u64) << 32 | Rng::new().random() as u64;
        let params = net::StationParams {
            ssid: mesh.app.prefs.wifi_ssid.clone(),
            password: mesh.app.prefs.wifi_pwd.clone(),
            // modem sleep would miss bridge frames between beacons; on the ESP32-C6 it also drops
            // plain station traffic (DNS/TCP time out while associated, see net.rs), so keep the
            // radio awake there regardless of the bridge
            power_save: !bridge_on && cfg!(feature = "esp32s3"),
            esp_now: bridge_on,
        };
        info!("boot: Wi-Fi station");
        reset::feed();
        match net::start(&spawner, wifi.take().unwrap(), seed, params) {
            Ok((stack, esp_now)) => {
                station = Some(stack);
                if let Some(e) = esp_now {
                    match espnow::spawn(&spawner, e) {
                        Ok(()) => info!("bridge: ESP-NOW on the router's channel"),
                        Err(e) => warn!("bridge: {e}"),
                    }
                }
            }
            Err(e) => warn!("Wi-Fi station: {e}"),
        }
    }
    if bridge_on
        && station.is_none()
        && let Some(w) = wifi.take()
    {
        let channel = mesh.app.prefs.bridge_channel;
        match net::start_esp_now(&spawner, w, channel).and_then(|e| espnow::spawn(&spawner, e)) {
            Ok(()) => info!("bridge: ESP-NOW on channel {channel}"),
            Err(e) => warn!("bridge: {e}"),
        }
        bridge_radio = true;
    }
    let iata = mesh.app.prefs.mqtt_iata.clone();
    if let Some(stack) = station
        && iata != "UNSET"
    {
        let p = &mesh.app.prefs;
        let radio_str = uplink::radio_string(p.radio.freq_mhz, p.radio.bw_khz, p.radio.sf, p.radio.cr);
        let obs = uplink::Observer {
            name: &p.node_name,
            device_id: &device_id,
            model: &model,
            firmware_version: meshcore::FIRMWARE_VERSION,
            client_version: &client_version,
            radio: &radio_str,
        };
        let will = uplink::status_json(&obs, mesh.ctx.current_time(), false, None);
        let prefix = alloc::format!("meshcore/{iata}/{device_id}/");
        let mut brokers = Vec::new();
        if mesh.app.prefs.mqtt_brokers & 0x01 != 0 {
            brokers.push(broker::BrokerConfig {
                index: 0,
                host: "meshcoretel.ru",
                port: 1883,
                user: "meshcore".into(),
                password: broker::Credential::Fixed("meshcore"),
                client_id: alloc::format!("mqtt_meshcoretel-{}", &device_id[..6]),
                topic_prefix: prefix.clone(),
                will: will.clone(),
            });
        }
        // ommesh over plain TCP, JWT scheme; index 1 is ommesh (see meshcore::repeater::MQTT_BROKERS)
        let ommesh_bit = meshcore::repeater::MQTT_BROKERS[1].1;
        if mesh.app.prefs.mqtt_brokers & ommesh_bit != 0 {
            let non_empty = |s: &str| (!s.is_empty()).then(|| String::from(s));
            brokers.push(broker::BrokerConfig {
                index: 1,
                host: "nw.ommesh.ru",
                port: 1883,
                user: alloc::format!("v1_{device_id}"),
                password: broker::Credential::Jwt {
                    id: self_id.clone(),
                    owner: non_empty(&mesh.app.prefs.mqtt_owner),
                    email: non_empty(&mesh.app.prefs.mqtt_email),
                },
                client_id: alloc::format!("mqtt_ommesh-{}", &device_id[..6]),
                topic_prefix: prefix.clone(),
                will: will.clone(),
            });
        }
        for cfg in brokers {
            if let Err(e) = broker::spawn(&spawner, stack, cfg) {
                warn!("MQTT: {e}");
            }
        }
        mesh.app.uplink_enabled = true;
    }
    if let (Some(stack), Some(s), Some(region)) = (station, &store, fw_region) {
        spawner.spawn(fwstore::fetch_task(stack, s.clone(), region).unwrap());
    }
    let mut fw_store_seen = 0u32;
    let uplink_pub = net::UPLINK.publisher().unwrap();
    let mut next_status = 0u64;

    // ---- GPS at boot
    board.set_gps(mesh.app.prefs.gps_enabled);

    // ---- the on-device interface (pages on a screen, turned with the button)
    let has_screen = board.has_screen();
    let mut awake_until = now_ms() + AWAKE_AFTER_ACTIVITY_MS;
    let (mut ota_requests, mut ota_seen) = (0u32, 0u64);
    let mut next_power_status = 0u64;
    let pages: &'static [Page] = match (has_screen, slots) {
        (false, _) => &[],
        (true, Some(_)) => PAGES,
        (true, None) => PAGES_SINGLE_SLOT,
    };
    let mut ui = Ui::new(pages).with_home_action(Action::PowerOff);
    let always_on = board.always_on();
    if !always_on {
        ui = ui.with_screen_timeout(SCREEN_ON_MS, now_ms());
    }
    let mut ui_button = if board.button_down() { Button::held() } else { Button::default() };
    let mut screen = ScreenPolicy::default();
    let (mut next_view, mut next_oled_try, mut oled_failures) = (0u64, 0u64, 0u8);
    let mut ble_pin: Option<u32> = None;

    println!("Ready. Type CLI commands (e.g. 'get name', 'set name X', 'get radio', 'neighbors').");

    // ---- main loop
    let mut noise = NoiseFloor::new();
    let mut next_noise = 0u64;
    let mut next_batt = now_ms() + 60_000;
    let mut next_agc_reset = 0u64;
    let mut next_clock_save = now_ms() + CLOCK_SAVE_INTERVAL_MS;
    let mut last_clock = mesh.ctx.current_time();
    let (mut gps_fix_since, mut next_gps_time) = (None::<u64>, 0u64);
    let mut reboot_at: Option<u64> = None;
    let mut power_off_at: Option<u64> = None;
    let mut ble_until: Option<u64> = None;
    let confirm_at = now_ms() + TRIAL_HEALTHY_MS;
    mesh.app.board.fw_trial_until = in_trial.then_some(confirm_at);
    info!("boot: main loop");
    loop {
        reset::feed();
        let now = now_ms();

        // transmit if something is due and the channel is clear: the repeater first, then the
        // hosted room server (the radio settings are the repeater's)
        let thr = mesh.app.prefs.interference_threshold as i16;
        let cad = mesh.app.prefs.cad_enabled;
        let mut transmitted = false;
        for m in core::iter::once(&mut mesh).chain(room.as_mut()) {
            if !m.tx_pending(now) {
                continue;
            }
            let mut busy = radio.is_receiving().unwrap_or(false);
            if !busy && thr != 0 {
                busy = radio.rssi_inst().is_ok_and(|r| r > noise.floor + thr);
            }
            if !busy && cad {
                busy = radio.channel_activity().await.unwrap_or(false);
            }
            if !(busy && m.tx_defer_busy(now))
                && let Some(frame) = m.tx_take(now)
            {
                if frame.cr.is_some() {
                    let _ = radio.set_frame_cr(frame.cr);
                }
                board.fem_tx_mode();
                board.tx_led(true);
                let res = radio.transmit(frame.bytes(), frame.timeout_ms).await;
                board.tx_led(false);
                board.fem_rx_mode();
                if frame.cr.is_some() {
                    let _ = radio.set_frame_cr(None);
                }
                if let Err(e) = radio.start_rx() {
                    warn!("start_rx: {e:?}");
                }
                match res {
                    Ok(()) => m.tx_done(now_ms()),
                    Err(e) => {
                        warn!("tx failed: {e:?}");
                        m.tx_failed(now_ms());
                    }
                }
                transmitted = true;
            }
            break; // one transmission (or deferral) per pass
        }
        if transmitted {
            continue;
        }

        // `powersaving on`: light sleep until the next timer or a packet, when nothing needs the
        // chip awake (radios other than LoRa, a console, queued packets, a lit screen: an e-paper
        // is never dark, and keeps its image through the sleep)
        if mesh.app.ota.requests != ota_requests {
            (ota_requests, ota_seen) = (mesh.app.ota.requests, now);
        }
        let usb_host = board.usb_host_active();
        if mesh.app.prefs.powersaving_enabled
            && now >= awake_until
            && (always_on || !ui.screen_on(now))
            && !usb_host
            && station.is_none()
            && wifi.is_some()
            && ble_until.is_none()
            && reboot_at.is_none()
            && power_off_at.is_none()
            && !(mesh.app.ota.busy() && now < ota_seen + OTA_IDLE_MS)
            && mesh.ctx.outbound_len() == 0
            && room.as_ref().is_none_or(|r| r.ctx.outbound_len() == 0)
        {
            let next = room.as_ref().and_then(|r| r.next_deadline()).into_iter().chain(mesh.next_deadline()).min();
            let next = next.unwrap_or(u64::MAX);
            let span = next.saturating_sub(now).min(MAX_SLEEP_MS);
            if span >= 100 && radio.arm_wake() {
                if let Some(woke) = board.light_sleep(span) {
                    if woke.button {
                        let t = now_ms();
                        ui.button_down(t);
                        let _ = ui_button.poll(true, t);
                        awake_until = t + AWAKE_AFTER_ACTIVITY_MS;
                        next_view = 0;
                    }
                    if woke.console {
                        let t = now_ms();
                        awake_until = t + AWAKE_AFTER_ACTIVITY_MS;
                        println!();
                        println!("  -> woke from sleep, that line was lost: send it again");
                    }
                    continue;
                }
            }
        }
        if now >= next_power_status {
            next_power_status = now + 60_000;
            mesh.app.board.power_status = board.power_summary();
        }
        // sleep until a frame arrives, a CLI line or a BLE frame comes in, or a timer is due
        let wake = room.as_ref().map_or(u64::MAX, |r| r.next_wakeup()).min(mesh.next_wakeup()).min(now + 50).max(now + 1);
        match select4(radio.wait_irq(), CLI_LINES.receive(), Timer::at(Instant::from_millis(wake)), FRAMES_IN.receive()).await {
            Either4::First(()) => match radio.read_rx() {
                Ok(RxEvent::Frame(f)) => {
                    let t = now_ms();
                    mesh.on_rx_frame(t, &f.data[..f.len], f.snr, f.rssi);
                    if let Some(r) = room.as_mut() {
                        r.on_rx_frame(t, &f.data[..f.len], f.snr, f.rssi);
                    }
                }
                Ok(RxEvent::CrcError) => mesh.ctx.stats.n_recv_errors += 1,
                Ok(RxEvent::Nothing) => {}
                Err(e) => warn!("rx: {e:?}"),
            },
            Either4::Second(line) => {
                awake_until = now_ms() + AWAKE_AFTER_ACTIVITY_MS;
                let cmd = String::from_utf8_lossy(&line);
                if cmd.trim() == "wdt test" {
                    println!("  -> hanging the main loop; the watchdog should reboot in {WATCHDOG_SECS}s");
                    #[allow(clippy::empty_loop)]
                    loop {}
                }
                let line = cmd.trim_end();
                let room_cmd = line.strip_prefix("room ").map(str::trim_start).or(line.starts_with("room.post").then_some(line));
                let reply = match (room_cmd, room.as_mut()) {
                    (Some(c), Some(r)) => r.app.handle_command(&mut r.ctx, 0, c),
                    (Some(_), None) => String::from("room server is off (set room on, then reboot)"),
                    (None, _) => mesh.app.handle_command(&mut mesh.ctx, 0, line),
                };
                if !reply.is_empty() {
                    println!("  -> {reply}");
                }
            }
            Either4::Third(()) => {}
            Either4::Fourth((_, frame)) => {
                if let Some(reply) = mesh.app.handle_link_frame(&frame) {
                    let _ = BLE_OUT.try_send(reply);
                }
            }
        }

        let now = now_ms();
        mesh.poll(now);
        if let Some(r) = room.as_mut() {
            r.poll(now);
        }
        // the bridge: frames out for what the repeater sent (or heard), packets in from others
        for frame in mesh.app.take_bridge_out() {
            let _ = espnow::OUT.try_send(frame);
        }
        while let Ok(frame) = espnow::IN.try_receive() {
            if let Some((pkt, delay)) = mesh.app.bridge_receive(&frame) {
                mesh.queue_bridged(now, pkt, delay);
            }
        }

        // a burst every 2 s; one cut short by a packet goes on in the next one
        if now >= next_noise {
            next_noise = now + NoiseFloor::INTERVAL_MS;
            if noise.measure(|| if radio.is_receiving().unwrap_or(true) { None } else { radio.rssi_inst().ok() }) {
                mesh.ctx.stats.noise_floor = noise.floor;
                if let Some(r) = room.as_mut() {
                    r.ctx.stats.noise_floor = noise.floor;
                }
            }
        }

        if now >= next_batt {
            next_batt = now + 60_000;
            mesh.app.board.batt_mv = board.battery_mv(mesh.app.adc_multiplier()).await;
            if let Some(r) = room.as_mut() {
                r.app.board.batt_mv = mesh.app.board.batt_mv;
            }
        }

        let gps_reading = board.gps_latest();
        mesh.app.board.gps = gps_reading;
        if let Some(r) = room.as_mut() {
            r.app.board.gps = gps_reading;
        }
        // the clock from the GPS, either way, once a fix has held for a moment, and again every
        // half hour (upstream `MicroNMEALocationProvider::loop`)
        match gps_reading.filter(|g| g.valid) {
            Some(g) => {
                let since = *gps_fix_since.get_or_insert(now);
                if now >= since + GPS_FIX_SETTLE_MS
                    && now >= next_gps_time
                    && let Some(t) = g.time
                {
                    next_gps_time = now + GPS_TIME_SYNC_MS;
                    if t.abs_diff(mesh.ctx.current_time()) > 2 {
                        info!("GPS: clock set to {t}");
                        mesh.ctx.set_current_time(t);
                    }
                }
            }
            None => gps_fix_since = None,
        }

        // periodic receiver reset against a stuck AGC (`agc.reset.interval`, off by default)
        let agc_secs = mesh.app.prefs.agc_reset_interval as u64 * 4;
        if agc_secs > 0 && now >= next_agc_reset {
            if next_agc_reset != 0 && !mesh.tx_in_flight() && !radio.is_receiving().unwrap_or(true) {
                if radio.reset_agc().await.is_err() {
                    warn!("AGC reset failed");
                }
            }
            next_agc_reset = now + agc_secs * 1000;
        }

        // the interface: the button lights the screen and turns the pages
        let down = board.button_down();
        if down && !ui_button.is_down() {
            ui.button_down(now);
            awake_until = now + AWAKE_AFTER_ACTIVITY_MS;
            next_view = 0;
        }
        ui.tick(now);
        if let Some(intent) = ui_button.poll(down, now)
            && power_off_at.is_none()
        {
            next_view = 0;
            match ui.press(intent, now) {
                Some(Action::PowerOff) => mesh.app.effects.power_off = true,
                Some(Action::Advert) => {
                    let _ = mesh.app.handle_command(&mut mesh.ctx, 0, "advert");
                    ui.alert(Alert::AdvertSent, now);
                }
                Some(Action::ToggleUpdate) if ble_until.is_none() => {
                    let _ = mesh.app.handle_command(&mut mesh.ctx, 0, "start ota ble");
                }
                Some(Action::ToggleUpdate) if !BLE_CONNECTED.load(Ordering::Relaxed) => {
                    (ble_until, ble_pin) = (None, None);
                    ble::stop();
                    println!("BLE update window closed with the button");
                }
                _ => {}
            }
        }
        if has_screen {
            if oled_failures >= 10 {
                warn!("OLED: disabled after repeated bus errors");
                board.screen_disable();
                oled_failures = 0;
            }
            if !ui.screen_on(now) {
                if board.screen_is_on() {
                    info!("OLED: screen off ({} failed I2C transfers since boot)", board.screen_bus_errors());
                }
                board.screen_set_on(false);
                screen.forget();
            } else if now >= next_view && now >= next_oled_try {
                next_view = now + 250;
                let info = ScreenInfo {
                    ble_window: ble_pin.zip(ble_until),
                    wifi: station.is_some(),
                    asleep_pct: board.asleep_pct(),
                };
                let mut view = repeater_view(&mut ui, &mesh, &info, now, build_date);
                if power_off_at.is_some() {
                    view.body = Body::Off { name: mesh.app.prefs.node_name.clone() };
                    view.alert = None;
                }
                if screen.due(&view, now) {
                    board.screen_set_on(true);
                    if board.screen_show(&view) {
                        if oled_failures > 0 {
                            info!("OLED: back after {oled_failures} failed frames");
                        }
                        oled_failures = 0;
                        screen.drawn(view, now);
                    } else {
                        oled_failures += 1;
                        next_oled_try = now + 1000 * oled_failures as u64;
                        screen.forget();
                        warn!("OLED: no answer ({oled_failures}), resetting it");
                    }
                }
            }
        }

        // network: SNTP time, observer uplink, status lines for the CLI
        if let Some(t) = net::TIME_SYNC.try_take()
            && t.abs_diff(mesh.ctx.current_time()) > 2
        {
            info!("SNTP: clock set to {t}");
            mesh.ctx.set_current_time(t);
        }
        if let Some(stack) = station {
            let ip = net::ipv4(&stack);
            mesh.app.board.wifi_status = alloc::format!(
                "ssid:{} status:{} ip:{}",
                mesh.app.prefs.wifi_ssid,
                if net::WIFI_UP.load(Ordering::Relaxed) { "connected" } else { "disconnected" },
                ip.as_deref().unwrap_or("-")
            );
            let online = net::WIFI_UP.load(Ordering::Relaxed) && ip.is_some();
            fwstore::sync(online, &mesh.app.prefs.fw_server);
            mesh.app.board.wifi_ip = ip;
        } else if !mesh.app.prefs.wifi_enabled && !mesh.app.prefs.wifi_ssid.is_empty() {
            mesh.app.board.wifi_status = String::from("off (set wifi.enabled on)");
        } else {
            mesh.app.board.wifi_status = String::from("off (set wifi.ssid / wifi.pwd)");
        }
        if let Some(line) = fwstore::status_if_changed(&mut fw_store_seen) {
            mesh.app.board.fw_store = line;
        }
        mesh.app.board.mqtt_status = mqtt_status_line(&mesh.app.prefs, station.is_some());
        if mesh.app.uplink_enabled {
            let p = &mesh.app.prefs;
            let radio_str = uplink::radio_string(p.radio.freq_mhz, p.radio.bw_khz, p.radio.sf, p.radio.cr);
            let obs = uplink::Observer {
                name: &p.node_name,
                device_id: &device_id,
                model: &model,
                firmware_version: meshcore::FIRMWARE_VERSION,
                client_version: &client_version,
                radio: &radio_str,
            };
            let t = mesh.ctx.current_time();
            for up in core::mem::take(&mut mesh.app.uplink) {
                if let Some(pkt) = meshcore::Packet::parse(&up.raw) {
                    let json = uplink::packet_json(&obs, t, up.tx, &pkt, &up.raw, up.snr, up.rssi, up.score_duration);
                    uplink_pub.publish_immediate(alloc::sync::Arc::new(net::UplinkMsg { leaf: "packets", json, retain: false }));
                    if p.mqtt_raw {
                        let json = uplink::raw_json(&obs, t, &up.raw);
                        uplink_pub.publish_immediate(alloc::sync::Arc::new(net::UplinkMsg { leaf: "raw", json, retain: false }));
                    }
                }
            }
            let connected = net::BROKER_CONNECTED.swap(false, Ordering::Relaxed);
            let send_now = core::mem::take(&mut mesh.app.effects.send_mqtt_status);
            if send_now || connected || (p.mqtt_status && now >= next_status) {
                next_status = now + uplink::STATUS_INTERVAL_SECS as u64 * 1000;
                let s = &mesh.ctx.stats;
                let stats = uplink::StatusStats {
                    battery_mv: mesh.app.board.batt_mv,
                    uptime_secs: (now / 1000) as u32,
                    errors: s.err_flags,
                    queue_len: mesh.ctx.outbound_len() as u16,
                    noise_floor: s.noise_floor,
                    tx_air_secs: (s.total_air_time_ms / 1000) as u32,
                    rx_air_secs: (s.rx_air_time_ms / 1000) as u32,
                    recv_errors: s.n_recv_errors,
                };
                let json = uplink::status_json(&obs, t, true, Some(&stats));
                uplink_pub.publish_immediate(alloc::sync::Arc::new(net::UplinkMsg { leaf: "status", json, retain: true }));
            }
        }

        // the hosted room server follows the repeater's clock
        if let Some(r) = room.as_mut() {
            let t = mesh.ctx.current_time();
            if r.ctx.current_time().abs_diff(t) > 2 {
                r.ctx.set_current_time(t);
            }
        }

        // persist the clock periodically, and right after it was set
        let clock = mesh.ctx.current_time();
        let jumped = clock.abs_diff(last_clock) > 120;
        if clock != last_clock {
            clock_cell::store(clock);
        }
        last_clock = clock;
        if now >= next_clock_save || jumped {
            next_clock_save = now + CLOCK_SAVE_INTERVAL_MS;
            save(&store, Blob::Clock, &clock.to_le_bytes());
        }

        // hardware side effects requested by the repeater
        let fx = mesh.app.take_effects();
        for line in &fx.console {
            println!("{line}");
        }
        if fx.save_prefs {
            let text = mesh.app.prefs.serialize();
            if !save(&store, Blob::Prefs, text.as_bytes()) {
                warn!("prefs save failed");
            }
        }
        if fx.save_acl {
            let acl = mesh.app.acl_serialize();
            if !save(&store, Blob::Acl, &acl) {
                warn!("ACL save failed");
            }
        }
        if fx.save_regions && !save(&store, Blob::Regions, mesh.app.regions.serialize().as_bytes()) {
            warn!("regions save failed");
        }
        if let Some(prv) = fx.save_identity
            && !save_identity(&store, &prv)
        {
            warn!("identity save failed");
        }
        if let Some(params) = fx.apply_radio {
            if radio.apply_params(params).and_then(|_| radio.start_rx()).is_err() {
                warn!("radio reconfigure failed");
            }
            mesh.ctx.radio = params;
        }
        if let Some(dbm) = fx.tx_power {
            let _ = radio.set_tx_power(dbm);
        }
        if let Some(on) = fx.rx_boosted_gain {
            let _ = radio.set_rx_boosted_gain(on);
        }
        if let Some(on) = fx.fem_lna {
            board.fem_set_lna(on);
        }
        if fx.gps {
            board.set_gps(mesh.app.prefs.gps_enabled);
        }
        if let Some((code, key)) = fx.start_ota {
            if let (Some(stack), Some(s), Some(sl)) = (station, &store, slots) {
                let params = wifi_ota::Params {
                    ssid: alloc::format!("LAN {}", net::ipv4(&stack).unwrap_or_default()),
                    password: None,
                    node_name: mesh.app.prefs.node_name.clone(),
                    version: client_version.clone(),
                    code,
                    store: s.clone(),
                    slots: sl,
                    roots: mesh.app.ota.roots.clone(),
                    owners: mesh.app.ota.owners.clone(),
                    build: mesh.app.ota.build,
                };
                match wifi_ota::start_http(&spawner, stack, params) {
                    Ok(()) => println!("Wi-Fi OTA: upload page on the LAN address"),
                    Err(e) => println!("Wi-Fi OTA: {e}"),
                }
            } else {
                match (wifi.take(), &store, slots) {
                    (Some(w), Some(s), Some(sl)) => {
                        let params = wifi_ota::Params {
                            ssid: ota_ssid(&mesh.app.prefs.node_name),
                            password: Some(key),
                            node_name: mesh.app.prefs.node_name.clone(),
                            version: client_version.clone(),
                            code,
                            store: s.clone(),
                            slots: sl,
                            roots: mesh.app.ota.roots.clone(),
                            owners: mesh.app.ota.owners.clone(),
                            build: mesh.app.ota.build,
                        };
                        let seed = (Rng::new().random() as u64) << 32 | Rng::new().random() as u64;
                        match wifi_ota::start(&spawner, w, seed, params) {
                            Ok(()) => println!("Wi-Fi OTA started: connect and open http://192.168.4.1/"),
                            Err(e) => println!("Wi-Fi OTA failed: {e}"),
                        }
                    }
                    (None, ..) if bridge_radio => println!("Wi-Fi OTA unavailable while the bridge holds the radio (start ota ble)"),
                    (None, ..) => {
                        wifi_ota::set_code(code);
                        println!("Wi-Fi OTA already running (new code set)");
                    }
                    _ => println!("Wi-Fi OTA unavailable"),
                }
            }
        }
        if let Some(pin) = fx.start_ble_ota {
            ble_until = Some(now + BLE_OTA_WINDOW_MINS as u64 * 60_000);
            ble_pin = Some(pin);
            next_view = 0;
            let params = ble::Params {
                name: ota_ssid(&mesh.app.prefs.node_name),
                address: ble::static_address(),
                bonds: store.as_ref().and_then(|s| s.borrow_mut().comp_load(CompBlob::Bonds)).unwrap_or_default(),
                pin: Some(pin),
            };
            // SAFETY: one BLE task at a time (`ble::start` checks), and an earlier one has dropped
            // its controller before clearing `BLE_RUNNING`
            ble::start(&spawner, unsafe { bt.clone_unchecked() }, params);
        }
        if ble_until.is_some_and(|t| now >= t) {
            if BLE_CONNECTED.load(Ordering::Relaxed) {
                ble_until = Some(now + 60_000);
            } else {
                (ble_until, ble_pin) = (None, None);
                ble::stop();
                println!("BLE update window closed");
            }
        }
        if let Some(bonds) = BONDS_CHANGED.try_take()
            && let Some(s) = &store
        {
            s.borrow_mut().comp_save(CompBlob::Bonds, &bonds);
        }
        if fx.network_changed || fx.bridge_changed {
            reboot_at = Some(now + 3000);
        }
        if fx.erase {
            if let Some(s) = &store {
                s.borrow_mut().erase_all();
            }
            reboot_at = Some(now + 500);
        }
        if fx.reboot {
            reboot_at = Some(now + 3000);
        }
        if fx.fw_rollback
            && let (Some(s), Some(sl)) = (&store, slots)
        {
            if ota::rollback(&mut s.borrow_mut(), sl) {
                reboot_at = Some(now + 3000);
            } else {
                warn!("fw rollback: could not select the other slot");
            }
        }
        if let Some(r) = room.as_mut() {
            let rfx = r.app.take_effects();
            for line in &rfx.console {
                println!("[room] {line}");
            }
            if (rfx.save_prefs || rfx.save_identity.is_some())
                && !save_room(&store, &rfx.save_identity.unwrap_or(*r.ctx.self_id.private_key()), &r.app.prefs)
            {
                warn!("room: prefs save failed");
            }
            if rfx.save_acl && !save(&store, Blob::RoomAcl, &r.app.acl_serialize()) {
                warn!("room: ACL save failed");
            }
            if rfx.reboot {
                reboot_at = Some(now + 3000);
            }
            if rfx.apply_radio.is_some() || rfx.tx_power.is_some() || rfx.save_regions || rfx.erase || rfx.power_off {
                println!("[room] radio, regions, erase and power are managed on the repeater: ignored");
            }
        }
        if fx.power_off && power_off_at.is_none() {
            println!("Powering off in {} s (the button turns it on again)...", POWER_OFF_DELAY_MS / 1000);
            power_off_at = Some(now + POWER_OFF_DELAY_MS);
            ui.wake(now);
            next_view = 0;
            board.tx_led(true);
        }
        if power_off_at.is_some_and(|t| now >= t) {
            save(&store, Blob::Clock, &mesh.ctx.current_time().to_le_bytes());
            let _ = radio.sleep();
            board.power_off().await;
        }
        if in_trial && now >= confirm_at {
            if let Some(s) = &store {
                ota::confirm(&mut s.borrow_mut());
            }
            in_trial = false;
            mesh.app.board.fw_trial_until = None;
            info!("OTA: new firmware confirmed");
        }

        if reboot_at.is_some_and(|t| now >= t) {
            save(&store, Blob::Clock, &mesh.ctx.current_time().to_le_bytes());
            reset::hard_reset();
        }
    }
}
